use anyhow::{bail, Context, Result};
use std::path::Path;

use super::*;

// ---------------------------------------------------------------------------
// Unpushed work
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Unpushed {
    /// No remote counterpart, so nothing on this branch has ever been pushed.
    /// `commits` is what it carries beyond the upstream base — the work that
    /// would actually be lost.
    NeverPushed {
        commits: Vec<String>,
    },
    Ahead {
        commits: Vec<String>,
    },
    UpToDate,
}

impl Unpushed {
    /// Blocks whenever there are commits that exist nowhere else.
    ///
    /// The spec's rule is "no remote counterpart means every commit is
    /// unpushed, block" (§2). Taken literally that also blocks a *fresh*
    /// worktree, which is branched straight off `upstream/develop` and carries
    /// nothing — it could never be removed again. Counting the commits beyond
    /// the base keeps the fail-closed behaviour for real work and drops only
    /// the false positive.
    pub fn blocks_teardown(&self) -> bool {
        match self {
            Unpushed::UpToDate => false,
            Unpushed::NeverPushed { commits } => !commits.is_empty(),
            Unpushed::Ahead { .. } => true,
        }
    }
}

/// Resolve the fork branch explicitly (§2).
///
/// `@{push}` does not resolve on a branch that was never pushed, and `@{u}`
/// resolves to `upstream/develop` — neither answers the question.
/// A unified diff of a file git has never seen, against nothing.
///
/// **`git diff` cannot report an untracked file**, so the changed-files pane
/// listed every file a session had just created and then answered "no textual
/// changes against this base" for each — on the one kind of row where the whole
/// file *is* the change. `--no-index` against `/dev/null` produces the ordinary
/// unified format with every line added, so the pane's parser needs no new shape.
///
/// **Its exit code is the reason this is not a [`git`] call.** `--no-index`
/// follows `diff(1)` and exits **1 when the files differ**, which is the success
/// case here and which `git` treats as a failure. Anything above 1 still is one.
///
/// `/dev/null` is POSIX, so it holds on both targets — see CLAUDE.md on shelling
/// out to anything that is not.
pub fn diff_untracked(cwd: &Path, path: &str, context: u32) -> Result<String> {
    let ctx = format!("-U{context}");
    let args = ["diff", "--no-index", &ctx, "--", "/dev/null", path];
    let out = run(cwd, &args).with_context(|| format!("running git {}", args.join(" ")))?;
    match out.status.code() {
        Some(0 | 1) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        _ => bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// Whether git has never seen this path.
///
/// `ls-files --error-unmatch` exits non-zero for one, which is the cheapest
/// question that tells "this file has no changes against the base" apart from
/// "this file is not in the base at all" — two states that produce the same empty
/// diff and mean opposite things.
pub fn is_untracked(cwd: &Path, path: &str) -> bool {
    git(cwd, &["ls-files", "--error-unmatch", "--", path]).is_err()
}

pub fn unpushed(cwd: &Path, branch: &str, upstream: &str) -> Result<Unpushed> {
    let (range, on_origin) = unpushed_range(cwd, branch, upstream);
    if !on_origin {
        let commits = git(cwd, &["log", &range, "--oneline"])
            .map(|out| out.lines().map(|l| l.trim().to_string()).collect())
            .unwrap_or_else(|_| vec!["(could not resolve the upstream base)".to_string()]);
        return Ok(Unpushed::NeverPushed { commits });
    }
    let out = git(cwd, &["log", &range, "--oneline"])?;
    let commits: Vec<String> = out.lines().map(|l| l.trim().to_string()).collect();
    if commits.is_empty() {
        Ok(Unpushed::UpToDate)
    } else {
        Ok(Unpushed::Ahead { commits })
    }
}

pub fn is_clean(cwd: &Path) -> Result<bool> {
    let raw = git_raw(cwd, &["status", "--porcelain"])?;
    Ok(raw.iter().all(|b| b.is_ascii_whitespace()))
}

/// Is this tree clean, ignoring one path prefix?
///
/// [`is_clean`] counts untracked files, which is right for a worktree and wrong
/// for **main**: main *contains* the worktrees dir, so on any repo that has not
/// gitignored `.claude/worktrees/` main is permanently dirty and every gate that
/// asks "is main clean" refuses forever. That is invisible on a repo whose own
/// hooks add the exclude, which is exactly how it stayed unnoticed.
///
/// So: `is_clean` for a worktree, this for main, with
/// `Config::worktrees_subdir_str` as the prefix — the same exclude `reconcile`
/// already passes to [`status()`] for the changed-file pane.
/// `Untracked::Each`, not `Collapsed`, and that is the whole trick. Git collapses
/// an untracked directory to its topmost entry, so a repo where nothing under
/// `.claude/` is tracked reports `.claude/` — *above* the exclude prefix, which
/// therefore never matches and main reads dirty anyway. Listing every untracked
/// file costs more, but this runs at a gate, not on every poll.
pub fn is_clean_excluding(cwd: &Path, exclude: Option<&str>) -> Result<bool> {
    let set = status(cwd, exclude, Untracked::Each)?;
    Ok(set.staged.is_empty() && set.unstaged.is_empty() && set.untracked.is_empty())
}
