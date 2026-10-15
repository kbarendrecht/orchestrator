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
//! included. It reaches them through [`crate::launch::session_flags`], beside the
//! settings file, because these sites have drifted before: that is the whole story
//! in [`crate::launch::session_env`]'s docblock.
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
pub const ORCH: &str = include_str!("../../../skills/orch/SKILL.md");

/// Getting a PR green, mechanically: take the base in, fix the red, amend, push,
/// watch.
///
/// Converted from `commands/fix-pr.md`, and the six values that prompt substituted
/// reach it through the *environment* rather than a context call. That is not
/// symmetry with `triage` for its own sake — a fix run force-pushes unattended and
/// was deliberately given no ask token (`942d01b`), so a route would have meant
/// handing it a credential to read values the daemon can just as easily put in the
/// environment it is already building for that run.
pub const FIX_PR: &str = include_str!("../../../skills/fix-pr/SKILL.md");

/// Working a PR's review threads in a pane, with a person watching.
///
/// **The answer to the rail's review button**: one agent and one terminal in front
/// of you. There was a second review verb, an overlay of cards; it was removed.
///
/// Vendored from the monorepo's own `/resolve`, which is where it was proven —
/// and generalised on the way, per this repo's own rule about one repo not being
/// the specification: no owner/repo hardcoded, no tracker tool named (the repo's
/// tracker skill knows those), and the task runner hedged.
///
/// It asks with `AskUserQuestion` rather than over the ask channel, which is right
/// for a pane: the person is reading the terminal.
pub const HANDLE_REVIEW: &str = include_str!("../../../skills/handle-review/SKILL.md");

/// The four values a fix run finds in its environment.
///
/// Named here because **the two halves must agree**: `spawn::spawn_fix_pr_session`
/// sets these and `skills/fix-pr/SKILL.md` reads them, and a rename on one side
/// alone is silent — the skill would fall back to asking `gh` for a value the
/// daemon had already handed it, or stop for one it thinks is missing.
pub const VAR_PR: &str = "ORCH_PR";
pub const VAR_UPSTREAM: &str = "ORCH_UPSTREAM";
pub const VAR_UPSTREAM_REMOTE: &str = "ORCH_UPSTREAM_REMOTE";
pub const VAR_LOGIN: &str = "ORCH_LOGIN";
/// The daemon's authorship answer, `yes` or `no`: may this session rewrite and
/// force-push the PR's branch. `forge::Pr::push_refusal` decides it, and the
/// skills read it rather than asking whose name is on the head repo — which is
/// the question the daemon stopped asking because it was wrong both ways.
pub const VAR_MAY_PUSH: &str = "ORCH_MAY_PUSH";
/// The pane review pass: what language to write a reply in when the thread it
/// answers does not settle it.
pub const VAR_LANGUAGE: &str = "ORCH_LANGUAGE";

