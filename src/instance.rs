//! One daemon at a time.
//!
//! Two orchds are not two tools, they are one tool disagreeing with itself: both
//! spawn sessions into the same worktrees, both write `sessions.json`, both
//! rewrite the hook settings file with *their* port in it — so whichever wrote
//! last owns every hook, and the other one's rail goes quiet. The headless
//! binary half-caught this by refusing a busy port; the desktop app falls back
//! to an ephemeral one on purpose, and so started a second daemon happily.
//!
//! The lock is a pid file rather than a port, because the port is the wrong
//! question: a foreign process on 7777 is not another instance, and an instance
//! on a fallback port still is one.

// One libc call: `flock`, plus the `getpgid`/`getpgrp` guard around it. The
// workspace denies `unsafe_code`; this is one of three modules that opt out.
#![allow(unsafe_code)]
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use crate::config::Config;

/// Held for the life of the daemon.
///
/// **The lock lives on the open descriptor, not on the file existing.** The
/// kernel drops it when this process does — on a clean exit, on a panic, on
/// `std::process::exit` (which runs no destructors, and is how the desktop app
/// exits), and on a `SIGKILL`. Nothing has to clean up after a crash, which is
/// what the pid-file version got wrong.
///
/// The file itself is deliberately **left behind**, and must be: it is the thing
/// the lock is taken on, and removing it would let a second daemon create a fresh
/// file and lock *that* while this one still holds the old inode.
#[derive(Debug)]
pub struct Lock {
    #[allow(dead_code)] // Kept for the message and for tests; the fd is the lock.
    path: PathBuf,
    /// Held open for the life of the daemon. Closing it releases the lock, so
    /// this field is load-bearing even though nothing reads it.
    _file: std::fs::File,
}

/// Take the lock, or say who has it.
///
/// `flock(LOCK_EX | LOCK_NB)` is the whole mutex, and it is the kernel's: the
/// claim is one syscall with no read-then-write to race in, and it is released by
/// the process ending however it ends.
///
/// **What this replaced, because the failure was subtle.** The lock used to be a
/// pid file taken with `create_new`, plus a stale-file path: read the pid, ask
/// `ps` whether it looks like an orchd, `remove_file` if not, then try again. That
/// stale path was not the exception but the *common* one — `process::exit` leaves
/// the file behind, so every launch went through it — and two launches could
/// interleave inside it: both read a dead pid, both unlink, both `create_new`, and
/// the second unlinked the first's file. Two daemons, which is precisely what this
/// module exists to prevent. It also guessed from a command line, so a recycled
/// pid belonging to `vim ~/orchestrator/x` read as a live holder and wedged the app
/// out of starting.
pub fn acquire() -> Result<Lock> {
    acquire_at(Config::config_dir()?.join("instance.pid"))
}

/// The real work, with the path injected so a test can point it at a temp dir
/// rather than the machine's one true `~/.config/orchd/instance.pid` — which a
/// real daemon might hold and which two tests cannot share.
fn acquire_at(path: PathBuf) -> Result<Lock> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    // Never `create_new` and never truncating: the file is expected to be there
    // from a previous run, and its contents are only a diagnostic.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening the instance lock at {}", path.display()))?;

    // `LOCK_NB` so this answers rather than waits. Three outcomes, and they must
    // not be conflated: taken, held by someone else, or a real error — reading the
    // last as "already running" would refuse to start for the wrong reason.
    let taken = loop {
        // SAFETY: a descriptor just opened here; `flock` touches only the kernel's
        // lock table for it.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break true;
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            // A signal arrived, which says nothing about the lock. Ask again.
            Some(libc::EINTR) => continue,
            // The only errno that means somebody else has it.
            Some(libc::EWOULDBLOCK) => break false,
            _ => {
                return Err(err)
                    .with_context(|| format!("locking {}", path.display()))
            }
        }
    };
    if !taken {
        // Whoever holds the lock wrote their pid below, so this needs no `ps` and
        // cannot mistake a recycled pid for a holder: if the lock is held, someone
        // is holding it, full stop.
        let who = std::fs::read_to_string(&path)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let who = if who.is_empty() {
            "another instance".to_string()
        } else {
            format!("pid {who}")
        };
        bail!(
            "Orchestrator is already running ({who}). \
             One instance at a time: a second one would spawn sessions into the \
             same worktrees and take over the hook settings."
        );
    }

    // Ours. Record the pid so the *next* attempt can name us; best effort, since
    // the lock is held either way and this is only for the message.
    let _ = file.set_len(0);
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();
    Ok(Lock { path, _file: file })
}

