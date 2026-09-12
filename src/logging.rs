//! Where a log line can be read back from, when nobody is looking at a terminal.
//!
//! **An app launched from Finder or a desktop launcher has no stdout**, so every
//! line the daemon writes goes nowhere. That is not a small gap: it is why a
//! colleague reporting a slow start could not send anything to look at, and why
//! [`crate::timing`] would have been invisible to the only people who can see the
//! problem. So the same lines also go to a file next to the config, which is the
//! one place every part of the app already agrees on.
//!
//! **This lives in the library rather than in the desktop shell**, which is where
//! it was written, because the shell is about to stop being the only host: a
//! checkout gets its own child `orchd`, and a child that logged to stdout would log
//! to a pipe the parent drains for one line and then leaves. One log shape means
//! the library owns the shape — same lines, same file name, same single kept
//! generation, following `ORCHD_CONFIG_DIR` per process, so two hosts never write
//! over each other.
//!
//! One generation is kept. A restart is the interesting case to compare against
//! and it would otherwise overwrite itself, while an unbounded log on a machine
//! nobody is watching is the other way to lose the information.

use std::path::PathBuf;

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
            let _ = std::fs::rename(&path, dir.join("orchd.log.1"));
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
