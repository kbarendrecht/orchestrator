//! A checkout's daemon, as a child process.
//!
//! One `orchd` per checkout, spawned by whatever hosts the page. The host knows
//! nothing about sessions and the child knows nothing about siblings, which is the
//! whole point of the split — see [`crate::host`].
//!
//! **The child mints its own token and reports it.** Handing one down through the
//! environment would put it in the environment of every session that child spawns,
//! and `triage.rs` asserts the opposite invariant for exactly that reason. So the
//! child prints one line — `ready <port> <token>` — and the parent reads it.
//!
//! **Two independent ways to die**, because one is never enough for a process that
//! holds ptys. [`Child::stop`] signals the process group and waits; and the child
//! also exits on **stdin EOF**, so a host that was `SIGKILL`ed still takes its
//! children with it. The pipes are `O_CLOEXEC`, so they do not leak into the
//! `claude` grandchildren and keep a dead host's EOF from arriving.
//!
//! **The observer has to know why the child exited**, and that is the part worth
//! reading twice. A `close`, a quit and a crash all produce the same EOF, so an
//! observer that restarts on exit restarts the daemon a `close` just stopped — and
//! a restart runs `auto_resume`, which spawns an agent per live record. That is the
//! resurrection `multirepo.md` lists as a safety finding, arriving through the
//! front door. So a deliberate stop sets [`Child::stopping`] **before** it signals,
//! never after, and the observer reads it once `wait` returns.
//!
//! Ownership follows from `std::process::Child` rather than from taste: `wait` and
//! `try_wait` both need `&mut self`, so the observer thread that owns the handle
//! and a stop path that also held it cannot both exist. The handle lives in the
//! observer; every stop path signals **by pid**.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long to wait for a child's `ready` line.
///
/// A start is a network `git fetch` away from slow — measured at 1.3 s on a real
/// repo, and a cold cache or a slow remote is worse. Generous rather than tight,
/// because the cost of waiting is a slower boot and the cost of giving up early is
/// a checkout that reads as dead while its daemon is fine.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a stop waits before it stops asking.
///
/// The child's own `shutdown` runs every session's `kill_gracefully` under one
/// shared grace, and that is the **only** thing that reaches those sessions:
/// `portable-pty` calls `setsid`, so each agent is its own process group and no
/// sweep from out here can find them. So this has to be long enough for the child
/// to do its own work, and a child past it leaves orphans that the log names.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// What a launched daemon reported about itself.
#[derive(Debug, Clone)]
pub struct Ready {
    pub port: u16,
    pub token: String,
}

/// A live child daemon.
#[derive(Debug)]
pub struct Child {
    /// The checkout it manages. Canonical, because everything else compares
    /// against paths git printed or `Config::parse` resolved.
    pub checkout: PathBuf,
    pub ready: Ready,
    /// The leader's pid, which is also its process group id: [`spawn`] puts the
    /// child in its own group, so signalling the group cannot reach the host.
    pub pid: u32,
    /// Set before any deliberate signal. The observer reads it to tell a stop from
    /// a crash; the module doc says why that distinction is load-bearing.
    stopping: Arc<AtomicBool>,
    /// Kept open for the child's whole life: dropping it is the EOF that makes the
    /// second kill switch work.
    stdin: Mutex<Option<std::process::ChildStdin>>,
}

