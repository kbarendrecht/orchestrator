//! Shortcut, reached through its MCP server.

use super::Tracker;

/// Shortcut. Carries nothing: everything the daemon needs is constant, and the
/// credential is resolved by [`crate::story::resolve_token`] from config.
#[derive(Debug, Clone, Copy)]
pub struct Shortcut;

impl Tracker for Shortcut {
    fn mcp_server(&self) -> &'static str {
        "shortcut"
    }

    /// What `${SHORTCUT_API_TOKEN}` in the repo's `.mcp.json` expands from.
    fn token_env(&self) -> &'static str {
        "SHORTCUT_API_TOKEN"
    }

    fn prompt(&self) -> &'static str {
        crate::prompt::STORY
    }
}
