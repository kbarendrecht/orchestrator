mod api;
mod config;
mod git;
mod hooks;
mod model;
mod pty;
mod ring;
mod spawn;
mod state;
mod store;
mod worktree;
mod ws;

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use config::Config;
use model::*;
use state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "orchd=info".into()),
        )
        .init();

    let main_checkout = std::env::args()
        .skip_while(|a| a != "--main")
        .nth(1)
        .map(PathBuf::from)
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p));

    let cfg = Config::load_or_init(main_checkout)?;
    if !cfg.main_checkout.join(".git").exists() {
        anyhow::bail!(
            "{} does not look like a git checkout",
            cfg.main_checkout.display()
        );
    }

    if !cfg.persist_transcripts {
        tracing::warn!(
            "persist_transcripts is off — spawned sessions write no transcript, \
             so resume and the teardown transcript check do nothing. \
             Set it to true in {} when you are done developing.",
            Config::path()?.display()
        );
    }

    let settings = hooks::write_settings(cfg.port)?;
    tracing::info!("hook settings at {}", settings.display());

    // Repo config from §4. fsmonitor is deliberately main-only.
    let _ = git::configure_repo(&cfg.main_checkout);

    let token = state::random_token();
    let app = AppState::new(cfg, token.clone());

    // Keep `upstream/develop` fresh or the merge-base the context bar shows
    // drifts (§5). Offline is not fatal — the last-known ref still resolves.
    if let Err(e) = git::fetch_upstream(&app.cfg.main_checkout) {
        tracing::warn!("upstream fetch failed, using last-known ref: {e:#}");
    }

    // Session records outlive the daemon; the processes they name do not.
    let records = store::load();
    let orphans = store::reap_orphans(&records);
    if orphans > 0 {
        tracing::warn!("reaped {orphans} orphan session(s) from a crashed daemon");
    }
    adopt_existing_worktrees(&app).await?;
    app.restore_sessions(records).await;
    reconcile_all(&app).await;
    autostart_processes(&app).await;

    let port = app.cfg.port;
    let router = Router::new()
        .route("/", get(index))
        .route("/app.js", get(asset_js))
        .route("/app.css", get(asset_css))
        .route("/vendor/:file", get(vendor))
        .route("/api/state", get(api::get_state))
        .route("/api/merge-base", get(api::merge_base))
        .route("/api/session", post(api::new_session))
        .route("/api/session/:id/kill", post(api::kill_session))
        .route("/api/session/:id/resume", post(api::resume_session))
        .route("/api/worktree", post(api::new_worktree))
        .route("/api/workspace/:id/shell", post(api::new_shell))
        .route("/api/workspace/:id/reconcile", post(api::reconcile))
        .route("/api/workspace/:id/preflight", get(api::preflight))
        .route("/api/workspace/:id/archive", post(api::archive_workspace))
        .route("/api/workspace/:id/teardown", post(api::teardown))
        .route(
            "/api/workspace/:id/process/:name/restart",
            post(api::restart_process),
        )
        .route("/api/process/:id/close", post(api::close_process))
        .route("/ws/events", get(ws::events))
        .route("/ws/pty", get(ws::pty))
        // Hook endpoints live under their own prefix and are treated as
        // write-only observers (§12).
        .route("/hooks/session-start", post(hooks::session_start))
        .route("/hooks/user-prompt-submit", post(hooks::user_prompt_submit))
        .route("/hooks/post-tool-use", post(hooks::post_tool_use))
        .route("/hooks/notification/:kind", post(hooks::notification))
        .route("/hooks/stop", post(hooks::stop))
        .route("/hooks/subagent-stop", post(hooks::subagent_stop))
        .route("/hooks/stop-failure", post(hooks::stop_failure))
        .route("/hooks/session-end", post(hooks::session_end))
        .route("/hooks/boundary-block", post(hooks::boundary_block))
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            api::guard,
        ))
        .with_state(app.clone());

    // Single machine, no remote access. Never 0.0.0.0 (§12).
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr} — is another orchd already running?"))?;

    println!("orchd  http://127.0.0.1:{port}/?token={token}");
    println!("main   {}", app.cfg.main_checkout.display());

    axum::serve(listener, router).await?;
    Ok(())
}

/// Register worktrees that already exist on disk.
///
/// The daemon owns worktree creation going forward, but it must not be blind to
/// the ones a previous run (or a hand-run `claude -w`) left behind.
async fn adopt_existing_worktrees(app: &Arc<AppState>) -> Result<()> {
    let entries = git::worktree_list(&app.cfg.main_checkout)?;
    let dir = app.cfg.worktrees_dir();
    for e in entries {
        let path = PathBuf::from(&e.path);
        if path == app.cfg.main_checkout {
            continue;
        }
        let Some(name) = spawn::worktree_name_of(&path, &dir) else {
            // A worktree outside `.claude/worktrees/` is not ours to manage.
            tracing::warn!("ignoring worktree outside the managed dir: {}", e.path);
            continue;
        };
        app.register_worktree(&name, path, e.branch).await;
    }
    Ok(())
}

async fn reconcile_all(app: &Arc<AppState>) {
    let ids: Vec<String> = app.inner.read().await.workspaces.keys().cloned().collect();
    for id in ids {
        if let Err(e) = app.reconcile(&id).await {
            tracing::warn!("reconcile {id} failed: {e:#}");
        }
    }
}

/// Managed processes start only when config says so.
///
/// `docker compose up` is not something to launch behind your back on daemon
/// start; the drawer's restart button is the explicit path.
async fn autostart_processes(app: &Arc<AppState>) {
    for spec in app.cfg.main_processes.clone() {
        if !spec.autostart {
            continue;
        }
        if let Err(e) = spawn::start_managed(app, MAIN, &spec).await {
            tracing::warn!("could not start {}: {e:#}", spec.name);
        }
    }
}

// ---------------------------------------------------------------------------
// SPA
// ---------------------------------------------------------------------------

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");

/// The token is embedded in the served page rather than fetched, so it never
/// exists as a value any other origin could ask for (§12).
async fn index(State(app): State<Arc<AppState>>) -> Html<String> {
    Html(INDEX.replace("__ORCH_TOKEN__", &app.token))
}

async fn asset_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript")], APP_JS)
}

async fn asset_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css")], APP_CSS)
}

/// xterm's own dist files, copied in at build time.
async fn vendor(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    // No path traversal: only a flat, known set of filenames is served.
    if file.contains('/') || file.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad asset").into_response();
    }
    let body = match file.as_str() {
        "xterm.js" => include_str!("../web/vendor/xterm.js"),
        "xterm.css" => include_str!("../web/vendor/xterm.css"),
        "addon-fit.js" => include_str!("../web/vendor/addon-fit.js"),
        "addon-webgl.js" => include_str!("../web/vendor/addon-webgl.js"),
        _ => return (StatusCode::NOT_FOUND, "no such asset").into_response(),
    };
    let ct = if file.ends_with(".css") {
        "text/css"
    } else {
        "text/javascript"
    };
    ([(header::CONTENT_TYPE, ct)], body).into_response()
}
