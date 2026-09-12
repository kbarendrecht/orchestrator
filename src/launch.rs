//! How a session's process is built: its environment and its argv.
//!
//! **Its own module because both halves reach into features, and `config` must
//! not.** These two lived in `config`, where `session_env` called
//! [`crate::env_source`] and [`crate::story`] and `session_flags` called
//! [`crate::skills`] — so configuration imported the three features it configures
//! and each imported `config` back. Four of the seventeen mutual pairs
//! `mise run check-modules` counts were these.
//!
//! The layer this makes explicit: `config` is what the file says, the feature
//! modules are what each one does, and **this** is the one place that knows how to
//! turn the first into a process. Nothing in `config`, `env_source`, `story` or
//! `skills` may import this module; the spawners are its only callers.

use crate::config::Config;
use anyhow::Result;
use std::path::Path;

/// The environment a spawned Claude session gets, so the outcome never depends
/// on which shell started the daemon.
///
/// Transcripts are always on: resume (§2) and the teardown transcript check both
/// need one, and a session without a transcript costs you the conversation. Set
/// explicitly rather than inherited, because a shell inside a Claude Code session
/// carries `CLAUDE_CODE_CHILD_SESSION`, which turns transcript saving off in every
/// child — so without clearing it the daemon would behave differently depending on
/// what launched it.
///
/// Returns `(set, unset)`.
///
/// # Why every spawn goes through here
///
/// This used to be `transcript_env`, and each spawn site added its own `ORCH_*` on
/// top. They drifted, silently and more than once: `spawn_worktree_session` set
/// `ORCH_SESSION_ID` and stopped, so `orch` in a `claude --worktree` session had a
/// name for itself and no address and answered "only runs inside a session the
/// daemon started" — a sentence describing a session that is not one. The fix-pr and
/// triage spawns had the same hole. The signature changed rather than gaining a
/// default so the compiler names every site, which is the only reason a future spawn
/// cannot quietly leave one out.
///
/// `ask_token` is `None` for a run with nobody to ask — the headless triage pass.
///
/// `cwd` is where the session will run, and it is asked what it exports
/// ([`crate::env_source`]): the daemon's own environment is whatever started it, so
/// from a desktop launcher it holds no checkout's variables at all. That is the
/// other half of the tracker token below, and of every `${…}` a repo's `.mcp.json`
/// expands.
pub fn session_env(
    cfg: &Config,
    cwd: &Path,
    id: uuid::Uuid,
    ask_token: Option<&str>,
) -> (Vec<(String, String)>, Vec<&'static str>) {
    // The checkout's own variables first, so everything the daemon sets below wins
    // — the pty applies these in order, and a repo exporting `ORCH_ASK_TOKEN` must
    // not be able to overwrite the one this session was given.
    let mut set = crate::env_source::read(cfg.env_source, cwd);
    set.push((
        "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE".to_string(),
        "1".to_string(),
    ));
    set.push(("ORCH_SESSION_ID".to_string(), id.to_string()));
    if let Some(t) = ask_token {
        set.push(("ORCH_ASK_TOKEN".to_string(), t.to_string()));
    }
    // So `orch` needs no configuration: the session's own environment says where
    // the daemon is and who it is.
    set.push((
        "ORCH_URL".to_string(),
        format!("http://127.0.0.1:{}", cfg.port),
    ));
    // And so it is *findable*. `orch` ships beside the binary that is running, but
    // only the tarball puts that directory on your PATH — inside an AppImage or a
    // macOS bundle it is a mount point nothing else knows about, and the agent's
    // `orch new` would be a command not found. Prepended, so a build you are
    // testing wins over an installed one.
    if let Some(dir) = crate::sibling_bin_dir() {
        // Prepend to the checkout's PATH when there is one, not the daemon's: the
        // source above may already have put a PATH here holding the tools this
        // checkout pins, and rebuilding from the daemon's would drop them. Last
        // wins, the same rule the pty applies.
        let rest = set
            .iter()
            .rev()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
        set.push(("PATH".to_string(), format!("{dir}:{rest}")));
    }
    // What the repo's `.mcp.json` `${…}` expands from, named by the tracker.
    //
    // Claude Code expands those from the **real process environment** and nowhere
    // else — an `env` block in a settings file is not consulted, and an unset
    // variable is passed through as the literal `${VAR}`. Shortcut answers a
    // literal with "the access token expired", so the one thing the chain never
    // said was that no token had been sent. In the child's environment rather than
    // the settings file the daemon writes, which would put a secret in
    // `~/.config/orchd/`.
    if let Some(pair) = crate::config::token_env_pair(&cfg.tracker, &set) {
        set.push(pair);
    }
    (set, vec!["CLAUDE_CODE_CHILD_SESSION"])
}

/// The argv every spawned `claude` carries whatever the run is: the daemon's hook
/// settings, and the plugin dir its vendored skills live in.
///
/// The pair beside [`session_env`], and here for the same reason. Both halves are
/// per *process*, not per conversation — a resume that omits either gets a session
/// with no hooks or no skill, and neither says so — and both were spelled out at
/// each spawn site, which is exactly how the environment drifted before. One call
/// is what a sixth site has to remember instead of two.
///
/// It cannot be made un-forgettable the way `session_env`'s signature was: an argv
/// tail a site simply never appends is invisible to the compiler. The e2e fake
/// agent checks it for that reason.
pub fn session_flags() -> Result<Vec<String>> {
    let mut v = vec![
        "--settings".to_string(),
        Config::hooks_settings_path()?
            .to_string_lossy()
            .into_owned(),
    ];
    v.extend(crate::skills::flag());
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EnvSourceKind;

    #[test]
    fn persistence_clears_the_child_marker() {
        // The marker is what a daemon launched from inside a Claude session
        // inherits, and it silently turns transcripts off in every child.
        let cfg = Config {
            env_source: EnvSourceKind::None,
            ..crate::config::test_config()
        };
        let (set, unset) = session_env(&cfg, Path::new("/tmp"), uuid::Uuid::nil(), None);
        assert!(unset.contains(&"CLAUDE_CODE_CHILD_SESSION"));
        assert!(set
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE" && v == "1"));
    }
}
