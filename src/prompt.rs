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
pub const FIX_PR: &str = include_str!("../commands/fix-pr.md");

/// The story pass. Places records the human already approved, and reports back by
/// writing one file — the only agent here whose *output* the daemon reads.
pub const STORY: &str = include_str!("../commands/story.md");

/// The interactive review pass. Unlike the others this one runs in a pane you can
/// take over, so it reaches the session as a file to read rather than as `-p`.
pub const RESOLVE: &str = include_str!("../commands/resolve.md");

/// The session that carries out a triaged review: applies the fixes, commits, and
/// stops to ask. It writes code and nothing outward.
pub const RESOLVE_RUN: &str = include_str!("../commands/resolve-run.md");

/// What `{{TRACKER}}` becomes when a tracker is configured.
///
/// A sentence rather than a boolean, so the prompt reads as prose in both states
/// instead of carrying a conditional the substitution cannot express. The agent
/// must not propose `story+reply` when the daemon would only refuse it — that
/// would put an option on a card that cannot be acted on.
pub const TRACKER_ON: &str = "A tracker is configured, so `story+reply` is available for a \
     point that is fair but out of scope here.";

/// And when it is not.
pub const TRACKER_OFF: &str = "**No tracker is configured, so never propose `story+reply`.** \
     There is nowhere to file one and the daemon would refuse it. For a fair but out-of-scope \
     point, say so plainly in a reply instead of promising a story.";

/// Everything a vendored prompt can ask for.
///
/// Deliberately not the token: that goes in the environment as `ORCHD_TOKEN`, so
/// it is never in prompt text that ends up in a transcript or a pty buffer.
///
/// One struct for three templates, so each caller fills in only the fields its own
/// template uses and leaves the rest at [`Default`]. Note what that costs: `render`
/// catches an unsubstituted placeholder but cannot catch an *empty* substitution,
/// because an empty string is a substitution. The guard against that is the
/// per-template assertions in this module's tests, which read the real files and
/// check the things downstream code depends on actually arriving.
#[derive(Default)]
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
    /// Whether `story+reply` is available at all, as a whole sentence rather than
    /// a flag. The prompt reads as prose either way, and the agent must not
    /// propose an option the daemon would then refuse.
    pub tracker: String,
    /// The stories to file, as the JSON array `commands/story.md` embeds.
    pub stories: String,
    /// The file a story run writes its answer into.
    pub drop_file: String,
    /// The language the agent writes replies and story text in — `{{LANGUAGE}}`.
    /// Prompts stay English; this is the outward prose only, and the thread's own
    /// language still wins when it is clear.
    pub language: String,
    /// Where a running session asks a question: the base the two interaction
    /// routes hang off, since the session id itself comes from the environment.
    pub ask_base: String,
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
        ("{{TRACKER}}", v.tracker.as_str()),
        ("{{STORIES}}", v.stories.as_str()),
        ("{{DROP_FILE}}", v.drop_file.as_str()),
        ("{{ASK_BASE}}", v.ask_base.as_str()),
        ("{{LANGUAGE}}", v.language.as_str()),
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
            owner: "acme".into(),
            repo: "monorepo".into(),
            login: "kbarendrecht".into(),
            upstream: "upstream/develop".into(),
            upstream_remote: "upstream".into(),
            proposals_url: "http://127.0.0.1:7777/api/pr/10001/proposals".into(),
            ask_base: "http://127.0.0.1:7777/api/session".into(),
            tracker: TRACKER_ON.into(),
            stories: "[]".into(),
            drop_file: "/tmp/x/stories.json".into(),
            language: "Dutch".into(),
        }
    }

    #[test]
    fn every_vendored_prompt_renders_with_nothing_left_over() {
        // The real templates, not fixtures: a placeholder added to either file
        // without being added here should fail this test, not a triage run.
        for (name, t) in [
            ("triage", TRIAGE),
            ("fix-pr", FIX_PR),
            ("story", STORY),
            ("resolve", RESOLVE),
            // The newest and most interpolated of them, and the one this guard
            // was missing: `resolve-run.md` carries three built URLs, so it is
            // the likeliest to gain a placeholder nobody substitutes.
            ("resolve-run", RESOLVE_RUN),
        ] {
            let out = render(t, &vars()).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!out.contains("{{"), "{name} still has a placeholder");
            assert!(out.contains("10001"), "{name} did not get the PR number");
        }
    }

    #[test]
    fn the_tracker_sentence_reads_both_ways() {
        // The schema block still lists `story+reply` as a valid `does`, so the
        // prose is the only thing that tells the agent not to use it. If these
        // two drift apart, the agent proposes an option the daemon refuses.
        let on = render(TRIAGE, &vars()).unwrap();
        assert!(on.contains("`story+reply` is available"), "tracker on");

        let off = render(
            TRIAGE,
            &Vars {
                tracker: TRACKER_OFF.into(),
                ..vars()
            },
        )
        .unwrap();
        assert!(off.contains("never propose `story+reply`"), "tracker off");
        // Either way the vocabulary itself is unchanged — the daemon validates
        // against it and the schema is one list, not two.
        assert!(off.contains("story+reply"));
    }

    #[test]
    fn the_story_prompt_still_says_the_things_it_must() {
        let out = render(
            STORY,
            &Vars {
                stories: r#"[{"thread_id":"PRRT_1"}]"#.into(),
                drop_file: "/tmp/orchd-story-1/stories.json".into(),
                ..vars()
            },
        )
        .unwrap();
        // Where it reports, and the two rules that keep a duplicate impossible.
        assert!(out.contains("/tmp/orchd-story-1/stories.json"), "drop file");
        assert!(out.contains(r#""thread_id":"PRRT_1""#), "the drafts");
        assert!(out.contains("Search before you create"), "the search rule");
        assert!(out.contains("Never two"), "the one-story rule");
        // The approved text is the human's, not the agent's to improve.
        assert!(
            out.contains("Do not rewrite `title` or `body`"),
            "the no-rewrite rule must be stated"
        );
        // Both halves must come from the tool, or a fabricated pair could be
        // reported and posted.
        assert!(out.contains("must both come from the tool response"));
    }

    #[test]
    fn the_output_language_is_substituted_and_no_language_is_hardcoded() {
        // Prompts stay English; the language the agent *writes* in is a setting.
        let out = render(TRIAGE, &Vars { language: "Portuguese".into(), ..vars() }).unwrap();
        assert!(out.contains("default to Portuguese"), "reply language substituted");
        assert!(out.contains("Write both in Portuguese"), "story language substituted");
        assert!(!out.contains("Dutch"), "no language is baked into the prompt");
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
    fn the_fix_prompt_gets_the_upstream_ref() {
        let out = render(FIX_PR, &vars()).unwrap();
        assert!(out.contains("upstream/develop"));
        assert!(!out.contains("{{UPSTREAM"));
    }
}
