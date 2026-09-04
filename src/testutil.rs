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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::forge::{Checks, Comment, Pr, Thread};
use crate::state::AppState;

/// A scratch directory of this test's own, emptied first.
///
/// **The thread id is in the name, not only the pid.** `cargo test` runs the
/// suite in parallel threads of one process, so a pid-keyed directory is shared
/// by every test that asks for it — and since this wipes what it finds, two of
/// them racing is one test deleting the other's fixture mid-run. `tag` is what
/// distinguishes tests running on the *same* thread.
///
/// **Digits of the thread id, not its `Debug`**, which prints `ThreadId(3)`.
/// Several tests build a `sh -c` command around this path, and unquoted
/// parentheses in it are a shell syntax error rather than a missing directory —
/// so the failure lands somewhere else entirely, as it did here once.
pub fn scratch(tag: &str) -> PathBuf {
    let thread: String = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let dir = std::env::temp_dir().join(format!("orchd-{tag}-{}-{thread}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run git in `dir`, asserting it worked, and hand back its stdout trimmed.
///
/// The assertion is the point: a fixture whose `git init` quietly failed produces
/// a test that fails somewhere else entirely, several calls later.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A scratch git repository on `main`, with one empty commit so `HEAD` resolves.
///
/// **Canonicalised**, because git reports resolved paths and a test comparing its
/// output against this one has to agree with it. `$TMPDIR` on macOS is under
/// `/var`, which is a symlink into `/private`, so unresolved it matched nothing
/// there — while on Linux `/tmp` is a real directory and the difference never
/// showed.
///
/// The commit is empty on purpose: a test that cares what is in the tree writes
/// and commits its own content, and one that only needs a repo with a history
/// gets one without a file it has to know about.
pub fn scratch_repo(tag: &str) -> PathBuf {
    let dir = scratch(tag);
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    git(&dir, &["commit", "-q", "--allow-empty", "-m", "root"]);
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

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

/// One comment by `author`, of the shape a review thread carries.
pub fn comment(id: u64, author: &str, body: &str) -> Comment {
    Comment {
        database_id: id,
        author: author.into(),
        body: body.into(),
        created_at: "2026-08-17T00:00:00Z".into(),
        url: "u".into(),
        diff_hunk: None,
        viewer_thumbed: false,
    }
}

/// An open, answerable thread carrying one comment by `author`.
pub fn thread(id: &str, path: Option<&str>, line: Option<u32>, author: &str) -> Thread {
    Thread {
        id: id.into(),
        path: path.map(str::to_string),
        line,
        start_line: None,
        original_line: None,
        is_resolved: false,
        is_outdated: false,
        comments: vec![comment(100, author, "you call this twice")],
        answerable: true,
    }
}
