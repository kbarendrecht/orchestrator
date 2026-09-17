//! A checkout's daemon, as a child process.
//!
//! One `orchd` per checkout, spawned by whatever hosts the page. The host knows
//! nothing about sessions and the child knows nothing about siblings, which is the
//! whole point of the split — see `host`.
//!
//! **The child mints its own token and reports it.** Handing one down through the
//! environment would put it in the environment of every session that child spawns,
//! and `triage.rs` asserts the opposite invariant for exactly that reason. So the
//! child prints one line — `ready <port> <token> <repo>` — and the parent reads it.
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
//! a restart runs `auto_resume`, which spawns an agent per live record — the same
//! resurrection `Host::add_checkout` spends an ask on avoiding, arriving through
//! the front door instead. So a deliberate stop sets [`Child::stopping`] **before** it signals,
//! never after, and the observer reads it once `wait` returns.
//!
//! Ownership follows from `std::process::Child` rather than from taste: `wait` and
//! `try_wait` both need `&mut self`, so the observer thread that owns the handle
//! and a stop path that also held it cannot both exist. The handle lives in the
//! observer; every stop path signals **by pid**.

use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long to wait for a child's `ready` line.
///
/// A start is a network `git fetch` away from slow — measured at 1.3 s on a real
/// repo, and a cold cache or a slow remote is worse. Generous rather than tight,
/// because the cost of waiting is a slower boot and the cost of giving up early is
/// a checkout that reads as dead while its daemon is fine.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// How many of the child's stderr lines to keep back for a failure message.
///
/// **Its stderr is the only place some failures exist.** A daemon that dies before
/// `logging::init` finishes — a refused code signature, a missing dynamic library,
/// an `exec` the kernel killed — writes no log line at all, so the file the host
/// points at is empty and the pipe held the whole diagnosis. Bounded rather than
/// whole, because this is kept for a process that may run for days.
const STDERR_KEPT: usize = 20;

/// How long a failed launch waits for the stderr reader before quoting it.
///
/// Long enough for a thread that already has the bytes to be scheduled on a loaded
/// runner, short enough that it is never felt: this runs only on the path where a
/// launch has already failed, and the alternative is a diagnosis missing the line
/// that names the cause. See [`settle`].
const STDERR_SETTLE: Duration = Duration::from_millis(500);

/// How long to keep asking for an exited child's status.
///
/// `try_wait` right after stdout EOF can still answer `None`: the last write and
/// the exit are two events, and on a loaded machine they are not the same
/// microsecond. Answering "still running" there would put the wrong sentence in
/// front of the one person who needs the right one.
const FATE_GRACE: Duration = Duration::from_millis(500);

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
    /// The repository it will poll, `owner/name`, or `None` when no remote gives
    /// it one. **Reported rather than derived by the host**, because the identity
    /// needs `upstream_remote` and `repo`, and both live in that checkout's own
    /// `config.json` — the file the host may only just have created. The host's own
    /// guess at `add` time refuses the ordinary case cheaply; this is the
    /// authoritative answer, and a clash the guess could not see is named on the
    /// row.
    pub repo: Option<String>,
}

