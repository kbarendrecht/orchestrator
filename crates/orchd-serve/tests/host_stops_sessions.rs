//! Closing a checkout must take that checkout's sessions with it.
//!
//! **An integration test, because the defect lived between two processes.** The
//! host drops the child's stdin and then signals it; the child's stdin-EOF thread
//! used to answer that with `std::process::exit(0)`, which won the race against
//! the signal handler and so skipped [`Server::shutdown`]. A session's pty is a
//! `setsid` child, so it is not in the daemon's process group and survived — the
//! app closed a checkout and left a Claude Code behind, every time. Nothing below
//! the process boundary can see that: both halves were individually correct.
//!
//! The agent is a shell script on `PATH`, the same trick `tools/e2e` uses — the
//! daemon spawns a bare `claude` and resolves it there, so a shim substitutes for
//! it without the daemon knowing. **It ignores `SIGHUP`, and that is what gives
//! this test its power.** A daemon that dies closes the pty master, and the kernel
//! then hangs up the session — so a shim that took `SIGHUP` would die either way
//! and the test would pass with the defect in place. It was written that way first
//! and did. Node is entitled to decline a `SIGHUP` and Claude Code does, so what
//! has to reach the agent is `kill_gracefully`'s escalation, and only
//! [`Server::shutdown`] runs it.
// A test binary, so a panic is how a failure is reported. `clippy.toml`'s
// `allow-*-in-tests` covers `#[test]` functions and `#[cfg(test)]` modules, and
// the helpers in an integration crate are neither.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A git checkout, since a daemon refuses to start without one.
fn scratch_repo(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("orchd-hoststop-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Canonical, because `Config::parse` resolves `main_checkout`: on macOS `/tmp`
    // is a symlink into `/private`, so skipping this makes every comparison
    // against the daemon's own paths fail silently.
    let dir = dir.canonicalize().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("git ran");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "test@test"]);
    git(&["config", "user.name", "test"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    dir
}

/// Wait for a condition, or say what it still was. A condition rather than a
/// sleep, for the reason `docs/e2e.md` gives.
fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A `POST` with a body, an Origin and a token — the shape the page's `fetch`
/// sends to a child daemon.
fn post_json(url: &str, origin: &str, token: &str, body: &str) -> (u32, String) {
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "-w",
            "\n%{http_code}",
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "-H",
            &format!("Origin: {origin}"),
            "-H",
            &format!("x-orch-token: {token}"),
            "-d",
            body,
            url,
        ])
        .output()
        .expect("curl ran");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
    (code.trim().parse().unwrap_or(0), body.to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_checkout_takes_its_sessions_with_it() {
    let repo = scratch_repo("one");
    let cfg = repo.parent().unwrap().join("orchd-hoststop-cfg");
    let _ = std::fs::remove_dir_all(&cfg);
    std::fs::create_dir_all(&cfg).unwrap();
    std::env::set_var("ORCHD_CONFIG_DIR", &cfg);

    // The agent. The pid file path is baked into the script rather than passed in
    // the environment, because what `portable-pty` hands a session is not this
    // process's environment and the shim must not depend on it.
    let bin = repo.parent().unwrap().join("orchd-hoststop-bin");
    let _ = std::fs::remove_dir_all(&bin);
    std::fs::create_dir_all(&bin).unwrap();
    let pidfile = bin.join("claude.pid");
    std::fs::write(
        bin.join("claude"),
        format!(
            // The trap is not `exec`able — `exec` replaces the shell and loses it —
            // so the wait is a loop whose `sleep` is free to die with the pty.
            "#!/bin/sh\ntrap '' HUP\necho $$ > {}\nwhile :; do sleep 1; done\n",
            pidfile.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    // The child inherits this, and `PtyHandle::spawn` resolves a bare `claude`
    // against it.
    std::env::set_var(
        "PATH",
        format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );

    std::fs::write(
        cfg.join("config.json"),
        format!(r#"{{"main_checkout":{:?}}}"#, repo.to_string_lossy()),
    )
    .unwrap();

    let serving = orchd_serve::host::serve("host-token".into(), 0, orchd::window::Chrome::None)
        .await
        .expect("the host bound a port");
    let host = serving.host.clone();
    let base = format!("http://127.0.0.1:{}", host.port);
    let exe = Path::new(env!("CARGO_BIN_EXE_orchd"));
    host.open_checkout_with(exe, &repo, false)
        .expect("the child started and reported ready");
    let row = host.checkouts()[0].clone();

    let (code, body) = post_json(
        &format!("http://127.0.0.1:{}/api/session", row.port),
        &base,
        &row.token,
        r#"{"workspace":"main"}"#,
    );
    assert_eq!(code, 200, "the session was refused: {body}");

    until("the agent to start", || pidfile.exists());
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .expect("the shim wrote its pid");
    assert!(
        orchd::pty::pid_alive(pid),
        "the agent was gone before the checkout closed"
    );

    // The path a person takes from the app. It blocks, because a stop waits out
    // the child's own shutdown — which is the thing under test.
    let closed = {
        let host = host.clone();
        let repo = repo.clone();
        tokio::task::spawn_blocking(move || host.close_checkout(&repo))
            .await
            .unwrap()
    };
    assert!(closed, "there was no daemon to stop");

    // `Child::stop` waits for the child to be reaped, so by here the daemon has
    // had its whole graceful shutdown. A short grace is still allowed, because the
    // kill it sends and the agent's death are two processes.
    until("the agent to go with its daemon", || {
        !orchd::pty::pid_alive(pid)
    });

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&cfg);
    let _ = std::fs::remove_dir_all(&bin);
}
