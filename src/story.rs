//! Filing a tracker story for a review comment that is fair but out of scope.
//!
//! "I'll pick it up in a follow-up PR" is a promise with nothing behind it; a
//! story link is a promise with a tracking number, and the reply then says
//! something checkable. So a `story+reply` position files one and replies with
//! its id.
//!
//! **The tracker is MCP-only**, so this is the one place the daemon borrows an
//! agent for a *value* rather than for a session. What that buys is not credential
//! avoidance — the Shortcut MCP entry is `Bearer ${SHORTCUT_API_TOKEN}`, so the
//! same token is needed either way — but the repo's own
//! `.claude/skills/shortcut/SKILL.md`: Dutch content, the Backlog workflow state,
//! the team id, epic routing by category, priority only settable by a follow-up
//! update. A daemon-side template would hardcode those and write a worse story.
//!
//! **Stories are re-derived, not remembered.** An earlier draft made the story id
//! "the single exception to derive, do not remember" and kept a ledger. That was
//! wrong for the same reason a reply ledger was: it can be killed between the tool
//! call succeeding and the write, and then lies in exactly the case it exists for.
//! Instead the story body always carries the thread's permalink — appended by the
//! daemon, not trusted to the agent — and the filer searches for a story
//! containing it before creating one. A duplicate is then impossible at the
//! source, and a retry heals rather than stranding the thread.
//!
//! [`Cache`] therefore is what its name says. It saves an agent run and drives the
//! report's "reused" wording; losing it costs latency, not correctness, which is
//! why it may degrade to empty like every other store here.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// The Shortcut API token, for the MCP server's `Authorization` header.
///
/// Same ladder as [`crate::github::resolve_token`] — env, then a `0600` file —
/// and deliberately **not** a reader for `acme/.env`, where the team's copy
/// actually lives. That file is shell-ish, and the line as it stands is
/// `SHORTCUT_API_TOKEN='' # can be generated in …`; a naive split yields
/// `'' # can be` and injects a garbage Bearer, which surfaces later as "Shortcut
/// is down" rather than "the token is not set".
pub fn resolve_token(token_file: Option<&Path>) -> Result<String> {
    if let Ok(v) = std::env::var("ORCHD_SHORTCUT_TOKEN") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Ok(v);
        }
    }
    if let Some(p) = token_file {
        let raw = std::fs::read_to_string(p)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display()))?;
        let v = raw.trim().to_string();
        if !v.is_empty() {
            crate::github::warn_if_world_readable(p);
            return Ok(v);
        }
        bail!("{} is empty", p.display());
    }
    bail!(
        "no Shortcut token: set ORCHD_SHORTCUT_TOKEN or point `shortcut_token_file` at a \
         0600 file holding one"
    )
}

/// A story that exists in the tracker.
///
/// Both halves come from the tool response and neither is ever constructed by
/// `format!`: the org slug in the URL is acme-specific and the daemon has no
/// business knowing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoryRef {
    /// Short form, `sc-3001`. What the report shows.
    pub id: String,
    /// The clickable one, `https://app.shortcut.com/<org>/story/3001`.
    pub url: String,
}

impl StoryRef {
    /// What `{story}` becomes in the posted reply.
    ///
    /// A markdown link rather than either half alone: the skill's rule is "never
    /// a bare number, always the full URL" because a colleague has to be able to
    /// click it, and a naked URL mid-sentence reads badly in Dutch prose. The
    /// substitution is deterministic given `(id, url)`, so `already_replied`'s
    /// exact match still recognises a reply it posted before.
    pub fn link(&self) -> String {
        format!("[{}]({})", self.id, self.url)
    }

    /// Does the URL actually point at this id?
    ///
    /// The agent hands back both, and an id it invented for a story it never
    /// created would put a permanent public link to *somebody else's* story into
    /// a reply. The id's number appearing in the URL is what ties the two together
    /// without the daemon having to know Shortcut's URL scheme.
    ///
    /// Matched as a whole path segment rather than as a substring, because
    /// Shortcut hands out URLs both bare and with a title slug on the end, and a
    /// slug can carry digits of its own.
    pub fn consistent(&self) -> bool {
        let number = self.id.trim_start_matches(|c: char| !c.is_ascii_digit());
        if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        self.url.split('/').any(|seg| seg == number)
    }
}

/// Stories filed per PR, keyed by the thread they answer.
///
/// Nested rather than keyed on a tuple because JSON object keys are strings, and
/// a `(u64, String)` key would have to be encoded and parsed back — a format to
/// get wrong for no gain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cache {
    #[serde(default)]
    pub by_pr: HashMap<u64, HashMap<String, StoryRef>>,
}

impl Cache {
    pub fn get(&self, pr: u64, thread_id: &str) -> Option<&StoryRef> {
        self.by_pr.get(&pr)?.get(thread_id)
    }

    pub fn put(&mut self, pr: u64, thread_id: &str, story: StoryRef) {
        self.by_pr
            .entry(pr)
            .or_default()
            .insert(thread_id.to_string(), story);
    }

    /// Never pruned by PR. A merged PR's stories still matter to a late retry,
    /// and the whole file is a handful of ids.
    pub fn len(&self) -> usize {
        self.by_pr.values().map(HashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn story() -> StoryRef {
        StoryRef {
            id: "sc-3001".into(),
            url: "https://app.shortcut.com/acme/story/3001".into(),
        }
    }

    #[test]
    fn the_substitution_is_clickable_and_short() {
        assert_eq!(
            story().link(),
            "[sc-3001](https://app.shortcut.com/acme/story/3001)"
        );
    }

    #[test]
    fn an_id_that_does_not_match_its_url_is_refused() {
        // The agent hands back both. If they disagree, one of them is invented,
        // and posting the link would point a colleague at someone else's story.
        assert!(story().consistent());

        let mut swapped = story();
        swapped.url = "https://app.shortcut.com/acme/story/99999".into();
        assert!(!swapped.consistent());

        // Shortcut hands out both forms; a title slug on the end is still the
        // same story.
        let mut slugged = story();
        slugged.url =
            "https://app.shortcut.com/acme/story/3001/document-the-schedules".into();
        assert!(slugged.consistent());

        // ...and a slug carrying digits of its own must not stand in for the id.
        let mut decoy = story();
        decoy.id = "sc-777".into();
        decoy.url = "https://app.shortcut.com/acme/story/3001/fix-777-errors".into();
        assert!(
            !decoy.consistent(),
            "matched a slug instead of the id segment"
        );

        let mut empty = story();
        empty.id = "sc-".into();
        assert!(!empty.consistent());
    }

    #[test]
    fn the_cache_is_keyed_by_pr_and_thread() {
        let mut c = Cache::default();
        assert!(c.is_empty());
        c.put(10001, "PRRT_1", story());
        assert_eq!(c.get(10001, "PRRT_1"), Some(&story()));
        // Same thread id under a different PR is a different story.
        assert_eq!(c.get(10004, "PRRT_1"), None);
        assert_eq!(c.get(10001, "PRRT_2"), None);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn the_cache_survives_a_round_trip() {
        let mut c = Cache::default();
        c.put(10001, "PRRT_1", story());
        let back: Cache = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.get(10001, "PRRT_1"), Some(&story()));
    }
}
