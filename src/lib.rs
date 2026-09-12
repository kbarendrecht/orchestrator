//! The daemon's library: everything it knows, and nothing about serving it.
//!
//! **The server moved to `orchd-serve`** — `start`, the router, the pollers and
//! the four modules that answer HTTP — so this crate is what that one assembles.
//! `docs/crate-split.md` has the plan and what each step cost.
//!
//! What is left here is one crate only because steps 3 and 4 have not happened:
//! `config`, the forge and the patch machinery are a layer below the runtime
//! core, and the graph says they can be split whenever somebody wants to.

// The primitives, at the paths they have always had. `crate::git::…` and the
// rest resolve through these, so moving them into `orchd-base` cost no call site
// a rename — and `cargo` now refuses an import from `orchd-base` back up here,
// which is the whole point of the move.
pub use orchd_base::{
    child, edit, git, guard, headroom, model, proc, proposal, pty, review_commit, secret, timing,
    window,
};

pub mod api;
pub mod config;
pub mod diff;
pub mod env_source;
pub mod fix_pr;
pub mod forge;
pub mod health;
pub mod instance;
pub mod launch;
pub mod logging;
pub mod machine;
pub mod migrate;
pub mod names;
pub mod patch;
pub mod post;
pub mod reviews;
pub mod skills;
pub mod spawn;
pub mod state;
pub mod store;
pub mod story;
// Behind a feature, not `#[cfg(test)]`: `orchd-serve`'s tests build this crate as
// an ordinary dependency and cannot see a `cfg(test)` module.
#[cfg(any(test, feature = "test-util"))]
pub mod testutil;
pub mod triage;
pub mod update;
pub mod worktree;

use std::sync::Arc;

use state::AppState;

/// The directory the running executable sits in, when `orch` is really there.
///
/// Every packaging puts the two binaries side by side — the tarball, the `.deb`'s
/// `/usr/bin`, the AppImage's AppDir, the macOS bundle's `Contents/MacOS` — but
/// only the tarball's directory is on anybody's PATH. Answering `None` when the
/// sibling is missing keeps a development build (`cargo run`, where `orch` may
/// not have been built) from prepending a directory that has no `orch` in it.
pub fn sibling_bin_dir() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    dir.join("orch")
        .is_file()
        .then(|| dir.to_string_lossy().into_owned())
}

/// The repository this daemon polls, `owner/name`.
///
/// `pub` because the host refuses a second checkout on exactly this value, and the
/// child reports it on its ready line — the daemon is the only thing that knows it,
/// since it needs this checkout's own `upstream_remote` and `repo`.
pub fn resolve_repo(app: &Arc<AppState>) -> Option<(String, String)> {
    if let Some(r) = &app.cfg.repo {
        let (o, n) = r.split_once('/')?;
        return Some((o.to_string(), n.to_string()));
    }
    let url = forge::remote_url(&app.cfg.main_checkout, &app.cfg.upstream_remote)?;
    forge::repo_from_remote(&url)
}

// ---------------------------------------------------------------------------
// The SPA is served by the host, not from here.
//
// `index`, the asset routes and the window commands moved to [`crate::host`]
// when the page stopped being a daemon's business: a daemon manages one checkout,
// and there is one page over all of them. The `include_str!` tables went with
// them, so adding a JS module is still a Rust change — the line to add is now in
// `host::module`.
// ---------------------------------------------------------------------------
