//! The forge seam.
//!
//! Everything the daemon reads from and writes to a code-hosting platform goes
//! through the [`Forge`] trait. GitHub is the only implementation today
//! ([`GitHubForge`]), but the whole point of the seam is that a second platform
//! is a sibling module behind the same trait rather than a rewrite of every
//! caller. TODO.md's "GitHub is the only forge" item is this boundary: the seam
//! is these modules, not the code that calls them.
//!
//! Two things deliberately sit *outside* the trait:
//!
//! - **Repo detection** ([`GitHubForge::detect`]) — it runs before an instance
//!   exists, to learn which repo to build one for, and it is inherently
//!   platform-specific (remote-URL shapes).
//! - **The update nudge** ([`latest_release`]) — an unauthenticated one-off
//!   against github.com's releases, scoped by TODO.md to after run-elsewhere.
//!
//! The model types ([`Pr`], [`Threads`], …) live in [`model`] so they carry no
//! GitHub dependency; the write handle and the token ladder are re-exported from
//! [`github`]/[`github_write`] so callers name `crate::forge::X` and never reach
//! into a platform module by hand.

use anyhow::Result;
use std::path::Path;

pub mod github;
pub mod github_write;
pub mod model;

pub use github::{
    graphql, latest_release, remote_url, repo_from_remote, resolve_token, warn_if_world_readable,
    GitHubForge, Token, TokenSource,
};
pub use github_write::{ready_to_rerequest, with_footer};
pub use model::{Checks, Comment, Pr, ReviewCandidate, ReviewRef, Thread, ThreadRoot, Threads};

/// Read + write against one repo on one forge.
///
/// Every method blocks on a subprocess (curl or `gh`), so callers wrap the call
/// in `spawn_blocking` — the trait does not, and must not, touch the async
/// runtime. Implementors are cheap to clone and hold no borrows so they can be
/// moved into that closure.
///
/// The read path and the write path can authenticate differently — GitHub reads
/// with a PAT and writes with `gh` — so a repo may satisfy one and not the
/// other. The `at` on the write methods is the checkout `gh` (or its analogue)
/// runs in; it varies per caller (the resolve run writes from a worktree, the
/// API from main), so it is a parameter rather than state on the forge.
pub trait Forge: Send + Sync + Clone + 'static {
    /// The open PRs you authored, plus your own login.
    fn poll_prs(&self) -> Result<(String, Vec<Pr>)>;

    /// Every review thread on one PR, paged to the end. The only thing that
    /// mints a [`ThreadRoot`], so a reply can only be aimed where a fetch proved
    /// a comment lives.
    fn threads(&self, pr: u64) -> Result<Threads>;

    /// Review candidates, raw and unranked, plus your login. Ranking and
    /// filtering are [`crate::reviews`]'s job and are config-driven, so they are
    /// the same whatever forge answered here.
    ///
    /// `all_open` selects coverage: `false` asks only for PRs where your review
    /// is requested (the lean default); `true` returns every open PR so a repo
    /// that treats review as a shared pool can rank the lot. The heavier query is
    /// off by default and opt-in per repo.
    fn review_candidates(&self, all_open: bool) -> Result<(String, Vec<ReviewCandidate>)>;

    /// Reply in an existing review thread.
    fn reply(&self, at: &Path, root: &ThreadRoot, body: &str) -> Result<()>;

    /// 👍 a comment — the plain-adoption case, applied as asked with nothing to
    /// add.
    fn thumbs_up(&self, at: &Path, root: &ThreadRoot) -> Result<()>;

    /// Ask one reviewer to look again.
    fn rerequest(&self, at: &Path, pr: u64, login: &str) -> Result<()>;
}

/// The forge the config selected, dispatched at runtime.
///
/// Enum rather than `Box<dyn Forge>` because the trait is `Clone` (the write
/// helpers move a copy into `spawn_blocking`), which is not object-safe. This is
/// the seam `ForgeKind` turns: every caller holds a `ForgeImpl` and never names
/// a concrete forge, so a second platform is a variant here plus its own `Forge`
/// impl — no call site changes.
#[derive(Debug, Clone)]
pub enum ForgeImpl {
    GitHub(GitHubForge),
}

impl ForgeImpl {
    /// Build the forge `kind` names. The one place a `ForgeKind` becomes a
    /// concrete forge; `token` is the read credential (a write-only handle can
    /// pass an empty one, since writes shell their own tool).
    pub fn for_kind(
        kind: crate::config::ForgeKind,
        owner: impl Into<String>,
        name: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        match kind {
            crate::config::ForgeKind::GitHub => {
                ForgeImpl::GitHub(GitHubForge::new(owner, name, token))
            }
        }
    }
}

impl Forge for ForgeImpl {
    fn poll_prs(&self) -> Result<(String, Vec<Pr>)> {
        match self {
            ForgeImpl::GitHub(f) => f.poll_prs(),
        }
    }
    fn threads(&self, pr: u64) -> Result<Threads> {
        match self {
            ForgeImpl::GitHub(f) => f.threads(pr),
        }
    }
    fn review_candidates(&self, all_open: bool) -> Result<(String, Vec<ReviewCandidate>)> {
        match self {
            ForgeImpl::GitHub(f) => f.review_candidates(all_open),
        }
    }
    fn reply(&self, at: &Path, root: &ThreadRoot, body: &str) -> Result<()> {
        match self {
            ForgeImpl::GitHub(f) => f.reply(at, root, body),
        }
    }
    fn thumbs_up(&self, at: &Path, root: &ThreadRoot) -> Result<()> {
        match self {
            ForgeImpl::GitHub(f) => f.thumbs_up(at, root),
        }
    }
    fn rerequest(&self, at: &Path, pr: u64, login: &str) -> Result<()> {
        match self {
            ForgeImpl::GitHub(f) => f.rerequest(at, pr, login),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_kind_builds_the_configured_forge_and_dispatches() {
        // The one place `ForgeKind` becomes a concrete forge; a second platform
        // is a variant plus its impl, and no caller changes.
        let f = ForgeImpl::for_kind(crate::config::ForgeKind::GitHub, "o", "n", "t");
        assert!(matches!(f, ForgeImpl::GitHub(_)));
        // And it is usable through the trait — reads and writes both dispatch.
        fn read_and_write(_: &impl Forge) {}
        read_and_write(&f);
    }
}
