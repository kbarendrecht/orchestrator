//! The vendored prompts, and filling in their placeholders.
//!
//! `commands/*.md` are compiled in with `include_str!` the same way the SPA is,
//! so the daemon carries its own prompts instead of resolving slash commands
//! from the agent's command path. That path depends on `you/commands`
//! being installed; it usually is not, and a missing command fails as "no such
//! command" at the worst possible moment — after a session has already spawned.
//!
//! Rendered text is passed to `claude -p` inline. Nothing is typed into a pty as
//! a `/command`, so nothing has to exist on the agent's side for a run to work.

use anyhow::{bail, Result};

/// The read-and-propose pass. Never applies, never posts.
pub const TRIAGE: &str = include_str!("../commands/triage.md");

/// The rebase-and-fix pass. Pushes, unlike triage.
pub const GREEN: &str = include_str!("../commands/green.md");

/// Everything a vendored prompt can ask for.
///
/// Deliberately not the token: that goes in the environment as `ORCHD_TOKEN`, so
/// it is never in prompt text that ends up in a transcript or a pty buffer.
pub struct Vars {
    pub pr: u64,
    pub owner: String,
    pub repo: String,
    /// The GitHub login whose PR this is — `{{LOGIN}}`, used to spot threads you
    /// already answered and to refuse someone else's branch.
    pub login: String,
    pub upstream: String,
    pub upstream_remote: String,
    /// Where a triage run POSTs its proposals.
    pub proposals_url: String,
}

/// Substitute every `{{PLACEHOLDER}}`, and refuse to ship one that is left.
///
/// An unreplaced placeholder is worse than a crash: the agent would read
/// `{{LOGIN}}` as literal text and quietly work from a nonsense value. So a
/// leftover is an error, which makes a typo in either the template or this list
/// fail at the first run rather than in the output.
pub fn render(template: &str, v: &Vars) -> Result<String> {
    let pr = v.pr.to_string();
    let out = [
        ("{{PR}}", pr.as_str()),
        ("{{OWNER}}", v.owner.as_str()),
        ("{{REPO}}", v.repo.as_str()),
        ("{{LOGIN}}", v.login.as_str()),
        ("{{UPSTREAM}}", v.upstream.as_str()),
        ("{{UPSTREAM_REMOTE}}", v.upstream_remote.as_str()),
        ("{{PROPOSALS_URL}}", v.proposals_url.as_str()),
    ]
    .iter()
    .fold(template.to_string(), |acc, (k, val)| acc.replace(k, val));

    if let Some(rest) = leftover(&out) {
        bail!("prompt has an unsubstituted placeholder: {rest}");
    }
    Ok(out)
}

/// The first `{{...}}` still in the text, if any.
fn leftover(s: &str) -> Option<String> {
    let start = s.find("{{")?;
    let end = s[start..].find("}}")? + start + 2;
    Some(s[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        Vars {
            pr: 10001,
            owner: "acme-org".into(),
            repo: "acme".into(),
            login: "kbarendrecht".into(),
            upstream: "upstream/develop".into(),
            upstream_remote: "upstream".into(),
            proposals_url: "http://127.0.0.1:7777/api/pr/10001/proposals".into(),
        }
    }

    #[test]
    fn both_vendored_prompts_render_with_nothing_left_over() {
        // The real templates, not fixtures: a placeholder added to either file
        // without being added here should fail this test, not a triage run.
        for (name, t) in [("triage", TRIAGE), ("green", GREEN)] {
            let out = render(t, &vars()).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!out.contains("{{"), "{name} still has a placeholder");
            assert!(out.contains("10001"), "{name} did not get the PR number");
        }
    }

    #[test]
    fn an_unknown_placeholder_is_refused_rather_than_shipped() {
        // Reading `{{VIEWER}}` as literal text would have the agent work from a
        // nonsense value, silently.
        let err = render("hello {{VIEWER}}", &vars()).unwrap_err().to_string();
        assert!(err.contains("{{VIEWER}}"), "{err}");
    }

    #[test]
    fn the_triage_prompt_still_says_the_things_it_must() {
        let out = render(TRIAGE, &vars()).unwrap();
        // The invariants the daemon depends on. If the prompt is reworded past
        // these, the contract in `proposal.rs` no longer matches what was asked.
        assert!(out.contains("$ORCHD_TOKEN"), "token must come from the env");
        assert!(out.contains("/api/pr/10001/proposals"), "handoff URL");
        assert!(out.contains("kbarendrecht"), "viewer login substituted");
        // The propose-only invariant everything downstream rests on. `proposal.rs`
        // refuses a change without evidence, and `patch.rs` assumes the tree was
        // clean; both are only true if the prompt actually said this.
        assert!(
            out.contains("You do not change anything"),
            "the propose-only invariant must be stated"
        );
        assert!(
            out.contains("committing, amending, rebasing, pushing"),
            "the writes the daemon owns must be listed as not the agent's"
        );
    }

    #[test]
    fn the_green_prompt_gets_the_upstream_ref() {
        let out = render(GREEN, &vars()).unwrap();
        assert!(out.contains("upstream/develop"));
        assert!(!out.contains("{{UPSTREAM"));
    }
}
