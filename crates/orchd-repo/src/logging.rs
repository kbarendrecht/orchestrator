//! Where a log line can be read back from, when nobody is looking at a terminal.
//!
//! **An app launched from Finder or a desktop launcher has no stdout**, so every
//! line the daemon writes goes nowhere. That is not a small gap: it is why a
//! colleague reporting a slow start could not send anything to look at, and why
//! [`orchd_base::timing`] would have been invisible to the only people who can see the
//! problem. So the same lines also go to a file next to the config, which is the
//! one place every part of the app already agrees on.
//!
//! **This lives in the library rather than in the desktop shell**, which is where
//! it was written, because the shell is about to stop being the only host: a
//! checkout gets its own child `orchd`, and a child that logged to stdout would log
//! to a pipe the parent drains for one line and then leaves. One log shape means
//! the library owns the shape — same lines, same file name, same kept
//! generations, following `ORCHD_CONFIG_DIR` per process, so two hosts never write
//! over each other.
//!
//! [`KEPT`] generations are kept. A restart is the interesting case to compare
//! against and it would otherwise overwrite itself, while an unbounded log on a
//! machine nobody is watching is the other way to lose the information.

use std::path::{Path, PathBuf};

/// How many previous runs are kept beside the live log.
///
/// **One was not enough, and what proved it is the failure the file exists for.**
/// A crash that takes the app restarts it, so the crashed run's log is already at
/// `.1` by the time the window is back — and the *next* start overwrites it.
/// Quitting and reopening once, which is what a person does after their window
/// disappears, is enough to lose the only record of why. #23 was reported against a
/// log that had already rotated past its own answer, and read as "the log holds no
/// shutdown line", which is exactly what a rotated file looks like.
///
/// Five rather than two, because the number has to cover the starts between the
/// crash and somebody being asked for the file, and those are the reporter's habit
/// rather than ours. A quiet log is tens of kilobytes, so being wrong in this
/// direction costs a few hundred.
pub const KEPT: usize = 5;

/// Shift the kept generations along, leaving `orchd.log` free.
///
/// Oldest first, so no generation is overwritten before it has been moved itself.
/// A rename that fails is skipped rather than reported: this runs *before* the
/// subscriber exists, so there is nowhere to report it to, and the cost is one
/// generation of history rather than the log.
fn rotate(dir: &Path, live: &Path) {
    for n in (1..KEPT).rev() {
        let _ = std::fs::rename(
            dir.join(format!("orchd.log.{n}")),
            dir.join(format!("orchd.log.{}", n + 1)),
        );
    }
    let _ = std::fs::rename(live, dir.join("orchd.log.1"));
}

/// The file half of the subscriber.
struct LogFile(PathBuf);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogFile {
    /// Boxed because a file that cannot be opened has to degrade to writing
    /// nowhere. Losing the file log is not worth losing the app over, and it is
    /// the stdout layer that a developer is reading anyway.
    type Writer = Box<dyn std::io::Write>;

    /// Opened per line rather than held. It costs a syscall on a log this
    /// quiet, and it buys a file that is complete after a crash, which is the
    /// one case the log is being read for.
    fn make_writer(&'a self) -> Self::Writer {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)
        {
            Ok(f) => Box::new(f),
            Err(_) => Box::new(std::io::sink()),
        }
    }
}

