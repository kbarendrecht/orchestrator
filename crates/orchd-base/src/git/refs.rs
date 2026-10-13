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

// ---------------------------------------------------------------------------
// Whose branch is it
// ---------------------------------------------------------------------------

/// What this checkout knows about where its branches come from: each remote's
/// repository, and which remote each local branch tracks.
///
/// **A branch name is not an identity.** A PR names its head as `owner/repo` plus
/// a ref, and the checkout names its branches by ref alone — so with `origin` as
/// your fork and `upstream` as the base repo, a PR from upstream headed `fix-x`
/// and your fork's own `fix-x` are two branches with one name, and matching on
/// the name alone attached the PR to whichever the checkout happened to have.
/// This is the other half of the identity, read from the config git already keeps:
/// `remote.<r>.url` and `branch.<b>.remote`.
#[derive(Debug, Default, Clone)]
pub struct HeadCheck {
    /// `(remote, owner/name)`, lowercased, for each remote whose URL names a
    /// hosted repository. A local path names none and is left out.
    remotes: Vec<(String, String)>,
    /// Local branch → the remote it tracks.
    tracking: std::collections::HashMap<String, String>,
}

impl HeadCheck {
    /// Two config reads for the whole checkout, whatever the number of PRs asked.
    pub fn read(main: &Path) -> HeadCheck {
        let pairs = |pattern: &str| -> Vec<(String, String)> {
            // Exit 1 is "no such key", which is an empty answer and not a failure.
            git(main, &["config", "--get-regexp", pattern])
                .map(|out| {
                    out.lines()
                        .filter_map(|l| l.trim().split_once(' '))
                        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let remotes = pairs(r"^remote\..*\.url$")
            .into_iter()
            .filter_map(|(key, url)| {
                let name = key.strip_prefix("remote.")?.strip_suffix(".url")?;
                Some((name.to_string(), repo_of_url(&url)?))
            })
            .collect();
        let tracking = pairs(r"^branch\..*\.remote$")
            .into_iter()
            .filter_map(|(key, remote)| {
                let branch = key.strip_prefix("branch.")?.strip_suffix(".remote")?;
                Some((branch.to_string(), remote))
            })
            .collect();
        HeadCheck { remotes, tracking }
    }

    /// The remotes that carry `head_repo`, `origin` first when it is one of them.
    fn carrying(&self, head_repo: &str) -> Vec<&str> {
        let want = head_repo.to_ascii_lowercase();
        let mut found: Vec<&str> = self
            .remotes
            .iter()
            .filter(|(_, repo)| *repo == want)
            .map(|(name, _)| name.as_str())
            .collect();
        found.sort_by_key(|r| *r != "origin");
        found
    }

    /// Why the local branch `head_ref` is **not** this PR's head, or `None` when it
    /// may stand for it.
    ///
    /// It degrades rather than insists, because a wrong refusal blocks a PR flow
    /// that worked: an unknown head repo, or a checkout none of whose remotes names
    /// a hosted repository (a local-path remote, a host this cannot parse), answers
    /// `None` and the name is matched as it always was. Only two things refuse — a
    /// head repo that none of the remotes carries, and a local branch that tracks
    /// a *different* remote from the ones that do. A local branch tracking nothing
    /// is taken as the PR's: it is what a branch you made and pushed without `-u`
    /// looks like, and that is the common way to have opened the PR.
    pub fn refusal(&self, head_ref: &str, head_repo: Option<&str>) -> Option<String> {
        let head_repo = head_repo?;
        if self.remotes.is_empty() {
            return None;
        }
        let carrying = self.carrying(head_repo);
        if carrying.is_empty() {
            return Some(format!(
                "its head is {head_repo}:{head_ref}, and {head_repo} is none of this \
                 checkout's remotes — the local {head_ref}, if there is one, is a \
                 different branch with the same name. Add {head_repo} as a remote to \
                 work on it here."
            ));
        }
        match self.tracking.get(head_ref) {
            Some(tracks) if !carrying.contains(&tracks.as_str()) => Some(format!(
                "its head is {head_repo}:{head_ref}, and the local {head_ref} tracks \
                 {tracks}, which is a different repository — a branch with the same \
                 name, not this PR's"
            )),
            _ => None,
        }
    }

    /// The remote to fetch `head_repo`'s branch from when the checkout has no local
    /// copy, or `None` to search as before (unknown head, or no hosted remotes).
    pub fn fetch_from(&self, head_repo: Option<&str>) -> Option<String> {
        self.carrying(head_repo?).first().map(|r| r.to_string())
    }
}

/// `owner/name`, lowercased, from a remote URL: `git@host:o/n.git`,
/// `ssh://git@host/o/n`, `https://host/o/n.git`. `None` for a local path, which
/// names a directory and not a hosted repository — and a guess there is the
/// one way this rule could refuse a PR it should not.
pub fn repo_of_url(url: &str) -> Option<String> {
    let url = url.trim();
    let path = if let Some((_, rest)) = url.split_once("://") {
        if url.starts_with("file://") {
            return None;
        }
        rest.split_once('/')?.1
    } else if let Some((host, rest)) = url.split_once(':') {
        // scp-like `[user@]host:path`. A Windows drive (`C:\…`) or a path that
        // merely contains a colon after a slash is not one.
        if host.is_empty() || host.contains('/') || host.len() == 1 {
            return None;
        }
        rest
    } else {
        return None;
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.rsplit('/');
    let name = parts.next().filter(|s| !s.is_empty())?;
    let owner = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{name}").to_ascii_lowercase())
}
