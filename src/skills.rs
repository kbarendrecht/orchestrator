//! The vendored skills, handed to every session as a plugin directory.
//!
//! `skills/*/SKILL.md` are compiled in with `include_str!` for the same reason
//! the vendored prompts were: the daemon carries what it depends on
//! rather than resolving it from the agent's skill path, which holds whatever the
//! person running the daemon happens to have installed. The difference is what a
//! skill *is*. A vendored prompt is a first turn the daemon types; a skill is a
//! capability the session can reach for at any turn, including one a human is
//! driving by hand in the pane. `orch` is exactly that shape — every session gets
//! the binary and the environment for it, and until now nothing told a session it
//! was there.
//!
//! # `--plugin-dir` is per invocation, and that is the whole trap
//!
//! Measured against Claude Code 2.1.260: a session spawned with `--plugin-dir`
//! runs the skill both ways, typed as `/orchd:orch` and picked up by the model
//! from its description. Resuming that same session id *without* the flag answers
//! `Unknown command`. So the flag is not a property of the conversation, it is a
//! property of the process — and every spawn has to push it, resumes and forks
//! included. It reaches them through [`crate::config::session_flags`], beside the
//! settings file, because these sites have drifted before: that is the whole story
//! in [`crate::config::session_env`]'s docblock.
//!
//! A missing directory is not an error to Claude Code — it starts and says
//! nothing — which is why the flag is pushed unconditionally and [`write_plugin`]
//! is allowed to fail non-fatally. The degraded outcome is a session without the
//! skill, never a session that would not start.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::config::Config;

/// What a session can ask the daemon for. Teaches the `orch` CLI, which is on
/// every session's `PATH` already.
pub const ORCH: &str = include_str!("../skills/orch/SKILL.md");

/// Getting a PR green: rebase, fix the easy red, ask about the rest, amend, push,
/// watch.
///
/// Bundled from the monorepo's own `/green` command, which is where it was proven,
/// and it is the reason `fix-pr` does not need to grow an ask channel: this is the
/// same job written for a person to invoke and for an agent to be handed. The two
/// lines that are that repo's convention rather than a rule (its task runner, its
/// `upstream` remote) are marked as such in the file, per the repo's own portability
/// rule.
pub const GREEN: &str = include_str!("../skills/green/SKILL.md");

/// The read-and-propose pass over a PR's review threads.
///
/// Converted from `commands/triage.md`, which was rendered per run: a skill is
/// static, so the seven values that were substituted come from
/// `/api/pr/:n/triage-context` at the start of the pass instead. That is also what
/// makes it work when a person types it, which a prompt file the daemon wrote
/// never did.
pub const TRIAGE: &str = include_str!("../skills/triage/SKILL.md");

/// Getting a PR green, mechanically: rebase, fix the red, amend, force-push with a
/// lease, watch.
///
/// Converted from `commands/fix-pr.md`, and the six values that prompt substituted
/// reach it through the *environment* rather than a context call. That is not
/// symmetry with `triage` for its own sake — a fix run force-pushes unattended and
/// was deliberately given no ask token (`942d01b`), so a route would have meant
/// handing it a credential to read values the daemon can just as easily put in the
/// environment it is already building for that run.
pub const FIX_PR: &str = include_str!("../skills/fix-pr/SKILL.md");

/// Working a PR's review threads in a pane, with a person watching.
///
/// **The default answer to the rail's review button**, and the reason is the
/// overlay rather than this file: the cards are not good enough to be the only way
/// through a review yet, so the flow that puts one agent and one terminal in front
/// of you is the one the button starts. `triage` and the overlay stay a menu item
/// away, and `RESOLVE_RUN` still carries out what the cards decide.
///
/// Vendored from the monorepo's own `/resolve`, which is where it was proven —
/// and generalised on the way, per this repo's own rule about one repo not being
/// the specification: no owner/repo hardcoded, no tracker tool named (the repo's
/// tracker skill knows those), the task runner hedged, and the story dedup line
/// spelled the way the daemon's own story pass spells it so a later search finds
/// what this pass filed.
///
/// It asks with `AskUserQuestion` rather than over the ask channel, which is right
/// for a pane and wrong for the overlay: nobody is reading a card here.
pub const HANDLE_REVIEW: &str = include_str!("../skills/handle-review/SKILL.md");

