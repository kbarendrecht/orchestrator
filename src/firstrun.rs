//! First-run logic: the recent-projects list and validating a chosen folder.
//!
//! The pure half of the first-boot flow — no window, no daemon, no Tauri — so it
//! runs and is tested like everything else. The desktop crate's bootstrap server
//! serves these over HTTP and adds the two things that need the app: the native
//! folder dialog and starting the daemon. Detection of a repo's settings (base
//! branch, GitHub repo, processes) is the review step and lands beside this later.
//!
//! Recents live in the config dir, so `ORCHD_CONFIG_DIR` relocates them with
//! everything else — which is what lets a test point the whole list at a temp dir.

use anyhow::{Context, Result};
use axum::{
    extract::{Path as AxPath, State},
    http::StatusCode,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;

/// A project opened before, newest first. The path is absolute; the name is its
/// last component, which is what a person recognises the checkout by.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentProject {
    pub path: String,
    pub name: String,
    /// Milliseconds since the epoch of the last open. The page renders "2 hours
    /// ago" from it; stored as a number so it needs no locale.
    pub last_opened_ms: u64,
}

/// What a valid checkout looks like to the open screen: enough to confirm the
/// choice before the daemon is asked to start on it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProjectInfo {
    pub path: String,
    pub name: String,
}

fn recent_file_in(dir: &Path) -> PathBuf {
    dir.join("recent.json")
}

/// The last component of a path, as a display name. `orchestrator` for
/// `~/development/orchestrator`. Falls back to the whole path if there is no
/// component (the filesystem root), which no real checkout is.
fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The recent-projects list, newest first. Missing or corrupt file reads as empty
/// rather than failing — a first run has no list, and a garbled one should not keep
/// the window shut.
pub fn recent_projects() -> Vec<RecentProject> {
    match Config::config_dir() {
        Ok(dir) => recent_projects_in(&dir),
        Err(_) => Vec::new(),
    }
}

fn recent_projects_in(dir: &Path) -> Vec<RecentProject> {
    let Ok(raw) = std::fs::read_to_string(recent_file_in(dir)) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// How many to keep. Long enough to cover the repos anyone juggles, short enough
/// that the list stays a glance rather than a history.
const MAX_RECENT: usize = 12;

/// Record that `path` was just opened: move it to the front with a fresh timestamp,
/// drop any older entry for the same path, and cap the list. Best effort — a failure
/// to write the list must never fail an open, so the caller logs and carries on.
pub fn record_recent(path: &Path) -> Result<()> {
    let dir = Config::config_dir()?;
    record_recent_in(&dir, path)
}

fn record_recent_in(dir: &Path, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    let mut list = recent_projects_in(dir);
    list.retain(|r| r.path != path_str);
    list.insert(
        0,
        RecentProject {
            name: name_of(path),
            path: path_str,
            last_opened_ms: now_ms(),
        },
    );
    list.truncate(MAX_RECENT);

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = recent_file_in(dir);
    std::fs::write(&file, serde_json::to_string_pretty(&list)? + "\n")
        .with_context(|| format!("writing {}", file.display()))?;
    Ok(())
}

/// Whether a chosen folder can host a session board, and its display name.
///
/// The two things that make an open fail later if they are wrong now: the folder
/// has to exist, and it has to be a git repository — the whole model is worktrees
/// cut from one checkout. `.git` as a file counts (a linked worktree), though
/// pointing the daemon at a worktree rather than its main checkout is a separate
/// mistake this does not police. Returns a human message, not an error type,
/// because it goes straight to the page.
pub fn validate(path: &Path) -> std::result::Result<ProjectInfo, String> {
    if !path.exists() {
        return Err("No such folder.".into());
    }
    if !path.is_dir() {
        return Err("That is a file, not a folder.".into());
    }
    if !path.join(".git").exists() {
        return Err("Not a git repository — orchd works on a git checkout.".into());
    }
    Ok(ProjectInfo {
        name: name_of(path),
        path: path.to_string_lossy().into_owned(),
    })
}

// ---------------------------------------------------------------------------
// The bootstrap server
// ---------------------------------------------------------------------------
//
// Served on an ephemeral loopback port before the daemon exists, so the window has
// a real page to load on first run. HTTP + `fetch` rather than Tauri IPC, on
// purpose: it is the same shape the daemon SPA already uses and it can be driven
// headlessly in a test, where IPC would need the real window. The two things that
// need the window — the native folder dialog and starting the daemon — are behind
// `BootstrapHost`, which the desktop crate implements and a test stubs.

/// The window-side actions the bootstrap page cannot do over HTTP. Implemented by
/// the desktop crate (Tauri) and stubbed in tests.
pub trait BootstrapHost: Send + Sync + 'static {
    /// Open the native folder dialog and block until the user answers. `None` on
    /// cancel. Runs on a request thread, never the GTK main thread.
    fn pick(&self) -> Option<PathBuf>;
    /// Commit to a checkout: start the daemon on it and hand the window over. Fire
    /// and forget — the page has already been told the open is under way.
    fn open(&self, path: PathBuf);
    /// Drive the frameless window — drag, resize edges, minimise, close. The
    /// first-run window has no decorations (the SPA that follows draws its own), so
    /// the page draws a titlebar and calls this, the same way the daemon's SPA does.
    fn window_cmd(&self, cmd: crate::window::WindowCmd);
}

