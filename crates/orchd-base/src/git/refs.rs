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
    /* **Read before either branch writes.** The boolean arm used to set `true`
    unconditionally, which took somebody's real `rs-git-fsmonitor` path out on
    every daemon start — the paragraph above had always said that path must
    survive, and `a_real_fsmonitor_hook_survives_the_repair` had always asserted
    it. It went unseen because the arm is only reached on 2.37 or newer: a
    development machine on 2.34 skips the write and the test passes having
    exercised nothing. CI's git is newer and it was red for two commits. */
    let current = git(main, &["config", "--get", "core.fsmonitor"])
        .map(|v| v.trim().to_string())
        .ok();
    match fsmonitor_plan(
        current.as_deref(),
        git_version().is_some_and(|v| v >= FSMONITOR_BOOL),
    ) {
        Fsmonitor::Write => {
            let _ = git(main, &["config", "core.fsmonitor", "true"]);
        }
        Fsmonitor::Unset => {
            let _ = git(main, &["config", "--unset", "core.fsmonitor"]);
        }
        Fsmonitor::Leave => {}
    }
    Ok(())
}

/// What the repair does to `core.fsmonitor`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Fsmonitor {
    Write,
    Unset,
    Leave,
}

/// The repair's whole decision, as a function of the value found and whether this
/// git reads the key as a boolean.
///
/// **Separated from the writing so it can be tested on any git.** The bug this
/// replaces lived in the boolean arm, which a machine on 2.34 never reaches — so
/// `a_real_fsmonitor_hook_survives_the_repair` passed locally for as long as the
/// bug existed and only ever went red on a runner. The six cases below are
/// checked without asking what git is installed.
pub(super) fn fsmonitor_plan(current: Option<&str>, boolean_git: bool) -> Fsmonitor {
    // Unset is neither ours nor theirs, which is the right answer twice over:
    // nothing of ours to take away, and nothing of theirs to write over.
    let ours = current == Some("true");
    let theirs = current.is_some() && !ours;
    match (boolean_git, theirs) {
        // A path is a person's own choice, and on an old git it is also the
        // correct spelling. It survives either way.
        (_, true) => Fsmonitor::Leave,
        (true, false) => Fsmonitor::Write,
        (false, false) if ours => Fsmonitor::Unset,
        (false, false) => Fsmonitor::Leave,
    }
}
