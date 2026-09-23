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

use crate::forge::Forge;
use crate::model::*;
use crate::spawn;
use crate::state::AppState;
use crate::worktree;

pub struct ApiError(pub(crate) anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Errors are shown verbatim in the rail: a refused `git worktree remove`
        // is information, not noise to swallow.
        //
        // The status is the *kind*, and the body is unchanged either way — the SPA
        // and `orch` both read `error`, and a refusal that changed its sentence to
        // gain a code would be a worse trade than the one it fixes.
        let status = self
            .0
            .chain()
            .find_map(|e| e.downcast_ref::<Refusal>())
            .map_or(StatusCode::BAD_REQUEST, |r| match r {
                // The kinds are the daemon's; the codes are this layer's, which is
                // why the mapping is here and the enum is in `model`.
                Refusal::Missing(_) => StatusCode::NOT_FOUND,
                Refusal::Busy(_) => StatusCode::CONFLICT,
            });
        (status, Json(json!({ "error": format!("{:#}", self.0) }))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

pub(crate) type ApiResult<T> = Result<Json<T>, ApiError>;

/// `bail!` for a handler: refuse the request with this message.
///
/// The same thing as `return Err(ApiError(anyhow::anyhow!(..)))`, which was
/// spelled out at forty-odd sites over three lines each. The rest of the crate
/// says `bail!`; this is the handler-shaped one.
macro_rules! refuse {
    ($($arg:tt)*) => {
        return Err(ApiError(anyhow::anyhow!($($arg)*)))
    };
}
// Usable from the modules that were split out of here, which keep the handler
// vocabulary: a `macro_rules!` is textually scoped, so a sibling module cannot see
// one without this.
pub(crate) use refuse;

/// `refuse!` for a resource somebody else is holding: the same sentence, answered
/// `409` rather than `400`.
///
/// **"Try again when the agent stops" and "the daemon will never accept this" are
/// different answers**, and a script could not tell them apart — every refusal was
/// a `400` and the only signal was English. Every site that uses this is a
/// *temporary* no: a session mid-turn, a stopped rebase, a question already open.
macro_rules! refuse_busy {
    ($($arg:tt)*) => {
        return Err(ApiError(
            crate::model::Refusal::Busy(format!($($arg)*)).into(),
        ))
    };
}
// Re-exported like `refuse` above, so a sibling route module can refuse the same
// way. `bank` is the first to need it.
pub(crate) use refuse_busy;

// ---------------------------------------------------------------------------
// Guards (§12)
// ---------------------------------------------------------------------------

pub fn host_allowed(host: &str, port: u16) -> bool {
    let expected = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    expected.iter().any(|e| e == host)
}

fn origin_allowed(origin: &str, port: u16, host_origin: Option<&str>) -> bool {
    let expected = [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ];
    // The page is served by the host, on the host's port, so every call a child
    // daemon receives is cross-origin. **One exact string**, handed in on the
    // child's argv by the process that spawned it and never read from the config
    // file — a widening a config could spell is a widening somebody else can spell.
    // Absent for a daemon nobody hosts, which is a browser tab on this port.
    expected.iter().any(|e| e == origin) || host_origin == Some(origin)
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
///   shape: `skills/triage/SKILL.md` POSTs its proposals with `curl`, which sends no
///   Origin, and this arm's absence meant the one route an agent calls answered
///   403 to the only caller it has. Driving it by hand *with* an Origin header
///   during development is what hid that.
///
///   Safe because Origin is a CSRF control, and CSRF is a browser problem: a page
///   cannot omit the header on a cross-origin fetch or form POST, and it cannot
///   read the token to forge this. Absence is positive evidence of a non-browser
///   caller; the token is what authenticates it.
pub fn origin_ok(
    origin: Option<&str>,
    port: u16,
    host_origin: Option<&str>,
    is_hook: bool,
    is_get: bool,
    token_ok: bool,
) -> bool {
    match origin {
        Some(o) => origin_allowed(o, port, host_origin),
        None => is_hook || is_get || token_ok,
    }
}

/// The routes a running session calls on its own `ask_token`.
///
/// A list rather than a growing chain of `ends_with`, because it has been
/// outgrown once already — see the note in [`guard`].
fn is_ask_route(path: &str) -> bool {
    const ASK_ROUTES: [&str; 10] = [
        "/ask",
        "/wait",
        // The worktree grant, asked by the agent and read by the push guard —
        // which is a `command` hook Claude Code spawns, so it arrives with the
        // session's ask token and no Origin, exactly like a vendored prompt.
        "/outside",
        "/spawn",
        // A review session posting one thread's reply. The rules the words go
        // through are `post_one`'s; see `review_api::thread_reply`.
        "/reply",
        "/process",
        // `orch kill`. Not `/kill` or `/delete`, which are the SPA's own routes on
        // any session — a suffix matcher cannot tell those apart from a narrower
        // verb spelled the same way.
        "/discard",
        // `orch teardown`. Safe as a suffix only because the SPA's own teardown is
        // `/api/workspace/:id/teardown`, outside the `/api/session/` prefix.
        "/teardown",
        // The end of phase 3: who still owes the PR a look. Which reviewers are
        // asked is the daemon's, off a fresh fetch — see `session_rerequest`.
        "/rerequest",
        // Phase 4 of `skills/review/SKILL.md`: the review saying it is done.
        "/handoff",
    ];
    path.starts_with("/api/session/") && ASK_ROUTES.iter().any(|s| path.ends_with(s))
}

/// Where a triage or review-session run POSTs what it proposed.
///
/// Separate from [`is_ask_route`] because it is keyed on a *PR*, not a session,
/// so it cannot share that matcher's `/api/session/` prefix. It carries the same
/// kind of credential: narrow, minted per run, and good for nothing else.
fn is_proposals_route(path: &str) -> bool {
    path.starts_with("/api/pr/") && path.ends_with("/proposals")
}

/// The two routes the vendored `triage` skill calls before it can propose
/// anything to start: the repo, the login, the language, the tracker and the base.
///
/// Keyed on a PR like the proposals route and carrying the same run credential.
/// A route missing from here is refused twice over, and neither refusal names the
/// cause: the Origin check has no arm for it, and `needs_token` then wants a
/// credential the agent is deliberately not given.
fn is_triage_route(path: &str) -> bool {
    path.starts_with("/api/pr/") && path.ends_with("/triage-context")
}

/// Every route an *agent* calls, on a credential that is not the app token.
///
/// One predicate because the guard has to make the same two allowances for all of
/// them — skip `needs_token`, and accept a missing `Origin` — and splitting that
/// is how `…/committed` once shipped reachable by neither. See [`guard`].
fn is_agent_route(path: &str) -> bool {
    is_ask_route(path) || is_proposals_route(path) || is_triage_route(path)
}

/// Reject anything that is not the SPA's own origin.
///
/// Binding to 127.0.0.1 is necessary but not sufficient: any web page you visit
/// can issue requests to it, and the daemon's surface is effectively local code
/// execution (§12).
///
/// **The token does not close the "any local process" hole, and this used to say
/// it did.** `GET /` is exempt and the page it returns has the token substituted
/// into it, so any process that can reach loopback can read it and then hold
/// everything. That is a deliberate trade — the threat model is a machine you do
/// not share, where a hostile local process can ptrace this daemon anyway — but it
/// is a trade, not a boundary, and the README says so in those words now. What the
/// token *does* close is the cross-origin hole, together with the Host and Origin
/// checks: a web page you visit cannot read the token, so it cannot forge these
/// calls.
///
/// Hooks cannot easily carry it, which is why they are confined to a separate
/// prefix and a schema that only ever updates state. GETs are otherwise exempt so
/// a same-origin address-bar read works — with the exception
/// `SPENDS_GITHUB_TOKEN` names.
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
        // Warn, not silence, on all three refusals below. A refusal is three words
        // over the wire and the caller is usually a script that cannot say which
        // rule turned it down — and the one that bites is `is_ask_route`: a route
        // an agent calls that nobody added to that list answers `bad origin`, which
        // reads as a CORS problem and is not one. No token material: the header
        // that failed is named, never its value.
        tracing::warn!(%path, %host, "refused: host is not this daemon's");
        return (StatusCode::FORBIDDEN, "bad host").into_response();
    }

    let origin = headers.get("origin").and_then(|v| v.to_str().ok());

    /* **A page served by a host is cross-origin to this daemon, so the browser
    asks first.** Every call the page makes carries `x-orch-token` and most
    carry a JSON content type, which makes them non-simple requests: the browser
    sends `OPTIONS` and refuses to send the real one unless the answer names its
    origin. Nothing here answered that, so under the app every fetch to a child
    daemon failed and only the websockets — which CORS does not cover — worked.
    The symptom was a board that drew and then could not do anything.

    Scoped to the one origin `host_origin` already names: the same exact string,
    from the argv of whoever spawned this daemon, never from a file. A `*` here
    would let any page in the browser drive this daemon, and the token is the
    only thing that would stop it. */
    let allowed_origin = app
        .cfg
        .host_origin
        .as_deref()
        .filter(|h| Some(*h) == origin);
    if req.method() == axum::http::Method::OPTIONS {
        return match allowed_origin {
            Some(origin) => cors_preflight(origin),
            // Not a preflight we recognise. `405` rather than `403`, because the
            // route may genuinely not take `OPTIONS`.
            None => (StatusCode::METHOD_NOT_ALLOWED, "no such method").into_response(),
        };
    }

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

    /* The agent's own routes authenticate differently: they carry the session's
    `ask_token` rather than the app token, and the handlers do that check
    because only they know which session the path names. Exempted here the way
    hooks are, and for the same reason — the caller is a local process with no
    Origin and no business holding the key to everything else.

    Named as a list because the suffix form has already been outgrown once:
    `/committed` arrived with the resolve run and this predicate was not
    extended, so the seam the whole two-phase flow turns on answered 403 to
    its only caller — twice over, since a curl that added an Origin would then
    have failed `needs_token` for want of an app token it is deliberately not
    given. Found by driving a real run, invisible to every unit test. */
    let is_ask = is_agent_route(&path);

    let is_get = req.method() == axum::http::Method::GET;
    if !origin_ok(
        origin,
        port,
        app.cfg.host_origin.as_deref(),
        is_hook || is_ask,
        is_get,
        token_ok,
    ) {
        tracing::warn!(
            %path,
            origin = origin.unwrap_or("-"),
            is_ask,
            is_hook,
            "refused: origin is not this daemon's, and no other arm applied"
        );
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }

    /* Every GET that reaches GitHub on our credential, not just the first one.
    A GET is otherwise exempt from the token because it hands back state the
    daemon already had; one that *spends the GitHub token* is a different
    proposition, because any local process could then drive authenticated
    GitHub traffic through the daemon and burn its rate limit. These carry the
    token like a mutating route does, which costs the UI nothing — the SPA's
    `get()` already sends it on every request.

    A named list rather than a suffix match: this started as `/threads` alone
    and `/review` was added later without it. `/threads` itself is gone now
    (nothing called it); a dead entry here would read as a route that exists. */
    const SPENDS_GITHUB_TOKEN: [&str; 1] = ["/review"];
    let spends_github_token =
        path.starts_with("/api/pr/") && SPENDS_GITHUB_TOKEN.iter().any(|s| path.ends_with(s));
    let needs_token =
        !is_hook && !is_ask && (req.method() != axum::http::Method::GET || spends_github_token);
    if needs_token && !token_ok {
        tracing::warn!(%path, "refused: no app token, or the wrong one");
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }

    let mut response = next.run(req).await;
    // The browser drops a cross-origin answer the response does not name it in,
    // whatever the status — so this rides every answer, including refusals, or a
    // refusal reads to the page as a network failure with no message.
    if let Some(origin) = allowed_origin {
        if let Ok(value) = origin.parse() {
            response
                .headers_mut()
                .insert(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        }
    }
    response
}

/// The answer to a preflight from the host's page.
///
/// `x-orch-token` because every call carries it, `content-type` because a JSON
/// body makes the request non-simple. No credentials header: the daemon
/// authenticates on that token and never on a cookie, so the browser must not be
/// told to send one.
fn cors_preflight(origin: &str) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    if let Ok(value) = origin.parse() {
        headers.insert(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        axum::http::HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        axum::http::HeaderValue::from_static("content-type, x-orch-token"),
    );
    // A browser that caches this asks once per page rather than once per call.
    headers.insert(
        axum::http::header::ACCESS_CONTROL_MAX_AGE,
        axum::http::HeaderValue::from_static("600"),
    );
    response
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

pub async fn get_state(State(app): State<Arc<AppState>>) -> impl IntoResponse {
    Json(app.snapshot().await)
}

// ---------------------------------------------------------------------------
// Editable settings
// ---------------------------------------------------------------------------

/// The six settings the panel edits, as the running daemon has them.
/// The settings as last saved, so the panel shows what a save just applied
/// rather than what the daemon started with.
pub async fn get_config(State(app): State<Arc<AppState>>) -> ApiResult<crate::config::Settings> {
    Ok(Json(app.settings()))
}

/// Persist edited settings to `config.json`, and apply what can be applied.
/// Validation is serde's — a bad tracker or a malformed process rejects the whole
/// POST.
///
/// **Most of it applies at once.** Every setting but three is read when it is
/// used, from [`AppState::settings`], so replacing that copy is the whole of
/// applying it. The three that are fixed at start are what
/// [`Settings::needs_restart`] names, and `restart_required` is true only when one
/// of them changed — the panel restarts on that answer and on nothing else.
///
/// [`Settings::needs_restart`]: crate::config::Settings::needs_restart
pub async fn set_config(
    State(app): State<Arc<AppState>>,
    Json(body): Json<crate::config::Settings>,
) -> ApiResult<serde_json::Value> {
    body.write()?;
    let reviews_moved = app.settings().reviews_command != body.reviews_command;
    let restart = app.replace_settings(body);
    // A new review command is a new queue, and waiting out the poll period to
    // show it is the restart this replaced, only slower.
    if reviews_moved {
        app.review_refresh.notify_one();
    }
    // The snapshot carries `several_in_main`, so the rail learns it now too.
    app.notify().await;
    Ok(Json(json!({ "ok": true, "restart_required": restart })))
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct NewSession {
    pub workspace: String,
}

/// Refuse if a workspace already holds a live session.
///
/// One live session per workspace: two agents in one checkout share a cwd and edit
/// the same files, and the daemon's dirty-tracking and reconcile are per-workspace,
/// so a second one is corruption waiting to happen. Main enforces this through
/// `claim_main`; this is the same rule for every other tree, applied at the paths a
/// person or agent asks for a session — `new_session`, `open_pr`, resume.
///
/// Deliberately *not* inside `spawn_session`: the swap's `relocate_session` goes
/// straight through that and legitimately holds both trees' sessions for the instant
/// it exchanges them. The guard lives at the request boundary so that internal move
/// is exempt. The trade-off is a narrow window — the check and the later insert are
/// not one atomic step, so two *simultaneous* duplicate requests could both pass —
/// which teardown already tolerates and which only the (rejected) in-`spawn_session`
/// version would close.
pub(crate) async fn refuse_if_occupied(
    app: &Arc<AppState>,
    workspace: &str,
) -> Result<(), ApiError> {
    // The one relaxation, and it is main's alone: a worktree exists so that two
    // pieces of work do not share an index, so lifting it there would undo the
    // thing the worktree is for.
    if workspace == MAIN && app.settings().allow_several_in_main {
        return Ok(());
    }
    let Some(held) = app.live_sessions_in(workspace).await.into_iter().next() else {
        return Ok(());
    };
    // Name it the way the rail does, so the message points at a row you can find.
    let who = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .get(&held)
            .and_then(|s| s.label().map(str::to_owned))
            .unwrap_or_else(|| crate::model::short_id(&held))
    };
    Err(ApiError(anyhow::anyhow!(
        "{workspace} already has a live session ({who}); close it before starting another"
    )))
}

pub async fn new_session(
    State(app): State<Arc<AppState>>,
    Json(body): Json<NewSession>,
) -> ApiResult<serde_json::Value> {
    refuse_if_occupied(&app, &body.workspace).await?;
    let id = spawn::spawn_session(&app, &body.workspace, None, None).await?;
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
    let id = spawn::spawn_worktree_session(&app, name, None).await?;
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
            // Escalating, and detached: one `SIGHUP` is a request Node is entitled
            // to decline, so the button "succeeded" while the row stayed Working
            // and the exit watcher never fired. The route answers at once; the
            // watcher settles the row when the exit really lands.
            tokio::spawn(async move {
                h.kill_gracefully().await;
            });
            Ok(Json(json!({ "killed": id })))
        }
        None => Err(ApiError(crate::state::no_such_session(id))),
    }
}

#[derive(Deserialize)]
pub struct Rename {
    /// Blank means "go back to Claude Code's name", which is the only way out of a
    /// rename you regret.
    #[serde(default)]
    pub name: Option<String>,
}

/// Name a session yourself.
///
/// Stored beside the ai-title rather than over it (`model::Session::name`): the
/// `Stop` hook rewrites the ai-title every turn, so a rename written into `title`
/// would revert the moment the agent finished anything. Archived conversations are
/// renameable too — that is where a name earns the most, since the archive is the
/// list you scan later.
pub async fn rename_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(body): Json<Rename>,
) -> ApiResult<serde_json::Value> {
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        // A rail row is one line: a name long enough to push the state out of it
        // makes the row useless, and nothing else would ever say so.
        .map(|n| n.chars().take(80).collect::<String>());
    {
        let mut inner = app.inner.write().await;
        let s = inner
            .sessions
            .get_mut(&id)
            .ok_or_else(|| crate::state::no_such_session(id))?;
        s.name = name.clone();
    }
    // Persists on the way out: `notify` writes `sessions.json` before it pushes.
    app.notify().await;
    Ok(Json(json!({ "session": id, "name": name })))
}