/// Filing the stories a human approved on the cards.
///
/// Converted from `commands/story.md`, which was the one prompt left — and the
/// thing that had kept it a prompt was a comment claiming `--allowedTools` stops
/// this run invoking a skill. Measured against 2.1.263: `claude -p "/orchd:orch"`
/// under `--allowedTools "Read Write"` runs the skill, because the allowlist gates
/// tool calls and a typed command is expanded before the model acts.
///
/// It names no tracker tool, deliberately. `config::Tracker`'s docblock has the
/// research; the short of it is that Linear does not publish its tool names and
/// Atlassian's are versioned, so the repo's own tracker skill is the only place
/// that knowledge can live without going stale in a release.
pub const STORY: &str = include_str!("../skills/story/SKILL.md");

/// The overlay session: read the threads, propose, make what the human picked, post.
///
/// Converted from `commands/review-session.md`, the most interpolated of them —
/// nine substitutions, and the values reach it the way `triage`'s do, because this
/// pass has the same post token and asks the same route. `upstream` was added to
/// `triage-context` for it: the prompt used it for "CI red or behind the base is
/// fix-pr's job", and deriving it in the skill would have been a second answer to
/// what "behind" means.
pub const REVIEW: &str = include_str!("../skills/review/SKILL.md");

/// Carrying out a triaged review: apply, commit per thread, tell the daemon.
///
/// Converted from `commands/resolve-run.md`, and the conversion cost nothing that
/// prompt was doing: three of its four substitutions were prose (`{{PR}}`,
/// `{{OWNER}}/{{REPO}}` in the opening sentence) and the fourth was `{{ASK_BASE}}`,
/// which is `$ORCH_URL/api/session` — a variable the run already has. Only the
/// plan's path is genuinely per-run, and that rides in the environment beside the
/// fix run's values.
///
/// One thing the conversion settled rather than carried over: the prompt told a
/// `mode: "manual"` thread to "ask the question below", two sections above saying
/// there is no question channel. The question below was `/stuck`, so the skill says
/// `/stuck` — the ask token here is for `committed` and `stuck`, never for asking.
pub const RESOLVE_RUN: &str = include_str!("../skills/resolve-run/SKILL.md");

/// The four values a fix run finds in its environment.
///
/// Named here because **the two halves must agree**: `spawn::spawn_fix_pr_session`
/// sets these and `skills/fix-pr/SKILL.md` reads them, and a rename on one side
/// alone is silent — the skill would fall back to asking `gh` for a value the
/// daemon had already handed it, or stop for one it thinks is missing. The same
/// reason `RESOLVE_RUN_COMMAND` is a constant rather than four literals.
pub const VAR_PR: &str = "ORCH_PR";
pub const VAR_UPSTREAM: &str = "ORCH_UPSTREAM";
pub const VAR_UPSTREAM_REMOTE: &str = "ORCH_UPSTREAM_REMOTE";
pub const VAR_LOGIN: &str = "ORCH_LOGIN";
/// Where a resolve run finds the plan it is carrying out. Same rule as the four
/// above: the daemon writes the file and names it here, the skill reads it here.
pub const VAR_PLAN: &str = "ORCH_PLAN";
/// The story pass: what to file, where to report, and the host a URL must be on.
pub const VAR_STORIES: &str = "ORCH_STORIES";
pub const VAR_DROP: &str = "ORCH_DROP";
pub const VAR_TRACKER_HOST: &str = "ORCH_TRACKER_HOST";
/// The pane review pass: what language to write a reply in when the thread it
/// answers does not settle it.
pub const VAR_LANGUAGE: &str = "ORCH_LANGUAGE";

