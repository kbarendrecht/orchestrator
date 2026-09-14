use anyhow::Result;
use std::path::{Path, PathBuf};

use super::*;

// ---------------------------------------------------------------------------
// Refs
// ---------------------------------------------------------------------------

pub fn current_branch(cwd: &Path) -> Result<String> {
    Ok(git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string())
}

/// The real `HEAD` file for this workspace, wherever git keeps it.
///
/// Not `<cwd>/.git/HEAD`: in a linked worktree `<cwd>/.git` is a pointer *file*,
/// and the HEAD that a checkout rewrites lives under the common dir at
/// `<main>/.git/worktrees/<id>/HEAD`. `--absolute-git-dir` resolves both — the
/// checkout's `.git` for main, the worktree's admin dir for a worktree — so the
/// HEAD poller watches the file that actually moves.
pub fn head_file(cwd: &Path) -> Result<PathBuf> {
    let dir = git(cwd, &["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(dir.trim()).join("HEAD"))
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
        if let Ok(out) = run(cwd, &["rev-list", "--count", &range]) {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).trim() != "0";
            }
        }
    }
    true
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
