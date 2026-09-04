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
fn recent_projects() -> Vec<RecentProject> {
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
fn record_recent(path: &Path) -> Result<()> {
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
    let path = expand_home(path, std::env::var_os("HOME").map(PathBuf::from).as_deref());
    if !path.exists() {
        return Err("No such folder.".into());
    }
    if !path.is_dir() {
        return Err("That is a file, not a folder.".into());
    }
    // Canonical, because this string is what `config.json` gets: a relative path
    // typed into the box would be resolved against whatever the daemon's cwd
    // happened to be on the next launch, and a symlinked one would fail the
    // path comparisons `Config::parse` canonicalises everything else for.
    let path = std::fs::canonicalize(&path).map_err(|e| format!("Cannot resolve that folder: {e}"))?;
    if !path.join(".git").exists() {
        return Err("Not a git repository — orchd works on a git checkout.".into());
    }
    Ok(ProjectInfo {
        name: name_of(&path),
        path: path.to_string_lossy().into_owned(),
    })
}

/// `~` and `~/x` mean the home directory, the way the box's own placeholder
/// writes it. A shell expands this; a text field does not, so the page's own
/// example was refused as "No such folder".
fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// What orchd worked out about a chosen checkout, for the review step to confirm.
/// Every field is a guess with a default, and the page says where each came from —
/// a wrong one is caught here rather than discovered on the first sweep.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Detected {
    pub path: String,
    pub name: String,
    /// The base ref worktrees branch from, `<remote>/<branch>`. The resolved
    /// default (`origin/main`) when the symref is known, else the first remote
    /// branch, else `origin/HEAD` — the daemon's own default.
    pub base_branch: String,
    /// The remote-tracking branches to choose among.
    pub base_branches: Vec<String>,
    /// `owner/name` for PR watching, from the origin remote. `None` off GitHub.
    pub repo: Option<String>,
    /// Where a session's environment comes from: `mise`, `direnv` or `none`,
    /// detected from the files in the checkout.
    pub env_source: String,
    /// Where worktrees are cut. Always the default today; shown so it is not a
    /// surprise later.
    pub worktrees: String,
    /// Long-running processes the repo appears to define — a compose stack, a dev
    /// watch. Offered unchecked: orchd never starts someone's stack behind their
    /// back on first open.
    pub processes: Vec<DetectedProcess>,
}

/// A process orchd guessed the repo runs, and how to run it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DetectedProcess {
    /// Short name for the drawer tab.
    pub name: String,
    /// The argv to run.
    pub command: Vec<String>,
    /// How it reads to a person (`pnpm run dev`).
    pub label: String,
    /// The file it was inferred from, shown so a wrong guess is obvious.
    pub source: String,
}

/// Long-running processes a repo appears to define. Best effort and conservative:
/// a compose file means a stack, and a small set of conventional dev scripts in a
/// `package.json` mean a watcher — anything cleverer would guess wrong more than it
/// helped, and these are offered unchecked anyway.
fn detect_processes(path: &Path) -> Vec<DetectedProcess> {
    let mut out = Vec::new();

    for f in [
        "compose.yaml",
        "compose.yml",
        "docker-compose.yml",
        "docker-compose.yaml",
    ] {
        if path.join(f).exists() {
            out.push(DetectedProcess {
                name: "docker".into(),
                command: vec!["docker".into(), "compose".into(), "up".into()],
                label: "docker compose up".into(),
                source: f.into(),
            });
            break; // one compose stack, not one per spelling
        }
    }

    if let Ok(raw) = std::fs::read_to_string(path.join("package.json")) {
        if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&raw) {
            // The package manager the repo uses, from its lockfile.
            let pm = if path.join("pnpm-lock.yaml").exists() {
                "pnpm"
            } else if path.join("yarn.lock").exists() {
                "yarn"
            } else if path.join("bun.lockb").exists() {
                "bun"
            } else {
                "npm"
            };
            // Only the conventional long-running ones — not every script, which is
            // mostly one-shot build and lint tasks nobody wants as a managed pty.
            const WANTED: &[&str] = &["dev", "start", "watch", "serve", "build-watch", "build:watch"];
            if let Some(scripts) = pkg.get("scripts").and_then(|s| s.as_object()) {
                for name in WANTED {
                    if scripts.contains_key(*name) {
                        out.push(DetectedProcess {
                            name: (*name).into(),
                            command: vec![pm.into(), "run".into(), (*name).into()],
                            label: format!("{pm} run {name}"),
                            source: "package.json".into(),
                        });
                    }
                }
            }
        }
    }

    out
}

