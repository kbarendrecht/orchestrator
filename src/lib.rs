//! The daemon, as a library.
//!
//! `main.rs` is the headless CLI over this; the desktop shell in `desktop/` is
//! the other caller. Everything the two share — startup order, the router, the
//! pollers — lives here so neither can drift from the other.

pub mod api;
pub mod capability;
pub mod config;
pub mod diff;
pub mod edit;
pub mod git;
pub mod github;
pub mod github_write;
pub mod green;
pub mod hooks;
pub mod model;
pub mod patch;
pub mod post;
pub mod prompt;
pub mod proposal;
pub mod pty;
pub mod reviews;
pub mod ring;
pub mod spawn;
pub mod state;
pub mod store;
pub mod story;
pub mod todo;
pub mod triage;
pub mod window;
pub mod worktree;
pub mod ws;

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

/// How the caller wants the daemon brought up.
#[derive(Debug, Clone)]
pub struct StartOptions {
    /// Overrides `main_checkout` in the config file. The CLI's `--main`.
    pub main_checkout: Option<PathBuf>,
    /// Fall back to an ephemeral port when the configured one is taken.
    ///
    /// The CLI wants the opposite: a busy port there means another orchd is
    /// already running and the honest move is to say so. The desktop app has
    /// no terminal to say it in, and a second window on a stray port still
    /// works, so it takes what it can get.
    pub fallback_port: bool,
    /// How the SPA should draw its top bar. Headless leaves this `None`.
    pub chrome: window::Chrome,
}

impl Default for StartOptions {
    fn default() -> Self {
        StartOptions {
            main_checkout: None,
            fallback_port: false,
            chrome: window::Chrome::None,
        }
    }
}

/// A running daemon.
///
/// Dropping this does not stop anything — the pty children outlive the future
/// that spawned them. Call [`Server::shutdown`] to actually take them down.
pub struct Server {
    pub port: u16,
    pub token: String,
    pub app: Arc<AppState>,
    serve: tokio::task::JoinHandle<()>,
}

impl Server {
    /// The URL that authenticates: the token is a query parameter exactly once,
    /// on the initial navigation (§12).
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/?token={}", self.port, self.token)
    }

    /// Kill every child this daemon owns, then stop serving.
    ///
    /// Sessions and managed processes both go: an orchd that is not running is
    /// not supervising `ng-watch`, and a build watcher nobody is watching is
    /// just a CPU leak with a log file.
    ///
    /// This reaches only as far as the ptys. `docker compose up -d` detaches
    /// and its pty child is long gone by the time we get here, so the
    /// containers keep running — which is the intent. Long-lived containers
    /// are infrastructure; `ng-watch` is a process this app started and should
    /// therefore finish.
    pub async fn shutdown(&self) {
        // Before the killing, not after: `was_live` is read off session state,
        // which the exit watchers are about to rewrite.
        self.app.persist_now().await;

        let mut killed = 0usize;
        {
            let mut inner = self.app.inner.write().await;
            for s in inner.sessions.values() {
                if let Some(h) = &s.pty {
                    if h.is_alive() {
                        let _ = h.kill();
                        killed += 1;
                    }
                }
            }
            for w in inner.workspaces.values_mut() {
                for p in &w.processes {
                    if let Some(h) = &p.pty {
                        if h.is_alive() {
                            let _ = h.kill();
                            killed += 1;
                        }
                    }
                }
                w.processes.clear();
            }
        }
        tracing::info!("shutdown: killed {killed} child process(es)");
        self.serve.abort();
    }
}