#[derive(Deserialize)]
struct PathReq {
    path: String,
}

/// The answer to validate/pick/open, flat so the page reads one shape. `picked` is
/// only meaningful for the dialog: false means cancelled, distinct from a folder
/// that was chosen and rejected.
#[derive(Serialize, Default)]
struct Outcome {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    picked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Outcome {
    fn of(result: std::result::Result<ProjectInfo, String>) -> Self {
        match result {
            Ok(info) => Outcome {
                ok: true,
                name: Some(info.name),
                path: Some(info.path),
                ..Default::default()
            },
            Err(error) => Outcome {
                ok: false,
                error: Some(error),
                ..Default::default()
            },
        }
    }
}

/// The bootstrap router: the first-run page and the JSON it calls.
pub fn router(host: Arc<dyn BootstrapHost>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("firstrun.html")) }))
        .route("/api/recent", get(|| async { Json(recent_projects()) }))
        .route("/api/validate", post(validate_route))
        .route("/api/pick", post(pick_route))
        .route("/api/open", post(open_route))
        .route("/api/window/:cmd", post(window_route))
        .route("/api/window/resize/:edge", post(resize_route))
        .with_state(host)
}

async fn window_route(
    State(host): State<Arc<dyn BootstrapHost>>,
    AxPath(cmd): AxPath<String>,
) -> StatusCode {
    match serde_json::from_value::<crate::window::WindowCmd>(serde_json::json!(cmd)) {
        Ok(cmd) => {
            host.window_cmd(cmd);
            StatusCode::OK
        }
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

async fn resize_route(
    State(host): State<Arc<dyn BootstrapHost>>,
    AxPath(edge): AxPath<String>,
) -> StatusCode {
    match serde_json::from_value::<crate::window::ResizeEdge>(serde_json::json!(edge)) {
        Ok(edge) => {
            host.window_cmd(crate::window::WindowCmd::StartResize(edge));
            StatusCode::OK
        }
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

async fn validate_route(Json(req): Json<PathReq>) -> Json<Outcome> {
    Json(Outcome::of(validate(Path::new(&req.path))))
}

async fn pick_route(State(host): State<Arc<dyn BootstrapHost>>) -> Json<Outcome> {
    match host.pick() {
        None => Json(Outcome {
            ok: false,
            picked: Some(false),
            ..Default::default()
        }),
        Some(path) => {
            let mut out = Outcome::of(validate(&path));
            out.picked = Some(true);
            Json(out)
        }
    }
}

async fn open_route(
    State(host): State<Arc<dyn BootstrapHost>>,
    Json(req): Json<PathReq>,
) -> Json<Outcome> {
    let path = PathBuf::from(&req.path);
    // Validate again server-side: the page validated to enable the button, but the
    // tree could have moved since, and this is the last gate before the daemon.
    match validate(&path) {
        Ok(_) => {
            if let Err(e) = record_recent(&path) {
                // A recents write must never block an open — log and go on.
                tracing::warn!("could not record the recent project: {e:#}");
            }
            host.open(path);
            Json(Outcome {
                ok: true,
                ..Default::default()
            })
        }
        Err(e) => Json(Outcome {
            ok: false,
            error: Some(e),
            ..Default::default()
        }),
    }
}

/// A running bootstrap server: its address and the task serving it.
pub struct Serving {
    pub addr: SocketAddr,
    pub task: tokio::task::JoinHandle<()>,
}

impl Serving {
    /// The URL to point the window at.
    pub fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }
}

/// Bind the bootstrap server on an ephemeral loopback port and start serving.
pub async fn serve(host: Arc<dyn BootstrapHost>) -> Result<Serving> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("binding the bootstrap server")?;
    let addr = listener.local_addr()?;
    let app = router(host);
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("bootstrap server stopped: {e}");
        }
    });
    Ok(Serving { addr, task })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique dir per test, so the recents functions can be exercised through
    /// their dir-taking half with no global `ORCHD_CONFIG_DIR` — the tests run in
    /// parallel, and one process-wide env var would race between them.
    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orchd-firstrun-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_repo(at: &Path) {
        std::fs::create_dir_all(at.join(".git")).unwrap();
    }

    #[test]
    fn validate_wants_a_folder_that_is_a_git_repo() {
        let base = std::env::temp_dir().join(format!("orchd-val-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        let missing = base.join("nope");
        assert!(validate(&missing).is_err(), "a path that does not exist");

        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(validate(&plain).is_err(), "a folder with no .git");

        let repo = base.join("myrepo");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);
        let info = validate(&repo).expect("a git repo validates");
        assert_eq!(info.name, "myrepo", "the name is the last path component");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn recents_round_trip_newest_first_without_duplicates() {
        let dir = tmp("roundtrip");
        assert!(recent_projects_in(&dir).is_empty(), "a fresh dir has no recents");

        record_recent_in(&dir, Path::new("/a/alpha")).unwrap();
        record_recent_in(&dir, Path::new("/b/bravo")).unwrap();
        let list = recent_projects_in(&dir);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].path, "/b/bravo", "the most recent is first");
        assert_eq!(list[0].name, "bravo");

        // Re-opening alpha moves it to the front and does not duplicate it.
        record_recent_in(&dir, Path::new("/a/alpha")).unwrap();
        let list = recent_projects_in(&dir);
        assert_eq!(list.len(), 2, "the same path is not listed twice");
        assert_eq!(list[0].path, "/a/alpha");
    }

    #[test]
    fn recents_are_capped() {
        let dir = tmp("capped");
        for i in 0..(MAX_RECENT + 5) {
            record_recent_in(&dir, &PathBuf::from(format!("/p/repo{i}"))).unwrap();
        }
        assert_eq!(recent_projects_in(&dir).len(), MAX_RECENT, "the list is bounded");
    }

    #[test]
    fn a_corrupt_recent_file_reads_as_empty() {
        let dir = tmp("corrupt");
        std::fs::write(recent_file_in(&dir), "not json").unwrap();
        assert!(recent_projects_in(&dir).is_empty(), "garbage does not keep the window shut");
    }

    // --- the bootstrap router ------------------------------------------------

    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use std::sync::Mutex;
    use tower::ServiceExt; // for `oneshot`

    /// A `BootstrapHost` that records opens and answers the dialog with a fixed
    /// path, so the router's contract can be checked without a window.
    struct StubHost {
        opened: Mutex<Vec<PathBuf>>,
        pick_result: Option<PathBuf>,
    }
    impl BootstrapHost for StubHost {
        fn pick(&self) -> Option<PathBuf> {
            self.pick_result.clone()
        }
        fn open(&self, path: PathBuf) {
            self.opened.lock().unwrap().push(path);
        }
        fn window_cmd(&self, _cmd: crate::window::WindowCmd) {}
    }

    async fn post(host: Arc<dyn BootstrapHost>, uri: &str, body: &str) -> serde_json::Value {
        let res = router(host)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(res.into_body(), 1 << 16).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    fn stub() -> Arc<StubHost> {
        Arc::new(StubHost {
            opened: Mutex::new(Vec::new()),
            pick_result: None,
        })
    }

    #[tokio::test]
    async fn validate_route_answers_ok_or_a_message() {
        let base = std::env::temp_dir().join(format!("orchd-router-val-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);

        let ok = post(stub(), "/api/validate", &format!("{{\"path\":{:?}}}", repo.to_string_lossy())).await;
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["name"], "proj");

        let bad = post(stub(), "/api/validate", "{\"path\":\"/no/such/place\"}").await;
        assert_eq!(bad["ok"], false);
        assert!(bad["error"].is_string(), "an invalid folder explains itself");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn open_route_validates_then_hands_the_path_to_the_host() {
        let base = std::env::temp_dir().join(format!("orchd-router-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);

        let host = stub();
        let out = post(host.clone(), "/api/open", &format!("{{\"path\":{:?}}}", repo.to_string_lossy())).await;
        assert_eq!(out["ok"], true);
        assert_eq!(host.opened.lock().unwrap().as_slice(), &[repo.clone()], "the host was handed the checkout");

        // A folder that is not a repo never reaches the host.
        let host = stub();
        let out = post(host.clone(), "/api/open", "{\"path\":\"/no/such/place\"}").await;
        assert_eq!(out["ok"], false);
        assert!(host.opened.lock().unwrap().is_empty(), "an invalid open is refused before the daemon");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn pick_route_distinguishes_cancel_from_a_chosen_folder() {
        let base = std::env::temp_dir().join(format!("orchd-router-pick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);

        // Cancelled dialog.
        let cancelled = post(stub(), "/api/pick", "").await;
        assert_eq!(cancelled["picked"], false);
        assert_eq!(cancelled["ok"], false);

        // A folder was chosen and it validates.
        let host = Arc::new(StubHost {
            opened: Mutex::new(Vec::new()),
            pick_result: Some(repo.clone()),
        });
        let picked = post(host, "/api/pick", "").await;
        assert_eq!(picked["picked"], true);
        assert_eq!(picked["ok"], true);
        assert_eq!(picked["name"], "proj");
        let _ = std::fs::remove_dir_all(&base);
    }
}
