//! The open-a-project screen's routes, on the host that now serves them.
//!
//! **These are the bootstrap server's tests, moved.** `firstrun.rs` used to run a
//! second application — its own axum server on its own port, its own router and
//! guard, a `BootstrapHost` trait for the two things needing a window, and an HTML
//! page with a copy of the SPA's palette. Its `validate` and `detect` routes are
//! the host's now and `web/js/open.js` is the screen, so what they promise is
//! asserted here, against a real host over HTTP.
//!
//! The third test is the behaviour the fold was *for*. The review step wrote the
//! **root** `config.json`, which is one checkout's — `host::checkout_dir` gives
//! every checkout its own `ORCHD_CONFIG_DIR`, and the root file is only ever
//! copied into one of them, once, when it happens to name that checkout. So a base
//! branch or a dev process detected for the second checkout you opened was
//! detected for nobody. An add now carries the answers into that checkout's own
//! directory, and this is what says so.
//!
//! Its own file because `ORCHD_CONFIG_DIR` is process-global and cargo gives each
//! `tests/*.rs` its own binary; two of these in one binary would race over the
//! variable that decides where every durable thing lands.
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

/// A git checkout with one commit, canonical for the reason `Config::parse` makes
/// it one: every comparison downstream is against a resolved path, and on macOS
/// `/tmp` is a symlink into `/private`.
fn scratch_repo(root: &Path, name: &str) -> PathBuf {
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
    dir
}

/// A `POST` with a JSON body and an Origin, which is what the page's `fetch` sends.
fn post(url: &str, origin: &str, token: &str, body: &str) -> (u32, serde_json::Value) {
    let out = std::process::Command::new("curl")
        .args([
            "-s",
            "-w",
            "\n%{http_code}",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-H",
            &format!("Origin: {origin}"),
            "--data-binary",
            body,
            url,
        ])
        .arg("-H")
        .arg(format!("x-orch-token: {token}"))
        .output()
        .expect("curl ran");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
    (
        code.trim().parse().unwrap_or(0),
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn the_open_screen_judges_a_folder_reads_it_and_carries_its_answers() {
    let root = std::env::temp_dir().join(format!("orchd-open-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();

    // **The host spawns `orchd` by name**, so the built binary has to be findable:
    // `child::daemon_binary` looks beside the running executable first, and a test
    // binary lives in `deps/`. `CARGO_BIN_EXE_orchd` is also what makes cargo build
    // it before this test runs.
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_orchd"));
    let bin_dir = exe.parent().unwrap().to_path_buf();
    let path_var = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{path_var}", bin_dir.display()));

    let cfg = root.join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::env::set_var("ORCHD_CONFIG_DIR", &cfg);

    let repo = scratch_repo(&root, "repo");
    let plain = root.join("plain");
    std::fs::create_dir_all(&plain).unwrap();

    let serving = orchd_serve::host::serve("host-token".into(), 0, orchd::window::Chrome::None)
        .await
        .expect("the host bound a port");
    let host = serving.host.clone();
    let base = format!("http://127.0.0.1:{}", host.port);
    let token = host.token.clone();

    // 1 — the cheap judgement the typed path asks per keystroke.
    let (code, body) = post(
        &format!("{base}/api/host/validate"),
        &base,
        &token,
        &format!(r#"{{"path":{:?}}}"#, repo.to_string_lossy()),
    );
    assert_eq!(code, 200, "a real checkout was refused: {body}");
    assert_eq!(body["name"], "repo");
    assert_eq!(body["path"].as_str(), Some(repo.to_string_lossy().as_ref()));

    // A refusal is a sentence the screen shows, not a status the screen has to
    // translate — the same shape every other host refusal takes.
    let (code, body) = post(
        &format!("{base}/api/host/validate"),
        &base,
        &token,
        &format!(r#"{{"path":{:?}}}"#, plain.to_string_lossy()),
    );
    assert_eq!(code, 400, "a directory with no .git was accepted: {body}");
    assert!(
        body["error"].as_str().is_some_and(|e| !e.is_empty()),
        "the refusal said nothing a person could act on: {body}"
    );

    // 2 — what the review panel fills itself from.
    let (code, found) = post(
        &format!("{base}/api/host/detect"),
        &base,
        &token,
        &format!(r#"{{"path":{:?}}}"#, repo.to_string_lossy()),
    );
    assert_eq!(code, 200, "detect refused a real checkout: {found}");
    assert_eq!(found["name"], "repo");
    assert!(
        found["base_branch"].as_str().is_some_and(|b| !b.is_empty()),
        "no base branch to preselect: {found}"
    );
    assert!(found["processes"].is_array(), "no process list: {found}");
    assert!(
        found["worktrees"].as_str().is_some(),
        "the screen shows where worktrees are cut: {found}"
    );

    // 3 — and an add carries the review's answers into *this* checkout's own
    // config, which is the whole reason the step can run for every checkout now.
    let (code, body) = post(
        &format!("{base}/api/host/checkout"),
        &base,
        &token,
        &format!(
            r#"{{"path":{0:?},"settings":{{"path":{0:?},"base_branch":"origin/trunk","env_source":"none"}}}}"#,
            repo.to_string_lossy()
        ),
    );
    assert_eq!(code, 200, "the add was refused: {body}");

    let written = orchd_serve::host::checkout_dir(&repo)
        .expect("the checkout has a state directory")
        .join("config.json");
    let raw = std::fs::read_to_string(&written)
        .unwrap_or_else(|e| panic!("no config at {}: {e}", written.display()));
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        value["upstream_ref"], "origin/trunk",
        "the reviewed base branch did not reach the checkout's own config: {raw}"
    );
    assert_eq!(
        value["main_checkout"].as_str(),
        Some(repo.to_string_lossy().as_ref()),
        "the config names another checkout: {raw}"
    );

    host.stop_all();
    let _ = std::fs::remove_dir_all(&root);
}
