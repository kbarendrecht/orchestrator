use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

use crate::model::{ChangedFile, FileSet, FileStatus};

/// Shell out to `git` rather than a library binding — you need fsmonitor and
/// the real worktree/remote semantics (§1).
fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Like [`git`] but returns the raw bytes, for `-z` output that is not valid
/// UTF-8 in the general case.
fn git_raw(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Whether a git command succeeded, for probes where failure is a valid answer.
fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// How much detail to ask for about untracked files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Untracked {
    /// One entry per untracked *directory*. What the rail wants: it runs on every
    /// hook event, for every workspace, including main — whose tree contains every
    /// worktree — and the result is broadcast to every client. One un-ignored build
    /// directory would otherwise become ten thousand rows in every snapshot.
    Collapsed,
    /// One entry per untracked *file*.
    ///
    /// What anything deciding what to *commit* needs. A collapsed `newdir/` cannot
    /// be counted, cannot be diffed, and cannot be refused one file at a time — so
    /// the manual phase listed a directory, showed none of it, and `git add -A`
    /// committed everything inside.
    Each,
}

/// Changed files for a workspace, grouped staged / unstaged / untracked (§4).
///
/// `exclude_worktrees` is set for main: main's file tree contains every
/// worktree, so without it you see every sibling session's work (§2).
pub fn status(cwd: &Path, exclude_worktrees: bool, untracked: Untracked) -> Result<FileSet> {
    let mode = match untracked {
        Untracked::Collapsed => "--untracked-files=normal",
        Untracked::Each => "--untracked-files=all",
    };
    let raw = git_raw(cwd, &["status", "--porcelain=v2", mode, "-z"])?;
    Ok(parse_status(&raw, exclude_worktrees))
}

fn parse_status(raw: &[u8], exclude_worktrees: bool) -> FileSet {
    let mut set = FileSet::default();
    let mut records = raw
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| String::from_utf8_lossy(r).into_owned())
        .peekable();

    while let Some(rec) = records.next() {
        let mut chars = rec.chars();
        let tag = chars.next().unwrap_or(' ');
        match tag {
            // Ordinary changed entry:
            //   1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
            '1' => {
                let fields: Vec<&str> = rec.splitn(9, ' ').collect();
                if fields.len() < 9 {
                    continue;
                }
                push_xy(&mut set, fields[1], fields[8], exclude_worktrees);
            }
            // Renamed or copied:
            //   2 <XY> ... <X><score> <path>\0<origPath>
            // The original path is its own NUL-separated record and is consumed
            // here so it is not mistaken for an entry of its own.
            '2' => {
                let fields: Vec<&str> = rec.splitn(10, ' ').collect();
                records.next();
                if fields.len() < 10 {
                    continue;
                }
                push_xy(&mut set, fields[1], fields[9], exclude_worktrees);
            }
            // Unmerged. Both sides count as unstaged work.
            'u' => {
                let fields: Vec<&str> = rec.splitn(11, ' ').collect();
                if let Some(path) = fields.last() {
                    if !skip(path, exclude_worktrees) {
                        set.unstaged.push(ChangedFile {
                            path: (*path).to_string(),
                            status: FileStatus::Unstaged,
                            code: "UU".to_string(),
                        });
                    }
                }
            }
            '?' => {
                let path = rec.strip_prefix("? ").unwrap_or("");
                if !path.is_empty() && !skip(path, exclude_worktrees) {
                    set.untracked.push(ChangedFile {
                        path: path.to_string(),
                        status: FileStatus::Untracked,
                        code: "??".to_string(),
                    });
                }
            }
            // '!' is ignored-file output, which is not requested here.
            _ => {}
        }
    }

    set.staged.sort_by(|a, b| a.path.cmp(&b.path));
    set.unstaged.sort_by(|a, b| a.path.cmp(&b.path));
    set.untracked.sort_by(|a, b| a.path.cmp(&b.path));
    set
}

/// `XY`: X is the staged status, Y the unstaged one. `.` means unmodified, and
/// a file can legitimately appear in both groups.
fn push_xy(set: &mut FileSet, xy: &str, path: &str, exclude_worktrees: bool) {
    if skip(path, exclude_worktrees) {
        return;
    }
    let mut it = xy.chars();
    let x = it.next().unwrap_or('.');
    let y = it.next().unwrap_or('.');
    if x != '.' {
        set.staged.push(ChangedFile {
            path: path.to_string(),
            status: FileStatus::Staged,
            code: xy.to_string(),
        });
    }
    if y != '.' {
        set.unstaged.push(ChangedFile {
            path: path.to_string(),
            status: FileStatus::Unstaged,
            code: xy.to_string(),
        });
    }
}

fn skip(path: &str, exclude_worktrees: bool) -> bool {
    exclude_worktrees && path.starts_with(".claude/worktrees/")
}

// ---------------------------------------------------------------------------
// Refs
// ---------------------------------------------------------------------------

pub fn current_branch(cwd: &Path) -> Result<String> {
    Ok(git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string())
}

