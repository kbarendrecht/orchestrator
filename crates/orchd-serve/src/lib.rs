//! The daemon: the server, and everything it starts.
//!
//! `main.rs` is the headless CLI over this; the desktop shell in `desktop/` is
//! the other caller. Everything the two share — startup order, the router, the
//! pollers — lives here so neither can drift from the other.
//!
//! **`orchd` is what the daemon knows; this crate is the daemon.** Every module
//! here answers HTTP or owns the process: `host` serves the page and the checkout
//! list, `hooks` takes what an agent reports, `ws` carries the snapshot and the
//! pty, `firstrun` is the bootstrap server for a machine with no config yet.
//! `docs/crate-split.md` has why the line is here.

pub mod firstrun;
pub mod hooks;
pub mod host;
pub mod link;
pub mod serving;
pub mod ws;

use anyhow::{Context, Result};
use axum::{
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use orchd::config::{self, Config};
use orchd::forge::{self, Forge};
use orchd::model::*;
use orchd::resolve_repo;
use orchd::state::{self, AppState};
use orchd::window;
use orchd::{
    api, bank, env_source, fix_pr, git, instance, machine, managed, model, proc, relocate,
    review_api, reviews, secret, skills, spawn, store, update,
};

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
    /// The origin of the host that spawned this daemon, if one did. Reaches
    /// [`config::Config::host_origin`], which is where the reasoning lives.
    pub host_origin: Option<String>,
    /// Start without resuming the sessions that were live, whatever the config
    /// says.
    ///
    /// **For one path only: a checkout being added back to the host.** A host that
    /// closed a checkout leaves its records alone, because closing a checkout is a
    /// statement about the window and not a decision about its conversations — so
    /// a re-add asks, and this is the answer "start empty" travelling to the
    /// daemon. An ordinary restart still resumes silently, which is why this is an
    /// option on one start rather than a change to `auto_resume`.
    ///
    /// The host stays out of the store: it says what the person chose, and the
    /// daemon is the only thing that knows what a session is.
    pub no_resume: bool,
}

