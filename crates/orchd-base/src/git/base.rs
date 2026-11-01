//! The base: what every diff, behind count and rebase measures against.
//!
//! **Parsed once, into one of two shapes, rather than split wherever it is read.**
//! It used to travel as the config's `upstream_ref` string, and three places split
//! it into a remote and a branch, each by its own rule: `split_upstream` defaulted
//! a bare name to `origin`, `Config::parse` took the part before the first `/`, and
//! the local-base check asked the repository whether that remote existed. They
//! disagreed in ways that cost three bugs in one day — `release/2.0` read as remote
//! `release`, a bare name counted local by one rule and remote by another, and a
//! local base fetched from `origin` while every measurement read the local branch.
//!
//! The rule now is the string's own shape, and nothing else: a `/` names a remote,
//! and a bare name is a branch of this checkout. Which repository the remote is,
//! and whether the checkout has gained one since, are questions about the
//! repository, asked by the methods that need them.

use anyhow::Result;
use std::fmt;
use std::path::Path;

use super::*;

/// The ref the daemon measures every tree against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// `<remote>/<branch>`. `branch` may be `HEAD`, the remote's default branch,
    /// and keeps any slashes of its own (`origin/release/2026`).
    Remote { remote: String, branch: String },
    /// A branch of this checkout, which has no remote to measure against.
    Local { branch: String },
}

impl Base {
    /// Read a configured base. Total: every string is one shape or the other.
    pub fn parse(s: &str) -> Base {
        match s.split_once('/') {
            Some((remote, branch)) => Base::Remote {
                remote: remote.to_string(),
                branch: branch.to_string(),
            },
            None => Base::Local {
                branch: s.to_string(),
            },
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Base::Local { .. })
    }

    /// The remote this base is fetched from, or `None` for a local one.
    pub fn remote(&self) -> Option<&str> {
        match self {
            Base::Remote { remote, .. } => Some(remote),
            Base::Local { .. } => None,
        }
    }

    /// The local branch to check out for this base, resolving a `HEAD` symref.
    ///
    /// `git switch HEAD` fails outright — "a branch is expected" — so a base of
    /// `origin/HEAD`, which is the default, needs the name behind the symref before
    /// it can be checked out. That needs the repo, which is why it takes one.
    ///
    /// `None` when the symref has never been recorded, which happens on a checkout
    /// whose remote was added by hand and not yet fetched. Callers treat that as
    /// "cannot", not as an error: [`Base::fetch`] records it on the next poll.
    pub fn checkout_branch(&self, main: &Path) -> Option<String> {
        match (self.head_of(), self) {
            (Some(remote), _) => default_branch(main, remote),
            (None, Base::Remote { branch, .. } | Base::Local { branch }) => Some(branch.clone()),
        }
    }

    /// The remote whose default branch this base names (`origin/HEAD`), or `None`
    /// for a named branch or a local one. The one place `HEAD` is recognised.
    fn head_of(&self) -> Option<&str> {
        match self {
            Base::Remote { remote, branch } if branch.eq_ignore_ascii_case("HEAD") => Some(remote),
            _ => None,
        }
    }

    /// Refresh the base from its remote.
    ///
    /// A concrete branch is fetched by name — one branch, cheap, the common case.
    ///
    /// **`HEAD` is not a branch you can fetch.** It is a symref under
    /// `refs/remotes/<remote>/` that only `git clone` and `git remote set-head`
    /// write — a plain `git fetch <remote>` does *not* create or refresh it, so on
    /// a checkout whose remote was added by hand `origin/HEAD` never resolves and
    /// every consumer (merge-base, divergence, rebase, the prompts' `{{UPSTREAM}}`)
    /// silently fails. So the `HEAD` arm asks the remote what its default branch
    /// is, records it in the symref, and then fetches that one branch by name.
    ///
    /// **A local base has nothing to fetch**, and is not an error: failing here put
    /// "upstream fetch failed" on every boot, sweep and rebase of a checkout with
    /// no remote. One that has gained a remote since is [`Base::outgrown`]'s to
    /// name, once, rather than a fetch failing every five minutes.
    pub fn fetch(&self, main: &Path) -> Result<()> {
        let Base::Remote { remote, branch } = self else {
            return Ok(());
        };
        if self.head_of().is_none() {
            // Through `git_net_ok` like the two `HEAD` arms below, not `git`: this
            // is the arm a configured base takes, on the boot path, and an https
            // remote without a credential helper prompted on `/dev/tty` here while
            // the window never opened.
            git_net_ok(main, &fetch_argv(remote, branch), "the upstream fetch")?;
            return Ok(());
        }
        // Steady state: the symref is already recorded, so fetch just the branch it
        // names — no dearer than the named case. If that fetch fails the recorded
        // branch is gone (renamed or deleted upstream), so fall through and
        // re-record.
        if let Some(b) = default_branch(main, remote) {
            if git_net_ok(main, &fetch_argv(remote, &b), "the upstream fetch").is_ok() {
                return Ok(());
            }
            tracing::debug!("{remote}/HEAD named {b}, which no longer fetches; re-recording");
        }
        // Bootstrap: `git remote set-head -a` picks from the remote-tracking refs, so
        // they have to exist first — which is why this fetches the whole remote
        // before recording. Only the first poll on a checkout pays for it.
        git_net_ok(main, &["fetch", remote, "--no-tags"], "the bootstrap fetch")?;
        if let Err(e) = git(main, &["remote", "set-head", remote, "-a"]) {
            // Not fatal: the refs are fetched, only the symref is missing, and the
            // caller's merge-base will report the real problem.
            tracing::debug!("could not record {remote}/HEAD: {e:#}");
        }
        Ok(())
    }