/// Open Claude Code's own rewind picker in this session.
///
/// Two escapes into the pty, and nothing else. Claude Code already has the whole
/// feature — a double-tap of `esc` at the prompt opens a picker that can put the
/// conversation *and* the files back — so the daemon's job is to reach it, not to
/// rebuild it. Measured in the shipped binary rather than assumed: 2.1.240 carries
/// `rewindToMessageIndex`, `rewindAnchorUuid`, `rewindDirectory` and a tip whose
/// text is "Double-tap esc to rewind the conversation to a previous point in
/// time".
///
/// Gated here rather than in the SPA, because a stray escape is not harmless. The
/// picker only opens at the prompt: mid-turn the keystroke interrupts the turn,
/// and at a question or a permission prompt it *answers* — cancelling the one and
/// declining the other. Those are the same two states the nudge refuses, for the
/// same reason.
///
/// Nothing is reconciled afterwards, and that is deliberate: a rewind that
/// restores files changes the worktree under the changed-files pane, but
/// `start_workspace_watcher` already re-reads every workspace holding a live
/// session on a 15s tick — it exists for exactly this class of change, the one no
/// hook reports.
pub async fn rewind_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<serde_json::Value> {
    let pty = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .ok_or_else(|| crate::state::no_such_session(id))?;
        // Nothing to rewind to. The picker opens on an empty conversation and has
        // nothing to offer, which reads as a broken button.
        if !s.had_a_turn {
            refuse!("this session has no conversation to rewind");
        }
        match &s.state {
            crate::model::State::YourTurn { reason, .. } => match reason {
                crate::model::TurnReason::AskedAQuestion => {
                    refuse!(
                        "it is asking you something — an escape would cancel the question, not rewind"
                    )
                }
                crate::model::TurnReason::NeedsPermission => {
                    refuse!("it is waiting for permission — an escape would decline it, not rewind")
                }
                _ => {}
            },
            other => {
                refuse!(
                    "the picker only opens at the prompt, and this session is {}",
                    match other {
                        crate::model::State::Working => "mid-turn",
                        crate::model::State::Starting => "still starting",
                        _ => "not live",
                    }
                )
            }
        }
        s.pty
            .clone()
            .filter(|p| p.is_alive())
            .ok_or_else(|| anyhow::anyhow!("session {id} has no live terminal"))?
    };

    // Two writes with a gap, not one `\x1b\x1b`: it is a double *tap*, so the TUI
    // is timing two key events. One burst risks arriving as a single escape — or
    // as the `ESC ESC` meta prefix — and the difference is invisible from here.
    // The gap is the same shape as the nudge's, which learned the lesson first.
    // On the pty's own queue, not this task's clock: `write` only *queues*, so
    // sleeping here separates two `send`s and not the two bytes at the fd.
    let _ = pty.write(b"\x1b");
    let _ = pty.pause(std::time::Duration::from_millis(60));
    let _ = pty.write(b"\x1b");
    Ok(Json(json!({ "rewinding": id })))
}

/// Forget a session outright.
///
/// [`kill_session`] is the other answer and the usual one: it ends the process
/// and keeps the row, because the scrollback and the conversation are still
/// worth something. This is for the ones that are not — a run you started by
/// mistake, twelve dead rows in one worktree — so the record goes, and with it
/// the copy of the transcript the daemon made for itself at teardown.
///
/// A live session is killed first: deleting the record while its pty runs would
/// leave an agent working in a worktree with nothing in the rail pointing at it.
/// Its own exit watcher still runs and still releases the locks and the PR's run
/// slot, because it holds the pty handle rather than looking the session up
/// again.
///
/// Claude Code's own transcript under `~/.claude/projects` is deliberately left
/// alone. It is not the daemon's file, and `claude --resume` outside orchd still
/// reads it.
pub async fn delete_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<serde_json::Value> {
    forget_session(&app, id).await?;
    Ok(Json(json!({ "deleted": id })))
}

/// End a session and drop its record. The body of [`delete_session`], shared with
/// [`discard_spawned`] so the agent's undo and the rail's delete cannot drift into
/// meaning different things.
async fn forget_session(app: &Arc<AppState>, id: SessionId) -> anyhow::Result<()> {
    let (pty, archived) = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .ok_or_else(|| crate::state::no_such_session(id))?;
        (s.pty.clone(), s.archived_transcript.clone())
    };

    if let Some(h) = pty {
        // Escalating, and awaited, because the record goes below: an agent that
        // outlives its row has no watcher, no rail entry and nothing in the UI that
        // can reach it, so "best effort" here meant leaking a live agent whenever
        // the child declined `SIGHUP`. A process already gone returns immediately.
        h.kill_gracefully().await;
    }
    app.release_main(id).await;

    {
        let mut inner = app.inner.write().await;
        inner.sessions.remove(&id);
    }
    if let Some(path) = archived {
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!("could not remove {}: {e}", path.display());
        }
    }

    // Persists the records without the deleted one, so it stays gone.
    app.notify().await;
    Ok(())
}

/// `orch kill` — undo a spawn.
///
/// **Only the sessions this caller spawned**, which is the whole of what makes it
/// safe to put on the ask token. That token opens asking and spawning; a destroy
/// that reached any session would reach the conversation you are sitting in, and an
/// agent only has to misread a uuid once. So the record's `spawned_by` is the
/// authorisation, and it is checked against the path's caller rather than trusted
/// from the body.
///
/// When that same spawn cut a worktree, the tree goes too — a spawn you regret
/// otherwise leaves a checkout on disk with no row in the rail pointing at it, which
/// is a worse mess than the row was. Through the ordinary preflight, so a tree with
/// uncommitted or unpushed work refuses and *says so* while the session is still
/// gone. Never for a workspace that already existed: `spawn_cut_worktree` is false
/// there, and a clean worktree of yours is not the agent's to remove.
pub async fn discard_spawned(
    State(app): State<Arc<AppState>>,
    Path((id, child)): Path<(Uuid, Uuid)>,
    headers: axum::http::HeaderMap,
) -> ApiResult<serde_json::Value> {
    ask_token_ok(&app, id, &headers).await?;

    let (workspace, cut) = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&child)
            .ok_or_else(|| crate::state::no_such_session(child))?;
        if s.spawned_by != Some(id) {
            refuse!("{child} is not a session you spawned; close it in the app");
        }
        (s.workspace.clone(), s.spawn_cut_worktree)
    };

    forget_session(&app, child).await?;

    // Only once the record is gone: preflight refuses a workspace with a live
    // session, and the session it would be refusing for is the one just discarded.
    let mut out = json!({ "killed": child, "workspace": workspace });
    // `MAIN` cannot be the answer here — `spawn_cut_worktree` is never true of it —
    // but the placeholder can, for a record an older daemon left behind: the tree
    // used to be cut by `claude --worktree`, which reported its name only at
    // `SessionStart`. No spawner produces it now (see `spawn_worktree_session`), so
    // this arm is defensive. Teardown would refuse "unknown workspace …creating",
    // which reads as a broken command rather than "you were faster than the hook".
    if cut && workspace != MAIN && workspace != spawn::PENDING_WORKTREE {
        match worktree::teardown(&app, &workspace).await {
            Ok(_) => out["removed"] = json!(workspace),
            // Not an error: the session is already gone and saying "kill failed"
            // would be false. The tree is still there and the reason is worth
            // reading, so it rides back as a note.
            Err(e) => out["kept"] = json!(format!("{e:#}")),
        }
    }
    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// The interaction channel
// ---------------------------------------------------------------------------

/// How long one poll waits before answering "not yet".
///
/// A blocking tool call survives well past this — a real session was held for
/// 150s and resumed cleanly — but a bounded wait the agent loops on is strictly
/// safer than betting on where the ceiling is. Whatever kills a long call, the
/// agent asks again rather than losing the turn.
const WAIT_SECS: u64 = 60;

#[derive(Deserialize)]
pub struct AskBody {
    pub question: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    pub options: Vec<crate::model::InteractionOption>,
}

/// A running session asks you something and blocks on the answer.
///
/// The first thing that travels from an agent *back* to the UI. Hooks are one-way
/// observers and the only daemon-to-agent path is a pty write, so a question used
/// to mean printing into a terminal and hoping; this makes it a card you answer.
///
/// One question per session at a time, deliberately: a queue of them would be a
/// UI that hides what the agent is actually stuck on.
pub async fn ask(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
    Json(body): Json<AskBody>,
) -> ApiResult<serde_json::Value> {
    ask_token_ok(&app, id, &headers).await?;
    if body.question.trim().is_empty() {
        refuse!("a question with no words");
    }
    if body.options.is_empty() {
        refuse!("a question with no options: the overlay renders buttons, not a prompt");
    }
    let interaction = crate::model::Interaction {
        id: Uuid::new_v4(),
        thread_id: body.thread_id,
        question: body.question,
        detail: body.detail,
        options: body.options,
        asked_at: std::time::SystemTime::now(),
        answer: None,
        answer_text: None,
    };
    let ask_id = interaction.id;
    {
        let mut inner = app.inner.write().await;
        let s = inner
            .sessions
            .get_mut(&id)
            .ok_or_else(|| crate::state::no_such_session(id))?;
        if let Some(open) = &s.interaction {
            if open.answer.is_none() {
                refuse_busy!("session {id} is already asking something else");
            }
        }
        s.interaction = Some(interaction);
        // Blocked on you is exactly what `YourTurn` means, so the rail, the
        // waitbar and the dot all say it without learning a new state. The clock
        // it starts is the one worth watching: how long the agent stood still.
        s.set_state(crate::model::State::YourTurn {
            since: std::time::SystemTime::now(),
            reason: crate::model::TurnReason::AskedAQuestion,
        });
    }
    app.notify().await;
    Ok(Json(json!({ "ask": ask_id })))
}

/// Block until the question is answered, or say "not yet".
///
/// The agent loops on this. `answered: false` is a normal outcome, not an error:
/// it means you have not decided yet, and the next call carries on waiting.
pub async fn ask_wait(
    State(app): State<Arc<AppState>>,
    Path((id, ask_id)): Path<(Uuid, Uuid)>,
    headers: axum::http::HeaderMap,
) -> ApiResult<serde_json::Value> {
    ask_token_ok(&app, id, &headers).await?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(WAIT_SECS);
    loop {
        // **Register for the wake before reading the answer.** `notify_waiters`
        // wakes only the futures that already exist, and `Notified` does not join
        // that list until it is polled — so creating it after the check drops the
        // answer that lands in between, and the caller waits out the whole deadline
        // for a question that is already answered. `enable` joins the list now.
        let wait = app.answered.notified();
        tokio::pin!(wait);
        wait.as_mut().enable();
        {
            let inner = app.inner.read().await;
            let s = inner
                .sessions
                .get(&id)
                .ok_or_else(|| crate::state::no_such_session(id))?;
            match &s.interaction {
                Some(i) if i.id == ask_id => {
                    if let Some(answer) = &i.answer {
                        return Ok(Json(json!({
                            "answered": true,
                            "answer": answer,
                            "text": i.answer_text,
                        })));
                    }
                }
                // Gone, or replaced by a later question: either way this one will
                // never be answered, and looping would hang the agent forever.
                _ => {
                    refuse!("question {ask_id} is no longer open on session {id}")
                }
            }
        }
        tokio::select! {
            _ = wait => {}
            _ = tokio::time::sleep_until(deadline) => {
                return Ok(Json(json!({ "answered": false })));
            }
        }
    }
}

/// Is this caller the agent whose session it claims to be?
///
/// The one place a credential other than the app token is accepted, so it is
/// deliberately narrow: it authenticates *this* session, for *these* routes, and
/// unlocks nothing else. The app token is taken as well, so the SPA and a test
/// can drive the same endpoints.
pub(crate) async fn ask_token_ok(
    app: &Arc<AppState>,
    id: Uuid,
    headers: &axum::http::HeaderMap,
) -> Result<(), ApiError> {
    let given = headers
        .get("x-orch-ask")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("x-orch-token").and_then(|v| v.to_str().ok()))
        .unwrap_or_default();
    if given == app.token {
        return Ok(());
    }
    let inner = app.inner.read().await;
    let s = inner
        .sessions
        .get(&id)
        .ok_or_else(|| ApiError(crate::state::no_such_session(id)))?;
    if given.is_empty() || given != s.ask_token {
        refuse!("bad ask token for session {id}");
    }
    Ok(())
}