/// Inspect a checkout and propose its settings. Best effort throughout — anything
/// it cannot read falls back to the daemon's own default rather than failing, so
/// the review always has something to show.
pub fn detect(path: &Path) -> Detected {
    let base_branches = crate::git::remote_branches(path);
    // A fork layout first — an `upstream` remote beside `origin` — because on one
    // every other guess below is the fork, and the review then pre-filled the
    // fork's default branch and the fork's repo. The daemon's own first write asks
    // the same question (`git::detect_base`), so the review shows what it would do.
    let fork = crate::git::detect_base(path);
    let remote = fork.as_ref().map(|(_, r)| r.as_str()).unwrap_or("origin");
    let prefix = format!("{remote}/");
    // Prefer the remote's recorded default; then a conventional main/master; then
    // any branch of that remote; then whatever branch there is.
    //
    // Every arm but the last is filtered on `base_branches`, because this fills a
    // `<select>` and pre-selecting a branch the list does not offer shows the
    // picker as blank. The last arm is deliberately *not* filtered: it is only
    // reached when there are no remote branches at all, so there is no list to be
    // absent from, and naming the daemon's own default ref is more use than "".
    let base_branch = fork
        .as_ref()
        .map(|(b, _)| b.clone())
        .filter(|b| base_branches.contains(b))
        .or_else(|| {
            crate::git::base_checkout_branch(path, &format!("{prefix}HEAD"))
                .map(|b| format!("{prefix}{b}"))
                .filter(|b| base_branches.contains(b))
        })
        .or_else(|| {
            base_branches
                .iter()
                .find(|b| *b == &format!("{prefix}main") || *b == &format!("{prefix}master"))
                .or_else(|| base_branches.iter().find(|b| b.starts_with(&prefix)))
                .cloned()
        })
        .or_else(|| base_branches.first().cloned())
        .unwrap_or_else(|| match &fork {
            Some((b, _)) => b.clone(),
            None => "origin/HEAD".to_string(),
        });

    let repo = crate::forge::github::GitHubForge::detect(path, remote)
        .map(|(owner, name)| format!("{owner}/{name}"));

    let env_source = if path.join("mise.toml").exists() || path.join(".mise.toml").exists() {
        "mise"
    } else if path.join(".envrc").exists() {
        "direnv"
    } else {
        "none"
    }
    .to_string();

    Detected {
        path: path.to_string_lossy().into_owned(),
        name: name_of(path),
        base_branch,
        base_branches,
        repo,
        env_source,
        worktrees: ".claude/worktrees".to_string(),
        processes: detect_processes(path),
    }
}

/// Write the first-run `config.json` from the review's answers, before the daemon
/// starts and reads it.
///
/// **Merged into the file that is there, never written over it.** This runs on
/// every open — a fresh checkout, a recent, a switch — and it used to build the
/// object from scratch, so a checkout that had moved was re-picked and every
/// hand-tuned key (`reviews_command`, `worktree_setup`, `main_processes`, the notes)
/// was gone with no copy kept. Only the keys the review answered change; the
/// previous file is kept beside it as `config.json.bak`.
///
/// Slim on purpose — only `main_checkout` and the fields that differ from a plain
/// default are written, the same shape a hand-written config takes, so the file
/// stays readable. Each value is validated by parsing the whole config before it is
/// written, so a bad override is a caught error rather than a daemon that refuses to
/// start. Never fatal to the caller: if this fails, the daemon's own `load_or_init`
/// still writes a sensible default for the checkout — the review's edits are just
/// lost, which is better than no window.
fn write_config(path: &Path, ov: &Overrides) -> Result<Written> {
    write_config_to(&Config::path()?, path, ov)
}

/// What [`write_config`] did, so a caller whose open is then refused can put the
/// file back the way it was: a switch writes the new checkout first and only then
/// learns whether the restart is possible, and without this a refused restart left
/// the daemon on one checkout and the disk naming another, so the *next* launch
/// opened the wrong project.
struct Written {
    file: PathBuf,
    /// The file's contents before the write; `None` when there was no file.
    ///
    /// **Held in memory rather than read back from `config.json.bak`.** The backup
    /// is written best effort, so a failed copy left this `None` while the config
    /// it was meant to preserve still had content, and undoing then *deleted* it.
    /// The text was already in hand at that point, which is what makes the whole
    /// question moot; the `.bak` stays as a convenience for a person, not as this
    /// function's memory.
    previous: Option<String>,
}

impl Written {
    /// Undo the write: put the previous contents back, or remove the file when
    /// nothing existed before. Best effort, logged.
    fn undo(&self) {
        let outcome = match &self.previous {
            Some(raw) => std::fs::write(&self.file, raw),
            None => std::fs::remove_file(&self.file),
        };
        if let Err(e) = outcome {
            tracing::warn!("could not restore {} after a refused open: {e}", self.file.display());
        }
    }
}

