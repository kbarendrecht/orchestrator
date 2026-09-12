//! The primitives `orchd` is built out of.
//!
//! **A crate rather than thirteen modules, so the boundary is the compiler's.**
//! `tools/rust-modules.mjs` holds the rest of the daemon's module graph at a
//! number it has to be told; here `cargo` refuses an upward import outright.
//! Nothing in this crate may name `config`, `state`, `spawn` or anything else
//! above it, and there is no way to write one by accident.
//!
//! What belongs here: how to run a process, how to run git, what a session and a
//! workspace *are*, and the leaves beside them. What does not: what this checkout
//! is configured as, or anything that keeps runtime state.
//!
//! `docs/crate-split.md` has the plan this is step one of, and the measurements
//! that say the next three are legal today.

pub mod child;
pub mod edit;
pub mod git;
pub mod guard;
pub mod headroom;
pub mod model;
pub mod proc;
pub mod proposal;
pub mod pty;
pub mod review_commit;
pub mod secret;
pub mod timing;
pub mod window;

// The fixtures, shared with `orchd`'s tests through the `test-util` feature.
// A `#[cfg(test)]` module is invisible to a dependent crate's tests, which is the
// one thing about test code that a crate line changes.
#[cfg(any(test, feature = "test-util"))]
pub mod testutil;
