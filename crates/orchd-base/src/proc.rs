//! Run a child command with a deadline, portably.
//!
//! The bound used to be GNU `timeout <secs> …`, which is simply not on a Mac, so
//! it failed at the spawn and blamed the command for a binary it never named.
//! This enforces the deadline in Rust — no coreutils — and does it more strictly
//! than `timeout` did.

// One libc call: `killpg` to take a bounded command's whole process group when its
// deadline fires. The workspace denies `unsafe_code`; this is one of three modules
// that opt out.
#![allow(unsafe_code)]
use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Run blocking work off the async runtime, naming it so a panic says what died.
///
/// The rule this exists to make cheap: **no `std::process::Command` and no
/// `std::fs` on a tokio worker.** Both park the thread, and a runtime with a
/// handful of workers has no spare capacity — the symptom is the board freezing
/// while a `git status` walks a worktree, which is exactly what `reconcile` was
/// moved off the runtime for. The rule was already written down; it was applied
/// unevenly, and every un-wrapped site was one worker away from the same freeze.
///
/// A helper rather than bare `spawn_blocking` at each site so the `JoinError` is
/// mapped the same way everywhere: a panic in the closure becomes an error naming
/// `what`, instead of a `JoinError` the caller has to interpret. Work returning
/// `Result` yields `Result<Result<T>>`, so those callers finish with `??` — the
/// shape the codebase already used where it wrapped by hand.
pub async fn run_blocking<T, F>(what: &'static str, f: F) -> Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .with_context(|| format!("{what} panicked"))
}

/// Was the spawn refused because the binary is not there?
///
/// Read off the chain rather than the top, because [`run_bounded`] wraps the io
/// error in context. It lives here rather than beside either caller: it is a
/// property of how a bounded spawn reports a missing binary, so the next
/// `run_bounded` caller that wants "not installed" rather than "failed" finds it
/// instead of writing a third copy. `git` and `gh` each had one.
pub fn not_installed(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Run `argv` in `cwd`, killed if it outlives `timeout_secs`.
///
/// The child leads **its own process group**, and the *group* is what gets
/// signalled — stricter than `timeout`, which reaches only the direct child, so a
/// script that shells out (a review command to `gh`, a worktree-setup script to
/// `git`) cannot leave those children behind.
///
/// Both pipes are drained on threads, or a child that fills one and blocks would
/// never reach the deadline. `label` is only for the timeout message, so a caller
/// gets "reviews timed out" rather than a generic one.
pub fn run_bounded(cwd: &Path, timeout_secs: u64, argv: &[String], label: &str) -> Result<Output> {
    run_bounded_with_input(cwd, timeout_secs, argv, label, None, &[], None)
}

/// A place for a bounded command's output to go **while it runs**, a line at a
/// time.
///
/// `Arc` and not a plain closure because both pipes are drained on their own
/// thread, so the two halves of one command's output share it. Each stream keeps
/// its own order; the order *between* them is the scheduler's, since two threads
/// are reading. A caller that needs stdout and stderr to relate to each other has
/// to ask for a pty, not for this.
pub type LineSink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// [`run_bounded`], reporting each line as the child writes it.
///
/// For a command a person is waiting on: a worktree cut is a fetch, a checkout of
/// 18k files and a warm-up, and the board had one word for all of it. The `Output`
/// is unchanged — the sink is a copy of the stream, not a replacement — so a
/// caller that also reads stdout as a protocol (`WorktreeCreate` prints the path
/// it made) keeps working.
pub fn run_bounded_streaming(
    cwd: &Path,
    timeout_secs: u64,
    argv: &[String],
    label: &str,
    sink: LineSink,
) -> Result<Output> {
    run_bounded_with_input(cwd, timeout_secs, argv, label, None, &[], Some(sink))
}

/// Read one pipe to the end, keeping every byte and announcing whole lines.
///
/// Kept as one function with the sink optional, because the two behaviours differ
/// only in whether anybody is listening: the bytes are collected either way, since
/// `stderr_tail` and the `WorktreeCreate` contract both read the whole stream after
/// the child has gone.
fn drain(
    mut pipe: impl Read + Send + 'static,
    sink: Option<LineSink>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut all = Vec::new();
        let Some(sink) = sink else {
            let _ = pipe.read_to_end(&mut all);
            return all;
        };
        let mut buf = [0u8; 4096];
        let mut line = Vec::new();
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    all.extend_from_slice(&buf[..n]);
                    for &b in &buf[..n] {
                        // A carriage return ends a line too: git writes its progress
                        // that way, and holding it back would mean one 4kB line at
                        // the end instead of a count that moves.
                        if b == b'\n' || b == b'\r' {
                            emit(&sink, &mut line);
                        } else {
                            line.push(b);
                        }
                    }
                }
            }
        }
        // A last line with no terminator is still something the child said.
        emit(&sink, &mut line);
        all
    })
}

