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

    fn prompt(&self) -> &'static str {
        crate::prompt::STORY
    }
}
