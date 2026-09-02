//! Where a slow start went, measured rather than guessed.
//!
//! Written for a report this machine cannot reproduce: colleagues on macOS see a
//! start several times slower than the one this is developed against, on better
//! hardware. Better hardware is the clue. Almost nothing in `start` is CPU work.
//! Every phase of it is a run of child processes, and a child process costs a Mac
//! far more than it costs Linux: dyld resolves the binary on every exec, and a
//! managed laptop usually has endpoint-security software with a hook on each one.
//! So the suspect is the *number* of execs, which nothing here was counting.
//!
//! Two things live here, and both are about being able to read a pasted log.
//!
//! - [`Phases`], a phase list logged as one line, so "which part was slow" is
//!   one line rather than a diff of timestamps.
//! - A process-wide exec count, because the phases alone cannot tell one slow
//!   `git status` from eighty fast ones, and those two have different fixes.
//!
//! Deliberately not a metrics system. It exists to turn a bug report into a
//! number, and the number has to survive being copied into a chat message.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Child processes spawned, and the wall time spent inside them.
///
/// Counted globally rather than handed down. The places that spawn are spread
/// over `git`, `proc` and `forge`, and threading a recorder through all of them
/// would be a larger change than the question deserves. Relaxed ordering,
/// because this is a diagnostic and a count that is one behind is still the
/// answer.
static EXECS: AtomicUsize = AtomicUsize::new(0);
static EXEC_MICROS: AtomicU64 = AtomicU64::new(0);

/// Note that a child process ran, and for how long.
pub fn record_exec(elapsed: Duration) {
    EXECS.fetch_add(1, Ordering::Relaxed);
    EXEC_MICROS.fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
}

/// How many child processes have run, and the wall time inside them.
pub fn execs() -> (usize, Duration) {
    (
        EXECS.load(Ordering::Relaxed),
        Duration::from_micros(EXEC_MICROS.load(Ordering::Relaxed)),
    )
}

/// A run of phases, reported as one line.
///
/// `mark` closes the phase that just finished rather than wrapping it, because
/// the phases in `start` are a straight sequence of statements and wrapping each
/// one in a closure would rewrite the function to suit the measurement.
pub struct Phases {
    started: Instant,
    last: Instant,
    /// Execs already counted when this began, so the line reports what *this*
    /// run spent rather than the process total. **Both halves, or the line is a
    /// contradiction**: the count was subtracted and the time was not, and a
    /// resumed session duly reported "1 child process(es) taking 6526ms" — the
    /// one exec it made, beside every exec the boot before it had made.
    execs_at_start: usize,
    micros_at_start: u64,
    marks: Vec<(&'static str, u128)>,
}

impl Phases {
    pub fn start() -> Self {
        let now = Instant::now();
        Phases {
            started: now,
            last: now,
            execs_at_start: EXECS.load(Ordering::Relaxed),
            micros_at_start: EXEC_MICROS.load(Ordering::Relaxed),
            marks: Vec::new(),
        }
    }

    /// Close the phase that just finished.
    pub fn mark(&mut self, what: &'static str) {
        let now = Instant::now();
        self.marks.push((what, now.duration_since(self.last).as_millis()));
        self.last = now;
    }

    /// The whole run so far.
    pub fn total(&self) -> Duration {
        self.started.elapsed()
    }

    /// Say it, loudly enough to reach a log a colleague pastes back.
    ///
    /// `info` rather than `debug`, because the only reason this exists is a
    /// report from a machine nobody here can attach a profiler to. A level that
    /// needs `RUST_LOG` set is a level that will not be on when it matters, and
    /// one line per start is not noise.
    pub fn log(&self, what: &str) {
        let (execs_now, time_now) = execs();
        let (mine, took) = self.share(execs_now, time_now.as_micros() as u64);
        let phases: Vec<String> = self
            .marks
            .iter()
            .map(|(name, ms)| format!("{name} {ms}ms"))
            .collect();
        tracing::info!(
            "{what}: {}ms total, {mine} child process(es) taking {}ms; {}",
            self.total().as_millis(),
            took.as_millis(),
            phases.join(", ")
        );
    }

    /// This run's share of the two process-wide counters.
    ///
    /// **Both subtractions in one place**, because doing one and forgetting the
    /// other is exactly what shipped: the count was this run's and the time was
    /// the process's, so a resumed session reported one child process taking 6.5
    /// seconds, which was the boot before it.
    ///
    /// Takes the totals rather than reading them, so the arithmetic can be tested
    /// without racing the suite. The counters are global and `cargo test` runs the
    /// tests that shell out in parallel with these, so any delta measured around a
    /// real `record_exec` is somebody else's traffic too.
    fn share(&self, execs_now: usize, micros_now: u64) -> (usize, Duration) {
        (
            execs_now.saturating_sub(self.execs_at_start),
            Duration::from_micros(micros_now.saturating_sub(self.micros_at_start)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both counters are process-wide, so a run reports its own share of
    /// **each**. Subtracting one and not the other is not a rounding error, it is
    /// a line that contradicts itself: a resumed session reported one child
    /// process taking 6.5 seconds, which was the boot before it.
    ///
    /// Fixed numbers, not real execs — see [`Phases::share`] for why measuring
    /// this against the live counters would race the rest of the suite.
    #[test]
    fn a_run_reports_only_the_execs_and_the_time_it_spent() {
        let now = Instant::now();
        let p = Phases {
            started: now,
            last: now,
            // 12 execs and 400ms of them already happened before this run began.
            execs_at_start: 12,
            micros_at_start: 400_000,
            marks: Vec::new(),
        };
        let (mine, took) = p.share(13, 410_000);
        assert_eq!(mine, 1, "one exec of its own");
        assert_eq!(took, Duration::from_millis(10), "and its 10ms, not the 410");
    }

    /// A counter that went backwards must not panic in a diagnostic. It cannot
    /// happen today, but a subtraction in a log line is the wrong place to find
    /// out that it can.
    #[test]
    fn a_share_of_less_than_nothing_is_nothing() {
        let now = Instant::now();
        let p = Phases {
            started: now,
            last: now,
            execs_at_start: 12,
            micros_at_start: 400_000,
            marks: Vec::new(),
        };
        assert_eq!(p.share(0, 0), (0, Duration::ZERO));
    }

    #[test]
    fn phases_are_recorded_in_order() {
        let mut p = Phases::start();
        p.mark("first");
        p.mark("second");
        let names: Vec<&str> = p.marks.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["first", "second"]);
    }
}