/// Is this caller the triage run this PR is expecting proposals from?
///
/// The sibling of [`ask_token_ok`] for the one agent route that is keyed on a PR
/// rather than a session. Same shape and same reasoning: the app token is taken
/// too, so the SPA and the tests can drive the endpoint, but a run is given only
/// the narrow one.
///
/// A PR with no token recorded refuses everything except the app token — there is
/// no run to be, so there is nothing to authenticate as.
pub(crate) async fn proposal_token_ok(
    app: &Arc<AppState>,
    pr: u64,
    headers: &axum::http::HeaderMap,
) -> Result<(), ApiError> {
    let given = headers
        .get("x-orch-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if given == app.token {
        return Ok(());
    }
    let inner = app.inner.read().await;
    match inner.proposal_tokens.get(&pr) {
        Some(want) if !given.is_empty() && given == want => Ok(()),
        _ => Err(ApiError(anyhow::anyhow!(
            "bad proposals token for PR #{pr}"
        ))),
    }
}

#[derive(Deserialize)]
pub struct AnswerBody {
    pub ask: Uuid,
    pub answer: String,
    /// Required by an option that asked for words, refused by one that did not.
    #[serde(default)]
    pub text: Option<String>,
}

/// Your answer, which releases the tool call the agent is sitting in.
pub async fn answer(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(body): Json<AnswerBody>,
) -> ApiResult<serde_json::Value> {
    {
        let mut inner = app.inner.write().await;
        let s = inner
            .sessions
            .get_mut(&id)
            .ok_or_else(|| crate::state::no_such_session(id))?;
        let open = s
            .interaction
            .as_mut()
            .filter(|i| i.id == body.ask)
            .ok_or_else(|| anyhow::anyhow!("session {id} is not asking {}", body.ask))?;
        // Only what was offered. A free-text *value* would reach the agent as an
        // instruction nobody wrote a branch for; words are carried separately, by
        // an option that asked for them.
        let picked = open
            .options
            .iter()
            .find(|o| o.value == body.answer)
            .ok_or_else(|| anyhow::anyhow!("{} is not one of the options", body.answer))?;
        let text = body
            .text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        if picked.free && text.is_none() {
            refuse!(
                "\"{}\" is the option that asks for words, and none were written",
                picked.label
            );
        }
        if !picked.free && text.is_some() {
            refuse!("\"{}\" takes no words", picked.label);
        }
        open.answer = Some(body.answer.clone());
        open.answer_text = text.map(str::to_string);
        /* A yes to the daemon's own worktree question, and only to that one: the
        ask id is compared rather than the option value, because an agent writes
        its own values in `orch ask` and could otherwise grant itself.

        The folder comes off the ask rather than out of the answer, for the same
        reason: the agent chose which path to ask about, but only this record
        says which one the question the user read was about. */
        if body.answer == OUTSIDE_ALLOW {
            if let Some(asked) = s.outside_ask.as_ref().filter(|a| a.id == body.ask) {
                let path = asked.path.clone();
                if !s.outside_granted(&path) {
                    s.outside_grants.push(path);
                }
            }
        }
        // Answered, so it is going again. `Stop` will correct this if the turn
        // ends for real a moment later.
        s.set_state(crate::model::State::Working);
    }
    app.answered.notify_waiters();
    app.notify().await;
    Ok(Json(json!({ "answered": body.answer })))
}

#[derive(Deserialize)]
pub struct FileVerbBody {
    pub workspace: String,
    pub path: String,
    /// `stage`, `unstage` or `discard`.
    pub verb: String,
}

/// Stage a changed file, unstage it, or throw its working-tree changes away.
///
/// **The changed-files pane's right-click.** It reads `DiffFile::staged` and
/// `unstaged` to decide which verbs a row gets, so it offers only what exists —
/// most rows on a PR branch differ from the base because of a *commit* and get
/// none. That is presentation; the rules below are the daemon's, because the route
/// is reachable without the pane and a snapshot is a moment old by the time you
/// click.
///
/// Three refusals, and the middle one is the reason this is not just a git call:
///
/// * **A path that leaves the workspace.** `edit::resolve_in_workspace` is the one
///   spelling: relative, no `..`, and resolved under the root.
/// * **A session mid-turn in that workspace.** Staging or discarding under a
///   working agent changes what its next `git commit` picks up and what its next
///   read returns, which is the "changed underneath it" case `pre_edit`'s stale
///   notice exists for — and this one would be *your* doing rather than another
///   agent's. Refused rather than announced, because the fix is to wait a moment.
/// * **A verb the file has nothing for.** Asked of `git status` here rather than
///   trusted from the request: discarding a file with no working-tree change is a
///   no-op the user would read as the button not working, and staging one that is
///   already staged the same.
///
/// `Discard` is not undoable by git and the SPA confirms it by name. This side
/// refuses what it can and does not second-guess the rest: a confirmed discard is
/// an answer, not a suggestion.
pub async fn file_verb(
    State(app): State<Arc<AppState>>,
    Json(body): Json<FileVerbBody>,
) -> ApiResult<serde_json::Value> {
    use crate::git::FileVerb as V;
    let verb = match body.verb.as_str() {
        "stage" => V::Stage,
        "unstage" => V::Unstage,
        "discard" => V::Discard,
        other => refuse!("{other} is not a file verb"),
    };
    let Some(root) = app.workspace_path(&body.workspace).await else {
        refuse!("no such workspace: {}", body.workspace);
    };
    // Relative, no `..`, under the root — and the same call every other
    // client-named path in this daemon goes through.
    let path = crate::edit::resolve_in_workspace(&root, body.path.trim(), &[])?;
    let rel = path
        .strip_prefix(&root)
        .unwrap_or(&path)
        .to_string_lossy()
        .into_owned();

    if let Some(who) = app.busy_session_in(&body.workspace).await {
        refuse_busy!("{who} is mid-turn in {} — wait for it", body.workspace);
    }

    let (at, want) = (root.clone(), rel.clone());
    let has = crate::proc::run_blocking("reading the status", move || {
        crate::git::status_of(&at, &want)
    })
    .await??;
    let in_set = |set: &[crate::model::ChangedFile]| set.iter().any(|f| f.path == rel);
    let ok = match verb {
        V::Stage => in_set(&has.unstaged) || in_set(&has.untracked),
        V::Unstage => in_set(&has.staged),
        V::Discard => in_set(&has.unstaged),
    };
    if !ok {
        refuse!("{rel} has nothing to {}", body.verb);
    }

    let (at, p) = (root, rel.clone());
    crate::proc::run_blocking("the file verb", move || {
        crate::git::file_verb(&at, verb, &p)
    })
    .await??;
    // The pane is drawn from the reconcile, so it has to be the fresh one.
    let _ = app.reconcile(&body.workspace).await;
    Ok(Json(json!({ "done": body.verb, "path": rel })))
}

/// Text the drawer is handing to a session, and how much of it is allowed.
#[derive(Deserialize)]
pub struct TellBody {
    pub text: String,
}

/// A prompt is a line somebody reads, not a log. 8 KB is about 100 lines of build
/// output, which is more than the pane offers to send and far less than a ring
/// buffer holds (~3600 lines): a paste of that size is not a message, and it costs
/// the agent its context to be told so.
const TELL_MAX: usize = 8 * 1024;

/// Hand a session some text, as though you had typed it.
///
/// **The drawer's way of pointing at something.** A process pane holds the output
/// that explains what an agent just broke, and the only ways to get it across were
/// to retype it or to describe it. This types it, so it lands as an ordinary user
/// turn — which is what it is: you asked for it, and the transcript should say a
/// human said so.
///
/// The daemon owns *when*, because only it knows the session's state, and the
/// guards are [`nudge_sessions`]' with one target instead of every eligible one.
/// They are the whole safety story here:
///
/// * **Never into a working session.** A keystroke mid-turn is a stray line of
///   input, and Claude Code takes `Enter` on a half-typed prompt as a submit.
/// * **Never into a permission prompt or an open question.** Both read a keystroke
///   as an answer — consent, or whichever choice is highlighted. Refused by name,
///   so a press that did nothing says why rather than looking broken.
/// * **Never into an archived one**, which has no pty at all.
///
/// The text is the *client's*, not read out of the ring buffer here, because the
/// useful payload is usually a selection and only the pane knows what you
/// highlighted. That is no wider a door than the app token already opens
/// (`nudge_sessions` takes arbitrary text too, for every session at once).
pub async fn tell_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(body): Json<TellBody>,
) -> ApiResult<serde_json::Value> {
    let text = body.text.trim_end().to_string();
    if text.trim().is_empty() {
        refuse!("nothing to send");
    }
    if text.len() > TELL_MAX {
        refuse!(
            "that is {} KB — send a selection or the last lines, not the whole buffer",
            text.len() / 1024
        );
    }

    type_user_turn(&app, id, &text).await?;
    Ok(Json(json!({ "told": true })))
}

/// Type text at a session as an ordinary user turn, or say why a keystroke there
/// would mean something else.
///
/// The three refusals are the states where the pty is listening for an answer
/// rather than for a prompt: mid-turn (Claude Code submits whatever is half
/// typed), a permission prompt (consent) and an open question (the highlighted
/// choice). `nudge_sessions` learned them first, the drawer's hand-off second, and
/// the rebase button's is the third caller — which is what made this one function
/// instead of three copies of the table.
pub(crate) async fn type_user_turn(
    app: &Arc<AppState>,
    id: SessionId,
    text: &str,
) -> anyhow::Result<()> {
    let pty = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .ok_or_else(|| crate::state::no_such_session(id))?;
        let name = s.label().unwrap_or(&s.workspace).to_string();
        let Some(pty) = s.pty.clone().filter(|p| p.is_alive()) else {
            anyhow::bail!("{name} is not running — resume it first");
        };
        match &s.state {
            crate::model::State::Starting => {
                anyhow::bail!("{name} is still starting")
            }
            crate::model::State::YourTurn { reason, .. } => match reason {
                // Both take a keystroke as an answer rather than as a prompt.
                crate::model::TurnReason::NeedsPermission => {
                    anyhow::bail!("{name} is waiting on a permission prompt; answer that first")
                }
                crate::model::TurnReason::AskedAQuestion => {
                    anyhow::bail!("{name} is asking you something; answer that first")
                }
                _ => {}
            },
            // Working, and everything else that is not a prompt: mid-turn.
            other => {
                if other.is_busy() {
                    anyhow::bail!("{name} is mid-turn; wait for it to finish")
                }
            }
        }
        pty
    };

    // The same two-step every typed line uses: write, wait, then send. Claude
    // Code's prompt box drops a `\r` that arrives in the same breath as the text.
    pty.type_and_send(text.as_bytes(), std::time::Duration::from_millis(500));
    Ok(())
}

/// The two option values [`allow_outside`] offers, and the one a yes carries.
///
/// Constants because three places have to agree on the spelling: the question,
/// the grant in [`answer`], and the words `orch outside` prints back.
pub const OUTSIDE_ALLOW: &str = "outside-allow";
pub const OUTSIDE_NO: &str = "outside-no";

#[derive(Deserialize)]
pub struct OutsideBody {
    /// What the agent was refused, named in the question so the answer is an
    /// informed one rather than a blanket yes.
    pub path: String,
}

/// Ask whether this session may run git outside its own worktree.
///
/// The other half of `guard::isolation`: the guard refuses by default and
/// its refusal names this command, so "not allowed" is a question rather than a
/// wall. It is the *ordinary* ask — the same `Interaction`, the same box in the
/// SPA, the same `/ask/:id/wait` the agent already polls — because a second
/// permission mechanism beside that one is how two of them come to disagree.
///
/// The grant is remembered on the session and nowhere else, so it lasts exactly as
/// long as the conversation in front of you; `Session::outside_grants` says why a
/// restart asks again.
///
/// **One question per folder.** A yes covers the path it names and what is under
/// it and nothing else, so a session already let out to one checkout is asked
/// again about the next. That is the whole point of naming the path in the
/// question: an answer about `/repo` was never an answer about anywhere else.
pub async fn allow_outside(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
    Json(body): Json<OutsideBody>,
) -> ApiResult<serde_json::Value> {
    ask_token_ok(&app, id, &headers).await?;
    let path = body.path.trim().to_string();
    if path.is_empty() {
        refuse!("name the path you were refused");
    }
    /* **Absolute, and folded, because the grant is *compared* — not stored and
    forgotten.** `guard::isolation` judges paths it has resolved against the
    command's own cwd, so a grant kept as the agent's literal `../other` or `.`
    matches nothing it will ever be asked about. And the loop that leaves is
    silent: `outside_granted` matches the same literal string, so every later
    `orch outside ../other` short-circuits to `allowed` without a question being
    raised, while the guard goes on refusing. The agent then retries a permission
    it believes it holds.
    Refused rather than resolved here, because *this* process is the daemon and
    resolving against its cwd would invent a path in the wrong tree — and the
    refusal the agent was handed already names an absolute one. */
    if !std::path::Path::new(&path).is_absolute() {
        refuse!("{path} is not an absolute path — ask about the one the refusal named");
    }
    let path = crate::guard::fold(std::path::Path::new(&path))
        .to_string_lossy()
        .into_owned();
    {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .ok_or_else(|| crate::state::no_such_session(id))?;
        // Already yes for this folder: asking again would spend attention on a
        // decision that is still in force. Covered by an outer grant counts, since
        // that is exactly what the outer yes said.
        if s.outside_granted(std::path::Path::new(&path)) {
            return Ok(Json(json!({ "allowed": true, "asked": false })));
        }
    }
    let interaction = crate::model::Interaction {
        id: Uuid::new_v4(),
        thread_id: None,
        question: format!(
            "This session works in one worktree, and it wants to run git in {path}. \
             Allow that folder for the rest of this session?"
        ),
        // What the guard protects, in the words of the thing that could go wrong.
        detail: Some(
            "Changing another checkout from here can move a branch the app is \
             tracking, or leave main on a branch nothing recorded."
                .to_string(),
        ),
        options: vec![
            crate::model::InteractionOption {
                value: OUTSIDE_ALLOW.to_string(),
                label: "Allow it".to_string(),
                // Both limits, because both are the reason a yes here is a small
                // thing to say: this folder only, and this session only.
                sub: "this folder and below, until the session ends".to_string(),
                free: false,
            },
            crate::model::InteractionOption {
                value: OUTSIDE_NO.to_string(),
                label: "Keep it in its worktree".to_string(),
                sub: "the guard goes on refusing".to_string(),
                free: false,
            },
        ],
        asked_at: std::time::SystemTime::now(),
        answer: None,
        answer_text: None,
    };
    let ask_id = interaction.id;
    {
        let mut inner = app.inner.write().await;
        let s = inner
            .sessions
            .get_mut(&id)
            .ok_or_else(|| crate::state::no_such_session(id))?;
        if let Some(open) = &s.interaction {
            if open.answer.is_none() {
                refuse_busy!("session {id} is already asking something else");
            }
        }
        s.interaction = Some(interaction);
        // The ask that may grant it, and the folder it grants: `answer` can then
        // tell this question from one the agent wrote itself, and knows what a yes
        // is about — see `Session::outside_ask`.
        s.outside_ask = Some(crate::model::OutsideAsk {
            id: ask_id,
            path: std::path::PathBuf::from(&path),
        });
        s.set_state(crate::model::State::YourTurn {
            since: std::time::SystemTime::now(),
            reason: crate::model::TurnReason::AskedAQuestion,
        });
    }
    app.notify().await;
    Ok(Json(
        json!({ "allowed": false, "asked": true, "ask": ask_id }),
    ))
}