fn write_config_to(file: &Path, path: &Path, ov: &Overrides) -> Result<Written> {
    use serde_json::json;
    let previous = std::fs::read_to_string(file).ok();
    let mut obj = match previous.as_deref().map(serde_json::from_str::<serde_json::Value>) {
        Some(Ok(serde_json::Value::Object(m))) => m,
        None => serde_json::Map::new(),
        // Not JSON, or not an object: nothing in it can be kept by key, so start
        // over. The backup below is what keeps whatever it was.
        Some(_) => {
            tracing::warn!("{} is not a JSON object; replacing it", file.display());
            serde_json::Map::new()
        }
    };
    obj.insert("main_checkout".into(), json!(path.to_string_lossy()));

    if let Some(base) = ov.base_branch.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("upstream_ref".into(), json!(base));
        // `<remote>/<branch>` — the remote is what the push guard and the fetch use.
        if let Some((remote, _)) = base.split_once('/') {
            obj.insert("upstream_remote".into(), json!(remote));
        }
    } else if !obj.contains_key("upstream_ref") {
        // A recent opens with no review, so nothing answered the one question a
        // checkout can answer for itself. `Config::default_for` asked it on a fresh
        // file, but this write comes first and so that path is never reached from
        // the app — without this a fork layout was measured against `origin/HEAD`.
        if let Some((base, remote)) = crate::git::detect_base(path) {
            tracing::info!(%base, %remote, "detected a fork layout");
            obj.insert("upstream_ref".into(), json!(base));
            obj.insert("upstream_remote".into(), json!(remote));
        }
    }
    if let Some(repo) = ov.repo.as_deref().filter(|s| !s.is_empty()) {
        // Only a repo the remote does *not* already derive is worth pinning. The
        // review pre-fills this field from a remote, and writing that back made an
        // explicit override out of a default — one that kept PR polling on the fork
        // after the base had been pointed at `upstream`.
        let remote = obj
            .get("upstream_remote")
            .and_then(|v| v.as_str())
            .unwrap_or("origin");
        // Through `detect`, which is that pair of calls, and is what filled the
        // field the review is handing back — so the two answers cannot disagree.
        let derived = crate::forge::github::GitHubForge::detect(path, remote)
            .map(|(o, n)| format!("{o}/{n}"));
        if derived.as_deref() == Some(repo) {
            obj.remove("repo");
        } else {
            obj.insert("repo".into(), json!(repo));
        }
    }
    // Typed on the way in, so an unknown value is refused by serde while the
    // request is being read rather than round-tripped through a string here.
    if let Some(env) = ov.env_source {
        obj.insert("env_source".into(), json!(env));
    }
    if let Some(tracker) = ov.tracker {
        obj.insert("tracker".into(), json!(tracker));
    }
    // Ticked processes become managed `main_processes`. `autostart` is true because
    // ticking one in the review is the consent the setting's default withholds — the
    // rest of a `ManagedSpec` is left to serde defaults.
    let procs: Vec<_> = ov
        .processes
        .iter()
        .filter(|p| !p.command.is_empty())
        .map(|p| json!({ "name": p.name, "command": p.command, "autostart": true }))
        .collect();
    if !procs.is_empty() {
        obj.insert("main_processes".into(), json!(procs));
    }

    let raw = serde_json::to_string_pretty(&serde_json::Value::Object(obj))? + "\n";
    // The whole thing has to parse, or the daemon would fail to start on it later.
    Config::parse(&raw).context("the first-run config did not validate")?;

    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // One generation back on disk, for the edit above that turns out to be wrong
    // and is only noticed later, by a person. Best effort: a failed backup is not
    // a reason to refuse the open, and [`Written`] does not depend on it.
    if previous.is_some() {
        let bak = file.with_extension("json.bak");
        if let Err(e) = std::fs::copy(file, &bak) {
            tracing::warn!("could not keep {}: {e}", bak.display());
        }
    }
    std::fs::write(file, raw).with_context(|| format!("writing {}", file.display()))?;
    Ok(Written {
        file: file.to_path_buf(),
        previous,
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
    /// Commit to a checkout: start the daemon on it and hand the window over.
    /// `true` when the hand-over is under way; `false` when it was refused (a
    /// switch that cannot restart, a second open while the first boots), so the
    /// caller can undo what it wrote for it.
    fn open(&self, path: PathBuf) -> bool;
    /// Drive the frameless window — drag, resize edges, minimise, close. The
    /// first-run window has no decorations (the SPA that follows draws its own), so
    /// the page draws a titlebar and calls this, the same way the daemon's SPA does.
    fn window_cmd(&self, cmd: crate::window::WindowCmd);

    /// True when a daemon is already running and this is a **switch**, not first
    /// run. The page shows a way back to the current project when so, and the copy
    /// changes from "open" to "switch".
    fn switching(&self) -> bool {
        false
    }

    /// Abandon a switch and return to the running project. No-op on first run,
    /// where there is nothing to go back to.
    fn cancel(&self) {}
}

#[derive(Deserialize)]
struct PathReq {
    path: String,
}

/// The review's answers, sent with the open. Every override is optional — an
/// unset one leaves the daemon's default, and a `None` `repo` means "watch what
/// origin resolves to" rather than "watch nothing".
#[derive(Deserialize, Default)]
pub struct Overrides {
    pub path: String,
    #[serde(default)]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    /// Typed rather than a string, so serde refuses a value the daemon could not
    /// have loaded — the page sends the enum's own spelling, and a mismatch is a
    /// 422 naming the field instead of a config written and then rejected.
    #[serde(default)]
    pub env_source: Option<crate::config::EnvSourceKind>,
    #[serde(default)]
    pub tracker: Option<crate::config::TrackerKind>,
    /// The processes the user ticked in the review, to manage from the start.
    #[serde(default)]
    pub processes: Vec<SelectedProcess>,
}

/// A process the review ticked, to write into `main_processes`.
#[derive(Deserialize, Default)]
pub struct SelectedProcess {
    pub name: String,
    #[serde(default)]
    pub command: Vec<String>,
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

/// The bootstrap router: the first-run page and the JSON it calls. `port` is the
/// one it is served on, for the Host and Origin checks in [`guard`].
pub fn router(host: Arc<dyn BootstrapHost>, port: u16) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("firstrun.html")) }))
        .route("/api/context", get(context_route))
        .route("/api/recent", get(|| async { Json(recent_projects()) }))
        .route("/api/validate", post(validate_route))
        .route("/api/detect", post(detect_route))
        .route("/api/pick", post(pick_route))
        .route("/api/open", post(open_route))
        .route("/api/cancel", post(cancel_route))
        .route("/api/window/:cmd", post(window_route))
        .route("/api/window/resize/:edge", post(resize_route))
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| guard(port, req, next),
        ))
        .with_state(host)
}

