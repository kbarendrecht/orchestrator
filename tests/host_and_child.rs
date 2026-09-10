//! A host serving the page, with one checkout's daemon under it as a child.
//!
//! **An integration test rather than a unit one, because it needs the real
//! binary.** `env!("CARGO_BIN_EXE_orchd")` is only defined for a test in `tests/`,
//! and cargo builds that binary before running this — which is the point: the
//! `ready <port> <token>` line, the extra accepted origin and the stop are one
//! protocol spoken between two processes, and a stub on either side would leave
//! the halves free to drift.
//!
//! **It is also the closest thing to the app that can run headlessly.** The
//! desktop shell adds a window and a webview and nothing else to this arrangement,
//! so the shape below is what the app will do — `host::serve`, then
//! `Host::open_checkout` per checkout, then `stop_all` at quit. What this cannot
//! answer is whether the window works; only a person at a screen can.
//!
//! What it asserts, in order:
//!
//!  1. The page comes from the **host** and carries the **child's** port and token,
//!     not the host's — the substitution a page needs before it can call anything.
//!  2. The child answers that token from the host's origin, and refuses a foreign
//!     one. That widening is one exact string on the child's argv.
//!  3. A window command with no window refuses by name rather than panicking.
//!  4. A stop is not a crash: the row goes `live: false` and nothing restarts it.
//!  5. The checkout's state is under `checkouts/<leaf>-<hash>`, and the settings
//!     that were in the old single `config.json` came across.
//!  6. The instance lock now guards the **checkout**: a second daemon for one
//!     checkout is refused, which is the invariant `instance.rs` always claimed
//!     and could not keep while the lock lived one level up.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The one line of setup every assertion needs: a git checkout, since a daemon
/// refuses to start without one, and a config dir of its own so nothing here
/// touches the real `~/.config/orchd`.
fn scratch_repo(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("orchd-hostchild-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Canonical, because `Config::parse` resolves `main_checkout` and every
    // comparison downstream is against a resolved path. On macOS `/tmp` is a
    // symlink into `/private`, so skipping this makes the checkout list disagree
    // with the daemon about which directory it manages.
    let dir = dir.canonicalize().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("git ran");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "test@test"]);
    git(&["config", "user.name", "test"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    dir
}

/// A `GET`, returning status and body.
fn get(url: &str, token: Option<&str>) -> (u32, String) {
    curl(&["-s", "-w", "\n%{http_code}", url], token)
}

/// A `POST` with an Origin, which is what a page's `fetch` sends.
fn post(url: &str, origin: &str, token: &str) -> u32 {
    let (code, _) = curl(
        &["-s", "-o", "/dev/null", "-w", "\n%{http_code}", "-X", "POST", "-H", &format!("Origin: {origin}"), url],
        Some(token),
    );
    code
}

fn curl(args: &[&str], token: Option<&str>) -> (u32, String) {
    let mut command = std::process::Command::new("curl");
    command.args(args);
    if let Some(token) = token {
        command.arg("-H").arg(format!("x-orch-token: {token}"));
    }
    let out = command.output().expect("curl ran");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
    (code.trim().parse().unwrap_or(0), body.to_string())
}

/// Wait for a condition, or say what it still was. Every wait in here is one of
/// these rather than a sleep, for the reason `docs/e2e.md` gives: a daemon's start
/// is a network fetch away from slow, so a fixed sleep trades flakiness for
/// slowness and gets both.
fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_serves_the_page_for_a_checkout_its_child_manages() {
    let repo = scratch_repo("one");
    let cfg = repo.parent().unwrap().join("orchd-hostchild-cfg");
    let _ = std::fs::remove_dir_all(&cfg);
    std::fs::create_dir_all(&cfg).unwrap();
    // The child inherits this, so every durable thing it writes — its config, its
    // `sessions.json`, its hook settings, its lock — lands here rather than in the
    // real one. `ORCHD_CONFIG_DIR` is what makes that true for all of them at once.
    std::env::set_var("ORCHD_CONFIG_DIR", &cfg);

    let serving = orchd::host::serve("host-token".into(), 0, orchd::window::Chrome::None)
        .await
        .expect("the host bound a port");
    let host = serving.host.clone();
    let base = format!("http://127.0.0.1:{}", host.port);

    // The page before any checkout: served, and honest about having none.
    let (code, page) = get(&base, None);
    assert_eq!(code, 200, "GET / must not be token-gated");
    assert!(page.contains("checkouts: []"), "the page claimed a checkout it did not have");

    // The config as it stood before the split: one file at the root of the config
    // dir, with a hand-tuned key in it. The point of assertion 5 is that this key
    // survives, because the failure mode of getting the move wrong is not a crash —
    // it is a daemon that quietly writes a default config and loses every setting.
    std::fs::write(
        cfg.join("config.json"),
        format!(
            r#"{{"main_checkout":{:?},"worktree_retention_days":21}}"#,
            repo.to_string_lossy()
        ),
    )
    .unwrap();

    let exe = Path::new(env!("CARGO_BIN_EXE_orchd"));
    host.open_checkout_with(exe, &repo).expect("the child started and reported ready");

    // 1 — the page carries the child's port and token, not the host's.
    let rows = host.checkouts();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.path, repo.to_string_lossy());
    assert!(row.live);
    assert_ne!(row.port, host.port, "the child took the host's port");
    assert_ne!(row.token, host.token, "the child did not mint its own token");

    let (_, page) = get(&base, None);
    assert!(page.contains(&row.token), "the page went out without the child's token");
    assert!(
        page.contains(&format!("\"port\":{}", row.port)),
        "the page went out without the child's port"
    );
    let (code, listed) = get(&format!("{base}/api/host/checkouts"), Some(&host.token));
    assert_eq!(code, 200);
    assert!(listed.contains(&row.token), "the host's own list disagreed with the page");

    // 1b — the child does **not** serve the page. Its own host would be a second
    // page server nobody visits, and the app's is the one with the window.
    // Asserted on the *body*, not the status: an unknown route on a daemon answers
    // `200 {}` — the trap `CLAUDE.md` names, since that reads exactly like an empty
    // daemon — so a status check here would pass whether the page moved or not.
    let (_, body) = get(&format!("http://127.0.0.1:{}/", row.port), None);
    assert!(
        !body.contains("__ORCH__") && !body.contains(&row.token),
        "a hosted child is still serving a page of its own: {}",
        body.chars().take(120).collect::<String>()
    );

    // 2 — the child accepts the host's origin and refuses a foreign one.
    let child_api = format!("http://127.0.0.1:{}/api/prs/refresh", row.port);
    assert_eq!(post(&child_api, &base, &row.token), 202, "the child refused its host's origin");
    assert_eq!(
        post(&child_api, "http://evil.example", &row.token),
        403,
        "the child accepted a foreign origin"
    );
    // And the daemon really is managing the checkout the row names.
    let (code, state) = get(
        &format!("http://127.0.0.1:{}/api/state", row.port),
        Some(&row.token),
    );
    assert_eq!(code, 200);
    assert!(
        state.contains(&repo.to_string_lossy().replace('\\', "\\\\")),
        "the child answered for a different checkout"
    );

    // 6 — one daemon per checkout, enforced.
    //
    // **This is the lock re-key, and it came for free with the state split.** The
    // lock is `<config dir>/instance.pid`, and `<config dir>` for a child is now
    // `checkouts/<leaf>-<hash>` — derived from the checkout — so two daemons for one
    // checkout meet on one file where before they only met if their *hosts* shared
    // a config dir. Asserted here rather than in `instance.rs`, because the unit
    // test there drives `acquire_at` with a path it chose; what needed proving is
    // that the path a real child computes is the checkout's.
    let second = orchd::child::launch_at(
        exe,
        &repo,
        &base,
        &orchd::host::checkout_dir(&repo).unwrap(),
        |_, _, _| {},
    );
    let refusal = format!("{:#}", second.expect_err("a second daemon for one checkout started"));
    assert!(
        refusal.contains("never said it was ready"),
        "the second daemon failed for some other reason: {refusal}"
    );
    // And the first one is untouched by the attempt.
    assert!(host.checkouts().first().is_some_and(|c| c.live), "the refused start took the live one");

    // 3 — no window, so the titlebar refuses by name rather than panicking. This
    // is the browser-tab case, and it is the behaviour that had to survive the
    // window handle moving off `AppState`.
    let (code, refusal) = curl(
        &[
            "-s",
            "-w",
            "\n%{http_code}",
            "-X",
            "POST",
            "-H",
            &format!("Origin: {base}"),
            &format!("{base}/api/window/minimize"),
        ],
        Some(&host.token),
    );
    assert_eq!(code, 400);
    assert!(
        refusal.contains("no native window attached"),
        "the refusal stopped naming what is missing: {refusal}"
    );

    // 3b — the path the page will actually take: a relative call would reach the
    // host, so `core.LOCAL` aims at the child's port with the host's origin. That
    // pairing is the whole wiring, and it is the one thing a green type-check
    // cannot see.
    let as_the_page_would = post(&child_api, &base, &row.token);
    assert_eq!(as_the_page_would, 202, "the page's own call shape was refused");
    assert_eq!(
        post(&format!("{base}/api/prs/refresh"), &base, &host.token),
        404,
        "the host answered a daemon route, so a relative call would silently work"
    );

    // 4 — a stop is not a crash.
    let pid = {
        // Read it before the stop, because the handle goes with the child.
        let (_, listed) = get(&format!("{base}/api/host/checkouts"), Some(&host.token));
        assert!(listed.contains("\"live\":true"));
        row.port
    };
    assert!(host.stop_checkout(&repo), "there was no daemon to stop");
    until("the row to read as down", || {
        host.checkouts().first().is_some_and(|c| !c.live)
    });
    // Nothing restarted it: the row stays down, and the port stops answering.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        host.checkouts().first().is_some_and(|c| !c.live),
        "a stop was read as a crash and the daemon came back"
    );
    let (code, _) = get(&format!("http://127.0.0.1:{pid}/api/state"), Some(&row.token));
    assert_eq!(code, 0, "the child is still serving after a stop");

    // 5 — the checkout's state is its own, and it inherited the old config.
    let state = orchd::host::checkout_dir(&repo).expect("a state directory");
    assert!(state.starts_with(cfg.join("checkouts")), "the state dir is somewhere else: {state:?}");
    assert!(
        state.file_name().unwrap().to_string_lossy().starts_with(&format!(
            "{}-",
            repo.file_name().unwrap().to_string_lossy()
        )),
        "the directory is not named for its checkout: {state:?}"
    );
    let carried: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state.join("config.json")).unwrap()).unwrap();
    assert_eq!(
        carried["worktree_retention_days"], 21,
        "the hand-tuned key did not come across, so the split loses settings"
    );
    // And the old file is still there, so an older build has something to read.
    assert!(cfg.join("config.json").exists(), "the move deleted the config a downgrade needs");
    // The daemon really wrote into its own directory rather than the root. The hook
    // settings file is the one to check: it is written on every start (unlike
    // `sessions.json`, which a daemon with no sessions never writes at all), and it
    // is the file that carries *this daemon's port* into every agent's hook URL —
    // so two checkouts sharing it is the failure that makes one rail go quiet.
    let hooks = state.join("hooks.json");
    assert!(hooks.exists(), "the child wrote its hook settings somewhere else");
    assert!(
        std::fs::read_to_string(&hooks).unwrap().contains(&format!("127.0.0.1:{}", row.port)),
        "the hook settings do not carry this daemon's own port"
    );
    assert!(
        !cfg.join("hooks.json").exists(),
        "the child wrote its hook settings into the shared config dir"
    );

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&cfg);
}