/// The folders this session has been let out to, which is what the guard reads
/// per git command.
///
/// A read the guard makes before it refuses anything, so it is cheap on purpose:
/// no git, no disk, one map lookup.
///
/// It answers the *list* rather than a yes or no, because the rule needs to know
/// which folders: `orch guard push` used to drop the worktree from its `Call` on a
/// blanket yes, and that cannot express "this checkout but not that one".
pub async fn outside_allowed(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
) -> ApiResult<serde_json::Value> {
    ask_token_ok(&app, id, &headers).await?;
    let inner = app.inner.read().await;
    let s = inner
        .sessions
        .get(&id)
        .ok_or_else(|| crate::state::no_such_session(id))?;
    let paths: Vec<String> = s
        .outside_grants
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    Ok(Json(json!({ "paths": paths })))
}

#[derive(Deserialize)]
pub struct SpawnBody {
    /// An *existing* workspace to work in. Defaults to the caller's own, which is
    /// what you mean when you want a hand with the thing you are already doing.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Cut a fresh worktree for the new session instead.
    ///
    /// A separate field rather than "create `workspace` if it is missing", because
    /// the two are different acts and only one of them should tolerate a typo: an
    /// unknown `workspace` must stay an error, or `--workspace dependabot-api`
    /// misspelt silently becomes a whole new checkout. This is the shape the CLI
    /// could not express at all, which is what made "spawn two independent fixers"
    /// impossible — both landed in the caller's own tree, sharing one git index.
    #[serde(default)]
    pub worktree: bool,
    /// What to call that worktree. Absent means the daemon names it.
    #[serde(default)]
    pub name: Option<String>,
    /// Typed into the new session once it is up. Without one it sits at a prompt.
    #[serde(default)]
    pub prompt: Option<String>,
}

/// One session starts another.
///
/// Authenticated by the caller's own ask token, so the daemon knows who asked and
/// an agent cannot spawn on behalf of a session it is not. The child gets a token
/// of its own and can spawn in turn — deliberately, since a session that can hand
/// work off is the point — which is exactly why the headroom check in
/// `spawn_session` is not optional: recursion plus no limit is how a machine dies.
pub async fn spawn_from_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SpawnBody>,
) -> ApiResult<serde_json::Value> {
    ask_token_ok(&app, id, &headers).await?;

    let mine = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .get(&id)
            .map(|s| s.workspace.clone())
            .ok_or_else(|| crate::state::no_such_session(id))?
    };

    let named = body
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty());
    // Two names for one place is a request nobody can honour, and guessing which
    // half was meant is how a fixer ends up in the tree you were reading.
    if named.is_some() && body.worktree {
        refuse!("name a workspace or ask for a worktree, not both");
    }
    // An unknown name is an error, and it now says what the names *are*. The old
    // message ("unknown workspace dependabot-management-api") reads as a rule you
    // have to go and discover, when the list that would settle it is one read away.
    if let Some(w) = named {
        let known = app.inner.read().await;
        if !known.workspaces.contains_key(w) {
            let mut names: Vec<&str> = known.workspaces.keys().map(String::as_str).collect();
            names.sort_unstable();
            refuse!(
                "unknown workspace {w} — known: {}. Use worktree:true to cut a new one",
                names.join(", ")
            );
        }
    }

    // Main can hold one live session and the caller *is* it, so defaulting to the
    // caller's own workspace made `orch new` impossible from main: every call came
    // back "main is occupied by session <itself>". An agent asked to hand work off
    // then has nowhere to put it. So the default is "somewhere this can actually
    // run": your own tree when you are in a worktree, which is what you mean when
    // you want a hand with what you are already doing, and a fresh worktree when
    // you are in main.
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let cut = body.worktree || (named.is_none() && mine == MAIN);
    let child = match named {
        Some(w) => spawn::spawn_session(&app, w, None, None).await?,
        None if cut => spawn::spawn_worktree_session(&app, name, None).await?,
        None => spawn::spawn_session(&app, &mine, None, None).await?,
    };
    {
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&child) {
            if let Some(prompt) = body
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
            {
                // The same path a vendored prompt takes: typed in at `SessionStart`,
                // because an interactive session honours nothing else.
                s.pending_prompt = Some(prompt.to_string());
            }
            // What makes this spawn undoable — see `discard_spawned`. Recorded on the
            // child rather than kept as a list on the parent, so the answer survives
            // the parent being forgotten and cannot go stale.
            s.spawned_by = Some(id);
            s.spawn_cut_worktree = cut;
        }
    }
    app.notify().await;
    // Read back off the record rather than echoed, because when the default cut a
    // worktree the caller never named it and has no other way to learn where its
    // request landed. It can still be the `…creating` placeholder for a record an
    // older daemon left behind — the tree used to be cut by `claude --worktree`,
    // which reported its name only at `SessionStart` — but no spawner produces one
    // now.
    //
    // The path comes back for the same reason the workspace does, one step further:
    // a bare id told the caller nothing about *where*, so confirming a spawn went
    // where you meant took a second call.
    let (workspace, path) = app
        .inner
        .read()
        .await
        .sessions
        .get(&child)
        .map(|s| (s.workspace.clone(), s.cwd.to_string_lossy().into_owned()))
        .unwrap_or_default();
    Ok(Json(
        json!({ "session": child, "workspace": workspace, "path": path }),
    ))
}

#[derive(Deserialize)]
pub struct ProcessBody {
    /// Which configured process to start. A name, never a command — see below.
    pub name: String,
}

/// One session starts a managed process in its own workspace.
///
/// The agent-facing half of the drawer's restart button, and the same shape as
/// `spawn_from_session`: authenticated by the caller's own ask token, acting on the
/// workspace the caller is actually in. An agent asked to bring the stack up should
/// not have to tell you to press a button.
///
/// **A name, never a command.** §12 refuses a generic "run this" endpoint, and this
/// is not one by a different door: the name is resolved against the processes this
/// workspace *declares* in config, and anything else is refused with the list of
/// what it could have meant. So the reachable set is exactly what the drawer shows,
/// which is the property that makes handing it to an agent unremarkable.
///
/// Refused when it is already running, rather than restarting it the way the
/// drawer's button does. The button is yours, pressed while looking at the tab; an
/// agent killing a watch you are reading, halfway through its own turn, is a
/// different act — and "it is already up" is what the agent needed to know anyway.
pub async fn process_from_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ProcessBody>,
) -> ApiResult<serde_json::Value> {
    ask_token_ok(&app, id, &headers).await?;

    let workspace = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .get(&id)
            .map(|s| s.workspace.clone())
            .ok_or_else(|| crate::state::no_such_session(id))?
    };

    let spec = app
        .cfg
        .managed_spec(&workspace, &body.name)
        .ok_or_else(|| {
            let known = managed_names(&app, &workspace);
            anyhow::anyhow!(
                "{workspace} declares no process called {}; it has {}",
                body.name,
                if known.is_empty() {
                    "none at all".to_string()
                } else {
                    known.join(", ")
                }
            )
        })?;

    {
        let inner = app.inner.read().await;
        let alive = inner.workspaces.get(&workspace).is_some_and(|w| {
            w.processes.iter().any(|p| {
                p.name == spec.name && p.pty.as_ref().is_some_and(|h| h.exit_code().is_none())
            })
        });
        if alive {
            refuse!("{} is already running in {workspace}", spec.name);
        }
    }

    let process = crate::managed::start_managed(&app, &workspace, &spec).await?;
    app.notify().await;
    Ok(Json(json!({ "process": process, "workspace": workspace })))
}

#[derive(Deserialize)]
pub struct NudgeBody {
    /// What to type. Defaults to `continue`, which is the whole point.
    #[serde(default)]
    pub text: Option<String>,
}

/// Type one word into every session that is sitting waiting on you.
///
/// After a restart the rail comes back full of agents that were mid-something and
/// are now parked at an empty prompt. Poking each one by hand is the tax on
/// auto-resume being any good, so this pays it once.
///
/// **A session showing a permission prompt is skipped**, and that is the whole
/// safety of it: that prompt takes a keystroke as an answer, so typing into it
/// would be approving something on your behalf, chosen by whichever option
/// happens to be under the cursor. Skipped and named, rather than nudged and
/// hoped for.
pub async fn nudge_sessions(
    State(app): State<Arc<AppState>>,
    Json(body): Json<NudgeBody>,
) -> ApiResult<serde_json::Value> {
    let text = body
        .text
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("continue")
        .to_string();

    let (targets, held) = {
        let inner = app.inner.read().await;
        let mut targets = Vec::new();
        let mut held = Vec::new();
        for s in inner.sessions.values() {
            let Some(pty) = s.pty.clone().filter(|p| p.is_alive()) else {
                continue;
            };
            // Nothing to continue: a session that has never had a turn would take
            // the word as its opening instruction, which is not what anyone
            // pressing this meant.
            if !s.had_a_turn {
                continue;
            }
            // Working, starting, failing: not waiting on you, so not yours to
            // interrupt — a nudge into a running turn is a stray line of input.
            // Which is why only `YourTurn` is looked at at all.
            if let crate::model::State::YourTurn { reason, .. } = &s.state {
                // `Ready` and interrupted: relaunched with an unfinished turn
                // behind it and never prompted since, which is the one state
                // where "continue" is both true and what you meant.
                //
                // The word is a real instruction that lands in the transcript, so
                // the other waiting states are wrong targets rather than merely
                // unnecessary ones. A finished turn would be told to invent more
                // work; a thread asking you something would get a non-answer; a
                // permission prompt or a question takes the keystroke as consent.
                match reason {
                    // And only one that was cut off mid-turn. A conversation that
                    // had finished before the restart comes back at the same empty
                    // prompt, and "continue" there invents the next piece of work.
                    crate::model::TurnReason::Ready if s.interrupted => targets.push((s.id, pty)),
                    // Both take a keystroke as an answer: a permission prompt as
                    // consent, a question as whichever choice is highlighted.
                    // Named rather than skipped, so pressing the button does not
                    // quietly leave the sessions that most need you behind.
                    crate::model::TurnReason::NeedsPermission
                    | crate::model::TurnReason::AskedAQuestion => {
                        held.push(s.label().unwrap_or(&s.workspace).to_string());
                    }
                    _ => {}
                }
            }
        }
        (targets, held)
    };

    let nudged: Vec<String> = targets.iter().map(|(id, _)| id.to_string()).collect();
    for (n, (_, pty)) in targets.into_iter().enumerate() {
        let text = text.clone();
        tokio::spawn(async move {
            // Staggered, the way auto-resume staggers its spawns: four agents all
            // being typed into on the same tick is four prompt boxes competing for
            // the same instant, and one of them swallowed the return.
            tokio::time::sleep(std::time::Duration::from_millis(250 * n as u64)).await;
            // 500ms rather than `SessionStart`'s 300: the shorter gap left one
            // session in four holding typed text it never sent.
            pty.type_and_send(text.as_bytes(), std::time::Duration::from_millis(500));
        });
    }
    Ok(Json(json!({ "nudged": nudged, "held": held })))
}

/// Resume an archived session.
///
/// A live worktree resumes trivially — relaunch with cwd set to the recorded
/// path. A torn-down one is rebuilt here from its recovery record (branch and
/// `head_sha`) at the same absolute path, then relaunched; if HEAD has moved on
/// since, that is reported as a warning rather than refused (§2). Only a
/// transcript-only session — branch gone, commit unreachable — cannot resume.
pub async fn resume_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<serde_json::Value> {
    revive(&app, id).await
}

/// What a restart route may be told. Optional, for the reason [`AdoptOutside`]
/// gives: a bare `curl -X POST` should get the ordinary answer.
#[derive(Default, Deserialize)]
pub struct RestartAsk {
    /// Take the session back out of the queue instead.
    #[serde(default)]
    pub cancel: bool,
}

/// Respawn one session on the `claude` installed now, once it is safe to.
///
/// Answers at once with what was queued; [`crate::restart::run_due`] does the
/// respawn, now if the session is at its prompt and when its turn ends if not.
pub async fn restart_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    body: Option<Json<RestartAsk>>,
) -> ApiResult<serde_json::Value> {
    if body.is_some_and(|Json(b)| b.cancel) {
        crate::restart::cancel(&app, id).await?;
        return Ok(Json(json!({ "cancelled": true })));
    }
    Ok(Json(serde_json::to_value(
        crate::restart::queue(&app, crate::restart::Which::One(id)).await?,
    )?))
}

/// What "restart all" may be told.
#[derive(Default, Deserialize)]
pub struct RestartAll {
    /// Only the sessions an upgrade left behind — the agent bar's button.
    #[serde(default)]
    pub stale: bool,
}

/// The same for every live session in this checkout — what a `mise up` of Claude
/// Code wants, without quitting the app.
pub async fn restart_sessions(
    State(app): State<Arc<AppState>>,
    body: Option<Json<RestartAll>>,
) -> ApiResult<serde_json::Value> {
    let which = if body.is_some_and(|Json(b)| b.stale) {
        crate::restart::Which::Stale
    } else {
        crate::restart::Which::All
    };
    Ok(Json(serde_json::to_value(
        crate::restart::queue(&app, which).await?,
    )?))
}

/// Look again for conversations this daemon did not start.
///
/// The archive fold asks for this when you open it, because the poller behind
/// [`crate::state::AppState::rescan_external`] runs on a minute and a list that is
/// a minute stale is a list missing the terminal you just closed. Cheap by
/// construction: a `read_dir` per workspace and a `stat` per file, since an
/// unchanged transcript keeps the title the last scan read.
pub async fn refresh_external(State(app): State<Arc<AppState>>) -> ApiResult<serde_json::Value> {
    // `found` rather than `external`: that name is an array of conversations
    // everywhere else, and a count under it reads as an empty list to anything
    // that looks.
    let found = app.rescan_external().await;
    Ok(Json(json!({ "found": found })))
}

/// Continue a conversation this daemon did not start.
///
/// The sibling of [`resume_session`] for the rows `store::external_conversations`
/// finds: a `claude` somebody ran in a terminal in this checkout. There is no
/// record to revive, so there is nothing to rebuild either: the tree it was in is
/// a live workspace or it would not have been listed. The whole of it is a
/// spawn with `--resume`, which resolves a conversation by id wherever the file
/// sits. From the next snapshot on it is an ordinary session with an ordinary row.
///
/// **It cannot tell whether that conversation is still open somewhere.** A shell's
/// `claude` reports nothing to the daemon, so the only evidence is an mtime, and
/// two agents appending to one transcript is a real way to lose turns. The recency
/// is handed back as a warning rather than a refusal: the daemon does not know, and
/// the person who opened the terminal does.
#[derive(Default, Deserialize)]
pub struct AdoptOutside {
    /// Take it over although the transcript was written to moments ago. The rail
    /// asks before it sends this; see [`resume_external`].
    #[serde(default)]
    pub force: bool,
}

