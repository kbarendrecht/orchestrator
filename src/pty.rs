// Four libc calls live here: `killpg` and `getpgid`/`getpgrp` to stop a session's
// whole process group, and `kill(pid, 0)` to ask whether a pid is alive. The
// workspace denies `unsafe_code`; this is one of three modules that opt out, and
// every block below carries its own SAFETY note.
#![allow(unsafe_code)]
use anyhow::{Context, Result};
use bytes::Bytes;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, watch};

/// How much scrollback the daemon keeps per pty for replay on reattach.
const BUFFER_BYTES: usize = 512 * 1024;

/// Dropped chunks past this point mean a client is too slow; it resyncs from the
/// ring buffer instead of receiving a torn stream.
const BROADCAST_CHUNKS: usize = 1024;

/// Bounded byte buffer holding the tail of a pty's output.
///
/// The SPA is stateless and disposable (§1): closing the browser kills nothing
/// and reopening replays from here. Buffers are in-memory only and are not
/// persisted across a daemon restart — session *records* are (§2).
struct RingBuffer {
    buf: std::collections::VecDeque<u8>,
    cap: usize,
}

impl RingBuffer {
    fn new(cap: usize) -> Self {
        RingBuffer {
            buf: std::collections::VecDeque::with_capacity(cap.min(64 * 1024)),
            cap,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        // A single write larger than the whole buffer keeps only its tail.
        if bytes.len() >= self.cap {
            self.buf.clear();
            self.buf.extend(&bytes[bytes.len() - self.cap..]);
            return;
        }
        let overflow = (self.buf.len() + bytes.len()).saturating_sub(self.cap);
        self.buf.drain(..overflow);
        self.buf.extend(bytes);
    }

    fn snapshot(&self) -> Vec<u8> {
        // Two memcpys, not a byte at a time: this runs on every attach and resync.
        let (a, b) = self.buf.as_slices();
        let mut out = Vec::with_capacity(a.len() + b.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        out
    }

}

pub struct Spawned {
    pub handle: Arc<PtyHandle>,
    pub pid: Option<u32>,
}

/// Where a program name points, resolved the way a shell would.
///
/// A name containing a `/` is a path, and stays relative to the spawn cwd. A bare
/// name is looked up on the PATH the child will actually get — the daemon's, with
/// the caller's overrides and removals applied, since a session may be spawned
/// with a PATH of its own.
///
/// Only a regular file with an execute bit counts. Taking the first thing that
/// merely *exists* is the bug this exists to avoid, and it costs nothing to be
/// stricter than the crate we hand the answer to.
fn resolve_program(
    prog: &str,
    cwd: &Path,
    env: &[(String, String)],
    unset: &[&str],
) -> Result<std::path::PathBuf> {
    if prog.contains('/') {
        return Ok(cwd.join(prog));
    }
    // The *last* PATH, because that is the one the child gets: `config::session_env`
    // pushes a second PATH on top of the first and says "last wins", and
    // `CommandBuilder` applies them in order. Reading the first here resolved the
    // program against a PATH the child would never see.
    let path = env
        .iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
        .or_else(|| {
            if unset.contains(&"PATH") {
                None
            } else {
                std::env::var("PATH").ok()
            }
        })
        .unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(prog);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    anyhow::bail!("{prog} is not on PATH")
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// One item on a pty's input queue. See [`PtyHandle::write`] and
/// [`PtyHandle::pause`].
enum Input {
    Bytes(Vec<u8>),
    /// A gap the writer thread keeps *at the fd*, between the bytes before it and
    /// the bytes after.
    Pause(std::time::Duration),
    /// The child is gone; the writer thread may end.
    Close,
}

/// A hosted pty: the process, its scrollback, and a fan-out of live output.
///
/// Sessions and Processes are hosted identically (§2) — same ring buffer, same
/// reattach. What differs is the hook lifecycle and whether it earns a rail
/// entry, and neither of those lives here.
pub struct PtyHandle {
    /// Input queued for the child, drained by a dedicated writer thread. See
    /// [`PtyHandle::write`] for why it is a queue rather than the fd.
    input: tokio::sync::mpsc::UnboundedSender<Input>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    buffer: Arc<Mutex<RingBuffer>>,
    tx: broadcast::Sender<Bytes>,
    exit_rx: watch::Receiver<Option<i32>>,
    /// The geometry the child has been told, so [`PtyHandle::resize`] can tell a
    /// real change from a client re-stating what it already asked for.
    size: Mutex<(u16, u16)>,
    /// Kept so a stop can reach the child's whole **process group** rather than
    /// only its leader — see [`PtyHandle::kill`].
    pid: Option<u32>,
}

/// How long a child gets to honour `SIGHUP` before it is `SIGKILL`ed.
///
/// Two seconds, and the number is a judgement rather than a measurement. Long
/// enough for a Node process to run its `SIGHUP` handler and flush: Claude Code
/// appends to its transcript per *turn*, so there is little left to write at exit.
/// Short enough that a swap, a teardown or closing the app does not read as a
/// hang — every one of those waits on this before it can finish.
pub const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

impl PtyHandle {
    /// Spawn `command` in a pty rooted at `cwd`, with `env` layered on top of
    /// the daemon's own environment.
    ///
    /// **A `cwd` that is not a directory is refused here, loudly.** `portable-pty`
    /// treats it as absent instead: `CommandBuilder::as_command` filters the cwd on
    /// `is_dir()` and falls back to `$HOME`, so a session aimed at a worktree that
    /// has been removed does not fail — it starts in your home directory and runs
    /// there. That is worse than a crash, because the agent is then working in a
    /// tree nobody chose. Seen for real: a fix-pr run on a stale workspace record
    /// opened in `$HOME`, and only Claude Code's own workspace-trust prompt stopped
    /// it, which is not a guarantee the daemon may rely on.
    pub fn spawn(
        command: &[String],
        cwd: &Path,
        env: &[(String, String)],
        unset: &[&str],
        size: (u16, u16),
    ) -> Result<Spawned> {
        if !cwd.is_dir() {
            anyhow::bail!(
                "{} is not a directory, so there is nowhere to start {}",
                cwd.display(),
                command.first().map(String::as_str).unwrap_or("the command")
            );
        }
        let (rows, cols) = size;
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening a pty")?;

        let (prog, args) = command
            .split_first()
            .context("empty command — nothing to spawn")?;
        // A bare name is a PATH lookup and nothing else. portable-pty tries the
        // *cwd* first and takes whatever exists there, a directory included, so a
        // checkout holding a `docker/` folder made `docker compose up` exec the
        // folder. The failure is unreadable rather than loud: portable-pty's
        // `close_random_fds` has already closed the pipe std reports an exec error
        // on, so the child aborts with `fatal runtime error: assertion failed:
        // output.write(&bytes).is_ok()` and never names the command. Resolving here
        // hands it an absolute path, which it only checks for X_OK.
        let resolved = resolve_program(prog, cwd, env, unset)?;
        let mut cmd = CommandBuilder::new(&resolved);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.cwd(cwd);
        // Removals first, so an explicit value below always wins.
        for k in unset {
            cmd.env_remove(k);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        // Without this many programs fall back to a dumb terminal and stop
        // emitting the escape sequences xterm.js exists to render.
        if !env.iter().any(|(k, _)| k == "TERM") {
            cmd.env("TERM", "xterm-256color");
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawning {prog} in {}", cwd.display()))?;
        // **This pid is also the child's process group id**, which is what lets
        // every stop below signal the group without asking the kernel who leads
        // it: `portable-pty` calls `setsid()` in the child before exec, and a
        // session leader's group id is its own pid by definition. Worth knowing
        // here rather than only at the `killpg` call, because it is a property of
        // how the child was spawned, not of how it is stopped.
        let pid = child.process_id();
        let killer = child.clone_killer();

        // Dropping the slave lets the master see EOF once the child exits;
        // holding it open would hang the reader loop forever.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().context("cloning pty reader")?;
        let writer = pair.master.take_writer().context("taking pty writer")?;

        let buffer = Arc::new(Mutex::new(RingBuffer::new(BUFFER_BYTES)));
        let (tx, _) = broadcast::channel(BROADCAST_CHUNKS);
        let (exit_tx, exit_rx) = watch::channel(None);

        // The child's input goes through a queue and a thread of its own, because
        // writing to a pty blocks when the child is not reading — see
        // [`PtyHandle::write`].
        let (input, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
        // The wait thread's way of ending the writer once the child is gone; see
        // below. Without it every exited session kept a parked thread and the
        // master fd for as long as its row existed.
        let closer = input.clone();
        std::thread::spawn(move || {
            let mut writer = writer;
            while let Some(item) = input_rx.blocking_recv() {
                match item {
                    Input::Bytes(data) => {
                        if let Err(e) = writer.write_all(&data).and_then(|()| writer.flush()) {
                            // Ordinary at the end of a session: the child has gone
                            // and the fd is closed. Logged rather than dropped, which
                            // is what the call sites did — every one of them
                            // discards the result.
                            tracing::debug!("pty write failed, giving up on this pty's input: {e}");
                            break;
                        }
                    }
                    Input::Pause(gap) => std::thread::sleep(gap),
                    Input::Close => break,
                }
            }
        });

        let handle = Arc::new(PtyHandle {
            input,
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
            buffer: buffer.clone(),
            tx: tx.clone(),
            exit_rx,
            size: Mutex::new((rows, cols)),
            pid,
        });

        // The pty reader is blocking, so it gets a dedicated blocking thread
        // rather than starving the async runtime.
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let bytes = Bytes::copy_from_slice(&chunk[..n]);
                        if let Ok(mut b) = buffer.lock() {
                            b.push(&bytes);
                        }
                        // No receivers is the normal case: nobody has the tab open.
                        let _ = tx.send(bytes);
                    }
                }
            }
        });

        std::thread::spawn(move || {
            let code = child.wait().ok().map(|s| s.exit_code() as i32);
            let _ = exit_tx.send(Some(code.unwrap_or(-1)));
            // Nothing will read the fd again, so the writer can go. `Session::pty`
            // is kept after the exit on purpose — it is what replays scrollback —
            // so the sender lives on, and this is the only thing that ends the
            // thread. A `write` after this fails, which is the documented answer.
            let _ = closer.send(Input::Close);
        });

        Ok(Spawned { handle, pid })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.tx.subscribe()
    }

    /// Everything the daemon still holds of this pty's output.
    pub fn snapshot(&self) -> Vec<u8> {
        self.buffer
            .lock()
            .map(|b| b.snapshot())
            .unwrap_or_default()
    }

    /// Queue `data` for the child. Never blocks.
    ///
    /// **Writing to a pty blocks when the child is not reading it.** The kernel
    /// buffer is small — a few kilobytes — so a bracketed paste, or a `/resolve`
    /// prompt handed to a session still busy starting up, filled it and parked the
    /// caller until the child drained. That caller was a tokio worker (the pty
    /// websocket's read loop calls this on every keystroke), and a parked worker is
    /// one fewer serving the board.
    ///
    /// So the fd belongs to a thread of its own, and this hands bytes to it. One
    /// queue and one consumer, so ordering is preserved — which matters, since
    /// these are keystrokes.
    ///
    /// The cost, stated because it is a real change in meaning: `Ok` now means
    /// "queued", not "written". Nothing is lost by it — every call site already
    /// discarded the result — and a failed write is now logged by the thread
    /// instead of vanishing. Only a dead writer thread is an error here, and it
    /// dies with the child: a write to an exited session is refused, not queued
    /// for nobody.
    ///
    /// One consequence for callers that used to sleep *between* two writes to
    /// keep them apart at the fd: that no longer works, because the first write
    /// may still be going out when the sleep ends. [`Self::pause`] is the gap.
    pub fn write(&self, data: &[u8]) -> Result<()> {
        self.input
            .send(Input::Bytes(data.to_vec()))
            .map_err(|_| anyhow::anyhow!("this pty's writer has gone"))
    }

    /// Queue a gap: the writer thread waits `gap` after everything queued before
    /// this has reached the fd, before it writes anything queued after.
    ///
    /// This is what "type the prompt, then a beat, then the return" needs. A
    /// prompt and its return arriving in one read is a *paste* to Claude Code, and
    /// a pasted newline is a line break rather than a send — so a `/resolve`
    /// prompt longer than the pty buffer sat typed and never submitted, because
    /// the caller's sleep began when the prompt was queued, not when it was
    /// written. Only the thread that owns the fd can keep that gap.
    pub fn pause(&self, gap: std::time::Duration) -> Result<()> {
        self.input
            .send(Input::Pause(gap))
            .map_err(|_| anyhow::anyhow!("this pty's writer has gone"))
    }

    /// Type `text` into an agent's prompt box and send it, keeping the two apart.
    ///
    /// **The return goes in its own write, after a gap held at the fd.** A line of
    /// text and its newline arriving in one read is a *paste* to Claude Code, and a
    /// pasted newline is a line break in the prompt box rather than a send — so the
    /// instructions sat there typed and never submitted. Short slash commands got
    /// away with it; a sentence with a path in it did not.
    ///
    /// The gap is [`Self::pause`] and not a `sleep` in the caller, because
    /// [`Self::write`] only *queues*: a prompt longer than the pty buffer was still
    /// going out when the caller's sleep ended, and the return then landed in the
    /// same read as its tail, which is the paste again.
    ///
    /// `gap` is the caller's, because the two callers measured different numbers:
    /// 300ms after `SessionStart`, and 500ms for a nudge, where the shorter gap
    /// left one session in four holding text it never sent.
    pub fn type_and_send(&self, text: &[u8], gap: std::time::Duration) {
        let _ = self.write(text);
        let _ = self.pause(gap);
        let _ = self.write(b"\r");
    }

    /// Tell the child it has a new window size.
    ///
    /// **The same size twice is not a resize, and saying so costs something.**
    /// `MasterPty::resize` is a `TIOCSWINSZ`, which raises `SIGWINCH` whatever the
    /// numbers are, and a full-screen TUI answers that by repainting itself. Every
    /// attached client states its geometry, and a client must be free to re-state
    /// it — that is how a pane recovers a size some *other* client changed — so
    /// without this the re-assertion flickers the pane it is trying to fix.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let mut size = self
            .size
            .lock()
            .map_err(|_| anyhow::anyhow!("pty size lock poisoned"))?;
        if *size == (rows, cols) {
            return Ok(());
        }
        let m = self
            .master
            .lock()
            .map_err(|_| anyhow::anyhow!("pty master lock poisoned"))?;
        m.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        *size = (rows, cols);
        Ok(())
    }