/// Bring the daemon up and start serving.
///
/// Returns as soon as the listener is bound, so a caller with a window to open
/// has a port and a token to point it at. Everything slower than that — the
/// upstream fetch, auto-resume, the pollers — runs on its own tasks.
pub async fn start(opts: StartOptions) -> Result<Server> {
    let mut cfg = Config::load_or_init(opts.main_checkout)?;
    check_config(&cfg)?;

    // Bind before anything else reads the port. The hook settings bake it into
    // URLs that `claude` subprocesses will call back on, and the request guard
    // checks Host and Origin against it — both would be wrong, in the silent
    // way, if we fell back to an ephemeral port after publishing the
    // configured one.
    let (listener, port) = bind(cfg.port, opts.fallback_port).await?;
    if port != cfg.port {
        tracing::warn!("port {} was taken — serving on {port} instead", cfg.port);
        cfg.port = port;
    }

    let settings = hooks::write_settings(cfg.port)?;
    tracing::info!("hook settings at {}", settings.display());

    // Repo config from §4. fsmonitor is deliberately main-only.
    let _ = git::configure_repo(&cfg.main_checkout);

    let token = state::random_token();
    let app = AppState::new(cfg, token.clone(), opts.chrome);

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
    app.restore_sessions(records.clone()).await;
    {
        let mut inner = app.inner.write().await;
        inner.automation = store::load_automation();
        inner.stories = store::load_stories();
        // A batch that stopped for the manual phase. Its patches are already
        // committed, so losing this to a restart would strand the branch.
        inner.manual = store::load_manual();
        if !inner.manual.is_empty() {
            let prs: Vec<String> = inner.manual.keys().map(|p| format!("#{p}")).collect();
            tracing::info!("manual phase still open on {}", prs.join(", "));
        }
        // Said out loud at boot, because `tracker` decides whether a whole option
        // appears on every review card. A misconfigured one must not read as
        // "triage never proposes stories".
        match app.cfg.tracker {
            config::Tracker::None => tracing::info!("tracker: none — `story+reply` is off"),
            t => match story::resolve_token(app.cfg.shortcut_token_file.as_deref()) {
                Ok(_) => tracing::info!(
                    "tracker: {t:?}, token resolved, {} story/ies cached",
                    inner.stories.len()
                ),
                Err(e) => tracing::warn!("tracker: {t:?} but no usable token — {e:#}"),
            },
        }
    }
    reconcile_all(&app).await;
    autostart_processes(&app).await;
    if app.cfg.auto_resume {
        auto_resume(app.clone(), records);
    }
    start_pr_poller(app.clone());
    start_review_poller(app.clone());
    start_stack_poller(app.clone());
    start_todo_writer(app.clone());
    // A debug build is `cargo run` from a checkout; its version is whatever the
    // working tree is, so comparing it against a release only ever nags. Only a
    // release build — which is what a downloaded/`mise`-installed one is — checks.
    if !cfg!(debug_assertions) {
        start_update_poller(app.clone());
    }

    let router = router(app.clone());
    let serve = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!("server stopped: {e:#}");
        }
    });

    Ok(Server {
        port,
        token,
        app,
        serve,
    })
}

/// The two config states worth refusing to start on.
fn check_config(cfg: &Config) -> Result<()> {
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
    Ok(())
}

/// Single machine, no remote access. Never 0.0.0.0 (§12).
async fn bind(port: u16, fallback: bool) -> Result<(tokio::net::TcpListener, u16)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => Ok((l, port)),
        Err(e) if fallback && e.kind() == std::io::ErrorKind::AddrInUse => {
            let l = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .context("binding an ephemeral port")?;
            let port = l.local_addr()?.port();
            Ok((l, port))
        }
        Err(e) => Err(anyhow::Error::new(e))
            .with_context(|| format!("binding {addr} — is another orchd already running?")),
    }
}

fn router(app: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(asset_js))
        .route("/app.css", get(asset_css))
        .route("/vendor/:file", get(vendor))
        .route("/vendor/fonts/:file", get(font))
        .route("/api/state", get(api::get_state))
        .route("/api/merge-base", get(api::merge_base))
        .route("/api/diff", get(api::diff_summary))
        .route("/api/diff/file", get(api::diff_file))
        .route("/api/file", get(api::read_file))
        .route("/api/file", post(api::write_file))
        .route("/api/session", post(api::new_session))
        .route("/api/session/:id/kill", post(api::kill_session))
        .route("/api/session/:id/resume", post(api::resume_session))
        .route("/api/worktree", post(api::new_worktree))
        .route("/api/workspace/:id/shell", post(api::new_shell))
        .route("/api/workspace/:id/reconcile", post(api::reconcile))
        .route("/api/workspace/:id/rebase", post(api::rebase))
        .route("/api/workspace/:id/rebase/abort", post(api::rebase_abort))
        .route("/api/workspace/:id/preflight", get(api::preflight))
        .route("/api/workspace/:id/capabilities", get(api::capabilities))
        .route("/api/workspace/:id/archive", post(api::archive_workspace))
        .route("/api/workspace/:id/teardown", post(api::teardown))
        .route(
            "/api/workspace/:id/process/:name/restart",
            post(api::restart_process),
        )
        .route("/api/process/:id/close", post(api::close_process))
        .route("/api/window/resize/:edge", post(api::window_resize))
        .route("/api/window/:cmd", post(api::window_cmd))
        .route("/api/reviews/refresh", post(api::refresh_reviews))
        .route("/api/prs/refresh", post(api::refresh_prs))
        .route("/api/open", post(api::open_url))
        .route("/api/pr/:number/threads", get(api::pr_threads))
        .route("/api/pr/:number/review", get(api::pr_review))
        .route("/api/pr/:number/triage", post(api::pr_triage))
        // The one route a subprocess calls. Hostile input; see `pr_proposals`.
        .route("/api/pr/:number/proposals", post(api::pr_proposals))
        .route("/api/pr/:number/commit", post(api::pr_commit))
        .route("/api/pr/:number/stash", post(api::pr_stash))
        // The only irreversible one. See `post::run` for the order.
        .route("/api/pr/:number/post", post(api::pr_post))
        // ...unless a thread was answered by hand, in which case the batch stops
        // after the local commit and this finishes it.
        .route("/api/pr/:number/manual", get(api::pr_manual))
        .route("/api/pr/:number/manual/done", post(api::pr_manual_done))
        .route("/api/pr/:number/green", post(api::green_pr))
        .route("/ws/events", get(ws::events))
        .route("/ws/pty", get(ws::pty))
        // Hook endpoints live under their own prefix and are treated as
        // write-only observers (§12).
        .route("/hooks/session-start", post(hooks::session_start))
        .route("/hooks/user-prompt-submit", post(hooks::user_prompt_submit))
        .route("/hooks/pre-edit", post(hooks::pre_edit))
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
        .with_state(app)
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