pub async fn resume_external(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    // Optional, because the safe answer is the default one and a POST with no body
    // at all is how this route is reached by hand. Required, it answered a bare
    // `curl -X POST` with a content-type complaint rather than with the refusal
    // that was the whole point of the call.
    body: Option<Json<AdoptOutside>>,
) -> ApiResult<serde_json::Value> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    /* Two clicks are two spawns of the same conversation id, and nothing further
    down stops them: the record does not exist until the spawn inserts it, so the
    "already one of ours" check above passes twice, and `refuse_if_occupied` finds
    no live session either. The claim is held for the whole adoption and released
    by the guard on every path out. */
    let Some(_claim) = app.try_claim(format!("adopt:{id}")) else {
        refuse_busy!(
            "conversation {} is already being taken over",
            crate::model::short_id(&id)
        );
    };
    let (workspace, last_used, was_on) = {
        let inner = app.inner.read().await;
        // A record means the archive has a row of its own for it, and that row
        // rebuilds the worktree where this one assumes it stands.
        if inner.sessions.contains_key(&id) {
            refuse!(
                "session {} is one of this daemon's own; resume it from its row",
                crate::model::short_id(&id)
            );
        }
        let Some(found) = inner.external.iter().find(|c| c.id == id) else {
            refuse!(
                "no conversation {} in this checkout's transcripts",
                crate::model::short_id(&id)
            );
        };
        (
            found.workspace.clone(),
            found.last_used,
            found.branch.clone(),
        )
    };
    /* **Asked before anything is written, not warned about afterwards.** The spawn
    itself appends to that transcript — `store::clear_worktree_pin`, whose own doc
    says never to do that beside a live agent — so by the time a warning could be
    composed the file has already been written to. The daemon cannot see a shell's
    `claude` at all (no pty here, no hook, no pid it knows), so the mtime is the
    whole of the evidence and the person who opened the terminal is the one who
    knows. `ExternalView::may_be_live` is what lets the rail ask rather than
    discover this as a refusal. */
    if !body.force && crate::store::recently_active(last_used) {
        refuse!(
            "conversation {} was written to moments ago and may still be open in a terminal; \
             close it first, or adopt it anyway",
            crate::model::short_id(&id)
        );
    }
    refuse_if_occupied(&app, &workspace).await?;
    let new_id =
        spawn::spawn_session(&app, &workspace, None, Some(spawn::Source::Resume(id))).await?;
    let now_on = {
        let mut inner = app.inner.write().await;
        /* Out of the list the moment it has a record, or the rail draws it twice —
        once as a live session and once as an outside conversation — until the next
        scan. The scan itself filters on exactly this, so this is the same rule
        applied a minute earlier rather than a second answer. */
        inner.external.retain(|c| c.id != id);
        inner
            .workspaces
            .get(&workspace)
            .and_then(|w| w.tree.branch.clone())
    };
    /* The drift `revive` warns about, for a conversation with no recovery record to
    compare against. A worktree path is reused — `ensure_pr_worktree` puts every run
    of `pr-16` in one tree — so its transcript directory collects every conversation
    that path ever held, and adopting one can drop an agent that remembers one
    branch into a tree checked out for another. The transcript's own `gitBranch` is
    what makes the comparison possible. */
    let warning = match (was_on, now_on) {
        (Some(was), Some(now)) if was != now => Some(format!(
            "this conversation was working on {was}; {workspace} is on {now} now"
        )),
        _ => None,
    };
    Ok(Json(json!({ "session": new_id, "warning": warning })))
}

/// Branch off a conversation instead of continuing it.
///
/// The new run starts with the whole conversation behind it and writes to an id
/// of its own, so the original is still sitting there to come back to. That is
/// the "same context, new direction" case (§2) — reading the answer, then asking
/// for something else, without losing the version that got you there.
///
/// **In a worktree of its own.** Sharing the parent's tree made "new direction"
/// a lie: two agents editing one checkout, and whichever wrote last decided what
/// the other was looking at. `--resume` resolves a session by id wherever it was
/// recorded, not by working directory, so the fork carries the conversation into
/// a tree the parent has never touched.
///
/// That also makes a fork cheaper than a resume: nothing has to be rebuilt, so a
/// conversation whose branch is long gone can still be forked.
///
/// A session started as a [`Pass`] forks like any other. It
/// used to resume instead, on the reasoning that a run cut a fresh worktree from
/// upstream and so would come back on the wrong code — but "fork" that silently
/// continues one conversation is the worse surprise, and the new tree is the
/// answer to two agents in one checkout either way.
pub async fn fork_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<serde_json::Value> {
    let had_a_turn = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .get(&id)
            .ok_or_else(|| crate::state::no_such_session(id))?
            .had_a_turn
    };
    // Refuse before a worktree is cut, not after the fork dies in it. A fork
    // replays the conversation with `--resume`, so a session that never had a turn
    // forks into an instant exit — and `spawn_worktree_session` would have done a
    // real `git worktree add` first, leaving a fresh tree holding a dead session.
    // The SPA greys the menu item on the same bit, but a stale snapshot or a direct
    // call reaches here regardless, which is why the guard lives on this side too.
    if !had_a_turn {
        refuse!(
            "session {} has no conversation yet — nothing to fork",
            crate::model::short_id(&id)
        );
    }
    let new_id = spawn::spawn_worktree_session(&app, None, Some(id)).await?;
    Ok(Json(
        json!({ "session": new_id, "warning": None::<String> }),
    ))
}

/// Resume: get the worktree back, then relaunch under the same id.
async fn revive(app: &Arc<AppState>, id: Uuid) -> ApiResult<serde_json::Value> {
    let (workspace, recovery, cwd) = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .ok_or_else(|| crate::state::no_such_session(id))?;
        (s.workspace.clone(), s.recovery.clone(), s.cwd.clone())
    };
    let path_exists = cwd.exists();

    if matches!(recovery, Some(ArchiveState::TranscriptOnly)) {
        refuse!(
            "session {id} is transcript-only: the branch is gone and the commit is unreachable"
        );
    }

    // Resume-into-occupied: the worktree this session lived in may hold a fresh
    // live session now. Refuse before rebuilding anything. The session being revived
    // is archived, so it is never the occupant this finds.
    refuse_if_occupied(app, &workspace).await?;

    // A worktree that was torn down is rebuilt before the session goes back into
    // it. `worktree::revive` owns that, next to the archive that recorded it; the
    // warning it hands back means the branch moved on, which is worth saying and
    // not worth refusing.
    let warning = if path_exists {
        // Standing, but not necessarily *this* conversation's tree — the rebuild
        // that would have noticed is the branch that is skipped here.
        // Off the runtime, because it reads the tree's branch with a git child
        // process, like every other git call on this path.
        let (at, rec) = (cwd.clone(), recovery.clone());
        crate::proc::run_blocking("reading the revived tree's branch", move || {
            crate::worktree::branch_drift(&at, rec.as_ref())
        })
        .await
        .unwrap_or(None)
    } else {
        crate::worktree::revive(app, &cwd, recovery)
            .await
            .with_context(|| format!("session {id}"))?
    };

    // Its recorded pass, not `None`: the run's PR and command are what
    // `posts_proposals` reads for the post token, and what the bar reads to say
    // which PR this session is a pass over.
    let pass = {
        let inner = app.inner.read().await;
        inner.sessions.get(&id).and_then(|s| s.pass.clone())
    };
    let new_id =
        spawn::spawn_session(app, &workspace, pass, Some(spawn::Source::Resume(id))).await?;
    Ok(Json(json!({ "session": new_id, "warning": warning })))
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

pub async fn new_shell(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    let id = crate::managed::spawn_shell(&app, &workspace).await?;
    Ok(Json(json!({ "process": id })))
}

/// Upgrade the agent binary, reporting through the bar that offered it.
///
/// Deliberately not a blocking call: mise fetches and unpacks, so the button
/// answers at once and the bar follows the run through the snapshot
/// (`update::UpgradeRun`). A failure keeps the *end* of the output, which is
/// the part that says why — an upgrade that fails silently behind a toast is worse
/// than no button.
///
/// Not a drawer process, which is where this used to run. The drawer is one
/// workspace's processes and upgrading the agent belongs to no workspace, so from
/// any worktree the run was invisible while main's drawer grew a tab that was not
/// main's process at all.
///
/// Safe to press with sessions running, which is the whole reason it is a button:
/// mise installs into a versioned directory and repoints, so a running `claude`
/// keeps the image it loaded. Sessions in flight finish on the old version; the
/// next one spawned gets the new. Nothing is restarted and nothing is asked of
/// you afterwards.
///
/// Refuses when the poller has not found an update, rather than running `mise
/// upgrade` on a hunch — the tool name comes from what mise reported, so without
/// that there is nothing to name. And refuses a second run while one is going,
/// because two `mise upgrade`s of one tool race over the same install directory.
pub async fn upgrade_agent(State(app): State<Arc<AppState>>) -> ApiResult<serde_json::Value> {
    upgrade(&app, crate::update::Subject::Agent).await
}

/// Both upgrade routes, which differ only in their subject.
///
/// The refusals, the claim and the answer are `update::start_upgrade`'s; this is
/// the HTTP shape over it. Two copies is how one of them would keep a fix the
/// other got, and the reporting here has already been got wrong once.
async fn upgrade(
    app: &Arc<AppState>,
    subject: crate::update::Subject,
) -> ApiResult<serde_json::Value> {
    let v = crate::update::start_upgrade(app, subject)
        .await
        .map_err(|why| ApiError(anyhow::anyhow!(why)))?;
    Ok(Json(json!({ "from": v.from, "to": v.to })))
}

/// The same for the two dismiss routes.
async fn dismiss(
    app: &Arc<AppState>,
    subject: crate::update::Subject,
) -> ApiResult<serde_json::Value> {
    crate::update::dismiss(app, subject)
        .await
        .map_err(|why| ApiError(anyhow::anyhow!(why)))?;
    Ok(Json(json!({ "dismissed": true })))
}

/// Upgrade the app itself to the release the bar is offering.
///
/// The sibling of [`upgrade_agent`], and everything it says about pressing a
/// button that runs `mise upgrade` holds here too — with one difference that runs
/// the other way. Upgrading the agent is invisible to what is running; upgrading
/// *this* is not applied at all until the app restarts, because the process
/// serving you is the old build and mise installs beside it. So a finished run
/// reports "restart", and the button becomes the restart.
///
/// Refuses when there is no update, when the install is not mise's (nothing to
/// name in `mise upgrade`), and when a run is already going.
pub async fn upgrade_app(State(app): State<Arc<AppState>>) -> ApiResult<serde_json::Value> {
    upgrade(&app, crate::update::Subject::App).await
}

/// Put the app upgrade's report away. [`dismiss_agent_upgrade`]'s sibling, and
/// refuses mid-run for the same reason.
pub async fn dismiss_app_upgrade(State(app): State<Arc<AppState>>) -> ApiResult<serde_json::Value> {
    dismiss(&app, crate::update::Subject::App).await
}

/// Put a finished upgrade's report away.
///
/// Daemon-side rather than a flag in the SPA, because the report is: a bar
/// dismissed in one window and back on the next reload is the same bar arguing
/// with you. Refuses while the run is going — there is nothing to dismiss yet, and
/// clearing it would leave the button enabled beside a running `mise upgrade`.
pub async fn dismiss_agent_upgrade(
    State(app): State<Arc<AppState>>,
) -> ApiResult<serde_json::Value> {
    dismiss(&app, crate::update::Subject::Agent).await
}

/// The names that workspace could start, for an error worth reading.
fn managed_names(app: &Arc<AppState>, workspace: &str) -> Vec<String> {
    app.cfg
        .processes_for(workspace)
        .iter()
        .map(|s| s.name.clone())
        .collect()
}

pub async fn restart_process(
    State(app): State<Arc<AppState>>,
    Path((workspace, name)): Path<(String, String)>,
) -> ApiResult<serde_json::Value> {
    let spec = app
        .cfg
        .managed_spec(&workspace, &name)
        .ok_or_else(|| anyhow::anyhow!("no managed process {name} for {workspace}"))?;

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
        // Through the stop path, or a restart of a `docker compose exec` watcher
        // leaves the old one running in the container and starts a second beside it.
        crate::managed::stop_managed(&app, &workspace, &name, &h).await;
    }

    let id = crate::managed::start_managed(&app, &workspace, &spec).await?;
    Ok(Json(json!({ "process": id })))
}

