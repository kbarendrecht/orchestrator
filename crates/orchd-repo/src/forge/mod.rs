//! The forge seam.
//!
//! Everything the daemon reads from and writes to a code-hosting platform goes
//! through the [`Forge`] trait. GitHub is the only implementation today
//! ([`GitHubForge`]), but the whole point of the seam is that a second platform
//! is a sibling module behind the same trait rather than a rewrite of every
//! caller. The seam is these modules, not the code that calls them.
//!
//! Four things sit *outside* the trait. The first two are deliberate; the last
//! two are what a second platform would actually cost, written down rather than
//! implied away.
//!
//! - **Repo detection** ([`GitHubForge::detect`]) — it runs before an instance
//!   exists, to learn which repo to build one for, and it is inherently
//!   platform-specific (remote-URL shapes).
//! - **The update nudge** ([`latest_release`]) — a one-off against github.com's
//!   releases, riding the read-token ladder to stay off the 60/hour anonymous
//!   rate limit. It asks about *orchd's own* release repo, not the hosted
//!   monorepo, so it belongs to no forge instance.
//! - **The review queue's search** — `crate::reviews` writes its own GraphQL and
//!   sends it through [`graphql`]. The trait has no "search for my reviews", so
//!   that query is GitHub's shape in a module that is not a forge module.
//! - **[`Pr::mergeable`] carries GitHub's own spelling** — `MERGEABLE` /
//!   `CONFLICTING` / `UNKNOWN` — and it reaches the SPA that way, where five
//!   places compare against `'CONFLICTING'`. [`Checks`] is an enum for exactly
//!   this reason and `mergeable` is the one field that was not made one.
//!
//! The model types ([`Pr`], …) live in [`model`] so they carry no GitHub
//! dependency; the token ladder is re-exported from [`github`] so callers name
//! `crate::forge::X` and never reach into a platform module by hand.
//!
//! **Reads only.** The daemon posted review replies, 👍s and re-requests for the
//! overlay review, and those writes left with it: the review skill posts through
//! the agent's own `gh`, in a pane you are watching.

use anyhow::Result;

pub mod github;
pub mod model;

pub use github::{
    graphql, latest_release, remote_url, repo_from_remote, resolve_token, upstream_repo,
    warn_if_world_readable, GitHubForge, Token, TokenSource,
};
pub use model::{Checks, EndedPr, Pr, PrOutcome};

/// Reads against one repo on one forge.
///
/// Every method blocks on a subprocess (curl or `gh`), so callers wrap the call
/// in `spawn_blocking` — the trait does not, and must not, touch the async
/// runtime. Implementors are cheap to clone and hold no borrows so they can be
/// moved into that closure.
pub trait Forge: Send + Sync + Clone + 'static {
    /// The open PRs you authored, plus your own login.
    fn poll_prs(&self) -> Result<(String, Vec<Pr>)>;

    /// What became of a PR the poll has stopped reporting: merged, closed, or
    /// `None` for one the forge still calls open.
    ///
    /// Separate from [`Forge::poll_prs`] because the poll asks for *open* PRs and
    /// this is the question that asking cannot answer. Issued once per
    /// disappearance, never per tick.
    fn pr_outcome(&self, pr: u64) -> Result<Option<EndedPr>>;

    /// Your PRs from `head` that have already merged or closed, oldest first.
    ///
    /// The other half of [`Forge::pr_outcome`]: that one needs a number, and a PR
    /// that opened and ended while nobody was polling left none behind. The branch
    /// is all a session remembers of it.
    fn prs_ended_on(&self, head: &str) -> Result<Vec<EndedPr>>;

    /// A browser URL for one file at one ref.
    ///
    /// On the trait rather than built in the SPA because the path grammar is the
    /// forge's, not ours — GitHub's `/blob/`, GitLab's `/-/blob/`, Bitbucket's
    /// `/src/`. Every other external URL the SPA opens came *from* a forge read
    /// (`Pr::url`), so this is the one link we have to mint, and minting it here
    /// keeps the client ignorant of URL shape.
    ///
    /// Pure string work, no request: the URL is handed to the OS opener, so a
    /// wrong one costs a 404 in the browser, not a failed call. Hence no
    /// `Result` — there is nothing to fail.
    fn blob_url(&self, r#ref: &str, path: &str) -> String;

    /// A browser URL for one commit, for the blame gutter. On the trait for the
    /// reason [`Forge::blob_url`] is.
    fn commit_url(&self, sha: &str) -> String;

    /// A browser URL for `#n`, an issue or a PR. One URL for both: a ref in
    /// prose does not say which, and GitHub forwards an issue number that is a PR
    /// to the PR.
    fn issue_url(&self, n: u64) -> String;
}

