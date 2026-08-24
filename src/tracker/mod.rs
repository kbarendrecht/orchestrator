//! The Tracker seam: where an out-of-scope review point gets filed.
//!
//! Shaped after [`crate::forge`], and deliberately much smaller, because the two
//! integrations are not the same kind of thing. The daemon *calls* a forge — it
//! polls PRs, posts replies, reacts — so [`crate::forge::Forge`] is a set of
//! operations. It never calls a tracker at all: filing a story is done by an
//! agent over MCP, and the daemon only sets that agent up.
//!
//! So a tracker is four facts:
//!
//! - which MCP server in the repo's `.mcp.json` speaks to it,
//! - which environment variable that server's config expands for its credential,
//! - which vendored prompt tells an agent how to file into it,
//! - and, from config, where the daemon reads that credential.
//!
//! Everything else — the two-phase run, the cache, the report shape, refusing a
//! story whose id and URL disagree — is in [`crate::story`] and is the same
//! whatever the tracker is. That split is the point: adding Jira or Linear is a
//! variant here plus a prompt, and no change to the filing flow.

use crate::config::TrackerKind;

mod shortcut;
mod stub;

pub use shortcut::Shortcut;
pub use stub::Stub;

/// What the daemon has to know to point an agent at a tracker.
///
/// `&'static str` throughout: every one of these is a compile-time constant of
/// the implementation, not runtime state, so there is nothing to own and nothing
/// to fail.
pub trait Tracker: Send + Sync + Clone + 'static {
    /// The MCP server's name in the repo's `.mcp.json`.
    ///
    /// Used twice, and they have to agree: `enabledMcpjsonServers` in the hook
    /// settings approves it (a project server stays *pending* and is dropped
    /// silently otherwise), and `--allowedTools` scopes the run to `mcp__<name>`.
    fn mcp_server(&self) -> &'static str;

    /// The variable the MCP entry expands for its credential, e.g. a
    /// `Bearer ${SHORTCUT_API_TOKEN}` header.
    ///
    /// The daemon resolves the value itself and pushes it into the agent's
    /// environment, so the token never reaches a prompt or a transcript.
    fn token_env(&self) -> &'static str;

    /// The vendored prompt that tells an agent how to file into this tracker.
    ///
    /// Per tracker rather than one templated prompt: the MCP tool names are the
    /// server's own (`stories-search` is not what Jira calls it), so a shared
    /// prompt would be a lie that fails mid-run.
    fn prompt(&self) -> &'static str;
}

/// The tracker the config selected, dispatched at runtime.
///
/// Enum rather than `Box<dyn Tracker>` for the same reason [`crate::forge`] is
/// one: the trait is `Clone`, which is not object-safe.
#[derive(Debug, Clone)]
pub enum TrackerImpl {
    Shortcut(Shortcut),
    Stub(Stub),
}

impl TrackerImpl {
    /// The tracker `kind` names, or `None` when no tracker is configured.
    ///
    /// `None` is not a failure: a repo with no tracker is a supported setup, and
    /// every caller reads it as "`story+reply` is not on offer" rather than as an
    /// error to report.
    pub fn for_kind(kind: TrackerKind) -> Option<Self> {
        match kind {
            TrackerKind::None => None,
            TrackerKind::Shortcut => Some(TrackerImpl::Shortcut(Shortcut)),
            TrackerKind::Stub => Some(TrackerImpl::Stub(Stub)),
        }
    }
}

impl Tracker for TrackerImpl {
    fn mcp_server(&self) -> &'static str {
        match self {
            TrackerImpl::Shortcut(t) => t.mcp_server(),
            TrackerImpl::Stub(t) => t.mcp_server(),
        }
    }

    fn token_env(&self) -> &'static str {
        match self {
            TrackerImpl::Shortcut(t) => t.token_env(),
            TrackerImpl::Stub(t) => t.token_env(),
        }
    }

    fn prompt(&self) -> &'static str {
        match self {
            TrackerImpl::Shortcut(t) => t.prompt(),
            TrackerImpl::Stub(t) => t.prompt(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tracker_is_absence_not_an_error() {
        assert!(TrackerImpl::for_kind(TrackerKind::None).is_none());
    }

    #[test]
    fn for_kind_builds_the_configured_tracker_and_dispatches() {
        // Through the enum, never a concrete tracker: this is the call every
        // caller makes, so if dispatch is wrong it is wrong here.
        let t = TrackerImpl::for_kind(TrackerKind::Shortcut).expect("a tracker");
        assert_eq!(t.mcp_server(), "shortcut");
        assert_eq!(t.token_env(), "SHORTCUT_API_TOKEN");
        assert!(!t.prompt().is_empty());

        // The stub answers to the same server name on purpose — it stands in for
        // the real one so the prompt's tool names still resolve.
        let s = TrackerImpl::for_kind(TrackerKind::Stub).expect("a tracker");
        assert_eq!(s.mcp_server(), "shortcut");
    }

    /// Compiles only if the trait stays object-safe-free and dispatchable, which
    /// is what lets a call site name `TrackerImpl` and no concrete tracker.
    #[test]
    fn callers_never_name_a_concrete_tracker() {
        fn takes_any(_: &impl Tracker) {}
        takes_any(&TrackerImpl::for_kind(TrackerKind::Shortcut).unwrap());
    }
}