/// The daemon's own Host and Origin rules, for the same reason it has them: a
/// loopback port is reachable by any page in any browser on the machine.
///
/// Body-less POSTs are CORS "simple requests", so without this every route here
/// was one `fetch` away from any site you had open — `/api/window/restart`, which
/// on a switch takes every live session with it, and `/api/pick`, which raises a
/// dialog. With DNS rebinding `GET /api/recent` read paths under `$HOME`. The
/// JSON routes were covered only by axum's content-type check, which is
/// incidental. The page is same-origin, so it costs nothing; no token, because
/// unlike the daemon's page this one has no secret to carry and nothing behind it
/// worth more than the window.
async fn guard(
    port: u16,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !crate::api::host_allowed(host, port) {
        return (StatusCode::FORBIDDEN, "bad host").into_response();
    }
    let origin = req.headers().get("origin").and_then(|v| v.to_str().ok());
    let is_get = req.method() == axum::http::Method::GET;
    if !crate::api::origin_ok(origin, port, false, is_get, false) {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }
    next.run(req).await
}

async fn window_route(
    State(host): State<Arc<dyn BootstrapHost>>,
    AxPath(cmd): AxPath<String>,
) -> StatusCode {
    match crate::api::parse_window_cmd(&cmd) {
        Some(cmd) => {
            host.window_cmd(cmd);
            StatusCode::OK
        }
        None => StatusCode::BAD_REQUEST,
    }
}

async fn resize_route(
    State(host): State<Arc<dyn BootstrapHost>>,
    AxPath(edge): AxPath<String>,
) -> StatusCode {
    match crate::api::parse_resize_edge(&edge) {
        Some(edge) => {
            host.window_cmd(crate::window::WindowCmd::StartResize(edge));
            StatusCode::OK
        }
        None => StatusCode::BAD_REQUEST,
    }
}

async fn validate_route(Json(req): Json<PathReq>) -> Json<Outcome> {
    Json(Outcome::of(validate(Path::new(&req.path))))
}

async fn detect_route(Json(req): Json<PathReq>) -> Result<Json<Detected>, StatusCode> {
    let path = PathBuf::from(req.path);
    // Several git runs, so off the runtime worker like every other git call: on a
    // switch this runtime is also serving the live daemon.
    crate::proc::run_blocking("detecting a checkout", move || detect(&path))
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Whether the page is a first run or a switch away from a running project.
async fn context_route(State(host): State<Arc<dyn BootstrapHost>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "switching": host.switching() }))
}

async fn cancel_route(State(host): State<Arc<dyn BootstrapHost>>) -> StatusCode {
    host.cancel();
    StatusCode::OK
}

