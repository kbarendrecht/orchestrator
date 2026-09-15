//! `orchd --version` answers and does nothing else.
//!
//! **An integration test because the defect is the process, not a function.**
//! `--version` used to fall through to an ordinary start — it took the instance
//! lock on whatever checkout the config named, rotated that checkout's log,
//! fetched upstream and began polling GitHub. No unit test can see that, because
//! what is wrong is everything the binary *did* on the way to not answering.
//!
//! The second half is the same shape: `println!` unwraps its write, so a reader
//! that has already gone takes the process out with `failed printing to stdout:
//! Broken pipe`. Reported from a real machine in #18, out of `orchd::main`. Only
//! running the binary against a closed pipe shows it.

use std::io::Read;
use std::process::{Command, Stdio};

/// Where this test lets the daemon keep state, so an accidental start is visible.
///
/// Its own directory per test, and deliberately **not** created: `--version` must
/// not bring it into being either.
fn unused_state(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("orchd-cliver-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn version_answers_and_leaves_nothing_behind() {
    let state = unused_state("plain");
    let out = Command::new(env!("CARGO_BIN_EXE_orchd"))
        .arg("--version")
        // A checkout that does not exist. A real start would fail loudly on it;
        // `--version` must not look.
        .env("ORCHD_CONFIG_DIR", &state)
        .output()
        .expect("orchd --version ran");

    assert!(out.status.success(), "--version exited {:?}", out.status);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains(env!("CARGO_PKG_VERSION")),
        "--version did not print the version: {said:?}"
    );

    /* **The whole point, and the half a `--version` test usually forgets.** The
    old behaviour printed nothing and started a daemon; a test that only read
    stdout would have caught that. This one also asserts the silence: no config
    dir, so no `config.json`, no `orchd.log` and no `instance.pid`. */
    assert!(
        !state.exists(),
        "--version created {} — it started a daemon",
        state.display()
    );
}

/// A reader that leaves must not take the process with it.
///
/// `--version` is the shortest path to a write, which makes it the one place this
/// can be provoked without racing a daemon's startup. The guard is not in this
/// binary's own code any more: `print_stdout` is denied across the workspace and
/// `main.rs` no longer opts out, so the lint refuses the panicking form outright.
/// This proves the deny is doing what it is there for.
#[test]
fn a_closed_stdout_is_not_a_panic() {
    let state = unused_state("epipe");
    let mut child = Command::new(env!("CARGO_BIN_EXE_orchd"))
        .arg("--version")
        .env("ORCHD_CONFIG_DIR", &state)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("orchd spawned");

    // Close the read end before the child writes, which is what a parent that has
    // already exited leaves behind.
    drop(child.stdout.take());

    let mut said = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut said);
    }
    let status = child.wait().expect("orchd exited");

    assert!(
        !said.contains("panicked"),
        "a closed stdout panicked the daemon: {said}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert!(
            status.signal().is_none(),
            "a closed stdout killed the daemon with signal {:?}",
            status.signal()
        );
    }
}
