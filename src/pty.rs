use anyhow::{Context, Result};
use bytes::Bytes;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, watch};

use crate::ring::RingBuffer;

/// How much scrollback the daemon keeps per pty for replay on reattach.
const BUFFER_BYTES: usize = 512 * 1024;

/// Dropped chunks past this point mean a client is too slow; it resyncs from the
/// ring buffer instead of receiving a torn stream.
const BROADCAST_CHUNKS: usize = 1024;

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
    let path = env
        .iter()
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

/// A hosted pty: the process, its scrollback, and a fan-out of live output.
///
/// Sessions and Processes are hosted identically (§2) — same ring buffer, same
/// reattach. What differs is the hook lifecycle and whether it earns a rail
/// entry, and neither of those lives here.
pub struct PtyHandle {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    buffer: Arc<Mutex<RingBuffer>>,
    tx: broadcast::Sender<Bytes>,
    exit_rx: watch::Receiver<Option<i32>>,
}

impl PtyHandle {
    /// Spawn `command` in a pty rooted at `cwd`, with `env` layered on top of
    /// the daemon's own environment.
    pub fn spawn(
        command: &[String],
        cwd: &Path,
        env: &[(String, String)],
        unset: &[&str],
        size: (u16, u16),
    ) -> Result<Spawned> {
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

        let handle = Arc::new(PtyHandle {
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
            buffer: buffer.clone(),
            tx: tx.clone(),
            exit_rx,
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

    pub fn write(&self, data: &[u8]) -> Result<()> {
        let mut w = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("pty writer lock poisoned"))?;
        w.write_all(data)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
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
        Ok(())
    }

    pub fn kill(&self) -> Result<()> {
        let mut k = self
            .killer
            .lock()
            .map_err(|_| anyhow::anyhow!("pty killer lock poisoned"))?;
        k.kill()?;
        Ok(())
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
        let dir = std::env::temp_dir().join(format!("orchd-shadow-{}", std::process::id()));
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

    /// The other half: a name with a `/` is a path, and still means the cwd.
    #[test]
    fn a_path_stays_relative_to_the_cwd() {
        let dir = std::env::temp_dir().join(format!("orchd-relpath-{}", std::process::id()));
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
