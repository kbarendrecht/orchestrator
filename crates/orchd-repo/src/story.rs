//! The story link a tracker implies, for a checkout that has not set one.
//!
//! **Shortcut is the one tracker with a ref format of its own**: its git
//! integration links `sc-<id>` wherever a branch, a commit or a PR title has it.
//! What it does not say is the URL, because that needs the workspace's name, and
//! the API is what knows it. So a Shortcut tracker with a token gets
//! `sc-` and `https://app.shortcut.com/<workspace>/story/{id}` without anybody
//! filling in Settings, and a link set there still wins.

use anyhow::{bail, Context, Result};
use std::path::Path;

use crate::config::{StoryLink, Tracker};

/// Whether `tracker` is Shortcut, by the server name or the host the default
/// config writes.
pub fn is_shortcut(tracker: &Tracker) -> bool {
    tracker.mcp_server.eq_ignore_ascii_case("shortcut") || tracker.host == "app.shortcut.com"
}

/// The story link for the Shortcut workspace `token` belongs to.
///
/// Blocking: one `curl` to `/api/v3/member`, with the token on stdin for the
/// reason `forge::github::graphql` gives.
pub fn shortcut_link(token: &str) -> Result<StoryLink> {
    let argv: Vec<String> = [
        "curl",
        "-sS",
        "--fail-with-body",
        "--max-time",
        "20",
        "-H",
        "@-",
        "-H",
        "User-Agent: orchd",
        "https://api.app.shortcut.com/api/v3/member",
    ]
    .map(String::from)
    .into();
    let out = orchd_base::proc::run_bounded_with_input(
        Path::new("/"),
        30,
        &argv,
        "the Shortcut API request",
        Some(format!("Shortcut-Token: {token}\n").into_bytes()),
        &[],
        None,
    )?;
    if !out.status.success() {
        bail!(
            "shortcut request failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let member: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing the Shortcut member")?;
    link_for(
        member["workspace2"]["url_slug"]
            .as_str()
            .unwrap_or_default(),
    )
}

/// The link for one workspace name. **Checked before it is spliced in**, since
/// the URL is handed to the OS opener.
fn link_for(slug: &str) -> Result<StoryLink> {
    let plain = !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !plain {
        bail!("shortcut gave no usable workspace name: {slug:?}");
    }
    Ok(StoryLink {
        prefix: "sc-".into(),
        url: format!("https://app.shortcut.com/{slug}/story/{{id}}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_name_becomes_a_usable_link_and_nothing_else_does() {
        let link = link_for("scienta").unwrap();
        assert!(link.usable());
        assert_eq!(link.url, "https://app.shortcut.com/scienta/story/{id}");
        for bad in ["", "a/b", "x?y", "Up"] {
            assert!(link_for(bad).is_err(), "{bad}");
        }
    }
}
