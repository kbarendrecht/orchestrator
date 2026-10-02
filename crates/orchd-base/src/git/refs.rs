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

/// `core.fsmonitor` is a boolean from git 2.37. Before that it is a **hook path**.
const FSMONITOR_BOOL: (u32, u32) = (2, 37);

/// Repo config from §4. fsmonitor is set on main only: a daemon per worktree
/// over a large monorepo puts the `inotify.max_user_watches` question straight
/// back, and worktree reconciles are event-driven off hooks anyway.
///
/// **`core.fsmonitor true` is a command on git below 2.37, and this wrote it
/// anyway.** The boolean arrived in 2.37; older git reads the value as a hook
/// path, so every `git status` ran a program called `true` and then warned
/// `Empty last update token` because the "hook" said nothing. Measured on git
/// 2.34.1, in this project's own checkout, which carried the setting:
/// `trace: run_command: …; true 2 ''` on every status. Nothing failed — the empty
/// token sends git back to a full scan, which is what it would have done without
/// the setting — so the cost was a process and a warning per command, and the
/// speed-up the line exists for was never there. It was written `let _ =`, which
/// is why it went unnoticed for as long as it did.
pub fn configure_repo(main: &Path) -> Result<()> {
    // Both are older than the floor and behave on every git this runs on:
    // `core.untrackedCache` is 2.8, `fetch.writeCommitGraph` is 2.24. Checked on
    // 2.34 rather than assumed — neither spawns anything, neither warns.
    let _ = git(main, &["config", "core.untrackedCache", "true"]);
    let _ = git(main, &["config", "fetch.writeCommitGraph", "true"]);

    /* **Repaired here rather than by a migration**, because this already runs on
    every daemon start for every checkout — so a repo configured by an older
    build is fixed the next time its daemon comes up, with nothing to version
    and no list of things to carry. Only the exact value `true` is touched: that
    string is this daemon's own signature, and somebody running a real fsmonitor
    has a *path* there that must survive. */
    let ours = git_version().is_some_and(|v| v >= FSMONITOR_BOOL);
    if ours {
        let _ = git(main, &["config", "core.fsmonitor", "true"]);
    } else if fsmonitor_is_ours(main) {
        let _ = git(main, &["config", "--unset", "core.fsmonitor"]);
    }
    Ok(())
}

/// Is `core.fsmonitor` the literal `true` this daemon writes, rather than a hook
/// somebody chose? Unset answers false, which is the same answer as somebody
/// else's hook: in both cases there is nothing of ours to take away.
fn fsmonitor_is_ours(main: &Path) -> bool {
    git(main, &["config", "--get", "core.fsmonitor"])
        .map(|v| v.trim() == "true")
        .unwrap_or(false)
}