/// Every vendored skill, as `(directory name, body)`.
///
/// A table rather than a write per skill, because the writer and the frontmatter
/// test both walk it: adding a skill is then a line here, and it cannot be
/// written out without also being checked.
const VENDORED: &[(&str, &str)] = &[
    ("orch", ORCH),
    ("green", GREEN),
    ("triage", TRIAGE),
    ("fix-pr", FIX_PR),
    ("resolve-run", RESOLVE_RUN),
    ("review", REVIEW),
    ("story", STORY),
    ("handle-review", HANDLE_REVIEW),
];

/// The plugin manifest.
///
/// `name` is the namespace a skill is invoked through — `/orchd:orch` — so it is
/// part of the interface and not free to change. The version is the daemon's, so
/// a report saying which skill it had says which daemon wrote it.
const MANIFEST: &str = concat!(
    r#"{"name":"orchd","description":"What a session can ask the orchestrator it runs inside for.","version":""#,
    env!("CARGO_PKG_VERSION"),
    r#""}"#
);

/// Where the plugin directory lives.
///
/// Under the daemon's own config dir, never inside the checkout it is driving —
/// the same rule as the rendered prompts and for the same two reasons: the repo's
/// worktree-edit-boundary hook blocks a write under the main checkout that lands
/// outside the worktree, and a file inside a worktree would make that tree dirty,
/// which the review flow then reads as your work.
pub fn plugin_dir() -> Result<PathBuf> {
    Ok(Config::config_dir()?.join("plugin"))
}

/// The plugin-dir argv, or nothing when the directory could not be resolved at
/// all.
///
/// Reached through [`crate::config::session_flags`], which is what every spawn
/// site calls. Uniform on purpose: a site that opts out is a site that silently
/// differs, and the cost of carrying it is two argv words.
///
/// It used to say the story run's `--allowedTools` would not let it invoke a skill
/// anyway. That was never measured and is false — the module doc above has the
/// measurement — and the story run's instructions are a skill now.
pub fn flag() -> Vec<String> {
    match plugin_dir() {
        Ok(dir) => vec!["--plugin-dir".to_string(), dir.to_string_lossy().into_owned()],
        Err(e) => {
            tracing::warn!("no plugin dir, sessions get no orch skill: {e:#}");
            Vec::new()
        }
    }
}

/// Write the plugin directory, returning where it went.
///
/// Every start and always overwriting, because this is the daemon's file the way
/// `hooks.json` is: the shipped copy is the only correct one, and an edit to it
/// would be a skill describing a daemon that is no longer running.
pub fn write_plugin() -> Result<PathBuf> {
    let dir = plugin_dir()?;
    write_plugin_at(&dir)?;
    Ok(dir)
}

