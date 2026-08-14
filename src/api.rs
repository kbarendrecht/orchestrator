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

    let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
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

    match origin {
        Some(o) if origin_allowed(o, port) => {}
        // Hook endpoints are the exception: they come from `claude`
        // subprocesses that send no Origin. They are write-only observers that
        // can never trigger a spawn, a push, or a teardown.
        None if is_hook => {}
        // A same-origin GET from the browser address bar also carries no
        // Origin; only mutating routes require it.
        None if req.method() == axum::http::Method::GET => {}
        _ => return (StatusCode::FORBIDDEN, "bad origin").into_response(),
    }

    // The token closes the "any local process" hole. Hooks cannot easily carry
    // it, which is why they are confined to a separate prefix and a schema that
    // only ever updates state.
    let needs_token = !is_hook && req.method() != axum::http::Method::GET;
    if needs_token {
        let presented = headers
            .get("x-orch-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if presented != app.token {
            return (StatusCode::UNAUTHORIZED, "bad token").into_response();
        }
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
    pub name: String,
}

pub async fn new_worktree(
    State(app): State<Arc<AppState>>,
    Json(body): Json<NewWorktree>,
) -> ApiResult<serde_json::Value> {
    let id = spawn::spawn_worktree_session(&app, &body.name).await?;
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
    let (workspace, recovery, path_exists) = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("no such session {id}"))?;
        (
            s.workspace.clone(),
            s.recovery.clone(),
            s.cwd.exists(),
        )
    };

    if matches!(recovery, Some(ArchiveState::TranscriptOnly)) {
        return Err(ApiError(anyhow::anyhow!(
            "session {id} is transcript-only: the branch is gone and the commit is unreachable"
        )));
    }
    if !path_exists {
        return Err(ApiError(anyhow::anyhow!(
            "the worktree for session {id} no longer exists; rebuilding it for resume is not implemented yet"
        )));
    }

    let new_id = spawn::spawn_session(&app, &workspace, Kind::Interactive, Some(id)).await?;
    Ok(Json(json!({ "session": new_id })))
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
        app.cfg.main_processes.iter().find(|s| s.name == name).cloned()
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
    let base = crate::diff::resolve_base(
        &path,
        q.base,
        &app.cfg.upstream_ref,
        q.pr_base.as_deref(),
    )?;
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
    Ok(Json(crate::diff::file_diff(&path, &base, &q.path, context)?))
}

// ---------------------------------------------------------------------------
// /resolve (§8)
// ---------------------------------------------------------------------------

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
    let id = spawn::spawn_skill_session(&app, number, &pr.head_ref, "resolve").await?;
    Ok(Json(json!({ "session": id })))
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