/// Where a ref an agent printed points: `#36356` on the forge, or a story ref
/// like `sc-54841` through the configured [`StoryLink`]. `None` for anything
/// else, including a story ref with no usable link, so the caller can refuse
/// rather than open a guess.
///
/// Pure, and checked here rather than trusted: the result is handed to the OS
/// opener, so only digits are ever spliced into a URL.
pub fn ref_url(
    r: &str,
    forge: &impl Forge,
    story: Option<&crate::config::StoryLink>,
) -> Option<String> {
    let digits = |s: &str| (1..=12).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(n) = r.strip_prefix('#') {
        return digits(n)
            .then(|| n.parse().ok())
            .flatten()
            .map(|n| forge.issue_url(n));
    }
    let story = story.filter(|s| s.usable())?;
    let prefix = story.prefix.trim();
    let head = r.get(..prefix.len())?;
    let id = r.get(prefix.len()..)?;
    (head.eq_ignore_ascii_case(prefix) && digits(id)).then(|| story.url.replace("{id}", id))
}

/// The forge the config selected, dispatched at runtime.
///
/// Enum rather than `Box<dyn Forge>` because the trait is `Clone` (a caller moves
/// a copy into `spawn_blocking`), which is not object-safe. This is
/// the seam `ForgeKind` turns: every caller holds a `ForgeImpl` and never names
/// a concrete forge, so a second platform is a variant here plus its own `Forge`
/// impl — no call site changes.
#[derive(Debug, Clone)]
pub enum ForgeImpl {
    GitHub(GitHubForge),
}

impl ForgeImpl {
    /// Build the forge `kind` names. The one place a `ForgeKind` becomes a
    /// concrete forge; `token` is the read credential (a caller that only mints a
    /// URL can pass an empty one).
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
    fn pr_outcome(&self, pr: u64) -> Result<Option<EndedPr>> {
        match self {
            ForgeImpl::GitHub(f) => f.pr_outcome(pr),
        }
    }
    fn prs_ended_on(&self, head: &str) -> Result<Vec<EndedPr>> {
        match self {
            ForgeImpl::GitHub(f) => f.prs_ended_on(head),
        }
    }
    fn blob_url(&self, r#ref: &str, path: &str) -> String {
        match self {
            ForgeImpl::GitHub(f) => f.blob_url(r#ref, path),
        }
    }
    fn commit_url(&self, sha: &str) -> String {
        match self {
            ForgeImpl::GitHub(f) => f.commit_url(sha),
        }
    }
    fn issue_url(&self, n: u64) -> String {
        match self {
            ForgeImpl::GitHub(f) => f.issue_url(n),
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
        // And it is usable through the trait.
        fn reads(_: &impl Forge) {}
        reads(&f);
    }

    /// A ref becomes a URL only when it is exactly the shape it claims, because
    /// what comes out is handed to the OS opener.
    #[test]
    fn a_ref_links_to_the_forge_or_the_story_and_nothing_else() {
        let f = ForgeImpl::for_kind(crate::config::ForgeKind::GitHub, "acme", "mono", "");
        let story = crate::config::StoryLink {
            prefix: "sc-".into(),
            url: "https://app.shortcut.com/acme/story/{id}".into(),
        };
        let url = |r: &str| ref_url(r, &f, Some(&story));
        assert_eq!(
            url("#36356").as_deref(),
            Some("https://github.com/acme/mono/issues/36356")
        );
        assert_eq!(
            url("sc-54841").as_deref(),
            Some("https://app.shortcut.com/acme/story/54841")
        );
        assert_eq!(
            url("SC-54841").as_deref(),
            Some("https://app.shortcut.com/acme/story/54841")
        );
        for not in [
            "#", "#12a", "#1/../x", "sc-", "sc-12x", "sc-1 2", "xc-12", "12", "", "s",
        ] {
            assert_eq!(url(not), None, "{not:?} is not a ref");
        }
        // Unset or half-filled, a story ref is plain text; a forge ref still links.
        assert_eq!(ref_url("sc-1", &f, None), None);
        let half = crate::config::StoryLink {
            prefix: "sc-".into(),
            url: "https://x/story".into(),
        };
        assert_eq!(ref_url("sc-1", &f, Some(&half)), None);
        assert!(ref_url("#1", &f, None).is_some());
    }
}
