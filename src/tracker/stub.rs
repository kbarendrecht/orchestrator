//! A stand-in tracker for proving the plumbing without filing a real story.

use super::Tracker;

/// Answers to the same MCP server name as the real one, on purpose.
///
/// `tools/stub-shortcut-mcp.py` is a real stdio MCP server *named* `shortcut`, so
/// the prompt's tool names and the `--allowedTools` scope resolve unchanged and
/// the run under test is the run that ships. Only what sits behind the socket
/// differs: it records what it was asked to do instead of doing it.
#[derive(Debug, Clone, Copy)]
pub struct Stub;

impl Tracker for Stub {
    fn mcp_server(&self) -> &'static str {
        "shortcut"
    }

    fn token_env(&self) -> &'static str {
        "SHORTCUT_API_TOKEN"
    }

    /// The real tracker's host, for the same reason the MCP name is shared: the
    /// run under test should be the run that ships, and a story URL it accepts
    /// here must be one the live tracker would accept too.
    fn host(&self) -> &'static str {
        "app.shortcut.com"
    }

    fn prompt(&self) -> &'static str {
        crate::prompt::STORY
    }
}