    /// Signal the child's whole process group, without waiting.
    ///
    /// **`portable-pty`'s own killer is one `SIGHUP` to the leader and nothing
    /// else** — `ProcessSignaller::kill` on unix is a bare `libc::kill(pid,
    /// SIGHUP)`; the escalate-to-`SIGKILL` loop exists only on the original
    /// `Child`, which the wait thread owns. That is not enough twice over: a
    /// shell's own children never see it, and Node — so Claude Code — installs a
    /// `SIGHUP` handler and is entitled to decline. The group is the right unit
    /// because `portable-pty` `setsid()`s the child, so the group is exactly this
    /// session's processes and nothing else.
    ///
    /// Best effort and non-blocking, which is what the shutdown path wants: it
    /// signals every session under the write lock and cannot wait on each.
    /// [`Self::kill_gracefully`] is the version that escalates.
    pub fn kill(&self) -> Result<()> {
        // A reaped leader's pid is a free number, and the kernel may have handed it
        // to somebody else's process group by now. Callers reach here without
        // asking `is_alive` first, so the check is made once, here.
        if self.exit_code().is_some() {
            return Ok(());
        }
        if self.signal_group(libc::SIGHUP) {
            return Ok(());
        }
        // No pid, or a group we must not signal: fall back to what portable-pty
        // would have done, which at least reaches the leader.
        let mut k = self
            .killer
            .lock()
            .map_err(|_| anyhow::anyhow!("pty killer lock poisoned"))?;
        k.kill()?;
        Ok(())
    }

