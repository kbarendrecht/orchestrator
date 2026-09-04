//! The vendored skills, handed to every session as a plugin directory.
//!
//! `skills/*/SKILL.md` are compiled in with `include_str!` for the same reason
//! `commands/*.md` are ([`crate::prompt`]): the daemon carries what it depends on
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

/// Every vendored skill, as `(directory name, body)`.
///
/// A table rather than a write per skill, because the writer and the frontmatter
/// test both walk it: adding a skill is then a line here, and it cannot be
/// written out without also being checked.
const VENDORED: &[(&str, &str)] = &[("orch", ORCH)];

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
/// site calls — including the story run whose `--allowedTools` will not let it
/// invoke a skill anyway. Uniform on purpose: a site that opts out is a site that
/// silently differs, and the cost of carrying it is two argv words.
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
