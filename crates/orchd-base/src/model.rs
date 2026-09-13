//! The value types the layers below the runtime core share.
//!
//! A file, a diff of one, and the ref a rebase banks work on — what `git` produces
//! and `diff` consumes. **The daemon's domain model is not here**: workspaces,
//! sessions and their state moved up to `orchd::model`, which re-exports this one,
//! because a `Session` holds a live pty and this crate keeps no runtime state.
//! `MAIN` stays because `config` names the main workspace and cannot see up.

use serde::{Deserialize, Serialize};

pub const MAIN: &str = "main";

// ---------------------------------------------------------------------------
// Changed files (§4)
//
// None of the three below carries a `ts_rs` export: `git::status` still produces
// them, but nothing reaches them from `Snapshot` any more — the changed-files
// pane reads `changed`, and `WorkspaceView.files` was sent to every client on
// every tick and read by none.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Staged,
    Unstaged,
    Untracked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub status: FileStatus,
    /// Two-letter XY code from `git status --porcelain=v2`, kept verbatim.
    pub code: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileSet {
    pub staged: Vec<ChangedFile>,
    pub unstaged: Vec<ChangedFile>,
    pub untracked: Vec<ChangedFile>,
}

// **Here rather than in `diff`, which is what measures it.** `Tree::changed` is
// a `Vec` of these, so a type in `diff` meant the data model importing the
// module that fills it while that module imported the model back — one of the
// seventeen mutual pairs `mise run check-modules` counts. The rule the two
// halves now follow: a *shape* lives here, and the module that produces it
// depends on this one.
//
// A plain comment, not a doc one: `ts-rs` copies doc comments into
// `snapshot.d.ts`, and an argument about Rust module layering is not something
// the SPA's type file should carry.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "base.d.ts")
)]
pub struct DiffFile {
    pub path: String,
    /// Verbatim from `--name-status`: M, A, D, R…, C…
    pub status: String,
    pub added: u32,
    pub deleted: u32,
    pub binary: bool,
    /// Whether the client should fetch hunks without being asked.
    pub eager: bool,
    /// Present for renames.
    pub old_path: Option<String>,
    /// Whether this file has changes in the **index**, and whether it has changes
    /// in the **working tree** — `git status`'s two answers, joined on by path.
    ///
    /// **Not derivable from `status` above, and that is the point.** This list is
    /// `git diff <merge-base>`, so most rows on a PR branch differ from the base
    /// because of a *commit* and are otherwise clean. Offering "discard changes"
    /// against that list would be offering to throw away nothing on some rows and
    /// a commit's content on others, from a menu that cannot tell them apart. The
    /// pane's git verbs are drawn from these two instead, so what is offered is
    /// exactly what exists: staged → unstage, working-tree → stage, discard.
    ///
    /// Both `false` is the ordinary case (changed in a commit, clean on disk) and
    /// gets no verbs at all.
    pub staged: bool,
    pub unstaged: bool,
}

impl DiffFile {
    /// A file git has never seen. `git diff` cannot report one, so the pane's
    /// list would be missing exactly the files a session just created.
    ///
    /// No line counts: counting them means reading every new file on every
    /// reconcile, and an untracked file is entirely new by definition — the
    /// number would only ever say "all of it".
    pub fn untracked(f: &crate::model::ChangedFile) -> Self {
        DiffFile {
            path: f.path.clone(),
            status: "?".to_string(),
            added: 0,
            deleted: 0,
            binary: false,
            // Nothing to diff against, so there are no hunks to fetch.
            eager: false,
            old_path: None,
            // Untracked is neither: nothing of it is in the index, and there is no
            // tracked version for the working tree to differ from.
            staged: false,
            unstaged: false,
        }
    }
}

/// Work parked out of the way of a rebase, and how much of it there is.
// Here for the same reason as `DiffFile` above: `Workspace::banked` is one of
// these, and `git` is the module that makes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bank {
    /// The WIP commit. Named in every message about it, because
    /// `git stash apply <sha>` is the recovery a person can run without us.
    pub sha: String,
    /// Tracked files in it, for a strip that says "3 changed files are banked".
    pub files: u32,
}