/// A live child daemon.
#[derive(Debug)]
pub struct Child {
    /// The checkout it manages. Canonical, because everything else compares
    /// against paths git printed or `Config::parse` resolved.
    pub checkout: PathBuf,
    pub ready: Ready,
    /// The leader's pid, which is also its process group id: [`launch`] puts the
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
    /// One consequence worth knowing: the wait below ends when the pid is
    /// *reaped*, not when it dies. `pid_alive` is `kill(pid, 0)` and a zombie
    /// answers yes, so this sits until the observer thread has drained stdout and
    /// called `wait`. That is the right thing to wait for — the child is only
    /// really gone once somebody has collected it — but it means a slow drain
    /// shows up here as a slow stop rather than as a missed signal.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        // The EOF, which is what the child is actually listening for. A stop that
        // only signalled would work too; this is the path that does not depend on
        // a signal handler being installed.
        // Poisoning does not matter here: the field is an `Option` this takes
        // whole, and refusing to stop the child because some other thread panicked
        // would strand the process this call exists to end.
        drop(
            self.stdin
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take(),
        );
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
    let beside = std::env::current_exe().ok();
    let (exe, found) = daemon_binary_beside(beside.as_deref());
    /* **The fallback is said out loud, because it is the one that fails later.**
    `orchd` alone is a PATH lookup, and a PATH that has no `orchd` turns into
    `starting orchd for <checkout>: No such file or directory` at spawn — the
    2026-09-14 half of #18, where the only clue that the lookup had missed was
    that the binary in the message had no directory in front of it. An install
    whose layout moved says so here instead, one line before the failure. */
    if !found {
        tracing::warn!(
            beside = ?beside,
            "no `orchd` beside this executable; falling back to PATH, and a PATH \
             without it will fail every checkout at spawn"
        );
    }
    exe
}

/// The choice on its own, with the running executable handed in.
///
/// Split so a test can drive both arms: `current_exe` is the process's own and
/// there is no setting it, so the interesting case — an install where the two
/// binaries are no longer siblings — is otherwise unreachable from a test.
///
/// Answers where to exec and whether that was the sibling, because the caller
/// wants to say which it got.
fn daemon_binary_beside(running: Option<&Path>) -> (PathBuf, bool) {
    let beside = running
        .and_then(Path::parent)
        .map(|dir| dir.join("orchd"))
        .filter(|p| p.is_file());
    match beside {
        Some(p) => (p, true),
        None => (PathBuf::from("orchd"), false),
    }
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
    no_resume: bool,
    on_exit: impl FnOnce(&Path, bool, Option<i32>) + Send + 'static,
) -> Result<Child> {
    launch_at(
        &daemon_binary(),
        checkout,
        host_origin,
        state,
        no_resume,
        on_exit,
    )
}