/// Every vendored skill, as `(directory name, body)`.
///
/// A table rather than a write per skill, because the writer and the frontmatter
/// test both walk it: adding a skill is then a line here, and it cannot be
/// written out without also being checked.
pub const VENDORED: &[(&str, &str)] = &[
    ("orch", ORCH),
    ("fix-pr", FIX_PR),
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
/// Reached through [`crate::launch::session_flags`], which is what every spawn
/// site calls. Uniform on purpose: a site that opts out is a site that silently
/// differs, and the cost of carrying it is two argv words.
///
/// It used to say the story run's `--allowedTools` would not let it invoke a skill
/// anyway. That was never measured and is false — the module doc above has the
/// measurement.
pub fn flag() -> Vec<String> {
    match plugin_dir() {
        Ok(dir) => vec![
            "--plugin-dir".to_string(),
            dir.to_string_lossy().into_owned(),
        ],
        Err(e) => {
            tracing::warn!("no plugin dir, sessions get no orch skill: {e:#}");
            Vec::new()
        }
    }
}

/// What tells a Codex session the daemon's skills are there.
///
/// **Codex has no `--plugin-dir`.** It finds skills in a repo's `.agents/skills`,
/// which it reads only for a project it trusts and which would put the daemon's
/// files in your checkout, and in `~/.agents/skills`, which would hand them to
/// every Codex you run. Its plugins install into a cache that outlives the
/// process. So the skills stay where [`write_plugin`] puts them for Claude Code,
/// and these instructions name each one and its file, handed to the process as
/// `developer_instructions`, the per-invocation layer Codex does have. Measured on
/// 0.162.1: told the path that way, it reads the file and follows it.
///
/// The line a run types is [`crate::agent::Agent::skill_line`]'s, which is what
/// the first sentence teaches it to recognise.
pub fn codex_instructions() -> Option<String> {
    let dir = plugin_dir().ok()?.join("skills");
    let mut out = String::from(
        "You are running inside orchd, the orchestrator that started this session. \
         It carries skills of its own as files. When a message is `orchd:<name>` \
         followed by arguments, or a task matches one of the descriptions below, read \
         that skill's file and follow it, taking the arguments as its input.\n",
    );
    for (name, body) in VENDORED {
        let description = body
            .split("---")
            .nth(1)
            .and_then(|head| head.lines().find_map(|l| l.strip_prefix("description:")))
            .map(str::trim)
            .unwrap_or("");
        out.push_str(&format!(
            "- orchd:{name} ({}): {description}\n",
            dir.join(name).join("SKILL.md").display()
        ));
    }
    Some(out)
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
    prune(&dir.join("skills"));
    Ok(())
}

/// The skills an earlier build shipped and this one does not.
///
/// **Named, not inferred from [`VENDORED`].** A folder in the plugin dir that is
/// not vendored may be one this daemon wrote, or one somebody put there by hand,
/// and only the first is the writer's to remove.
const RETIRED: &[&str] = &["green", "review", "story"];

/// Remove a skill an earlier build wrote and this one no longer ships.
///
/// The directory outlives the build that wrote it, and Claude Code loads every
/// skill it finds there, so a skill dropped from [`VENDORED`] would otherwise stay
/// callable in every session forever. Only [`RETIRED`] names are touched, only the
/// `SKILL.md` this writer put there is removed, and the directory only once it is
/// empty. Best effort: a leftover is a stale skill, not a broken start.
fn prune(skills: &Path) {
    for name in RETIRED {
        let at = skills.join(name);
        let _ = std::fs::remove_file(at.join("SKILL.md"));
        let _ = std::fs::remove_dir(&at);
    }
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

    /// A skill an earlier build shipped is gone after the next write, and a skill
    /// the writer never shipped is not its to remove — not even its `SKILL.md`.
    #[test]
    fn a_skill_no_longer_shipped_is_removed() {
        let dir = std::env::temp_dir().join(format!("orchd-prune-{}", std::process::id()));
        let old = dir.join("skills").join("green");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("SKILL.md"), "---\nname: green\n---\n").unwrap();
        let kept = dir.join("skills").join("mine");
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::write(kept.join("SKILL.md"), "---\nname: mine\n---\n").unwrap();
        // Beside a retired skill, a file that is not the writer's.
        let leftover = dir.join("skills").join("review");
        std::fs::create_dir_all(&leftover).unwrap();
        std::fs::write(leftover.join("SKILL.md"), "---\nname: review\n---\n").unwrap();
        std::fs::write(leftover.join("notes.txt"), "not the daemon's").unwrap();

        write_plugin_at(&dir).unwrap();

        assert!(!old.exists(), "a dropped skill must not stay callable");
        assert!(
            kept.join("SKILL.md").exists(),
            "a skill somebody else put there is not the writer's"
        );
        assert!(!leftover.join("SKILL.md").exists(), "a retired skill goes");
        assert!(
            leftover.join("notes.txt").exists(),
            "and only its SKILL.md is the writer's"
        );
        for (name, _) in VENDORED {
            assert!(dir.join("skills").join(name).join("SKILL.md").exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Codex is told each skill by name, file and description, and a skill
    /// whose description did not make it is one the model never reaches for.
    #[test]
    fn codex_is_told_every_skill_and_what_it_is_for() {
        let text = codex_instructions().expect("a plugin dir in a test");
        for (name, _) in VENDORED {
            let line = text
                .lines()
                .find(|l| l.starts_with(&format!("- orchd:{name} (")))
                .unwrap_or_else(|| panic!("{name} is not named"));
            assert!(line.contains(&format!("/skills/{name}/SKILL.md): ")));
            assert!(
                line.len() > format!("- orchd:{name} ()").len() + 40,
                "{name} has no description: {line}"
            );
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
            (
                "fix-pr",
                FIX_PR,
                &[
                    VAR_PR,
                    VAR_UPSTREAM,
                    VAR_UPSTREAM_REMOTE,
                    VAR_LOGIN,
                    VAR_MAY_PUSH,
                ][..],
            ),
            // The pane pass gets three and reads all of them. It deliberately does
            // *not* read `VAR_LOGIN`, which its run does not set: its authorship
            // stop reads the daemon's `VAR_MAY_PUSH`, and asks `gh` the same
            // question when typed by hand.
            (
                "handle-review",
                HANDLE_REVIEW,
                &[VAR_PR, VAR_LANGUAGE, VAR_MAY_PUSH][..],
            ),
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

    /// No vendored skill may name a task runner.
    ///
    /// **A file shipped with the daemon guessing at the repo it lands in.** Four of
    /// them said `mise run pre-commit:run`, hedged as "where it exists" — and that
    /// hedge was carrying the whole sentence, because the task exists in no repo
    /// this was ever run against: one calls it `pre-commit`, one has `lint` and
    /// `test`, and this repo has only a git hook. A run following that line either
    /// found nothing or checked nothing, and neither said so.
    ///
    /// They say `run this repo's checks` now, and there is deliberately no setting
    /// behind it — a `checks_command` was built and reverted, because a repo with
    /// many kinds of check has no single answer to put in one, and a config field
    /// only moves the guess from the skill into the daemon. The agent is in the
    /// repo and can read what it says.
    ///
    /// **Matched on whole words, and the list holds only unambiguous ones.** Two
    /// drafts of this failed on the skills' own prose: `just`, `rake` and `invoke`
    /// are task runners *and* ordinary English, and `promise` contains `mise`. A
    /// check that fires on a sentence is one people learn to work around, which is
    /// the argument `clippy::indexing_slicing` already lost here.
    #[test]
    fn no_vendored_skill_names_a_task_runner() {
        for (name, body) in VENDORED {
            // Whole words, so `promise` is not `mise`. Split the same way the
            // language check below does.
            for word in body.split(|c: char| !c.is_alphanumeric()) {
                assert!(
                    word != "mise",
                    "skills/{name} names `mise`, which is one repo's task runner"
                );
            }
            // Phrases are unambiguous on their own and need no boundary.
            for runner in ["npm run", "yarn run", "pnpm run", "bundle exec", "nx run"] {
                assert!(
                    !body.contains(runner),
                    "skills/{name} names `{runner}`, which is one repo's task runner"
                );
            }
        }
        /* And every skill that pushes still asks for the checks, so the line was not
        deleted along with the command it named.

        **Over `VENDORED` rather than a second list**, which is the whole reason
        that table exists: a fifth pushing skill added there would have been written
        to disk and checked for `mise` while silently escaping this half. */
        for (name, body) in VENDORED {
            /* **Derived from the body, not from a list of names.** Naming the four
            was a second list by another spelling — the thing the paragraph above
            says this avoids — so a fifth pushing skill would have been written to
            disk, checked for `mise`, and skipped by the `continue`. A skill that
            pushes says `--force-with-lease`, because the daemon's guard denies
            every other form. */
            if !body.contains("--force-with-lease") {
                continue;
            }
            assert!(
                body.contains("this repo's checks"),
                "skills/{name} pushes without running this repo's checks"
            );
        }
    }

    /// The language a run writes replies in is a setting, and a skill cannot have
    /// it substituted — so it has to *read* it. Moved here from `prompt.rs`, where
    /// the same rule was checked against the rendered template this replaced.
    #[test]
    fn the_review_skill_reads_the_language_rather_than_naming_one() {
        assert!(
            HANDLE_REVIEW.contains(&format!("${VAR_LANGUAGE}")),
            "the review skill hardcodes a language"
        );
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
        assert_eq!(
            v["name"], "orchd",
            "the namespace `/orchd:orch` resolves through"
        );
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