/// Start logging to `<config dir>/orchd.log`, and optionally to stdout.
///
/// `default_filter` is what to use when `RUST_LOG` says nothing — the caller's,
/// because a host knows its own crate name and the library does not.
///
/// **`to_stdout` is false for a host-spawned child**, and that is not tidiness: a
/// child's stdout is a pipe its parent reads one protocol line from
/// (`child::ready_line`), so a subscriber writing there mixes prose and ANSI colour
/// into the channel the parent parses, and every line of it is already in the file.
/// A terminal daemon and the app keep stdout, where somebody is reading.
pub fn init(default_filter: &'static str, to_stdout: bool) {
    use tracing_subscriber::layer::{Layer, SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = move || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| default_filter.into())
    };
    let stdout = to_stdout.then(|| tracing_subscriber::fmt::layer().with_filter(filter()));

    // `ORCHD_CONFIG_DIR` moves this with everything else durable, which is what
    // keeps a fixture daemon from writing over the real log.
    let dir = crate::config::Config::config_dir().ok();
    let path = dir.as_ref().map(|dir| dir.join("orchd.log"));
    let file = path.clone().map(|path| {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
            rotate(dir, &path);
        }
        tracing_subscriber::fmt::layer()
            // No colour: this one is read in an editor, not a terminal.
            .with_ansi(false)
            .with_writer(LogFile(path))
            .with_filter(filter())
    });

    tracing_subscriber::registry()
        .with(stdout)
        .with(file)
        .init();

    // Said once, first, because the whole point of the file is that somebody has
    // to be able to find it without being told by hand.
    match path {
        Some(p) => tracing::info!("logging to {}", p.display()),
        None => tracing::warn!("no config dir — this run leaves no log file behind"),
    }
}

/// A panic reaches the log file, not only stderr.
///
/// The default hook prints to stderr, and a launcher-started app has none, so a
/// daemon that died this way left `orchd.log` ending mid-sweep with no line about
/// it: the last two and a half minutes read like a process that was still fine.
/// Written through `tracing` so it lands in the same file as everything else,
/// and *before* the default hook, which is kept: a terminal run still sees it.
///
/// `force_capture` rather than `capture`, because `RUST_BACKTRACE` is not set in
/// a Finder launch and this is the one place a backtrace is wanted regardless.
/// The release profile does not strip, so the frames carry names.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let thread = std::thread::current();
        tracing::error!(
            thread = thread.name().unwrap_or("unnamed"),
            "panic: {info}\n{backtrace}"
        );
        default(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(dir: &Path, name: &str) -> Option<String> {
        std::fs::read_to_string(dir.join(name)).ok()
    }

    /// A crash and the restart it causes must not be able to bury each other.
    ///
    /// The sequence this asserts is the one #23 was reported from: the run that
    /// crashed is at `.1` the moment the app comes back, and every ordinary start
    /// after that pushes it one further along. With a single generation the second
    /// start overwrote it, and the reporter read a file that began at the restart
    /// and concluded the log held nothing.
    #[test]
    fn a_crashed_run_survives_the_restarts_that_follow_it() {
        let dir = crate::testutil::scratch("log-rotate");
        let live = dir.join("orchd.log");
        std::fs::write(&live, "the run that crashed\n").expect("write the live log");

        // The restart the crash itself causes. Its own log becomes the live one.
        rotate(&dir, &live);
        assert_eq!(
            read(&dir, "orchd.log.1").as_deref(),
            Some("the run that crashed\n")
        );

        // And every start after it, which is what used to overwrite the evidence.
        for n in 2..=KEPT {
            std::fs::write(&live, format!("run {n}\n")).expect("write the live log");
            rotate(&dir, &live);
            assert_eq!(
                read(&dir, &format!("orchd.log.{n}")).as_deref(),
                Some("the run that crashed\n"),
                "the crashed run was lost after {n} start(s)"
            );
        }

        // It falls off the end eventually, which is the bound the unbounded log
        // does not have. Said here so the number is a decision rather than a
        // surprise: one more start than `KEPT` and it is gone.
        std::fs::write(&live, "one too many\n").expect("write the live log");
        rotate(&dir, &live);
        assert!(
            (1..=KEPT).all(|n| read(&dir, &format!("orchd.log.{n}")).as_deref()
                != Some("the run that crashed\n")),
            "the oldest generation must fall off rather than accumulate"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rotating an empty config dir is not an error, because it is every first run.
    #[test]
    fn a_first_run_has_nothing_to_rotate_and_says_nothing() {
        let dir = crate::testutil::scratch("log-rotate-first");
        rotate(&dir, &dir.join("orchd.log"));
        assert!(read(&dir, "orchd.log.1").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