/// The real work, with the binary injected — the same split as
/// `instance::acquire_at` and `config::Config::existing_at`, and
/// for the same reason: a test can drive the protocol and the observer against a
/// stub without needing a built `orchd`, and without setting a process-global
/// environment variable that every other test in the binary would also see.
pub fn launch_at(
    exe: &Path,
    checkout: &Path,
    host_origin: &str,
    state: &Path,
    no_resume: bool,
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
        /* **Piped, and it used to be inherited.** A host started from a launcher
        has no stderr of its own, so a child's went to `/dev/null` — and that is
        where the whole diagnosis went for a daemon that died before its log
        file existed (#18: 644 ms, no log line, and a message that named
        nothing). Drained on its own thread below, because a pipe nobody reads
        fills and then blocks the daemon writing into it. */
        .stderr(std::process::Stdio::piped());
    // Its own process group, so a signal aimed at the child cannot reach the host,
    // and so the group is there to sweep when the leader has been reaped.
    // The person's answer to "resume what was live here?", travelling to the one
    // process that knows what a session is. Only ever set by an `add`.
    if no_resume {
        command.arg("--no-resume");
    }
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
    let stdout = child
        .stdout
        .take()
        .context("the child has no stdout to read")?;
    let stderr = child
        .stderr
        .take()
        .context("the child has no stderr to read")?;
    let (said, reading) = drain_stderr(stderr, pid);

    /* **Read on a thread, received with a deadline.** This used to iterate the
    child's lines here, and the deadline was checked *after* each line arrived —
    so a child that was alive and silent parked this thread for ever and
    [`READY_TIMEOUT`] could not fire at all. The pump turns "no line yet" into
    something a timeout can observe, and the observer below drains the same
    channel afterwards, so there is still exactly one reader of that pipe. */
    let lines = pump_stdout(stdout);
    let ready = match read_ready(&lines, pid, READY_TIMEOUT) {
        Ok(r) => r,
        Err(unready) => {
            // Asked before the kill below, or the only status this can report is
            // the signal it is about to send.
            let fate = fate_of(&mut child, unready.child_may_have_exited());
            // Nothing has been recorded yet, so the failed child has to go here or
            // it is a process nobody is watching.
            crate::pty::signal_group_of(pid, libc::SIGKILL);
            let _ = child.wait();
            // The pipe is closed now, so its reader is about to end. Wait for it,
            // or quote a buffer that has not been filled yet.
            settle(&reading);
            /* **Everything the caller needs to act, in one sentence.** The host
            shows this string and nothing else, and a person reading "the daemon
            never said it was ready" has no next step — not the exit status, not
            the stderr that named the real cause, and not even the path of the
            log to go and read. */
            bail!(
                "{unready}{}{}; its log is at {}",
                fate,
                kept_stderr(&said),
                state.join("orchd.log").display()
            );
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
                    if !line.is_empty() {
                        tracing::info!(pid, "checkout daemon: {line}");
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

/// Why a launch had no `ready` line, which is the thing the old message left out.
///
/// Three outcomes, not one, because the next step differs for each: a child that
/// closed its stdout has a status and probably some stderr, a child still running
/// after [`READY_TIMEOUT`] is wedged rather than broken, and a pipe that failed to
/// read is the host's own problem.
#[derive(Debug)]
enum Unready {
    /// The child's stdout closed without a `ready` line.
    Gone,
    /// The child is alive and has said nothing for this long.
    Silent(Duration),
    /// Reading the pipe itself failed.
    Unreadable(String),
}

impl Unready {
    /// Whether it is worth waiting a moment for an exit status.
    ///
    /// Only [`Self::Gone`] means the child was on its way out; polling a `Silent`
    /// one is [`FATE_GRACE`] spent to learn what is already known.
    fn child_may_have_exited(&self) -> bool {
        matches!(self, Self::Gone)
    }
}

impl std::fmt::Display for Unready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The wording the host has shown since this existed, kept because it
            // is the sentence in every report and every test.
            Self::Gone => write!(f, "the daemon never said it was ready"),
            Self::Silent(d) => write!(
                f,
                "the daemon never said it was ready within {}s and is still running",
                d.as_secs()
            ),
            Self::Unreadable(e) => write!(f, "could not read the daemon's first line: {e}"),
        }
    }
}

/// Read the child's stdout on its own thread, one line per message.
///
/// **A blocking read cannot be given a deadline, and a channel can.** That is the
/// whole reason this thread exists — see the call site. The sender is dropped when
/// the pipe ends, which is the EOF the receiver sees as a disconnect.
fn pump_stdout(stdout: std::process::ChildStdout) -> Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    // Detached on purpose: it ends with the pipe, and nothing waits for it.
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Keep the child's stderr, and log it as it arrives.
///
/// Both halves matter. The log line is for a failure that happens later, when
/// there is a checkout to attach it to; the kept ring is for a failure that
/// happens *now*, before the child has a log file of its own.
fn drain_stderr(
    stderr: std::process::ChildStderr,
    pid: u32,
) -> (Arc<Mutex<VecDeque<String>>>, std::thread::JoinHandle<()>) {
    let kept = Arc::new(Mutex::new(VecDeque::new()));
    let mine = kept.clone();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if line.is_empty() {
                continue;
            }
            tracing::warn!(pid, "checkout daemon said: {line}");
            let mut kept = mine.lock().unwrap_or_else(|p| p.into_inner());
            if kept.len() == STDERR_KEPT {
                kept.pop_front();
            }
            kept.push_back(line);
        }
    });
    (kept, reader)
}