    /// `killpg` the child's group, or `false` when there is no group safe to
    /// signal and the caller should fall back to the leader.
    ///
    /// The guard is the point. `killpg(0, …)` means *our own* process group, which
    /// would take the daemon and every other session with it — the same hazard
    /// [`pid_alive`] documents for `kill`. So a pid of 0 is refused, and so is a
    /// group that turns out to be the daemon's own.
    ///
    /// **The group id is the leader's pid, not looked up.** `portable-pty` calls
    /// `setsid()` in the child before exec, which makes the leader's pid the group
    /// id by definition. Asking `getpgid` instead broke the moment the leader was
    /// reaped: the lookup fails with `ESRCH` while the group still exists — a
    /// grandchild that ignored `SIGHUP`, in a session whose shell did not — so the
    /// escalation refused to signal exactly the processes it existed to reach.
    /// POSIX keeps a group id reserved while any member lives, so `killpg(pid)`
    /// stays correct after the leader is gone. Not after the *whole group* is
    /// gone, which is why [`Self::kill`] and [`Self::kill_hard`] check the exit
    /// first and only [`Self::kill_gracefully`] sweeps.
    fn signal_group(&self, sig: libc::c_int) -> bool {
        let Some(pid) = self.pid.filter(|p| *p != 0) else {
            return false;
        };
        let pgid = pid as libc::pid_t;
        // SAFETY: read-only, and cannot fail.
        if pgid == unsafe { libc::getpgrp() } {
            return false;
        }
        // SAFETY: a group `setsid()` gave this child, proven above not to be ours.
        unsafe { libc::killpg(pgid, sig) == 0 }
    }