pub fn head_sha(cwd: &Path) -> Result<String> {
    Ok(git(cwd, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// Does this branch hold commits its remote does not?
///
/// For when GitHub could not say what the remote head is. `@{upstream}` is whatever
/// the branch tracks; with no tracking branch there is nothing to compare against, so
/// the honest answer is "assume yes" — an unnecessary push is refused by the lease,
/// while a skipped one posts a reply about a commit nobody can see.
pub fn has_unpushed(cwd: &Path, branch: &str) -> bool {
    for range in [
        format!("@{{upstream}}..{branch}"),
        format!("origin/{branch}..{branch}"),
    ] {
        if let Ok(out) = Command::new("git")
            .args(["rev-list", "--count", &range])
            .current_dir(cwd)
            .output()
        {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).trim() != "0";
            }
        }
    }
    true
}

/// Who git will author a commit as here.
///
/// Wanted by [`amend_target`], which refuses to fold into a commit somebody else
/// wrote. Empty rather than an error when git has no `user.email`: an unset
/// identity means "match nobody", which degrades the fold to a plain HEAD amend
/// instead of failing the batch.
pub fn user_email(cwd: &Path) -> String {
    Command::new("git")
        .args(["config", "user.email"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Two-dot against the merge-base *commit*, not the ref, or develop's own
/// commits appear as your deletions (§5).
pub fn merge_base(cwd: &Path, upstream: &str) -> Result<String> {
    Ok(git(cwd, &["merge-base", upstream, "HEAD"])?
        .trim()
        .to_string())
}

/// Repo config from §4. fsmonitor is set on main only: a daemon per worktree
/// over a large monorepo puts the `inotify.max_user_watches` question straight
/// back, and worktree reconciles are event-driven off hooks anyway.
pub fn configure_repo(main: &Path) -> Result<()> {
    let _ = git(main, &["config", "core.fsmonitor", "true"]);
    let _ = git(main, &["config", "core.untrackedCache", "true"]);
    let _ = git(main, &["config", "fetch.writeCommitGraph", "true"]);
    Ok(())
}

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
pub fn unpushed(cwd: &Path, branch: &str, upstream: &str) -> Result<Unpushed> {
    let remote_ref = format!("refs/remotes/origin/{branch}");
    if !git_ok(cwd, &["rev-parse", "--verify", "--quiet", &remote_ref]) {
        // Nothing on origin, so measure against the upstream base instead:
        // that is exactly the set of commits that exist nowhere but here.
        let range = format!("{upstream}..HEAD");
        let commits = git(cwd, &["log", &range, "--oneline"])
            .map(|out| out.lines().map(|l| l.trim().to_string()).collect())
            .unwrap_or_else(|_| vec!["(could not resolve the upstream base)".to_string()]);
        return Ok(Unpushed::NeverPushed { commits });
    }
    let range = format!("origin/{branch}..HEAD");
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

// ---------------------------------------------------------------------------
// Worktrees
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct WorktreeEntry {
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
}

pub fn worktree_list(main: &Path) -> Result<Vec<WorktreeEntry>> {
    let out = git(main, &["worktree", "list", "--porcelain"])?;
    let mut entries = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(e) = cur.take() {
                entries.push(e);
            }
            cur = Some(WorktreeEntry {
                path: path.to_string(),
                branch: None,
                head: None,
            });
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            if let Some(e) = cur.as_mut() {
                e.head = Some(head.to_string());
            }
        } else if let Some(branch) = line.strip_prefix("branch ") {
            if let Some(e) = cur.as_mut() {
                e.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
            }
        }
    }
    if let Some(e) = cur {
        entries.push(e);
    }
    Ok(entries)
}

/// Removal is `git worktree remove` followed by `git worktree prune` (§2).
///
/// **Never `rm -rf` a worktree.** It contains `.plan/` symlinked to main's
/// `.plan/`, and a `vendor/` full of per-package symlinks into main. A recursive
/// delete that follows symlinks destroys the main checkout. If this refuses, the
/// refusal is surfaced — it is never escalated to `--force` and never falls back
/// to a filesystem delete.
pub fn worktree_remove(main: &Path, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    git(main, &["worktree", "remove", &path_str]).with_context(|| {
        format!(
            "git worktree remove refused for {}; not escalating to --force",
            path.display()
        )
    })?;
    let _ = git(main, &["worktree", "prune"]);
    Ok(())
}

/// Add a worktree checked out on an **existing** branch.
///
/// `claude --worktree` always cuts a fresh `worktree-<name>` from
/// `upstream/develop`, which is wrong for `/resolve`: that has to land on the
/// PR's own head branch (§8). The repo's `worktree-create` hook is therefore
/// not involved here, but `worktree-link` still runs at `SessionStart` and does
/// the symlinks, which is the same path §2 describes for rebuilding a worktree
/// on resume.
pub fn worktree_add_existing(main: &Path, path: &Path, branch: &str) -> Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    if branch_exists(main, branch) {
        git(main, &["worktree", "add", &path_str, branch])?;
        return Ok(());
    }
    // The head ref lives on the fork, since PRs are opened from origin (§6).
    let _ = git(main, &["fetch", "origin", branch, "--no-tags"]);
    let remote = format!("origin/{branch}");
    if !git_ok(main, &["rev-parse", "--verify", "--quiet", &remote]) {
        bail!("branch {branch} exists neither locally nor on origin");
    }
    git(main, &["worktree", "add", &path_str, "-b", branch, &remote])?;
    Ok(())
}

pub fn branch_exists(main: &Path, branch: &str) -> bool {
    git_ok(
        main,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

/// How far this branch has drifted from the upstream base.
///
/// `behind` is what makes the rebase affordance appear: commits on
/// `upstream/develop` that this branch does not have, i.e. other people's work
/// that has landed since you branched.
pub fn divergence(cwd: &Path, upstream: &str) -> Result<(u32, u32)> {
    let range = format!("{upstream}...HEAD");
    let out = git(cwd, &["rev-list", "--left-right", "--count", &range])?;
    let mut parts = out.split_whitespace();
    let behind = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let ahead = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Ok((behind, ahead))
}

/// Whether a rebase is stopped part-way in this worktree.
///
/// Checked on disk rather than inferred: a button that offers to rebase a tree
/// already mid-rebase would make a mess that is annoying to unpick.
pub fn rebase_in_progress(cwd: &Path) -> bool {
    let Ok(dir) = git(cwd, &["rev-parse", "--path-format=absolute", "--git-dir"]) else {
        return false;
    };
    let dir = Path::new(dir.trim());
    dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists()
}

/// Rebase onto the upstream base.
///
/// Never a merge: history stays linear, which is how this repo is worked (§5's
/// base choice depends on it too). A rebase that stops on conflicts is left
/// stopped — that is the state you resolve from — and reported rather than
/// silently aborted.
pub fn rebase_onto(cwd: &Path, upstream: &str) -> Result<()> {
    let out = Command::new("git")
        .args(["rebase", upstream])
        .current_dir(cwd)
        .output()
        .context("running git rebase")?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    if rebase_in_progress(cwd) {
        let files = conflicted_files(cwd).unwrap_or_default();
        bail!(
            "rebase stopped on conflicts in {} file(s): {}. Resolve them in a session, \
             or abort.",
            files.len(),
            files.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
        );
    }
    bail!(
        "rebase failed: {}",
        stderr
            .lines()
            .chain(stdout.lines())
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output")
    );
}

pub fn rebase_abort(cwd: &Path) -> Result<()> {
    git(cwd, &["rebase", "--abort"])?;
    Ok(())
}

pub fn conflicted_files(cwd: &Path) -> Result<Vec<String>> {
    let out = git(cwd, &["diff", "--name-only", "--diff-filter=U"])?;
    Ok(out.lines().map(|l| l.to_string()).collect())
}

pub fn fetch_upstream(main: &Path) -> Result<()> {
    git(main, &["fetch", "upstream", "develop", "--no-tags"])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The review flow's writes
// ---------------------------------------------------------------------------

/// Refs a push must never target, mirroring `guards/push.py`.
///
/// That guard is a `PreToolUse` hook on the **agent's** Bash, so a daemon-side
/// push never passes through it. The rest of its rules — plain `--force`,
/// `--set-upstream`, pushing to `upstream` — are structurally impossible below
/// because the command is a fixed string, but the protected-ref rule has to be
/// re-stated here or it simply is not enforced.
const PROTECTED: [&str; 4] = ["develop", "main", "master", "release"];

/// Push the PR's own branch, refusing to clobber anyone else's work.
///
/// `--force-with-lease` rather than `--force`: it fails when the remote moved
/// since the last fetch, which is exactly the "someone else pushed" case that
/// must not be overwritten. Never `-u`: rebinding upstream to origin breaks pull
/// tracking in a triangular remote setup.
pub fn push_with_lease(cwd: &Path, branch: &str) -> Result<()> {
    if PROTECTED.contains(&branch) {
        bail!("refusing to push to {branch}: open a PR instead");
    }
    let out = Command::new("git")
        .args(["push", "--force-with-lease", "origin", branch])
        .current_dir(cwd)
        .output()
        .context("running git push")?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // The lease failing is the one refusal worth naming: it means the remote
    // moved, and the fix is to look at both sides rather than push harder.
    if err.contains("stale info") || err.contains("fetch first") || err.contains("rejected") {
        bail!(
            "push refused: {branch} moved on origin since this review started. \
             Someone else pushed, or /green ran. Re-triage rather than overwrite it."
        );
    }
    bail!(
        "push failed: {}",
        err.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output")
    );
}

/// Who last touched a line, and who wrote that commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blame {
    pub sha: String,
    pub author_email: String,
}

/// `git blame` one line of the **committed** state.
///
/// `rev` is which committed state. `None` blames the working tree, which is only
/// correct *before* anything has been applied — a dirty tree attributes the line
/// to the uncommitted change (an all-zero sha), which is nobody's commit and
/// cannot be a fixup target, so this returns `None` for it.
///
/// `Some("HEAD")` reads through a dirty tree, which is what the manual phase
/// needs: the human has already edited the very line being blamed, so blaming the
/// working tree would degrade every manual fold to a plain HEAD amend and the pass
/// would never do its job. Measured: `git blame HEAD -L n,n` returns the owning
/// commit where the bare form returns all zeros.
pub fn blame_line(cwd: &Path, rev: Option<&str>, path: &str, line: u32) -> Result<Option<Blame>> {
    let range = format!("{line},{line}");
    let mut args = vec!["blame"];
    if let Some(r) = rev {
        args.push(r);
    }
    args.extend_from_slice(&["-L", &range, "--porcelain", "--", path]);
    let out = Command::new("git")
        .args(&args)
        .current_dir(cwd)
        .output()
        .context("running git blame")?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let sha = match lines.next().and_then(|l| l.split_whitespace().next()) {
        Some(s) if !s.chars().all(|c| c == '0') => s.to_string(),
        // All-zero means "not committed yet".
        _ => return Ok(None),
    };
    let email = text
        .lines()
        .find_map(|l| l.strip_prefix("author-mail "))
        .map(|m| m.trim_matches(['<', '>']).to_string())
        .unwrap_or_default();
    Ok(Some(Blame {
        sha,
        author_email: email,
    }))
}

/// Where the accepted changes should be folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Amend {
    /// Fold into this commit of the PR's own history, keeping the subject of
    /// each commit true to what it contains.
    Fixup(String),
    /// Amend `HEAD` instead, with the reason — shown so the fallback is visible
    /// rather than silent.
    Head(String),
    /// Make a **new** commit, because rewriting anything here would rewrite work
    /// that is not ours. The reason is shown for the same purpose.
    OnTop(String),
}

/// Who git will actually author a commit as, asked of git rather than of config.
///
/// `git config user.email` is empty in a container that never set one — and git
/// commits there anyway, as `you@hostname`. Reading the config would report "no
/// identity", which the authorship checks below would take to mean *every* commit
/// belongs to somebody else.
pub fn effective_email(cwd: &Path) -> Option<String> {
    let ident = Command::new("git")
        .args(["var", "GIT_AUTHOR_IDENT"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())?;
    // `Name <email> 1699999999 +0200`
    let email = ident
        .split('<')
        .nth(1)?
        .split('>')
        .next()?
        .trim()
        .to_string();
    (!email.is_empty()).then(|| email.to_lowercase())
}

/// Every author `git log <args>` selects, lowercased.
///
/// `None` when git could not answer. The callers read that as "cannot tell", which
/// has to degrade rather than proceed: an empty list would mean "nobody else is
/// involved" and authorise a rewrite. Takes the arguments as a slice because
/// `["-1 HEAD"]` is one argument git cannot parse — which is how the first draft of
/// this check silently never fired.
fn authors_in(cwd: &Path, args: &[&str]) -> Option<Vec<String>> {
    let mut argv = vec!["log", "--format=%ae"];
    argv.extend_from_slice(args);
    let out = Command::new("git")
        .args(&argv)
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_lowercase())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

/// Does `rev` have more than one parent?
fn is_merge(cwd: &Path, rev: &str) -> bool {
    Command::new("git")
        .args(["rev-list", "--parents", "-n", "1", rev])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .count()
                > 2
        })
        .unwrap_or(false)
}

/// The **only** way to build an [`Amend::Head`].
///
/// Amending `HEAD` rewrites it, so the "someone else authored it" guard above is
/// worthless if the fallback then rewrites `HEAD` regardless — which is exactly
/// what happened: `Amend::Head("<sha> is <email>'s commit")` was a value whose own
/// text said it had refused, handed to a `git commit --amend`.
///
/// Note the discriminator is **authorship, not publication**. It has to be: a batch
/// only starts when the branch is level with origin, so `HEAD` is always already
/// pushed. The question is never "has anyone seen this commit", it is "is it mine
/// to rewrite".
pub fn head_or_on_top(cwd: &Path, my_email: Option<&str>, why: String) -> Amend {
    let Some(mine) = my_email else {
        return Amend::OnTop(format!(
            "{why}, and git has no identity here so no commit can be shown to be yours"
        ));
    };
    if is_merge(cwd, "HEAD") {
        return Amend::OnTop(format!("{why}, and HEAD is a merge"));
    }
    match authors_in(cwd, &["-n", "1", "HEAD"]).as_deref() {
        Some([author, ..]) if author != mine => {
            Amend::OnTop(format!("{why}, and HEAD is {author}'s commit"))
        }
        Some(_) => Amend::Head(why),
        None => Amend::OnTop(format!("{why}, and who wrote HEAD could not be read")),
    }
}

/// Decide the fixup target for a set of touched lines, degrading to `HEAD`.
///
/// The original command asked a human to fold each change "into the commit that
/// owns it"; `git blame` answers that mechanically. Three cases degrade, each
/// deliberately:
///
/// - **the lines disagree** — one commit cannot be two, and splitting the batch
///   per target is more history surgery than this is worth;
/// - **the line came from the base branch** — it is not the PR's commit to
///   rewrite;
/// - **someone else authored it** — rewriting a colleague's commit under your own
///   force-push is not ours to do, and it changes a sha they may have checked out.
pub fn amend_target(
    cwd: &Path,
    rev: Option<&str>,
    merge_base: &str,
    touched: &[(String, u32)],
    my_email: &str,
) -> Result<Amend> {
    // The caller's identity wins; `effective_email` is the fallback for when it is
    // empty, which is what `git config user.email` returns in a container that never
    // set one — git still commits there, as `you@hostname`, so "no identity" must not
    // be read as "every commit belongs to somebody else".
    let mine = Some(my_email.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .or_else(|| effective_email(cwd));
    let degrade = |why: String| head_or_on_top(cwd, mine.as_deref(), why);

    if touched.is_empty() {
        return Ok(degrade("nothing to attribute".into()));
    }

    let mut target: Option<Blame> = None;
    for (path, line) in touched {
        let Some(b) = blame_line(cwd, rev, path, *line)? else {
            return Ok(degrade(format!("{path}:{line} is not in any commit yet")));
        };
        match &target {
            None => target = Some(b),
            Some(t) if t.sha == b.sha => {}
            Some(t) => {
                return Ok(degrade(format!(
                    "the changes span {} and {}",
                    short(&t.sha),
                    short(&b.sha)
                )))
            }
        }
    }
    let hit = target.expect("non-empty touched set");

    // An ancestor of the merge base came from the base branch, not this PR.
    if is_ancestor(cwd, &hit.sha, merge_base) {
        return Ok(degrade(format!("{} predates this branch", short(&hit.sha))));
    }

    // **The whole range, not just the target.** `fold_in` autosquashes with
    // `rebase -i <sha>~1`, which replays every commit from `<sha>` to `HEAD` and
    // gives each a new sha — so a colleague's commit sitting *on top* of the one
    // that owns this line is rewritten and force-pushed, without this path ever
    // being taken. Checking only `hit.author_email` guarded the target and left
    // its descendants wide open.
    let range = format!("{}^..HEAD", hit.sha);
    let in_range = authors_in(cwd, &[&range]).or_else(|| {
        // No `^` on a root commit, so ask for the commit itself.
        authors_in(cwd, &["-n", "1", &hit.sha])
    });
    match in_range {
        None => return Ok(degrade("who wrote this branch could not be read".into())),
        Some(authors) => {
            if let Some(other) = authors.iter().find(|a| Some(a.as_str()) != mine.as_deref()) {
                return Ok(degrade(format!(
                    "folding into {} would rewrite {}'s commit above it",
                    short(&hit.sha),
                    other
                )));
            }
        }
    }
    // A root commit has no `~1` for the rebase to start from.
    if !rev_exists(cwd, &format!("{}~1", hit.sha)) {
        return Ok(degrade(format!(
            "{} is the first commit, so there is nothing to rebase onto",
            short(&hit.sha)
        )));
    }
    Ok(Amend::Fixup(hit.sha))
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Does this revision resolve?
fn rev_exists(cwd: &Path, rev: &str) -> bool {
    git_ok(cwd, &["rev-parse", "--verify", "--quiet", rev])
}

/// Is `a` an ancestor of `b`? Exit status only, so a failure means "no".
fn is_ancestor(cwd: &Path, a: &str, b: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", a, b])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Commit everything staged-or-not into the shape [`Amend`] chose.
///
/// `Fixup` writes a `fixup!` commit and then autosquashes it away, so the PR's
/// history keeps one commit per change rather than growing a "fix review" commit.
/// **`--autosquash` is silently ignored without `-i`**, which is why the
/// sequence editor is stubbed out rather than the flag used alone.
///
/// A conflict during the rebase aborts and reports: there is no session attached
/// to a button press to resolve one, and leaving a stopped rebase behind would
/// strand the worktree.
pub fn fold_in(cwd: &Path, amend: &Amend) -> Result<()> {
    git(cwd, &["add", "-A"])?;
    match amend {
        Amend::OnTop(why) => {
            // `--amend` succeeds on an empty staged diff; `commit -m` does not, and
            // a hook that reverted an edit back to HEAD's content would otherwise
            // turn a silent success into a hard error.
            if git_ok(cwd, &["diff", "--cached", "--quiet"]) {
                return Ok(());
            }
            // Never `fixup!`/`squash!`: a later batch's autosquash over this range
            // would silently absorb it.
            git(cwd, &["commit", "-m", &format!("review batch: {why}")])?;
            Ok(())
        }
        Amend::Head(_) => {
            git(cwd, &["commit", "--amend", "--no-edit"])?;
            Ok(())
        }
        Amend::Fixup(sha) => {
            git(cwd, &["commit", "--fixup", sha])?;
            let out = Command::new("git")
                .args(["rebase", "-i", "--autosquash", &format!("{sha}~1")])
                .env("GIT_SEQUENCE_EDITOR", "true")
                .env("GIT_EDITOR", "true")
                .current_dir(cwd)
                .output()
                .context("running autosquash rebase")?;
            if out.status.success() {
                return Ok(());
            }
            let files = conflicted_files(cwd).unwrap_or_default();
            if rebase_in_progress(cwd) {
                let _ = rebase_abort(cwd);
            }
            bail!(
                "could not fold into {}: the rebase conflicted{}. The change is still \
                 committed on top; fold it by hand or leave it.",
                short(sha),
                if files.is_empty() {
                    String::new()
                } else {
                    format!(" in {}", files.join(", "))
                }
            );
        }
    }
}

/// Commit the worktree as it stands — the gate's `commit…` button.
pub fn commit_all(cwd: &Path, message: &str) -> Result<()> {
    anyhow::ensure!(!message.trim().is_empty(), "a commit needs a message");
    git(cwd, &["add", "-A"])?;
    git(cwd, &["commit", "-m", message])?;
    Ok(())
}

/// Stash the worktree — the gate's `stash` button.
///
/// Never popped automatically: popping onto a branch the review just amended can
/// conflict, and silently juggling your uncommitted work is worse than leaving it
/// where you put it. Untracked files go too, or the tree is not actually clean.
pub fn stash(cwd: &Path) -> Result<()> {
    git(
        cwd,
        &[
            "stash",
            "push",
            "--include-untracked",
            "-m",
            "orchd: before a review batch",
        ],
    )?;
    Ok(())
}

/// What running the repo's pre-commit hooks concluded.
#[derive(Debug, PartialEq, Eq)]
pub enum PreCommit {
    /// No `.pre-commit-config.yaml`, so there is nothing configured to run.
    NotConfigured,
    /// Configured but `pre-commit` is not on PATH. A warning, not a stop: that
    /// is an environment problem, and blocking a whole review on it is worse
    /// than pushing code the local hooks did not see. (CI still runs them.)
    NotInstalled,
    /// Every hook passed and nothing was rewritten.
    Passed,
    /// A hook failed. A hard stop — the daemon cannot fix a lint error.
    Failed(String),
    /// The hooks passed but **rewrote files**. Also a stop: what would land is
    /// no longer what was approved on the cards, and absorbing the difference
    /// silently is exactly what the design refuses. The paths are handed back so
    /// the extra delta can be shown.
    Reformatted(Vec<String>),
}

/// Run the repo's pre-commit hooks over the files just written.
///
/// Detected rather than configured: if `.pre-commit-config.yaml` is absent there
/// is nothing to run. Scoped with `--files` rather than `--all-files`, because the
/// batch is answerable for what it wrote and not for the rest of the tree.
///
/// Called with the patches applied but **not yet committed**, so a rewrite can be
/// refused with nothing to undo.
pub fn pre_commit(cwd: &Path, files: &[String]) -> Result<PreCommit> {
    if !cwd.join(".pre-commit-config.yaml").exists() {
        return Ok(PreCommit::NotConfigured);
    }
    if files.is_empty() {
        return Ok(PreCommit::Passed);
    }

    // Hashes before and after: the only reliable way to tell "passed" from
    // "passed and rewrote your file" is to look.
    let before = hash_files(cwd, files);

    let mut args: Vec<&str> = vec!["run", "--files"];
    args.extend(files.iter().map(String::as_str));
    let out = match Command::new("pre-commit")
        .args(&args)
        .current_dir(cwd)
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PreCommit::NotInstalled),
        Err(e) => return Err(e).context("running pre-commit"),
    };

    let rewritten: Vec<String> = files
        .iter()
        .filter(|f| {
            let h = hash_one(cwd, f);
            before.get(*f).map(|b| b != &h).unwrap_or(false)
        })
        .cloned()
        .collect();
    // **Exit status first.** `fail_fast` is off by default, so a formatter
    // rewriting a file and a linter erroring in the same run is ordinary — and
    // checking `rewritten` first reported that as a mere reformat, made the
    // `Failed` arm below unreachable, and threw the hook's own output away.
    // `write_batch` refuses on either, so it never showed; `write_manual` only
    // logs a reformat, so it committed and pushed code that failed lint.
    if !out.status.success() {
        // Hooks report on stdout; stderr carries pre-commit's own troubles.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail: String = stdout
            .lines()
            .chain(stderr.lines())
            .filter(|l| !l.trim().is_empty())
            .take(20)
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(PreCommit::Failed(detail));
    }
    if !rewritten.is_empty() {
        return Ok(PreCommit::Reformatted(rewritten));
    }
    Ok(PreCommit::Passed)
}

fn hash_files(cwd: &Path, files: &[String]) -> std::collections::HashMap<String, u64> {
    files
        .iter()
        .map(|f| (f.clone(), hash_one(cwd, f)))
        .collect()
}

/// Content hash, not mtime: a hook can rewrite a file to the same bytes, and a
/// checkout can change mtime without changing content (`edit.rs` makes the same
/// choice for the same reason).
fn hash_one(cwd: &Path, rel: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match std::fs::read(cwd.join(rel)) {
        Ok(bytes) => bytes.hash(&mut h),
        // A hook may legitimately delete a file; absent hashes to a constant
        // distinct from any content.
        Err(_) => 0u8.hash(&mut h),
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(parts: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p.as_bytes());
            v.push(0);
        }
        v
    }

    #[test]
    fn splits_staged_and_unstaged_from_one_entry() {
        // XY = "MM": staged modification and a further unstaged one.
        let raw = rec(&["1 MM N... 100644 100644 100644 aaa bbb src/Foo.php"]);
        let set = parse_status(&raw, false);
        assert_eq!(set.staged.len(), 1);
        assert_eq!(set.unstaged.len(), 1);
        assert_eq!(set.staged[0].path, "src/Foo.php");
    }

    #[test]
    fn a_staged_only_entry_does_not_appear_as_unstaged() {
        let raw = rec(&["1 M. N... 100644 100644 100644 aaa bbb src/Foo.php"]);
        let set = parse_status(&raw, false);
        assert_eq!(set.staged.len(), 1);
        assert!(set.unstaged.is_empty());
    }

    #[test]
    fn consumes_the_original_path_of_a_rename() {
        let raw = rec(&[
            "2 R. N... 100644 100644 100644 aaa bbb R100 src/New.php",
            "src/Old.php",
            "? untracked.txt",
        ]);
        let set = parse_status(&raw, false);
        assert_eq!(set.staged.len(), 1);
        assert_eq!(set.staged[0].path, "src/New.php");
        // The old path must not be read back as an entry of its own.
        assert_eq!(set.untracked.len(), 1);
        assert_eq!(set.untracked[0].path, "untracked.txt");
    }

    #[test]
    fn excludes_sibling_worktrees_from_mains_view() {
        let raw = rec(&["? .claude/worktrees/other/file.php", "? src/Mine.php"]);
        let set = parse_status(&raw, true);
        assert_eq!(set.untracked.len(), 1);
        assert_eq!(set.untracked[0].path, "src/Mine.php");
    }

    #[test]
    fn keeps_worktree_paths_when_not_excluding() {
        let raw = rec(&["? .claude/worktrees/other/file.php"]);
        let set = parse_status(&raw, false);
        assert_eq!(set.untracked.len(), 1);
    }

    fn scratch_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orchd-git-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "one"]);
        dir
    }

    #[test]
    fn adds_a_worktree_on_an_existing_branch() {
        // /resolve has to land on the PR's own head branch, not a fresh one cut
        // from upstream/develop (§8).
        let repo = scratch_repo();
        std::process::Command::new("git")
            .args(["branch", "feature/x"])
            .current_dir(&repo)
            .output()
            .unwrap();

        let wt = repo.join(".claude/worktrees/pr-1");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        worktree_add_existing(&repo, &wt, "feature/x").expect("worktree add");

        assert!(wt.join("f.txt").exists());
        assert_eq!(current_branch(&wt).unwrap(), "feature/x");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn refuses_a_branch_that_exists_nowhere() {
        let repo = scratch_repo();
        let wt = repo.join(".claude/worktrees/pr-2");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        let err = worktree_add_existing(&repo, &wt, "feature/nope").unwrap_err();
        assert!(
            format!("{err:#}").contains("neither locally nor on origin"),
            "unexpected: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn unpushed_commits_block_teardown() {
        assert!(Unpushed::NeverPushed {
            commits: vec!["abc work".into()]
        }
        .blocks_teardown());
        assert!(Unpushed::Ahead {
            commits: vec!["abc x".into()]
        }
        .blocks_teardown());
        assert!(!Unpushed::UpToDate.blocks_teardown());
    }

    #[test]
    fn a_fresh_worktree_carrying_nothing_can_still_be_removed() {
        // Branched straight off upstream/develop and never pushed: there is no
        // work to lose, so teardown must not be blocked forever.
        assert!(!Unpushed::NeverPushed { commits: vec![] }.blocks_teardown());
    }
    /// A repo with two commits so blame has something to distinguish, plus a
    /// `base` ref standing in for the merge base.
    fn amend_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orchd-amend-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "me@here"]);
        run(&["config", "user.name", "me"]);
        // Commit 1 stands in for the base branch's history.
        std::fs::write(dir.join("f.txt"), "base1\nbase2\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "from develop"]);
        run(&["branch", "base"]);
        // Commit 2 is the PR's own.
        std::fs::write(dir.join("f.txt"), "base1\nbase2\nmine\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "the PR commit"]);
        dir
    }

    fn sha_of(dir: &Path, subject: &str) -> String {
        let out = std::process::Command::new("git")
            .args(["log", "--format=%H %s"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| l.contains(subject))
            .map(|l| l.split_whitespace().next().unwrap().to_string())
            .expect("commit")
    }

    #[test]
    fn a_line_from_this_prs_own_commit_is_a_fixup_target() {
        let d = amend_repo();
        let want = sha_of(&d, "the PR commit");
        let got = amend_target(&d, None, "base", &[("f.txt".into(), 3)], "me@here").unwrap();
        assert_eq!(got, Amend::Fixup(want));
    }

    #[test]
    fn a_line_that_predates_the_branch_degrades_to_head() {
        // Line 1 came from `develop`; it is not this PR's commit to rewrite.
        let d = amend_repo();
        match amend_target(&d, None, "base", &[("f.txt".into(), 1)], "me@here").unwrap() {
            Amend::Head(why) => assert!(why.contains("predates"), "{why}"),
            other => panic!("expected Head, got {other:?}"),
        }
    }

    #[test]
    fn someone_elses_commit_is_never_rewritten() {
        // Rewriting a colleague's commit under your own force-push changes a sha
        // they may have checked out.
        //
        // This used to assert `Head`, which was the bug wearing the test's clothes:
        // `fold_in`'s `Head` arm runs `git commit --amend`, so "degrading" to it
        // rewrote the very commit the guard had just refused to touch. When nothing
        // on the branch is yours, the only safe fold is a new commit.
        let d = amend_repo();
        match amend_target(&d, None, "base", &[("f.txt".into(), 3)], "someone@else").unwrap() {
            Amend::OnTop(why) => assert!(why.contains("me@here"), "must name them: {why}"),
            other => panic!("expected OnTop, got {other:?}"),
        }
    }

    #[test]
    fn a_colleagues_commit_stacked_on_the_target_is_not_rewritten_either() {
        // The half the first attempt missed. `fold_in` autosquashes with
        // `rebase -i <sha>~1`, which replays every commit from the target to HEAD and
        // gives each a new sha — so guarding only the target left its descendants
        // wide open, and this path was never taken.
        let d = amend_repo();
        let target = sha_of(&d, "the PR commit");
        // A colleague commits on top of the commit that owns the line.
        std::fs::write(d.join("theirs.txt"), "their work\n").unwrap();
        for args in [
            vec!["add", "-A"],
            vec![
                "-c",
                "user.email=bob@example.com",
                "-c",
                "user.name=bob",
                "commit",
                "-qm",
                "bob's commit",
            ],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&d)
                .output()
                .expect("git");
        }

        match amend_target(&d, None, "base", &[("f.txt".into(), 3)], "me@here").unwrap() {
            Amend::OnTop(why) => {
                assert!(why.contains("bob@example.com"), "must name them: {why}");
                assert!(why.contains(&target[..7]), "and the target: {why}");
            }
            other => panic!("a stacked commit must not be rewritten, got {other:?}"),
        }
    }

    #[test]
    fn committing_on_top_leaves_every_existing_sha_alone() {
        // The whole point: an amend rewrites, a new commit does not.
        let d = amend_repo();
        let before_head = head_sha(&d).unwrap();
        let before_count = std::process::Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(&d)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap();

        std::fs::write(d.join("f.txt"), "base1\nbase2\nmine\nby hand\n").unwrap();
        fold_in(&d, &Amend::OnTop("HEAD is bob's commit".into())).unwrap();

        let after_count = std::process::Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(&d)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap();
        assert_eq!(
            after_count.parse::<u32>().unwrap(),
            before_count.parse::<u32>().unwrap() + 1,
            "one commit added, not folded"
        );
        // Byte-identical: nothing anybody may have checked out has moved.
        let parent = std::process::Command::new("git")
            .args(["rev-parse", "HEAD^"])
            .current_dir(&d)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap();
        assert_eq!(parent, before_head, "the old HEAD must be untouched");
        assert!(is_clean(&d).unwrap());
    }

    #[test]
    fn committing_on_top_with_nothing_staged_is_not_an_error() {
        // `--amend` succeeds on an empty diff and `commit -m` does not, so a hook
        // that reverted an edit back to HEAD's content would turn a silent success
        // into a hard error — which, mid-batch, is a 500 with HEAD already moved.
        let d = amend_repo();
        let before = head_sha(&d).unwrap();
        fold_in(&d, &Amend::OnTop("nothing to attribute".into())).unwrap();
        assert_eq!(
            head_sha(&d).unwrap(),
            before,
            "nothing to commit, nothing done"
        );
    }

    #[test]
    fn changes_spanning_two_commits_degrade_to_head() {
        let d = amend_repo();
        let got = amend_target(
            &d,
            None,
            "base",
            &[("f.txt".into(), 1), ("f.txt".into(), 3)],
            "me@here",
        )
        .unwrap();
        match got {
            // Line 1 is from develop, so the span check or the predates check
            // fires first — either way it must not be a fixup.
            Amend::Head(_) => {}
            other => panic!("expected Head, got {other:?}"),
        }
    }

    #[test]
    fn blame_on_an_uncommitted_line_is_none() {
        // Blame must run against the committed state: a dirty tree attributes
        // the line to an all-zero sha, which is nobody's commit.
        let d = amend_repo();
        std::fs::write(d.join("f.txt"), "base1\nbase2\nmine\nfresh\n").unwrap();
        assert_eq!(blame_line(&d, None, "f.txt", 4).unwrap(), None);
    }

    #[test]
    fn a_fixup_folds_away_and_keeps_the_commit_count() {
        let d = amend_repo();
        let target = sha_of(&d, "the PR commit");
        let before = count_commits(&d);

        // Edit the line that commit owns, then fold into it.
        std::fs::write(d.join("f.txt"), "base1\nbase2\nMINE\n").unwrap();
        fold_in(&d, &Amend::Fixup(target)).unwrap();

        assert_eq!(count_commits(&d), before, "fixup should not add a commit");
        assert!(!subjects(&d).iter().any(|s| s.starts_with("fixup!")));
        let content = std::fs::read_to_string(d.join("f.txt")).unwrap();
        assert!(content.contains("MINE"));
    }

    #[test]
    fn a_head_amend_also_keeps_the_commit_count() {
        let d = amend_repo();
        let before = count_commits(&d);
        std::fs::write(d.join("g.txt"), "new\n").unwrap();
        fold_in(&d, &Amend::Head("spanned".into())).unwrap();
        assert_eq!(count_commits(&d), before);
        assert!(d.join("g.txt").exists());
    }

    fn count_commits(dir: &Path) -> usize {
        subjects(dir).len()
    }

    fn subjects(dir: &Path) -> Vec<String> {
        let out = std::process::Command::new("git")
            .args(["log", "--format=%s"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect()
    }

    #[test]
    fn a_push_to_a_protected_ref_is_refused_before_it_runs() {
        // guards/push.py only hooks the agent's Bash; a daemon push bypasses it.
        let d = amend_repo();
        for branch in ["develop", "main", "master", "release"] {
            let err = push_with_lease(&d, branch).unwrap_err().to_string();
            assert!(err.contains("refusing to push"), "{branch}: {err}");
        }
    }

    #[test]
    fn commit_all_needs_a_message_and_then_cleans_the_tree() {
        let d = amend_repo();
        std::fs::write(d.join("h.txt"), "x\n").unwrap();
        assert!(commit_all(&d, "   ").is_err());
        commit_all(&d, "wip").unwrap();
        assert!(is_clean(&d).unwrap());
    }

    #[test]
    fn stash_clears_untracked_files_too() {
        // Otherwise the tree is not actually clean and the gate would still fire.
        let d = amend_repo();
        std::fs::write(d.join("tracked-edit.txt"), "x\n").unwrap();
        std::fs::write(d.join("f.txt"), "base1\nbase2\nedited\n").unwrap();
        stash(&d).unwrap();
        assert!(is_clean(&d).unwrap());
        assert!(!d.join("tracked-edit.txt").exists());
    }
}