impl Child {
    /// Ask the child to stop, and wait for it.
    ///
    /// Sets [`Self::stopping`] first, so the observer that is about to be woken by
    /// the exit does not read it as a crash and restart what this just stopped.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        // The EOF, which is what the child is actually listening for. A stop that
        // only signalled would work too; this is the path that does not depend on
        // a signal handler being installed.
        drop(self.stdin.lock().unwrap().take());
        let deadline = Instant::now() + STOP_GRACE;
        // SIGTERM the group rather than the pid: the child's own children are in
        // it, and git removes its `.lock` files on TERM and not on KILL.
        crate::pty::signal_group_of(self.pid, libc::SIGTERM);
        while crate::pty::pid_alive(self.pid) {
            if Instant::now() > deadline {
                tracing::warn!(
                    pid = self.pid,
                    checkout = %self.checkout.display(),
                    "the checkout's daemon did not stop in time; killing it, and its \
                     sessions may outlive it"
                );
                crate::pty::signal_group_of(self.pid, libc::SIGKILL);
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }
}

/// Where the `orchd` binary is.
///
/// Beside the running executable first, which covers both the shipped bundle and
/// `cargo run` out of `target/debug`; then whatever is on `PATH`. **Not
/// `current_exe` itself**: re-executing the app would make every child pay the
/// desktop binary's loader cost — 133 shared objects against `orchd`'s 5, twenty of
/// them WebKit and GTK — to serve a page it never serves, and per-exec cost is
/// exactly what a Mac pays dearly for.
pub fn daemon_binary() -> PathBuf {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("orchd")))
        .filter(|p| p.is_file());
    beside.unwrap_or_else(|| PathBuf::from("orchd"))
}

/// Start a daemon for one checkout and wait for it to say it is serving.
///
/// `host_origin` is the origin the page is served from, which the child's own
/// `api::guard` then accepts — one exact string, on the argv, never in a file.
///
/// `on_exit` runs on the observer thread when the child goes, with `stopping`
/// telling it whether that was asked for. It is called exactly once, because the
/// observer is the only thing holding `wait`.
pub fn launch(
    checkout: &Path,
    host_origin: &str,
    state: &Path,
    on_exit: impl FnOnce(&Path, bool, Option<i32>) + Send + 'static,
) -> Result<Child> {
    launch_at(&daemon_binary(), checkout, host_origin, state, on_exit)
}

