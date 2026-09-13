//! What every host test needs: a scratch checkout, `curl`, and a wait.
//!
//! **These were six copies, and they had already drifted.** `scratch_repo` came in
//! three signatures, `curl` in four return shapes, and one of them carried a
//! `_unused: Option<()>` parameter that is nothing but the scar of a copy. Six
//! copies of a fixture is six places to fix the next thing a fixture gets wrong —
//! and the thing this fixture has got wrong before is not hypothetical: a checkout
//! that is not canonicalised is on the wrong side of the boundary `Config::parse`
//! draws, which on macOS is the normal case and fails silently.
//!
//! **A module rather than a crate**, because cargo treats `tests/common/mod.rs` as
//! a module each test binary compiles for itself rather than as a seventh test
//! binary. `ORCHD_CONFIG_DIR` is process-global and each `tests/*.rs` is its own
//! process, which is exactly why these tests are in six files to begin with.
//!
//! Not every test binary uses every helper, and each compiles this module for
//! itself — so `dead_code` fires on whatever that one did not call. Allowed here
//! rather than at six `mod common;` lines: what is unused in one binary is
//! load-bearing in another, and the answer is the same at all six.

// A test binary, so a panic is how a failure is reported. `clippy.toml`'s
// `allow-*-in-tests` reaches `#[test]` functions and `#[cfg(test)]` modules, and a
// helper in an integration crate is neither.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A scratch directory of this test binary's own, emptied first.
///
/// The pid is in the name because `cargo test` runs the six binaries at once, and
/// the tag because one binary may want two.
pub fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("orchd-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A git checkout with one commit, and optionally an identity.
///
/// **Canonical, and that is the whole of why this is a function.** `Config::parse`
/// resolves `main_checkout`, so every comparison downstream is against a resolved
/// path — and on macOS `/tmp` is a symlink into `/private`, so a fixture that skips
/// this makes the checkout list disagree with the daemon about which directory it
/// manages, with nothing reporting it.
///
/// **`repo` is pinned in the checkout's own config rather than left to a remote**,
/// and that is what keeps these tests offline. `polled_repo` and the daemon's
/// `resolve_repo` both read that key before they shell out to `git remote
/// get-url`, so a pinned value exercises the same refusals with no URL anything
/// might try to fetch — and the boot fetch is on the critical path of a start, so
/// an unreachable remote would cost a test the whole ready timeout.
pub fn scratch_repo(root: &Path, name: &str, repo: Option<&str>) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
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

    if let Some(repo) = repo {
        let state = orchd_serve::host::checkout_dir(&dir).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join("config.json"),
            format!(
                r#"{{"main_checkout":{:?},"repo":{repo:?},"auto_resume":false}}"#,
                dir.to_string_lossy()
            ),
        )
        .unwrap();
    }
    dir
}

/// Put the built `orchd` on PATH, where the host looks for it.
///
/// `child::daemon_binary` looks beside the running executable first, and a test
/// binary lives in `deps/`. `CARGO_BIN_EXE_orchd` is also what makes cargo build
/// the daemon before a test that spawns one runs — so the caller passes it in
/// rather than this guessing a target directory.
pub fn daemon_on_path(exe: &str) {
    let bin_dir = Path::new(exe).parent().unwrap().to_path_buf();
    let path_var = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{path_var}", bin_dir.display()));
}

/// The status and body of a `curl` run, split off the `-w` line.
fn run_curl(args: &[&str]) -> (u32, String) {
    let out = std::process::Command::new("curl")
        .args(args)
        .output()
        .expect("curl ran");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
    (code.trim().parse().unwrap_or(0), body.to_string())
}

/// `GET`, with the daemon token when there is one.
///
/// `None` is a request with no token at all, which is what a guard test wants:
/// the refusal and the absence are different answers.
pub fn get(url: &str, token: Option<&str>) -> (u32, String) {
    let mut args = vec!["-s", "-w", "\n%{http_code}", url];
    let header;
    if let Some(token) = token {
        header = format!("x-orch-token: {token}");
        args.extend(["-H", &header]);
    }
    run_curl(&args)
}