// No `Drop`: closing `_file` is what releases the lock, and the kernel does that
// for us however the process ends. Removing the file here would be actively wrong —
// see [`Lock`].


#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        crate::testutil::scratch(&format!("lock-{tag}")).join("instance.pid")
    }

    /// The invariant the module exists for, and the one the pid-file version
    /// could lose: while a lock is held, nobody else takes it.
    ///
    /// `flock` locks the *open file description*, not the process, so a second
    /// `open` of the same path in this same process is refused exactly as another
    /// daemon would be — which is what makes this testable at all.
    #[test]
    fn a_second_attempt_is_refused_while_the_first_is_held() {
        let path = scratch("held");
        let first = acquire_at(path.clone()).expect("the first lock");
        let err = acquire_at(path.clone())
            .expect_err("a second instance must be refused")
            .to_string();
        assert!(err.contains("already running"), "unhelpful: {err}");
        // It names the holder, which it reads from the file the holder wrote.
        assert!(
            err.contains(&std::process::id().to_string()),
            "the refusal should name the pid holding it: {err}"
        );
        drop(first);
    }

    /// **The case the old stale-clear path was for, and got wrong.** A file left
    /// behind by a dead process is not a holder: nothing holds its lock, so the
    /// next launch simply takes it. No `ps`, no unlink, no second pass.
    #[test]
    fn a_file_left_behind_by_a_dead_process_is_not_a_holder() {
        let path = scratch("stale");
        // Exactly what a previous run leaves: the file, with a pid in it. 999999
        // is either dead or something unrelated; neither may block the lock.
        std::fs::write(&path, "999999").unwrap();
        let lock = acquire_at(path.clone()).expect("a stale file must not block the lock");
        // And ours is recorded over it.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string()
        );
        drop(lock);
    }

    /// Releasing is the kernel's job, so dropping the guard frees it — and the
    /// file stays, because the file is what the lock is taken on.
    #[test]
    fn dropping_the_lock_frees_it_and_keeps_the_file() {
        let path = scratch("release");
        {
            let _lock = acquire_at(path.clone()).expect("lock");
            assert!(path.exists());
        }
        assert!(
            path.exists(),
            "the lock file must survive the guard — it is the lock's target"
        );

        /* **Retried, and the reason is worth knowing.** `fork` duplicates every
           descriptor, so any *other* thread in this process spawning a child while
           our lock fd is open hands that child a copy — and the copy holds the
           flock until its `exec` closes it (`CLOEXEC`, which Rust sets). The window
           is microseconds, but a 460-test suite forks constantly, and asserting
           "free immediately" failed about two runs in five. Measured, not guessed:
           the refusal came back `EWOULDBLOCK`, not `EINTR`.

           Nothing to fix in the daemon — it takes this lock once at startup and
           does not release and immediately retake it — so the test's assumption was
           the wrong half. What is actually being asserted is that the lock is
           released at all, which a bounded retry says just as well. */
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match acquire_at(path.clone()) {
                Ok(again) => {
                    drop(again);
                    return;
                }
                Err(e) if std::time::Instant::now() > deadline => {
                    panic!("the lock was never released: {e}")
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
    }
}

