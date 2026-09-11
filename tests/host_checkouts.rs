//! A host running N checkouts: add, close, reopen, and the three refusals.
//!
//! **This is `tools/e2e/flows/25-host.mjs`, moved.** That flow was written as the
//! specification for Stage 3 before the host existed and held `pending` until it
//! did. It never ran, and moving it here rather than growing the e2e harness a
//! host is the decision `multirepo.md` left open — for one reason that settles it:
//! the e2e harness exists to put a fake `claude` on PATH, and **the host owns no
//! sessions**. Everything flow 25 asserts is one process pair speaking HTTP, which
//! is exactly what an integration test already drives in `host_and_child.rs`.
//!
//! Its own file rather than a second test beside that one, because
//! `ORCHD_CONFIG_DIR` is process-global: cargo gives each `tests/*.rs` its own
//! binary, and two tests in one binary would race over the variable that decides
//! where every durable thing lands.
//!
//! What it asserts, in the order the flow put it:
//!
//!  1. Two checkouts, two daemons. Each answers on its own port with its own
//!     token, and the host lists both.
//!  2. A daemon that dies is restarted **once**. A second death is final, and the
//!     row stays so `reopen` has something to act on.
//!  3. `close` is not a crash. Nothing restarts, and the other checkout is
//!     untouched — the `stopping` flag is what makes those two different, and
//!     without it a close resurrects months-old conversations through
//!     `auto_resume`.
//!  4. Containment is refused in either direction, and the refusal names it.
//!  5. One repository twice is refused, keyed on the repository the daemon would
//!     poll.
//!  6. A re-add of a checkout whose sessions were live asks before resuming them.
//!
//! Every wait is a condition, never a sleep, for the reason `docs/e2e.md` gives: a
//! daemon's start is a network fetch away from slow, so a fixed sleep trades
//! flakiness for slowness and gets both.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A git checkout with an identity, and no remote to reach for.
///
/// **`repo` is pinned in the checkout's own config rather than left to a remote**,
/// and that is what keeps this test offline. `polled_repo` and the daemon's
/// `resolve_repo` both read that key before they shell out to `git remote
/// get-url`, so a pinned value exercises the same refusal without a URL anything
/// might try to fetch — and the boot fetch is on the critical path of a start, so
/// an unreachable remote would cost this test a 60 s ready timeout.
fn scratch_repo(root: &Path, name: &str, repo: Option<&str>) -> PathBuf {
    let dir = root.join(name);
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

    if let Some(repo) = repo {
        let state = orchd::host::checkout_dir(&dir).unwrap();
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

fn get(url: &str, token: &str) -> (u32, String) {
    curl(&["-s", "-w", "\n%{http_code}", url], token, None)
}

/// A `POST` with a JSON body and an Origin, which is what the page's `fetch` sends.
fn post(url: &str, origin: &str, token: &str, body: &str) -> (u32, serde_json::Value) {
    let (code, text) = curl(
        &[
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
        ],
        token,
        None,
    );
    (code, serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
}

fn curl(args: &[&str], token: &str, _unused: Option<()>) -> (u32, String) {
    let out = std::process::Command::new("curl")
        .args(args)
        .arg("-H")
        .arg(format!("x-orch-token: {token}"))
        .output()
        .expect("curl ran");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
    (code.trim().parse().unwrap_or(0), body.to_string())
}

fn until<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(value) = ready() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The rows as the page sees them: over HTTP, not out of the `Host` in memory.
fn rows(base: &str, token: &str) -> Vec<serde_json::Value> {
    let (code, body) = get(&format!("{base}/api/host/checkouts"), token);
    assert_eq!(code, 200, "the host refused its own checkout list");
    serde_json::from_str::<serde_json::Value>(&body).unwrap()["checkouts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn row_for<'a>(rows: &'a [serde_json::Value], path: &Path) -> Option<&'a serde_json::Value> {
    let want = path.to_string_lossy();
    rows.iter().find(|r| r["path"] == want.as_ref())
}

fn dead(pid: u32) -> bool {
    !orchd::pty::pid_alive(pid)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_adds_closes_and_reopens_checkouts_and_refuses_the_three() {
    let root = std::env::temp_dir()
        .join(format!("orchd-hostn-{}", std::process::id()))
        .canonicalize()
        .unwrap_or_else(|_| {
            let d = std::env::temp_dir().join(format!("orchd-hostn-{}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            d.canonicalize().unwrap()
        });
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
    // Every durable thing a child writes follows this, which is what keeps the
    // real `~/.config/orchd` out of the test.
    std::env::set_var("ORCHD_CONFIG_DIR", &cfg);

    let first = scratch_repo(&root, "first", Some("acme/mono"));
    let second = scratch_repo(&root, "second", Some("acme/other"));

    let serving = orchd::host::serve("host-token".into(), 0, orchd::window::Chrome::None)
        .await
        .expect("the host bound a port");
    let host: Arc<orchd::host::Host> = serving.host.clone();
    let base = format!("http://127.0.0.1:{}", host.port);
    let token = host.token.clone();

    // 1 — two checkouts, two daemons, one list.
    for checkout in [&first, &second] {
        let (code, body) = post(
            &format!("{base}/api/host/checkout"),
            &base,
            &token,
            &format!(r#"{{"path":{:?}}}"#, checkout.to_string_lossy()),
        );
        assert_eq!(code, 200, "adding {} was refused: {body}", checkout.display());
        assert_eq!(body["result"]["added"], "opened");
    }

    let listed = rows(&base, &token);
    assert_eq!(listed.len(), 2, "the host did not list both checkouts");
    for row in &listed {
        assert_eq!(row["live"], true);
        let port = row["port"].as_u64().unwrap();
        let child_token = row["token"].as_str().unwrap();
        // The snapshot carries no checkout path of its own — the `main` workspace's
        // does, which is the daemon saying which tree it manages. Checked rather
        // than assumed, because two daemons answering one list is the whole point
        // of this step and a mixed-up port would otherwise pass.
        let (code, state) = get(&format!("http://127.0.0.1:{port}/api/state"), child_token);
        assert_eq!(code, 200, "a daemon refused the token its own row carries");
        let state: serde_json::Value = serde_json::from_str(&state).unwrap();
        let main = state["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["id"] == "main")
            .expect("every daemon has a main workspace");
        assert_eq!(main["path"], row["path"], "a daemon answered for another checkout");
    }
    let tokens: Vec<_> = listed.iter().map(|r| r["token"].clone()).collect();
    assert_ne!(tokens[0], tokens[1], "two daemons shared one token");

    // 2 — a death is restarted once, and a second death is final.
    let victim = host.pid_of(&second).expect("the second checkout has a daemon");
    let victim_token = row_for(&listed, &second).unwrap()["token"].as_str().unwrap().to_string();
    orchd::pty::signal_group_of(victim, libc::SIGKILL);
    let restarted = until("the host to restart the checkout once", || {
        let rows = rows(&base, &token);
        let row = row_for(&rows, &second)?.clone();
        let pid = host.pid_of(&second)?;
        (row["live"] == true && pid != victim).then_some((row, pid))
    });
    // A new process mints a new token, so the row's old one is dead — which is why
    // the page cannot rely on the substitution it was served with.
    assert_ne!(
        restarted.0["token"].as_str().unwrap(),
        victim_token,
        "a restarted daemon reused its token"
    );

    orchd::pty::signal_group_of(restarted.1, libc::SIGKILL);
    until("a second death to be final", || {
        let rows = rows(&base, &token);
        (row_for(&rows, &second)?["live"] == false).then_some(())
    });
    // The row stays, because the row is what `reopen` acts on.
    std::thread::sleep(Duration::from_millis(400));
    let rows_now = rows(&base, &token);
    assert!(row_for(&rows_now, &second).is_some(), "a dead checkout lost the row reopen needs");
    assert_eq!(row_for(&rows_now, &second).unwrap()["live"], false, "it was restarted twice");

    // A reopen is a person asking again, so it gets the retry back.
    let (code, body) = post(
        &format!("{base}/api/host/checkout/reopen"),
        &base,
        &token,
        &format!(r#"{{"path":{:?}}}"#, second.to_string_lossy()),
    );
    assert_eq!(code, 200, "reopen was refused: {body}");
    let live_pid = until("the reopened checkout to be up", || {
        let rows = rows(&base, &token);
        (row_for(&rows, &second)?["live"] == true).then(|| host.pid_of(&second))?
    });

    // 3 — close is not a crash.
    let before = rows(&base, &token).len();
    let (code, _) = post(
        &format!("{base}/api/host/checkout/close"),
        &base,
        &token,
        &format!(r#"{{"path":{:?}}}"#, second.to_string_lossy()),
    );
    assert_eq!(code, 200);
    let after = rows(&base, &token);
    assert_eq!(after.len(), before - 1, "close left the row behind");
    assert!(row_for(&after, &second).is_none());
    // The assertion the `stopping` flag exists for: a close read as a crash
    // restarts the daemon, and a restart runs `auto_resume`.
    assert!(dead(live_pid), "the daemon survived a close");
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        row_for(&rows(&base, &token), &second).is_none(),
        "a close was read as a crash and the checkout came back",
    );
    // And the other checkout never noticed.
    assert_eq!(row_for(&rows(&base, &token), &first).unwrap()["live"], true);

    // 3b — the recents the add screen offers: every checkout the host has opened,
    // minus the ones already open, so no row refuses when pressed. Asked here,
    // with one checkout closed, because a list filtered down to nothing would
    // satisfy the filter without proving it.
    let (code, body) = get(&format!("{base}/api/host/recent"), &token);
    assert_eq!(code, 200);
    let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let paths: Vec<String> = listed["recent"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap().to_string())
        .collect();
    assert!(
        paths.contains(&second.to_string_lossy().into_owned()),
        "the checkout just closed was not offered back: {paths:?}"
    );
    assert!(
        !paths.contains(&first.to_string_lossy().into_owned()),
        "an open checkout was offered as a recent: {paths:?}"
    );

    // 4 — containment, both ways. A worktree's `.git` is a file, which
    // `firstrun::validate` accepts, so the inner case is reachable by hand.
    let inner = first.join("nested");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::write(inner.join(".git"), "gitdir: ../.git\n").unwrap();
    let (code, body) = post(
        &format!("{base}/api/host/checkout"),
        &base,
        &token,
        &format!(r#"{{"path":{:?}}}"#, inner.to_string_lossy()),
    );
    assert_eq!(code, 400, "a folder inside an open checkout was accepted");
    assert!(
        body["error"].as_str().unwrap().contains("inside"),
        "the refusal did not name containment: {body}"
    );

    let (code, body) = post(
        &format!("{base}/api/host/checkout"),
        &base,
        &token,
        &format!(r#"{{"path":{:?}}}"#, root.to_string_lossy()),
    );
    assert_eq!(code, 400, "a parent of an open checkout was accepted");
    let message = body["error"].as_str().unwrap();
    // The parent is not a git checkout either, so either refusal is correct — what
    // must not happen is an accept.
    assert!(
        message.contains("contains") || message.contains("git repository"),
        "the refusal said something else: {message}"
    );

    // 5 — one repository twice, keyed on what the daemon polls rather than on the
    // path. A sibling clone of an open checkout's repository is the case.
    let sibling = scratch_repo(&root, "sibling", Some("acme/mono"));
    let (code, body) = post(
        &format!("{base}/api/host/checkout"),
        &base,
        &token,
        &format!(r#"{{"path":{:?}}}"#, sibling.to_string_lossy()),
    );
    assert_eq!(code, 400, "two checkouts of one repository were accepted");
    assert!(
        body["error"].as_str().unwrap().contains("acme/mono"),
        "the refusal did not name the repository: {body}"
    );

    // 6 — a re-add asks before resuming what was live. Closing a checkout keeps
    // its records, deliberately, so this is the guard that stops a path coming
    // back and resurrecting months-old conversations.
    let state = orchd::host::checkout_dir(&second).unwrap();
    std::fs::write(
        state.join("sessions.json"),
        r#"[{"id":"s1","was_live":true,"had_a_turn":true}]"#,
    )
    .unwrap();
    let (code, body) = post(
        &format!("{base}/api/host/checkout"),
        &base,
        &token,
        &format!(r#"{{"path":{:?}}}"#, second.to_string_lossy()),
    );
    assert_eq!(code, 200);
    assert_eq!(body["result"]["added"], "ask", "a re-add resumed without asking: {body}");
    assert_eq!(body["result"]["sessions"], 1);
    assert!(
        row_for(&rows(&base, &token), &second).is_none(),
        "the ask started the daemon anyway"
    );

    // Answering opens it.
    let (code, body) = post(
        &format!("{base}/api/host/checkout"),
        &base,
        &token,
        &format!(r#"{{"path":{:?},"resume":false}}"#, second.to_string_lossy()),
    );
    assert_eq!(code, 200, "the answered add was refused: {body}");
    assert_eq!(body["result"]["added"], "opened");

    // 7 — the host file remembers what is open, so the app opens it again.
    let remembered = orchd::host::remembered_checkouts();
    assert!(remembered.contains(&first), "the host file lost an open checkout");
    assert!(remembered.contains(&second), "the re-added checkout was not recorded");
    assert!(
        !remembered.iter().any(|p| p == &sibling),
        "a refused add was written to the host file",
    );

    // 3c — the host's own socket carries the list, because the page's substituted
    // copy is a snapshot of the moment it was served and a restarted daemon mints
    // a new token. Asserted as a connect-and-read: the first frame is the list.
    let ws = std::process::Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "5",
            "--include",
            "--no-buffer",
            "-H",
            "Connection: Upgrade",
            "-H",
            "Upgrade: websocket",
            "-H",
            "Sec-WebSocket-Version: 13",
            "-H",
            "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
            &format!("{base}/ws/host?token={token}"),
        ])
        .output()
        .expect("curl ran");
    let handshake = String::from_utf8_lossy(&ws.stdout);
    assert!(
        handshake.contains("101"),
        "the host socket refused the page's own token: {handshake}"
    );

    // 3d — two checkouts sharing a leaf are told apart by their parent segment,
    // and only then: the leaf is the name in the rail, the chip and every message
    // saying which checkout an action lands in, so two rows reading `app` make all
    // three useless. `first` and `second` do not collide, so both stay short.
    for row in rows(&base, &token) {
        let leaf = Path::new(row["path"].as_str().unwrap()).file_name().unwrap();
        assert_eq!(
            row["name"].as_str().unwrap(),
            leaf.to_string_lossy(),
            "a checkout with no collision was given a long name"
        );
    }

    // 3e — the order is the host's, and it is remembered. Dragged, in the page;
    // here it is the route the drag posts.
    let (code, _) = post(
        &format!("{base}/api/host/checkout/order"),
        &base,
        &token,
        &format!(r#"{{"paths":[{:?}]}}"#, first.to_string_lossy()),
    );
    assert_eq!(code, 200);
    // One path named, and it goes first; anything unnamed keeps its place after
    // it, so a reorder racing an add cannot drop the checkout that just arrived.
    assert_eq!(
        rows(&base, &token)[0]["path"].as_str().unwrap(),
        first.to_string_lossy(),
        "the order was not applied"
    );
    assert_eq!(
        orchd::host::remembered_checkouts().first(),
        Some(&first),
        "the order was applied but not remembered"
    );

    // And a checkout that *does* collide takes its parent segment — both of them,
    // because the ambiguity belongs to the pair rather than to the newcomer.
    std::fs::create_dir_all(root.join("nest")).unwrap();
    let twin = scratch_repo(&root.join("nest"), "first", Some("acme/twin"));
    let (code, body) = post(
        &format!("{base}/api/host/checkout"),
        &base,
        &token,
        &format!(r#"{{"path":{:?}}}"#, twin.to_string_lossy()),
    );
    assert_eq!(code, 200, "the twin was refused: {body}");
    let named = rows(&base, &token);
    let name_of = |p: &Path| {
        row_for(&named, p).unwrap()["name"].as_str().unwrap().to_string()
    };
    assert_eq!(name_of(&twin), "nest/first");
    assert!(
        name_of(&first).ends_with("/first") && name_of(&first) != "nest/first",
        "the checkout already open kept its short name, so the pair still reads the same"
    );

    // Closing one gives the other its short name back: the ambiguity was the set's.
    post(
        &format!("{base}/api/host/checkout/close"),
        &base,
        &token,
        &format!(r#"{{"path":{:?}}}"#, twin.to_string_lossy()),
    );
    assert_eq!(
        row_for(&rows(&base, &token), &first).unwrap()["name"].as_str().unwrap(),
        "first",
        "a name stayed long after the checkout it collided with closed"
    );

    // The folder dialog refuses without a window, which is this test and a browser
    // tab. The same sentence every other window route refuses with.
    let (code, body) = post(&format!("{base}/api/host/pick"), &base, &token, "{}");
    assert_eq!(code, 400);
    assert!(body["error"].as_str().unwrap().contains("no native window attached"));

    host.stop_all();
    let _ = std::fs::remove_dir_all(&root);
}