/// One GraphQL query per 5 minutes, read-only (§6).
///
/// No ETag caching: conditional requests are a REST feature and the GraphQL
/// endpoint is a POST, so the budget is points rather than round trips.
fn start_pr_poller(app: Arc<AppState>) {
    tokio::spawn(async move {
        let repo = match resolve_repo(&app) {
            Some(r) => r,
            None => {
                tracing::warn!("no upstream remote on GitHub — PR polling is off");
                let mut inner = app.inner.write().await;
                inner.pr_error = Some("no GitHub upstream remote configured".into());
                return;
            }
        };
        tracing::info!("polling PRs for {}/{}", repo.0, repo.1);

        let interval = std::time::Duration::from_secs(app.cfg.poll_seconds.max(30));
        loop {
            // Piggyback the upstream fetch on this timer (§5): the merge-base
            // and the behind count are both answered from that ref.
            let main = app.cfg.main_checkout.clone();
            let _ = tokio::task::spawn_blocking(move || git::fetch_upstream(&main)).await;
            reconcile_all(&app).await;

            let token = github::resolve_token(app.cfg.github_token_file.as_deref());
            match token {
                Ok(t) => {
                    if t.source == github::TokenSource::GhCli {
                        // §6 wants read scopes only; gh's token carries write.
                        tracing::warn!(
                            "using `gh auth token`, which has broader scopes than orchd needs. \
                             Set ORCHD_GITHUB_TOKEN or github_token_file to a read-only PAT."
                        );
                    }
                    let source = t.source;
                    let (owner, name) = (repo.0.clone(), repo.1.clone());
                    let result =
                        tokio::task::spawn_blocking(move || github::poll(&t.value, &owner, &name))
                            .await;
                    let mut inner = app.inner.write().await;
                    inner.token_source = Some(source);
                    match result {
                        Ok(Ok((viewer, prs))) => {
                            if !viewer.is_empty() && inner.viewer.as_deref() != Some(&viewer) {
                                tracing::info!(login = %viewer, "github viewer");
                                inner.viewer = Some(viewer);
                            }
                            for p in &prs {
                                let alive = matches!(
                                    inner.automation.get(p.number),
                                    Some(green::PrAutomation::Running { .. })
                                );
                                if !alive {
                                    inner
                                        .automation
                                        .reconcile_head(p.number, p.head_sha.as_deref());
                                }
                            }
                            let _ = store::save_automation(&inner.automation);
                            inner.prs = prs;
                            inner.pr_error = None;
                            inner.pr_fetched = Some(std::time::SystemTime::now());
                        }
                        Ok(Err(e)) => {
                            // Keep the last good list: stale is more useful than
                            // empty, as long as the pane says it is stale.
                            tracing::warn!("PR poll failed: {e:#}");
                            inner.pr_error = Some(format!("{e:#}"));
                        }
                        Err(e) => inner.pr_error = Some(format!("poll task failed: {e}")),
                    }
                }
                Err(e) => {
                    let mut inner = app.inner.write().await;
                    inner.pr_error = Some(format!("{e:#}"));
                }
            }
            // Signals the refresh button that a fetch landed, success or not.
            app.inner.write().await.pr_poll += 1;
            app.notify().await;
            // A manual refresh cuts the wait short and restarts the period, so a
            // button press and the next scheduled poll never land back to back.
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = app.pr_refresh.notified() => {}
            }
        }
    });
}

