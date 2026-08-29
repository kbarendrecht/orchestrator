//! The daemon, as a library.
//!
//! `main.rs` is the headless CLI over this; the desktop shell in `desktop/` is
//! the other caller. Everything the two share — startup order, the router, the
//! pollers — lives here so neither can drift from the other.

pub mod agent_update;
pub mod api;
pub mod config;
pub mod diff;
pub mod edit;
pub mod git;
pub mod forge;
pub mod fix_pr;
pub mod headroom;
pub mod health;
pub mod hooks;
pub mod instance;
pub mod model;
pub mod patch;
pub mod post;
pub mod proc;
pub mod prompt;
pub mod proposal;
pub mod pty;
pub mod review_commit;
pub mod reviews;
pub mod ring;
pub mod spawn;
pub mod state;
pub mod store;
pub mod story;
pub mod todo;
pub mod tracker;
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
use forge::Forge;
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
    /// Dropped last, releasing the single-instance lock when the daemon goes.
    _lock: instance::Lock,
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
        // Before anything is killed: every pty about to die wakes an exit watcher,
        // and one of them would otherwise read a restart as "you finished with
        // main" and move the checkout out from under auto-resume.
        self.app
            .shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);

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
    // First, and before anything is written: a second daemon would spawn into
    // the same worktrees and rewrite the hook settings with its own port.
    let lock = instance::acquire()?;

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

    let settings = {
        use crate::tracker::Tracker as _;
        let t = crate::tracker::TrackerImpl::for_kind(cfg.tracker);
        hooks::write_settings(cfg.port, t.as_ref().map(|t| t.mcp_server()))?
    };
    tracing::info!("hook settings at {}", settings.display());

    // Put the default queue on disk if it is not there. Every start, not just the
    // first: deleting the file is how you ask for the shipped version back, and a
    // config pointing at a script that has gone would otherwise read as a broken
    // command rather than repairing itself. Never overwrites, so an edited copy is
    // safe (`reviews::eject_default_script`).
    if let Err(e) = reviews::eject_default_script() {
        tracing::warn!("could not write the default review queue: {e:#}");
    }

    // Repo config from §4. fsmonitor is deliberately main-only.
    let _ = git::configure_repo(&cfg.main_checkout);

    let token = state::random_token();
    let app = AppState::new(cfg, token.clone(), opts.chrome);

    // Keep the base ref fresh or the merge-base the context bar shows drifts
    // (§5). Offline is not fatal — the last-known ref still resolves.
    if let Err(e) = git::fetch_upstream(&app.cfg.main_checkout, &app.cfg.upstream_ref) {
        tracing::warn!("upstream fetch failed, using last-known ref: {e:#}");
    }

    // Session records outlive the daemon; the processes they name do not.
    let records = store::load();
    let orphans = store::reap_orphans(&records);
    if orphans > 0 {
        tracing::warn!("reaped {orphans} orphan session(s) from a crashed daemon");
    }
    // Before restoring, not after: a record with no transcript has nothing behind
    // it and no row in the rail, so restoring one only adds something invisible to
    // the snapshot. Written back so they go for good rather than being re-read and
    // re-dropped on every start.
    let (records, gone) = {
        // Who they were, before the vector is consumed — this deletes durable
        // state, and the first version of it deleted every record on a real
        // machine, so the log names them rather than counting them. cwd and the
        // recorded path travel too, so a dropped ghost's header file can be removed
        // rather than left for a later id-scan to resurrect.
        let was: Vec<(model::SessionId, String, PathBuf, Option<PathBuf>)> = records
            .iter()
            .map(|r| (r.id, r.workspace.clone(), r.cwd.clone(), r.transcript_path.clone()))
            .collect();
        let (kept, _) = store::prune_ghosts(records);
        let ids: std::collections::HashSet<_> = kept.iter().map(|r| r.id).collect();
        let gone: Vec<String> = was
            .into_iter()
            .filter(|(id, ..)| !ids.contains(id))
            .map(|(id, ws, cwd, recorded)| {
                // A dropped record has no conversation and is not live, so its file
                // is a headers-only remnant with nothing to lose.
                store::delete_transcript(id, &cwd, recorded.as_deref());
                format!("{} ({ws})", &id.to_string()[..8])
            })
            .collect();
        (kept, gone)
    };
    if !gone.is_empty() {
        tracing::info!(
            "dropped {} session record(s) with no conversation to return to: {}",
            gone.len(),
            gone.join(", ")
        );
        // Written back pinned as well as pruned, so the survivors' transcript
        // paths stop being re-hunted on every start.
        if let Err(e) = store::save(&records) {
            tracing::error!("could not write the pruned session store: {e:#}");
        }
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
        // A resolve run's commits outlive its session, and this is the only record
        // of which commit answers which thread. Restored as an account: `load`
        // marks every one ended, because no pty survives a restart.
        inner.resolve_runs = store::load_resolve_runs();
        if !inner.resolve_runs.is_empty() {
            let prs: Vec<String> = inner
                .resolve_runs
                .keys()
                .map(|p| format!("#{p}"))
                .collect();
            tracing::info!("resolve runs recovered for {}", prs.join(", "));
        }
        // Said out loud at boot, because `tracker` decides whether a whole option
        // appears on every review card. A misconfigured one must not read as
        // "triage never proposes stories".
        match app.cfg.tracker {
            config::TrackerKind::None => tracing::info!("tracker: none — `story+reply` is off"),
            t => match story::resolve_token() {
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
    start_workspace_watcher(app.clone());
    start_head_poller(app.clone());
    start_todo_writer(app.clone());
    // A debug build is `cargo run` from a checkout; its version is whatever the
    // working tree is, so comparing it against a release only ever nags. Only a
    // release build — which is what a downloaded/`mise`-installed one is — checks.
    if !cfg!(debug_assertions) {
        start_update_poller(app.clone());
        // The agent's own version, which is the one that nags you in a terminal.
        agent_update::start_poller(app.clone());
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
        _lock: lock,
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

/// The HTTP surface.
///
/// **Two lists in `api.rs::guard` have to be kept in step with this table**, and
/// neither will fail loudly if you forget:
///
/// - `is_ask` decides which routes an *agent* may call with its own narrow token,
///   and it matches by path **suffix** (`/ask`, `/wait`, `/spawn`) under
///   `/api/session/`. A new route ending in one of those is silently
///   agent-reachable.
/// - `SPENDS_GITHUB_TOKEN` lists the GETs that spend the GitHub credential
///   outbound and therefore need the daemon token despite being GETs. Its own
///   comment records `/review` having been added without it once.
///
/// Adding a route is otherwise a one-liner; adding one that touches either of
/// those two properties is not.
fn router(app: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(asset_js))
        .route("/app.css", get(asset_css))
        .route("/review-preview", get(review_preview))
        .route("/js/:file", get(module))
        .route("/vendor/:file", get(vendor))
        .route("/vendor/fonts/:file", get(font))
        .route("/api/state", get(api::get_state))
        .route("/api/config", get(api::get_config).post(api::set_config))
        .route("/api/diff", get(api::diff_summary))
        .route("/api/diff/file", get(api::diff_file))
        .route("/api/file", get(api::read_file))
        .route("/api/file", post(api::write_file))
        .route("/api/session", post(api::new_session))
        .route("/api/session/:id/kill", post(api::kill_session))
        .route("/api/session/:id/rename", post(api::rename_session))
        .route("/api/session/:id/out-of-main", post(api::move_out_of_main))
        .route("/api/session/:id/rewind", post(api::rewind_session))
        .route("/api/session/:id/resume", post(api::resume_session))
        .route("/api/sessions/nudge", post(api::nudge_sessions))
        .route("/api/session/:id/fork", post(api::fork_session))
        .route("/api/session/:id/spawn", post(api::spawn_from_session))
        .route("/api/session/:id/process", post(api::process_from_session))
        .route("/api/session/:id/ask", post(api::ask))
        .route("/api/session/:id/ask/:ask/wait", get(api::ask_wait))
        .route("/api/session/:id/answer", post(api::answer))
        .route(
            "/api/session/:id/thread/:thread/committed",
            post(api::thread_committed),
        )
        .route(
            "/api/session/:id/thread/:thread/stuck",
            post(api::thread_stuck),
        )
        .route("/api/session/:id/delete", post(api::delete_session))
        .route("/api/worktree", post(api::new_worktree))
        .route("/api/workspace/:id/shell", post(api::new_shell))
        .route("/api/workspace/:id/reconcile", post(api::reconcile))
        .route("/api/workspace/:id/rebase", post(api::rebase))
        .route("/api/workspace/:id/rebase/abort", post(api::rebase_abort))
        .route("/api/workspace/:id/preflight", get(api::preflight))
        .route("/api/workspace/:id/archive", post(api::archive_workspace))
        .route("/api/workspace/:id/teardown", post(api::teardown))
        .route("/api/workspace/:id/swap-main", post(api::swap_with_main))
        .route(
            "/api/workspace/:id/process/:name/restart",
            post(api::restart_process),
        )
        .route("/api/process/:id/close", post(api::close_process))
        .route("/api/window/resize/:edge", post(api::window_resize))
        .route("/api/window/:cmd", post(api::window_cmd))
        .route("/api/reviews/refresh", post(api::refresh_reviews))
        .route("/api/prs/refresh", post(api::refresh_prs))
        // The agent's own version: check it now, and install it in the drawer.
        .route("/api/agent/update/refresh", post(api::refresh_agent_update))
        .route("/api/agent/upgrade/dismiss", post(api::dismiss_agent_upgrade))
        .route("/api/agent/upgrade", post(api::upgrade_agent))
        .route("/api/open", post(api::open_url))
        .route("/api/open/file", post(api::open_file))
        .route("/api/pr/:number/review", get(api::pr_review))
        .route("/api/pr/:number/triage", post(api::pr_triage))
        .route("/api/pr/:number/review-session", post(api::pr_review_session))
        // The one route a subprocess calls. Hostile input; see `pr_proposals`.
        .route("/api/pr/:number/proposals", post(api::pr_proposals))
        .route("/api/pr/:number/commit", post(api::pr_commit))
        .route("/api/pr/:number/stash", post(api::pr_stash))
        // The only irreversible one. See `post::run` for the order.
        .route("/api/pr/:number/post", post(api::pr_post))
        .route("/api/pr/:number/resolve-run", post(api::pr_resolve_run))
        .route("/api/pr/:number/run/push", post(api::pr_run_push))
        .route("/api/pr/:number/run/rerequest", post(api::pr_run_rerequest))
        // ...unless a thread was answered by hand, in which case the batch stops
        // after the local commit and this finishes it.
        .route("/api/pr/:number/manual", get(api::pr_manual))
        .route("/api/pr/:number/manual/done", post(api::pr_manual_done))
        // The rail's default: spawn a session running `/resolve <pr>` in a pane.
        .route("/api/pr/:number/open", post(api::open_pr))
        .route("/api/pr/:number/resolve", post(api::resolve_pr))
        .route("/api/pr/:number/fix-pr", post(api::fix_pr))
        .route("/ws/events", get(ws::events))
        .route("/ws/pty", get(ws::pty))
        // Hook endpoints live under their own prefix and are treated as
        // write-only observers (§12). The one that answers a question rather
        // than recording something stays on this router; the rest are merged in
        // below, behind the layer that stops a one-second timeout cancelling
        // them.
        .route("/hooks/pre-edit", post(hooks::pre_edit))
        .merge(observer_hooks())
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            api::guard,
        ))
        .with_state(app)
}

/// The hooks that only ever record what happened, answered on arrival.
///
/// Separate router because the layer is what makes them safe to be slow, and a
/// handler added to the list gets it without anyone remembering to.
fn observer_hooks() -> Router<Arc<AppState>> {
    Router::new()
        .route("/hooks/session-start", post(hooks::session_start))
        .route("/hooks/user-prompt-submit", post(hooks::user_prompt_submit))
        .route("/hooks/post-tool-use", post(hooks::post_tool_use))
        .route("/hooks/notification/:kind", post(hooks::notification))
        .route("/hooks/stop", post(hooks::stop))
        .route("/hooks/subagent-stop", post(hooks::subagent_stop))
        .route("/hooks/stop-failure", post(hooks::stop_failure))
        .route("/hooks/session-end", post(hooks::session_end))
        .route("/hooks/boundary-block", post(hooks::boundary_block))
        .layer(axum::middleware::from_fn(hooks::detach))
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
            let base = app.cfg.upstream_ref.clone();
            let _ = tokio::task::spawn_blocking(move || git::fetch_upstream(&main, &base)).await;
            reconcile_all(&app).await;

            app.inner.write().await.pr_polling = true;
            app.notify().await;
            let token = forge::resolve_token(app.cfg.github_token_file.as_deref());
            match token {
                Ok(t) => {
                    // No warning for a `gh auth token`. §6 wants read scopes only
                    // and gh's carries write, but it is also the fallback that
                    // makes the app work out of the box — so saying so *on every
                    // poll* was a line that could not be acted on and could not be
                    // silenced, which is noise rather than information. The fact
                    // still reaches you where it is useful: `token_source` is in
                    // the snapshot and the PR pane marks it with a `⚠`.
                    let source = t.source;
                    let forge =
                        forge::ForgeImpl::for_kind(app.cfg.forge, repo.0.clone(), repo.1.clone(), t.value);
                    let result = tokio::task::spawn_blocking(move || forge.poll_prs()).await;
                    let mut inner = app.inner.write().await;
                    inner.token_source = Some(source);
                    match result {
                        Ok(Ok((viewer, prs))) => {
                            if !viewer.is_empty() && inner.viewer.as_deref() != Some(&viewer) {
                                tracing::info!(login = %viewer, "github viewer");
                                inner.viewer = Some(viewer);
                            }
                            // Exhaustion clears when a head moves with no run
                            // alive. Through `with_automation` so the write is not
                            // a thing this poller has to remember — it used to
                            // drop the error entirely.
                            let heads: Vec<(u64, Option<String>)> =
                                prs.iter().map(|p| (p.number, p.head_sha.clone())).collect();
                            inner.with_automation("pr poll", |a| {
                                let mut changed = false;
                                for (number, head) in heads {
                                    let alive = matches!(
                                        a.get(number),
                                        Some(fix_pr::PrAutomation::Running { .. })
                                    );
                                    if !alive {
                                        // Reported rather than assumed, so a poll
                                        // that changed nothing does not rewrite
                                        // `automation.json` — and the poll that
                                        // adopts a baseline does.
                                        changed |= a.reconcile_head(number, head.as_deref());
                                    }
                                }
                                changed
                            });
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
            {
                let mut inner = app.inner.write().await;
                inner.pr_poll += 1;
                inner.pr_polling = false;
            }
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
/// Of several resumable records, the ones to actually bring back: at most one per
/// workspace, oldest first.
///
/// Pure and separate from the spawn loop so the rule can be tested without a daemon.
/// Oldest-first both orders the rail the way it was built up and decides *which* of
/// two records sharing a workspace wins.
fn first_per_workspace(mut resumable: Vec<store::SessionRecord>) -> Vec<store::SessionRecord> {
    resumable.sort_by_key(|r| r.created_at);
    let mut seen = std::collections::HashSet::new();
    resumable
        .into_iter()
        .filter(|r| seen.insert(r.workspace.clone()))
        .collect()
}

fn auto_resume(app: Arc<AppState>, records: Vec<store::SessionRecord>) {
    tokio::spawn(async move {
        // Any session that was live, whatever started it. `/resolve` and the fix
        // run are `Automation`, and skipping them silently meant the pane you
        // were actually sitting in was the one that did not come back. `--resume`
        // reopens the conversation at its prompt; it re-runs nothing, so there is
        // no rebase or push waiting to fire on boot.
        let candidates: Vec<store::SessionRecord> =
            records.into_iter().filter(|r| r.was_live).collect();
        if candidates.is_empty() {
            return;
        }

        // Only the records worth bringing back: a real directory to return to and a
        // turn behind them. A header-only transcript resumes into an instant exit,
        // which used to log "auto-resumed" about a session already gone; `prune_ghosts`
        // repaired the `had_a_turn` bit from disk before these got here.
        let mut resumable = Vec::new();
        for r in candidates {
            if !r.cwd.exists() {
                tracing::warn!(session = %r.id, "not resumed: {} is gone", r.cwd.display());
            } else if !r.had_a_turn {
                tracing::warn!(session = %r.id, "not resumed: no conversation to resume from.");
            } else {
                resumable.push(r);
            }
        }

        // One live session per workspace, the same rule the API enforces at runtime
        // (`refuse_if_occupied`). A cold start has spawned nothing yet, so the restore
        // path is where it holds — and it also defends a `sessions.json` written
        // before that invariant existed, where two records shared one worktree.
        let to_resume = first_per_workspace(resumable);
        let mut resumed = 0usize;
        for r in to_resume {
            // Its own kind, not `Interactive`: a resumed fix run is still the
            // automation the rail colours teal and the guard table counts.
            match spawn::spawn_session(&app, &r.workspace, r.kind.clone(), Some(spawn::Source::Resume(r.id)))
                .await
            {
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

    // `gh auth token` was reported here as a finding, and it is gone for the reason
    // the `⚠` beside `pr_age_ms` went: a condition you have decided to live with is
    // not a finding, it is furniture, and furniture in a list you read deliberately
    // teaches you to stop reading it. The fact is still in the snapshot as
    // `token_source` for anyone diagnosing over the API, and TODO.md's decisions
    // section says the fallback is accepted.
    let review_bad = {
        let inner = app.inner.read().await;
        match &inner.reviews {
            crate::reviews::ReviewState::Degraded { reason } => Some(reason.clone()),
            // Pending is startup, not a fault.
            _ => None,
        }
    };

    if let Some(reason) = review_bad {
        out.push(todo::Finding {
            what: "review queue is unavailable".into(),
            why: format!("the forge is not answering the review query: {}", reason.trim()),
        });
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
            app.inner.write().await.reviews_polling = true;
            app.notify().await;
            // The command answers for itself: no `reviews_command` configured →
            // `Off`, a non-zero exit or unparseable output → `Degraded`. It shells
            // out, so it runs off the async runtime.
            let main = app.cfg.main_checkout.clone();
            let timeout = app.cfg.review_timeout_seconds;
            let command = app.cfg.reviews_command.clone();
            // For the URL fallback when a row omits one; `None` just means the
            // row does not link.
            let repo = app.repos.upstream.clone();
            let state = tokio::task::spawn_blocking(move || {
                reviews::fetch(&main, timeout, &command, repo.as_deref())
            })
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
                inner.reviews_polling = false;
                inner.reviews_fetched = Some(std::time::SystemTime::now());
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

/// Re-read the file list of workspaces somebody is sitting in.
///
/// Hooks are the primary signal (§4) but they only fire for things the agent does
/// through a tool. A `!` command typed into a session runs no tool, so no
/// `PostToolUse` arrives and no `Stop` either — a `git restore` that way changed
/// 849 files and the pane never heard. Same for an editor, a build, or a `git`
/// command in a shell tab.
///
/// Only workspaces with a live session, so an idle machine does no git at all.
fn start_workspace_watcher(app: Arc<AppState>) {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(15);
        loop {
            tokio::time::sleep(interval).await;
            let busy: std::collections::HashSet<String> = {
                let inner = app.inner.read().await;
                inner
                    .sessions
                    .values()
                    .filter(|s| s.state.is_live())
                    .map(|s| s.workspace.clone())
                    .collect()
            };
            if busy.is_empty() {
                continue;
            }
            for ws in busy {
                let _ = app.reconcile(&ws).await;
            }
            app.notify().await;
        }
    });
}

/// Catch a branch switch fast, without paying reconcile's git on a short timer.
///
/// A `git checkout` in any workspace — a shell tab, an editor, the agent — rewrites
/// that workspace's HEAD file. The 15s watcher only looks at workspaces with a live
/// session, and the PR poll is slower still, so a switch could sit unseen for the
/// better part of a minute. This reads each workspace's tiny HEAD file every couple
/// of seconds — a couple dozen bytes — and only when the contents change does it
/// run the expensive reconcile + snapshot push. A poll rather than inotify on
/// purpose: no dependency, no per-OS backend, and no watch to add and drop as
/// worktrees come and go — and the reconcile it triggers is the same path every
/// other refresh uses.
fn start_head_poller(app: Arc<AppState>) {
    use std::collections::hash_map::Entry;
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;

    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(2);
        // workspace id -> (its HEAD file, last-seen contents).
        let mut seen: HashMap<String, (PathBuf, String)> = HashMap::new();
        loop {
            tokio::time::sleep(interval).await;
            let spaces: Vec<(String, PathBuf)> = {
                let inner = app.inner.read().await;
                inner
                    .workspaces
                    .values()
                    .map(|w| (w.id.clone(), w.path.clone()))
                    .collect()
            };
            // Forget torn-down worktrees so the tracking map does not grow forever.
            let live: HashSet<&String> = spaces.iter().map(|(id, _)| id).collect();
            seen.retain(|id, _| live.contains(id));

            let mut changed = false;
            for (id, path) in spaces {
                match seen.entry(id.clone()) {
                    // First sight: cache the git-resolved HEAD path (a subprocess,
                    // so only once) and its current contents, without reconciling —
                    // the branch was already read when the workspace was registered.
                    Entry::Vacant(e) => {
                        let Ok(head) = git::head_file(&path) else { continue };
                        let contents = std::fs::read_to_string(&head).unwrap_or_default();
                        e.insert((head, contents));
                    }
                    Entry::Occupied(mut e) => {
                        let (head, last) = e.get_mut();
                        // A read can miss mid-rename; keep the old value and retry
                        // next tick rather than treat a blip as a change.
                        let Ok(contents) = std::fs::read_to_string(&*head) else { continue };
                        if contents != *last {
                            *last = contents;
                            let _ = app.reconcile(&id).await;
                            changed = true;
                        }
                    }
                }
            }
            if changed {
                app.notify().await;
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
            // Scoped, because the guard used to outlive the `if` and stay held
            // across the sleep below whenever the answer had not changed — which
            // is every poll, normally. That is the state write lock, so the
            // daemon spent 20 seconds out of every 20 holding it, and anything
            // that touched state waited for the gap: a spawn, a hook, the
            // rail's own snapshot.
            let changed = {
                let mut inner = app.inner.write().await;
                let changed = inner.stack_up != Some(up);
                if changed {
                    inner.stack_up = Some(up);
                }
                changed
            };
            if changed {
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
    let token_file = app.cfg.github_token_file.clone();
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(6 * 60 * 60);
        loop {
            let cur = current.clone();
            // The release repo is private, so the check rides the same token
            // ladder the PR poller uses. Resolved per poll, off-thread, so a
            // rotated token is picked up and a slow `gh auth token` never blocks
            // the runtime.
            let tf = token_file.clone();
            if let Ok(Some((tag, url))) = tokio::task::spawn_blocking(move || {
                let token = forge::resolve_token(tf.as_deref()).ok().map(|t| t.value);
                forge::latest_release(RELEASE_REPO.0, RELEASE_REPO.1, token.as_deref())
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
/// The directory the running executable sits in, when `orch` is really there.
///
/// Every packaging puts the two binaries side by side — the tarball, the `.deb`'s
/// `/usr/bin`, the AppImage's AppDir, the macOS bundle's `Contents/MacOS` — but
/// only the tarball's directory is on anybody's PATH. Answering `None` when the
/// sibling is missing keeps a development build (`cargo run`, where `orch` may
/// not have been built) from prepending a directory that has no `orch` in it.
pub fn sibling_bin_dir() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    dir.join("orch")
        .is_file()
        .then(|| dir.to_string_lossy().into_owned())
}

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
    let url = forge::remote_url(&app.cfg.main_checkout, &app.cfg.upstream_remote)?;
    forge::repo_from_remote(&url)
}

// ---------------------------------------------------------------------------
// SPA
// ---------------------------------------------------------------------------

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");
// A dev-only page that drives the real review overlay against canned data, so the
// flattened UI can be clicked without GitHub, CI or an agent. Reachable only if you
// know the path; it holds no secret beyond the app token every asset already carries.
const REVIEW_PREVIEW: &str = include_str!("../web/review-preview.html");

/// The token is embedded in the served page rather than fetched, so it never
/// exists as a value any other origin could ask for (§12).
async fn index(State(app): State<Arc<AppState>>) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store, must-revalidate")],
        Html(
            INDEX
                .replace("__ORCH_TOKEN__", &app.token)
                .replace("__ORCH_CHROME__", app.chrome.as_str())
                // Which key the app's own chords wear: ⌘ on a Mac, Ctrl
                // elsewhere. Told rather than sniffed — the daemon knows at
                // compile time, and `navigator.platform` is both deprecated and
                // a lie under a webview.
                .replace("__ORCH_PLATFORM__", if cfg!(target_os = "macos") { "mac" } else { "other" }),
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

/// The review-overlay preview page. Same token/platform substitution as `index`,
/// because the module graph reads `window.__ORCH__` at import time.
async fn review_preview(State(app): State<Arc<AppState>>) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store, must-revalidate")],
        Html(
            REVIEW_PREVIEW
                .replace("__ORCH_TOKEN__", &app.token)
                .replace("__ORCH_PLATFORM__", if cfg!(target_os = "macos") { "mac" } else { "other" }),
        ),
    )
        .into_response()
}

async fn asset_css() -> Response {
    asset("text/css; charset=utf-8", APP_CSS)
}

/// The SPA's own ES modules.
///
/// A flat, known set exactly like `vendor`: no traversal, and the compiled-in
/// file is the only thing servable. The content type must be a JavaScript one or
/// a `type="module"` script fetches it and then refuses to run it.
///
/// Every module needs a line here — `include_str!` means adding one is a Rust
/// change and a rebuild, not a JS-only change. That cost is why the modules track
/// the seams rather than being cut finer.
async fn module(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    if file.contains('/') || file.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad asset").into_response();
    }
    let body = match file.as_str() {
        "core.js" => include_str!("../web/js/core.js"),
        "term.js" => include_str!("../web/js/term.js"),
        "rail.js" => include_str!("../web/js/rail.js"),
        "diff.js" => include_str!("../web/js/diff.js"),
        "review.js" => include_str!("../web/js/review.js"),
        "review-diff.js" => include_str!("../web/js/review-diff.js"),
        "queue.js" => include_str!("../web/js/queue.js"),
        "settings.js" => include_str!("../web/js/settings.js"),
        _ => return (StatusCode::NOT_FOUND, "no such module").into_response(),
    };
    asset("text/javascript; charset=utf-8", body)
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
        // All Prism grammars, dependency-ordered, for diff/open-question
        // highlighting. Vendored whole rather than fetched: the daemon owns its
        // assets and must work offline, wherever the repo lives.
        "prism.min.js" => include_str!("../web/vendor/prism.min.js"),
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
        // Diffs only, and only the one weight they use.
        "jetbrains-mono-400.woff2" => {
            include_bytes!("../web/vendor/fonts/jetbrains-mono-400.woff2")
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One record per workspace comes back, and it is the oldest — the rule that,
    /// on a cold start, keeps two sessions that once shared a worktree from both
    /// re-hydrating into it.
    #[test]
    fn auto_resume_brings_back_one_session_per_workspace() {
        use std::time::{Duration, UNIX_EPOCH};
        let rec = |ws: &str, age_secs: u64| {
            let mut s = model::Session::new(
                uuid::Uuid::new_v4(),
                ws.to_string(),
                std::path::PathBuf::from("/tmp"),
                model::Kind::Interactive,
            );
            // Older = smaller created_at. Distinct so "oldest wins" is unambiguous.
            s.created_at = UNIX_EPOCH + Duration::from_secs(1_000_000 - age_secs);
            store::SessionRecord::of(&s)
        };

        // Two in one worktree, one in another, one in main. Newest listed first to
        // prove the sort, not the input order, decides.
        let newer_a = rec("wt-a", 10);
        let older_a = rec("wt-a", 90);
        let b = rec("wt-b", 50);
        let main = rec(MAIN, 5);
        let kept = first_per_workspace(vec![
            newer_a.clone(),
            b.clone(),
            main.clone(),
            older_a.clone(),
        ]);

        let by_ws: std::collections::HashMap<_, _> =
            kept.iter().map(|r| (r.workspace.clone(), r.id)).collect();
        assert_eq!(kept.len(), 3, "one per workspace: wt-a, wt-b, main");
        assert_eq!(by_ws.get("wt-a"), Some(&older_a.id), "the older of the two in wt-a wins");
        assert_eq!(by_ws.get("wt-b"), Some(&b.id));
        assert_eq!(by_ws.get(MAIN), Some(&main.id));
    }
}
