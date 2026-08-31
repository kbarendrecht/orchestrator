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
//! Shaped after [`crate::tracker`], and for the same reason: what differs between
//! mise and direnv is two facts, not a workflow. A source is
//!
//! - the command that prints a directory's environment, and
//! - how to read what it printed.
//!
//! Everything else — the deadline, reading it per spawn in the session's own cwd,
//! and answering "no opinion" to every failure — is [`read`] and is the same
//! whatever the source is. Adding one is a variant here and nothing at the call
//! sites.

use crate::config::EnvSourceKind;
use std::path::Path;

mod direnv;
mod mise;

pub use direnv::Direnv;
pub use mise::Mise;

/// Long enough for a cold tool on a large config, short enough that a session
/// still starts if it hangs. `mise env` can reach the network — an uninstalled
/// tool — and a session must not wait on that.
const TIMEOUT_SECS: u64 = 5;

/// What the daemon has to know to ask a tool what a directory's environment is.
///
/// No credential and no config: the tool reads the checkout, and the checkout is
/// the argument. That is why a source carries nothing.
pub trait EnvSource: Send + Sync + Clone + 'static {
    /// What to call it in a log line.
    fn name(&self) -> &'static str;

    /// The command that prints the directory's environment, run *in* that
    /// directory.
    ///
    /// Both tools have a machine-readable mode, and it is the one to use: the
    /// human one is shell to `eval`, which would put a shell back in a path that
    /// exists because there is no shell.
    fn argv(&self) -> Vec<String>;

    /// The variables that command's stdout describes.
    ///
    /// Fallible only in the sense that junk yields nothing — see [`read`] for why
    /// nothing is the answer to every failure here.
    fn parse(&self, stdout: &str) -> Vec<(String, String)>;
}

/// The source the config selected, dispatched at runtime.
///
/// Enum rather than `Box<dyn EnvSource>` for the same reason [`crate::tracker`]
/// is one: the trait is `Clone`, which is not object-safe.
#[derive(Debug, Clone)]
pub enum EnvSourceImpl {
    Mise(Mise),
    Direnv(Direnv),
}

impl EnvSourceImpl {
    /// The source `kind` names, or `None` when the daemon should ask nothing.
    ///
    /// `None` is not a failure: a checkout whose variables are already in the
    /// daemon's environment is a supported setup, and it is what you fall back to
    /// when a tool here is doing the wrong thing.
    pub fn for_kind(kind: EnvSourceKind) -> Option<Self> {
        match kind {
            EnvSourceKind::None => None,
            EnvSourceKind::Mise => Some(EnvSourceImpl::Mise(Mise)),
            EnvSourceKind::Direnv => Some(EnvSourceImpl::Direnv(Direnv)),
        }
    }
}

impl EnvSource for EnvSourceImpl {
    fn name(&self) -> &'static str {
        match self {
            EnvSourceImpl::Mise(s) => s.name(),
            EnvSourceImpl::Direnv(s) => s.name(),
        }
    }

    fn argv(&self) -> Vec<String> {
        match self {
            EnvSourceImpl::Mise(s) => s.argv(),
            EnvSourceImpl::Direnv(s) => s.argv(),
        }
    }

    fn parse(&self, stdout: &str) -> Vec<(String, String)> {
        match self {
            EnvSourceImpl::Mise(s) => s.parse(stdout),
            EnvSourceImpl::Direnv(s) => s.parse(stdout),
        }
    }
}

/// The variables `source` would export in `cwd`, or nothing at all.
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
pub fn read(source: &EnvSourceImpl, cwd: &Path) -> Vec<(String, String)> {
    let argv = source.argv();
    let out = match crate::proc::run_bounded(cwd, TIMEOUT_SECS, &argv, source.name()) {
        Ok(out) => out,
        // Includes the common case of the tool not being installed, which is not
        // worth a warning on every spawn.
        Err(e) => {
            tracing::debug!("no {} environment for {}: {e:#}", source.name(), cwd.display());
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
    source.parse(&String::from_utf8_lossy(&out.stdout))
}

/// A flat JSON object read as an environment, which is what both tools print.
///
/// A non-string value is dropped rather than stringified: an environment holds
/// strings, so anything else is a shape change to notice rather than to guess at.
/// direnv's `null` is the one that means something, and [`Direnv`] says what.
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
        assert!(EnvSourceImpl::for_kind(EnvSourceKind::None).is_none());
    }

    #[test]
    fn for_kind_builds_the_configured_source_and_dispatches() {
        // Through the enum, never a concrete source: this is the call every
        // caller makes, so if dispatch is wrong it is wrong here.
        let s = EnvSourceImpl::for_kind(EnvSourceKind::Mise).expect("a source");
        assert_eq!(s.name(), "mise");
        assert_eq!(s.argv(), vec!["mise", "env", "--json"]);

        let d = EnvSourceImpl::for_kind(EnvSourceKind::Direnv).expect("a source");
        assert_eq!(d.name(), "direnv");
        assert_eq!(d.argv(), vec!["direnv", "export", "json"]);
    }

    /// Compiles only if the trait stays dispatchable, which is what lets a call
    /// site name `EnvSourceImpl` and no concrete source.
    #[test]
    fn callers_never_name_a_concrete_source() {
        fn takes_any(_: &impl EnvSource) {}
        takes_any(&EnvSourceImpl::for_kind(EnvSourceKind::Mise).unwrap());
    }

    #[test]
    fn reads_the_shape_the_tools_actually_emit() {
        let s = EnvSourceImpl::for_kind(EnvSourceKind::Mise).unwrap();
        let env = s.parse(r#"{"PATH":"/a:/b","SHORTCUT_API_TOKEN":"sct_x"}"#);
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
        let s = EnvSourceImpl::for_kind(EnvSourceKind::Direnv).unwrap();
        // `direnv` may well be installed on the machine running this; either way
        // a temp dir has no `.envrc`, so the answer is empty and not a panic.
        assert!(read(&s, &std::env::temp_dir()).is_empty());
    }
}