    /// `SIGKILL` the child's group, without waiting.
    ///
    /// The escalation half on its own, and [`Self::kill_gracefully`] is the only
    /// caller: it is what happens once the grace has run out. Shutdown does *not*
    /// use it — `lib.rs` says why it cannot escalate yet — so do not read this as
    /// a second stop path. It stays separate because the two halves of
    /// "ask, then insist" are easier to read named than inlined.
    fn kill_hard(&self) {
        // Same reason as in [`Self::kill`]: a reaped pid may be somebody else's.
        if self.exit_code().is_some() {
            return;
        }
        if !self.signal_group(libc::SIGKILL) {
            if let Ok(mut k) = self.killer.lock() {
                let _ = k.kill();
            }
        }
    }

    /// Stop the child and wait for it: `SIGHUP` the group, allow [`KILL_GRACE`],
    /// then `SIGKILL` the group.
    ///
    /// Use this wherever the caller depends on the process actually being gone —
    /// a swap moving a session's branch, a managed process being replaced, a
    /// session the UI has just dropped. `kill(); wait().await` was the shape
    /// before, and with a child that ignores `SIGHUP` it waits forever: a swap
    /// hung its own HTTP request, and a dropped session left an agent running with
    /// no record, no watcher and nothing in the UI that could reach it.
    ///
    /// Returns the exit code, or `None` if the child outlived even the `SIGKILL` —
    /// which should not happen, and is worth logging where it does.
    pub async fn kill_gracefully(&self) -> Option<i32> {
        if let Some(code) = self.exit_code() {
            return Some(code); // already gone
        }
        let _ = self.kill();
        let code = match tokio::time::timeout(KILL_GRACE, self.wait()).await {
            Ok(code) => Some(code),
            Err(_) => {
                tracing::warn!(
                    pid = ?self.pid,
                    "the child did not go on SIGHUP within {KILL_GRACE:?} — killing its group"
                );
                self.kill_hard();
                // A SIGKILLed group cannot trap, so this is the reaper catching up
                // rather than another wait on the child's goodwill. Bounded all the
                // same.
                tokio::time::timeout(KILL_GRACE, self.wait()).await.ok()
            }
        };
        // `wait()` resolves when the *leader* is reaped, not when the group is
        // empty. A shell goes on `SIGHUP` in under a millisecond and leaves behind
        // any child that ignored it — and once the leader is gone `kill_hard`
        // refuses, by design, so nothing could reach them: a session forgotten or
        // relocated with a live grandchild sitting in its worktree. The caller of
        // this function wants the session *gone*, so the group is swept once more.
        //
        // **This is the one place that signals the group after the leader was
        // reaped, and the exit-code guard `kill`/`kill_hard` carry is exactly what
        // it must not have.** Their guard is against a *recycled* pid: a caller
        // holding a long-dead handle would otherwise signal whatever now owns that
        // number. Here the wait resolved a moment ago, within this call, so the
        // group id cannot have been reused yet — and POSIX keeps it reserved for
        // as long as any member is alive, which is precisely the case being swept.
        self.signal_group(libc::SIGKILL);
        code
    }

