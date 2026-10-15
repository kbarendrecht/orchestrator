//! Forge fixtures, shared with `orchd`'s tests.
//!
//! Behind a `test-util` feature for the reason `orchd-base`'s copy gives: a
//! `#[cfg(test)]` module is invisible to a dependent crate's tests. `orchd`
//! re-exports them, so there is one `pr(1)` in the workspace.
//!
//! What is *deliberately* different stays at the call site. These are neutral
//! defaults, meant to be overridden with struct-update syntax
//! (`Pr { checks: Checks::Failing, ..testutil::pr(1) }`), so a test that needs a
//! failing PR still says so where it is read.

// Fixtures, so a panic is the report — and this module is compiled as a library
// under a feature, which is what `clippy.toml`'s `allow-*-in-tests` cannot see.
#![allow(clippy::unwrap_used, clippy::expect_used)]

/// The filesystem and git fixtures live in `orchd-base`, re-exported here so a
/// test in this crate says `testutil::scratch` like every other.
pub use orchd_base::testutil::{git, scratch, scratch_repo};

use crate::forge::{Checks, Pr};

/// A PR with nothing remarkable about it: open, mergeable, checks unknown, no
/// stack. Override what a test is actually about.
pub fn pr(number: u64) -> Pr {
    Pr {
        number,
        title: "t".into(),
        url: String::new(),
        head_ref: "feature/x".into(),
        head_repo: None,
        head_pushable: None,
        base_ref: "develop".into(),
        is_draft: false,
        mergeable: "MERGEABLE".into(),
        merge_state: "CLEAN".into(),
        checks: Checks::Unknown,
        head_sha: None,
        unresolved: 0,
        unresolved_capped: false,
        awaiting_you: 0,
        changes_requested: false,
        needs_you: false,
        children: vec![],
    }
}

/// A parsed `Config` for a test that only needs one to exist. `Config` has no
/// `Default` on purpose — `main_checkout` is canonicalised at parse, and a
/// default one would be a path that resolves to nothing.
///
/// Here rather than beside `Config` because `orchd`'s tests want it too, and a
/// `#[cfg(test)]` item does not cross a crate line.
pub fn test_config() -> crate::config::Config {
    crate::config::Config::parse(&format!(
        r#"{{"main_checkout":"{}"}}"#,
        std::env::temp_dir().display()
    ))
    .expect("a config")
}