/// Give the stderr reader a moment to finish before its lines are quoted.
///
/// **The buffer is filled on another thread, so reading it the instant a launch
/// fails is a race the failure loses.** The child writes its complaint, exits, and
/// the parent notices the missing `ready` line — all before that thread has been
/// scheduled to turn the bytes into lines. The failure then names the exit status
/// and says the child said nothing, which is the one sentence #18 exists to
/// prevent.
///
/// It cost a release rather than a test run: `a_failed_launch_names_the_status_the_stderr_and_the_log`
/// went red on a loaded CI runner in the `build` leg of a tag, so v2026.9.20 was
/// never published. It had been flaky for a while and read as noise.
///
/// Bounded, and never a join: a child that is alive and silent keeps its pipe open
/// for ever, and this runs on the path where that is exactly what may have
/// happened. EOF arrives when the child exits, so the wait is real only when there
/// is something to wait for.
fn settle(reader: &std::thread::JoinHandle<()>) {
    let deadline = Instant::now() + STDERR_SETTLE;
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The kept stderr, as a clause to append to a failure, or nothing.
fn kept_stderr(kept: &Mutex<VecDeque<String>>) -> String {
    let kept = kept.lock().unwrap_or_else(|p| p.into_inner());
    if kept.is_empty() {
        return String::new();
    }
    format!(
        "; it said: {}",
        kept.iter().cloned().collect::<Vec<_>>().join(" | ")
    )
}

/// What became of the child, as a clause to append to a failure, or nothing.
///
/// **A signal is not a code**, and conflating them is what hides the case this was
/// written for: a binary the kernel refuses leaves no exit code at all, only
/// `SIGKILL`, and `ExitStatus::code` answers `None` for it — which read as "no
/// status" and printed nothing.
fn fate_of(child: &mut std::process::Child, wait_a_moment: bool) -> String {
    let deadline = Instant::now()
        + if wait_a_moment {
            FATE_GRACE
        } else {
            Duration::ZERO
        };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if let Some(code) = status.code() {
                    return format!(" (it exited with code {code})");
                }
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    if let Some(sig) = status.signal() {
                        return format!(" (it was killed by signal {sig})");
                    }
                }
                return " (it exited without a status)".to_string();
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => return " (it was still running)".to_string(),
            Err(e) => return format!(" (its status could not be read: {e})"),
        }
    }
}

/// The one line a child prints for its parent: `ready <port> <token>`.
///
/// Anything else on the way is logged and skipped, because the child's own
/// subscriber writes to stdout too and a strict reader would fail on the first
/// warning it happened to print first.
///
/// **The deadline is on the wait, not on the lines.** It used to be tested inside
/// the loop body, so it could only fire *after* a line arrived — a child that
/// printed nothing and stayed alive was waited on for ever, and the timeout was
/// unreachable code that read as a guarantee.
fn read_ready(lines: &Receiver<String>, pid: u32, patience: Duration) -> Result<Ready, Unready> {
    let deadline = Instant::now() + patience;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = match lines.recv_timeout(left) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => return Err(Unready::Silent(patience)),
            Err(RecvTimeoutError::Disconnected) => return Err(Unready::Gone),
        };
        if let Some(rest) = line.strip_prefix("ready ") {
            let mut parts = rest.split_whitespace();
            let port = match parts.next().and_then(|p| p.parse::<u16>().ok()) {
                Some(port) => port,
                None => {
                    return Err(Unready::Unreadable(format!(
                        "a ready line with no port: {line}"
                    )))
                }
            };
            let token = match parts.next() {
                Some(token) => token.to_string(),
                None => {
                    return Err(Unready::Unreadable(format!(
                        "a ready line with no token: {line}"
                    )))
                }
            };
            // Optional, and the one field that may be absent: a checkout with no
            // matching remote has no repository identity, and that is an ordinary
            // local-only checkout rather than a malformed line.
            let repo = parts.next().filter(|r| *r != NO_REPO).map(str::to_string);
            return Ok(Ready { port, token, repo });
        }
        if !line.is_empty() {
            tracing::info!(pid, "checkout daemon: {line}");
        }
    }
}

