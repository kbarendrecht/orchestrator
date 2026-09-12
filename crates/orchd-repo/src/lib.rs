//! One checkout, described.
//!
//! **The layer between the primitives and the runtime.** Everything here reads or
//! describes a single repository — what it is configured as, what its forge says,
//! what changed in it, what a session launched in it gets — and nothing here keeps
//! state about a session. That is `orchd`'s, and `cargo` refuses an import from
//! here into it.
//!
//! `docs/crate-split.md` has the plan and what each step cost.

pub mod config;
pub mod diff;
pub mod env_source;
pub mod forge;
pub mod instance;
pub mod launch;
pub mod logging;
pub mod machine;
pub mod migrate;
pub mod patch;
pub mod reviews;
pub mod skills;

// The forge fixtures, shared with `orchd`'s tests. A `#[cfg(test)]` module is
// invisible to a dependent crate's tests — the one thing a crate line changes.
#[cfg(any(test, feature = "test-util"))]
pub mod testutil;
