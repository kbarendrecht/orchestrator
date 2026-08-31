//! direnv, asked what it would export.

use super::EnvSource;

/// direnv. Carries nothing, the same as [`super::Mise`].
#[derive(Debug, Clone, Copy)]
pub struct Direnv;

impl EnvSource for Direnv {
    fn name(&self) -> &'static str {
        "direnv"
    }

    /// `direnv export json` is the machine-readable half of the hook a shell
    /// runs at every prompt. A directory with no `.envrc`, or one direnv has not
    /// been told to allow, prints nothing and exits 0 — which reads here as no
    /// variables, the same as no direnv at all.
    fn argv(&self) -> Vec<String> {
        vec!["direnv".into(), "export".into(), "json".into()]
    }

    /// A `null` value means direnv would *remove* that variable, and this drops
    /// it rather than acting on it.
    ///
    /// The removals describe leaving the previous directory, and a spawned
    /// session never was in one: it starts from the daemon's environment, which
    /// no `.envrc` has touched. Honouring them would remove things the daemon put
    /// there on purpose.
    fn parse(&self, stdout: &str) -> Vec<(String, String)> {
        super::json_object(stdout)
    }
}
