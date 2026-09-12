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
//!
//! **It guards the checkout, and now it really does.** The invariant above is
//! stated about worktrees and a `sessions.json` — the *checkout's* — while the file
//! was `<config dir>/instance.pid`, one per app. That was the same thing while an
//! app managed one checkout. It stopped being the same thing the moment a host
//! could hold several, and the fix was not here: a child daemon's config dir is
//! `checkouts/<leaf>-<hash>` (see `orchd_serve::host::checkout_dir`), derived from the
//! checkout, so this path is now derived from the checkout too. Two daemons for one
//! checkout meet on one file; two daemons for two checkouts do not meet at all,
//! which is what makes several checkouts possible.
//! `tests/host_and_child.rs` asserts it against two real children, because the test
//! below drives `acquire_at` with a path it chose and so cannot see which path a
//! daemon computes.
//!
//! What it still does not cover: two hosts with different `ORCHD_CONFIG_DIR` values
//! pointed at one checkout. `mise run fixture` and `mise run e2e` are that shape and
//! are safe for another reason — each uses a throwaway clone, so the checkout
//! differs anyway.

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
    /// Never read, and kept anyway: it is what a `Debug` of this guard has to say
    /// to be worth printing, since the descriptor beside it prints as a number.
    ///
    /// `expect` rather than `allow`, per the rule the workspace lints follow — it
    /// fails the build the day something does read it, which is the day this
    /// attribute should go. The comment here used to claim the refusal message and
    /// the tests read it; both build their own path and neither ever did.
    #[expect(dead_code, reason = "carried for Debug; see above")]
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

/// How long a refusal waits before it believes itself.
///
/// **A dead daemon's lock can outlive the daemon**, and that is not a kernel
/// delay: `fork` duplicates every descriptor, so a child the daemon spawned holds
/// a copy of the lock fd from the `fork` until its own `exec` closes it
/// (`CLOEXEC`, which Rust sets). The copy holds the `flock` for that window, and
/// the window is whatever the scheduler gives it.
///
/// The daemon forks hardest exactly where this bites — `reconcile_all` runs four
/// git processes wide at boot — so a checkout's daemon killed during its own sweep
/// leaves the lock held for a few milliseconds after it has been reaped. The host
/// restarts a dead daemon **once**, so one such millisecond spends the only
/// recovery that checkout has, and it stays down. Measured on a 2-core CI runner:
/// the restart was refused 250ms into the run, naming the pid it had just reaped.
///
/// Two seconds costs nothing it should not: a real second instance holds the lock
/// for its whole life, so the refusal still arrives, with the same sentence.
const PATIENCE: std::time::Duration = std::time::Duration::from_secs(2);

/// The real work, with the path injected so a test can point it at a temp dir
/// rather than the machine's one true `~/.config/orchd/instance.pid` — which a
/// real daemon might hold and which two tests cannot share.
fn acquire_at(path: PathBuf) -> Result<Lock> {
    acquire_within(path, PATIENCE)
}

/// The same, with the patience injected so a test does not wait [`PATIENCE`] out.
fn acquire_within(path: PathBuf, patience: std::time::Duration) -> Result<Lock> {
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
    //
    // `LOCK_NB` rather than a blocking `flock` even though this now waits, because
    // the two waits are not the same wait: a blocking call gives up never, and a
    // daemon that hangs forever on a lock somebody else really holds is worse than
    // one that says so. The deadline is ours to enforce, so the syscall stays the
    // one that answers.
    let deadline = std::time::Instant::now() + patience;
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
            // Somebody has it. Whether that is an instance or a corpse's forked
            // descriptor is not answerable from here — only waiting tells them
            // apart, and [`PATIENCE`] is how long that is worth.
            Some(libc::EWOULDBLOCK) => {
                if std::time::Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }
            _ => return Err(err).with_context(|| format!("locking {}", path.display())),
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
    ///
    /// Driven through `acquire_within` with no patience at all, because the wait
    /// [`PATIENCE`] buys is for a *corpse's* descriptor and this holder is alive:
    /// waiting two seconds here would only make the suite two seconds slower and
    /// assert nothing the first `flock` has not already answered.
    #[test]
    fn a_second_attempt_is_refused_while_the_first_is_held() {
        let path = scratch("held");
        let first =
            acquire_within(path.clone(), std::time::Duration::ZERO).expect("the first lock");
        let err = acquire_within(path.clone(), std::time::Duration::ZERO)
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
        let lock = acquire_within(path.clone(), std::time::Duration::ZERO)
            .expect("a stale file must not block the lock");
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

        /* **Retaken rather than probed once, and the reason is the whole of
        [`PATIENCE`].** `fork` duplicates every descriptor, so any *other* thread in
        this process spawning a child while our lock fd is open hands that child a
        copy — and the copy holds the flock until its `exec` closes it (`CLOEXEC`,
        which Rust sets). The window is microseconds, but a 575-test suite forks
        constantly, and asserting "free immediately" failed about two runs in five.
        Measured, not guessed: the refusal came back `EWOULDBLOCK`, not `EINTR`.

        That used to be the test's own bounded retry, on the reasoning that nothing
        in the daemon releases and immediately retakes this lock. The host does:
        it restarts a dead checkout's daemon the moment it reaps it. So the retry
        moved into `acquire_at`, and this asserts it from the outside. */
        drop(acquire_at(path).expect("the lock was never released"));
    }

    /// A holder that lets go inside [`PATIENCE`] is waited out, not refused.
    ///
    /// This is the host's restart in miniature: the corpse's descriptor goes away
    /// a moment after the daemon does, and the replacement must get the lock
    /// rather than spend the checkout's one free recovery on a few milliseconds.
    #[test]
    fn a_lock_let_go_during_the_wait_is_taken() {
        let path = scratch("patience");
        let held = acquire_at(path.clone()).expect("the first lock");
        let releasing = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(held);
        });
        let took = acquire_within(path, std::time::Duration::from_secs(2));
        releasing.join().unwrap();
        drop(took.expect("a lock released during the wait must be taken, not refused"));
    }
}