impl Default for StartOptions {
    fn default() -> Self {
        StartOptions {
            main_checkout: None,
            fallback_port: false,
            chrome: window::Chrome::None,
            host_origin: None,
            no_resume: false,
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
    /// The page, the window and the checkout list. Shares this process while the
    /// app embeds its daemon; [`crate::host`] says what changes when it does not.
    pub host: Arc<crate::host::Host>,
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

    /// Run every managed process's `stop_command`, before anything is killed.
    ///
    /// Sequential and bounded: there is normally one such process, and a restart
    /// of the app is waiting on this.
    async fn stop_declared_processes(&self) {
        let declared: Vec<(String, String, std::sync::Arc<orchd::pty::PtyHandle>)> = {
            let inner = self.app.inner.read().await;
            inner
                .workspaces
                .values()
                .flat_map(|w| {
                    w.processes.iter().filter_map(|p| {
                        p.pty
                            .as_ref()
                            .filter(|h| h.is_alive())
                            .map(|h| (w.id.clone(), p.name.clone(), h.clone()))
                    })
                })
                .collect()
        };
        for (workspace, name, pty) in declared {
            if self
                .app
                .cfg
                .processes_for(&workspace)
                .iter()
                .any(|s| s.name == name && !s.stop_command.is_empty())
            {
                orchd::managed::stop_managed(&self.app, &workspace, &name, &pty).await;
            }
        }
    }

    /// Kill every child this daemon owns, then stop serving.
    ///
    /// Sessions and managed processes both go: an orchd that is not running is
    /// not supervising `ng-watch`, and a build watcher nobody is watching is
    /// just a CPU leak with a log file.
    ///
    /// This reaches the ptys, and whatever a process declares as its
    /// `stop_command`. `docker compose up -d` detaches and its pty child is long
    /// gone by the time we get here, so the containers keep running — which is the
    /// intent. Long-lived containers are infrastructure; `ng-watch` is a process
    /// this app started and should therefore finish, wherever it is running.
    pub async fn shutdown(&self) {
        // Before anything is killed: every pty about to die wakes an exit watcher,
        // and one of them would otherwise read a restart as "you finished with
        // main" and move the checkout out from under auto-resume.
        self.app
            .shutting_down
            .store(true, std::sync::atomic::Ordering::SeqCst);

        /* **The resume set is captured here and written at the very end.** It is
        read off session state, which the exit watchers are about to rewrite, so
        it has to be taken before anything is killed. It used to be *written*
        here instead, and that is what stopped shutdown escalating a kill: any
        await point after the kills let the watchers run and re-persist, and
        auto-resume then found every session `was_live: false` and restored
        nothing (caught by the restart e2e flow).
        `AppState::persist` now refuses to write while `shutting_down` is set, so
        the last word on disk is this set rather than whichever watcher ran last
        — and the kills below can be waited on. */
        let resume_set = self.app.session_records().await;

        // And before the lock is taken, because each of these is a bounded child
        // process. A watcher running through `docker compose exec` outlives its
        // client, so killing the pty alone leaves it in the container — which is
        // how five of them stacked up.
        self.stop_declared_processes().await;

        // The handles, out from under the lock: the escalation below awaits, and
        // holding the write lock across it would park every hook and snapshot for
        // as long as the slowest child takes to go.
        let handles: Vec<std::sync::Arc<orchd::pty::PtyHandle>> = {
            let mut inner = self.app.inner.write().await;
            let mut all: Vec<_> = inner
                .sessions
                .values()
                .filter_map(|s| s.pty.clone())
                .filter(|h| h.is_alive())
                .collect();
            for w in inner.workspaces.values_mut() {
                // The pty only. A `stop_command` is a bounded child of its own and
                // `stop_declared_processes` above did that half already, with the
                // lock released.
                all.extend(
                    w.processes
                        .iter()
                        .filter_map(|p| p.pty.clone())
                        .filter(|h| h.is_alive()),
                );
                w.processes.clear();
            }
            all
        };
        let killed = handles.len();

        /* **`SIGHUP`, a grace, then `SIGKILL` — the same escalation every other stop
        path gets.** One `SIGHUP` is a request a child is entitled to decline, and
        this used to be the one caller that could only ask: an agent that traps it
        was left to the pty master closing as the process exited, and a child that
        survives even that was never reached at all.

        **In parallel, so the grace is spent once rather than once per child.**
        `kill_gracefully` is bounded by construction (two `KILL_GRACE` waits at
        worst), so a board of thirty sessions still closes in seconds — and the
        ordinary case is unchanged, since a child that goes on `SIGHUP` resolves
        the first wait in under a millisecond.

        Measured against a managed process spelled `trap '' HUP; sleep 1000`:
        shutdown took 2.05s, said so in the log, and both that shell and its
        `sleep` grandchild were gone. Before this it exited at once and left
        them. */
        let mut going = tokio::task::JoinSet::new();
        for h in handles {
            going.spawn(async move { h.kill_gracefully().await });
        }
        while going.join_next().await.is_some() {}
        tracing::info!("shutdown: killed {killed} child process(es)");

        /* And now the set captured before any of that, verbatim. Every watcher woken
        by those kills has had its say and `persist` has been refusing them all
        along, so this is the only thing that writes the file after the killing —
        which is what auto-resume reads next launch. */
        let written = orchd::proc::run_blocking("persisting the resume set", move || {
            store::save(&resume_set)
        })
        .await;
        if let Ok(Err(e)) | Err(e) = written {
            tracing::warn!("could not persist the resume set: {e:#}");
        }
        self.serve.abort();
    }
}

/// Bring the daemon up and start serving.
///
/// Returns as soon as the listener is bound, so a caller with a window to open
/// has a port and a token to point it at. Everything slower than that — the
/// upstream fetch, auto-resume, the pollers — runs on its own tasks.
pub async fn start(opts: StartOptions) -> Result<Server> {
    // Everything down to the `serve` spawn holds the window shut, and all of it
    // is child processes and file reads rather than work this machine can be
    // fast at. Marked per phase because "the app takes twelve seconds to open"
    // is not a report anybody can act on, and it was the only report there was.
    let mut phases = orchd::timing::Phases::start();

    // First, and before anything is written: a second daemon would spawn into
    // the same worktrees and rewrite the hook settings with its own port.
    let lock = instance::acquire()?;

    let mut cfg = Config::load_or_init(opts.main_checkout)?;
    // From the argv of whoever spawned us, never from the file this just read.
    cfg.host_origin = opts.host_origin.clone();
    let cfg_host_origin = cfg.host_origin.clone();
    check_config(&cfg)?;
    // Read before `cfg` is moved into the state, and used by the phase line at the
    // end of this function.
    let cfg_name = cfg.checkout_name();
    /* **Every open counts as recent, not only the ones picked in the picker.**
    `firstrun` recorded the list from its own switch route alone, so a daemon
    that started on the checkout already in `config.json` — which is every
    launch after the first — wrote nothing. The list the "open a project"
    screen offers was therefore empty for anyone who had never switched, and
    the one project they actually use was the one entry it could not show.
    Best effort: a list that cannot be written is not a reason to refuse a
    start.

    **Only when nothing else hosts this daemon.** `recent.json` is the host's
    list, and a hosted child's `ORCHD_CONFIG_DIR` is its own checkout directory
    — so a child writing it would leave one single-entry list per checkout and
    none of them the one the "add a checkout" screen reads. A solo `orchd` is
    its own host, which is exactly what this arm says. `host::Host::open_checkout`
    is the other writer. */
    if cfg.host_origin.is_none() {
        if let Err(e) = firstrun::record_recent(&cfg.main_checkout) {
            tracing::warn!("could not record the recent project: {e:#}");
        }
    }
    phases.mark("config");

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

    /* Said before the first session can be spawned, because every one of these
    otherwise surfaces as a failure that blames something else — and **kept**,
    rather than only logged, because a launcher-started app has no terminal and
    the person reading `unavailable` in the PR pane has no way to reach the log.
    Handed to the state below, as soon as there is one. */
    let warnings = {
        /* Off the runtime, because every check here is a blocking `which`, a file
        read or a `git`, and CLAUDE.md's rule about that is not a style note: a
        `which` against a stalled mount parks the worker with no yield point, and
        the host's own health probe of this child is on it. */
        let cfg2 = cfg.clone();
        tokio::task::spawn_blocking(move || {
            let server = cfg2.tracker.as_ref().map(|t| t.mcp_server.as_str());
            machine::check(&cfg2, server)
        })
        .await
        /* **A preflight that did not finish is itself a finding.** Folded to an empty
        list it reads as a healthy machine, so the panes go on saying `unavailable`
        with the cause nowhere — the exact silence this bar was added to end,
        reachable through the one path nobody looks at. */
        .unwrap_or_else(|e| {
            vec![orchd::machine::Warning {
                what: "the boot preflight did not finish".into(),
                cost: format!("nothing below was checked: {e}"),
            }]
        })
    };

    let settings = {
        for w in &warnings {
            tracing::warn!("{} — {}", w.what, w.cost);
        }
        // The push guard protects the branch this repo is measured against, so it
        // is read from config rather than a list of likely names. `origin/HEAD`
        // that has never been fetched resolves to nothing, and the guard then
        // enforces the force-with-lease rule alone.
        let base = git::base_checkout_branch(&cfg.main_checkout, &cfg.upstream_ref);
        hooks::write_settings(
            cfg.port,
            cfg.tracker.as_ref().map(|t| t.mcp_server.as_str()),
            base.as_deref(),
            &cfg.main_checkout,
            &cfg.worktrees_dir(),
        )?
    };
    tracing::info!("hook settings at {}", settings.display());
    // Non-fatal on purpose: a session without the `orch` skill still works, and
    // Claude Code ignores a `--plugin-dir` that is not there — so the flag every
    // spawn pushes costs nothing when this failed.
    match skills::write_plugin() {
        Ok(at) => tracing::info!("session skills at {}", at.display()),
        Err(e) => tracing::warn!("could not write the session skills: {e:#}"),
    }
    // `machine::check`, the base-branch read and the settings write together.
    // They share a phase because they share a cause: each is a small run of
    // child processes, and the fix for any of them is the same fix.
    phases.mark("preflight");

    // Repo config from §4. fsmonitor is deliberately main-only. Not fatal, but
    // not silent either: a checkout without fsmonitor scans the whole tree on
    // every status, and that is worth one line when it is the reason.
    if let Err(e) = git::configure_repo(&cfg.main_checkout) {
        tracing::warn!("could not configure the repo: {e:#}");
    }

    let token = secret::random_token();
    let app = AppState::new(cfg, token.clone(), opts.chrome);
    // Before the first snapshot anyone can read, so a board that opens on a broken
    // machine says so on its first paint rather than after the next poll.
    app.set_machine_warnings(warnings).await;
    // Before anything can spawn a session, `auto_resume` included: a run whose
    // exit nobody is listening for is a verdict that never lands.
    app.observe_runs(settle_run);

    // Keep the base ref fresh or the merge-base the context bar shows drifts
    // (§5). Offline is not fatal — the last-known ref still resolves.
    if let Err(e) = git::fetch_upstream(&app.cfg.main_checkout, &app.cfg.upstream_ref) {
        tracing::warn!("upstream fetch failed, using last-known ref: {e:#}");
    }
    // The one phase here that is a network round trip, so it is the one whose
    // cost depends on where you are sitting rather than on the machine.
    phases.mark("fetch");

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
            .map(|r| {
                (
                    r.id,
                    r.workspace.clone(),
                    r.cwd.clone(),
                    r.transcript_path.clone(),
                )
            })
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
                format!("{} ({ws})", orchd::model::short_id(&id))
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
    // A transcript read per record, plus a `find_transcript` scan for the ones
    // whose path is not pinned yet. Grows with how many conversations you keep.
    phases.mark("records");
    adopt_existing_worktrees(&app).await?;
    app.restore_sessions(records.clone()).await;
    phases.mark("adopt");
    {
        let mut inner = app.inner.write().await;
        inner.automation = state::Durable::new(store::load_automation());
        inner.stories = state::Durable::new(store::load_stories());
        // Said out loud at boot, because `tracker` decides whether a whole option
        // appears on every review card. A misconfigured one must not read as
        // "triage never proposes stories".
        match app.cfg.tracker.as_ref().map(|t| t.mcp_server.as_str()) {
            None => tracing::info!("tracker: none — `story+reply` is off"),
            /* A tracker that names no token variable authenticates itself — both
            official Linear and Atlassian servers are OAuth-first — so there is
            nothing to resolve, and a warning here would be about a credential
            the daemon was never meant to hold. */
            Some(server) => match app
                .cfg
                .tracker
                .as_ref()
                .and_then(|t| t.token_env.as_deref())
            {
                None => tracing::info!(
                    "tracker: {server}, authenticating itself, {} story/ies cached",
                    inner.stories.len()
                ),
                Some(var) => {
                    // The main checkout's own environment is the filer's fallback,
                    // so the boot line has to read it too. Without this it would
                    // warn about a missing token that a run then finds.
                    let checkout = env_source::read(app.cfg.env_source, &app.cfg.main_checkout);
                    match config::resolve_token(&checkout, var) {
                        Ok(_) => tracing::info!(
                            "tracker: {server}, token resolved, {} story/ies cached",
                            inner.stories.len()
                        ),
                        Err(e) => tracing::warn!("tracker: {server} but no usable token — {e:#}"),
                    }
                }
            },
        }
    }
    // The tracker's boot line asks the env source for the main checkout, which
    // is a bounded child process of somebody else's tool.
    phases.mark("stores");
    /* **The sweep is spawned, not awaited, and that is the whole of the startup
    fix.** It was seven git runs per workspace, one workspace after another,
    with the window shut for all of it: 6294ms of a 7836ms start over 64
    worktrees, 447 child processes. None of it is needed to serve the page —
    the rail, the terminals and the session records are all already in hand —
    so the only thing awaiting it bought was a first snapshot with the
    changed-file lists already filled.

    That is a real thing to give up, which is why `Tree::measured` exists: the
    pane can now say "still counting" instead of showing an unmeasured tree as
    a clean one. Every snapshot after each workspace lands carries the answer
    through, so the panes fill in as the sweep walks. */
    tokio::spawn({
        let app = app.clone();
        async move { reconcile_all(&app).await }
    });
    phases.mark("reconcile-spawn");
    adopt_banked_work(&app).await;
    autostart_processes(&app).await;
    if app.cfg.auto_resume && !opts.no_resume {
        auto_resume(app.clone(), records);
    }
    start_pr_poller(app.clone());
    start_review_poller(app.clone());
    start_stack_poller(app.clone());
    start_workspace_watcher(app.clone());
    start_head_poller(app.clone());
    start_title_poller(app.clone());
    start_worktree_reaper(app.clone());
    start_external_poller(app.clone());
    start_restart_watcher(app.clone());
    orchd::update::start_stale_poller(app.clone());
    /* The spare pool, re-attached to the worktrees `adopt_existing_worktrees`
    just rediscovered. Spawned for the same reason the sweep above is: the
    reconcile is a map lookup, but the refill behind it is a `git worktree add`
    and a repo hook, and a start must not wait 4.4 seconds for a tree nobody has
    asked for yet. */
    tokio::spawn({
        let app = app.clone();
        async move { orchd::spare::adopt_at_boot(&app).await }
    });
    // A debug build is `cargo run` from a checkout; its version is whatever the
    // working tree is, so comparing it against a release only ever nags. Only a
    // release build — which is what a downloaded/`mise`-installed one is — checks.
    if !cfg!(debug_assertions) {
        update::start_release_poller(app.clone());
        // The agent's own version, which is the one that nags you in a terminal.
        update::start_agent_poller(app.clone());
    }

    // The host, for this one checkout. It carries the daemon's token and port
    // because one process serves both; a child daemon mints its own and reports
    // them, which is why `Checkout` carries both rather than the page assuming.
    let host = crate::host::Host::new(app.token.clone(), app.cfg.port, opts.chrome);
    host.record(crate::host::Checkout {
        path: app.cfg.main_checkout.to_string_lossy().into_owned(),
        name: cfg_name.clone(),
        port: app.cfg.port,
        token: app.token.clone(),
        live: true,
        repo: resolve_repo(&app).map(|(o, n)| format!("{o}/{n}")),
        // One checkout, so nothing to clash with.
        clash: None,
    });
    // **A daemon a host spawned does not serve the page.** Its own host would be a
    // second page server nobody visits, on a port whose only caller is the parent,
    // and two owners for one page is the conflation this split exists to remove.
    // `host_origin` is exactly the question "did somebody host me": it arrives on
    // the argv of the process that spawned this one and cannot come from a file.
    let hosted_elsewhere = cfg_host_origin.is_some();
    let router = if hosted_elsewhere {
        daemon_router(app.clone())
    } else {
        router(app.clone(), host.clone())
    };
    // `TCP_NODELAY` and the reason for it are in `serving`: the pty websocket is
    // nothing *but* small frames in both directions, a character out and the
    // redrawn line back.
    let serve = serving::spawn("the daemon", listener, router);
    phases.mark("serve");
    // Named, because with a daemon per checkout two of these lines are otherwise
    // indistinguishable — and this line exists so a number survives being pasted
    // into a chat message, where it arrives without the file it came from.
    phases.log(&format!("daemon start [{}]", cfg_name));

    Ok(Server {
        port,
        token,
        app,
        host,
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

/// The HTTP surface: the daemon's routes, with the host's merged in.
///
/// **Two routers rather than one list**, because they answer to different owners:
/// `/api/*`, `/ws/*` and `/hooks/*` belong to the daemon that manages one
/// checkout, and the page, the window and the checkout list belong to the host.
/// They share a port while one process serves both — see [`crate::host`] — and
/// each keeps its own guard, so the daemon's arms for hooks and agent routes stay
/// where they are and the host's stay narrow.
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
fn router(app: Arc<AppState>, host: Arc<crate::host::Host>) -> Router {
    crate::host::router(host).merge(daemon_router(app))
}

fn daemon_router(app: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/state", get(api::get_state))
        .route("/api/config", get(api::get_config).post(api::set_config))
        .route("/api/diff", get(api::diff_summary))
        .route("/api/diff/file", get(api::diff_file))
        .route("/api/search", get(api::search))
        .route("/api/archive/search", get(api::search_archive))
        .route("/api/paths", get(api::paths))
        .route("/api/def", get(api::definitions))
        .route("/api/file", get(review_api::read_file))
        .route("/api/file", post(review_api::write_file))
        .route("/api/session", post(api::new_session))
        .route("/api/session/:id/kill", post(api::kill_session))
        .route("/api/session/:id/rename", post(api::rename_session))
        .route(
            "/api/session/:id/out-of-main",
            post(relocate::move_out_of_main),
        )
        .route("/api/session/:id/rewind", post(api::rewind_session))
        .route("/api/session/:id/resume", post(api::resume_session))
        .route("/api/session/:id/restart", post(api::restart_session))
        .route("/api/sessions/restart", post(api::restart_sessions))
        .route("/api/external/refresh", post(api::refresh_external))
        .route("/api/external/:id/resume", post(api::resume_external))
        .route("/api/sessions/nudge", post(api::nudge_sessions))
        .route("/api/session/:id/fork", post(api::fork_session))
        .route("/api/session/:id/spawn", post(api::spawn_from_session))
        // Both halves of the worktree grant on one path: the agent asks with a
        // POST, the push guard reads with a GET before it refuses anything.
        .route(
            "/api/session/:id/outside",
            post(api::allow_outside).get(api::outside_allowed),
        )
        // `/discard` rather than the `/kill` or `/delete` the SPA already uses:
        // `is_ask_route` matches by *suffix*, so reusing either name would hand the
        // agent the rail's own unrestricted verbs on every session at once.
        .route(
            "/api/session/:id/spawned/:child/discard",
            post(api::discard_spawned),
        )
        .route("/api/session/:id/process", post(api::process_from_session))
        // The workspace travels in the body, not the path, for the same suffix
        // reason: `/api/session/:id/teardown/:workspace` would end in a name the
        // matcher cannot know.
        .route(
            "/api/session/:id/teardown",
            post(api::teardown_from_session),
        )
        .route(
            "/api/session/:id/handoff",
            post(review_api::session_handoff),
        )
        // Where a review session's replies actually leave the machine; the rules
        // live in `post_one` rather than in the skill. See `thread_reply`.
        .route(
            "/api/session/:id/thread/:thread/reply",
            post(review_api::thread_reply),
        )
        // Who is asked to look again is derived from a fresh fetch, not from what
        // the session believes it posted. See `session_rerequest`.
        .route(
            "/api/session/:id/rerequest",
            post(review_api::session_rerequest),
        )
        .route("/api/session/:id/tell", post(api::tell_session))
        .route("/api/session/:id/ask", post(api::ask))
        .route("/api/session/:id/ask/:ask/wait", get(api::ask_wait))
        .route("/api/session/:id/answer", post(api::answer))
        .route("/api/session/:id/delete", post(api::delete_session))
        .route("/api/worktree", post(api::new_worktree))
        .route("/api/workspace/:id/shell", post(api::new_shell))
        .route("/api/workspace/:id/reconcile", post(api::reconcile))
        .route("/api/workspace/:id/rebase", post(bank::rebase))
        .route("/api/workspace/:id/rebase/abort", post(bank::rebase_abort))
        .route("/api/workspace/:id/wip/restore", post(bank::wip_restore))
        .route("/api/workspace/:id/wip/discard", post(bank::wip_discard))
        .route("/api/workspace/:id/wip/resolve", post(bank::wip_resolve))
        .route("/api/workspace/:id/preflight", get(api::preflight))
        .route("/api/workspace/:id/teardown", post(api::teardown))
        .route(
            "/api/workspace/:id/swap-main",
            post(relocate::swap_with_main),
        )
        .route(
            "/api/workspace/:id/process/:name/restart",
            post(api::restart_process),
        )
        .route("/api/process/:id/close", post(api::close_process))
        .route("/api/reviews/refresh", post(api::refresh_reviews))
        .route("/api/prs/refresh", post(api::refresh_prs))
        // The agent's own version: check it now, and install it in the drawer.
        .route(
            "/api/agent/upgrade/dismiss",
            post(api::dismiss_agent_upgrade),
        )
        .route("/api/agent/upgrade", post(api::upgrade_agent))
        .route(
            "/api/update/upgrade/dismiss",
            post(api::dismiss_app_upgrade),
        )
        .route("/api/update/upgrade", post(api::upgrade_app))
        // The page's own boot timing, so a slow start reads as one story rather
        // than a daemon log with a hole where the webview should be.
        .route("/api/client/timing", post(api::client_timing))
        .route("/api/client/note", post(api::client_note))
        .route("/api/open", post(api::open_url))
        .route("/api/open-all", post(api::open_urls))
        .route("/api/open/file", post(api::open_file))
        .route("/api/open/reveal", post(api::reveal_path))
        .route("/api/file/verb", post(api::file_verb))
        .route("/api/pr/:number/review", get(review_api::pr_review))
        // The overlay session's first call: everything only the daemon knows.
        .route(
            "/api/pr/:number/triage-context",
            get(review_api::pr_triage_context),
        )
        // The rail's default review verb: one agent and one pane, which the
        // overlay is not good enough to replace yet.
        .route(
            "/api/pr/:number/handle-review",
            post(review_api::pr_handle_review),
        )
        .route(
            "/api/pr/:number/review-session",
            post(review_api::pr_review_session),
        )
        // The one route a subprocess calls. Hostile input; see `pr_proposals`.
        .route("/api/pr/:number/proposals", post(review_api::pr_proposals))
        // The rail's default: spawn a session running `/resolve <pr>` in a pane.
        .route("/api/pr/:number/open", post(review_api::open_pr))
        .route("/api/pr/:number/fix-pr", post(api::fix_pr))
        .route("/api/preview", post(orchd::preview::open))
        .route("/api/file/image", get(orchd::preview::image))
        // Outside `/api/`: a sandboxed frame reads it, on a token of its own.
        .route("/preview/:token/*path", get(orchd::preview::serve))
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
    let main = app.cfg.main_checkout.clone();
    let entries =
        proc::run_blocking("listing worktrees", move || git::worktree_list(&main)).await??;
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

/// The order a sweep should visit workspaces in, so the pane you are looking at
/// fills first.
///
/// It used to be `HashMap` order, which is arbitrary, and that was fine while the
/// whole sweep finished before the window existed. Now that the window opens
/// first, the order *is* the perceived speed: with 64 worktrees, landing last in
/// an arbitrary order means six seconds of loader on the one pane being read.
///
/// Sessions first, because after a restart those are the records `auto_resume` is
/// bringing back and the selection lands on one of them. Then main, which the
/// context bar reads even when nothing is selected. Then the rest, which nobody
/// is looking at until they go looking, and by then this has finished.
fn sweep_order(inner: &state::Inner) -> Vec<String> {
    let mut ids: Vec<String> = inner.workspaces.keys().cloned().collect();
    // Archived counts. At boot every restored session is `Archived` until
    // auto-resume spawns it, so ranking on *live* would rank nothing at all —
    // which is the case this ordering exists for.
    let occupied: std::collections::HashSet<&str> = inner
        .sessions
        .values()
        .map(|s| s.workspace.as_str())
        .collect();
    ids.sort_by_key(|id| {
        let rank = if occupied.contains(id.as_str()) {
            0
        } else if id == MAIN {
            1
        } else {
            2
        };
        // The id as a tiebreak, so a sweep is deterministic and two of them
        // report the same thing in the same order.
        (rank, id.clone())
    });
    ids
}

/// How many workspaces a sweep measures at once.
///
/// The sweep is bound by process starts, not by CPU: seven git processes per tree,
/// and on a Mac every exec costs 8 to 9 ms before git does anything (#10 measured
/// 200 execs of `/usr/bin/true` at 1.7 s, with or without the endpoint agent). 58
/// trees took 20 to 46 s one at a time, and the per-tree half of that product is
/// nothing a user can configure away, so the width is the only side that moves.
/// Four rather than "all of them": the old concern that 64 git processes at once
/// turn a slow start into a slow machine still holds, and four is four blocking
/// threads and four `git status` reads, which no machine notices.
const SWEEP_WIDTH: usize = 4;

/// What one pass of the sweep did with a workspace.
#[derive(Debug, PartialEq, Eq)]
enum Swept {
    /// The tree was there and was measured, or git said why it could not be,
    /// which `reconcile` logs itself.
    Measured,
    /// The directory is gone, so nothing was run in it.
    Skipped,
}

/// Measure one workspace, or skip it when its directory is not there.
///
/// **A row whose tree is gone is kept, on purpose.** `claude --worktree` removes
/// its own tree when its session ends, and a person runs `git worktree remove` by
/// hand; either way the record is the point the PR flows and `revive` rebuild the
/// tree *at* (`recorded_worktree_for` says why a second tree elsewhere is worse).
/// What the row must not do is cost anything meanwhile: measuring it ran seven git
/// processes into ENOENT and logged a warning per sweep, 68 of them after 34 trees
/// were removed by hand (#10), and the tally still counted the ghosts.
async fn sweep_one(app: &Arc<AppState>, id: &str) -> Swept {
    // Main is canonicalised in `Config::parse`, so it is only ever absent when the
    // checkout itself went, and a warning is then the right answer.
    if id != MAIN {
        if let Some(path) = app.workspace_path(id).await {
            if !path.is_dir() {
                tracing::debug!(workspace = %id, "tree is gone, not measured: {}", path.display());
                return Swept::Skipped;
            }
        }
    }
    if let Err(e) = app.reconcile(id).await {
        tracing::warn!("reconcile {id} failed: {e:#}");
    }
    Swept::Measured
}

/// Measure every workspace's tree, [`SWEEP_WIDTH`] at a time.
///
/// Off the boot path, so what makes it *feel* fast is [`sweep_order`]: the first
/// tasks started are the panes being looked at. The width is what makes it *be*
/// fast on a machine where an exec is expensive; see the constant.
async fn reconcile_all(app: &Arc<AppState>) {
    let Ok(_sweep) = app.sweeping.try_lock() else {
        tracing::debug!("a reconcile sweep is already running; skipping this one");
        return;
    };
    let ids = sweep_order(&*app.inner.read().await);
    let total = ids.len();
    let began = std::time::Instant::now();
    let mut queue = ids.into_iter();
    let mut running = tokio::task::JoinSet::new();
    let mut skipped = 0usize;
    loop {
        // Topped up in sweep order, so the visible panes are the first four in
        // flight and a slow tree elsewhere never holds a slot they need.
        while running.len() < SWEEP_WIDTH {
            let Some(id) = queue.next() else { break };
            let app = app.clone();
            running.spawn(async move { sweep_one(&app, &id).await });
        }
        match running.join_next().await {
            None => break,
            Some(Ok(Swept::Skipped)) => skipped += 1,
            // Per workspace, not per sweep. The pane is on screen while this runs,
            // so each answer has to reach it as it lands rather than 64 of them at
            // the end, which would be the loader sitting there for the whole sweep
            // and then everything appearing at once.
            Some(Ok(Swept::Measured)) => app.notify().await,
            Some(Err(e)) => tracing::warn!("a reconcile task died: {e}"),
        }
    }
    let ms = began.elapsed().as_millis();
    let measured = total - skipped;
    if skipped == 0 {
        tracing::info!("reconciled {measured} workspace(s) in {ms}ms");
    } else {
        tracing::info!(
            "reconciled {measured} workspace(s) in {ms}ms, skipped {skipped} whose tree is gone"
        );
    }
}

/// Find the work a previous run of the daemon parked out of a rebase's way.
///
/// **One exec for every workspace**, which is what keeps this off the sweep: refs
/// live in the repository, not in a worktree, so `git for-each-ref` on main lists
/// every bank there is. The alternative was a field on `Tree` and an eighth git
/// child per tree per sweep, on a walk whose entire cost is child processes.
///
/// **A bank is matched to a workspace by computing its ref, never by reading a
/// workspace out of one.** `git::wip_ref` mangles the two names git will not take,
/// so the reverse is a guess — and the two names it exists for would have been the
/// two it got wrong.
///
/// A bank with no workspace to attach to is logged and left where it is. The ref is
/// the only record of that work, and this runs once per start: a tree registered
/// later in this process (a `revive`, a PR flow rebuilding one) gets its strip back
/// on the next start rather than the moment it appears. Worth knowing before
/// trusting the strip to be the whole truth; `git for-each-ref refs/orchd/wip` is.
async fn adopt_banked_work(app: &Arc<AppState>) {
    let main = app.cfg.main_checkout.clone();
    let Ok(found) = orchd::proc::run_blocking("looking for banked work", move || {
        orchd::git::all_banked(&main)
    })
    .await
    else {
        return;
    };
    if found.is_empty() {
        return;
    }
    let known: Vec<String> = {
        let inner = app.inner.read().await;
        inner.workspaces.keys().cloned().collect()
    };
    for (at, bank) in found {
        match known.iter().find(|ws| orchd::git::wip_ref(ws) == at) {
            Some(workspace) => {
                tracing::info!(%workspace, files = bank.files, "adopted banked work at {}", bank.sha);
                app.set_banked(workspace, Some(bank)).await;
            }
            None => tracing::info!("{at} holds banked work and names no workspace we know"),
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
        if let Err(e) = managed::start_managed(app, MAIN, &spec).await {
            tracing::warn!("could not start {}: {e:#}", spec.name);
        }
    }
}

/// What a finished run owes, dispatched by the one place that knows every module.
///
/// **This is the inversion [`orchd::state::RunObserver`] exists for.** `spawn` owns
/// the only `pty.wait()`, so it learns that a run is over — and it used to settle
/// the run by naming `fix_pr::settle` and `fix_pr::start`, which is why `spawn`
/// imported the module that calls `spawn_run` to start one. The wiring belongs
/// here, where the pollers are already wired and every module is already in scope.
///
/// Installed once per process, so a session resumed by `auto_resume` — which
/// rebuilds a run from its persisted `Pass` and nothing else — is dispatched
/// exactly as the first spawn was.
fn settle_run(
    app: Arc<AppState>,
    exit: orchd::state::RunExit,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        // A fix run's verdict belongs to `fix_pr`.
        if exit.pass.command == model::Pass::FIX_PR {
            fix_pr::settle(&app, exit.pass.pr, exit.session).await;
        }
        /* The review that asked, on its way out, for the CI it is not allowed to
        touch to be picked up by a run. The branch is free by now, which is the only
        reason this waits for the exit.

        A refusal is not raised, because by here nobody is waiting: the guard table's
        reasons are written for whoever asked, and the rail's own `fix` button is
        still there to be pressed and will say the same thing. But it is *taken
        back*. `handed_off` is what tells the overlay to hold its report and wait for
        a run, so a refusal that left the flag standing would strand the review on
        "applying" for good — the fault this whole hand-off was built to fix. */
        if exit.hand_off {
            let pr = exit.pass.pr;
            match fix_pr::start(&app, pr).await {
                Ok(session) => {
                    tracing::info!(pr, %session, "review handed the checks to a fix-pr run")
                }
                Err(e) => {
                    tracing::warn!(pr, "review's hand-off to fix-pr refused: {e}");
                    if let Some(s) = app.inner.write().await.sessions.get_mut(&exit.session) {
                        s.fix_pr_on_exit = false;
                    }
                }
            }
        }
    })
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
        /* **`start` has already done this pass**, so the first tick skips it: it
        fetches the base ref and spawns the first sweep before this task exists,
        and repeating them is a network round trip and a walk of every worktree
        for an answer just given. (CLAUDE.md carries what the two cost.)

        Skipping is safe because boot's fetch is *unconditional* and awaited: by
        now it has either refreshed the ref or logged that it could not, and the
        second case is the offline one `start` already treats as "the last-known
        ref still resolves". The worst this costs is a base one poll interval
        staler, in the case where fetching does not work anyway.

        The sweep half was already *usually* skipped by `AppState::sweeping`,
        which made the behaviour depend on which of the two finished first. Not
        doing it at all is the same outcome without the race; the lock stays for
        the genuinely concurrent cases (a manual reconcile, the workspace
        watcher, a later tick that overruns). */
        let mut boot_already_did_this = true;
        loop {
            if boot_already_did_this {
                boot_already_did_this = false;
            } else {
                // Piggyback the upstream fetch on this timer (§5): the merge-base
                // and the behind count are both answered from that ref.
                let main = app.cfg.main_checkout.clone();
                let base = app.cfg.upstream_ref.clone();
                let _ =
                    tokio::task::spawn_blocking(move || git::fetch_upstream(&main, &base)).await;
                reconcile_all(&app).await;
                // After the fetch, because "behind the base" is the question this
                // answers and the fetch is what makes the answer current.
                orchd::spare::refresh(&app).await;
            }

            app.inner.write().await.pr_polling = true;
            app.notify().await;
            // Off the runtime. The ladder ends at `gh auth token`, a bounded child
            // process whose pipes are drained on threads of their own, and this
            // runs on every tick.
            let file = app.cfg.github_token_file.clone();
            let token = orchd::proc::run_blocking("resolving the GitHub token", move || {
                forge::resolve_token(file.as_deref())
            })
            .await
            .unwrap_or_else(Err);
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
                    let forge = forge::ForgeImpl::for_kind(
                        app.cfg.forge,
                        repo.0.clone(),
                        repo.1.clone(),
                        t.value,
                    );
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
                                        Some(model::PrAutomation::Running { .. })
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
            next_tick(interval, &app.pr_refresh).await;
        }
    });
}

/// Wait out a poll interval, unless somebody presses refresh first.
///
/// **A refresh cuts the wait short *and* restarts the period**, so a button press
/// and the next scheduled poll never land back to back. Both pollers with a
/// button spelled this out; a third would have had to know to.
async fn next_tick(interval: std::time::Duration, refresh: &tokio::sync::Notify) {
    tokio::select! {
        _ = tokio::time::sleep(interval) => {}
        _ = refresh.notified() => {}
    }
}

/// Of several resumable records, the ones to actually bring back: at most one per
/// workspace, oldest first — **except in main when the config allows several**.
///
/// Pure and separate from the spawn loop so the rule can be tested without a daemon.
/// Oldest-first both orders the rail the way it was built up and decides *which* of
/// two records sharing a workspace wins.
///
/// **`several_in_main` is not an extra: it is the same carve-out the API has.**
/// `api::refuse_if_occupied` returns early for main when `allow_several_in_main` is
/// set, and this comment used to claim parity with that rule while squeezing main
/// to one record like any worktree. So a repo that had turned the setting on lost
/// every main session but the oldest on each restart, silently — the setting meant
/// one thing while the daemon was up and another the moment it came back.
fn first_per_workspace(
    mut resumable: Vec<store::SessionRecord>,
    several_in_main: bool,
) -> Vec<store::SessionRecord> {
    resumable.sort_by_key(|r| r.created_at);
    let mut seen = std::collections::HashSet::new();
    resumable
        .into_iter()
        .filter(|r| {
            // Main keeps all of them when the config says main may hold several.
            // Every other workspace admits one, restart or not.
            if several_in_main && r.workspace == MAIN {
                return true;
            }
            seen.insert(r.workspace.clone())
        })
        .collect()
}

/// Bring back the sessions that were live when the daemon last went down.
///
/// A crash or a reboot takes every Claude process with it, because the daemon
/// owns the pty. Resuming costs the scrollback — ring buffers are in memory —
/// but keeps the conversation, which is the part that took time to build.
fn auto_resume(app: Arc<AppState>, records: Vec<store::SessionRecord>) {
    tokio::spawn(async move {
        // Any session that was live, whatever started it. A run started with a
        // skill used to be skipped here, and skipping it silently meant the pane
        // you were actually sitting in was the one that did not come back. `--resume`
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
        let to_resume = first_per_workspace(resumable, app.settings().allow_several_in_main);
        /* **Declared before the first spawn, because the window is the bug** (#33).
        These sessions are coming back and none of them is live yet; a shutdown
        between two spawns reads live state and would write `was_live: false` for
        everything this loop has not reached — the flag the *next* start filters on.
        An in-app update restarts the daemon several times in a row, so that window
        is entered three times in thirty seconds and the rail comes back empty.
        `AppState::session_records` writes `true` for anything still in here. */
        app.pending_resume
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(to_resume.iter().map(|r| r.id));
        let mut resumed = 0usize;
        for r in to_resume {
            // Its recorded pass, not `None`: a resumed fix run is still the run
            // the guard table counts, and the one `posts_proposals` mints a post
            // token for.
            match spawn::spawn_session(
                &app,
                &r.workspace,
                r.kind.clone().pass(),
                Some(spawn::Source::Resume(r.id)),
            )
            .await
            {
                Ok(id) => {
                    /* Live now, so its own state says what the set was standing in
                    for. Removed on success only: a spawn that failed should be
                    tried again by the next start rather than dropped, and the
                    filters above have already taken out the ones no start can fix. */
                    app.pending_resume
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
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
            // `fetch` answers for itself: a configured `reviews_command` that
            // exits non-zero or prints something unreadable → `Degraded`, no
            // GitHub repo → `Off`, and no command at all → the built-in queue.
            let main = app.cfg.main_checkout.clone();
            let timeout = app.cfg.review_timeout_seconds;
            let command = app.settings().reviews_command;
            // Two jobs now: the URL fallback for a configured command that omits
            // one, and the repository the built-in asks GitHub about. `None` means
            // no queue rather than an unlinked row.
            let repo = app.repos.upstream.clone();
            /* The same ladder the PR poller climbs, and resolved here rather than
            shared with it because there is nowhere to share it: only the *source*
            reaches the snapshot, never the value. One bounded child per poll
            period, which is what that poller already pays per tick. */
            let token_file = app.cfg.github_token_file.clone();
            let state = tokio::task::spawn_blocking(move || {
                let token = forge::resolve_token(token_file.as_deref())
                    .map_err(|e| tracing::debug!("review queue: no token: {e:#}"))
                    .ok();
                reviews::fetch(
                    &main,
                    timeout,
                    &command,
                    repo.as_deref(),
                    token.as_ref().map(|t| t.value.as_str()),
                )
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

            next_tick(interval, &app.review_refresh).await;
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
            adopt_pending_worktrees(&app).await;
            if busy.is_empty() {
                continue;
            }
            for ws in busy {
                // Debug: a reconcile that cannot read a tree keeps the previous
                // measurement by design, so this is a note, not a fault.
                if let Err(e) = app.reconcile(&ws).await {
                    tracing::debug!(workspace = %ws, "reconcile failed: {e:#}");
                }
            }
            app.notify().await;
        }
    });
}

/// Find the worktree a `…creating` session cut, when its `SessionStart` did not
/// say.
///
/// **The design had no retry, and one missed hook was permanent.** A worktree the
/// daemon did not cut itself — the `claude --worktree` arm, which is gone — reveals
/// its path only through that hook, so a session whose event was lost kept the
/// placeholder workspace id for its whole life — and with it no workspace record at all: no changed-files
/// pane, no divergence, no reconcile, no swap or move. The tree itself is fine
/// and the agent works in it, which is what makes this so easy to live with and
/// so confusing to look at.
///
/// **Only when the answer is unambiguous.** One pending session and one worktree
/// on disk that no workspace claims is a pairing with nothing to get wrong.
/// Anything else is left alone and said out loud, because guessing here attaches
/// a conversation to somebody else's tree, which is the one mistake in this area
/// that costs real work (`worktree::branch_drift` exists for its cousin).
async fn adopt_pending_worktrees(app: &Arc<AppState>) {
    let pending: Vec<model::SessionId> = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .values()
            .filter(|s| s.state.is_live() && s.workspace == spawn::PENDING_WORKTREE)
            .map(|s| s.id)
            .collect()
    };
    if pending.is_empty() {
        return;
    }

    let dir = app.cfg.worktrees_dir();
    let main = app.cfg.main_checkout.clone();
    let Ok(Ok(entries)) =
        proc::run_blocking("listing worktrees", move || git::worktree_list(&main)).await
    else {
        return;
    };
    let known: std::collections::HashSet<String> =
        app.inner.read().await.workspaces.keys().cloned().collect();
    let orphans: Vec<(String, PathBuf, Option<String>)> = entries
        .into_iter()
        .filter_map(|e| {
            let path = PathBuf::from(&e.path);
            let name = spawn::worktree_name_of(&path, &dir)?;
            (!known.contains(&name)).then_some((name, path, e.branch))
        })
        .collect();

    if pending.len() != 1 || orphans.len() != 1 {
        tracing::warn!(
            "{} session(s) still on the pending-worktree placeholder and {} unclaimed \
             worktree(s) on disk — too ambiguous to pair, so they stay as they are",
            pending.len(),
            orphans.len()
        );
        return;
    }
    #[expect(
        clippy::expect_used,
        reason = "the guard above returned unless there is exactly one"
    )]
    let (name, path, branch) = orphans.into_iter().next().expect("one");
    let id = pending[0];
    app.register_worktree(&name, path.clone(), branch).await;
    app.with_session(id, |s| {
        s.workspace = name.clone();
        // Its recorded cwd was main, because that is where the pty was spawned.
        s.cwd = path.clone();
    })
    .await;
    tracing::info!(
        session = %model::short_id(&id),
        "adopted {name} after its SessionStart did not report a cwd",
    );
    app.notify().await;
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
                        let at = path.clone();
                        let Ok(Ok(head)) =
                            proc::run_blocking("resolving HEAD", move || git::head_file(&at)).await
                        else {
                            continue;
                        };
                        let contents = std::fs::read_to_string(&head).unwrap_or_default();
                        e.insert((head, contents));
                    }
                    Entry::Occupied(mut e) => {
                        let (head, last) = e.get_mut();
                        // A read can miss mid-rename; keep the old value and retry
                        // next tick rather than treat a blip as a change.
                        let Ok(contents) = std::fs::read_to_string(&*head) else {
                            continue;
                        };
                        if contents != *last {
                            *last = contents;
                            if let Err(e) = app.reconcile(&id).await {
                                tracing::debug!(workspace = %id, "reconcile failed: {e:#}");
                            }
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

/// Re-read a live session's title when its transcript changes.
///
/// The hooks read it on `SessionStart` and `Stop`, and a `/rename` fires neither:
/// it is a local command, so no turn runs. Claude Code writes the new name within
/// a second, and the rail sat on the old one until the next turn ended. A `stat`
/// per live session is about a microsecond; the tail is read only when the file
/// moved, which for a working session is every tick, at about 125µs.
///
/// No `pin_transcript` here: its fallback is a `read_dir` of every project, and
/// the hooks already run it. A session whose file is not where its record says
/// is simply skipped until they have.
fn start_title_poller(app: Arc<AppState>) {
    use std::collections::{HashMap, HashSet};
    use std::time::SystemTime;

    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(2);
        // session id -> the transcript's mtime when its title was last read.
        let mut seen: HashMap<uuid::Uuid, SystemTime> = HashMap::new();
        loop {
            tokio::time::sleep(interval).await;
            let live: Vec<(uuid::Uuid, PathBuf, Option<PathBuf>)> = {
                let inner = app.inner.read().await;
                inner
                    .sessions
                    .values()
                    .filter(|s| s.state.is_live())
                    .map(|s| (s.id, s.cwd.clone(), s.transcript_path.clone()))
                    .collect()
            };
            let ids: HashSet<uuid::Uuid> = live.iter().map(|(id, ..)| *id).collect();
            seen.retain(|id, _| ids.contains(id));

            let last = seen.clone();
            let Ok((stamps, titles)) = proc::run_blocking("re-reading session titles", move || {
                let mut stamps = Vec::new();
                let mut titles = Vec::new();
                for (id, cwd, recorded) in live {
                    let Some(path) = store::transcript_file(id, &cwd, recorded.as_deref()) else {
                        continue;
                    };
                    let Ok(at) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
                        continue;
                    };
                    stamps.push((id, at));
                    if last.get(&id) == Some(&at) {
                        continue;
                    }
                    if let Some(title) = store::ai_title(id, &cwd, Some(&path)) {
                        titles.push((id, cwd, title));
                    }
                }
                (stamps, titles)
            })
            .await
            else {
                continue;
            };
            seen.extend(stamps);

            let mut changed = false;
            for (id, cwd, title) in titles {
                // Same guard as the hooks' read: an answer about a directory the
                // session has since left is not an answer about it.
                changed |= app
                    .with_session(id, |s| {
                        if s.cwd != cwd || s.title.as_deref() == Some(title.as_str()) {
                            return false;
                        }
                        s.title = Some(title);
                        true
                    })
                    .await
                    .unwrap_or(false);
            }
            if changed {
                app.notify().await;
            }
        }
    });
}

/// Remove the worktrees of conversations nobody came back to, hourly.
///
/// Hourly rather than at boot only, because a daemon that runs for a week would
/// otherwise never look again; and hourly rather than often, because nothing here
/// is urgent and every pass shells `git status` per candidate tree. The first pass
/// waits a minute: startup is already spending its time on auto-resume, the PR poll
/// and the boot checks, and nothing about this is worth being third in that queue.
///
/// `worktree::reap_old` decides and refuses; this only decides when to ask. A
/// `worktree_retention_days` of `0` makes each pass a no-op rather than keeping the
/// task from starting, because the setting applies on save now: a timer that was
/// never spawned could not notice it being turned on. An hourly wakeup is the cost.
fn start_worktree_reaper(app: Arc<AppState>) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            let removed = orchd::worktree::reap_old(&app).await;
            if removed > 0 {
                // The rail lists workspaces, and one of them has just stopped
                // existing.
                app.notify().await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
}

/// The conversations in this checkout that orchd did not start, for the archive.
///
/// **Polled rather than read when the snapshot is built.** The answer is a
/// `read_dir` per workspace, and a snapshot is built for every hook, every state
/// change and every notify, and `store::find_transcript` records what that shape
/// costs: a scan a second, forever. So it is a background read that the snapshot
/// copies out of `Inner`, like the PR list and the stack probe beside it.
///
/// A minute, because what it watches is somebody closing a terminal, and the
/// number that has to be right before you open the fold is the count on the
/// caret. Opening it rescans (`api::refresh_external`), so this interval is not
/// what decides whether the list you are reading is current.
fn start_external_poller(app: Arc<AppState>) {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(60);
        loop {
            app.rescan_external().await;
            tokio::time::sleep(interval).await;
        }
    });
}

/// Respawn the sessions queued for a restart, as each becomes safe to.
///
/// **Driven by the snapshot stream, not a clock.** A snapshot goes out on every
/// hook and every state change, so the `Stop` that makes a queued session safe is
/// also the event that wakes this — the restart follows the turn by milliseconds,
/// and an idle daemon with nothing queued pays one read lock per snapshot.
///
/// Here rather than in `state`, because `state` may not import `spawn` and the
/// respawn is a spawn.
fn start_restart_watcher(app: Arc<AppState>) {
    tokio::spawn(async move {
        let mut events = app.events.subscribe();
        loop {
            match events.recv().await {
                // A lagged receiver missed snapshots, not work: the queue is on the
                // records, so any wake-up is as good as the one it missed.
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    orchd::restart::run_due(&app).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
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
            /* **`Some(false)` when the probe itself fails, not `None`.** `None` now
            means "this checkout has no stack", and a panicked or cancelled
            probe folded onto it would erase the badge from a checkout that
            plainly has a compose file — silently, and for good, since an
            unchanged value never notifies. A probe that did not answer is a
            stack we cannot see, which is what down has always meant here. */
            let up = tokio::task::spawn_blocking(move || stack_running(&main))
                .await
                .unwrap_or(Some(false));
            // Scoped, because the guard used to outlive the `if` and stay held
            // across the sleep below whenever the answer had not changed — which
            // is every poll, normally. That is the state write lock, so the
            // daemon spent 20 seconds out of every 20 holding it, and anything
            // that touched state waited for the gap: a spawn, a hook, the
            // rail's own snapshot.
            let changed = {
                let mut inner = app.inner.write().await;
                let changed = inner.stack_up != up;
                if changed {
                    inner.stack_up = up;
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

/// `Some(true)` when `docker compose ps` reports at least one running container.
/// A missing `docker` or a stopped daemon fails the command and reads as
/// `Some(false)`, which is the honest answer for "is the stack up".
///
/// **`None` is a checkout with no compose file**, and that is the third answer
/// this used to fold into "down". A repo that carries no containers at all got a
/// permanent red dot and the words `stack down` in its drawer — a feature of one
/// repo drawn as a fault on every other, and the opposite of what
/// `docs/workspace-isolation.md` records as the portable default. The filesystem
/// check was already here; it was the *return type* that had nowhere to put the
/// answer.
/// The four spellings docker accepts for a compose file.
///
/// One list, because two readers had their own: `firstrun::detect_processes` offers
/// a `docker` process off it and [`has_compose_file`] decides whether a checkout has
/// a stack at all, and they were written in different orders. A fifth spelling added
/// to one of them would offer the process and draw no badge.
pub(crate) const COMPOSE_FILES: [&str; 4] = [
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yml",
    "compose.yaml",
];

/// Does this checkout carry a compose file at all?
///
/// Its own function so a test can drive it without spawning `docker`.
fn has_compose_file(main: &std::path::Path) -> bool {
    COMPOSE_FILES.iter().any(|f| main.join(f).exists())
}

fn stack_running(main: &std::path::Path) -> Option<bool> {
    if !has_compose_file(main) {
        return None;
    }
    Some(
        std::process::Command::new("docker")
            .args(["compose", "ps", "--status", "running", "-q"])
            .current_dir(main)
            .output()
            .map(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A checkout with no compose file has no stack, and that is not "down".
    ///
    /// The distinction is invisible in Rust and loud in the window: folded into
    /// `false` it drew a red dot and the words `stack down` in the drawer of every
    /// repo that carries no containers. Asserted here because the filesystem check
    /// has always been right and the *return type* was what threw the answer away.
    #[test]
    fn a_checkout_with_no_compose_file_has_no_stack_rather_than_a_stopped_one() {
        let root = orchd::testutil::scratch("stack-state");
        assert_eq!(
            stack_running(&root),
            None,
            "no compose file is no stack at all"
        );

        /* **The compose-file half is deliberately not driven here.** With one
        present `stack_running` shells out to `docker compose ps` with no deadline,
        so a machine with the client installed and no reachable daemon can make
        `cargo test` sit for as long as docker takes to give up — for an assertion
        the `if !has_compose` branch above already settles. The filenames it looks
        for are the thing worth holding, and they are held by name. */
        for f in [
            "docker-compose.yml",
            "docker-compose.yaml",
            "compose.yml",
            "compose.yaml",
        ] {
            let one = root.join("only");
            let _ = std::fs::remove_dir_all(&one);
            std::fs::create_dir_all(&one).unwrap();
            std::fs::write(one.join(f), "services: {}\n").unwrap();
            assert!(
                has_compose_file(&one),
                "{f} must count as a compose file, or this checkout reads as having no stack"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The order the boot sweep walks, which is now the perceived start time.
    ///
    /// Worth a test rather than a comment because it is invisible when wrong: an
    /// arbitrary order still measures everything and still ends up correct, it
    /// just leaves the one pane being read until last. With 64 worktrees that is
    /// the difference between a loader that blinks and one that sits there for six
    /// seconds.
    #[tokio::test]
    async fn a_sweep_measures_the_workspaces_you_are_looking_at_first() {
        let (app, dir) = orchd::testutil::app("sweep");

        // Named so alphabetical order would put them in exactly the wrong places:
        // `a-empty` first and `z-session` last.
        for ws in ["a-empty", "m-empty", "z-session"] {
            app.register_worktree(ws, dir.clone(), None).await;
        }
        {
            let mut inner = app.inner.write().await;
            // Archived, the state every restored session is in before auto-resume
            // spawns it — which is the moment the boot sweep runs. Ranking on
            // `is_live` here would rank nothing and sort alphabetically.
            let s = model::Session::new(
                uuid::Uuid::new_v4(),
                "z-session".to_string(),
                dir.clone(),
                None,
            );
            inner.sessions.insert(s.id, s);
        }

        let order = sweep_order(&*app.inner.read().await);
        assert_eq!(
            order,
            vec!["z-session", MAIN, "a-empty", "m-empty"],
            "sessions first, then main, then the rest alphabetically"
        );
    }

    /// A workspace whose directory is gone is skipped, and its row survives the
    /// sweep. The row is what `revive` and the PR flows rebuild the tree at, so
    /// dropping it would trade a warning per sweep for a second tree on the same
    /// branch; the sweep's only job here is to stop paying for it.
    #[tokio::test]
    async fn a_sweep_skips_a_workspace_whose_tree_is_gone_and_keeps_its_row() {
        let (app, dir) = orchd::testutil::app("sweep-gone");
        app.register_worktree(
            "gone",
            dir.join("no-such-tree"),
            Some("worktree-gone".into()),
        )
        .await;
        app.register_worktree("here", dir.clone(), None).await;

        assert_eq!(sweep_one(&app, "gone").await, Swept::Skipped);
        // Not a git repo, so the measurement itself fails and is logged; the point
        // is that it was attempted, because the directory is there.
        assert_eq!(sweep_one(&app, "here").await, Swept::Measured);
        // Main is never skipped: its path is canonicalised at parse and exists.
        assert_eq!(sweep_one(&app, MAIN).await, Swept::Measured);

        let inner = app.inner.read().await;
        assert!(
            inner.workspaces.contains_key("gone"),
            "the row is the rebuild point"
        );
        assert!(
            !inner.workspaces["gone"].tree.measured,
            "nothing was measured in it"
        );
    }

    /// One record per workspace comes back, and it is the oldest — the rule that,
    /// on a cold start, keeps two sessions that once shared a worktree from both
    /// re-hydrating into it.
    #[test]
    fn auto_resume_brings_back_one_per_workspace_and_all_of_main_when_allowed() {
        use std::time::{Duration, UNIX_EPOCH};
        let rec = |ws: &str, age_secs: u64| {
            let mut s = model::Session::new(
                uuid::Uuid::new_v4(),
                ws.to_string(),
                std::path::PathBuf::from("/tmp"),
                None,
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
        let kept = first_per_workspace(
            vec![newer_a.clone(), b.clone(), main.clone(), older_a.clone()],
            false,
        );

        let by_ws: std::collections::HashMap<_, _> =
            kept.iter().map(|r| (r.workspace.clone(), r.id)).collect();
        assert_eq!(kept.len(), 3, "one per workspace: wt-a, wt-b, main");
        assert_eq!(
            by_ws.get("wt-a"),
            Some(&older_a.id),
            "the older of the two in wt-a wins"
        );
        assert_eq!(by_ws.get("wt-b"), Some(&b.id));
        assert_eq!(by_ws.get(MAIN), Some(&main.id));

        /* **And with `allow_several_in_main`, main keeps all of them.** The rule
        this path enforces is `api::refuse_if_occupied`'s, which returns early for
        main when that setting is on — so squeezing main to one record here made
        the setting mean one thing while the daemon was up and another after a
        restart, and a repo that had turned it on lost every main session but the
        oldest, silently. A worktree is unaffected either way. */
        let second_main = rec(MAIN, 3);
        let kept = first_per_workspace(
            vec![
                newer_a.clone(),
                main.clone(),
                second_main.clone(),
                older_a.clone(),
            ],
            true,
        );
        let mains: Vec<_> = kept
            .iter()
            .filter(|r| r.workspace == MAIN)
            .map(|r| r.id)
            .collect();
        assert_eq!(mains.len(), 2, "both main sessions come back");
        assert!(mains.contains(&main.id) && mains.contains(&second_main.id));
        assert_eq!(
            kept.iter().filter(|r| r.workspace == "wt-a").count(),
            1,
            "a worktree still admits one, whatever main is allowed"
        );
    }

    // --- the page, and the window it may not have --------------------------

    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt; // for `oneshot`

    /// The app and the host that serves its page, as `start` pairs them.
    ///
    /// One token and one port, because one process serves both — see
    /// [`crate::host`] for why `Checkout` carries its own copies anyway.
    fn app_and_host(tag: &str) -> (Arc<AppState>, Arc<crate::host::Host>, std::path::PathBuf) {
        let (app, dir) = orchd::testutil::app(tag);
        let host = crate::host::Host::new(app.token.clone(), app.cfg.port, app.chrome);
        (app, host, dir)
    }

    /// A same-origin request, spelled the way a browser on this port spells it.
    fn req(app: &Arc<AppState>, method: &str, uri: &str, token: bool) -> Request<Body> {
        let port = app.cfg.port;
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", format!("127.0.0.1:{port}"))
            .header("origin", format!("http://127.0.0.1:{port}"));
        if token {
            b = b.header("x-orch-token", app.token.clone());
        }
        b.body(Body::empty()).unwrap()
    }

    async fn body_of(res: axum::response::Response) -> String {
        String::from_utf8(to_bytes(res.into_body(), 1 << 22).await.unwrap().to_vec()).unwrap()
    }

    /// `GET /` hands the page a complete token, and is deliberately not gated.
    ///
    /// Pinned because the whole authentication story rests on it: the token is
    /// *embedded* rather than fetched, so it never exists as a value another
    /// origin could ask for — and the page cannot carry a token it does not have
    /// yet, which is why this one route is exempt. The contrast is the second
    /// half of the test: the same origin without a token cannot mutate anything.
    ///
    /// The placeholder assertion is the one that catches a rename. A substitution
    /// that stops matching leaves `__ORCH_TOKEN__` in the page, the SPA reads
    /// that string as its token, and every call fails on a token that looks
    /// perfectly well-formed.
    #[tokio::test]
    async fn the_page_carries_its_token_and_needs_none_to_ask_for_it() {
        let (app, host, _dir) = app_and_host("index-token");
        let res = router(app.clone(), host.clone())
            .oneshot(req(&app, "GET", "/", false))
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "GET / must not be token-gated");
        let page = body_of(res).await;
        assert!(
            page.contains(&app.token),
            "the page went out without its token"
        );
        assert!(
            !page.contains("__ORCH_TOKEN__"),
            "a placeholder survived substitution"
        );
        assert!(!page.contains("__ORCH_CHROME__"));
        assert!(!page.contains("__ORCH_PLATFORM__"));

        let refused = router(app.clone(), host)
            .oneshot(req(&app, "POST", "/api/prs/refresh", false))
            .await
            .unwrap();
        assert_eq!(
            refused.status(),
            401,
            "an untokened POST from the same origin was allowed"
        );
    }

    /// The review preview substitutes the same three, and is reached the same way.
    ///
    /// Its own assertion rather than a loop over both, because it is a separate
    /// handler with its own copy of the substitution — which is exactly the shape
    /// that drifts.
    #[tokio::test]
    async fn the_review_preview_is_substituted_too() {
        let (app, host, _dir) = app_and_host("preview-token");
        let res = router(app.clone(), host)
            .oneshot(req(&app, "GET", "/review-preview", false))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let page = body_of(res).await;
        assert!(page.contains(&app.token));
        assert!(!page.contains("__ORCH_TOKEN__"));
        assert!(!page.contains("__ORCH_PLATFORM__"));
    }

    /// A window command with no window degrades, and says so.
    ///
    /// This is the browser-tab case: `cargo run -p orchd` plus a tab has no Tauri
    /// handle, the tab draws its own chrome, and a titlebar press must be a
    /// refusal with a sentence rather than a panic or a silent `ok: true`. Pinned
    /// because the daemon is about to stop being the process that holds the
    /// handle, and this is the behaviour that has to survive the move.
    #[tokio::test]
    async fn a_titlebar_press_with_no_native_window_refuses_by_name() {
        let (app, host, _dir) = app_and_host("no-window");

        let res = router(app.clone(), host.clone())
            .oneshot(req(&app, "POST", "/api/window/minimize", true))
            .await
            .unwrap();
        // 400 with a sentence, which is what every refusal in this API is — not a
        // 404 (the route exists) and not `ok: true` (nothing happened). The rail
        // shows the sentence verbatim.
        assert_eq!(res.status(), 400);
        let answer: serde_json::Value = serde_json::from_str(&body_of(res).await).unwrap();
        assert_eq!(
            answer["error"], "no native window attached",
            "the refusal stopped naming what is missing"
        );

        // An unknown command is a different refusal, and the two must not merge:
        // one is "this daemon has no window", the other is "no such button".
        let bogus = router(app.clone(), host)
            .oneshot(req(&app, "POST", "/api/window/explode", true))
            .await
            .unwrap();
        assert_eq!(bogus.status(), 400);
        let answer: serde_json::Value = serde_json::from_str(&body_of(bogus).await).unwrap();
        assert_eq!(answer["error"], "no such window command: explode");
    }

    /// A refused hand-off takes the flag back.
    ///
    /// `handed_off` is what tells the overlay to hold its report and wait for a
    /// run, so a refusal that left the flag standing strands the review on
    /// "applying" for good — the fault the hand-off was built to fix. The start is
    /// refused here because the PR is not in the poll, which is the cheapest real
    /// refusal there is.
    ///
    /// **Here rather than in `spawn`**, because settling a run is this crate's job
    /// now: `spawn` publishes the exit and [`settle_run`] is what acts on it. The
    /// matching half — that the exit is published, and with the flag — is
    /// `spawn::tests::a_review_exit_acts_on_its_hand_off_flag`.
    #[tokio::test]
    async fn a_refused_hand_off_takes_the_flag_back() {
        let (app, dir) = orchd::testutil::app("handoff-refused");
        let id = uuid::Uuid::new_v4();
        let pass = model::Pass {
            pr: 4242,
            command: model::Pass::REVIEW.to_string(),
        };
        {
            let mut inner = app.inner.write().await;
            let mut s = model::Session::new(id, "wt".to_string(), dir.clone(), Some(pass.clone()));
            s.fix_pr_on_exit = true;
            inner.sessions.insert(id, s);
        }

        settle_run(
            app.clone(),
            orchd::state::RunExit {
                session: id,
                pass,
                hand_off: true,
            },
        )
        .await;

        assert!(
            !app.inner.read().await.sessions[&id].fix_pr_on_exit,
            "the refusal left the overlay waiting on a run that never started"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