/// The response headers alone, for the CORS answers.
pub fn headers_of(url: &str, origin: &str, token: &str) -> String {
    run_curl(&[
        "-s",
        "-D",
        "-",
        "-o",
        "/dev/null",
        "-w",
        "\n%{http_code}",
        "-H",
        &format!("Origin: {origin}"),
        "-H",
        &format!("x-orch-token: {token}"),
        url,
    ])
    .1
}

/// `POST`, in the shape the page's `fetch` sends: a JSON body, an Origin and a
/// token.
///
/// **A body-less POST is a CORS *simple* request**, which is why the Origin goes
/// on every one of these rather than only the ones carrying JSON: it is the header
/// the daemon's guard refuses on, and a test that omits it is testing a request no
/// browser makes.
pub fn post(url: &str, origin: &str, token: &str, body: Option<&str>) -> (u32, String) {
    let mut args = vec![
        "-s",
        "-w",
        "\n%{http_code}",
        "-X",
        "POST",
        "-H",
        "content-type: application/json",
    ];
    let origin_header = format!("Origin: {origin}");
    let token_header = format!("x-orch-token: {token}");
    args.extend(["-H", &origin_header, "-H", &token_header]);
    if let Some(body) = body {
        args.extend(["--data-binary", body]);
    }
    args.push(url);
    run_curl(&args)
}

/// The same, with the answer parsed. `Null` when it was not JSON, which reads as
/// the assertion failure it is rather than as a panic inside the helper.
pub fn post_json(url: &str, origin: &str, token: &str, body: &str) -> (u32, serde_json::Value) {
    let (code, text) = post(url, origin, token, Some(body));
    (
        code,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

/// The `OPTIONS` a browser sends before a cross-origin call that carries a token.
///
/// **Every call the page makes is one of these.** The page comes from the host's
/// port and carries `x-orch-token`, which makes the request non-simple — so the
/// browser asks first and refuses to send the real one unless the answer names its
/// origin. Nothing answered that for a long time, and the symptom was a board that
/// drew from its websockets and could then do nothing at all: only curl, which has
/// no CORS, worked.
pub fn preflight(url: &str, origin: &str) -> u32 {
    run_curl(&[
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "\n%{http_code}",
        "-X",
        "OPTIONS",
        "-H",
        &format!("Origin: {origin}"),
        "-H",
        "Access-Control-Request-Method: POST",
        "-H",
        "Access-Control-Request-Headers: content-type,x-orch-token",
        url,
    ])
    .0
}

/// Wait for a condition, or say what was there on the last look.
///
/// **A condition rather than a sleep**, for the reason `docs/e2e.md` gives: a
/// daemon's start is a network fetch away from slow, so a fixed sleep trades
/// flakiness for slowness and gets both.
///
/// **The deadline is the child's own, not a round number.** `child::READY_TIMEOUT`
/// is 60s, so anything waiting on a daemon *starting* — which a restart is —
/// cannot honestly give up sooner than the launch it waits for. 30s was shorter
/// than the thing it waited for, and a cold CI runner is exactly where that shows.
pub fn until<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    until_seeing(what, String::new, &mut ready)
}

/// [`until`] for a `bool` condition, where there is nothing to hand back.
pub fn until_true(what: &str, mut ready: impl FnMut() -> bool) {
    until(what, || ready().then_some(()));
}

/// [`until`] with a closure that renders what the condition could see — the
/// difference between "timed out" and a failure that names its own cause.
pub fn until_seeing<T>(
    what: &str,
    mut seen: impl FnMut() -> String,
    ready: &mut impl FnMut() -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(value) = ready() {
            return value;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}\nlast saw: {}", seen());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
