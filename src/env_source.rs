//! The EnvSource seam: where a checkout's own variables come from.
//!
//! A spawned session used to get the daemon's environment and nothing else, and
//! the daemon's environment is whatever started it — under a desktop launcher
//! that is the systemd user manager's, which holds no checkout's variables. So a
//! `.mcp.json` entry spelled `"Authorization": "Bearer ${SHORTCUT_API_TOKEN}"`
//! went out as literal text and the server answered 401, while the same session
//! started by typing `claude` in that checkout worked. That is the tool doing it:
//! `mise activate` exports a directory's variables at a shell prompt, and a
//! long-lived app has no prompt.
//!
//! The failure is quiet twice over. Nothing is unset — the variable is simply
//! absent, so the header expands to itself — and the terminal case keeps working,
//! which is where anybody would go to reproduce it.
//!
//! What differs between mise and direnv is one fact: the command that prints a
//! directory's environment. Both print a flat JSON object, so the parse is shared.
//! Everything else — the deadline, reading it per spawn in the session's own cwd,
//! and answering "no opinion" to every failure — is [`read`] and is the same
//! whatever the source is. Adding one is an arm in [`argv`] and nothing at the
//! call sites. (A trait and an enum dispatching over two unit structs used to
//! carry that one fact; both parsed the same way.)

use crate::config::EnvSourceKind;
use std::path::Path;

/// Long enough for a cold tool on a large config, short enough that a session
/// still starts if it hangs. `mise env` can reach the network — an uninstalled
/// tool — and a session must not wait on that.
const TIMEOUT_SECS: u64 = 5;

/// The command that prints a directory's environment, run *in* that directory, or
/// `None` when the daemon should ask nothing.
///
/// `None` is not a failure: a checkout whose variables are already in the daemon's
/// environment is a supported setup, and it is what you fall back to when a tool
/// here is doing the wrong thing.
///
/// Both tools have a machine-readable mode, and it is the one to use: the human
/// one is shell to `eval`, which would put a shell back in a path that exists
/// because there is no shell.
pub fn argv(kind: EnvSourceKind) -> Option<&'static [&'static str]> {
    match kind {
        EnvSourceKind::None => None,
        // `mise env --json` is `mise activate` without a shell: the same variables
        // a prompt in that directory would have exported, printed once. It prints
        // only what mise sets, not the whole environment — one exception, `PATH`,
        // which it derives from the caller's, so the tools this checkout pins come
        // out in front of the daemon's own.
        EnvSourceKind::Mise => Some(&["mise", "env", "--json"]),
        // `direnv export json` is the machine-readable half of the hook a shell
        // runs at every prompt. A directory with no `.envrc`, or one direnv has
        // not been told to allow, prints nothing and exits 0 — which reads here as
        // no variables, the same as no direnv at all. A `null` value means direnv
        // would *remove* that variable; `json_object` drops it rather than acting
        // on it, because the removals describe leaving the previous directory, and
        // a spawned session never was in one. Honouring them would remove things
        // the daemon put there on purpose.
        EnvSourceKind::Direnv => Some(&["direnv", "export", "json"]),
    }
}

/// The variables `kind`'s tool would export in `cwd`, or nothing at all.
///
/// Every failure answers "no opinion" rather than refusing the spawn: no tool on
/// PATH, a directory it has no config for, a config it does not trust, output it
/// cannot parse. A session missing a variable is degraded; a session that never
/// starts is lost.
///
/// Values are never logged. Half of what this returns is credentials.
///
/// Read per spawn rather than cached: it is ~30ms, and the alternative is a cache
/// that has to be invalidated by an edit to a file the daemon does not watch.
pub fn read(kind: EnvSourceKind, cwd: &Path) -> Vec<(String, String)> {
    let Some(argv) = argv(kind) else {
        return Vec::new();
    };
    let argv: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
    let out = match crate::proc::run_bounded(cwd, TIMEOUT_SECS, &argv, argv[0].as_str()) {
        Ok(out) => out,
        // Includes the common case of the tool not being installed, which is not
        // worth a warning on every spawn.
        Err(e) => {
            tracing::debug!("no {} environment for {}: {e:#}", argv[0], cwd.display());
            return Vec::new();
        }
    };
    if !out.status.success() {
        // An untrusted config lands here, and that one *is* worth saying: the
        // checkout has variables and the session will not get them until somebody
        // trusts it.
        tracing::warn!(
            "{} failed in {}: {}",
            argv.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Vec::new();
    }
    json_object(&String::from_utf8_lossy(&out.stdout))
}

/// A flat JSON object read as an environment, which is what both tools print.
///
/// A non-string value is dropped rather than stringified: an environment holds
/// strings, so anything else is a shape change to notice rather than to guess at.
/// direnv's `null` is the one that means something, and [`argv`] says what.
fn json_object(stdout: &str) -> Vec<(String, String)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return Vec::new();
    };
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_source_is_absence_not_an_error() {
        assert!(argv(EnvSourceKind::None).is_none());
        assert!(read(EnvSourceKind::None, &std::env::temp_dir()).is_empty());
    }

    /// The one fact per tool, pinned: a wrong flag here is a session that silently
    /// gets no variables.
    #[test]
    fn each_source_asks_its_tool_for_json() {
        assert_eq!(argv(EnvSourceKind::Mise), Some(&["mise", "env", "--json"][..]));
        assert_eq!(argv(EnvSourceKind::Direnv), Some(&["direnv", "export", "json"][..]));
    }

    #[test]
    fn reads_the_shape_the_tools_actually_emit() {
        let env = json_object(r#"{"PATH":"/a:/b","SHORTCUT_API_TOKEN":"sct_x"}"#);
        assert_eq!(env.len(), 2);
        assert!(env.contains(&("SHORTCUT_API_TOKEN".to_string(), "sct_x".to_string())));
    }

    /// Every unparseable answer is "no opinion", never a panic on the spawn path.
    #[test]
    fn junk_yields_no_variables() {
        assert!(json_object("").is_empty());
        assert!(json_object("mise ERROR config file is not trusted").is_empty());
        assert!(json_object("[1,2]").is_empty());
    }

    /// Guessing at a non-string would put the word `null` in an environment.
    #[test]
    fn a_non_string_value_is_dropped_not_stringified() {
        assert_eq!(
            json_object(r#"{"A":"1","B":null,"C":2}"#),
            vec![("A".to_string(), "1".to_string())]
        );
    }

    /// The read is bounded and fails open, so a directory with no tool at all
    /// still spawns a session.
    #[test]
    fn a_directory_with_no_tool_yields_no_variables() {
        // `direnv` may well be installed on the machine running this; either way
        // a temp dir has no `.envrc`, so the answer is empty and not a panic.
        assert!(read(EnvSourceKind::Direnv, &std::env::temp_dir()).is_empty());
    }
}