async fn pick_route(State(host): State<Arc<dyn BootstrapHost>>) -> Json<Outcome> {
    // The dialog blocks until you answer it, and the trait promises "a request
    // thread" for exactly that — so give it one, rather than parking a runtime
    // worker for as long as the dialog is open.
    let picked = crate::proc::run_blocking("the folder dialog", move || host.pick())
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("{e:#}");
            None
        });
    match picked {
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
    Json(ov): Json<Overrides>,
) -> Json<Outcome> {
    // Off the runtime, like `detect_route` beside it: `validate` canonicalises and
    // `write_config` spawns git twice (the fork probe and the repo derivation), and
    // on a switch this runtime is also serving the live daemon.
    let prepared = crate::proc::run_blocking("preparing the chosen project", move || {
        // Validate again server-side: the page validated to enable the button, but
        // the tree could have moved since, and this is the last gate before the
        // daemon.
        let info = validate(Path::new(&ov.path))?;
        // The canonical path, not the one typed: it is what the config and the
        // recents record.
        let path = PathBuf::from(&info.path);
        // Persist the review's answers before the daemon reads them. Non-fatal:
        // a failure here loses the edits, not the open — `load_or_init` still
        // writes a default for the checkout. Written *before* the open on
        // purpose: on a switch the open closes the window, and the process may
        // be gone before a write placed after it lands.
        let written = write_config(&path, &ov)
            .map_err(|e| tracing::warn!("could not write the first-run config: {e:#}"))
            .ok();
        Ok((path, written))
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("{e:#}");
        Err("Could not read that folder. See the log.".to_string())
    });

    match prepared {
        Ok((path, written)) => {
            if !host.open(path.clone()) {
                // A refused open must not leave the disk naming a checkout the
                // daemon is not on, or the next launch opens the wrong project.
                if let Some(w) = written {
                    w.undo();
                }
                return Json(Outcome::of(Err(
                    "Could not switch to that project; the running one is kept. See the log."
                        .to_string(),
                )));
            }
            if let Err(e) = record_recent(&path) {
                tracing::warn!("could not record the recent project: {e:#}");
            }
            Json(Outcome {
                ok: true,
                ..Default::default()
            })
        }
        Err(e) => Json(Outcome::of(Err(e))),
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
    let app = router(host, addr.port());
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
        crate::testutil::scratch(&format!("firstrun-{tag}"))
    }

    fn git_repo(at: &Path) {
        std::fs::create_dir_all(at.join(".git")).unwrap();
    }

    use crate::testutil::git as run_git;

    #[test]
    fn validate_wants_a_folder_that_is_a_git_repo() {
        let base = tmp("val");

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

        // What is handed on is the canonical path: `..` segments and links are
        // resolved, since the string ends up in `config.json`.
        let roundabout = base.join("plain").join("..").join("myrepo");
        let info = validate(&roundabout).expect("a roundabout spelling still validates");
        assert_eq!(info.path, std::fs::canonicalize(&repo).unwrap().to_string_lossy());
        assert_eq!(info.name, "myrepo");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The placeholder in the box says `~/code/project`, and a text field does not
    /// expand `~` the way a shell does.
    #[test]
    fn a_tilde_means_the_home_directory() {
        let home = Path::new("/home/someone");
        assert_eq!(expand_home(Path::new("~/code/x"), Some(home)), PathBuf::from("/home/someone/code/x"));
        assert_eq!(expand_home(Path::new("~"), Some(home)), PathBuf::from("/home/someone"));
        assert_eq!(expand_home(Path::new("/abs/x"), Some(home)), PathBuf::from("/abs/x"));
        assert_eq!(expand_home(Path::new("~user/x"), Some(home)), PathBuf::from("~user/x"), "not a shell");
        assert_eq!(expand_home(Path::new("~/x"), None), PathBuf::from("~/x"), "no home, no expansion");
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

    // --- detection and config writing ---------------------------------------

    #[test]
    fn detect_reads_the_repo_and_environment_it_can() {
        let dir = tmp("detect");
        run_git(&dir, &["init", "-q"]);

        // No remote, no mise/direnv yet: honest fallbacks.
        let d = detect(&dir);
        assert_eq!(d.name, dir.file_name().unwrap().to_string_lossy());
        assert_eq!(d.repo, None, "no remote means no repo to watch");
        assert_eq!(d.env_source, "none");
        assert_eq!(d.worktrees, ".claude/worktrees");
        assert_eq!(d.base_branch, "origin/HEAD", "nothing fetched, so the daemon default");

        // A GitHub origin becomes the repo to watch.
        run_git(&dir, &["remote", "add", "origin", "git@github.com:acme/thing.git"]);
        assert_eq!(detect(&dir).repo.as_deref(), Some("acme/thing"));

        // A mise.toml is read as the environment source.
        std::fs::write(dir.join("mise.toml"), "[tools]\n").unwrap();
        assert_eq!(detect(&dir).env_source, "mise");
    }

    #[test]
    fn detect_offers_a_compose_stack_and_conventional_scripts() {
        let dir = tmp("procs");
        std::fs::write(dir.join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"dev":"vite","build":"vite build","watch":"tsc -w"}}"#,
        )
        .unwrap();

        let procs = detect_processes(&dir);
        let labels: Vec<_> = procs.iter().map(|p| p.label.as_str()).collect();
        assert!(labels.contains(&"docker compose up"), "the compose stack is offered");
        assert!(labels.contains(&"pnpm run dev"), "dev is a long-running script");
        assert!(labels.contains(&"pnpm run watch"), "watch is too");
        assert!(!labels.iter().any(|l| l.contains("build\"")), "one-shot build is not offered");
        assert!(
            !labels.contains(&"pnpm run build"),
            "a plain build is one-shot, not a managed process"
        );
        // The package manager comes from the lockfile.
        assert!(procs.iter().any(|p| p.command == ["pnpm", "run", "dev"]));
    }

    #[test]
    fn write_config_adds_ticked_processes_as_autostart() {
        let base = tmp("wcp");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);
        let cfg = base.join("config.json");

        write_config_to(
            &cfg,
            &repo,
            &Overrides {
                processes: vec![SelectedProcess {
                    name: "docker".into(),
                    command: vec!["docker".into(), "compose".into(), "up".into()],
                }],
                ..Default::default()
            },
        )
        .unwrap();

        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let procs = v["main_processes"].as_array().expect("main_processes written");
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0]["name"], "docker");
        assert_eq!(procs[0]["autostart"], true, "a ticked process starts with the daemon");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn write_config_is_slim_and_validated() {
        let base = tmp("wc");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);
        let cfg = base.join("config.json");

        write_config_to(
            &cfg,
            &repo,
            &Overrides {
                base_branch: Some("upstream/develop".into()),
                repo: Some("acme/thing".into()),
                env_source: Some(crate::config::EnvSourceKind::Direnv),
                tracker: Some(crate::config::TrackerKind::Shortcut),
                ..Default::default()
            },
        )
        .unwrap();

        let written = std::fs::read_to_string(&cfg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["upstream_ref"], "upstream/develop");
        assert_eq!(v["upstream_remote"], "upstream", "the remote is split off the ref");
        assert_eq!(v["repo"], "acme/thing");
        assert_eq!(v["env_source"], "direnv");
        assert_eq!(v["tracker"], "shortcut");
        // Slim: nothing that was not asked for.
        assert!(v.get("poll_seconds").is_none(), "defaults are left out");

        // A bad override never reaches this function at all: the field is typed,
        // so serde refuses the request that carries it. Asserted where the refusal
        // now lives, because a value that cannot be constructed cannot be passed.
        assert!(
            serde_json::from_str::<Overrides>(r#"{"path":"/x","env_source":"nonsense"}"#).is_err(),
            "an unknown env source has to be refused while the request is read"
        );
        assert!(
            serde_json::from_str::<Overrides>(r#"{"path":"/x","tracker":"jira"}"#).is_err(),
            "and an unknown tracker"
        );
        // The spellings the page really sends, so a rename of either enum fails
        // here rather than on somebody's first run.
        let ok: Overrides = serde_json::from_str(
            r#"{"path":"/x","env_source":"mise","tracker":"shortcut"}"#,
        )
        .expect("the page's own values");
        assert_eq!(ok.env_source, Some(crate::config::EnvSourceKind::Mise));
        assert_eq!(ok.tracker, Some(crate::config::TrackerKind::Shortcut));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A write that is then undone puts the previous file back, or removes the new
    /// one when there was none: the two shapes a refused switch can leave behind.
    #[test]
    fn a_written_config_can_be_undone() {
        let base = tmp("wc-undo");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);
        let cfg = base.join("config.json");

        // Nothing before: undo removes.
        let w = write_config_to(&cfg, &repo, &Overrides::default()).unwrap();
        assert!(cfg.exists());
        w.undo();
        assert!(!cfg.exists(), "a first write is undone by removing the file");

        // Something before: undo restores it byte for byte.
        let old = r#"{"main_checkout":"/somewhere/else"}"#;
        std::fs::write(&cfg, old).unwrap();
        let w = write_config_to(&cfg, &repo, &Overrides::default()).unwrap();
        assert_ne!(std::fs::read_to_string(&cfg).unwrap(), old);
        w.undo();
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), old);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// And it puts it back when the `.bak` copy failed, which is the case that
    /// made undoing *destructive*: the backup is best effort, so a failed copy
    /// used to leave the restore with nothing to restore from, and it removed a
    /// config that had content. A directory sitting on the `.bak` path is the
    /// cheapest way to make `fs::copy` fail.
    #[test]
    fn an_undo_survives_a_backup_that_could_not_be_written() {
        let base = tmp("wc-undo-nobak");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);
        let cfg = base.join("config.json");
        let old = r#"{"main_checkout":"/somewhere/else"}"#;
        std::fs::write(&cfg, old).unwrap();
        std::fs::create_dir_all(cfg.with_extension("json.bak")).unwrap();

        let w = write_config_to(&cfg, &repo, &Overrides::default()).unwrap();
        assert_ne!(std::fs::read_to_string(&cfg).unwrap(), old, "the write still happened");
        w.undo();
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            old,
            "the previous config came back rather than being deleted"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Re-picking a checkout keeps every key the review did not answer, and keeps
    /// the previous file beside it. Building the object from scratch lost them all.
    #[test]
    fn write_config_merges_into_the_existing_file_and_keeps_a_backup() {
        let base = tmp("wc-merge");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);
        let cfg = base.join("config.json");
        let old = r#"{"main_checkout":"/somewhere/else","worktree_setup":["mise","trust"],"repo":"acme/thing"}"#;
        std::fs::write(&cfg, old).unwrap();

        write_config_to(&cfg, &repo, &Overrides::default()).unwrap();

        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["main_checkout"], repo.to_string_lossy().as_ref());
        assert_eq!(v["worktree_setup"], serde_json::json!(["mise", "trust"]));
        assert_eq!(v["repo"], "acme/thing", "a key the review did not answer is left alone");
        assert_eq!(
            std::fs::read_to_string(base.join("config.json.bak")).unwrap(),
            old,
            "the previous file is kept"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The review pre-fills `repo` from a remote, so writing it back pinned a
    /// default as an override. Only a repo the remote does not derive is written.
    #[test]
    fn write_config_pins_a_repo_only_when_the_remote_does_not_derive_it() {
        let base = tmp("wc-repo");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["remote", "add", "origin", "git@github.com:acme/thing.git"]);
        let cfg = base.join("config.json");

        let with = |r: &str| Overrides {
            repo: Some(r.into()),
            ..Default::default()
        };
        write_config_to(&cfg, &repo, &with("acme/thing")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(v.get("repo").is_none(), "the derived repo is not pinned: {v}");

        write_config_to(&cfg, &repo, &with("other/name")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["repo"], "other/name");

        // Answering with the derived one again lifts the pin.
        write_config_to(&cfg, &repo, &with("acme/thing")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(v.get("repo").is_none(), "{v}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A fork layout is what a checkout can answer for itself, and it used to be
    /// answered only by the daemon's own first write — which this write pre-empts.
    /// So the detection lives here too, for the review's pre-fill and for a recent
    /// that opens with no review at all.
    #[test]
    fn a_fork_layout_is_detected_by_the_review_and_by_an_unreviewed_open() {
        let base = tmp("wc-fork");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q", "-b", "main"]);
        run_git(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "x"]);
        run_git(&repo, &["remote", "add", "origin", "git@github.com:fork/thing.git"]);
        run_git(&repo, &["remote", "add", "upstream", "git@github.com:acme/thing.git"]);
        run_git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        run_git(&repo, &["update-ref", "refs/remotes/upstream/develop", "HEAD"]);

        let d = detect(&repo);
        assert_eq!(d.base_branch, "upstream/develop", "the upstream's branch, not the fork's");
        assert_eq!(d.repo.as_deref(), Some("acme/thing"), "the upstream's repo, not the fork's");

        let cfg = base.join("config.json");
        write_config_to(&cfg, &repo, &Overrides::default()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["upstream_remote"], "upstream");
        assert!(
            v["upstream_ref"].as_str().unwrap().starts_with("upstream/"),
            "an unreviewed open still measures against upstream: {v}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // --- the bootstrap router ------------------------------------------------

    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use std::sync::Mutex;
    use tower::ServiceExt; // for `oneshot`

    /// A `BootstrapHost` that records opens and answers the dialog with a fixed
    /// path, so the router's contract can be checked without a window.
    #[derive(Default)]
    struct StubHost {
        opened: Mutex<Vec<PathBuf>>,
        pick_result: Option<PathBuf>,
        switching: bool,
        cancelled: Mutex<bool>,
        refuse_open: bool,
    }
    impl BootstrapHost for StubHost {
        fn pick(&self) -> Option<PathBuf> {
            self.pick_result.clone()
        }
        fn open(&self, path: PathBuf) -> bool {
            if self.refuse_open {
                return false;
            }
            self.opened.lock().unwrap().push(path);
            true
        }
        fn window_cmd(&self, _cmd: crate::window::WindowCmd) {}
        fn switching(&self) -> bool {
            self.switching
        }
        fn cancel(&self) {
            *self.cancelled.lock().unwrap() = true;
        }
    }

    /// The port the test router claims to serve on; the helpers send the Host and
    /// Origin a same-origin page would, so the guard lets them through.
    const PORT: u16 = 7777;

    async fn post(host: Arc<dyn BootstrapHost>, uri: &str, body: &str) -> serde_json::Value {
        let res = router(host, PORT)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("host", format!("127.0.0.1:{PORT}"))
                    .header("origin", format!("http://127.0.0.1:{PORT}"))
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
        Arc::new(StubHost::default())
    }

    async fn get(host: Arc<dyn BootstrapHost>, uri: &str) -> serde_json::Value {
        let res = router(host, PORT)
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("host", format!("127.0.0.1:{PORT}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(res.into_body(), 1 << 16).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    /// The bootstrap server is a loopback port like the daemon's, and gets the
    /// daemon's rules: a request from another page — a foreign Host, or a POST
    /// whose Origin is not ours or is missing — is refused before any handler.
    #[tokio::test]
    async fn the_bootstrap_router_refuses_a_foreign_page() {
        let status = |req: Request<Body>| async {
            router(stub(), PORT).oneshot(req).await.unwrap().status()
        };
        let ours = format!("127.0.0.1:{PORT}");

        // A rebound host name, on the innocuous-looking GET.
        let req = Request::builder()
            .uri("/api/recent")
            .header("host", "evil.example:7777")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(req).await, StatusCode::FORBIDDEN);

        // A cross-site POST: body-less, so a "simple request" no browser blocks.
        let req = Request::builder()
            .method("POST")
            .uri("/api/window/restart")
            .header("host", &ours)
            .header("origin", "https://evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(req).await, StatusCode::FORBIDDEN);
        let req = Request::builder()
            .method("POST")
            .uri("/api/pick")
            .header("host", &ours)
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(req).await, StatusCode::FORBIDDEN, "no Origin at all");

        // And the page's own requests still pass.
        assert_eq!(get(stub(), "/api/context").await["switching"], false);
    }

    #[tokio::test]
    async fn validate_route_answers_ok_or_a_message() {
        let base = tmp("router-val");
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
    async fn context_says_whether_it_is_a_switch_and_cancel_reaches_the_host() {
        // First run: not switching, and there is nothing to cancel to.
        assert_eq!(get(stub(), "/api/context").await["switching"], false);

        // A switch: the page is told so, and cancel reaches the host.
        let host = Arc::new(StubHost { switching: true, ..Default::default() });
        assert_eq!(get(host.clone(), "/api/context").await["switching"], true);
        post(host.clone(), "/api/cancel", "").await;
        assert!(*host.cancelled.lock().unwrap(), "cancel returns to the running project");
    }

    #[tokio::test]
    async fn detect_route_returns_the_detected_settings() {
        let dir = tmp("route-detect");
        run_git(&dir, &["init", "-q"]);
        run_git(&dir, &["remote", "add", "origin", "git@github.com:acme/thing.git"]);
        let out = post(stub(), "/api/detect", &format!("{{\"path\":{:?}}}", dir.to_string_lossy())).await;
        assert_eq!(out["repo"], "acme/thing");
        assert_eq!(out["worktrees"], ".claude/worktrees");
        assert!(out["base_branch"].is_string());
    }

    #[tokio::test]
    async fn open_route_validates_then_hands_the_path_to_the_host() {
        let base = tmp("router-open");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);

        let host = stub();
        let out = post(host.clone(), "/api/open", &format!("{{\"path\":{:?}}}", repo.to_string_lossy())).await;
        assert_eq!(out["ok"], true);
        let canonical = std::fs::canonicalize(&repo).unwrap();
        assert_eq!(host.opened.lock().unwrap().as_slice(), std::slice::from_ref(&canonical), "the host was handed the checkout");

        // A host that cannot take the open (a switch with no binary to restart
        // into) is reported as such, not as a success the window never follows.
        let host = Arc::new(StubHost { refuse_open: true, ..Default::default() });
        let out = post(host.clone(), "/api/open", &format!("{{\"path\":{:?}}}", repo.to_string_lossy())).await;
        assert_eq!(out["ok"], false);
        assert!(out["error"].as_str().unwrap().contains("kept"), "{out}");

        // A folder that is not a repo never reaches the host.
        let host = stub();
        let out = post(host.clone(), "/api/open", "{\"path\":\"/no/such/place\"}").await;
        assert_eq!(out["ok"], false);
        assert!(host.opened.lock().unwrap().is_empty(), "an invalid open is refused before the daemon");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn pick_route_distinguishes_cancel_from_a_chosen_folder() {
        let base = tmp("router-pick");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);

        // Cancelled dialog.
        let cancelled = post(stub(), "/api/pick", "").await;
        assert_eq!(cancelled["picked"], false);
        assert_eq!(cancelled["ok"], false);

        // A folder was chosen and it validates.
        let host = Arc::new(StubHost {
            pick_result: Some(repo.clone()),
            ..Default::default()
        });
        let picked = post(host, "/api/pick", "").await;
        assert_eq!(picked["picked"], true);
        assert_eq!(picked["ok"], true);
        assert_eq!(picked["name"], "proj");
        let _ = std::fs::remove_dir_all(&base);
    }
}
