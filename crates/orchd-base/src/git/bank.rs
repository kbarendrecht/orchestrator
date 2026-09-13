use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::model::Bank;

use super::*;

// ---------------------------------------------------------------------------
// Banked work
// ---------------------------------------------------------------------------

/// Where a workspace's banked work is kept.
///
/// **Not `refs/stash`.** That stack is shared by every worktree of the repo —
/// measured: a `git stash` in a worktree is `stash@{0}` in the main checkout — so
/// a bank left there could be popped into the wrong tree by somebody who never
/// pressed rebase. A ref of our own is per-workspace by name, invisible to
/// `git stash list`, and enough to keep the object alive: it survives
/// `git gc --prune=now`, which a bare `stash create` object does not promise to.
pub fn wip_ref(workspace: &str) -> String {
    // Ref names may not end in a dot or in `.lock`, and `validate_worktree_name`
    // allows both. Everything else it allows is already ref-safe.
    let safe = if workspace.ends_with('.') || workspace.ends_with(".lock") {
        format!("{workspace}-")
    } else {
        workspace.to_string()
    };
    format!("refs/orchd/wip/{safe}")
}

/// Bank this tree's uncommitted work under the workspace's ref, then clean the
/// tree. `None` means it was already clean and nothing was touched.
///
/// The ref goes on **before** the reset, so there is no window in which the work
/// exists only as a sha in the daemon's memory. Tracked changes only, as ever —
/// `stash create` has no `--include-untracked`, and an untracked file is not in
/// the rebase's way unless the base adds one at the same path, which git refuses
/// on its own.
pub fn bank_wip(cwd: &Path, workspace: &str) -> Result<Option<Bank>> {
    let Some(sha) = create_wip(cwd)? else {
        return Ok(None);
    };
    let at = wip_ref(workspace);
    git(cwd, &["update-ref", &at, &sha])
        .with_context(|| format!("banking this tree's work at {at}"))?;
    git(cwd, &["reset", "--hard", "-q"])?;
    Ok(Some(Bank {
        files: wip_files(cwd, &sha),
        sha,
    }))
}

/// Put banked work back and drop the ref.
///
/// The ref is dropped **only** on a clean apply. An apply that conflicts leaves
/// both sides in the working tree as `UU` *and* the bank standing, which is the
/// state worth being in: the conflict is where it can be resolved, and the work
/// as it was is still one object away.
pub fn restore_wip(cwd: &Path, workspace: &str) -> Result<()> {
    let at = wip_ref(workspace);
    let bank =
        banked_wip(cwd, workspace).with_context(|| format!("{at} holds nothing to put back"))?;
    // By sha rather than by the ref, so the failure names the object the way
    // `apply_wip`'s sentence promises — the ref is added beside it, because that is
    // the name that survives and the one a person types.
    apply_wip(cwd, &bank.sha, "the rebase finished")
        .with_context(|| format!("the work is banked at {at}"))?;
    discard_wip(cwd, workspace)
}

/// Forget banked work. The object stays until git collects it; the name does not.
pub fn discard_wip(cwd: &Path, workspace: &str) -> Result<()> {
    let at = wip_ref(workspace);
    git(cwd, &["update-ref", "-d", &at])
        .with_context(|| format!("dropping {at}"))
        .map(|_| ())
}

/// What this workspace has banked, if anything.
pub fn banked_wip(cwd: &Path, workspace: &str) -> Option<Bank> {
    let at = wip_ref(workspace);
    let sha = git(cwd, &["rev-parse", "--verify", "--quiet", &at]).ok()?;
    let sha = sha.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    Some(Bank {
        files: wip_files(cwd, &sha),
        sha,
    })
}

/// Every bank in the repository, as `(ref, bank)`.
///
/// One exec for the whole repo, which is what keeps this out of the sweep: refs
/// are per-repository, so the daemon can re-derive at boot what it knew before it
/// was restarted rather than asking each worktree.
///
/// **The ref, not the workspace it belongs to.** [`wip_ref`] mangles the two names
/// git will not take (`x.lock`, `trailing.`), and reading a workspace back out of
/// a ref would have to undo that — which it cannot do without guessing. The caller
/// matches the other way instead: it knows its workspaces, and `wip_ref` is a
/// function it can run on each of them.
pub fn all_banked(main: &Path) -> Vec<(String, Bank)> {
    let Ok(out) = git(
        main,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/orchd/wip",
        ],
    ) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|line| {
            let (name, sha) = line.trim().split_once(' ')?;
            Some((
                name.to_string(),
                Bank {
                    files: wip_files(main, sha),
                    sha: sha.to_string(),
                },
            ))
        })
        .collect()
}

/// How many tracked files a WIP commit carries. Zero when git will not say,
/// because a count is a label on a strip and never a decision.
pub(super) fn wip_files(cwd: &Path, sha: &str) -> u32 {
    git(cwd, &["stash", "show", "--name-only", sha])
        .map(|out| out.lines().filter(|l| !l.trim().is_empty()).count() as u32)
        .unwrap_or(0)
}

/// Stage one path, unstage it, or throw its working-tree changes away.
///
/// One function for the three because they are one decision with one guard in
/// front of it, and splitting them is how two of them keep a check the third
/// loses. Pathspec-terminated (`--`) on all three: a file called `-f` is a file,
/// not a flag.
///
/// **`Discard` cannot be undone by git.** `git restore` overwrites the working
/// tree from the index, so uncommitted content is gone — there is no reflog for a
/// file that was never committed. The confirm in front of it is not politeness;
/// it is the only thing between a click and lost work.
pub enum FileVerb {
    Stage,
    Unstage,
    Discard,
}