/// Hand one line over and clear the buffer, unless it is nothing.
fn emit(sink: &LineSink, line: &mut Vec<u8>) {
    let text = strip_ansi(&String::from_utf8_lossy(line));
    line.clear();
    if !text.trim().is_empty() {
        sink(&text);
    }
}

/// Drop the escape sequences a terminal would have acted on.
///
/// The reader here is a web page, not a terminal, so a `\x1b[K` arrives as
/// visible rubbish rather than as an erased line — and scripts written for a
/// person emit them freely. Only CSI and the two-byte forms are handled, which is
/// all a progress line uses; anything more needs a real parser, and the answer to
/// that is a pty rather than a bigger regex.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI: parameters, then one letter that says what it meant. Anything else
        // is a two-byte escape, and the `next` above has already eaten its second
        // byte.
        if let Some('[') = chars.next() {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

/// The last three stderr lines of a failed command, on one line, for a log entry
/// that says what went wrong without pasting the whole stream.
pub fn stderr_tail(stderr: &[u8]) -> String {
    let tail: String = String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .join(" / ");
    if tail.is_empty() {
        "no stderr".to_string()
    } else {
        tail
    }
}

/// [`run_bounded`], with a payload written to the child's stdin and the pipe then
/// closed.
///
/// For a Claude Code hook, which is defined as reading one JSON object on stdin.
/// Closing the pipe after the write is what keeps the no-prompting property the
/// plain version gets from `/dev/null`: a command that waits for more input sees
/// EOF rather than hanging until the deadline.
///
/// Written on a thread, like the two output pipes and for the same reason: a
/// payload larger than the pipe buffer would otherwise deadlock against a child
/// that is writing output before it finishes reading.
///
/// `envs` are added to the child's inherited environment, and `sink` — if there is
/// one — sees each line as the child writes it; see [`run_bounded_streaming`].
pub fn run_bounded_with_input(
    cwd: &Path,
    timeout_secs: u64,
    argv: &[String],
    label: &str,
    input: Option<Vec<u8>>,
    envs: &[(String, String)],
    sink: Option<LineSink>,
) -> Result<Output> {
    use std::os::unix::process::CommandExt;

    let (exe, args) = argv
        .split_first()
        .with_context(|| format!("{label}: empty command"))?;
    let mut child = Command::new(exe)
        .args(args)
        .current_dir(cwd)
        // No stdin unless the caller has something to say: a command that stops to
        // prompt would otherwise hang until the deadline rather than failing.
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        // On the child, never on this process. `std::env::set_var` is
        // process-global and this daemon is full of threads, so setting a variable
        // "for the hook" would set it for every other thing running at that moment.
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Makes the child a group leader, so its pid *is* the group id below.
        .process_group(0)
        .spawn()
        .with_context(|| format!("running `{}`", argv.join(" ")))?;

    let pgid = child.id() as libc::pid_t;
    if let (Some(payload), Some(mut pipe)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            use std::io::Write as _;
            // Both halves are ignorable on purpose: a hook that exits without
            // reading its input gives EPIPE, which is its business, not a failure
            // of the run. The drop closes the pipe and is what sends EOF.
            let _ = pipe.write_all(&payload);
        });
    }
    let out_pipe = child.stdout.take().context("no stdout pipe")?;
    let err_pipe = child.stderr.take().context("no stderr pipe")?;
    let out_thread = drain(out_pipe, sink.clone());
    let err_thread = drain(err_pipe, sink);

    let began = Instant::now();
    let deadline = began + Duration::from_secs(timeout_secs);
    // Backs off rather than sitting at one interval. Every wait here used to be
    // a flat 50ms, which rounded a 30ms `mise env` up to 50 and a 610ms login
    // shell up to 650 — a tax paid per spawn, on the path a session starts on.
    // Starting fine and widening keeps the short commands short without spending
    // a wakeup every 2ms on a build that runs for a minute.
    let mut wait = Duration::from_millis(2);
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("waiting on {label}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            // SIGTERM first, and a second to act on it. Git removes its `.lock`
            // files on SIGTERM and not on SIGKILL, and network git now runs under
            // this deadline: a fetch killed outright left `packed-refs.lock`
            // behind, and every later fetch failed on it.
            // SAFETY: a group this call created; the signal goes to the whole of
            // it, the command's own children included.
            unsafe { libc::killpg(pgid, libc::SIGTERM) };
            let grace = Instant::now() + Duration::from_secs(1);
            while Instant::now() < grace {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            // SIGKILL regardless of whether the leader went: a child of the
            // command that ignored SIGTERM would otherwise keep the output pipes
            // open, and the joins below would wait on it for as long as it lived.
            // SAFETY: as above.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
            let _ = child.wait();
            // What it said before it was stopped, because "timed out" alone is the
            // least useful line a log can carry about a stuck command.
            let stderr = err_thread.join().unwrap_or_default();
            bail!(
                "{label} timed out after {timeout_secs}s ({})",
                stderr_tail(&stderr)
            );
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_millis(50));
    };

    let took = began.elapsed();
    crate::timing::record_exec(took);
    // `info`, not `debug`, and per call rather than only when slow: these are the
    // few commands the daemon runs that are somebody else's shell config, so the
    // interesting fact is which one and how long, not whether it crossed a line.
    // There are a handful per start, so the log stays readable.
    tracing::info!("{label} took {}ms in {}", took.as_millis(), cwd.display());

    Ok(Output {
        status,
        stdout: out_thread.join().unwrap_or_default(),
        stderr: err_thread.join().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bound holds without GNU `timeout`, and takes the command's *children*
    /// with it — a setup or review script shelling out is the normal case.
    #[test]
    fn the_deadline_fires_and_takes_the_whole_process_group() {
        let dir = std::env::temp_dir();
        let marker = dir.join(format!("orchd-proc-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        // A grandchild that outlives the deadline; killing only the direct child
        // would leave it — the case `timeout` did not cover.
        let script = format!(
            "sleep 30 & echo $! > {}; sleep 30",
            marker.to_string_lossy()
        );
        let argv = vec!["sh".to_string(), "-c".to_string(), script];

        let start = Instant::now();
        let err = run_bounded(&dir, 1, &argv, "test").expect_err("must time out");
        assert!(
            format!("{err:#}").contains("timed out after 1s"),
            "got {err:#}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "did not stop at the deadline"
        );

        let grandchild: u32 = std::fs::read_to_string(&marker)
            .expect("recorded")
            .trim()
            .parse()
            .expect("a pid");
        let mut alive = true;
        for _ in 0..100 {
            if !crate::pty::pid_alive(grandchild) {
                alive = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_file(&marker);
        assert!(!alive, "the group's grandchild survived the deadline");
    }

    /// The deadline is SIGTERM before SIGKILL, so a command that cleans up on
    /// SIGTERM gets to. Git does, for its lock files. The shell here says so on
    /// stderr and exits, and that line has to come back in the error, which is
    /// the other half: a timeout that names what the command was saying.
    #[test]
    fn the_deadline_asks_before_it_kills_and_reports_what_it_heard() {
        let dir = std::env::temp_dir();
        let script = "trap 'echo cleaned-up >&2; exit 3' TERM; sleep 30".to_string();
        let argv = vec!["sh".to_string(), "-c".to_string(), script];
        let start = Instant::now();
        let err = run_bounded(&dir, 1, &argv, "test").expect_err("must time out");
        let said = format!("{err:#}");
        assert!(said.contains("timed out after 1s"), "got {said}");
        assert!(
            said.contains("cleaned-up"),
            "the trap's stderr is not in the error: {said}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "did not stop at the deadline"
        );
    }

    /// A hook is defined as reading one JSON object on stdin, so the payload has to
    /// arrive and the pipe has to close.
    ///
    /// The closing half is the one worth a test: without it a hook that reads to EOF
    /// waits for the full deadline and is then killed, which looks exactly like a
    /// hook that hung.
    #[test]
    fn a_payload_reaches_the_child_and_the_pipe_then_closes() {
        let argv = vec!["sh".to_string(), "-c".to_string(), "cat".to_string()];
        let out = run_bounded_with_input(
            Path::new("/tmp"),
            5,
            &argv,
            "t",
            Some(br#"{"worktreePath":"/x"}"#.to_vec()),
            &[],
            None,
        )
        .expect("it finished rather than hitting the deadline");
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            r#"{"worktreePath":"/x"}"#
        );
    }

    /// Set on the child, never on this process: `std::env::set_var` is
    /// process-global, and a daemon full of threads would be setting it for
    /// everything else running at that moment.
    #[test]
    fn an_env_var_reaches_the_child_without_touching_this_process() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf %s \"$ORCH_T\"".to_string(),
        ];
        let envs = vec![("ORCH_T".to_string(), "child-only".to_string())];
        let out =
            run_bounded_with_input(Path::new("/tmp"), 5, &argv, "t", None, &envs, None).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "child-only");
        assert!(
            std::env::var("ORCH_T").is_err(),
            "the daemon's own env is untouched"
        );
    }

    #[test]
    fn a_command_that_finishes_in_time_comes_back_whole() {
        let out = run_bounded(
            &std::env::temp_dir(),
            10,
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo hi; echo bad >&2".to_string(),
            ],
            "test",
        )
        .expect("ran");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "bad");
    }

    /// Output bigger than the pipe buffer would deadlock a naive wait-then-read,
    /// and a deadline it cannot reach is not a deadline. This is why both pipes
    /// drain on their own threads.
    #[test]
    fn output_larger_than_a_pipe_buffer_does_not_deadlock() {
        let out = run_bounded(
            &std::env::temp_dir(),
            30,
            &[
                "sh".to_string(),
                "-c".to_string(),
                // ~1MB, well past the 64K pipe buffer.
                "i=0; while [ $i -lt 16384 ]; do printf '%s\\n' \
                 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; \
                 i=$((i+1)); done"
                    .to_string(),
            ],
            "test",
        )
        .expect("ran");
        assert!(out.status.success());
        assert!(out.stdout.len() > 900_000, "got {} bytes", out.stdout.len());
    }

    #[test]
    fn an_empty_command_is_an_error_not_a_panic() {
        assert!(run_bounded(&std::env::temp_dir(), 5, &[], "test").is_err());
    }

    /// The point of streaming is that a line arrives *before* the command ends.
    /// Asserted by holding the child open past the first write: a sink that only
    /// sees its lines at the end would still pass an assertion made afterwards.
    #[test]
    fn a_line_reaches_the_sink_while_the_command_is_still_running() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let into = std::sync::Arc::clone(&seen);
        let sink: LineSink = std::sync::Arc::new(move |line: &str| {
            if let Ok(mut v) = into.lock() {
                v.push(line.to_string());
            }
        });
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo first; sleep 1; echo second".to_string(),
        ];
        let began = Instant::now();
        let watcher = {
            let seen = std::sync::Arc::clone(&seen);
            std::thread::spawn(move || {
                // Well inside the sleep, and well after the first echo.
                std::thread::sleep(Duration::from_millis(400));
                seen.lock().map(|v| v.len()).unwrap_or(0)
            })
        };
        let out =
            run_bounded_streaming(&std::env::temp_dir(), 10, &argv, "test", sink).expect("it ran");
        assert!(out.status.success());
        assert!(
            began.elapsed() >= Duration::from_millis(900),
            "the child really did wait"
        );
        // `>=`, not `==`: what this is about is that a line arrived before the
        // command ended, and pinning the count would make a slow runner's timing
        // into a failure about something the test does not claim.
        assert!(
            watcher.join().unwrap_or(0) >= 1,
            "the first line was reported while the command was still running"
        );
        assert_eq!(
            *seen.lock().expect("not poisoned"),
            vec!["first".to_string(), "second".to_string()]
        );
        // The sink is a copy of the stream, never a replacement: a caller that reads
        // stdout as a protocol (`WorktreeCreate` prints the path it made) is unaffected.
        assert_eq!(String::from_utf8_lossy(&out.stdout), "first\nsecond\n");
    }

    /// A script written for a person writes for a terminal: it overwrites its own
    /// line with a carriage return and erases with `\x1b[K`. The reader here is a
    /// web page, so a held-back line would arrive as one 4kB blob at the end and an
    /// escape would arrive as visible rubbish.
    ///
    /// **One stream, deliberately.** This asserted an interleaving of stdout and
    /// stderr at first, and the two pipes are drained on two threads — so the order
    /// *between* them is the scheduler's, not the script's. It passed on every
    /// Linux run and went red on macos-14, after the tag had been pushed. What this
    /// is about is how one stream is cut into lines; ordering across two is not
    /// something the sink promises.
    #[test]
    fn progress_lines_are_split_on_a_return_and_stripped_of_escapes() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let into = std::sync::Arc::clone(&seen);
        let sink: LineSink = std::sync::Arc::new(move |line: &str| {
            if let Ok(mut v) = into.lock() {
                v.push(line.to_string());
            }
        });
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            // Two steps on one terminal line, then a third with no terminator at
            // all — all on stderr, which is where a script reports its steps.
            "printf '  … step one\\r\\033[K  ✔ step one\\nno newline here' 1>&2".to_string(),
        ];
        run_bounded_streaming(&std::env::temp_dir(), 10, &argv, "test", sink).expect("it ran");
        let seen = seen.lock().expect("not poisoned").clone();
        assert_eq!(
            seen,
            vec![
                "  … step one".to_string(),
                "  ✔ step one".to_string(),
                "no newline here".to_string(),
            ]
        );
    }
}