pub async fn close_process(
    State(app): State<Arc<AppState>>,
    Path(proc_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    // Found under the read lock and stopped without it: `stop_managed` awaits a
    // bounded command, and holding the write lock across that would freeze every
    // snapshot for as long as the stop takes.
    let found = {
        let inner = app.inner.read().await;
        inner.workspaces.values().find_map(|w| {
            w.processes
                .iter()
                .find(|p| p.id == proc_id)
                .map(|p| (w.id.clone(), p.name.clone(), p.pty.clone()))
        })
    };
    if let Some((workspace, name, Some(h))) = found {
        crate::managed::stop_managed(&app, &workspace, &name, &h).await;
    }
    let mut inner = app.inner.write().await;
    for w in inner.workspaces.values_mut() {
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

pub async fn teardown(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<worktree::Preflight> {
    Ok(Json(worktree::teardown(&app, &workspace).await?))
}

#[derive(Deserialize)]
pub struct TeardownBody {
    pub workspace: String,
}

/// `orch teardown <workspace>`: the same teardown as the rail's button, reached
/// on the ask token.
///
/// Wider than `discard_spawned`, on purpose, and no wider than the button: any
/// worktree, but through the same preflight, so a live session, a dirty tree or
/// an unpushed commit refuses here exactly as it refuses a right-click, and main
/// is never a worktree. Closed to agents until #10, which left bulk cleanup to
/// `git worktree remove` by hand, and that skips the transcript archive and the
/// recovery record the preflight exists to write.
///
/// The one refusal an agent will meet that a person does not: its own workspace
/// holds a live session, itself. The help says so, because the preflight's
/// wording ("a live session") reads as somebody else's.
pub async fn teardown_from_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
    Json(body): Json<TeardownBody>,
) -> ApiResult<worktree::Preflight> {
    ask_token_ok(&app, id, &headers).await?;
    Ok(Json(worktree::teardown(&app, &body.workspace).await?))
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
    // Off the runtime: `resolve_base` shells out to git, and every diff click and
    // every editor open comes through here.
    let base = {
        let (at, which) = (path.clone(), q.base);
        let (upstream, pr_base) = (app.cfg.upstream_ref.clone(), q.pr_base.clone());
        crate::proc::run_blocking("resolving the diff base", move || {
            crate::diff::resolve_base(&at, which, &upstream, pr_base.as_deref())
        })
        .await??
    };
    Ok((path, base))
}

pub async fn diff_summary(
    State(app): State<Arc<AppState>>,
    Query(q): Query<DiffQuery>,
) -> ApiResult<crate::diff::DiffSummary> {
    let (path, base) = base_for(&app, &q).await?;
    // Off the runtime: a `git diff` over the changeset, per click.
    Ok(Json(
        crate::proc::run_blocking("the diff summary", move || {
            let mut sum = crate::diff::summary(&path, &base)?;
            /* The pane's git verbs are drawn from these two, and this route serves
            the very same rows the rail's list does — so leaving them unset made
            one file offer `stage` in one pane and nothing in the other. One more
            git child per click, on a request that has already run two diffs.
            Degraded rather than fatal: the diff is what was asked for. */
            match crate::git::status(&path, None, crate::git::Untracked::Collapsed) {
                Ok(set) => crate::diff::mark_worktree_state(&mut sum.files, &set),
                Err(e) => tracing::warn!("no git verbs on this diff: {e:#}"),
            }
            Ok::<_, anyhow::Error>(sum)
        })
        .await??,
    ))
}

/// What the search overlay asks for: a workspace, and the query itself.
#[derive(Deserialize)]
pub struct SearchQuery {
    pub workspace: String,
    #[serde(flatten)]
    pub find: crate::search::Query,
}

/// Every matching line in one workspace's working tree.
pub async fn search(
    State(app): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> ApiResult<crate::search::Matches> {
    let (root, exclude) = searchable(&app, &q.workspace).await?;
    // Off the runtime: the walk is blocking and owns a thread pool while it runs.
    Ok(Json(
        crate::proc::run_blocking("the search", move || {
            crate::search::search(&root, exclude.as_deref(), &q.find)
        })
        .await??,
    ))
}

/// What the archive filter asks for: the word, and nothing else.
///
/// No workspace, unlike [`SearchQuery`]: the archive is the checkout's, and a past
/// conversation's worktree may not exist any more.
#[derive(Deserialize)]
pub struct ArchiveQuery {
    pub find: String,
}

/// Which of this checkout's past conversations said this, and what they said.
///
/// **The slow half of the archive filter.** The rail filters the names itself, out
/// of the snapshot it already holds, and asks this for the rest — so a keystroke
/// costs nothing and this answers when it answers.
///
/// Only what people wrote counts, which is [`crate::store::first_spoken_match`]:
/// searching the tool output as well matches nearly every conversation and tells
/// you nothing about any of them.
pub async fn search_archive(
    State(app): State<Arc<AppState>>,
    Query(q): Query<ArchiveQuery>,
) -> ApiResult<serde_json::Value> {
    let needle = q.find.trim().to_ascii_lowercase();
    /* One character matches everything and costs a full read of every transcript in
    the checkout. The page debounces, but the first keystroke of every search
    would still land here, so the refusal is on this side as well. */
    if needle.chars().count() < 2 {
        return Ok(Json(json!({ "hits": [] })));
    }

    /* The archive's two halves, exactly as the rail lists them: the daemon's own
    finished conversations, and the ones it never started. Collected under the
    lock and scanned outside it — the read holds up every other request, and this
    walks megabytes. */
    let files: Vec<(Uuid, std::path::PathBuf)> = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .values()
            .filter(|s| {
                matches!(
                    s.state,
                    crate::model::State::Archived { .. } | crate::model::State::Exited
                )
            })
            .filter_map(|s| {
                let recorded = s
                    .archived_transcript
                    .as_deref()
                    .or(s.transcript_path.as_deref());
                crate::store::transcript_file(s.id, &s.cwd, recorded).map(|p| (s.id, p))
            })
            .chain(inner.external.iter().map(|c| (c.id, c.transcript.clone())))
            .collect()
    };

    // Off the runtime: this is `std::fs` over every file in the list.
    let hits = crate::proc::run_blocking("the archive search", move || {
        files
            .into_iter()
            .filter_map(|(id, path)| {
                crate::store::first_spoken_match(&path, &needle)
                    .map(|line| json!({ "id": id, "line": line }))
            })
            .collect::<Vec<_>>()
    })
    .await?;

    Ok(Json(json!({ "hits": hits })))
}

/// A modifier-click: which file it happened in, and the word under the pointer.
#[derive(Deserialize)]
pub struct DefQuery {
    pub workspace: String,
    /// The file the symbol was clicked in. Its extension is what names the
    /// language — a symbol has none of its own.
    pub path: String,
    pub symbol: String,
}

/// Where a symbol looks like it is defined.
///
/// **Empty is a normal answer**, not an error: an unknown extension, a word that
/// is not an identifier, or a definition the shapes do not describe. The page
/// falls back to an ordinary search for the symbol, which is what was wanted.
pub async fn definitions(
    State(app): State<Arc<AppState>>,
    Query(q): Query<DefQuery>,
) -> ApiResult<crate::search::Matches> {
    let (root, exclude) = searchable(&app, &q.workspace).await?;
    // Off the runtime: the same walk `search` runs.
    Ok(Json(
        crate::proc::run_blocking("looking for a definition", move || {
            crate::symbols::definitions(
                &root,
                exclude.as_deref(),
                std::path::Path::new(&q.path),
                &q.symbol,
            )
        })
        .await??,
    ))
}

#[derive(Deserialize)]
pub struct PathsQuery {
    pub workspace: String,
}

/// Every file in one workspace, for the name search.
///
/// Fetched once per workspace rather than per keystroke — the page does the
/// matching, because a subsequence rank over a list this size is cheaper than a
/// round trip.
pub async fn paths(
    State(app): State<Arc<AppState>>,
    Query(q): Query<PathsQuery>,
) -> ApiResult<crate::search::Paths> {
    let (root, exclude) = searchable(&app, &q.workspace).await?;
    // Off the runtime: a whole-tree walk.
    Ok(Json(
        crate::proc::run_blocking("listing the paths", move || {
            crate::search::paths(&root, exclude.as_deref())
        })
        .await??,
    ))
}

/// Where a workspace's search starts, and what it must not descend into.
///
/// **Main's tree contains every worktree**, so it carries the same exclude
/// `git::status` and the changed-files pane take (§2); a worktree carries none.
/// Both routes ask through here rather than each deciding, because one of them
/// forgetting is a search that quietly answers for other people\'s sessions.
async fn searchable(
    app: &Arc<AppState>,
    workspace: &str,
) -> Result<(std::path::PathBuf, Option<String>), ApiError> {
    let inner = app.inner.read().await;
    let w = inner
        .workspaces
        .get(workspace)
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?;
    let exclude = w.is_main().then(|| app.cfg.worktrees_subdir_str());
    Ok((w.path.clone(), exclude))
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
    // Off the runtime: another `git diff`, per file you open.
    let file = q.path.clone();
    Ok(Json(
        crate::proc::run_blocking("a file diff", move || {
            crate::diff::file_diff(&path, &base, &file, context)
        })
        .await??,
    ))
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

/// Boot milestones the page measured, in milliseconds from its own `timeOrigin`.
///
/// A free-form map rather than named fields, because which milestones exist is
/// the page's business and the daemon's only job is to write them down. Adding
/// one is then a JS change, which is the point: the Rust side has nothing to say
/// about what is worth timing in a webview.
#[derive(Deserialize)]
pub struct ClientTiming {
    pub marks: std::collections::BTreeMap<String, i64>,
}

/// Put the page's own boot timing in the daemon's log.
///
/// The daemon can time everything up to serving the page and sending the first
/// snapshot, and nothing after it. So the half that is missing from a slow-start
/// report is exactly the half that only the page can see: the vendored scripts
/// parsing, the first snapshot rendering, the centre pane's terminal attaching
/// and painting. This is how the two halves end up in one log a colleague can
/// paste back.
///
/// Sorted by the milestone's own timestamp, so the line reads in the order the
/// start actually happened rather than alphabetically.
pub async fn client_timing(
    State(_app): State<Arc<AppState>>,
    Json(body): Json<ClientTiming>,
) -> impl IntoResponse {
    let mut marks: Vec<(String, i64)> = body.marks.into_iter().collect();
    marks.sort_by_key(|(_, ms)| *ms);
    let said: Vec<String> = marks
        .iter()
        .map(|(what, ms)| format!("{what} {ms}ms"))
        .collect();
    tracing::info!("page start: {}", said.join(", "));
    (StatusCode::ACCEPTED, Json(json!({ "logged": true })))
}

#[derive(Deserialize)]
pub struct ClientNote {
    pub note: String,
}

/// One line from the page into the daemon's log.
///
/// **The page's own state is otherwise absent from a bug report.** `orchd.log`
/// records the daemon and nothing else, so "my fonts break" could not say which
/// renderer was active, which engine it was on, or whether the WebGL context had
/// been lost — and on a packaged app there is no console to look in either. That
/// gap is what made the macOS glyph-corruption report (#8) take a screen recording
/// to diagnose.
///
/// Truncated, because it lands in a log the daemon does not control the size of.
/// Token-gated like every non-agent route, so a session cannot write here.
pub async fn client_note(
    State(_app): State<Arc<AppState>>,
    Json(body): Json<ClientNote>,
) -> impl IntoResponse {
    // No control characters: a newline in the body would end this line and start
    // another, in the one file this feature exists to make trustworthy.
    let note: String = body
        .note
        .chars()
        .filter(|c| !c.is_control())
        .take(300)
        .collect();
    tracing::info!("page: {note}");
    (StatusCode::ACCEPTED, Json(json!({ "logged": true })))
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
        refuse!("refusing to open non-http URL");
    }
    // Logged because the report this answers cannot be told apart otherwise: a
    // review row that "does not open" is either a click the page never delivered
    // or an opener that did nothing, and those have different fixes.
    tracing::info!("opening {url} in the browser");
    open_detached(url).await?;
    Ok(Json(json!({ "opened": url })))
}

/// How many URLs one press may open.
///
/// **On the daemon as well as in the page**, because this is the half that spawns
/// processes and the page that sent the body is the one thing it cannot check. The
/// pane asks above eight and never sends more than it is showing; a body naming
/// five hundred reviews is not that pane, and thirty-two browser hand-offs is
/// already more than anybody meant.
const OPEN_ALL_MAX: usize = 32;

#[derive(Deserialize)]
pub struct OpenUrls {
    pub urls: Vec<String>,
}

/// Open several external URLs, one browser hand-off each.
///
/// **One route rather than one call per URL**, because a per-URL round trip has a
/// way to half-fail for every row and no way to say so: the page would tally its
/// own failures and still not know which opener refused. Here the count comes back
/// whole, and the log names each one that did not.
///
/// A refusal is counted rather than fatal. A queue holding one malformed row must
/// still open the other four — the press meant "open what you can", and the answer
/// says how many that was.
pub async fn open_urls(
    State(_app): State<Arc<AppState>>,
    Json(body): Json<OpenUrls>,
) -> ApiResult<serde_json::Value> {
    if body.urls.len() > OPEN_ALL_MAX {
        refuse!(
            "refusing to open {} URLs at once — the cap is {OPEN_ALL_MAX}",
            body.urls.len()
        );
    }
    let mut opened = 0usize;
    for url in &body.urls {
        let url = url.trim();
        // The same rule `open_url` keeps, for the same reason: this can never be
        // coaxed into launching a local file or a `mailto:`/`file:` handler.
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            tracing::warn!("refusing to open a non-http URL");
            continue;
        }
        match open_detached(url).await {
            Ok(()) => opened += 1,
            Err(e) => tracing::warn!("could not open {url}: {e:#}"),
        }
    }
    tracing::info!("opened {opened} of {} in the browser", body.urls.len());
    Ok(Json(json!({ "opened": opened, "asked": body.urls.len() })))
}

#[derive(Deserialize)]
pub struct OpenFile {
    pub workspace: String,
    pub path: String,
}

#[derive(Deserialize)]
pub struct RevealPath {
    pub workspace: String,
    pub path: String,
    /// Hand the machine the *directory* the file is in rather than the file.
    #[serde(default)]
    pub folder: bool,
}

/// Hand a file, or the folder holding it, to whatever this machine opens things
/// with.
///
/// **The containment check is `resolve_in_workspace`'s, not this route's**, and
/// that is the whole reason this is a route rather than a page trick: the client
/// asks about a workspace-relative path and the daemon decides what that means on
/// disk, including the symlink that points out of the tree. A path the SPA read
/// off a terminal is text an agent printed, so it is exactly the input that must
/// not become a way to open `/etc/shadow` with the desktop's default handler.
///
/// `open_detached` is the same opener the browser routes use — `open` on macOS,
/// `xdg-open` and its fallbacks elsewhere — so a folder opens in the machine's
/// file manager and a file opens in whatever is registered for it.
pub async fn reveal_path(
    State(app): State<Arc<AppState>>,
    Json(body): Json<RevealPath>,
) -> ApiResult<serde_json::Value> {
    let Some(root) = app.workspace_path(&body.workspace).await else {
        refuse!("unknown workspace {}", body.workspace);
    };
    let (rel, folder) = (body.path.clone(), body.folder);
    let shared = app.cfg.shared_worktree_paths.clone();
    // Off the runtime: `resolve_in_workspace` canonicalises, which is disk.
    let target = crate::proc::run_blocking("resolving a path to open", move || {
        let file = orchd_base::edit::resolve_in_workspace(&root, &rel, &shared)?;
        let at = if folder {
            file.parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or(file)
        } else {
            file
        };
        anyhow::Ok(at)
    })
    .await??;
    let shown = target.display().to_string();
    open_detached(&shown).await?;
    tracing::info!("handed {shown} to the machine");
    Ok(Json(json!({ "opened": shown })))
}

/// Open one changed file on the forge, in the browser.
///
/// The URL is minted here rather than in the SPA so the client carries no
/// knowledge of a forge's path grammar — see [`crate::forge::Forge::blob_url`].
/// The daemon already owns outward navigation (`/api/open`), so this is the same
/// boundary with the link-building moved behind the seam.
///
/// The ref is always a **sha**, never a branch name, and that is load-bearing:
/// this is a triangular setup where branches are pushed to the fork while PRs
/// (and so `resolve_repo`, and so the URL host) name upstream. A fork's branch
/// does not exist on upstream, so `blob/<branch>/…` 404s for every worktree —
/// verified against a live PR branch. A sha is shared across a fork network, so
/// it resolves under either repo's URL.
///
/// The PR's head sha wins when a PR holds the workspace: it is guaranteed to be
/// pushed, and it reads the file as the review sees it. Otherwise local `HEAD`,
/// which is exactly the commit the changed-files pane measured — and 404s while
/// it is unpushed, which is the honest answer rather than a silently older one.
pub async fn open_file(
    State(app): State<Arc<AppState>>,
    Json(body): Json<OpenFile>,
) -> ApiResult<serde_json::Value> {
    let path = body.path.trim();
    if path.is_empty() {
        refuse!("no path given");
    }
    /* A blob URL is a *tracked file at a ref*, and the pane lists two things that
    are neither: an untracked file (`DiffFile::untracked`, which git has never
    seen, so no ref has a blob for it) and a whole untracked directory, which
    `--untracked-files=normal` collapses to a trailing `/`. Both used to open a
    404 in the browser, which reads as the forge being broken rather than as the
    file not being there.

    Asked of git rather than of the `?` status the client happens to hold: the
    route is reachable without the pane, and "does this ref have this path" is
    the question the URL is about. The directory case is refused before the git
    call, since `ls-tree` on `x/` answers nothing useful either way. */
    if path.ends_with('/') {
        refuse!("{path} is a directory, and a blob URL is for a file");
    }
    let (head_sha, at) = {
        let inner = app.inner.read().await;
        let w = inner
            .workspaces
            .get(&body.workspace)
            .ok_or_else(|| anyhow::anyhow!("no such workspace: {}", body.workspace))?;
        // The PR holding this workspace, matched the way `workspace_for` does in
        // reverse: by head branch.
        let sha = inner
            .prs
            .iter()
            .find(|p| w.branches.contains(&p.head_ref))
            .and_then(|p| p.head_sha.clone());
        (sha, w.path.clone())
    };
    let at2 = at.clone();
    let r#ref = match head_sha {
        Some(sha) => sha,
        None => tokio::task::spawn_blocking(move || crate::git::head_sha(&at))
            .await
            .context("resolving HEAD panicked")?
            .context("could not resolve HEAD for this workspace")?,
    };
    // Off the runtime like every other git call, and *after* the ref is resolved,
    // because the question is about this ref rather than about the working tree.
    let (at2, ref2, p2) = (at2, r#ref.clone(), path.to_string());
    let tracked = tokio::task::spawn_blocking(move || crate::git::has_path_at(&at2, &ref2, &p2))
        .await
        .context("asking git about the path panicked")?;
    if !tracked {
        refuse!(
            "{path} is not in {} — an untracked file has no blob to open",
            &r#ref[..r#ref.len().min(8)]
        );
    }
    let forge = write_forge(&app)?;
    let url = forge.blob_url(&r#ref, path);
    open_detached(&url).await?;
    Ok(Json(json!({ "opened": url })))
}

/// [`open_external`], off the runtime.
///
/// It probes each candidate with `which` and then spawns one, so a single click is
/// up to four child processes plus a `/proc/version` read — all of it fork and exec
/// rather than work this machine can be fast at.
async fn open_detached(url: &str) -> anyhow::Result<()> {
    let url = url.to_string();
    crate::proc::run_blocking("handing the URL to the browser", move || {
        open_external(&url)
    })
    .await?
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
        let child = Command::new(cmd)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning {cmd}"))?;
        // Reaped on a thread. Dropping the `Child` unwaited leaves a zombie for
        // the life of the daemon, and this is a per-click path — `xdg-open` exits
        // as soon as it has handed the URL on, so the thread is short-lived.
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
        return Ok(());
    }
    anyhow::bail!("no browser opener found (tried {})", candidates.join(", "))
}

// ---------------------------------------------------------------------------
// The forge, for whoever asks
//
// The overlay's own routes are `crate::review_api`. These five stayed because the
// thread routes above and `open_file` ask for them too.
// ---------------------------------------------------------------------------

/// The forge the config selects, built for reads: repo + read token. The one
/// spot the four-line resolve/token/build dance lived; now the same for every
/// read endpoint, and dispatched by `cfg.forge`.
pub(crate) fn read_forge(app: &Arc<AppState>) -> Result<crate::forge::ForgeImpl, ApiError> {
    let (owner, name) = repo_of(app)
        .ok_or_else(|| anyhow::anyhow!("no GitHub repo configured and none on the remote"))?;
    let token = crate::forge::resolve_token(app.cfg.github_token_file.as_deref())?;
    Ok(crate::forge::ForgeImpl::for_kind(
        app.cfg.forge,
        owner,
        name,
        token.value,
    ))
}

/// The same, for writes. Writes shell their own tool, so no read token is
/// needed — the forge carries only the repo it writes to.
pub(crate) fn write_forge(app: &Arc<AppState>) -> Result<crate::forge::ForgeImpl, ApiError> {
    let (owner, name) = repo_of(app).context("no GitHub repo configured and none on the remote")?;
    Ok(crate::forge::ForgeImpl::for_kind(
        app.cfg.forge,
        owner,
        name,
        String::new(),
    ))
}

/// The repo the forge talks to, without a child process.
///
/// `resolve_repo` shells `git remote get-url` every time, and the two forge
/// constructors called it on every review action, on the runtime. `AppState::new`
/// already read the same remote into `repos.upstream` at boot, so that is the
/// answer here; the shell-out stays only as the fallback for a boot that could
/// not read the remote. Both are `forge::upstream_repo`, so a configured `repo`
/// and a derived one reach this the same way.
pub(crate) fn repo_of(app: &Arc<AppState>) -> Option<(String, String)> {
    /* `repos.upstream` is `forge::upstream_repo`'s own answer, cached at boot — so
    reading it here is the cheap path *and* the same rule, which it was not when this
    carried its own first arm. The shell-out stays as the fallback for a boot that
    could not read the remote. */
    app.repos
        .upstream
        .as_deref()
        .and_then(|r| r.split_once('/'))
        .map(|(o, n)| (o.to_string(), n.to_string()))
        .or_else(|| crate::resolve_repo(app))
}

/// Fetch a PR's threads and hand back the parsed set.
///
/// Shared by the endpoints that need to know what is awaiting an answer *now*.
/// Always refetches straight from the forge: a stale thread list is the one
/// thing this flow must never act on, which is why nothing here caches.
pub(crate) async fn fetch_threads(
    app: &Arc<AppState>,
    pr: u64,
) -> Result<crate::forge::Threads, ApiError> {
    // `read_forge` shells out as well — `gh auth token` for the credential — so it
    // goes in the *same* hop as the fetch rather than running on a worker just
    // before it. The PR poller wraps its own copy of that call for the same
    // reason.
    let app = app.clone();
    let fetched = crate::proc::run_blocking("the thread fetch", move || {
        let forge = read_forge(&app).map_err(|e| e.0)?;
        forge.threads(pr)
    })
    .await??;
    Ok(fetched)
}

pub(crate) fn pr_from_poll(
    prs: &[crate::forge::Pr],
    number: u64,
) -> Result<crate::forge::Pr, ApiError> {
    prs.iter()
        .find(|p| p.number == number)
        .cloned()
        .ok_or_else(|| ApiError(anyhow::anyhow!("PR #{number} is not in the current poll")))
}

// ---------------------------------------------------------------------------
// fix-pr (§8) — never on the poll's say-so
// ---------------------------------------------------------------------------

/// Start a `fix-pr` run for a PR. The rail's button.
///
/// Still **not** what §8 describes: a run never starts because a PR went red. The
/// difference that rule is protecting is between a tool that helps and one that
/// rebases your branches while you are looking elsewhere, and that turns on whether
/// a *person* set the run in motion — not on which line of code spawns it. Two
/// things now do: this, and a review handing on the CI it is forbidden to fix
/// (`session_handoff`), which you started by sending the decisions and which
/// announces itself in the same rail chip. The guard table is the same either way,
/// because it is the whole of what makes a run safe to start.
pub async fn fix_pr(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let session = crate::fix_pr::start(&app, number).await?;
    Ok(Json(json!({ "session": session })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An install with no installer behind it must refuse, and say what to do.
    ///
    /// The test binary runs out of `target/`, so the install this resolves is
    /// `Checkout` — which is exactly the shape the refusal is for: nothing to ask,
    /// and upgrading anyway would install over some *other* copy of the app and
    /// report success. A cask and a `.deb` take the other branch and are covered in
    /// `update`'s own tests, where the install can be named without lying about the
    /// machine the suite is on.
    #[tokio::test]
    async fn an_install_with_no_installer_cannot_upgrade_itself() {
        use crate::model::{Offer, UpdateInfo};

        let (app, _dir) = crate::testutil::app("selfup");

        // Nothing found yet: nothing to install.
        assert!(upgrade_app(State(app.clone())).await.is_err());

        let mut info = UpdateInfo {
            current: "2026.9.1".into(),
            latest: "2026.9.2".into(),
            url: "https://example.invalid/r".into(),
            tool: None,
            offer: Offer::LinkOnly,
        };
        app.inner.write().await.update = Some(info.clone());
        let err = match upgrade_app(State(app.clone())).await {
            Ok(_) => panic!("an install with no installer must refuse"),
            Err(e) => e,
        };
        let said = format!("{}", err.0);
        assert!(
            said.contains("mise") && said.contains("Homebrew") && said.contains("apt"),
            "the refusal has to name the channels that could: {said}"
        );
        assert!(
            app.inner.read().await.self_upgrade_run.is_none(),
            "a refusal must not leave a run behind for the bar to report"
        );

        // A run in flight refuses the next press, because two `mise upgrade`s of one
        // tool race over the same install directory.
        //
        // Set here rather than by pressing the button: a real press spawns a real
        // `mise upgrade`, and a unit test that reaches the network — or worse,
        // installs something on the machine running it — is not a unit test. The
        // claim itself is three lines above this in the handler and is read there.
        info.tool = Some("github:kbarendrecht/orchestrator".into());
        app.inner.write().await.update = Some(info);
        app.inner.write().await.self_upgrade_run = Some(crate::model::UpgradeRun {
            to: "2026.9.2".into(),
            running: true,
            tail: String::new(),
        });
        assert!(
            upgrade_app(State(app.clone())).await.is_err(),
            "one run at a time"
        );
    }

    /// The route's own rules, which the pane cannot be trusted to keep.
    ///
    /// A snapshot is a moment old by the time you click it, and the route is
    /// reachable without the pane at all — so the path, the verb and "is anybody
    /// working in there" are all asked here rather than inferred from what the
    /// client sent.
    #[tokio::test]
    async fn a_file_verb_stays_in_its_workspace_and_off_a_working_tree() {
        use crate::model::{Session, State as S, MAIN};

        let (app, dir) = crate::testutil::app("fileverb-api");
        crate::testutil::git(&dir, &["init", "-q", "-b", "main"]);
        crate::testutil::git(&dir, &["config", "user.email", "t@t"]);
        crate::testutil::git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "committed\n").unwrap();
        crate::testutil::git(&dir, &["add", "-A"]);
        crate::testutil::git(&dir, &["commit", "-qm", "base"]);
        std::fs::write(dir.join("f.txt"), "edited\n").unwrap();

        let go = |path: &str, verb: &str| {
            let (app, path, verb) = (app.clone(), path.to_string(), verb.to_string());
            async move {
                file_verb(
                    State(app),
                    Json(FileVerbBody {
                        workspace: MAIN.to_string(),
                        path,
                        verb,
                    }),
                )
                .await
            }
        };
        let said = |e: ApiError| format!("{:#}", e.0);

        // Out of the workspace, both spellings. `edit::resolve_in_workspace` is the
        // one rule, and this is the route that would otherwise hand git a path
        // somebody else's tree.
        assert!(go("../elsewhere/f.txt", "stage").await.is_err());
        assert!(go("/etc/passwd", "stage").await.is_err());
        // Not a verb at all.
        let e = said(go("f.txt", "delete").await.expect_err("not a verb"));
        assert!(e.contains("not a file verb"), "{e}");

        // Nothing to do is refused rather than silently succeeding: a button that
        // does nothing reads as broken.
        let e = said(go("f.txt", "unstage").await.expect_err("nothing staged"));
        assert!(e.contains("nothing to unstage"), "{e}");

        // The happy path, and then the guard that only this daemon needs.
        assert!(go("f.txt", "stage").await.is_ok());
        {
            let mut inner = app.inner.write().await;
            let id = Uuid::new_v4();
            let mut s = Session::new(id, MAIN.to_string(), dir.clone(), None);
            s.set_state(S::Working);
            inner.sessions.insert(id, s);
        }
        let e = said(
            go("f.txt", "unstage")
                .await
                .expect_err("an agent is working"),
        );
        assert!(e.contains("mid-turn"), "{e}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The drawer may hand a session text only when a keystroke means "a prompt".
    ///
    /// Every refusal here is a state where `Enter` means something else — a submit
    /// mid-turn, consent to a permission prompt, an answer to a question — so this
    /// walks them rather than testing the happy path alone. Driven against a real
    /// pty, because `type_and_send` is what the guards are protecting.
    #[tokio::test(flavor = "multi_thread")]
    async fn text_reaches_a_session_at_its_prompt_and_no_other_state() {
        use crate::model::{Session, State as S, TurnReason as R, MAIN};
        use crate::pty::PtyHandle;
        use std::time::SystemTime;

        let (app, dir) = crate::testutil::app("tell");
        let id = Uuid::new_v4();
        // `cat` echoes, so the pty is both alive and readable — the happy path can
        // assert the text actually arrived rather than that nothing complained.
        let spawned = PtyHandle::spawn(&["cat".to_string()], &dir, &[], &[], (24, 80))
            .expect("a pty to type into");
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), dir.clone(), None);
            s.pty = Some(spawned.handle.clone());
            s.had_a_turn = true;
            inner.sessions.insert(id, s);
        }
        let set = |st: S| {
            let app = app.clone();
            async move { app.with_session(id, |s| s.set_state(st)).await }
        };
        let tell = |text: &str| {
            let (app, text) = (app.clone(), text.to_string());
            async move { tell_session(State(app), Path(id), Json(TellBody { text })).await }
        };

        // Mid-turn: a stray line of input, and `Enter` submits whatever is typed.
        set(S::Working).await;
        let said = |e: ApiError| format!("{:#}", e.0);
        let e = said(
            tell("ng-watch said: TS2345")
                .await
                .expect_err("mid-turn is refused"),
        );
        assert!(e.contains("mid-turn"), "{e}");

        // Both of these read a keystroke as an *answer*.
        set(S::YourTurn {
            since: SystemTime::now(),
            reason: R::NeedsPermission,
        })
        .await;
        assert!(
            tell("x").await.is_err(),
            "a permission prompt takes it as consent"
        );
        set(S::YourTurn {
            since: SystemTime::now(),
            reason: R::AskedAQuestion,
        })
        .await;
        assert!(
            tell("x").await.is_err(),
            "a question takes it as the highlighted choice"
        );

        // Size, which is the other half of "a prompt is not a log".
        set(S::YourTurn {
            since: SystemTime::now(),
            reason: R::TurnComplete,
        })
        .await;
        assert!(
            tell(&"x".repeat(9 * 1024)).await.is_err(),
            "a buffer-sized paste is refused"
        );
        assert!(tell("   ").await.is_err(), "and so is nothing at all");

        // At its prompt: it goes, and `cat` hands it back.
        assert!(
            tell("ng-watch said: TS2345").await.is_ok(),
            "a session at its prompt takes it",
        );
        let seen = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let out = String::from_utf8_lossy(&spawned.handle.snapshot()).into_owned();
                if out.contains("TS2345") {
                    return out;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(seen.is_ok(), "the text never reached the pty");

        // A session with no pty is a resume, not a target.
        app.with_session(id, |s| s.pty = None).await;
        let e = said(tell("x").await.expect_err("nothing to type into"));
        assert!(e.contains("resume it first"), "{e}");

        let _ = spawned.handle.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole point of the channel: the agent's poll is released by the answer
    /// rather than by a timeout, and it comes back carrying the choice.
    #[tokio::test]
    async fn an_answer_releases_the_poll_the_agent_is_sitting_in() {
        use crate::model::{Interaction, InteractionOption, Session, MAIN};

        let (app, dir) = crate::testutil::app("ask");

        let id = Uuid::new_v4();
        let ask_id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut sess = Session::new(id, MAIN.to_string(), dir.clone(), None);
            sess.interaction = Some(Interaction {
                id: ask_id,
                thread_id: None,
                question: "rebase or stop?".into(),
                detail: None,
                options: vec![InteractionOption {
                    value: "rebase".into(),
                    label: "Rebase".into(),
                    sub: String::new(),
                    free: false,
                }],
                asked_at: std::time::SystemTime::now(),
                answer: None,
                answer_text: None,
            });
            inner.sessions.insert(id, sess);
        }

        let mut agent = axum::http::HeaderMap::new();
        let token = app.inner.read().await.sessions[&id].ask_token.clone();
        agent.insert("x-orch-ask", token.parse().unwrap());

        // Another session's agent, or any local process, must not be able to read
        // this one's answer.
        let mut wrong = axum::http::HeaderMap::new();
        wrong.insert("x-orch-ask", "not-the-token".parse().unwrap());
        assert!(
            ask_wait(State(app.clone()), Path((id, ask_id)), wrong)
                .await
                .is_err(),
            "a wrong ask token was let through"
        );

        // The agent is already waiting when the answer arrives, which is the
        // ordering that matters: a poll that started first must still be woken.
        let waiter = tokio::spawn({
            let app = app.clone();
            async move { ask_wait(State(app), Path((id, ask_id)), agent).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = answer(
            State(app.clone()),
            Path(id),
            Json(AnswerBody {
                ask: ask_id,
                answer: "rebase".into(),
                text: None,
            }),
        )
        .await
        .map_err(|e| format!("{}", e.0))
        .expect("answer accepted");

        let got = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("the poll was never released")
            .unwrap()
            .map_err(|e| format!("{}", e.0))
            .expect("wait succeeded");
        assert_eq!(got.0["answered"], true);
        assert_eq!(got.0["answer"], "rebase");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The escape hatch: an option that asks for words is not answered by picking
    /// it, and the words travel beside the value rather than as it.
    #[tokio::test]
    async fn the_option_that_asks_for_words_is_not_answered_without_them() {
        use crate::model::{Interaction, InteractionOption, Session, MAIN};

        let (app, dir) = crate::testutil::app("ask3");
        let id = Uuid::new_v4();
        let ask_id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut sess = Session::new(id, MAIN.to_string(), dir.clone(), None);
            sess.interaction = Some(Interaction {
                id: ask_id,
                thread_id: None,
                question: "how should it be documented?".into(),
                detail: None,
                options: vec![InteractionOption {
                    value: "mine".into(),
                    label: "Let me write it…".into(),
                    sub: String::new(),
                    free: true,
                }],
                asked_at: std::time::SystemTime::now(),
                answer: None,
                answer_text: None,
            });
            inner.sessions.insert(id, sess);
        }

        let err = answer(
            State(app.clone()),
            Path(id),
            Json(AnswerBody {
                ask: ask_id,
                answer: "mine".into(),
                text: None,
            }),
        )
        .await
        .expect_err("refused with no words");
        assert!(format!("{}", err.0).contains("none were written"));

        let _ = answer(
            State(app.clone()),
            Path(id),
            Json(AnswerBody {
                ask: ask_id,
                answer: "mine".into(),
                text: Some("put it under Pushing, but say why".into()),
            }),
        )
        .await
        .map_err(|e| format!("{}", e.0))
        .expect("accepted with words");

        let inner = app.inner.read().await;
        let got = inner.sessions[&id].interaction.as_ref().unwrap();
        assert_eq!(got.answer.as_deref(), Some("mine"));
        assert_eq!(
            got.answer_text.as_deref(),
            Some("put it under Pushing, but say why")
        );
        drop(inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An answer nobody offered would reach the agent as an instruction no branch
    /// was written for.
    #[tokio::test]
    async fn an_answer_that_was_not_offered_is_refused() {
        use crate::model::{Interaction, InteractionOption, Session, MAIN};

        let (app, dir) = crate::testutil::app("ask2");
        let id = Uuid::new_v4();
        let ask_id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut sess = Session::new(id, MAIN.to_string(), dir.clone(), None);
            sess.interaction = Some(Interaction {
                id: ask_id,
                thread_id: None,
                question: "rebase or stop?".into(),
                detail: None,
                options: vec![InteractionOption {
                    value: "rebase".into(),
                    label: "Rebase".into(),
                    sub: String::new(),
                    free: false,
                }],
                asked_at: std::time::SystemTime::now(),
                answer: None,
                answer_text: None,
            });
            inner.sessions.insert(id, sess);
        }
        let err = answer(
            State(app.clone()),
            Path(id),
            Json(AnswerBody {
                ask: ask_id,
                answer: "force-push".into(),
                text: None,
            }),
        )
        .await
        .expect_err("refused");
        assert!(format!("{}", err.0).contains("not one of the options"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_spas_own_origin_is_accepted() {
        assert!(origin_allowed("http://127.0.0.1:7777", 7777, None));
        assert!(origin_allowed("http://localhost:7777", 7777, None));
        assert!(!origin_allowed("http://evil.example", 7777, None));
        // A page on another port is still another origin.
        assert!(!origin_allowed("http://127.0.0.1:7778", 7777, None));
        // Guards against a DNS-rebinding host that merely contains the address.
        assert!(!origin_allowed(
            "http://127.0.0.1.evil.example:7777",
            7777,
            None
        ));
    }

    /// `(origin, is_hook, is_get, token_ok)` at port 7777.
    fn ok(origin: Option<&str>, is_hook: bool, is_get: bool, token_ok: bool) -> bool {
        origin_ok(origin, 7777, None, is_hook, is_get, token_ok)
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
        // `skills/triage/SKILL.md` POSTs with curl, which sends no Origin. Without
        // this arm the one route an agent calls answered 403 to its only caller.
        assert!(ok(None, false, false, true));
        // Still nothing without the token.
        assert!(!ok(None, false, false, false));
    }

    /// A stray escape is not harmless, so the states that would misread it are
    /// refused by name.
    ///
    /// The two waiting ones are the point: mid-turn was driven against a real
    /// session and refused, but a question and a permission prompt cannot be
    /// arranged on demand, and those are exactly the two where the keystroke would
    /// *answer* — cancelling the one, declining the other — rather than do nothing.
    #[tokio::test]
    async fn rewind_refuses_every_state_that_would_read_an_escape_as_an_answer() {
        use crate::model::{Session, State as S, TurnReason as R, MAIN};

        let (app, dir) = crate::testutil::app("rewind");

        let at = |reason| S::YourTurn {
            since: std::time::SystemTime::now(),
            reason,
        };
        for (state, want) in [
            (at(R::AskedAQuestion), "cancel the question"),
            (at(R::NeedsPermission), "decline it"),
            (S::Working, "mid-turn"),
            (S::Starting, "still starting"),
        ] {
            let id = Uuid::new_v4();
            {
                let mut inner = app.inner.write().await;
                let mut s = Session::new(id, MAIN.to_string(), dir.clone(), None);
                s.had_a_turn = true;
                s.state = state.clone();
                inner.sessions.insert(id, s);
            }
            match rewind_session(State(app.clone()), Path(id)).await {
                Err(e) => {
                    let said = format!("{:#}", e.0);
                    assert!(
                        said.contains(want),
                        "{state:?} said {said:?}, wanted {want:?}"
                    );
                }
                Ok(_) => panic!("{state:?} must not open the picker"),
            }
        }

        // And a session at the prompt with nothing behind it: the picker would
        // open on an empty conversation, which reads as a broken button.
        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), dir.clone(), None);
            s.state = at(R::TurnComplete);
            inner.sessions.insert(id, s); // had_a_turn stays false
        }
        let said = format!(
            "{:#}",
            rewind_session(State(app.clone()), Path(id))
                .await
                .expect_err("no conversation must refuse")
                .0
        );
        assert!(said.contains("no conversation to rewind"), "{said}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole of what makes `orch kill` safe to put on the ask token: it reaches
    /// the caller's own spawns and nothing else. Only the refusal is driven here —
    /// it answers before anything is mutated, whereas the accepting path forgets a
    /// record and tears a worktree down, which is what the e2e flows are for.
    #[tokio::test]
    async fn discard_reaches_only_the_sessions_the_caller_spawned() {
        use crate::model::{Session, MAIN};

        let (app, dir) = crate::testutil::app("discard");

        let caller = Uuid::new_v4();
        let mine = Uuid::new_v4();
        let someone_elses = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            for id in [caller, mine, someone_elses] {
                let s = Session::new(id, MAIN.to_string(), dir.clone(), None);
                inner.sessions.insert(id, s);
            }
            // Spawned by a third session, not by the caller — the shape an agent
            // reaches by misreading a uuid out of `orch ls`.
            inner.sessions.get_mut(&someone_elses).unwrap().spawned_by = Some(Uuid::new_v4());
            inner.sessions.get_mut(&mine).unwrap().spawned_by = Some(caller);
        }

        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-orch-token", "t".parse().unwrap());

        for (child, want) in [
            (someone_elses, "is not a session you spawned"),
            // A session nobody spawned — every one you started yourself, so the
            // conversation you are sitting in is refused by the same rule.
            (caller, "is not a session you spawned"),
            (Uuid::new_v4(), "no such session"),
        ] {
            let said = format!(
                "{:#}",
                discard_spawned(State(app.clone()), Path((caller, child)), headers.clone())
                    .await
                    .expect_err("must refuse")
                    .0
            );
            assert!(
                said.contains(want),
                "{child} said {said:?}, wanted {want:?}"
            );
        }

        // And the one that is the caller's own is not refused on authorship. Not
        // carried further here: the next step writes records and removes a tree.
        let inner = app.inner.read().await;
        assert_eq!(inner.sessions[&mine].spawned_by, Some(caller));
        drop(inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_route_the_vendored_prompts_call_on_the_ask_token_is_exempt() {
        // The literal paths the vendored prompts curl.
        for p in [
            "/api/session/<id>/ask",
            "/api/session/<id>/ask/<ask>/wait",
            // A review session posting one thread's reply.
            "/api/session/<id>/thread/PRRT_x/reply",
            "/api/session/<id>/spawn",
            // `orch run`. Named processes only, so this exemption widens what an
            // agent can start without widening it to arbitrary commands (§12).
            "/api/session/<id>/process",
            // `orch kill`. Only the caller's own spawns, which `discard_spawned`
            // enforces from the record — the exemption is what lets it be called at
            // all, not what decides which sessions it reaches.
            "/api/session/<id>/spawned/<child>/discard",
            // `orch teardown`. Any worktree, through the ordinary preflight.
            "/api/session/<id>/teardown",
            // The end of phase 3. Which reviewers are asked is derived from a
            // fresh fetch, so the agent supplies nothing but the call.
            "/api/session/<id>/rerequest",
            // Phase 4 of `skills/review/SKILL.md`: the review saying it is done.
            "/api/session/<id>/handoff",
            // `orch outside`, and the push guard's read of what it granted. The
            // guard is a `command` hook: it has the session's ask token and no
            // app token, so a missing entry here would make the grant unreadable
            // and the refusal permanent.
            "/api/session/<id>/outside",
        ] {
            assert!(is_ask_route(p), "{p} must not need the app token");
        }
        // And nothing else on the session: these are the SPA's, on the app token.
        for p in [
            "/api/session/<id>/kill",
            "/api/session/<id>/answer",
            "/api/session/<id>/fork",
            // The unrestricted delete the rail's own button uses. `/discard` above
            // exists *because* this one must stay out of the agent's reach.
            "/api/session/<id>/delete",
            "/api/state",
            // The drawer's own button, which restarts a *running* process. Named
            // the same thing, deliberately not the agent's.
            "/api/workspace/main/process/docker/restart",
            // The rail's teardown. Same verb as `orch teardown`, which is only safe
            // because this one is outside the `/api/session/` prefix.
            "/api/workspace/pr-1/teardown",
        ] {
            assert!(!is_ask_route(p), "{p} is not the agent's to call");
        }
    }

    /// The guard consults [`is_agent_route`], not [`is_ask_route`]. A route the
    /// prompts really curl that is missing from it is refused twice over — `bad
    /// origin` first, because the agent's curl carries none, and then for want of
    /// an app token it is deliberately not given. That is how `…/committed` shipped
    /// unreachable by its only caller.
    #[test]
    fn the_proposals_post_is_an_agent_route_and_reachable_without_an_origin() {
        let p = "/api/pr/10001/proposals";
        assert!(is_proposals_route(p));
        assert!(
            is_agent_route(p),
            "{p} is curled by the triage and review skills"
        );
        // Not an *ask* route: it is keyed on a PR, and has no session to check.
        assert!(!is_ask_route(p));
        // The Origin allowance the agent's curl depends on.
        assert!(
            ok(None, true, false, false),
            "no Origin must pass for an agent route"
        );
        // Neighbours that stay the SPA's, on the app token.
        for other in [
            "/api/pr/10001/review",
            "/api/pr/10001/fix-pr",
            "/api/pr/10001",
        ] {
            assert!(!is_agent_route(other), "{other} is not the agent's to call");
        }
    }

    /// The two the vendored `triage` skill calls before it can propose anything.
    ///
    /// Same trap as the proposals route and the same reason for a test: the skill
    /// is the only caller, it curls with no Origin and the run credential, and a
    /// route missing from `is_agent_route` is refused twice over without either
    /// refusal naming the cause.
    #[test]
    fn the_review_skill_can_reach_the_route_it_opens_with() {
        let p = "/api/pr/10001/triage-context";
        assert!(is_triage_route(p));
        assert!(is_agent_route(p), "{p} is curled by skills/review/SKILL.md");
        // Keyed on a PR, so not an ask route: there is no session in the path.
        assert!(!is_ask_route(p));
        // The run that *starts* a review session is the SPA's, on the app token.
        assert!(!is_agent_route("/api/pr/10001/review-session"));
    }

    #[tokio::test]
    async fn a_run_posts_proposals_on_a_token_that_opens_nothing_else() {
        let (app, dir) = crate::testutil::app("proposaltok");
        let pr = 10001u64;
        let narrow = crate::secret::random_token();
        app.inner
            .write()
            .await
            .proposal_tokens
            .insert(pr, narrow.clone());

        let hdr = |v: &str| {
            let mut h = axum::http::HeaderMap::new();
            h.insert("x-orch-token", v.parse().unwrap());
            h
        };

        // The run's own credential works for its own PR.
        assert!(proposal_token_ok(&app, pr, &hdr(&narrow)).await.is_ok());
        // The app token still works, so the SPA and these tests can drive it.
        assert!(proposal_token_ok(&app, pr, &hdr(&app.token)).await.is_ok());
        // A wrong one, an empty one, and no header at all are all refused.
        assert!(proposal_token_ok(&app, pr, &hdr("nope")).await.is_err());
        assert!(proposal_token_ok(&app, pr, &hdr("")).await.is_err());
        assert!(proposal_token_ok(&app, pr, &axum::http::HeaderMap::new())
            .await
            .is_err());
        // Scoped to the PR it was minted for: the same token is nothing on another.
        assert!(proposal_token_ok(&app, 999, &hdr(&narrow)).await.is_err());
        // And a PR with no run recorded authenticates nobody but the app.
        assert!(proposal_token_ok(&app, 999, &hdr(&app.token)).await.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
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

    /// The daemon says the facts; the repo says what its own checkouts mean. A note
    /// is attached to the destination it was written about and nowhere else, which
    /// is what keeps "the dev stack only runs in main" out of orchd.
    #[test]
    fn a_project_note_reaches_only_the_workspace_kind_it_was_written_for() {
        let notes = crate::config::WorkspaceNotes {
            main: Some("the stack runs here".into()),
            worktree: None,
        };
        assert_eq!(notes.for_main(true), Some("the stack runs here"));
        assert_eq!(notes.for_main(false), None);
        assert_eq!(
            crate::config::WorkspaceNotes::default().for_main(true),
            None
        );
    }
}
