use anyhow::{bail, Context, Result};
use std::path::Path;

use super::*;

// ---------------------------------------------------------------------------
// The review flow's writes
// ---------------------------------------------------------------------------

/// Push the PR's own branch, refusing to clobber anyone else's work.
///
/// `--force-with-lease` rather than `--force`: it fails when the remote moved
/// since the last fetch, which is exactly the "someone else pushed" case that
/// must not be overwritten. Never `-u`: rebinding upstream to origin breaks pull
/// tracking in a triangular remote setup.
///
/// `base` is the branch this checkout is measured against, from `upstream_ref`.
/// The agent-side guard ([`crate::guard`]) is a `PreToolUse` hook on **Bash**, so
/// a daemon-side push never passes through it and the rule has to be re-stated
/// here or it is simply not enforced. Its other rule — plain `--force` — is
/// structurally impossible below, because the command is a fixed string.
///
/// This used to be a hardcoded `["develop", "main", "master", "release"]`, which
/// was wrong in both directions: it let a push to a base called `trunk` through,
/// and refused an ordinary feature branch that happened to be named `release`.
/// `None` is "no resolvable base", and refuses nothing.
pub fn push_with_lease(cwd: &Path, branch: &str, base: Option<&str>) -> Result<()> {
    if base == Some(branch) {
        bail!("refusing to push to {branch}: it is the base branch, open a PR instead");
    }
    // Bounded and unpromptable like every other network git call: a push against
    // a remote wanting credentials would otherwise wait on a tty forever, and this
    // one runs inside an HTTP request somebody is watching.
    let out = git_net(
        cwd,
        &["push", "--force-with-lease", "origin", branch],
        "the push",
    )
    .context("running git push")?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // The lease failing is the one refusal worth naming: it means the remote
    // moved, and the fix is to look at both sides rather than push harder.
    if lease_refused(&err) {
        bail!(
            "push refused: {branch} moved on origin since this review started. \
             Someone else pushed, or fix-pr ran. Re-triage rather than overwrite it."
        );
    }
    bail!(
        "push failed: {}",
        err.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output")
    );
}

/// Did the remote refuse the push because it had moved?
///
/// Git's own markers, and only those: `--force-with-lease` reports `[rejected] …
/// (stale info)`, and an unforced push behind the remote says `fetch first` or
/// `non-fast-forward`. A bare `rejected` used to count too, and it also matches a
/// hook's `pre-receive hook declined` and a protected branch's refusal, both of
/// which were then blamed on somebody else's push and answered with "re-triage".
pub(super) fn lease_refused(stderr: &str) -> bool {
    stderr.contains("stale info")
        || stderr.contains("fetch first")
        || stderr.contains("non-fast-forward")
}

pub fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
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
/// Never popped automatically: popping onto a branch the review just changed can
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
            "orchd: before a review",
        ],
    )?;
    Ok(())
}
