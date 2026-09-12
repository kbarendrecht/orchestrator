//! Fixtures the test suites share.
//!
//! Every one of these was written out by hand in six to eight places, with the
//! copies drifting in ways that were never deliberate: hand-varied ports on
//! `AppState`s that never bind a socket, a scratch directory keyed to the pid in
//! one file and to the pid *and thread* in another, five spellings of `git init`.
//! The drift is the argument for a shared module rather than the line count: a
//! fixture that differs by accident makes two tests disagree about what they are
//! testing, and nothing says so.
//!
//! What is *deliberately* different stays at the call site. The shapes here are
//! neutral defaults, meant to be overridden with struct-update syntax
//! (`Pr { checks: Checks::Failing, ..testutil::pr(1) }`), so a test that needs a
//! failing PR or a repo on `develop` still says so where it is read.
//!
//! `#[cfg(test)]` in `lib.rs`, so none of this reaches a binary.
// Fixtures, so a panic is the report — the same reason `orchd-base`'s copy says
// it, and the same cause: this module is compiled as a library under a feature,
// which is what `clippy.toml`'s `allow-*-in-tests` cannot see.
#![allow(clippy::unwrap_used, clippy::expect_used)]

/// The filesystem and git fixtures live in `orchd-base` now, and are re-exported
/// here so every `testutil::scratch` in the workspace is the same function. The
/// builders below are the half that cannot move: they build an `AppState`.
pub use orchd_base::testutil::{git, scratch, scratch_repo, TRUE_BIN};
/// The forge fixtures moved with `forge` itself, and are re-exported for the same
/// reason as the three above: one `pr(1)` in the workspace, not two.
pub use orchd_repo::testutil::{comment, pr, test_config, thread};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::state::AppState;

/// An `AppState` over a scratch directory, and that directory.
///
/// No git in it: enough for everything that decides from session records rather
/// than from a tree. The port is the default, since nothing here binds a socket —
/// a test that reads the port back (`ORCH_URL`) sets its own through
/// [`app_with`].
pub fn app(tag: &str) -> (Arc<AppState>, PathBuf) {
    app_with(tag, "")
}

/// [`app`] with extra config keys, written as a JSON object body without the
/// braces: `app_with("tag", r#""upstream_ref":"origin/develop""#)`.
pub fn app_with(tag: &str, extra: &str) -> (Arc<AppState>, PathBuf) {
    let dir = scratch(tag);
    (app_at(&dir, extra), dir)
}

/// The same, over a directory the caller has already built — a real repo, say.
///
/// `{:?}` rather than `{}` for the path, so a directory whose name holds a quote
/// or a backslash still produces JSON that parses.
pub fn app_at(main: &Path, extra: &str) -> Arc<AppState> {
    let main = main.to_string_lossy().into_owned();
    let raw = if extra.is_empty() {
        format!(r#"{{"main_checkout":{main:?}}}"#)
    } else {
        format!(r#"{{"main_checkout":{main:?},{extra}}}"#)
    };
    let cfg: Config = serde_json::from_str(&raw).expect("the fixture config parses");
    AppState::new(cfg, "t".into(), crate::window::Chrome::None)
}
