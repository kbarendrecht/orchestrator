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
        let mut cmd = CommandBuilder::new(prog);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.cwd(cwd);
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
/// Teardown's "no live session" preflight consults `/proc` rather than
/// in-memory state (§8b) — a crashed daemon is exactly the moment a stale
/// in-memory answer would let you delete a worktree with a live agent in it.
pub fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
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

    #[test]
    fn reports_a_live_pid_as_alive() {
        let spawned = PtyHandle::spawn(
            &["sh".to_string(), "-c".to_string(), "sleep 5".to_string()],
            Path::new("/tmp"),
            &[],
            (24, 80),
        )
        .expect("spawn");
        let pid = spawned.pid.expect("pid");
        assert!(pid_alive(pid));
        spawned.handle.kill().expect("kill");
    }
}
