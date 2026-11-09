//! Whether two trees would merge, asked of git without touching either.

use anyhow::{bail, Result};
use std::path::Path;

use super::exec::{git, run};

/// A commit of the tree at `cwd` as it is now, tracked changes included, that no
/// branch points to.
///
/// `git stash create` writes the commit and stops: no ref, no reset, so the tree
/// and the agent working in it see nothing. A clean tree has nothing to stash and
/// answers `HEAD`. Untracked files are left out, which is why a file both sides
/// created is judged without this (`overlap::compare`).
pub fn snapshot_commit(cwd: &Path) -> Result<String> {
    // Without optional locks, like every read here: an agent's own git in this
    // tree must not meet an index lock taken by the daemon.
    let made = git(cwd, &["--no-optional-locks", "stash", "create"])?;
    let made = made.trim();
    if !made.is_empty() {
        return Ok(made.to_string());
    }
    Ok(git(cwd, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// The files git could not merge between commits `a` and `b`, run in `cwd`.
///
/// **A merge in memory**: `merge-tree --write-tree` (git 2.38) writes objects and
/// no ref, index or file. Exit 0 is clean and 1 is conflicts, so this cannot go
/// through [`git`], which calls 1 a failure. Anything else, an older git
/// included, is an error for the caller to fall back from.
pub fn merge_conflicts(cwd: &Path, a: &str, b: &str) -> Result<Vec<String>> {
    let args = [
        "merge-tree",
        "--write-tree",
        "--name-only",
        "--no-messages",
        a,
        b,
    ];
    let out = run(cwd, &args)?;
    match out.status.code() {
        // The first line is the merged tree; the conflicted paths follow.
        Some(0) => Ok(Vec::new()),
        Some(1) => Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .skip(1)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()),
        _ => bail!(
            "git merge-tree failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}
