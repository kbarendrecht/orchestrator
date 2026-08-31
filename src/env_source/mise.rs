//! mise, asked what it would export.

use super::EnvSource;

/// mise. Carries nothing: the checkout is the argument, and there is no
/// credential to hold.
#[derive(Debug, Clone, Copy)]
pub struct Mise;

impl EnvSource for Mise {
    fn name(&self) -> &'static str {
        "mise"
    }

    /// `mise env --json` is `mise activate` without a shell: the same variables a
    /// prompt in that directory would have exported, printed once.
    ///
    /// It prints only what mise sets, not the whole environment — one exception,
    /// `PATH`, which it derives from the caller's, so the tools this checkout
    /// pins come out in front of the daemon's own.
    fn argv(&self) -> Vec<String> {
        vec!["mise".into(), "env".into(), "--json".into()]
    }

    fn parse(&self, stdout: &str) -> Vec<(String, String)> {
        super::json_object(stdout)
    }
}