/// Bring back the sessions that were live when the daemon last went down.
///
/// A crash or a reboot takes every Claude process with it, because the daemon
/// owns the pty. Resuming costs the scrollback — ring buffers are in memory —
/// but keeps the conversation, which is the part that took time to build.
///
/// Deliberately skipped: automation runs, because the PR has moved on and §8
/// demotes an orphaned run to `Exhausted` rather than resurrecting it.
fn auto_resume(app: Arc<AppState>, records: Vec<store::SessionRecord>) {
    tokio::spawn(async move {
        let mut candidates: Vec<store::SessionRecord> = records
            .into_iter()
            .filter(|r| r.was_live && matches!(r.kind, Kind::Interactive))
            .collect();
        if candidates.is_empty() {
            return;
        }
        // Oldest first, so the rail comes back in the order you built it up.
        candidates.sort_by_key(|r| r.created_at);

        let mut resumed = 0usize;
        let mut main_taken = false;
        for r in candidates {
            if resumed >= green::MAX_CLAUDE_PROCESSES {
                tracing::warn!(
                    "stopping auto-resume at {} sessions (process cap)",
                    green::MAX_CLAUDE_PROCESSES
                );
                break;
            }
            if !r.cwd.exists() {
                tracing::warn!(session = %r.id, "not resumed: {} is gone", r.cwd.display());
                continue;
            }
            if !store::resumable(&r) {
                tracing::warn!(
                    session = %r.id,
                    "not resumed: no transcript. persist_transcripts was off when it ran."
                );
                continue;
            }
            // Main is exclusive, so only the first one there comes back (§2).
            if r.workspace == MAIN {
                if main_taken {
                    tracing::warn!(session = %r.id, "not resumed: main is already occupied");
                    continue;
                }
                main_taken = true;
            }

            match spawn::spawn_session(&app, &r.workspace, Kind::Interactive, Some(r.id)).await {
                Ok(id) => {
                    tracing::info!(session = %id, workspace = %r.workspace, "auto-resumed");
                    resumed += 1;
                    // Staggered: half a dozen Claude processes starting at once
                    // makes for a slow, noisy boot.
                    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                }
                Err(e) => tracing::warn!(session = %r.id, "auto-resume failed: {e:#}"),
            }
        }
        if resumed > 0 {
            tracing::info!("auto-resumed {resumed} session(s)");
            app.notify().await;
        }
    });
}

/// Keep TODO.md's generated block honest about what the daemon can currently
/// see. Only conditions that are true now, so the list stays worth reading.
fn start_todo_writer(app: Arc<AppState>) {
    tokio::spawn(async move {
        let path = app.cfg.todo_path.clone().unwrap_or_else(todo::default_path);
        loop {
            let findings = live_findings(&app).await;
            if let Err(e) = todo::update(&path, &findings) {
                tracing::warn!("could not update {}: {e:#}", path.display());
            }
            tokio::time::sleep(std::time::Duration::from_secs(app.cfg.poll_seconds.max(60))).await;
        }
    });
}