    pub fn exit_code(&self) -> Option<i32> {
        *self.exit_rx.borrow()
    }

    pub fn is_alive(&self) -> bool {
        self.exit_code().is_none()
    }

    /// Resolves once the child exits. Used to drive state transitions off a
    /// process dying rather than polling for it.
    pub async fn wait(&self) -> i32 {
        let mut rx = self.exit_rx.clone();
        loop {
            if let Some(code) = *rx.borrow() {
                return code;
            }
            if rx.changed().await.is_err() {
                return -1;
            }
        }
    }
}

/// Whether a pid is still in the process table.
///
/// Teardown's "no live session" preflight asks the kernel rather than the
/// daemon's in-memory state (§8b) — a crashed daemon is exactly the moment a
/// stale in-memory answer would let you delete a worktree with a live agent in
/// it.
///
/// `kill(pid, 0)` sends no signal and only reports whether the pid could be
/// signalled, which is the POSIX way to ask this and works on macOS as well as
/// Linux. It replaced a `/proc/<pid>` stat that was silently **always false**
/// off Linux: this guard fails *open*, so on macOS every session read as dead
/// and the check it exists to perform was not being performed at all.
///
/// `EPERM` counts as alive. A pid we are not allowed to signal is still a
/// running process, and treating "permission denied" as "gone" would put the
/// hole straight back.
pub fn pid_alive(pid: u32) -> bool {
    // pid 0 means "every process in our group" to `kill`, which would be a wildly
    // different question — and no session ever has it.
    if pid == 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs error checking only; it touches no
    // memory and delivers nothing.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_ring_keeps_the_tail_when_it_overflows() {
        let mut r = RingBuffer::new(4);
        r.push(b"abc");
        r.push(b"de");
        assert_eq!(r.snapshot(), b"bcde");
    }

    #[test]
    fn a_write_larger_than_the_ring_keeps_only_its_tail() {
        let mut r = RingBuffer::new(3);
        r.push(b"abcdefg");
        assert_eq!(r.snapshot(), b"efg");
    }

    #[test]
    fn the_ring_stays_empty_until_written() {
        // Through `snapshot`, which is the only thing anything ever asks a ring:
        // two accessors existed for this one assertion and answered nothing it
        // could not.
        assert!(RingBuffer::new(8).snapshot().is_empty());
    }

    /// portable-pty answers a missing cwd with `$HOME` rather than an error
    /// (`CommandBuilder::as_command` filters it on `is_dir()`), so without this
    /// guard a session aimed at a removed worktree runs in the home directory and
    /// says nothing. The refusal is the whole point; the wording only has to name
    /// the path.
    #[test]
    fn a_cwd_that_is_not_there_is_refused_rather_than_swapped_for_home() {
        // `scratch` for the unique name, then removed: this is the one test that
        // needs a path which is *not* there, and the helper's whole job is to
        // create one.
        let gone = crate::testutil::scratch("gone");
        std::fs::remove_dir_all(&gone).unwrap();
        let err = match PtyHandle::spawn(&["cat".to_string()], &gone, &[], &[], (24, 80)) {
            Ok(_) => panic!("a missing directory must fail the spawn"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains(&gone.display().to_string()),
            "the refusal has to name the path: {err}"
        );
    }


    #[test]
    fn spawns_reads_and_reports_exit() {
        let spawned = PtyHandle::spawn(
            &["sh".to_string(), "-c".to_string(), "echo hello; exit 3".to_string()],
            Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");

        // Give the reader thread a moment to drain before the child is reaped.
        for _ in 0..100 {
            if spawned.handle.exit_code().is_some() && !spawned.handle.snapshot().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let out = String::from_utf8_lossy(&spawned.handle.snapshot()).to_string();
        assert!(out.contains("hello"), "expected output, got {out:?}");
        assert_eq!(spawned.handle.exit_code(), Some(3));
        assert!(spawned.pid.is_some());
    }

    /// A checkout that holds a `docker/` directory made `docker compose up` exec
    /// the *directory*: portable-pty resolves a bare name against the cwd first
    /// and accepts anything that exists. The failure never names the command —
    /// its `close_random_fds` has closed the pipe std reports an exec error on, so
    /// the child aborts with `fatal runtime error: assertion failed:
    /// output.write(&bytes).is_ok()` and that is the whole of what the pane shows.
    #[test]
    fn a_directory_in_the_cwd_does_not_shadow_a_program_on_the_path() {
        let dir = crate::testutil::scratch("shadow");
        std::fs::create_dir_all(dir.join("echo")).expect("the shadowing directory");

        let spawned = PtyHandle::spawn(
            &["echo".to_string(), "hi".to_string()],
            &dir,
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");

        for _ in 0..100 {
            if spawned.handle.exit_code().is_some() && !spawned.handle.snapshot().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = String::from_utf8_lossy(&spawned.handle.snapshot()).to_string();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(out.contains("hi"), "expected /bin/echo to have run, got {out:?}");
    }

    /// Two PATH entries in `env` is the normal shape (`session_env` layers one on
    /// the daemon's), and the child sees the last. Resolving against the first
    /// found a program the child could not.
    #[test]
    fn the_last_path_entry_is_the_one_resolved_against() {
        let dir = crate::testutil::scratch("lastpath");
        let (first, last) = (dir.join("first"), dir.join("last"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&last).unwrap();
        for d in [&first, &last] {
            let p = d.join("prog");
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = vec![
            ("PATH".to_string(), first.to_string_lossy().into_owned()),
            ("PATH".to_string(), last.to_string_lossy().into_owned()),
        ];
        let found = resolve_program("prog", &dir, &env, &[]).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found, last.join("prog"));
    }

    /// The other half: a name with a `/` is a path, and still means the cwd.
    #[test]
    fn a_path_stays_relative_to_the_cwd() {
        let dir = crate::testutil::scratch("relpath");
        std::fs::create_dir_all(&dir).expect("the temp dir");
        let script = dir.join("say.sh");
        std::fs::write(&script, "#!/bin/sh
echo from-the-cwd
").expect("the script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        let spawned =
            PtyHandle::spawn(&["./say.sh".to_string()], &dir, &[], &[], (24, 80)).expect("spawn");
        for _ in 0..100 {
            if spawned.handle.exit_code().is_some() && !spawned.handle.snapshot().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = String::from_utf8_lossy(&spawned.handle.snapshot()).to_string();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(out.contains("from-the-cwd"), "got {out:?}");
    }

    /// **The whole reason `kill` is not `portable-pty`'s `kill`.** Its signaller
    /// sends one `SIGHUP` to the leader, and a child is free to trap it — Node
    /// does, so Claude Code does. Before this, such a child survived the kill and
    /// `wait()` after it never returned: a swap hung its own request, and a deleted
    /// session left a live agent with no record.
    ///
    /// The child here traps `SIGHUP` and reports it, so a pass proves the
    /// escalation ran rather than that the shell happened to exit.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_child_that_traps_sighup_is_still_killed() {
        let spawned = PtyHandle::spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                // Trap and keep going. `wait` on a sleep so the shell is not in an
                // uninterruptible read, which would mask the point.
                "trap 'echo trapped' HUP; while :; do sleep 0.1; done".to_string(),
            ],
            Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");

        // Let the trap be installed before signalling, or the default action
        // applies and the test proves nothing.
        for _ in 0..100 {
            if !spawned.handle.snapshot().is_empty() || spawned.handle.exit_code().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(spawned.handle.is_alive(), "the child should still be running");

        let started = std::time::Instant::now();
        let code = spawned.handle.kill_gracefully().await;
        assert!(code.is_some(), "the child outlived even the SIGKILL");
        assert!(!spawned.handle.is_alive(), "still alive after kill_gracefully");
        // It had to wait out the grace, which is what says the trap really fired
        // and the SIGKILL is what ended it.
        assert!(
            started.elapsed() >= KILL_GRACE,
            "returned before the grace elapsed, so SIGHUP was not trapped"
        );
        let out = String::from_utf8_lossy(&spawned.handle.snapshot()).to_string();
        assert!(out.contains("trapped"), "the trap never ran, got {out:?}");
    }

    /// The other half of signalling the *group*: a shell's own children.
    ///
    /// `portable-pty`'s killer signals the leader only, so a session's grandchildren
    /// — the agent's subprocesses, a watcher a shell started — kept running after
    /// the session was gone. The child here backgrounds a sleep and reports its pid,
    /// and a pass is that pid being gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn killing_a_session_takes_its_grandchildren() {
        let dir = crate::testutil::scratch("group");
        std::fs::create_dir_all(&dir).expect("the temp dir");
        let pidfile = dir.join("grandchild.pid");

        let spawned = PtyHandle::spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(
                    "sleep 300 & echo $! > {}; while :; do sleep 0.1; done",
                    pidfile.display()
                ),
            ],
            &dir,
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");

        // Wait for the grandchild's pid to be written.
        let mut grandchild = None;
        for _ in 0..150 {
            if let Ok(raw) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = raw.trim().parse::<u32>() {
                    grandchild = Some(pid);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let grandchild = grandchild.expect("the shell never reported its background child");
        assert!(pid_alive(grandchild), "the grandchild should be running");

        spawned.handle.kill_gracefully().await;

        // The signal is delivered to the group; reaping is the kernel's own pace.
        let mut gone = false;
        for _ in 0..150 {
            if !pid_alive(grandchild) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "the grandchild outlived its session (pid {grandchild})");
    }

    /// The case the group lookup got wrong: the shell goes on `SIGHUP` at once,
    /// its child ignores it. `wait()` then resolved on the leader and `getpgid` on
    /// the reaped pid failed, so the grandchild was never signalled. The sleep here
    /// inherits the ignored disposition across exec, which is what makes it survive
    /// the first signal and only the sweep after the exit can end it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_grandchild_that_ignores_sighup_is_swept_after_the_leader_exits() {
        let dir = crate::testutil::scratch("group-hup");
        std::fs::create_dir_all(&dir).expect("the temp dir");
        let pidfile = dir.join("grandchild.pid");

        let spawned = PtyHandle::spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!(
                    "trap '' HUP; sleep 300 & echo $! > {}; trap - HUP; while :; do sleep 0.1; done",
                    pidfile.display()
                ),
            ],
            &dir,
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");

        let mut grandchild = None;
        for _ in 0..150 {
            if let Ok(raw) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = raw.trim().parse::<u32>() {
                    grandchild = Some(pid);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let grandchild = grandchild.expect("the shell never reported its background child");
        assert!(pid_alive(grandchild), "the grandchild should be running");

        let started = std::time::Instant::now();
        let code = spawned.handle.kill_gracefully().await;
        assert!(code.is_some(), "the leader itself did not exit");
        // The leader went on the first signal, so no grace was spent: the sweep is
        // what has to reach the grandchild, not the escalation.
        assert!(started.elapsed() < KILL_GRACE, "the shell should have gone on SIGHUP");

        let mut gone = false;
        for _ in 0..150 {
            if !pid_alive(grandchild) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "the HUP-ignoring grandchild outlived its session (pid {grandchild})");
    }

    /// A pause holds the bytes after it back until the gap has passed *at the fd*.
    /// `cat` echoes what it gets, so the scrollback says what has arrived.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pause_keeps_the_writes_around_it_apart() {
        let spawned = PtyHandle::spawn(
            &["/bin/cat".to_string()],
            Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");
        spawned.handle.write(b"first").unwrap();
        spawned.handle.pause(Duration::from_millis(600)).unwrap();
        spawned.handle.write(b"second").unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;
        let early = String::from_utf8_lossy(&spawned.handle.snapshot()).to_string();
        assert!(early.contains("first"), "the bytes before the pause went out: {early:?}");
        assert!(!early.contains("second"), "the bytes after the pause were held back: {early:?}");

        tokio::time::sleep(Duration::from_millis(700)).await;
        let late = String::from_utf8_lossy(&spawned.handle.snapshot()).to_string();
        assert!(late.contains("second"), "the pause ended and the rest went out: {late:?}");
        let _ = spawned.handle.kill();
    }

    /// The writer thread ends with the child. It used to end only when the sender
    /// dropped, which for a session is never — the record keeps the handle for its
    /// scrollback — so every exited session pinned a thread and the master fd.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_writer_goes_when_the_child_does() {
        let spawned = PtyHandle::spawn(
            &["/bin/true".to_string()],
            Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");
        spawned.handle.wait().await;
        // The close rides the queue behind whatever was there, so give it a moment.
        let mut refused = false;
        for _ in 0..100 {
            if spawned.handle.write(b"x").is_err() {
                refused = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(refused, "a write to an exited child was still being queued");
    }

    /// Signalling a handle whose child is already reaped is a no-op, not a signal
    /// to whatever now holds that pid. Both non-waiting killers are reached by
    /// callers that never asked `is_alive`.
    #[tokio::test(flavor = "multi_thread")]
    async fn killing_an_exited_child_does_nothing() {
        let spawned = PtyHandle::spawn(
            &["/bin/true".to_string()],
            Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");
        spawned.handle.wait().await;
        assert!(spawned.handle.kill().is_ok());
        spawned.handle.kill_hard();
        assert_eq!(spawned.handle.kill_gracefully().await, Some(0));
    }

    /// A child that goes on `SIGHUP` is not made to wait out the grace.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_well_behaved_child_goes_immediately() {
        let spawned = PtyHandle::spawn(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "while :; do sleep 0.1; done".to_string(),
            ],
            Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");
        tokio::time::sleep(Duration::from_millis(100)).await;

        let started = std::time::Instant::now();
        assert!(spawned.handle.kill_gracefully().await.is_some());
        assert!(
            started.elapsed() < KILL_GRACE,
            "an ordinary child should not wait out the grace"
        );
    }

    /// Writing to a pty blocks once the kernel buffer fills and the child is not
    /// reading, and this used to happen on the caller's thread — a tokio worker,
    /// since the websocket read loop writes every keystroke. A child that never
    /// reads is the worst case, so that is what this uses: the write has to return
    /// anyway, because the bytes only reach a queue.
    #[test]
    fn writing_to_a_child_that_never_reads_does_not_block() {
        // `sleep` reads nothing at all, so its buffer fills and stays full.
        let spawned = PtyHandle::spawn(
            &["/bin/sleep".to_string(), "30".to_string()],
            Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");

        // Comfortably more than a pty's buffer, which is a few KB.
        let big = vec![b'x'; 512 * 1024];
        let started = std::time::Instant::now();
        spawned.handle.write(&big).expect("queued");
        spawned.handle.write(b"and another").expect("queued");
        let elapsed = started.elapsed();

        let _ = spawned.handle.kill();
        assert!(
            elapsed < Duration::from_millis(250),
            "the write blocked for {elapsed:?} — it is back on the caller's thread"
        );
    }

    /// Keystrokes, so order is the whole point: one queue and one consumer.
    #[test]
    fn queued_input_reaches_the_child_in_order() {
        let spawned = PtyHandle::spawn(
            // `cat` echoes what it reads, so the pty's own echo is not the only
            // thing under test.
            &["/bin/cat".to_string()],
            Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");

        for part in ["alpha\n", "bravo\n", "charlie\n"] {
            spawned.handle.write(part.as_bytes()).expect("queued");
        }

        let mut out = String::new();
        for _ in 0..150 {
            out = String::from_utf8_lossy(&spawned.handle.snapshot()).to_string();
            if out.contains("charlie") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = spawned.handle.kill();

        let a = out.find("alpha").expect("alpha never arrived");
        let b = out.find("bravo").expect("bravo never arrived");
        let c = out.find("charlie").expect("charlie never arrived");
        assert!(a < b && b < c, "input arrived out of order: {out:?}");
    }

    /// A name that is nowhere on PATH is an error the caller can read, rather
    /// than a child that aborts with the crate's own assertion.
    #[test]
    fn a_program_that_is_not_on_the_path_says_so() {
        let spawned = PtyHandle::spawn(
            &["orchd-definitely-not-a-binary".to_string()],
            Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        );
        let err = match spawned {
            Ok(_) => panic!("spawned something that does not exist"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("not on PATH"), "unhelpful error: {err}");
    }

    #[test]
    fn reports_a_live_pid_as_alive() {
        let spawned = PtyHandle::spawn(
            &["sh".to_string(), "-c".to_string(), "sleep 5".to_string()],
            Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");
        let pid = spawned.pid.expect("pid");
        assert!(pid_alive(pid));
        spawned.handle.kill().expect("kill");
    }

    /// The guard fails *open*, so "says dead when it is not" is the dangerous
    /// direction: it lets teardown delete a worktree with a live agent in it.
    /// These are the answers that were wrong off Linux, where the `/proc` stat
    /// this replaced returned false for everything.
    #[test]
    fn answers_the_liveness_edges_the_proc_stat_got_wrong() {
        // Ourselves: alive, trivially, and the one pid a test can be sure of.
        assert!(pid_alive(std::process::id()), "our own pid must read alive");

        // pid 1 is init/launchd — always running, and never signallable by a
        // normal user. It is the EPERM case, which must read alive rather than
        // dead: "cannot signal" is not "not there".
        assert!(pid_alive(1), "pid 1 must read alive even when unsignallable");

        // 0 means "my process group" to `kill`, a different question entirely,
        // and no session ever carries it.
        assert!(!pid_alive(0), "pid 0 is not a session");

        // Reaped and gone. Spawn, kill, reap, then ask.
        let spawned = PtyHandle::spawn(
            &["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
            Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .expect("spawn");
        let pid = spawned.pid.expect("pid");
        spawned.handle.kill().expect("kill");
        // Wait for the child to be reaped: until it is, it lingers as a zombie
        // and `kill(pid, 0)` still succeeds, which would make this flaky.
        let mut gone = false;
        for _ in 0..200 {
            if spawned.handle.exit_code().is_some() && !pid_alive(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone, "a killed and reaped pid must read dead");
    }
}
