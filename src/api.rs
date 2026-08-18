use anyhow::Context;
use axum::{
    extract::{Path, Query, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

use crate::model::*;
use crate::spawn;
use crate::state::AppState;
use crate::worktree;

pub struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Errors are shown verbatim in the rail: a refused `git worktree remove`
        // is information, not noise to swallow.
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{:#}", self.0) })),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

// ---------------------------------------------------------------------------
// Guards (§12)
// ---------------------------------------------------------------------------

fn host_allowed(host: &str, port: u16) -> bool {
    let expected = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    expected.iter().any(|e| e == host)
}

fn origin_allowed(origin: &str, port: u16) -> bool {
    let expected = [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ];
    expected.iter().any(|e| e == origin)
}

/// Whether a request gets past the Origin check.
///
/// Extracted from [`guard`] because it is the policy, not the plumbing, and it
/// now has four ways to pass — a matrix worth pinning in a test rather than
/// re-deriving from an `axum` handler.
///
/// A **present** Origin must be ours; the three remaining arms are all about the
/// header being absent entirely, which no browser page can arrange:
///
/// - a hook, which comes from a `claude` subprocess and is a write-only observer
///   that can never trigger a spawn, a push or a teardown;
/// - a GET, so a same-origin read from the address bar works;
/// - **anything else carrying a valid token.** This is the vendored prompts'
///   shape: `commands/triage.md` POSTs its proposals with `curl`, which sends no
///   Origin, and this arm's absence meant the one route an agent calls answered
///   403 to the only caller it has. Driving it by hand *with* an Origin header
///   during development is what hid that.
///
///   Safe because Origin is a CSRF control, and CSRF is a browser problem: a page
///   cannot omit the header on a cross-origin fetch or form POST, and it cannot
///   read the token to forge this. Absence is positive evidence of a non-browser
///   caller; the token is what authenticates it.
fn origin_ok(origin: Option<&str>, port: u16, is_hook: bool, is_get: bool, token_ok: bool) -> bool {
    match origin {
        Some(o) => origin_allowed(o, port),
        None => is_hook || is_get || token_ok,
    }
}

/// Reject anything that is not the SPA's own origin.
///
/// Binding to 127.0.0.1 is necessary but not sufficient: any web page you visit
/// can issue requests to it, and the daemon's surface is effectively local code
/// execution (§12).
pub async fn guard(
    State(app): State<Arc<AppState>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let port = app.cfg.port;
    let path = req.uri().path().to_string();
    let headers = req.headers();

    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !host_allowed(host, port) {
        return (StatusCode::FORBIDDEN, "bad host").into_response();
    }

    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    let is_hook = path.starts_with("/hooks/");
    if is_hook {
        tracing::debug!(
            %path,
            session = headers.get("x-orch-session").and_then(|v| v.to_str().ok()).unwrap_or("-"),
            "hook received"
        );
    }

    // Read before the Origin check, because one of its arms depends on it.
    let token_ok = headers
        .get("x-orch-token")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t == app.token);

    let is_get = req.method() == axum::http::Method::GET;
    if !origin_ok(origin, port, is_hook, is_get, token_ok) {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }

    // The token closes the "any local process" hole. Hooks cannot easily carry
    // it, which is why they are confined to a separate prefix and a schema that
    // only ever updates state.
    //
    // GETs are otherwise exempt so a same-origin address-bar read works. That
    // holds while a GET only hands back state the daemon already had; a GET
    // that *spends the GitHub token* on an outbound call is a different
    // proposition, because any local process could then drive authenticated
    // GitHub traffic through the daemon and burn its rate limit. Those carry
    // the token like a mutating route does — the SPA's `get()` already sends it
    // on every request, so this costs the UI nothing.
    // Every GET that reaches GitHub on our credential, not just the first one.
    // This started as a match on `/threads` alone and `/review` was added later
    // without it — so the list is named here, with the rule, rather than left as
    // a suffix nobody notices they have to extend.
    const SPENDS_GITHUB_TOKEN: [&str; 2] = ["/threads", "/review"];
    let spends_github_token =
        path.starts_with("/api/pr/") && SPENDS_GITHUB_TOKEN.iter().any(|s| path.ends_with(s));
    let needs_token = !is_hook && (req.method() != axum::http::Method::GET || spends_github_token);
    if needs_token && !token_ok {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }

    next.run(req).await
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

pub async fn get_state(State(app): State<Arc<AppState>>) -> impl IntoResponse {
    Json(app.snapshot().await)
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct NewSession {
    pub workspace: String,
}

pub async fn new_session(
    State(app): State<Arc<AppState>>,
    Json(body): Json<NewSession>,
) -> ApiResult<serde_json::Value> {
    let id = spawn::spawn_session(&app, &body.workspace, Kind::Interactive, None).await?;
    Ok(Json(json!({ "session": id })))
}

#[derive(Deserialize)]
pub struct NewWorktree {
    /// Absent means let Claude Code name it, which is the common case.
    #[serde(default)]
    pub name: Option<String>,
}

pub async fn new_worktree(
    State(app): State<Arc<AppState>>,
    Json(body): Json<NewWorktree>,
) -> ApiResult<serde_json::Value> {
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let id = spawn::spawn_worktree_session(&app, name).await?;
    Ok(Json(json!({ "session": id })))
}

pub async fn kill_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<serde_json::Value> {
    let handle = {
        let inner = app.inner.read().await;
        inner.sessions.get(&id).and_then(|s| s.pty.clone())
    };
    match handle {
        Some(h) => {
            h.kill()?;
            Ok(Json(json!({ "killed": id })))
        }
        None => Err(ApiError(anyhow::anyhow!("no such session {id}"))),
    }
}

/// Resume an archived session.
///
/// A live worktree resumes trivially — relaunch with cwd set to the recorded
/// path. A torn-down one needs its worktree rebuilt first, which is refused
/// here rather than half-done (§2).
pub async fn resume_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<serde_json::Value> {
    let (workspace, recovery, cwd) = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("no such session {id}"))?;
        (s.workspace.clone(), s.recovery.clone(), s.cwd.clone())
    };
    let path_exists = cwd.exists();

    if matches!(recovery, Some(ArchiveState::TranscriptOnly)) {
        return Err(ApiError(anyhow::anyhow!(
            "session {id} is transcript-only: the branch is gone and the commit is unreachable"
        )));
    }

    // A worktree that was torn down is rebuilt here, at the same absolute path:
    // transcripts are keyed by working directory, so a different path resumes
    // nothing (§2).
    let mut warning = None;
    if !path_exists {
        let Some(ArchiveState::Recoverable {
            name,
            branch,
            head_sha,
        }) = recovery
        else {
            return Err(ApiError(anyhow::anyhow!(
                "session {id} has no recovery record, so its worktree cannot be rebuilt"
            )));
        };
        let main = app.cfg.main_checkout.clone();
        let (path, b, sha) = (cwd.clone(), branch.clone(), head_sha.clone());
        let moved = tokio::task::spawn_blocking(move || {
            crate::git::worktree_rebuild(&main, &path, &b, &sha)
        })
        .await
        .map_err(|e| anyhow::anyhow!("rebuild task failed: {e}"))??;

        app.register_worktree(&name, cwd.clone(), Some(branch.clone()))
            .await;

        // The conversation happened on a different commit than the one now
        // checked out, so it describes files that are not there. Said rather than
        // refused: the branch moving on is the normal case for work that landed.
        if let Some(tip) = moved {
            warning = Some(format!(
                "{name} moved since this conversation: recorded {}, branch now {}",
                &head_sha[..head_sha.len().min(7)],
                &tip[..tip.len().min(7)]
            ));
        }
    }

    let new_id = spawn::spawn_session(&app, &workspace, Kind::Interactive, Some(id)).await?;
    Ok(Json(json!({ "session": new_id, "warning": warning })))
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

pub async fn new_shell(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    let id = spawn::spawn_shell(&app, &workspace).await?;
    Ok(Json(json!({ "process": id })))
}

pub async fn restart_process(
    State(app): State<Arc<AppState>>,
    Path((workspace, name)): Path<(String, String)>,
) -> ApiResult<serde_json::Value> {
    let spec = if workspace == MAIN {
        app.cfg
            .main_processes
            .iter()
            .find(|s| s.name == name)
            .cloned()
    } else {
        app.cfg
            .worktree_processes
            .iter()
            .find(|s| s.name == name)
            .cloned()
    };
    let spec = spec.ok_or_else(|| anyhow::anyhow!("no managed process {name} for {workspace}"))?;

    let existing = {
        let inner = app.inner.read().await;
        inner.workspaces.get(&workspace).and_then(|w| {
            w.processes
                .iter()
                .find(|p| p.name == name)
                .and_then(|p| p.pty.clone())
        })
    };
    if let Some(h) = existing {
        let _ = h.kill();
    }

    let id = spawn::start_managed(&app, &workspace, &spec).await?;
    Ok(Json(json!({ "process": id })))
}

/// Drive the window the daemon is displayed in.
///
/// A mutating route, so it carries the token like every other one — which is
/// the point of doing this over HTTP instead of Tauri's IPC: the SPA's origin
/// is a localhost URL on a port chosen at bind time, and granting IPC to
/// `http://127.0.0.1:*` would hand window control to anything else that
/// managed to get itself loaded there.
pub async fn window_cmd(
    State(app): State<Arc<AppState>>,
    Path(cmd): Path<String>,
) -> ApiResult<serde_json::Value> {
    let cmd: crate::window::WindowCmd = serde_json::from_value(json!(cmd))
        .map_err(|_| ApiError(anyhow::anyhow!("no such window command: {cmd}")))?;
    dispatch_window(&app, cmd).await
}

/// Resize takes an edge, so it gets its own route rather than bending the
/// command enum into something that serialises from a single word.
pub async fn window_resize(
    State(app): State<Arc<AppState>>,
    Path(edge): Path<String>,
) -> ApiResult<serde_json::Value> {
    let edge: crate::window::ResizeEdge = serde_json::from_value(json!(edge))
        .map_err(|_| ApiError(anyhow::anyhow!("no such window edge: {edge}")))?;
    dispatch_window(&app, crate::window::WindowCmd::StartResize(edge)).await
}

async fn dispatch_window(
    app: &Arc<AppState>,
    cmd: crate::window::WindowCmd,
) -> ApiResult<serde_json::Value> {
    let control = app.window.read().await.clone();
    let Some(control) = control else {
        // Running in a browser tab. The tab has its own chrome; this is not an
        // error worth a toast, but it is not a success either.
        return Err(ApiError(anyhow::anyhow!("no native window attached")));
    };
    control.dispatch(cmd)?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn close_process(
    State(app): State<Arc<AppState>>,
    Path(proc_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    let mut inner = app.inner.write().await;
    for w in inner.workspaces.values_mut() {
        if let Some(p) = w.processes.iter().find(|p| p.id == proc_id) {
            if let Some(h) = &p.pty {
                let _ = h.kill();
            }
        }
        w.processes.retain(|p| p.id != proc_id);
    }
    drop(inner);
    app.notify().await;
    Ok(Json(json!({ "closed": proc_id })))
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

pub async fn reconcile(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    app.reconcile(&workspace).await?;
    app.notify().await;
    Ok(Json(json!({ "reconciled": workspace })))
}

pub async fn preflight(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<worktree::Preflight> {
    Ok(Json(worktree::preflight(&app, &workspace).await?))
}

pub async fn archive_workspace(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    worktree::archive(&app, &workspace).await?;
    Ok(Json(json!({ "archived": workspace })))
}

pub async fn teardown(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<worktree::Preflight> {
    Ok(Json(worktree::teardown(&app, &workspace).await?))
}

// ---------------------------------------------------------------------------
// Diff base, for the context bar
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct BaseQuery {
    #[serde(default)]
    pub workspace: Option<String>,
}

pub async fn merge_base(
    State(app): State<Arc<AppState>>,
    Query(q): Query<BaseQuery>,
) -> ApiResult<serde_json::Value> {
    let ws = q.workspace.unwrap_or_else(|| MAIN.to_string());
    let path = app
        .workspace_path(&ws)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {ws}"))?;
    let base = crate::git::merge_base(&path, &app.cfg.upstream_ref)?;
    let branch = crate::git::current_branch(&path).unwrap_or_default();
    Ok(Json(json!({
        "workspace": ws,
        "branch": branch,
        "upstream": app.cfg.upstream_ref,
        "merge_base": base,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_spas_own_origin_is_accepted() {
        assert!(origin_allowed("http://127.0.0.1:7777", 7777));
        assert!(origin_allowed("http://localhost:7777", 7777));
        assert!(!origin_allowed("http://evil.example", 7777));
        // A page on another port is still another origin.
        assert!(!origin_allowed("http://127.0.0.1:7778", 7777));
        // Guards against a DNS-rebinding host that merely contains the address.
        assert!(!origin_allowed("http://127.0.0.1.evil.example:7777", 7777));
    }

    /// `(origin, is_hook, is_get, token_ok)` at port 7777.
    fn ok(origin: Option<&str>, is_hook: bool, is_get: bool, token_ok: bool) -> bool {
        origin_ok(origin, 7777, is_hook, is_get, token_ok)
    }

    #[test]
    fn a_present_origin_must_be_ours_whatever_else_is_true() {
        assert!(ok(Some("http://127.0.0.1:7777"), false, false, true));
        // A token does not buy a pass for a page on another origin: that is
        // exactly the request the check exists to stop.
        assert!(!ok(Some("http://evil.example"), false, false, true));
        assert!(!ok(Some("http://evil.example"), true, true, true));
    }

    #[test]
    fn a_tokened_post_with_no_origin_is_the_agents_own_shape() {
        // `commands/triage.md` POSTs with curl, which sends no Origin. Without
        // this arm the one route an agent calls answered 403 to its only caller.
        assert!(ok(None, false, false, true));
        // Still nothing without the token.
        assert!(!ok(None, false, false, false));
    }

    #[test]
    fn a_no_origin_get_or_hook_still_passes_untokened() {
        assert!(ok(None, false, true, false));
        assert!(ok(None, true, false, false));
    }

    #[test]
    fn host_must_be_loopback() {
        assert!(host_allowed("127.0.0.1:7777", 7777));
        assert!(host_allowed("localhost:7777", 7777));
        assert!(!host_allowed("evil.example:7777", 7777));
        assert!(!host_allowed("127.0.0.1", 7777));
    }
}

// ---------------------------------------------------------------------------
// Diff (§5)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct DiffQuery {
    pub workspace: String,
    #[serde(default)]
    pub base: crate::diff::Base,
    #[serde(default)]
    pub pr_base: Option<String>,
}

async fn base_for(
    app: &Arc<AppState>,
    q: &DiffQuery,
) -> Result<(std::path::PathBuf, String), ApiError> {
    let path = app
        .workspace_path(&q.workspace)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {}", q.workspace))?;
    let base =
        crate::diff::resolve_base(&path, q.base, &app.cfg.upstream_ref, q.pr_base.as_deref())?;
    Ok((path, base))
}

pub async fn diff_summary(
    State(app): State<Arc<AppState>>,
    Query(q): Query<DiffQuery>,
) -> ApiResult<crate::diff::DiffSummary> {
    let (path, base) = base_for(&app, &q).await?;
    Ok(Json(crate::diff::summary(&path, &base)?))
}

#[derive(Deserialize)]
pub struct FileDiffQuery {
    pub workspace: String,
    pub path: String,
    #[serde(default)]
    pub base: crate::diff::Base,
    #[serde(default)]
    pub pr_base: Option<String>,
    /// Widening this is how expand-on-click is served.
    #[serde(default = "default_context")]
    pub context: u32,
}

fn default_context() -> u32 {
    3
}

pub async fn diff_file(
    State(app): State<Arc<AppState>>,
    Query(q): Query<FileDiffQuery>,
) -> ApiResult<crate::diff::FileDiff> {
    let dq = DiffQuery {
        workspace: q.workspace.clone(),
        base: q.base,
        pr_base: q.pr_base.clone(),
    };
    let (path, base) = base_for(&app, &dq).await?;
    // A pathological context value would ask git for the whole repo.
    let context = q.context.min(10_000);
    Ok(Json(crate::diff::file_diff(
        &path, &base, &q.path, context,
    )?))
}

// ---------------------------------------------------------------------------
// Review queue
// ---------------------------------------------------------------------------

/// Ask the review poller to fetch now. It owns the fetch and the state write, so
/// this only pulses it: `notify_one` stores a permit even if a poll is mid-flight,
/// so a press during a fetch still forces one more right after.
pub async fn refresh_reviews(State(app): State<Arc<AppState>>) -> impl IntoResponse {
    app.review_refresh.notify_one();
    (StatusCode::ACCEPTED, Json(json!({ "refreshing": true })))
}

/// Ask the PR poller to fetch now, the same way `refresh_reviews` does.
pub async fn refresh_prs(State(app): State<Arc<AppState>>) -> impl IntoResponse {
    app.pr_refresh.notify_one();
    (StatusCode::ACCEPTED, Json(json!({ "refreshing": true })))
}

#[derive(Deserialize)]
pub struct OpenUrl {
    pub url: String,
}

/// Open an external URL in the OS browser.
///
/// The desktop webview wires no IPC and no shell, so a `target="_blank"` link
/// goes nowhere inside it — under WSLg especially. The SPA routes those clicks
/// here instead, and the daemon (which is a local process) hands the URL to the
/// platform opener. Only `http(s)` is accepted, so this can never be coaxed into
/// launching a local file or a `mailto:`/`file:` handler.
pub async fn open_url(
    State(_app): State<Arc<AppState>>,
    Json(body): Json<OpenUrl>,
) -> ApiResult<serde_json::Value> {
    let url = body.url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(ApiError(anyhow::anyhow!("refusing to open non-http URL")));
    }
    open_external(url)?;
    Ok(Json(json!({ "opened": url })))
}

/// Hand a URL to the platform browser opener, detached.
///
/// On WSL the standard `xdg-open` often resolves to a portal that silently does
/// nothing, so `wslview` (which reaches the Windows default browser) is tried
/// first there. Elsewhere the usual Linux openers are tried in turn.
fn open_external(url: &str) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};

    let is_wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/version")
            .map(|v| v.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false);

    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["open"]
    } else if is_wsl {
        &["wslview", "xdg-open", "x-www-browser", "sensible-browser"]
    } else {
        &["xdg-open", "x-www-browser", "sensible-browser"]
    };

    for cmd in candidates {
        // Skip openers that are not installed rather than spawning one that
        // "succeeds" but does nothing, which would stop the fallthrough.
        let present = Command::new("which")
            .arg(cmd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !present {
            continue;
        }
        Command::new(cmd)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning {cmd}"))?;
        return Ok(());
    }
    anyhow::bail!("no browser opener found (tried {})", candidates.join(", "))
}

/// Every review thread on a PR, with bodies.
///
/// Deliberately not part of the 5-minute poll: that needs a count, and pulling
/// every comment on every open PR to get one would be a large query on a short
/// timer (§6). This runs when the review overlay opens, for a single PR, and
/// pages past 50 so the `50+` the rail shows becomes a real number here.
///
/// Always refetches. The cache it writes exists so the later post step can
/// check the `head_sha` it replied against, not to serve reads — a stale thread
/// list is exactly the thing this endpoint must never hand back.
pub async fn pr_threads(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let (owner, name) = crate::resolve_repo(&app)
        .ok_or_else(|| anyhow::anyhow!("no GitHub repo configured and none on the remote"))?;
    let token = crate::github::resolve_token(app.cfg.github_token_file.as_deref())?;

    // `github::threads` shells curl, so it must not run on the async runtime.
    let fetched = tokio::task::spawn_blocking(move || {
        crate::github::threads(&token.value, &owner, &name, number)
    })
    .await
    .context("the thread fetch panicked")??;

    let body = json!({
        "viewer": fetched.viewer,
        "head_sha": fetched.head_sha,
        "answerable": fetched.answerable_count(),
        "threads": fetched.items,
    });

    {
        let mut inner = app.inner.write().await;
        inner.threads.insert(
            number,
            crate::state::ThreadCache {
                fetched: std::time::SystemTime::now(),
                threads: fetched,
            },
        );
    }
    Ok(Json(body))
}

// ---------------------------------------------------------------------------
// Review overlay
// ---------------------------------------------------------------------------

/// Fetch a PR's threads, cache them, and hand back the parsed set.
///
/// Shared by the endpoints that need to know what is awaiting an answer *now*
/// rather than what a cache said earlier. Always refetches: a stale thread list
/// is the one thing this flow must never act on.
async fn fetch_threads(app: &Arc<AppState>, pr: u64) -> Result<crate::github::Threads, ApiError> {
    let (owner, name) = crate::resolve_repo(app)
        .ok_or_else(|| anyhow::anyhow!("no GitHub repo configured and none on the remote"))?;
    let token = crate::github::resolve_token(app.cfg.github_token_file.as_deref())?;
    let fetched = tokio::task::spawn_blocking(move || {
        crate::github::threads(&token.value, &owner, &name, pr)
    })
    .await
    .context("the thread fetch panicked")??;
    let mut inner = app.inner.write().await;
    inner.threads.insert(
        pr,
        crate::state::ThreadCache {
            fetched: std::time::SystemTime::now(),
            threads: fetched.clone(),
        },
    );
    Ok(fetched)
}

/// The workspace already holding this PR's head branch, if any.
///
/// Read-only: unlike `ensure_pr_worktree` this never creates one, so the gate can
/// be reported without a side effect.
pub(crate) async fn workspace_for(app: &Arc<AppState>, head_ref: &str) -> Option<String> {
    let inner = app.inner.read().await;
    inner
        .workspaces
        .values()
        .find(|w| w.branches.iter().any(|b| b == head_ref))
        .map(|w| w.id.clone())
}

fn pr_from_poll(prs: &[crate::github::Pr], number: u64) -> Result<crate::github::Pr, ApiError> {
    prs.iter()
        .find(|p| p.number == number)
        .cloned()
        .ok_or_else(|| ApiError(anyhow::anyhow!("PR #{number} is not in the current poll")))
}

/// Start a triage run.
///
/// Refuses on the worktree gates rather than starting a run whose output could
/// not be applied. The threads are fetched first so the viewer login is current
/// and the run has something to triage.
pub async fn pr_triage(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let fetched = fetch_threads(&app, number).await?;
    if fetched.answerable_count() == 0 {
        return Err(ApiError(anyhow::anyhow!(
            "PR #{number} has no threads awaiting an answer"
        )));
    }
    let session = crate::triage::spawn(&app, number, &pr.head_ref, &fetched.viewer).await?;
    Ok(Json(json!({ "session": session })))
}

/// The agent's hand-off. **The only endpoint a subprocess calls.**
///
/// Treated as hostile input: the agent's own input includes review comments other
/// people wrote, so nothing here is trusted for having parsed. The shape is
/// checked by `ProposalSet::validate`, the thread ids against a **fresh** fetch,
/// and `base_sha` against the branch as it stands — a force-push during the run
/// invalidates every patch it produced.
pub async fn pr_proposals(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(body): Json<crate::proposal::ProposalSet>,
) -> ApiResult<serde_json::Value> {
    let fetched = fetch_threads(&app, number).await?;

    if let Some(head) = &fetched.head_sha {
        if head != &body.base_sha {
            return Err(ApiError(anyhow::anyhow!(
                "the branch moved during triage ({} → {head}); its patches no longer apply",
                &body.base_sha[..body.base_sha.len().min(7)]
            )));
        }
    }

    let answerable: Vec<String> = fetched
        .items
        .iter()
        .filter(|t| t.answerable)
        .map(|t| t.id.clone())
        .collect();
    let validated = body.validate(&answerable)?;
    let count = validated.proposals.len();

    app.inner.write().await.proposals.insert(number, validated);
    app.notify().await;
    Ok(Json(json!({ "accepted": count })))
}

/// Everything the overlay needs in one call: the threads, what triage proposed,
/// and whether the worktree can be written to.
pub async fn pr_review(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let fetched = fetch_threads(&app, number).await?;

    let gate = match workspace_for(&app, &pr.head_ref).await {
        Some(ws) => crate::triage::gate(&app, number, &ws).await?,
        // No worktree yet means nothing to be dirty; triage creates one.
        None => None,
    };
    let (proposals, manual) = {
        let inner = app.inner.read().await;
        (
            inner.proposals.get(&number).cloned(),
            inner.manual.get(&number).cloned(),
        )
    };

    Ok(Json(json!({
        "viewer": fetched.viewer,
        "head_sha": fetched.head_sha,
        "answerable": fetched.answerable_count(),
        "threads": fetched.items,
        "proposals": proposals,
        // A batch that stopped for the manual phase. Served so a reload or a restart
        // resumes it rather than stranding a branch whose patches are committed.
        "manual": manual,
        "gate": gate,
        // Shown in the header, never gating: a red or conflicting PR is still
        // answerable, and `/green` is offered rather than required.
        "checks": pr.checks,
        "mergeable": pr.mergeable,
        // Whether a `story+reply` position can be acted on at all. The overlay
        // should not be welded to Shortcut, so with no tracker it hides the
        // option rather than offering something that would be refused.
        "tracker": app.cfg.tracker.is_configured(),
    })))
}

#[derive(Deserialize)]
pub struct CommitBody {
    pub message: String,
}

/// The gate's `commit…` button: commit the worktree as it stands.
pub async fn pr_commit(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(body): Json<CommitBody>,
) -> ApiResult<serde_json::Value> {
    let path = gate_worktree(&app, number).await?;
    crate::git::commit_all(&path, &body.message)?;
    app.notify().await;
    Ok(Json(json!({ "committed": true })))
}

/// The gate's `stash` button. Never popped automatically — see `git::stash`.
pub async fn pr_stash(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let path = gate_worktree(&app, number).await?;
    crate::git::stash(&path)?;
    app.notify().await;
    Ok(Json(json!({ "stashed": true })))
}

/// The batch — the one irreversible action.
///
/// The threads are refetched here rather than read from the cache, and that fetch
/// does four jobs at once: the head-sha staleness check, the comment ids the
/// writes are aimed at, which replies are already posted, and which threads are
/// still open for the re-request pass. See `post::run` for the order.
///
/// A refusal from the local half comes back **200 with `refused` set**, not as an
/// error: nothing was written, and the overlay renders it as a panel with the
/// decisions still staged rather than as a failed request.
pub async fn pr_post(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(batch): Json<crate::post::Batch>,
) -> ApiResult<crate::post::PostReport> {
    // One batch per PR at a time, refused rather than queued — same shape as
    // `green_pr`'s `branch_busy`.
    //
    // This was invisible while every write was idempotent: two concurrent batches
    // would post the same reply twice and GitHub would collapse the reaction. A
    // story is neither. Both would search, both would find nothing, both would
    // create — so the check-then-file is a plain race on the one action that cannot
    // be undone.
    let lock = format!("post:{number}");
    {
        let mut inner = app.inner.write().await;
        if inner.locks_held.iter().any(|l| l == &lock) {
            return Err(ApiError(anyhow::anyhow!(
                "a batch for PR #{number} is already running; wait for it rather than \
                 sending a second one"
            )));
        }
        inner.locks_held.push(lock.clone());
    }
    // Held for the whole batch and released however it ends, including a panic in
    // the middle: a leaked lock would make the PR unpostable until a restart.
    let released = Released {
        app: app.clone(),
        lock,
    };

    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let fresh = fetch_threads(&app, number).await?;
    let report = crate::post::run(&app, &pr, &fresh, batch).await?;
    drop(released);
    app.notify().await;
    Ok(Json(report))
}

/// What you have edited since the manual phase opened.
///
/// The tree against `HEAD`, which after the phase's own commit is exactly your
/// hand-written work and nothing else — the ordering is what makes that true. Its
/// own endpoint rather than the diff viewer's, because the phase wants one refresh
/// call returning both the file list and the patch text, and because `git diff`
/// being the source is the point: nobody declared these files, so the list cannot
/// be wrong about them.
pub async fn pr_manual(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let path = gate_worktree(&app, number).await?;
    let (files, diff) = tokio::task::spawn_blocking(move || crate::patch::worktree_change(&path))
        .await
        .context("reading the worktree diff panicked")??;
    Ok(Json(json!({ "files": files, "diff": diff })))
}

/// Finish a batch that stopped for the manual phase.
///
/// The same lock and the same pipeline as `/post`; the difference is that the local
/// half folds *your* edits rather than applying a patch, and the clean-tree gate
/// stands down because the phase is what asked you to make it dirty.
pub async fn pr_manual_done(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(done): Json<crate::post::Finish>,
) -> ApiResult<crate::post::PostReport> {
    let lock = format!("post:{number}");
    {
        let mut inner = app.inner.write().await;
        if inner.locks_held.iter().any(|l| l == &lock) {
            return Err(ApiError(anyhow::anyhow!(
                "a batch for PR #{number} is already running; wait for it rather than \
                 sending a second one"
            )));
        }
        inner.locks_held.push(lock.clone());
    }
    let released = Released {
        app: app.clone(),
        lock,
    };

    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let fresh = fetch_threads(&app, number).await?;
    let report = crate::post::finish(&app, &pr, &fresh, done).await?;
    drop(released);
    app.notify().await;
    Ok(Json(report))
}

/// The rail's default review action: spawn a session and run `/resolve <pr>`.
///
/// The robust path. The agent does the work in a pane you supervise; the daemon
/// itself makes no irreversible write. The native overlay (`/triage` → cards →
/// `/post`) is the opt-in alternative, chosen from the same rail row.
pub async fn resolve_pr(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        inner.prs.iter().find(|p| p.number == number).cloned()
    };
    let pr = pr.ok_or_else(|| anyhow::anyhow!("PR #{number} is not in the current poll"))?;
    if pr.unresolved == 0 && !pr.unresolved_capped && !pr.changes_requested {
        return Err(ApiError(anyhow::anyhow!(
            "PR #{number} has no unresolved review threads"
        )));
    }
    let id = spawn::spawn_command_session(&app, number, &pr.head_ref, "resolve").await?;
    Ok(Json(json!({ "session": id })))
}

/// Releases a lock in `locks_held` when it goes out of scope.
///
/// A guard rather than a `let _ = ...` at each exit, because `pr_post` has several
/// `?` between taking the lock and finishing, and every one of them would otherwise
/// leak it.
struct Released {
    app: Arc<AppState>,
    lock: String,
}

impl Drop for Released {
    fn drop(&mut self) {
        let (app, lock) = (self.app.clone(), self.lock.clone());
        // `Drop` cannot await, so the release is spawned. It is the last thing
        // touching this lock, so nothing races it.
        tokio::spawn(async move {
            let mut inner = app.inner.write().await;
            inner.locks_held.retain(|l| l != &lock);
        });
    }
}

/// The worktree the gate buttons act on, refusing when there is not one.
async fn gate_worktree(app: &Arc<AppState>, number: u64) -> Result<std::path::PathBuf, ApiError> {
    let head_ref = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?.head_ref
    };
    let ws = workspace_for(app, &head_ref)
        .await
        .ok_or_else(|| anyhow::anyhow!("no worktree for PR #{number} yet"))?;
    app.workspace_path(&ws)
        .await
        .ok_or_else(|| ApiError(anyhow::anyhow!("the worktree for PR #{number} vanished")))
}

// ---------------------------------------------------------------------------
// Test capabilities (§7) — reporting only
// ---------------------------------------------------------------------------

/// What could be run here and how far it can be trusted.
///
/// Nothing acts on this: §10 puts `/green` behind step 8 being correct for a
/// week on real PRs, so the registry reports and stops.
pub async fn capabilities(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<crate::capability::CapabilityReport> {
    let (path, is_main) = {
        let inner = app.inner.read().await;
        let w = inner
            .workspaces
            .get(&workspace)
            .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?;
        (w.path.clone(), w.is_main())
    };
    let cfg = app.cfg.capabilities.clone();
    let main = app.cfg.main_checkout.clone();
    let ws = workspace.clone();
    // Shells out to php, so it does not belong on the async runtime.
    let report = tokio::task::spawn_blocking(move || {
        crate::capability::report(&cfg, &ws, &path, &main, is_main)
    })
    .await
    .map_err(|e| anyhow::anyhow!("capability probe failed: {e}"))?;
    Ok(Json(report))
}

// ---------------------------------------------------------------------------
// Editable right pane (§5, step 9)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct FileQuery {
    pub workspace: String,
    pub path: String,
    /// Read the file as it was at this base instead of from the working tree.
    /// Used for the read-only left pane, and never writable.
    #[serde(default)]
    pub base: Option<crate::diff::Base>,
    #[serde(default)]
    pub pr_base: Option<String>,
}

pub async fn read_file(
    State(app): State<Arc<AppState>>,
    Query(q): Query<FileQuery>,
) -> ApiResult<crate::edit::FileContents> {
    let root = app
        .workspace_path(&q.workspace)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {}", q.workspace))?;

    if let Some(base) = q.base {
        let rev =
            crate::diff::resolve_base(&root, base, &app.cfg.upstream_ref, q.pr_base.as_deref())?;
        let content = crate::diff::show_at(&root, &rev, &q.path)?;
        return Ok(Json(crate::edit::FileContents {
            path: q.path.clone(),
            bytes: content.len() as u64,
            // No version: a historical revision is never written back.
            version: String::new(),
            content,
        }));
    }
    Ok(Json(crate::edit::read(&root, &q.path)?))
}

#[derive(Deserialize)]
pub struct WriteBody {
    pub workspace: String,
    pub path: String,
    pub content: String,
    /// The version the buffer was loaded at. A mismatch means an agent edited
    /// the file underneath you, and the write is refused (§5).
    pub version: String,
}

pub async fn write_file(
    State(app): State<Arc<AppState>>,
    Json(body): Json<WriteBody>,
) -> ApiResult<crate::edit::WriteOutcome> {
    let root = app
        .workspace_path(&body.workspace)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {}", body.workspace))?;
    let out = crate::edit::write(&root, &body.path, &body.content, &body.version)?;
    if matches!(out, crate::edit::WriteOutcome::Written { .. }) {
        // Agents working in this workspace hold a stale copy now, and will
        // overwrite it unless they are told (§5's invalidation, in the
        // direction that actually loses work).
        let resolved = crate::edit::resolve_in_workspace(&root, &body.path)?;
        app.record_human_edit(resolved).await;
        // The changed-file pane and the diff must both reflect the write.
        let _ = app.reconcile(&body.workspace).await;
        app.notify().await;
    }
    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// /green (§8) — hand-triggered only
// ---------------------------------------------------------------------------

/// Start a `/green` run for a PR.
///
/// Deliberately **not** automatic. §8 fires it on a PR going red; running it by
/// hand instead means the guard table is a gate you read rather than one that
/// trips behind you, and it is the difference between a tool that helps and one
/// that rebases your branches while you are looking elsewhere.
pub async fn green_pr(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    use crate::green::{evaluate, GuardInput, PrAutomation, Verdict};

    let pr = {
        let inner = app.inner.read().await;
        inner.prs.iter().find(|p| p.number == number).cloned()
    }
    .ok_or_else(|| anyhow::anyhow!("PR #{number} is not in the current poll"))?;

    // The worktree has to exist before its capabilities can be probed, and the
    // probe is what decides whether the run may happen at all.
    let workspace = spawn::ensure_pr_worktree(&app, number, &pr.head_ref).await?;
    let path = app
        .workspace_path(&workspace)
        .await
        .ok_or_else(|| anyhow::anyhow!("worktree for #{number} vanished"))?;

    let cfg = app.cfg.capabilities.clone();
    let main = app.cfg.main_checkout.clone();
    let ws = workspace.clone();
    let capability = tokio::task::spawn_blocking(move || {
        crate::capability::report(&cfg, &ws, &path, &main, false)
    })
    .await
    .map_err(|e| anyhow::anyhow!("capability probe failed: {e}"))?;

    let verdict = {
        let inner = app.inner.read().await;
        let branch_busy = inner
            .sessions
            .values()
            .any(|s| s.workspace == workspace && s.state.is_busy());
        let running_automations = inner
            .sessions
            .values()
            .filter(|s| s.is_automation() && s.state.is_live())
            .count();
        let live_claude_processes = inner
            .sessions
            .values()
            .filter(|s| s.state.is_live())
            .count();
        let main_occupant = inner.workspaces.get(MAIN).and_then(|w| w.occupant);
        let locks = inner.locks_held.clone();

        evaluate(&GuardInput {
            pr: &pr,
            automation: inner.automation.get(number),
            capability: &capability,
            viewer: pr.head_owner.as_deref().unwrap_or_default(),
            branch_busy,
            running_automations,
            live_claude_processes,
            main_occupant,
            locks_held: &locks,
        })
    };

    let locks = match verdict {
        Verdict::Go { locks } => locks,
        Verdict::No { reason } => return Err(ApiError(anyhow::anyhow!("{reason}"))),
    };

    // `{{LOGIN}}` is you: /green refuses a head branch that is not yours.
    let login = {
        let inner = app.inner.read().await;
        inner.viewer.clone()
    }
    .ok_or_else(|| anyhow::anyhow!("no GitHub login yet — the PR poller has not run"))?;
    let session = spawn::spawn_green_session(&app, number, &pr.head_ref, &login).await?;
    {
        let mut inner = app.inner.write().await;
        inner.automation.by_pr.insert(
            number,
            PrAutomation::Running {
                session,
                started: std::time::SystemTime::now(),
            },
        );
        inner.locks_held.extend(locks.iter().cloned());
        let _ = crate::store::save_automation(&inner.automation);
    }

    // Releasing the locks and recording exhaustion belongs to whoever sees the
    // session end.
    watch_green(app.clone(), number, session, locks);
    app.notify().await;
    Ok(Json(json!({ "session": session })))
}

fn watch_green(app: Arc<AppState>, number: u64, session: uuid::Uuid, locks: Vec<String>) {
    use crate::green::{ended_red, PrAutomation};
    tokio::spawn(async move {
        let handle = {
            let inner = app.inner.read().await;
            inner.sessions.get(&session).and_then(|s| s.pty.clone())
        };
        if let Some(h) = handle {
            h.wait().await;
        }

        let mut inner = app.inner.write().await;
        inner.locks_held.retain(|l| !locks.contains(l));

        // A run that ends with the PR still red means the run is asking for
        // you. Record it and never re-fire on its own (§8).
        let pr = inner.prs.iter().find(|p| p.number == number).cloned();
        match pr {
            Some(pr) if ended_red(&pr) => {
                inner.automation.by_pr.insert(
                    number,
                    PrAutomation::Exhausted {
                        at_head: pr.head_sha.clone().unwrap_or_default(),
                        at: std::time::SystemTime::now(),
                    },
                );
            }
            // Green, or the PR is gone from the poll: nothing to remember.
            _ => {
                inner.automation.by_pr.remove(&number);
            }
        }
        let _ = crate::store::save_automation(&inner.automation);
        drop(inner);
        app.notify().await;
    });
}

// ---------------------------------------------------------------------------
// Rebase onto the upstream base
// ---------------------------------------------------------------------------

/// Take in `upstream/develop` by rebasing, never merging: history stays linear.
///
/// Refuses rather than half-doing it — a dirty tree or a working session would
/// both turn a one-click rebase into a mess someone has to unpick.
pub async fn rebase(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    let path = app
        .workspace_path(&workspace)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?;

    if crate::git::rebase_in_progress(&path) {
        return Err(ApiError(anyhow::anyhow!(
            "a rebase is already stopped part-way here; finish or abort it first"
        )));
    }
    if !crate::git::is_clean(&path).unwrap_or(false) {
        return Err(ApiError(anyhow::anyhow!(
            "uncommitted changes — commit or stash before rebasing"
        )));
    }
    {
        let inner = app.inner.read().await;
        if inner
            .sessions
            .values()
            .any(|s| s.workspace == workspace && s.state.is_busy())
        {
            return Err(ApiError(anyhow::anyhow!(
                "a session is working here; rebasing under it would fight it"
            )));
        }
    }

    // Refresh the base first, or "behind" is answered from a stale ref.
    let _ = crate::git::fetch_upstream(&app.cfg.main_checkout);

    let upstream = app.cfg.upstream_ref.clone();
    let p = path.clone();
    let result = tokio::task::spawn_blocking(move || crate::git::rebase_onto(&p, &upstream)).await;

    let _ = app.reconcile(&workspace).await;
    app.notify().await;

    match result {
        Ok(Ok(())) => Ok(Json(json!({ "rebased": workspace }))),
        Ok(Err(e)) => Err(ApiError(e)),
        Err(e) => Err(ApiError(anyhow::anyhow!("rebase task failed: {e}"))),
    }
}

pub async fn rebase_abort(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    let path = app
        .workspace_path(&workspace)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?;
    crate::git::rebase_abort(&path)?;
    let _ = app.reconcile(&workspace).await;
    app.notify().await;
    Ok(Json(json!({ "aborted": workspace })))
}
