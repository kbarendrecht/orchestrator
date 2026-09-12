//! Fixtures shared by every crate's tests.
//!
//! **Behind a `test-util` feature, not `#[cfg(test)]`.** A `#[cfg(test)]` module
//! is compiled only when *this* crate's own tests build, so a dependent crate's
//! tests cannot see it — the one thing about test code that a crate boundary
//! changes. `orchd` turns the feature on in its dev-dependencies and re-exports
//! these three, so there is still exactly one `scratch` in the workspace.
//!
//! The `AppState` builders (`app`, `app_with`, `app_at`) stay in `orchd`: they
//! build a thing that does not exist down here.

// Fixtures, so a panic is the report: a `git init` that quietly failed produces a
// test that fails somewhere else entirely, several calls later.
//
// Spelled here rather than covered by `clippy.toml`'s `allow-*-in-tests`, because
// that reaches `#[cfg(test)]` code and this module is compiled as a library under
// a feature — which is the only way a dependent crate's tests can see it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

/// A command that exits at once, spelled the one way both platforms have.
///
/// **Not `/bin/true`, which macOS does not have** — only `/usr/bin/true`. Three
/// tests hardcoded the Linux path and so failed on the macos-14 runner alone,
/// with `ENOENT` from `portable-pty` rather than anything about the code they
/// were testing. The same shape as every other portability trap in this repo: it
/// compiles everywhere and answers wrongly on one platform.
///
/// Here rather than in `pty`'s test module, because `spawn` in `orchd` wants it
/// too and a `#[cfg(test)]` item does not cross a crate line.
pub const TRUE_BIN: &str = "/usr/bin/true";

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
///
/// **Canonical, and that is not a nicety — it is what makes a fixture behave like
/// the real thing on a Mac.** `Config::parse` resolves `main_checkout`, so every
/// path the daemon holds is resolved, and so is every path git prints. `$TMPDIR`
/// on macOS is a symlink into `/private`, so a fixture that skips this step hands
/// the code under test a path that matches *nothing* it will be compared against —
/// and the comparisons that fail are the silent kind: `workspace_for_path` decides
/// no workspace owns the directory, `holder_of_branch`'s answer looks like a
/// different tree. Two tests shipped that way and failed only on the macos-14
/// runner, after the tag was already pushed.
pub fn scratch(tag: &str) -> PathBuf {
    let thread: String = format!("{:?}", std::thread::current().id())
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let dir = std::env::temp_dir().join(format!("orchd-{tag}-{}-{thread}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // After the directory exists, since `canonicalize` reads the filesystem. The
    // fallback keeps a fixture on a platform that cannot resolve it working, which
    // is the same trade `git::holder_of_branch` makes.
    std::fs::canonicalize(&dir).unwrap_or(dir)
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
/// Canonical, like every [`scratch`] path now is; that docblock has the reason.
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
    dir
}