pub fn file_verb(cwd: &Path, verb: FileVerb, path: &str) -> Result<()> {
    let args: &[&str] = match verb {
        // `add` also covers an untracked file, which is the one row where staging
        // is the only verb on offer.
        FileVerb::Stage => &["add", "--", path],
        FileVerb::Unstage => &["restore", "--staged", "--", path],
        FileVerb::Discard => &["restore", "--", path],
    };
    git(cwd, args).map(|_| ())
}

/// Does `at_ref` contain `path`?
///
/// Asked before a blob URL is built, because a URL for a path the ref does not
/// have opens a 404 in the browser and that reads as the forge being broken rather
/// than as the file not being there. An untracked file is the ordinary case: the
/// changed-files pane lists it (`diff::DiffFile::untracked`) precisely because git
/// has never seen it.
///
/// `ls-tree` rather than `cat-file`, so a large file is not read to answer whether
/// it exists. Its exit status is 0 either way, so the *output* is the answer: empty
/// means no such path at that ref.
pub fn has_path_at(cwd: &Path, at_ref: &str, path: &str) -> bool {
    git(cwd, &["ls-tree", "--name-only", at_ref, "--", path])
        .map(|out| !out.trim().is_empty())
        .unwrap_or(false)
}

/// Which working tree has `branch` checked out, if any.
///
/// Asked because git allows one checkout per branch, so "main cannot return to
/// base" and "some worktree is sitting on base" are the same fact — and the bare
/// git refusal names the tree without saying that is what it means.
///
/// `--porcelain` rather than the human listing: the readable one pads with spaces
/// and puts the branch in brackets, and a path with a space in it then cannot be
/// told from the columns. Detached trees have no `branch` line at all and so
/// answer nothing, which is right.
///
/// **Resolved before it is handed back**, because the answer is *compared* — against
/// `main_checkout` and against a workspace's path, both canonical (`Config::parse`),
/// and a comparison across that boundary fails by deciding no worktree holds the
/// branch. Git resolves the path itself today (measured: a listing taken through a
/// symlinked checkout comes back resolved), so this is belt and braces rather than
/// a fix — it makes the invariant hold by construction instead of by a git
/// behaviour nothing documents. Falls back to the raw path, since a worktree whose
/// directory is gone cannot be canonicalised and is still the holder of record.
///
/// Worth knowing where it bites: on macOS `/tmp`, `/var` and `$TMPDIR` are symlinks
/// into `/private`, so an unresolved path on *either* side matches nothing. Two
/// tests handed one in and failed only on the macos-14 runner.
pub fn holder_of_branch(main: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let out = git(main, &["worktree", "list", "--porcelain"])?;
    let want = format!("refs/heads/{branch}");
    let mut at: Option<PathBuf> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            let raw = PathBuf::from(path);
            at = Some(std::fs::canonicalize(&raw).unwrap_or(raw));
        } else if line.strip_prefix("branch ") == Some(want.as_str()) {
            return Ok(at);
        }
    }
    Ok(None)
}

/// Give `tree` a branch of its own at the commit it already has, releasing
/// whatever it held.
///
/// The content does not move: the new branch starts at this tree's own HEAD, so
/// every commit and every file stays exactly where it is. Only the name changes,
/// which is what makes this safe to do to a tree nobody is working in — and the
/// name is the thing that was in the way.
///
/// Refuses a dirty tree, like every other move in here: a switch carries
/// uncommitted work with it, and `-c` at the same commit would leave that work on
/// a branch the user never chose.
///
/// Returns the branch it created. `stem` is uniquified, so calling this twice on
/// two trees cut from the same name does not collide.
pub fn release_branch(tree: &Path, stem: &str) -> Result<String> {
    let held = current_branch(tree)?;
    refuse_if_dirty(tree, stem)?;
    let fresh = free_branch(tree, stem);
    git(tree, &["switch", "-c", &fresh, "-q"])
        .with_context(|| format!("{} could not be moved off {held}", tree.display()))?;
    Ok(fresh)
}

/// Put a checkout back on `base`, unless something says not to.
///
/// Returns the branch it left, or `None` when it did nothing: already there, or
/// carrying uncommitted work. Refusing on a dirty tree is the whole safety
/// argument — a checkout takes uncommitted changes with it, and finding your work
/// sitting on the base branch is not recoverable by pressing back.
///
/// Blocking and repo-only on purpose: the daemon-side caller owns the "is anyone
/// still working here" half, and this half is what a test can drive against a real
/// repo.
pub fn park_on_base(cwd: &Path, base: &str, exclude: Option<&str>) -> Result<Option<String>> {
    let on = current_branch(cwd)?;
    if on == base {
        return Ok(None);
    }
    // Excluding, not plain: this only ever runs on main, which contains the
    // worktrees dir. Plain `is_clean` reads that as dirty and parking never
    // happens — silently, since "not clean" is a legitimate reason to do nothing.
    if !is_clean_excluding(cwd, exclude)? {
        return Ok(None);
    }
    switch_branch(cwd, base)?;
    Ok(Some(on))
}

/// Split out so the named-branch rule is testable without a network fetch.
pub(super) fn upstream_fetch_argv(upstream_ref: &str) -> Vec<&str> {
    let (remote, branch) = split_upstream(upstream_ref);
    vec!["fetch", remote, branch, "--no-tags"]
}