    /// A local base on a checkout that has since gained a remote, and the remote
    /// ref it should name instead.
    ///
    /// **The one way a local base goes wrong quietly.** The first run of a repo
    /// with no remote writes a bare `trunk`. Add `origin` later and nothing is
    /// fetched for it, while every diff, behind count and rebase still resolves
    /// `trunk`, the local branch nobody updates. Nothing fails, so nothing says so.
    /// Named rather than rewritten: which remote is the base is the person's
    /// answer, and the base is read at start.
    ///
    /// **Whether or not the local branch exists.** A bare `develop` written by hand
    /// on a checkout with `origin` and no local `develop` resolves to nothing; the
    /// fetch that used to fail every sweep and name it is gone, so this is now the
    /// only thing that does.
    pub fn outgrown(&self, main: &Path) -> Option<String> {
        let Base::Local { branch } = self else {
            return None;
        };
        let names = remotes(main)?;
        let remote = names
            .iter()
            .find(|r| *r == "origin")
            .or_else(|| names.first())?;
        Some(format!("{remote}/{branch}"))
    }
}

/// The string the config holds, and the rev git resolves: `origin/main`, `main`.
impl fmt::Display for Base {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Base::Remote { remote, branch } => write!(f, "{remote}/{branch}"),
            Base::Local { branch } => f.write_str(branch),
        }
    }
}

/// The base for a checkout with no remote at all: one of its own branches.
///
/// The other half of [`detect_base`], for a repo nobody has pushed anywhere. The
/// default `origin/HEAD` cannot resolve there, so without this every worktree cut
/// failed with "base ref origin/HEAD does not resolve" and the checkout could only
/// ever run a session in main. A local branch is a base like any other: worktrees
/// branch from it, diffs measure against it, and [`Base::fetch`] skips it.
///
/// **Never a name with a slash in it**, because a slash is what makes a base
/// remote ([`Base::parse`]): `release/2.0` would read as remote `release`. So the
/// checked-out branch is taken only when it has no slash; otherwise `main`, then
/// `master`, then any branch without one.
///
/// `None` when there is any remote — that repo has a base somewhere else, and a
/// guess here would hide it — and when no branch without a slash has a commit.
pub fn local_base(main: &Path) -> Option<String> {
    if !remotes(main)?.is_empty() {
        return None;
    }
    let current = current_branch(main).ok();
    let others = git(
        main,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )
    .unwrap_or_default();
    current
        .into_iter()
        .chain(["main".to_string(), "master".to_string()])
        .chain(others.lines().map(|l| l.trim().to_string()))
        .find(|b| !b.is_empty() && !b.contains('/') && branch_exists(main, b))
}

/// The fetch of one named branch: the argv both the named and the `HEAD` arms of
/// [`Base::fetch`] run, and the one the tests pin.
pub(super) fn fetch_argv<'a>(remote: &'a str, branch: &'a str) -> [&'a str; 4] {
    ["fetch", remote, branch, "--no-tags"]
}

/// The repo's remotes by name; `None` when git could not say.
fn remotes(main: &Path) -> Option<Vec<String>> {
    Some(
        git(main, &["remote"])
            .ok()?
            .lines()
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(String::from)
            .collect(),
    )
}