async fn live_findings(app: &Arc<AppState>) -> Vec<todo::Finding> {
    let mut out = Vec::new();

    if !app.cfg.persist_transcripts && app.cfg.auto_resume {
        out.push(todo::Finding {
            what: "auto-resume cannot work".into(),
            why: "`auto_resume` is on but `persist_transcripts` is off, so a crash leaves \
                  nothing to resume from. One of the two wants changing."
                .into(),
        });
    }
    if !app.cfg.persist_transcripts {
        out.push(todo::Finding {
            what: "transcripts are off".into(),
            why: "spawned sessions write no `.jsonl`, so resume and the teardown transcript \
                  check do nothing. Set `persist_transcripts` back to true when you are done \
                  developing the daemon."
                .into(),
        });
    }

    let (workspaces, token, review_bad) = {
        let inner = app.inner.read().await;
        (
            inner
                .workspaces
                .values()
                .map(|w| (w.id.clone(), w.path.clone(), w.is_main()))
                .collect::<Vec<_>>(),
            inner.token_source,
            match &inner.reviews {
                crate::reviews::ReviewState::Degraded { reason } => Some(reason.clone()),
                // Pending is startup, not a fault.
                _ => None,
            },
        )
    };

    if token == Some(github::TokenSource::GhCli) {
        out.push(todo::Finding {
            what: "GitHub token is gh's".into(),
            why: "it carries write scopes; §6 wants a read-only PAT in `ORCHD_GITHUB_TOKEN` \
                  or `github_token_file`."
                .into(),
        });
    }
    if let Some(reason) = review_bad {
        out.push(todo::Finding {
            what: "review queue is unavailable".into(),
            why: format!(
                "`mise run reviews --json` is not answering: {}",
                reason.trim()
            ),
        });
    }

    // The capability probe is the one that finds real drift, so it drives the
    // per-workspace entries.
    for (id, path, is_main) in workspaces {
        if is_main {
            continue;
        }
        let cfg = app.cfg.capabilities.clone();
        let main = app.cfg.main_checkout.clone();
        let ws = id.clone();
        let report =
            tokio::task::spawn_blocking(move || capability::report(&cfg, &ws, &path, &main, false))
                .await;
        let Ok(report) = report else { continue };

        if let capability::AutoloadProbe::Outside { file } = &report.autoload {
            out.push(todo::Finding {
                what: format!("`{id}` cannot be trusted to run PHP suites"),
                why: format!(
                    "autoload resolves to `{file}`, outside the worktree, so a suite run there \
                     loads main's code. §7's post-WIP table assumes otherwise."
                ),
            });
        }
        for d in report.deps.iter().filter(|d| d.present && !d.matches_main) {
            out.push(todo::Finding {
                what: format!("`{id}` has a stale `{}`", d.file),
                why: "re-link from main; results from a frozen lockfile are not this \
                      workspace's (§7 rule 3)."
                    .into(),
            });
        }
    }

    out
}

/// Own timer, offset from the PR poll so the two do not burst together (§6b).
fn start_review_poller(app: Arc<AppState>) {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(app.cfg.poll_seconds.max(30));
        loop {
            // Fetch straight away on launch, so the queue is not blank for the
            // first period, then again on each period or whenever the refresh
            // button pulses `review_refresh`.
            let main = app.cfg.main_checkout.clone();
            let timeout = app.cfg.review_timeout_seconds;
            let state = tokio::task::spawn_blocking(move || reviews::fetch(&main, timeout))
                .await
                .unwrap_or_else(|e| reviews::ReviewState::Degraded {
                    reason: format!("review poll task failed: {e}"),
                });
            if let reviews::ReviewState::Degraded { reason } = &state {
                tracing::warn!("review queue degraded: {reason}");
            }
            {
                let mut inner = app.inner.write().await;
                inner.reviews = state;
                // Signals the refresh button that a fetch landed, even when the
                // queue is byte-for-byte the same as before.
                inner.reviews_poll = inner.reviews_poll.wrapping_add(1);
            }
            app.notify().await;

            // A manual refresh cuts the wait short and restarts the period, so a
            // button press and the next scheduled poll never land back to back.
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = app.review_refresh.notified() => {}
            }
        }
    });
}

