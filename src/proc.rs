//! Run a child command with a deadline, portably.
//!
//! The bound used to be GNU `timeout <secs> …`, which is simply not on a Mac, so
//! it failed at the spawn and blamed the command for a binary it never named.
//! This enforces the deadline in Rust — no coreutils — and does it more strictly
//! than `timeout` did.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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
    run_bounded_with_input(cwd, timeout_secs, argv, label, None, &[])
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
/// `envs` are added to the child's inherited environment.
pub fn run_bounded_with_input(
    cwd: &Path,
    timeout_secs: u64,
    argv: &[String],
    label: &str,
    input: Option<Vec<u8>>,
    envs: &[(String, String)],
) -> Result<Output> {
    use std::os::unix::process::CommandExt;

    let (exe, args) = argv.split_first().with_context(|| format!("{label}: empty command"))?;
    let mut child = Command::new(exe)
        .args(args)
        .current_dir(cwd)
        // No stdin unless the caller has something to say: a command that stops to
        // prompt would otherwise hang until the deadline rather than failing.
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
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
    let mut out_pipe = child.stdout.take().context("no stdout pipe")?;
    let mut err_pipe = child.stderr.take().context("no stderr pipe")?;
    let out_thread = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out_pipe.read_to_end(&mut v);
        v
    });
    let err_thread = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err_pipe.read_to_end(&mut v);
        v
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let status = loop {
        if let Some(status) = child.try_wait().with_context(|| format!("waiting on {label}"))? {
            break status;
        }
        if Instant::now() >= deadline {
            // SAFETY: a group this call created, and SIGKILL takes the whole of
            // it — the command's own children included.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
            let _ = child.wait();
            bail!("{label} timed out after {timeout_secs}s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

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
        let script = format!("sleep 30 & echo $! > {}; sleep 30", marker.to_string_lossy());
        let argv = vec!["sh".to_string(), "-c".to_string(), script];

        let start = Instant::now();
        let err = run_bounded(&dir, 1, &argv, "test").expect_err("must time out");
        assert!(format!("{err:#}").contains("timed out after 1s"), "got {err:#}");
        assert!(start.elapsed() < Duration::from_secs(10), "did not stop at the deadline");

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
        )
        .expect("it finished rather than hitting the deadline");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), r#"{"worktreePath":"/x"}"#);
    }

    /// Set on the child, never on this process: `std::env::set_var` is
    /// process-global, and a daemon full of threads would be setting it for
    /// everything else running at that moment.
    #[test]
    fn an_env_var_reaches_the_child_without_touching_this_process() {
        let argv = vec!["sh".to_string(), "-c".to_string(), "printf %s \"$ORCH_T\"".to_string()];
        let envs = vec![("ORCH_T".to_string(), "child-only".to_string())];
        let out = run_bounded_with_input(Path::new("/tmp"), 5, &argv, "t", None, &envs).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "child-only");
        assert!(std::env::var("ORCH_T").is_err(), "the daemon's own env is untouched");
    }

    #[test]
    fn a_command_that_finishes_in_time_comes_back_whole() {
        let out = run_bounded(
            &std::env::temp_dir(),
            10,
            &["sh".to_string(), "-c".to_string(), "echo hi; echo bad >&2".to_string()],
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
                 i=$((i+1)); done".to_string(),
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
}
