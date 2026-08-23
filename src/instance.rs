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

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::PathBuf;

use crate::config::Config;

/// Held for the life of the daemon. The file goes when this drops, which covers
/// a clean shutdown; a crash leaves it behind and [`acquire`] clears it.
pub struct Lock {
    path: PathBuf,
}

/// Take the lock, or say who has it.
///
/// `create_new` is the whole mutex: the check and the claim are one syscall, so
/// two instances starting together cannot both pass it.
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

    // Two passes at most: the second is the one that runs after a stale file has
    // been cleared, and a third would mean someone is racing us for the corpse.
    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                write!(f, "{}", std::process::id())?;
                return Ok(Lock { path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match holder(&path) {
                    Some(pid) => bail!(
                        "Orchestrator is already running (pid {pid}). \
                         One instance at a time: a second one would spawn sessions into the \
                         same worktrees and take over the hook settings."
                    ),
                    // Nobody home. A crash, a reboot, or a pid that has since
                    // been recycled by something unrelated.
                    None => {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            Err(e) => {
                return Err(e).with_context(|| format!("taking the lock at {}", path.display()))
            }
        }
    }
    bail!("could not take the instance lock at {}", path.display())
}

/// The pid in the file, if it is still ours to respect.
///
/// Liveness alone is not enough — pids are recycled, and a stale file naming a
/// pid that now belongs to somebody's editor would wedge the app out of starting
/// with no way to tell why. So the command line has to look like us too.
fn holder(path: &PathBuf) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
    let cmdline = process_command(pid)?;
    (cmdline.contains("orchd") || cmdline.contains("orchestrator")).then_some(pid)
}

/// The command line of a running pid, or `None` if it is gone.
///
/// `ps` rather than `/proc/<pid>/cmdline`, which does not exist on macOS: there
/// the read failed, every lock file read as stale, and a second daemon would
/// start beside a running one — the precise thing the lock exists to stop. One
/// short-lived process per lock acquisition, which happens once at startup.
///
/// A non-zero exit (no such pid) and empty output are both `None`, so a dead or
/// unreadable pid is never mistaken for a holder.
fn process_command(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_file_naming_a_dead_pid_is_not_a_holder() {
        let d = std::env::temp_dir().join(format!("orchd-lock-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("dead.pid");
        // `ps -p 0` reports no such process on Linux, and kernel_task on macOS —
        // which is not an orchd either, so both answer None.
        std::fs::write(&p, "0").unwrap();
        assert_eq!(holder(&p), None);
    }

    /// The lookup itself, since the whole guard rests on it. It read nothing on
    /// macOS before (`/proc/<pid>/cmdline`), which made every lock file look
    /// stale and let a second daemon start beside a running one.
    #[test]
    fn a_live_pid_resolves_to_a_command_line() {
        let mine = process_command(std::process::id())
            .expect("our own pid must resolve to a command line");
        assert!(!mine.is_empty());
        // A pid that cannot exist resolves to nothing rather than to something
        // empty that `holder` might read as a name.
        assert_eq!(process_command(u32::MAX), None, "no such pid");
    }

    #[test]
    fn a_live_pid_that_is_not_us_is_not_a_holder_either() {
        // The test binary is alive but is not orchd, which is exactly the
        // recycled-pid case the cmdline check exists for.
        let d = std::env::temp_dir().join(format!("orchd-lock-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("foreign.pid");
        std::fs::write(&p, "1").unwrap();
        assert_eq!(holder(&p), None, "pid 1 is init, not an orchd");
    }

    #[test]
    fn the_file_goes_when_the_lock_drops() {
        // A temp path, not the real config dir: the old version acquired the
        // machine's one lock, so it raced a running daemon and could not survive
        // the suite running in parallel.
        let d = std::env::temp_dir()
            .join(format!("orchd-lock-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("instance.pid");
        let _ = std::fs::remove_file(&path);
        {
            let _lock = acquire_at(path.clone()).expect("lock");
            assert!(path.exists());
        }
        assert!(!path.exists(), "the lock file outlived the guard");
    }
}