/// Whether the main checkout's `docker compose` stack is up, for the drawer
/// header. Not the managed `docker` process's state — the containers' own — so it
/// stays right whether the stack was brought up through the drawer or by hand.
fn start_stack_poller(app: Arc<AppState>) {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(20);
        loop {
            let main = app.cfg.main_checkout.clone();
            let up = tokio::task::spawn_blocking(move || stack_running(&main))
                .await
                .unwrap_or(false);
            let mut inner = app.inner.write().await;
            if inner.stack_up != Some(up) {
                inner.stack_up = Some(up);
                drop(inner);
                app.notify().await;
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// True when `docker compose ps` reports at least one running container. A missing
/// `docker` or a stopped daemon fails the command and reads as down, which is the
/// honest answer for "is the stack up".
///
/// A checkout with no compose file has no stack at all, so it answers with a cheap
/// filesystem check rather than spawning `docker` every poll for a fixed "down".
fn stack_running(main: &std::path::Path) -> bool {
    let has_compose = [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ]
    .iter()
    .any(|f| main.join(f).exists());
    if !has_compose {
        return false;
    }
    std::process::Command::new("docker")
        .args(["compose", "ps", "--status", "running", "-q"])
        .current_dir(main)
        .output()
        .map(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false)
}

/// Notice a newer GitHub release than the running build.
///
/// The release lives on the fork (`kbarendrecht/orchestrator`), which is where
/// the app itself ships from — distinct from the *monorepo's* upstream that the
/// PR poller watches. Checks on launch and every six hours; a found update sits
/// in the snapshot as a dismissible nudge, and `mise up` is what installs it.
fn start_update_poller(app: Arc<AppState>) {
    // The repo the binary is released from, not the monorepo it hosts.
    const RELEASE_REPO: (&str, &str) = ("kbarendrecht", "orchestrator");
    let current = env!("CARGO_PKG_VERSION").to_string();
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(6 * 60 * 60);
        loop {
            let cur = current.clone();
            if let Ok(Some((tag, url))) = tokio::task::spawn_blocking(|| {
                github::latest_release(RELEASE_REPO.0, RELEASE_REPO.1)
            })
            .await
            {
                let newer = match (parse_semver(&tag), parse_semver(&cur)) {
                    (Some(latest), Some(running)) => latest > running,
                    _ => false,
                };
                let next = newer.then(|| crate::state::UpdateInfo {
                    current: cur.clone(),
                    latest: tag.trim_start_matches('v').to_string(),
                    url,
                });
                let mut inner = app.inner.write().await;
                if inner.update != next {
                    inner.update = next;
                    drop(inner);
                    app.notify().await;
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// `v1.2.3` / `1.2.3` / `1.2.3-rc1` → `(1, 2, 3)`. Prerelease and build metadata
/// are dropped: good enough to answer "is there a newer release", which is all
/// the nudge asks. Anything unparseable is `None` and simply never nags.
fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

pub(crate) fn resolve_repo(app: &Arc<AppState>) -> Option<(String, String)> {
    if let Some(r) = &app.cfg.repo {
        let (o, n) = r.split_once('/')?;
        return Some((o.to_string(), n.to_string()));
    }
    let url = github::remote_url(&app.cfg.main_checkout, &app.cfg.upstream_remote)?;
    github::repo_from_remote(&url)
}

// ---------------------------------------------------------------------------
// SPA
// ---------------------------------------------------------------------------

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");

/// The token is embedded in the served page rather than fetched, so it never
/// exists as a value any other origin could ask for (§12).
async fn index(State(app): State<Arc<AppState>>) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store, must-revalidate")],
        Html(
            INDEX
                .replace("__ORCH_TOKEN__", &app.token)
                .replace("__ORCH_CHROME__", app.chrome.as_str()),
        ),
    )
        .into_response()
}

/// Serve a static asset with its real type and no caching.
///
/// `no-store` matters more than it looks: the SPA is baked into the binary with
/// `include_str!`, so a cached bundle silently shadows a rebuilt daemon and you
/// debug code that is not running. Found exactly that way.
fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store, must-revalidate"),
        ],
        body,
    )
        .into_response()
}

async fn asset_js() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}

async fn asset_css() -> Response {
    asset("text/css; charset=utf-8", APP_CSS)
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
        "text/css; charset=utf-8"
    } else {
        "text/javascript; charset=utf-8"
    };
    asset(ct, body)
}

/// Webfonts, baked in like everything else.
///
/// A desktop app that reaches out to fonts.googleapis.com on every launch is
/// one flaky DNS lookup away from rendering in Times New Roman, and it tells a
/// third party when you start work. These are bytes, not text, so they cannot
/// go through `asset`.
async fn font(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    if file.contains('/') || file.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad asset").into_response();
    }
    // Plex Sans and Martian Mono ship as variable fonts, so one file covers
    // every weight the UI asks for. Plex Mono is still static per weight.
    let body: &'static [u8] = match file.as_str() {
        "plex-sans.woff2" => include_bytes!("../web/vendor/fonts/plex-sans.woff2"),
        "plex-mono-400.woff2" => include_bytes!("../web/vendor/fonts/plex-mono-400.woff2"),
        "plex-mono-500.woff2" => include_bytes!("../web/vendor/fonts/plex-mono-500.woff2"),
        "plex-mono-600.woff2" => include_bytes!("../web/vendor/fonts/plex-mono-600.woff2"),
        "martian-mono.woff2" => include_bytes!("../web/vendor/fonts/martian-mono.woff2"),
        _ => return (StatusCode::NOT_FOUND, "no such asset").into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            // Immutable, unlike the SPA: these never change without a rebuild
            // that also changes the filename set.
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
        .into_response()
}