/// The half that takes a directory, so a test can exercise the layout without
/// setting `ORCHD_CONFIG_DIR` — one process-wide variable would race the tests
/// beside it (the same reasoning as `firstrun`'s `tmp`).
fn write_plugin_at(dir: &Path) -> Result<()> {
    let meta = dir.join(".claude-plugin");
    std::fs::create_dir_all(&meta).with_context(|| format!("creating {}", meta.display()))?;
    std::fs::write(meta.join("plugin.json"), MANIFEST)
        .with_context(|| format!("writing {}", meta.join("plugin.json").display()))?;

    for (name, body) in VENDORED {
        let at = dir.join("skills").join(name);
        std::fs::create_dir_all(&at).with_context(|| format!("creating {}", at.display()))?;
        std::fs::write(at.join("SKILL.md"), body)
            .with_context(|| format!("writing {}", at.join("SKILL.md").display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frontmatter name is what `/orchd:<name>` resolves through, and the
    /// description is the whole of what the model has to decide on. A skill
    /// missing either loads as nothing, silently.
    #[test]
    fn every_vendored_skill_has_frontmatter() {
        for (name, body) in VENDORED {
            let head = body.split("---").nth(1).expect("frontmatter block");
            assert!(
                head.contains(&format!("name: {name}")),
                "{name}'s frontmatter must name it {name}"
            );
            assert!(head.contains("description:"), "{name} needs a description");
        }
    }

    /// The fix run's two halves, checked against each other.
    ///
    /// A run gets these four out of its environment rather than out of a context
    /// call, so nothing at runtime would complain if the skill read a name the
    /// spawner does not set: it would quietly take the `gh` fallback for a value
    /// it had been given, and the run would rebase onto whatever it worked out for
    /// itself. That is the failure this test exists for.
    #[test]
    fn a_skill_reads_the_variables_its_run_is_given() {
        for (name, body, vars) in [
            ("fix-pr", FIX_PR, &[VAR_PR, VAR_UPSTREAM, VAR_UPSTREAM_REMOTE, VAR_LOGIN][..]),
            ("resolve-run", RESOLVE_RUN, &[VAR_PLAN][..]),
            ("story", STORY, &[VAR_STORIES, VAR_DROP, VAR_TRACKER_HOST][..]),
            // The pane pass gets two and reads both. It deliberately does *not*
            // read `VAR_LOGIN`, which its run does not set: its authorship stop
            // asks `gh` instead, the way `green` does.
            ("handle-review", HANDLE_REVIEW, &[VAR_PR, VAR_LANGUAGE][..]),
        ] {
            for v in vars {
                assert!(
                    body.contains(&format!("${v}")),
                    "skills/{name}/SKILL.md never reads ${v}, which its run sets"
                );
            }
        }
        // And the other way for the one variable that is easy to reach for and is
        // not there: the pane pass has no `$ORCH_LOGIN`, so a stop written against
        // it would compare against the empty string and pass.
        assert!(
            !HANDLE_REVIEW.contains(&format!("${VAR_LOGIN}")),
            "handle-review reads ${VAR_LOGIN}, which its run does not set"
        );
    }

    /// It is typed as one line, so a newline in the invocation would submit half a
    /// command — and the daemon builds that line from `fix_pr::COMMAND`, which has
    /// to be the directory the skill is written to.
    /// Each is typed as `/orchd:<command> <pr>` from the command string the run
    /// carries, so the directory it is written to has to *be* that string. A
    /// mismatch is `Unknown command` on the run's first turn and nothing before it.
    #[test]
    fn a_skill_is_named_after_the_command_that_types_it() {
        for command in [
            crate::fix_pr::COMMAND,
            crate::spawn::RESOLVE_RUN_COMMAND,
            crate::triage::COMMAND,
            crate::triage::TRIAGE_COMMAND,
            crate::story::COMMAND,
            crate::spawn::HANDLE_REVIEW_COMMAND,
        ] {
            assert!(
                VENDORED.iter().any(|(name, _)| *name == command),
                "no vendored skill directory called {command}"
            );
        }
    }

    /// The language a run writes replies in is a setting, and a skill cannot have
    /// it substituted — so it has to *ask*. Moved here from `prompt.rs`, where the
    /// same rule was checked against the rendered template this replaced.
    #[test]
    fn the_review_skill_asks_for_the_language_rather_than_naming_one() {
        assert!(REVIEW.contains("$LANGUAGE"), "the review skill hardcodes a language");
        for (name, body) in VENDORED {
            for word in body.split(|c: char| !c.is_alphabetic()) {
                assert!(
                    !["Dutch", "Portuguese", "Nederlands"].contains(&word),
                    "{name} names a language instead of asking for one"
                );
            }
        }
    }

    #[test]
    fn manifest_is_json_and_names_the_namespace() {
        let v: serde_json::Value = serde_json::from_str(MANIFEST).expect("valid json");
        assert_eq!(v["name"], "orchd", "the namespace `/orchd:orch` resolves through");
    }

    /// The two paths are Claude Code's, not ours: a manifest somewhere else is a
    /// plugin with no namespace, and a `SKILL.md` somewhere else is not found at
    /// all. Neither failure says anything at spawn time.
    #[test]
    fn writing_lays_out_what_claude_code_looks_for() {
        let dir = std::env::temp_dir().join(format!("orchd-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_plugin_at(&dir).expect("write the plugin");
        assert!(dir.join(".claude-plugin/plugin.json").is_file());
        assert!(dir.join("skills/orch/SKILL.md").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