/// The real work, with the binary injected — the same split as
/// [`crate::instance::acquire_at`] and [`crate::config::Config::existing_at`], and
/// for the same reason: a test can drive the protocol and the observer against a
/// stub without needing a built `orchd`, and without setting a process-global
/// environment variable that every other test in the binary would also see.
pub fn launch_at(
    exe: &Path,
    checkout: &Path,
    host_origin: &str,
    state: &Path,
    on_exit: impl FnOnce(&Path, bool, Option<i32>) + Send + 'static,
) -> Result<Child> {
    let mut command = std::process::Command::new(exe);
    command
        .arg("--main")
        .arg(checkout)
        .arg("--host-origin")
        .arg(host_origin)
        // **Every durable thing this daemon writes goes here**: its config, its
        // `sessions.json`, its `automation.json`, its hook settings file, its
        // skills plugin dir, its transcripts archive, its log and its instance
        // lock. One variable rather than a flag per store, because a flag per
        // store is a flag somebody forgets and two checkouts then share a file.
        .env("ORCHD_CONFIG_DIR", state)
        // Without this the child prints prose for a person; with it, one line for a
        // parent. A token on stdout is only ever for the process that spawned this.
        .arg("--announce")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    // Its own process group, so a signal aimed at the child cannot reach the host,
    // and so the group is there to sweep when the leader has been reaped.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {} for {}", exe.display(), checkout.display()))?;

    let pid = child.id();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().context("the child has no stdout to read")?;

    // Read the ready line here rather than on the observer thread: a launch that
    // cannot report a port has failed, and the caller is the one that can say so.
    let mut lines = BufReader::new(stdout).lines();
    let ready = match read_ready(&mut lines, pid) {
        Ok(r) => r,
        Err(e) => {
            // Nothing has been recorded yet, so the failed child has to go here or
            // it is a process nobody is watching.
            crate::pty::signal_group_of(pid, libc::SIGKILL);
            let _ = child.wait();
            return Err(e);
        }
    };

    let stopping = Arc::new(AtomicBool::new(false));
    // **The observer is armed before this function returns**, and it is the only
    // thing that waits on this handle. A second waiter would work and then rot,
    // because "is this over" would have two answers maintained apart.
    let watcher = {
        let stopping = stopping.clone();
        let checkout = checkout.to_path_buf();
        std::thread::Builder::new()
            .name(format!("orchd-child-{pid}"))
            .spawn(move || {
                // Drain the rest of the child's stdout into the log, so a line it
                // prints after `ready` is not lost to a full pipe — a blocked write
                // in a daemon is worse than a noisy log.
                for line in lines {
                    match line {
                        Ok(text) if !text.is_empty() => {
                            tracing::info!(pid, "checkout daemon: {text}")
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                let code = child.wait().ok().and_then(|s| s.code());
                let asked = stopping.load(Ordering::SeqCst);
                tracing::info!(
                    pid,
                    code = code.unwrap_or(-1),
                    asked,
                    checkout = %checkout.display(),
                    "the checkout's daemon exited"
                );
                on_exit(&checkout, asked, code);
            })
    };
    if let Err(e) = watcher {
        crate::pty::signal_group_of(pid, libc::SIGKILL);
        bail!("could not watch the daemon for {}: {e}", checkout.display());
    }

    Ok(Child {
        checkout: checkout.to_path_buf(),
        ready,
        pid,
        stopping,
        stdin: Mutex::new(stdin),
    })
}

/// The one line a child prints for its parent: `ready <port> <token>`.
///
/// Anything else on the way is logged and skipped, because the child's own
/// subscriber writes to stdout too and a strict reader would fail on the first
/// warning it happened to print first.
fn read_ready(
    lines: &mut std::io::Lines<BufReader<std::process::ChildStdout>>,
    pid: u32,
) -> Result<Ready> {
    let deadline = Instant::now() + READY_TIMEOUT;
    for line in lines.by_ref() {
        let line = line.context("reading the daemon's first line")?;
        if let Some(rest) = line.strip_prefix("ready ") {
            let mut parts = rest.split_whitespace();
            let port = parts
                .next()
                .and_then(|p| p.parse::<u16>().ok())
                .with_context(|| format!("a ready line with no port: {line}"))?;
            let token = parts
                .next()
                .with_context(|| format!("a ready line with no token: {line}"))?
                .to_string();
            return Ok(Ready { port, token });
        }
        if !line.is_empty() {
            tracing::info!(pid, "checkout daemon: {line}");
        }
        if Instant::now() > deadline {
            break;
        }
    }
    bail!("the daemon never said it was ready")
}

/// The line [`read_ready`] parses, written by the child.
///
/// One spelling, because the writer and the reader are one protocol and this repo
/// has already paid for a pair that drifted (`--wait-for-pid`).
pub fn ready_line(port: u16, token: &str) -> String {
    format!("ready {port} {token}")
}

/// Exit when the parent closes our stdin.
///
/// The second kill switch, and the one that survives the host being `SIGKILL`ed:
/// there is no signal to catch then, only the pipe going away. A blocking read on
/// its own thread rather than anything clever, because a daemon's stdin has no
/// other use.
pub fn exit_on_stdin_eof(then: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name("orchd-stdin-eof".into())
        .spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                match std::io::Read::read(&mut std::io::stdin(), &mut buf) {
                    // EOF: the host is gone, or it asked by closing the pipe.
                    Ok(0) => break,
                    // A host that writes to it is not a protocol we have; ignore.
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            tracing::info!("the host closed our stdin; stopping");
            let _ = std::io::stdout().flush();
            then();
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Launch a stub, retrying the one failure that is the test's own doing.
    ///
    /// **`ETXTBSY` here is a fork race, not a defect.** These tests write a script
    /// and then exec it, they run in parallel, and a `fork` on another thread
    /// copies the fd table — so the write fd for a file one thread just closed can
    /// still be open in a sibling that has not reached its own `exec` yet, and
    /// Linux refuses to exec a file open for writing. `O_CLOEXEC` narrows that
    /// window to between fork and exec; it does not close it. The product never
    /// meets this: it execs `orchd`, which nobody has just written.
    fn launch_stub(
        exe: &Path,
        checkout: &Path,
        on_exit: impl FnOnce(&Path, bool, Option<i32>) + Send + 'static + Clone,
    ) -> Result<Child> {
        for _ in 0..50 {
            match launch_at(exe, checkout, "http://127.0.0.1:1234", checkout, on_exit.clone()) {
                Err(e) if format!("{e:#}").contains("Text file busy") => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => return other,
            }
        }
        launch_at(exe, checkout, "http://127.0.0.1:1234", checkout, on_exit)
    }

    /// A stand-in daemon: whatever the test needs said on stdout, then a wait.
    ///
    /// A fresh filename per call, so two tests never write one path.
    fn stub(script: &str) -> PathBuf {
        let dir = crate::testutil::scratch("child");
        let path = dir.join(format!("fake-orchd-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// The ready line is parsed, and prose before it does not break the read.
    #[test]
    fn a_child_reports_its_port_and_token_past_whatever_else_it_printed() {
        let exe = stub("echo 'logging to somewhere'\necho 'ready 7799 abc123'\nsleep 30");
        let repo = crate::testutil::scratch("child-ready");
        let child = launch_stub(&exe, &repo, |_, _, _| {}).expect("the child reported ready");
        assert_eq!(child.ready.port, 7799);
        assert_eq!(child.ready.token, "abc123");
        child.stop();
        assert!(!crate::pty::pid_alive(child.pid), "the child outlived its stop");
    }

    /// A stop is not a crash, and the observer must be able to tell.
    ///
    /// The distinction the whole module exists for: an observer that reads a stop
    /// as a crash restarts the daemon a `close` just stopped, and a restart runs
    /// `auto_resume`.
    #[test]
    fn a_stop_is_reported_as_asked_for_and_a_crash_is_not() {
        for (script, expect_asked) in
            [("echo 'ready 1 t'\nsleep 30", true), ("echo 'ready 1 t'\nexit 3", false)]
        {
            let exe = stub(script);
            let repo = crate::testutil::scratch("child-why");
            let seen = Arc::new(Mutex::new(None));
            let child = {
                let seen = seen.clone();
                launch_stub(&exe, &repo, move |_, asked, _| {
                    *seen.lock().unwrap() = Some(asked);
                })
                .expect("launched")
            };
            if expect_asked {
                child.stop();
            }
            // The observer runs on its own thread; wait for it rather than sleeping
            // a fixed time, which is the same rule the e2e flows follow.
            let deadline = Instant::now() + Duration::from_secs(10);
            while seen.lock().unwrap().is_none() {
                assert!(Instant::now() < deadline, "the observer never fired");
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(*seen.lock().unwrap(), Some(expect_asked));
        }
    }

    /// Closing the child's stdin is the second kill switch.
    ///
    /// Proven with a stub that only reads stdin, because that is what the real
    /// daemon's `exit_on_stdin_eof` thread does: no signal is involved, so this is
    /// the path that still works when the host was `SIGKILL`ed.
    #[test]
    fn a_child_dies_when_its_stdin_closes() {
        let exe = stub("echo 'ready 2 t'\ncat > /dev/null\nexit 0");
        let repo = crate::testutil::scratch("child-eof");
        let child = launch_stub(&exe, &repo, |_, _, _| {}).expect("launched");
        let pid = child.pid;
        // No signal: just the pipe going away.
        drop(child.stdin.lock().unwrap().take());
        let deadline = Instant::now() + Duration::from_secs(10);
        while crate::pty::pid_alive(pid) {
            assert!(Instant::now() < deadline, "the child ignored its stdin closing");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A binary that never reports is a failed launch, not a silent one.
    #[test]
    fn a_child_that_never_says_ready_is_a_failed_launch() {
        let exe = stub("exit 1");
        let repo = crate::testutil::scratch("child-mute");
        let err = launch_stub(&exe, &repo, |_, _, _| {}).expect_err("a mute child launched");
        assert!(
            format!("{err:#}").contains("never said it was ready"),
            "the failure did not say what was missing: {err:#}"
        );
    }
}
