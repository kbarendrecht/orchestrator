//! Is there room to start another agent?
//!
//! A count cap was the obvious answer and it was dropped for good reason: eight
//! sessions on a 64GB workstation is a limit that means nothing, and three on a
//! laptop already hurts. What actually runs out is memory, so ask about memory.
//!
//! This is not a reservation and it cannot be. Nothing here stops the *other*
//! things on the machine — a browser, an IDE, an esbuild worker — from taking the
//! headroom a moment later. It refuses the spawn that would obviously be the last
//! straw, which is the difference between a session that fails to start and a
//! desktop that dies. The compositor being OOM-killed is not a hypothetical here;
//! it happened, and the machine lost its whole session over it.

use std::path::Path;

/// What one Claude Code session costs, resident.
///
/// Measured rather than guessed: four live sessions on this machine averaged
/// 380MB PSS each. Rounded up, because a session that has been working for an
/// hour is not the one that was just measured.
pub const SESSION_MB: u64 = 450;

/// How much has to be left *after* the new session fits.
///
/// A desktop needs room to keep drawing. Below this the kernel starts choosing
/// what to kill, and it does not choose the agent.
pub const FLOOR_MB: u64 = 1500;

/// `MemAvailable` in MB: the kernel's own estimate of what can be handed out
/// without swapping, which is a better answer than free+cached arithmetic.
///
/// `None` when the file is not there or does not say — every caller treats that
/// as "no opinion" and allows the spawn, because refusing to work on a platform
/// whose memory we cannot read would be worse than the problem.
fn available_mb() -> Option<u64> {
    read_available(Path::new("/proc/meminfo"))
}

fn read_available(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

/// Refuse the spawn if it would leave the machine with nothing.
///
/// The error is the whole point of the feature, so it says the numbers: a message
/// that only says "not enough memory" leaves you guessing whether to close a tab
/// or reboot.
pub fn check() -> Result<(), String> {
    let Some(free) = available_mb() else {
        return Ok(());
    };
    if free < SESSION_MB + FLOOR_MB {
        return Err(format!(
            "only {free}MB available and a session needs about {SESSION_MB}MB, which would \
             leave the machine under its {FLOOR_MB}MB floor. Close something first — the \
             kernel's next move is to pick a process to kill, and it does not pick the agent."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meminfo(available_kb: u64) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("orchd-mem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("meminfo");
        std::fs::write(
            &p,
            format!("MemTotal:       32302708 kB\nMemFree:  100 kB\nMemAvailable: {available_kb} kB\n"),
        )
        .unwrap();
        p
    }

    #[test]
    fn available_is_read_in_megabytes() {
        let p = meminfo(4 * 1024 * 1024);
        assert_eq!(read_available(&p), Some(4096));
    }

    /// A machine with no `MemAvailable` line, or no `/proc` at all, must not be a
    /// machine where nothing can start.
    #[test]
    fn an_unreadable_meminfo_is_not_a_refusal() {
        assert_eq!(read_available(Path::new("/nonexistent/meminfo")), None);
        let dir = std::env::temp_dir().join(format!("orchd-mem-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("meminfo");
        std::fs::write(&p, "MemTotal: 32302708 kB\n").unwrap();
        assert_eq!(read_available(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