/// What the fourth field says when the checkout has no repository identity.
///
/// A placeholder rather than a short line, because the line is positional and a
/// reader that accepts both lengths is two protocols.
const NO_REPO: &str = "-";

/// The line [`read_ready`] parses, written by the child.
///
/// One spelling, because the writer and the reader are one protocol and this repo
/// has already paid for a pair that drifted (`--wait-for-pid`).
pub fn ready_line(port: u16, token: &str, repo: Option<&str>) -> String {
    format!("ready {port} {token} {}", repo.unwrap_or(NO_REPO))
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
            match launch_at(
                exe,
                checkout,
                "http://127.0.0.1:1234",
                checkout,
                false,
                on_exit.clone(),
            ) {
                Err(e) if format!("{e:#}").contains("Text file busy") => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => return other,
            }
        }
        launch_at(
            exe,
            checkout,
            "http://127.0.0.1:1234",
            checkout,
            false,
            on_exit,
        )
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
        let exe = stub("echo 'logging to somewhere'\necho 'ready 7799 abc123 acme/mono'\nsleep 30");
        let repo = crate::testutil::scratch("child-ready");
        let child = launch_stub(&exe, &repo, |_, _, _| {}).expect("the child reported ready");
        assert_eq!(child.ready.port, 7799);
        assert_eq!(child.ready.token, "abc123");
        assert_eq!(child.ready.repo.as_deref(), Some("acme/mono"));
        /* **Waited for, not read once.** `pid_alive` is `kill(pid, 0)`, which
        succeeds for a *zombie* — and the thing that reaps this child is the
        observer thread, which only gets there after it has drained the child's
        stdout. So a stop that worked perfectly can still read as alive for as
        long as that takes, which is why the neighbouring tests all wait. This
        one asserted instantly and went red on the macos-14 runner alone. */
        child.stop();
        let deadline = Instant::now() + Duration::from_secs(10);
        while crate::pty::pid_alive(child.pid) {
            assert!(Instant::now() < deadline, "the child outlived its stop");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A stop is not a crash, and the observer must be able to tell.
    ///
    /// The distinction the whole module exists for: an observer that reads a stop
    /// as a crash restarts the daemon a `close` just stopped, and a restart runs
    /// `auto_resume`.
    #[test]
    fn a_stop_is_reported_as_asked_for_and_a_crash_is_not() {
        for (script, expect_asked) in [
            ("echo 'ready 1 t -'\nsleep 30", true),
            ("echo 'ready 1 t -'\nexit 3", false),
        ] {
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
        let exe = stub("echo 'ready 2 t -'\ncat > /dev/null\nexit 0");
        let repo = crate::testutil::scratch("child-eof");
        let child = launch_stub(&exe, &repo, |_, _, _| {}).expect("launched");
        let pid = child.pid;
        // No signal: just the pipe going away.
        drop(child.stdin.lock().unwrap().take());
        let deadline = Instant::now() + Duration::from_secs(10);
        while crate::pty::pid_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "the child ignored its stdin closing"
            );
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

    /// **The failure has to carry the diagnosis, because nothing else will.**
    ///
    /// #18 is this test: a daemon that died in 644 ms, a host that said only "the
    /// daemon never said it was ready", and a log file that was empty because the
    /// child never got as far as writing one. The exit status was in hand and
    /// dropped, the stderr went to an inherited descriptor that a launcher-started
    /// app does not have, and the log path was never printed — so the one person
    /// who could act had nothing to act on.
    #[test]
    fn a_failed_launch_names_the_status_the_stderr_and_the_log() {
        let exe = stub("echo 'libxcrun is missing' >&2\nexit 9");
        let repo = crate::testutil::scratch("child-diagnosis");
        let err = launch_stub(&exe, &repo, |_, _, _| {}).expect_err("a dying child launched");
        let said = format!("{err:#}");
        assert!(said.contains("code 9"), "no exit status: {said}");
        assert!(said.contains("libxcrun is missing"), "no stderr: {said}");
        assert!(said.contains("orchd.log"), "no log path: {said}");
    }

    /// A child killed by a signal has no exit code, and that is the case to name.
    ///
    /// The shape #18 most likely was — a binary the kernel refused — leaves
    /// `ExitStatus::code() == None`, which the first version of this reported as no
    /// status at all.
    #[cfg(unix)]
    #[test]
    fn a_child_the_kernel_killed_reports_its_signal_rather_than_nothing() {
        let exe = stub("kill -9 $$");
        let repo = crate::testutil::scratch("child-signal");
        let err = launch_stub(&exe, &repo, |_, _, _| {}).expect_err("a killed child launched");
        let said = format!("{err:#}");
        assert!(
            said.contains("signal 9"),
            "the signal was not named: {said}"
        );
    }

    /// **A silent, living child must fail the launch, not park the thread.**
    ///
    /// The deadline used to be checked inside the read loop, so it could only fire
    /// after a line arrived — which meant a child that printed nothing and stayed
    /// alive was waited on for ever, and `READY_TIMEOUT` was unreachable code that
    /// read as a guarantee. Driven through `read_ready` with a short patience
    /// rather than through `launch_at`, because the real one is 60 seconds.
    ///
    /// **Answered over a channel, not asserted in place**, because the regression
    /// this guards against does not return a wrong value — it never returns. A
    /// test that called `read_ready` directly would hang the whole binary and read
    /// as a CI timeout, which is the report nobody can act on. This one fails.
    #[test]
    fn a_child_that_stays_silent_and_alive_times_out_rather_than_hanging() {
        let (answer, answered) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // The sender is held for the whole call: dropping it would be EOF,
            // which is the *other* failure and the one that already worked.
            let (_tx, rx) = std::sync::mpsc::channel::<String>();
            let _ = answer.send(format!(
                "{}",
                read_ready(&rx, 0, Duration::from_millis(200))
                    .map(|r| r.port)
                    .expect_err("a silent child reported ready")
            ));
        });
        let said = answered
            .recv_timeout(Duration::from_secs(10))
            .expect("the read never honoured its deadline");
        assert!(
            said.contains("still running"),
            "the message does not tell a wedged daemon from a dead one: {said}"
        );
    }

    /// Prose before the ready line is still skipped, now that a deadline is running.
    #[test]
    fn the_deadline_does_not_eat_a_ready_line_that_arrives_after_chatter() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send("logging to somewhere".to_string()).unwrap();
        tx.send("ready 4242 tok -".to_string()).unwrap();
        let ready = read_ready(&rx, 0, Duration::from_secs(5)).expect("ready");
        assert_eq!(ready.port, 4242);
        assert_eq!(ready.repo, None);
    }

    /// **The sibling binary wins, and the PATH fallback is visible.**
    ///
    /// The 2026-09-14 half of #18 was `starting orchd for <checkout>: No such file
    /// or directory` — a bare `orchd`, which is this function's fallback, and the
    /// only sign that the sibling lookup had missed.
    #[test]
    fn the_daemon_binary_prefers_its_sibling_and_says_when_it_has_none() {
        let dir = crate::testutil::scratch("daemon-binary");
        let running = dir.join("orchestrator-desktop");
        std::fs::write(&running, "").unwrap();

        // No sibling yet: a PATH lookup, and the caller is told.
        let (exe, found) = daemon_binary_beside(Some(&running));
        assert!(!found);
        assert_eq!(exe, PathBuf::from("orchd"));

        std::fs::write(dir.join("orchd"), "").unwrap();
        let (exe, found) = daemon_binary_beside(Some(&running));
        assert!(found, "the sibling was there and was not used");
        assert_eq!(exe, dir.join("orchd"));

        // An executable path nobody could resolve is the fallback too, not a panic.
        assert_eq!(daemon_binary_beside(None), (PathBuf::from("orchd"), false));
    }
}
