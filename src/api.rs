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

/// The routes a running session calls on its own `ask_token`.
///
/// A list rather than a growing chain of `ends_with`, because it has been
/// outgrown once already — see the note in [`guard`].
fn is_ask_route(path: &str) -> bool {
    const ASK_ROUTES: [&str; 5] = ["/ask", "/wait", "/spawn", "/committed", "/stuck"];
    path.starts_with("/api/session/") && ASK_ROUTES.iter().any(|s| path.ends_with(s))
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
    let is_ask = is_ask_route(&path);

    let is_get = req.method() == axum::http::Method::GET;
    if !origin_ok(origin, port, is_hook || is_ask, is_get, token_ok) {
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
    // a suffix nobody notices they have to extend. `/threads` itself is gone now
    // (nothing called it); a dead entry here would read as a route that exists.
    const SPENDS_GITHUB_TOKEN: [&str; 1] = ["/review"];
    let spends_github_token =
        path.starts_with("/api/pr/") && SPENDS_GITHUB_TOKEN.iter().any(|s| path.ends_with(s));
    let needs_token =
        !is_hook && !is_ask && (req.method() != axum::http::Method::GET || spends_github_token);
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
// Editable settings
// ---------------------------------------------------------------------------

/// The six settings the panel edits, as the running daemon has them.
pub async fn get_config(State(app): State<Arc<AppState>>) -> ApiResult<crate::config::Settings> {
    Ok(Json(crate::config::Settings::of(&app.cfg)))
}

/// Persist edited settings to `config.json`. Validation is serde's — a bad
/// tracker or a malformed process rejects the whole POST. Applied on the next
/// start; the running config is not mutated, so the panel says "restart to apply".
pub async fn set_config(
    State(_app): State<Arc<AppState>>,
    Json(body): Json<crate::config::Settings>,
) -> ApiResult<serde_json::Value> {
    body.write()?;
    Ok(Json(json!({ "ok": true, "restart_required": true })))
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
async fn refuse_if_occupied(app: &Arc<AppState>, workspace: &str) -> Result<(), ApiError> {
    let Some(held) = app.live_sessions_in(workspace).await.into_iter().next() else {
        return Ok(());
    };
    // Name it the way the rail does, so the message points at a row you can find.
    let who = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .get(&held)
            .and_then(|s| s.title.clone())
            .unwrap_or_else(|| held.to_string()[..8].to_string())
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
            h.kill()?;
            Ok(Json(json!({ "killed": id })))
        }
        None => Err(ApiError(anyhow::anyhow!("no such session {id}"))),
    }
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
            .ok_or_else(|| anyhow::anyhow!("no such session {id}"))?;
        // Nothing to rewind to. The picker opens on an empty conversation and has
        // nothing to offer, which reads as a broken button.
        if !s.had_a_turn {
            return Err(ApiError(anyhow::anyhow!(
                "this session has no conversation to rewind"
            )));
        }
        match &s.state {
            crate::model::State::YourTurn { reason, .. } => match reason {
                crate::model::TurnReason::AskedAQuestion => {
                    return Err(ApiError(anyhow::anyhow!(
                        "it is asking you something — an escape would cancel the question, not rewind"
                    )))
                }
                crate::model::TurnReason::NeedsPermission => {
                    return Err(ApiError(anyhow::anyhow!(
                        "it is waiting for permission — an escape would decline it, not rewind"
                    )))
                }
                _ => {}
            },
            other => {
                return Err(ApiError(anyhow::anyhow!(
                    "the picker only opens at the prompt, and this session is {}",
                    match other {
                        crate::model::State::Working => "mid-turn",
                        crate::model::State::Starting => "still starting",
                        _ => "not live",
                    }
                )))
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
    tokio::spawn(async move {
        let _ = pty.write(b"\x1b");
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let _ = pty.write(b"\x1b");
    });
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
/// Its own exit watcher still runs and still releases the locks and the
/// automation slot, because it holds the pty handle rather than looking the
/// session up again.
///
/// Claude Code's own transcript under `~/.claude/projects` is deliberately left
/// alone. It is not the daemon's file, and `claude --resume` outside orchd still
/// reads it.
pub async fn delete_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<serde_json::Value> {
    let (pty, archived) = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("no such session {id}"))?;
        (s.pty.clone(), s.archived_transcript.clone())
    };

    if let Some(h) = pty {
        // Best effort: a process that is already gone is not a reason to keep
        // the row you asked to be rid of.
        let _ = h.kill();
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
    Ok(Json(json!({ "deleted": id })))
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
        return Err(ApiError(anyhow::anyhow!("a question with no words")));
    }
    if body.options.is_empty() {
        return Err(ApiError(anyhow::anyhow!(
            "a question with no options: the overlay renders buttons, not a prompt"
        )));
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
            .ok_or_else(|| anyhow::anyhow!("no such session {id}"))?;
        if let Some(open) = &s.interaction {
            if open.answer.is_none() {
                return Err(ApiError(anyhow::anyhow!(
                    "session {id} is already asking something else"
                )));
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
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(WAIT_SECS);
    loop {
        {
            let inner = app.inner.read().await;
            let s = inner
                .sessions
                .get(&id)
                .ok_or_else(|| anyhow::anyhow!("no such session {id}"))?;
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
                    return Err(ApiError(anyhow::anyhow!(
                        "question {ask_id} is no longer open on session {id}"
                    )))
                }
            }
        }
        tokio::select! {
            _ = app.answered.notified() => {}
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
async fn ask_token_ok(app: &Arc<AppState>, id: Uuid, headers: &axum::http::HeaderMap) -> Result<(), ApiError> {
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
        .ok_or_else(|| ApiError(anyhow::anyhow!("no such session {id}")))?;
    if given.is_empty() || given != s.ask_token {
        return Err(ApiError(anyhow::anyhow!("bad ask token for session {id}")));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct AnswerBody {
    pub ask: Uuid,
    pub answer: String,
    /// Required by an option that asked for words, refused by one that did not.
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Deserialize)]
pub struct CommittedBody {
    /// The commit the session just made for this thread.
    pub sha: String,
}

#[derive(Deserialize)]
pub struct StuckBody {
    /// What stopped it, in the session's own words. Shown verbatim in the
    /// overview, so it is the whole of what you get to act on.
    pub note: String,
}

/// The session reports a thread it could not finish.
///
/// The counterpart to [`thread_committed`], and the reason `NeedsYou` existed as
/// a state nothing could reach: a run had exactly one way to report progress —
/// a commit — so a thread it gave up on stayed `Pending` and read as one it had
/// not got to yet. An honest overview needs the difference, and only the session
/// knows it.
///
/// Does not block and posts nothing: there is no commit to show and no reply that
/// could truthfully go out. The thread stays open on GitHub, which is what
/// "needs you" means.
pub async fn thread_stuck(
    State(app): State<Arc<AppState>>,
    Path((id, thread_id)): Path<(Uuid, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<StuckBody>,
) -> ApiResult<serde_json::Value> {
    ask_token_ok(&app, id, &headers).await?;
    let note = body.note.trim();
    if note.is_empty() {
        // A bare "could not do it" is worse than silence: it removes the thread
        // from the list of things still moving and says nothing about why.
        return Err(ApiError(anyhow::anyhow!(
            "say what stopped it — the note is all the overview can show"
        )));
    }
    let number = {
        let inner = app.inner.read().await;
        let (number, run) = inner
            .resolve_runs
            .iter()
            .find(|(_, r)| r.session == id)
            .ok_or_else(|| anyhow::anyhow!("session {id} is not carrying out a resolve run"))?;
        if !run.plan.threads.iter().any(|t| t.thread_id == thread_id) {
            return Err(ApiError(anyhow::anyhow!(
                "thread {thread_id} is not in this run's plan"
            )));
        }
        *number
    };
    mark_thread(&app, number, &thread_id, |t| {
        t.status = crate::post::ThreadStatus::NeedsYou;
        t.note = Some(note.to_string());
    })
    .await;
    app.notify().await;
    Ok(Json(json!({ "recorded": true })))
}

/// The session reports a thread's work is committed; the daemon shows it and,
/// with your say-so, posts the reply.
///
/// This is the seam the whole design turns on. The agent wrote the code and knows
/// nothing about GitHub credentials; the daemon holds them and has not read the
/// code. Neither can answer a reviewer alone, and that is deliberate — the reply
/// only goes out attached to a change you have just looked at.
///
/// Blocks like `ask` does, and for the same reason: the session must not run on to
/// the next thread while this one's reply is still a question.
pub async fn thread_committed(
    State(app): State<Arc<AppState>>,
    Path((id, thread_id)): Path<(Uuid, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CommittedBody>,
) -> ApiResult<serde_json::Value> {
    ask_token_ok(&app, id, &headers).await?;

    let (number, planned, cwd, base_sha) = {
        let inner = app.inner.read().await;
        let (number, run) = inner
            .resolve_runs
            .iter()
            .find(|(_, r)| r.session == id)
            .ok_or_else(|| anyhow::anyhow!("session {id} is not carrying out a resolve run"))?;
        let planned = run
            .plan
            .threads
            .iter()
            .find(|t| t.thread_id == thread_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("thread {thread_id} is not in this run's plan"))?;
        let cwd = inner
            .sessions
            .get(&id)
            .map(|s| s.cwd.clone())
            .ok_or_else(|| anyhow::anyhow!("no such session {id}"))?;
        (*number, planned, cwd, run.plan.base_sha.clone())
    };

    // The real diff, not the one triage staged: what the reviewer is about to be
    // told happened is what actually landed, including whatever the agent had to
    // change to make it apply.
    //
    // Taken with the ancestry check below, because both ask git about this worktree
    // and both must describe the same moment.
    let (sha, dir, base) = (body.sha.clone(), cwd.clone(), base_sha.clone());
    let (diff, still_ours) = tokio::task::spawn_blocking(move || {
        (
            crate::git::commit_diff(&dir, &sha, crate::proposal::MAX_FIELD),
            crate::git::is_ancestor(&dir, &base, "HEAD"),
        )
    })
    .await
    .context("reading the commit panicked")?;
    let diff = diff?;

    // Is the tree this run was triaged against still in our history?
    //
    // Per thread, and *not* `base_sha == HEAD`: the agent commits once per thread,
    // so from the second thread on the head has moved by design — which is why the
    // prompt checks equality only at the start. What must stay true for the whole
    // run is ancestry. It stops being true when the branch is rewritten underneath
    // the run: a push from another machine, or somebody force-pushing your branch.
    //
    // The harm is outward, which is why it is checked here rather than left to the
    // push. The agent's commits are then on an orphaned history, and every reply
    // after that would tell a reviewer about a fix that cannot land — the final
    // `--force-with-lease` would refuse, but only after N public comments had
    // already claimed the work. So the reply is held and the thread says why.
    if !still_ours {
        let note = format!(
            "the branch was rewritten under this run ({} is no longer in its history) — \
             the commit stands but nothing was posted",
            &base_sha[..base_sha.len().min(7)]
        );
        mark_thread(&app, number, &thread_id, |t| {
            t.commit = Some(body.sha.clone());
            t.status = crate::post::ThreadStatus::NeedsYou;
            t.note = Some(note.clone());
        })
        .await;
        app.notify().await;
        return Ok(Json(json!({ "posted": false, "reason": note })));
    }

    mark_thread(&app, number, &thread_id, |t| {
        t.commit = Some(body.sha.clone());
        t.status = crate::post::ThreadStatus::Committed;
    })
    .await;

    let Some(reply) = planned.reply.clone() else {
        // No words: the stance was a bare thumbs up. This used to return saying it
        // was "posted with the rest" — nothing posted it, and the run has no
        // "rest", so the reaction never left. Send it here, without a card: you
        // approved the stance at triage and there is no diff to weigh it against.
        if planned.stance.gives_thumbs_up() {
            // Its own fetch: the reaction needs a comment id from now, and the one
            // the reply path uses is taken after the confirmation this case skips.
            let fresh = fetch_threads(&app, number).await?;
            let forge = write_forge(&app)?;
            crate::post::react_one(&forge, &app.cfg.main_checkout, &thread_id, &fresh).await?;
        }
        mark_thread(&app, number, &thread_id, |t| {
            t.status = crate::post::ThreadStatus::Replied;
        })
        .await;
        app.notify().await;
        return Ok(Json(json!({ "posted": false, "reacted": planned.stance.gives_thumbs_up() })));
    };

    let ask_id = Uuid::new_v4();
    {
        let mut inner = app.inner.write().await;
        let s = inner
            .sessions
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("no such session {id}"))?;
        s.interaction = Some(crate::model::Interaction {
            id: ask_id,
            thread_id: Some(planned.location.clone()),
            question: format!("Post this reply on {}?", planned.location),
            detail: Some(format!("{diff}\n--- the reply ---\n{reply}")),
            options: vec![
                crate::model::InteractionOption {
                    value: "post".into(),
                    label: "Post it".into(),
                    sub: "the change is on the branch; the reviewer is told".into(),
                    free: false,
                },
                crate::model::InteractionOption {
                    value: "hold".into(),
                    label: "Hold it back".into(),
                    sub: "keep the commit, say nothing — you answer this one yourself".into(),
                    free: false,
                },
            ],
            asked_at: std::time::SystemTime::now(),
            answer: None,
            answer_text: None,
        });
        s.set_state(crate::model::State::YourTurn {
            since: std::time::SystemTime::now(),
            reason: crate::model::TurnReason::AskedAQuestion,
        });
    }
    app.notify().await;

    // Wait it out. No deadline: unlike `ask`, the caller here is the daemon's own
    // endpoint and the agent is looping on *this* request, so a timeout would only
    // move the loop somewhere less obvious.
    let verdict = loop {
        {
            let inner = app.inner.read().await;
            let open = inner
                .sessions
                .get(&id)
                .and_then(|s| s.interaction.as_ref())
                .filter(|i| i.id == ask_id);
            match open {
                Some(i) => {
                    if let Some(a) = &i.answer {
                        break a.clone();
                    }
                }
                None => return Err(ApiError(anyhow::anyhow!("the confirmation was dropped"))),
            }
        }
        app.answered.notified().await;
    };

    if verdict != "post" {
        mark_thread(&app, number, &thread_id, |t| {
            t.status = crate::post::ThreadStatus::Held;
        })
        .await;
        app.notify().await;
        return Ok(Json(json!({ "posted": false, "reason": "held back" })));
    }

    // Fetched now: the thread must still be there, and the ids the write needs are
    // this fetch's, not the ones triage saw.
    let fresh = fetch_threads(&app, number).await?;
    let forge = write_forge(&app)?;
    // `gh` runs in the main checkout, the way every other write does, so it picks
    // up the same auth and config. Through `post::post_one` rather than
    // `forge.reply` directly, so this path files the story its reply links to,
    // substitutes the token, and stays idempotent — the same three rules the
    // batch obeys, from the same code.
    let posted = crate::post::post_one(
        &app,
        &forge,
        &app.cfg.main_checkout.clone(),
        number,
        &thread_id,
        &reply,
        planned.story.as_ref(),
        &fresh,
    )
    .await?;

    // A reply that could not be written is not "answered": leave the thread
    // committed-but-unanswered so the overview still shows it as yours.
    if let crate::post::Posted::HeldNoStory(why) = &posted {
        mark_thread(&app, number, &thread_id, |t| {
            t.note = Some(format!("story not filed — {why}"));
        })
        .await;
        app.notify().await;
        return Ok(Json(
            json!({ "posted": false, "reason": "the story it links to was not filed" }),
        ));
    }

    mark_thread(&app, number, &thread_id, |t| {
        t.status = crate::post::ThreadStatus::Replied;
    })
    .await;
    app.notify().await;
    Ok(Json(json!({
        "posted": true,
        "already": posted == crate::post::Posted::AlreadyThere,
    })))
}

/// Update one thread's place in the run, if the run is still there.
///
/// Silent when it is not: a run can be closed while its session is still winding
/// down, and failing the agent's call over bookkeeping would be the tail wagging
/// the dog.
async fn mark_thread(
    app: &Arc<AppState>,
    pr: u64,
    thread_id: &str,
    f: impl FnOnce(&mut crate::post::PlannedThread),
) {
    let mut inner = app.inner.write().await;
    inner.with_resolve_runs("thread progress", |runs| {
        let Some(run) = runs.get_mut(&pr) else {
            return false;
        };
        match run.plan.threads.iter_mut().find(|t| t.thread_id == thread_id) {
            Some(t) => {
                f(t);
                true
            }
            None => false,
        }
    });
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
            .ok_or_else(|| anyhow::anyhow!("no such session {id}"))?;
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
        let text = body.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
        if picked.free && text.is_none() {
            return Err(ApiError(anyhow::anyhow!(
                "\"{}\" is the option that asks for words, and none were written",
                picked.label
            )));
        }
        if !picked.free && text.is_some() {
            return Err(ApiError(anyhow::anyhow!(
                "\"{}\" takes no words",
                picked.label
            )));
        }
        open.answer = Some(body.answer.clone());
        open.answer_text = text.map(str::to_string);
        // Answered, so it is going again. `Stop` will correct this if the turn
        // ends for real a moment later.
        s.set_state(crate::model::State::Working);
    }
    app.answered.notify_waiters();
    app.notify().await;
    Ok(Json(json!({ "answered": body.answer })))
}

#[derive(Deserialize)]
pub struct SpawnBody {
    /// Where to work. Defaults to the caller's own workspace, which is what you
    /// mean when you want a hand with the thing you are already doing.
    #[serde(default)]
    pub workspace: Option<String>,
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

    let workspace = match body.workspace {
        Some(w) => w,
        None => {
            let inner = app.inner.read().await;
            inner
                .sessions
                .get(&id)
                .map(|s| s.workspace.clone())
                .ok_or_else(|| anyhow::anyhow!("no such session {id}"))?
        }
    };

    let child = spawn::spawn_session(&app, &workspace, Kind::Interactive, None).await?;
    if let Some(prompt) = body.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&child) {
            // The same path a vendored prompt takes: typed in at `SessionStart`,
            // because an interactive session honours nothing else.
            s.pending_prompt = Some(prompt.to_string());
        }
    }
    app.notify().await;
    Ok(Json(json!({ "session": child, "workspace": workspace })))
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
            match &s.state {
                // `Ready` and interrupted: relaunched with an unfinished turn
                // behind it and never prompted since, which is the one state
                // where "continue" is both true and what you meant.
                //
                // The word is a real instruction that lands in the transcript, so
                // the other waiting states are wrong targets rather than merely
                // unnecessary ones. A finished turn would be told to invent more
                // work; a thread asking you something would get a non-answer; a
                // permission prompt or a question takes the keystroke as consent.
                crate::model::State::YourTurn { reason, .. } => match reason {
                    // And only one that was cut off mid-turn. A conversation that
                    // had finished before the restart comes back at the same empty
                    // prompt, and "continue" there invents the next piece of work.
                    crate::model::TurnReason::Ready if s.interrupted => {
                        targets.push((s.id, pty))
                    }
                    // Both take a keystroke as an answer: a permission prompt as
                    // consent, a question as whichever choice is highlighted.
                    // Named rather than skipped, so pressing the button does not
                    // quietly leave the sessions that most need you behind.
                    crate::model::TurnReason::NeedsPermission
                    | crate::model::TurnReason::AskedAQuestion => {
                        held.push(s.title.clone().unwrap_or_else(|| s.workspace.clone()));
                    }
                    _ => {}
                },
                // Working, starting, failing: not waiting on you, so not yours to
                // interrupt. A nudge into a running turn is a stray line of input.
                _ => {}
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
            let _ = pty.write(text.as_bytes());
            // The return goes separately, for the reason `SessionStart` learned:
            // text and newline in one burst read as a paste, and a pasted newline
            // is a line break in the box rather than a send. 500ms rather than
            // 300: the shorter gap left one session in four holding typed text it
            // never sent.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let _ = pty.write(b"\r");
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
    revive(&app, id, false).await
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
/// **Except an automation.** A fix or resolve run is an agent working that PR's
/// branch, and a fresh worktree is cut from upstream — the fork would come back
/// on the wrong code entirely. Those stay in the workspace they were run in.
pub async fn fork_session(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<serde_json::Value> {
    let (automation, had_a_turn) = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("no such session {id}"))?;
        (s.is_automation(), s.had_a_turn)
    };
    // Refuse before a worktree is cut, not after the fork dies in it. A fork
    // replays the conversation with `--resume`, so a session that never had a turn
    // forks into an instant exit — and `spawn_worktree_session` would have done a
    // real `git worktree add` first, leaving a fresh tree holding a dead session.
    // The SPA greys the menu item on the same bit, but a stale snapshot or a direct
    // call reaches here regardless, which is why the guard lives on this side too.
    if !had_a_turn {
        return Err(ApiError(anyhow::anyhow!(
            "session {} has no conversation yet — nothing to fork",
            &id.to_string()[..8]
        )));
    }
    if automation {
        return revive(&app, id, true).await;
    }
    let new_id = spawn::spawn_worktree_session(&app, None, Some(id)).await?;
    Ok(Json(json!({ "session": new_id, "warning": None::<String> })))
}

/// The shared half of resume and fork: get the worktree back, then relaunch.
async fn revive(app: &Arc<AppState>, id: Uuid, fork: bool) -> ApiResult<serde_json::Value> {
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
        crate::worktree::branch_drift(&cwd, recovery.as_ref())
    } else {
        crate::worktree::revive(app, &cwd, recovery)
            .await
            .with_context(|| format!("session {id}"))?
    };

    // Its recorded kind, not `Interactive`: reopening a fix run should come back as
    // the automation the rail colours and the guard table counts.
    let kind = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .get(&id)
            .map(|s| s.kind.clone())
            .unwrap_or(Kind::Interactive)
    };
    let source = if fork {
        spawn::Source::Fork(id)
    } else {
        spawn::Source::Resume(id)
    };
    let new_id = spawn::spawn_session(app, &workspace, kind, Some(source)).await?;
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

/// Upgrade the agent binary, in the drawer where you can watch it.
///
/// Deliberately not a blocking call that reports a toast: mise fetches and
/// unpacks, so it takes long enough to want progress, and an upgrade that failed
/// halfway is exactly the thing you need the output of.
///
/// Safe to press with sessions running, which is the whole reason it is a button:
/// mise installs into a versioned directory and repoints, so a running `claude`
/// keeps the image it loaded. Sessions in flight finish on the old version; the
/// next one spawned gets the new. Nothing is restarted and nothing is asked of
/// you afterwards.
///
/// Refuses when the poller has not found an update, rather than running `mise
/// upgrade` on a hunch — the tool name comes from what mise reported, so without
/// that there is nothing to name.
pub async fn upgrade_agent(State(app): State<Arc<AppState>>) -> ApiResult<serde_json::Value> {
    let pending = app.inner.read().await.agent_update.clone();
    let Some(u) = pending else {
        return Err(ApiError(anyhow::anyhow!(
            "no agent update to install — refresh the check first"
        )));
    };
    let argv = crate::agent_update::upgrade_argv(&u.tool);
    // In main: the version mise resolves is the one that checkout's config pins,
    // and the check that found this update ran there too.
    let id = spawn::spawn_in_drawer(
        &app,
        MAIN,
        "upgrade",
        &argv,
        spawn::AfterDrawerExit::RecheckAgentUpdate,
    )
    .await?;
    Ok(Json(json!({ "process": id, "from": u.current, "to": u.latest })))
}

/// Re-run the agent version check now.
pub async fn refresh_agent_update(State(app): State<Arc<AppState>>) -> ApiResult<serde_json::Value> {
    crate::agent_update::refresh(&app).await?;
    let now = app.inner.read().await.agent_update.clone();
    Ok(Json(json!({ "update": now })))
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

/// Exchange the branches checked out in main and a worktree.
///
/// What it is *for*: main is where the managed processes and the dev stack live,
/// so work that needs them has to be *in* main. Before this the only way was to
/// commit, push, and check the branch out by hand.
///
/// # What it is not
///
/// Not a swap of roles. Worktrees live inside main, so one cannot become the
/// primary checkout without containing its own parent, and git will not move the
/// main worktree anyway. Only what each has checked out is exchanged, and both
/// directories stay exactly where they are.
///
/// # Both ways
///
/// The branches trade places, and so do the conversations about them: the
/// worktree's session is relocated into main and main's own session, if it had one,
/// is relocated out to the worktree. Symmetry is the point — a swap that moved one
/// side only left main's session reading a tree that had changed under it.
///
/// Relocating keeps the session id, so each conversation continues as one rail row
/// rather than gaining a forked sibling; `spawn::relocate_session` has the how, and
/// falls back to a fork only if a resume will not stay up.
///
/// # The refusals
///
/// An unclean tree, a stopped rebase, or a session **mid-turn** in either.
///
/// Mid-turn, not merely open: swapping is a regular move, and a session sitting at
/// its prompt in main is the normal state to do it from. Refusing on any live
/// session would make the ordinary case fail. An idle session's next turn re-reads
/// the tree; an agent that is *writing* into one that changes under it is the case
/// worth stopping, and it is the same distinction `nudge_sessions` already draws
/// with `is_busy`.
///
/// Main having no session at all is equally fine — nothing here requires one.
pub async fn swap_with_main(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    if workspace == MAIN {
        return Err(ApiError(anyhow::anyhow!(
            "main cannot be swapped with itself"
        )));
    }
    let tree = app
        .workspace_path(&workspace)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?;
    let main = app.cfg.main_checkout.clone();

    // Sessions first: the cheapest refusal. Only the ones actually working — an
    // idle session at its prompt is the normal place to swap from.
    {
        let inner = app.inner.read().await;
        let busy = |ws: &str| -> Option<String> {
            inner
                .sessions
                .values()
                .find(|s| s.workspace == ws && s.state.is_busy())
                .map(|s| s.title.clone().unwrap_or_else(|| s.id.to_string()[..8].to_string()))
        };
        for (label, ws) in [("main", MAIN), ("this worktree", workspace.as_str())] {
            if let Some(who) = busy(ws) {
                return Err(ApiError(anyhow::anyhow!(
                    "{label} has an agent mid-turn ({who}); the swap replaces every file \
                     under it, so let that turn finish first"
                )));
            }
        }
    }

    // Uncommitted work is carried, not refused — see `git::swap_branches`. Only a
    // stopped rebase is still a refusal: a tree mid-rebase cannot switch at all.
    let (m, t) = (main.clone(), tree.clone());
    let (swapped, untracked) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(crate::git::Swap, Vec<String>)> {
            for (label, path) in [("the main checkout", &m), ("this worktree", &t)] {
                if crate::git::rebase_in_progress(path) {
                    anyhow::bail!(
                        "{label} has a rebase stopped part-way; finish or abort it first"
                    );
                }
            }
            // Listed before the swap, because afterwards they are indistinguishable
            // from whatever the other branch leaves untracked. `stash create` does
            // not take untracked files, so these stay put and are named rather than
            // quietly not moving.
            let left = crate::git::untracked_in(&t, None)?;
            let swapped = crate::git::swap_branches(&m, &t)?;
            Ok((swapped, left))
        })
        .await
        .map_err(|e| anyhow::anyhow!("the swap task panicked: {e}"))??;

    // Main now holds a swapped-in branch, which can itself be a PR head. That must
    // not be parked away as if `open_pr(main)` had left it there, so the open-PR
    // provenance mark is cleared: a swap is a deliberate placement that stays.
    *app.main_pr_park.write().await = None;

    // Who travels is decided **here**, before `reconcile` runs, and the ordering is
    // load-bearing. `reconcile` re-stamps a live session's branch from whatever its
    // tree has checked out now, which is the right rule everywhere else and is
    // exactly wrong in this window: the branches have already moved, so a reconcile
    // first would tell every live session it had always been on the branch that just
    // arrived, and then nobody matches the branch that left. Driving this against a
    // fixture daemon is what caught it: the swap reported carrying nothing while
    // two live conversations sat in the two trees.
    //
    // By branch, not by address: `swapped.worktree_now` is what left main and
    // `swapped.main_now` is what left the worktree, so each side asks "who here was
    // working on the branch that just moved out".
    //
    // Both are picked before either moves. Choosing as we go would let the second
    // choice see the session the first one just delivered — for the moment in
    // between, both conversations live in the worktree — and send it straight back.
    let (outgoing, outgoing_records) = to_carry(&app, MAIN, &swapped.worktree_now).await;
    let (incoming, incoming_records) = to_carry(&app, &workspace, &swapped.main_now).await;

    // Each tree gave a branch away, and `reconcile` only adds. Left in, the
    // worktree would go on claiming the branch main now holds, and a PR flow for it
    // would be pointed at the wrong tree — found by driving this against a real
    // daemon, not by reading it.
    //
    // This and everything after it runs even when the WIP did not re-apply. The
    // branches moved; refusing to record that would leave the daemon describing a
    // world git no longer agrees with, which is worse than the failure itself.
    app.forget_branch(&workspace, &swapped.main_now).await;
    app.forget_branch(MAIN, &swapped.worktree_now).await;

    // Both panes describe a tree whose every file just changed.
    let _ = app.reconcile(MAIN).await;
    let _ = app.reconcile(&workspace).await;

    // The conversations follow their branches, in both directions, or the swap only
    // moves half of what you meant.
    //
    // Main's session travels too: its branch is in the worktree now, and a
    // conversation left staring at a tree that changed under it is the half of the
    // old behaviour that made this a one-way move rather than a swap.

    // Out of main **first**, and the order is load-bearing rather than tidy: main
    // holds one session at a time, so while the outgoing one is still sitting there
    // the arrival is refused outright ("main is occupied by …"). Vacating makes the
    // room. Found by driving a two-way swap against a real daemon, not by reading it.

    let into_worktree = match outgoing {
        Some(id) => Some(spawn::relocate_session(&app, id, &workspace, CARRY_GRACE).await),
        None => None,
    };
    let into_main = match incoming {
        Some(id) => Some(spawn::relocate_session(&app, id, MAIN, CARRY_GRACE).await),
        None => None,
    };

    // The conversations that were not running. No process work, so these cannot
    // fail the swap and are not reported back as a carry: the rail simply shows
    // them where their branch went.
    for id in outgoing_records {
        carry_record(&app, id, &workspace, &tree, &swapped.worktree_now).await;
    }
    for id in incoming_records {
        carry_record(&app, id, MAIN, &main, &swapped.main_now).await;
    }

    // The relocated ones are running, so they are told the same thing the records
    // are. Set after the resume, because `spawn_session` rebuilds the record under
    // the same id and would overwrite a notice left on the session it replaced.
    for (moved, branch, from, to, into_main) in [
        (&into_worktree, &swapped.worktree_now, &main, &tree, false),
        (&into_main, &swapped.main_now, &tree, &main, true),
    ] {
        let Some(Ok(moved)) = moved else { continue };
        let notice = arrival_notice(&app, branch, from, to, into_main);
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&moved.id) {
            s.arrival_notice = Some(notice);
        }
    }
    app.notify().await;

    // Where to land the pane: main is what you pressed this for, so the session that
    // arrived there wins, and the one that left main is the fallback.
    let select = into_main
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .or_else(|| into_worktree.as_ref().and_then(|r| r.as_ref().ok()))
        .map(|r| r.id.to_string());

    tracing::info!(
        %workspace,
        main_now = %swapped.main_now,
        worktree_now = %swapped.worktree_now,
        wip_error = ?swapped.wip_error,
        "swapped branches with main"
    );
    Ok(Json(json!({
        "main": swapped.main_now,
        "worktree": swapped.worktree_now,
        "workspace": workspace,
        "select": select,
        "into_main": carried_json(&into_main),
        "into_worktree": carried_json(&into_worktree),
        // A partial success, said as one: the branches moved, this did not. The
        // message names the WIP commit the work is still in.
        "wip_error": swapped.wip_error,
        // Named, not counted: knowing *which* files stayed behind is the difference
        // between going to fetch them and wondering what you lost.
        "untracked_left": untracked,
    })))
}

/// How long a relocated session has to prove it stayed up.
///
/// A grace window, not a health check: what has to be ruled out is the *instant*
/// exit of a `--resume` that found nothing, because the fork fallback hangs on it.
const CARRY_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// One direction of the carry, as the SPA reads it.
///
/// `null` is its own answer and not an error: it means there was nothing in that
/// tree to move, which is the ordinary case for a swap into an empty main.
fn carried_json(r: &Option<anyhow::Result<spawn::Relocated>>) -> serde_json::Value {
    match r {
        None => serde_json::Value::Null,
        Some(Ok(moved)) => json!({
            "session": moved.id.to_string(),
            // A fork, not the move that was promised — the id changed, so the rail
            // is about to show a second row and it is worth saying why.
            "degraded": moved.degraded,
            "error": serde_json::Value::Null,
        }),
        Some(Err(e)) => json!({
            "session": serde_json::Value::Null,
            "degraded": false,
            "error": format!("{e:#}"),
        }),
    }
}

/// Everything in a workspace that belongs to `branch`, split by what moving it costs.
///
/// `.0` is the one live conversation, which has to be *relocated*: killed, re-filed
/// and resumed in the destination. `.1` is every other session recorded there on the
/// same branch, which is a field update and a file move with no process in it.
///
/// **Both halves matter, and the second one is the ordinary case.** The old version
/// of this returned only the live pick, so a swap made while nothing was running
/// moved the branch and carried no conversation at all. That is not a rare race:
/// you swap between pieces of work, and the session you are swapping away from is
/// usually the one you just finished with. The symptom was a pane whose transcript
/// and whose changed files were about different stories, days later, with nothing
/// saying why.
///
/// Selection is by branch rather than by recency, which is the point of the field:
/// a worktree outlives the branches that pass through it, so "newest here" and
/// "about the work that is leaving" are different questions. A session with no
/// recorded branch answers neither and stays put.
///
/// Automation is left alone deliberately: a fix or resolve run belongs to its PR's
/// worktree, and moving one into main would put an agent that rebases and
/// force-pushes on the tree every worktree is cut from.
async fn to_carry(
    app: &Arc<AppState>,
    workspace: &str,
    branch: &str,
) -> (Option<SessionId>, Vec<SessionId>) {
    // Read out under one guard, decided outside it: `has_conversation` below is a
    // file read, and the lock is fair, so a writer queued behind it would block
    // every reader queued behind that in turn.
    let (mut live, records) = {
        let inner = app.inner.read().await;
        let mine = inner
            .sessions
            .values()
            .filter(|s| s.workspace == workspace)
            .filter(|s| matches!(s.kind, Kind::Interactive))
            .filter(|s| s.branch.as_deref() == Some(branch));
        let mut live = Vec::new();
        let mut records = Vec::new();
        for s in mine {
            if s.state.is_live() {
                live.push((s.created_at, s.id, s.cwd.clone(), s.transcript_path.clone()));
            } else if s.recovery.is_none() {
                // A recovery record describes a worktree that was torn down, so the
                // session is not *in* either tree here and which branch sits where
                // has nothing to do with it. `worktree::branch_drift` already says
                // its piece when one of those is resumed.
                records.push(s.id);
            }
        }
        live.sort_by_key(|(created, ..)| std::cmp::Reverse(*created));
        (live, records)
    };
    // A file does not prove a conversation — a session that started but never spoke
    // owns a file of headers — so this asks `has_conversation`. Newest-first,
    // stopping at the first hit, so the usual cost is one read rather than one per
    // session to then discard all but the newest.
    let carried = live
        .drain(..)
        .find(|(_, id, cwd, recorded)| {
            crate::store::has_conversation(*id, cwd, recorded.as_deref())
        })
        .map(|(_, id, ..)| id);
    (carried, records)
}

/// What an agent is told when its conversation has been moved.
///
/// Two halves on purpose. The factual one the daemon can always say: which branch
/// you are on, where it is now, where you were reading before. The second is
/// `workspace_notes`, which is the only part that knows anything about a particular
/// repo (that main is where the dev stack runs, say), and it comes from that
/// repo's config rather than from anything here.
///
/// Written as an instruction rather than a status line because that is what it is:
/// the paths the agent has been using are stale from this point on, and it will
/// reach for one on its very next tool call unless it is told not to.
fn arrival_notice(
    app: &Arc<AppState>,
    branch: &str,
    from: &std::path::Path,
    to: &std::path::Path,
    into_main: bool,
) -> String {
    let mut note = format!(
        "This conversation has been moved. The branch it is working on ({branch}) was \
         swapped into another checkout, and the session followed it: your working \
         directory is now {}, and until this move it was {}. Treat remembered absolute \
         paths as stale and re-read anything you are about to change.",
        to.display(),
        from.display(),
    );
    if let Some(extra) = app.cfg.workspace_notes.for_main(into_main) {
        note.push(' ');
        note.push_str(extra);
    }
    note
}

/// Move a session that is not running. The record follows its branch, and the
/// transcript follows the record.
///
/// Separate from `spawn::relocate_session` because there is no pty to kill and no
/// `--resume` to watch stay up, so none of that machinery applies and none of its
/// failure modes exist. Auto-resume brings this conversation back in the directory
/// recorded here, which is the whole reason the record has to move at all.
///
/// The transcript move is best effort for the reason `store::move_transcript`
/// documents: `--resume` resolves a conversation by id wherever the file sits, so a
/// failure costs the slug lookup its cheap path and nothing else.
async fn carry_record(
    app: &Arc<AppState>,
    id: SessionId,
    dest: &str,
    dest_path: &std::path::Path,
    branch: &str,
) {
    let src_cwd = {
        let inner = app.inner.read().await;
        match inner.sessions.get(&id) {
            Some(s) => s.cwd.clone(),
            None => return,
        }
    };
    let refiled = match crate::store::move_transcript(id, &src_cwd, dest_path) {
        Ok(moved) => moved,
        Err(e) => {
            tracing::warn!(
                session = %id,
                "could not re-file the transcript under {}; the record moves anyway: {e:#}",
                dest_path.display()
            );
            None
        }
    };
    let notice = arrival_notice(app, branch, &src_cwd, dest_path, dest == MAIN);
    let mut inner = app.inner.write().await;
    if let Some(s) = inner.sessions.get_mut(&id) {
        s.workspace = dest.to_string();
        s.cwd = dest_path.to_path_buf();
        // Only on a move that happened. Left pointing at the old slug otherwise,
        // which is still where the file is.
        if let Some(path) = refiled {
            s.transcript_path = Some(path);
        }
        // Waits here until auto-resume starts the conversation again, which is the
        // first moment there is an agent to tell.
        s.arrival_notice = Some(notice);
    }
}

pub async fn teardown(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<worktree::Preflight> {
    Ok(Json(worktree::teardown(&app, &workspace).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the channel: the agent's poll is released by the answer
    /// rather than by a timeout, and it comes back carrying the choice.
    #[tokio::test]
    async fn an_answer_releases_the_poll_the_agent_is_sitting_in() {
        use crate::config::Config;
        use crate::model::{Interaction, InteractionOption, Kind, Session, MAIN};
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-ask-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7797}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let id = Uuid::new_v4();
        let ask_id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut sess = Session::new(id, MAIN.to_string(), dir.clone(), Kind::Interactive);
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
        answer(
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
        use crate::config::Config;
        use crate::model::{Interaction, InteractionOption, Kind, Session, MAIN};
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-ask3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7795}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);
        let id = Uuid::new_v4();
        let ask_id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut sess = Session::new(id, MAIN.to_string(), dir.clone(), Kind::Interactive);
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
            Json(AnswerBody { ask: ask_id, answer: "mine".into(), text: None }),
        )
        .await
        .err()
        .expect("refused with no words");
        assert!(format!("{}", err.0).contains("none were written"));

        answer(
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
        assert_eq!(got.answer_text.as_deref(), Some("put it under Pushing, but say why"));
        drop(inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An answer nobody offered would reach the agent as an instruction no branch
    /// was written for.
    #[tokio::test]
    async fn an_answer_that_was_not_offered_is_refused() {
        use crate::config::Config;
        use crate::model::{Interaction, InteractionOption, Kind, Session, MAIN};
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-ask2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7796}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);
        let id = Uuid::new_v4();
        let ask_id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut sess = Session::new(id, MAIN.to_string(), dir.clone(), Kind::Interactive);
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
        .err()
        .expect("refused");
        assert!(format!("{}", err.0).contains("not one of the options"));
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// A stray escape is not harmless, so the states that would misread it are
    /// refused by name.
    ///
    /// The two waiting ones are the point: mid-turn was driven against a real
    /// session and refused, but a question and a permission prompt cannot be
    /// arranged on demand, and those are exactly the two where the keystroke would
    /// *answer* — cancelling the one, declining the other — rather than do nothing.
    #[tokio::test]
    async fn rewind_refuses_every_state_that_would_read_an_escape_as_an_answer() {
        use crate::config::Config;
        use crate::model::{Kind, Session, State as S, TurnReason as R, MAIN};
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-rewind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7796}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let at = |reason| S::YourTurn { since: std::time::SystemTime::now(), reason };
        for (state, want) in [
            (at(R::AskedAQuestion), "cancel the question"),
            (at(R::NeedsPermission), "decline it"),
            (S::Working, "mid-turn"),
            (S::Starting, "still starting"),
        ] {
            let id = Uuid::new_v4();
            {
                let mut inner = app.inner.write().await;
                let mut s = Session::new(id, MAIN.to_string(), dir.clone(), Kind::Interactive);
                s.had_a_turn = true;
                s.state = state.clone();
                inner.sessions.insert(id, s);
            }
            match rewind_session(State(app.clone()), Path(id)).await {
                Err(e) => {
                    let said = format!("{:#}", e.0);
                    assert!(said.contains(want), "{state:?} said {said:?}, wanted {want:?}");
                }
                Ok(_) => panic!("{state:?} must not open the picker"),
            }
        }

        // And a session at the prompt with nothing behind it: the picker would
        // open on an empty conversation, which reads as a broken button.
        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), dir.clone(), Kind::Interactive);
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

    #[test]
    fn every_route_the_vendored_prompts_call_on_the_ask_token_is_exempt() {
        // The three in `commands/resolve-run.md` plus `/spawn`. `/committed` was
        // missing and the run's central seam answered 403 to the only caller it
        // has; these are the literal paths those prompts curl.
        for p in [
            "/api/session/<id>/ask",
            "/api/session/<id>/ask/<ask>/wait",
            "/api/session/<id>/thread/PRRT_x/committed",
            "/api/session/<id>/thread/PRRT_x/stuck",
            "/api/session/<id>/spawn",
        ] {
            assert!(is_ask_route(p), "{p} must not need the app token");
        }
        // And nothing else on the session: these are the SPA's, on the app token.
        for p in [
            "/api/session/<id>/kill",
            "/api/session/<id>/answer",
            "/api/session/<id>/fork",
            "/api/state",
        ] {
            assert!(!is_ask_route(p), "{p} is not the agent's to call");
        }
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

    /// Build an app whose main checkout is a scratch directory, with no git in it.
    /// Enough for everything below: the carry decides from session records, and the
    /// only filesystem it touches is a transcript move that finds nothing.
    fn app_in(tag: &str) -> (std::sync::Arc<crate::state::AppState>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "orchd-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: crate::config::Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7798}}"#,
            dir.display()
        ))
        .unwrap();
        let app = crate::state::AppState::new(cfg, "t".into(), crate::window::Chrome::None);
        (app, dir)
    }

    /// The bug this is here for. Two swaps moved three branches between three trees
    /// and carried no conversation at all, because every session involved happened
    /// to be archived at the time, so days later a pane's transcript was about one
    /// story and its changed files about another.
    ///
    /// `is_live` was the whole filter, and "not running" is not the rare case: you
    /// swap between pieces of work, and the one you are swapping away from is
    /// usually the one you just stopped.
    #[tokio::test]
    async fn a_swap_carries_the_conversation_that_was_not_running() {
        use crate::model::{ArchiveState, Kind, Session, State};

        let (app, dir) = app_in("carry-archived");
        let put = |ws: &str, branch: Option<&str>, state: State, recovery: Option<ArchiveState>| {
            let mut s = Session::new(Uuid::new_v4(), ws.to_string(), dir.clone(), Kind::Interactive);
            s.branch = branch.map(str::to_string);
            s.had_a_turn = true;
            s.recovery = recovery;
            s.state = state;
            s
        };
        let archived = || State::Archived { resumable: true };

        let (mine, others, unknown, torn, live_elsewhere) = {
            let mut inner = app.inner.write().await;
            let mine = put("wt", Some("feature/a"), archived(), None);
            // Same tree, different branch: a worktree outlives the branches that
            // pass through it, so "in this directory" was never the question.
            let others = put("wt", Some("feature/b"), archived(), None);
            // Written before the branch was recorded. An unknown branch answers
            // nothing, so it travels nowhere.
            let unknown = put("wt", None, archived(), None);
            // Its worktree was torn down, so it is not *in* either tree here and
            // which branch sits where has nothing to do with it.
            let torn = put(
                "wt",
                Some("feature/a"),
                archived(),
                Some(ArchiveState::Recoverable {
                    name: "gone".into(),
                    branch: "feature/a".into(),
                    head_sha: "abc".into(),
                }),
            );
            let live_elsewhere = put("other", Some("feature/a"), archived(), None);
            let ids = (
                mine.id,
                others.id,
                unknown.id,
                torn.id,
                live_elsewhere.id,
            );
            for s in [mine, others, unknown, torn, live_elsewhere] {
                inner.sessions.insert(s.id, s);
            }
            ids
        };

        let (live, records) = to_carry(&app, "wt", "feature/a").await;
        assert_eq!(live, None, "nothing was running, so nothing is relocated");
        assert_eq!(records, vec![mine], "only the conversation whose branch left");
        for stranded in [others, unknown, torn, live_elsewhere] {
            assert!(!records.contains(&stranded));
        }
    }

    /// Selection is by branch, not by recency, which is the whole point of
    /// recording it. The old picker took the newest session in the directory, so a
    /// swap could carry a conversation about work that was not moving and leave the
    /// one that was.
    #[tokio::test]
    async fn the_newest_conversation_is_not_the_one_that_travels() {
        use crate::model::{Kind, Session, State};

        let (app, dir) = app_in("carry-newest");
        let (wanted, newer) = {
            let mut inner = app.inner.write().await;
            let mut wanted =
                Session::new(Uuid::new_v4(), "wt".into(), dir.clone(), Kind::Interactive);
            wanted.branch = Some("feature/a".into());
            wanted.had_a_turn = true;
            wanted.state = State::Archived { resumable: true };

            let mut newer =
                Session::new(Uuid::new_v4(), "wt".into(), dir.clone(), Kind::Interactive);
            newer.branch = Some("feature/b".into());
            newer.had_a_turn = true;
            newer.state = State::Archived { resumable: true };
            newer.created_at = wanted.created_at + std::time::Duration::from_secs(60);

            let ids = (wanted.id, newer.id);
            inner.sessions.insert(wanted.id, wanted);
            inner.sessions.insert(newer.id, newer);
            ids
        };

        let (_, records) = to_carry(&app, "wt", "feature/a").await;
        assert_eq!(records, vec![wanted]);
        assert!(!records.contains(&newer), "recency is not the question");
    }

    /// Moving the record is only half of it: the conversation comes back believing
    /// it is in the tree it was reading all along, so it is told once, at the next
    /// prompt, and the project's own note about the destination rides along.
    #[tokio::test]
    async fn a_carried_conversation_is_told_where_it_now_is() {
        use crate::model::{Kind, Session, MAIN};

        let dir = std::env::temp_dir().join(format!(
            "orchd-carry-notice-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // The half only the project knows. orchd supplies the facts about the move;
        // this sentence is the repo's business and comes from its own config.
        let cfg: crate::config::Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7799,
                 "workspace_notes":{{"main":"the dev stack only runs here"}}}}"#,
            dir.display()
        ))
        .unwrap();
        let app = crate::state::AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, "wt".to_string(), dir.join("wt"), Kind::Interactive);
            s.branch = Some("feature/a".into());
            s.had_a_turn = true;
            s.state = crate::model::State::Archived { resumable: true };
            inner.sessions.insert(id, s);
        }

        carry_record(&app, id, MAIN, &dir, "feature/a").await;

        let inner = app.inner.read().await;
        let s = &inner.sessions[&id];
        assert_eq!(s.workspace, MAIN, "the record follows its branch");
        assert_eq!(s.cwd, dir);
        let notice = s.arrival_notice.as_deref().expect("it has to be told");
        assert!(notice.contains("feature/a"), "which branch moved: {notice}");
        assert!(
            notice.contains(&dir.display().to_string()),
            "where it is now: {notice}"
        );
        assert!(
            notice.contains(&dir.join("wt").display().to_string()),
            "where it was reading before: {notice}"
        );
        assert!(
            notice.contains("the dev stack only runs here"),
            "and what the project says main is for: {notice}"
        );
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
        assert_eq!(crate::config::WorkspaceNotes::default().for_main(true), None);
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

#[derive(Deserialize)]
pub struct OpenFile {
    pub workspace: String,
    pub path: String,
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
        return Err(ApiError(anyhow::anyhow!("no path given")));
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
    let r#ref = match head_sha {
        Some(sha) => sha,
        None => tokio::task::spawn_blocking(move || crate::git::head_sha(&at))
            .await
            .context("resolving HEAD panicked")?
            .context("could not resolve HEAD for this workspace")?,
    };
    let forge = write_forge(&app)?;
    let url = forge.blob_url(&r#ref, path);
    open_external(&url)?;
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

// ---------------------------------------------------------------------------
// Review overlay
// ---------------------------------------------------------------------------

/// The forge the config selects, built for reads: repo + read token. The one
/// spot the four-line resolve/token/build dance lived; now the same for every
/// read endpoint, and dispatched by `cfg.forge`.
fn read_forge(app: &Arc<AppState>) -> Result<crate::forge::ForgeImpl, ApiError> {
    let (owner, name) = crate::resolve_repo(app)
        .ok_or_else(|| anyhow::anyhow!("no GitHub repo configured and none on the remote"))?;
    let token = crate::forge::resolve_token(app.cfg.github_token_file.as_deref())?;
    Ok(crate::forge::ForgeImpl::for_kind(app.cfg.forge, owner, name, token.value))
}

/// The same, for writes. Writes shell their own tool, so no read token is
/// needed — the forge carries only the repo it writes to.
fn write_forge(app: &Arc<AppState>) -> Result<crate::forge::ForgeImpl, ApiError> {
    let (owner, name) =
        crate::resolve_repo(app).context("no GitHub repo configured and none on the remote")?;
    Ok(crate::forge::ForgeImpl::for_kind(app.cfg.forge, owner, name, String::new()))
}

/// Fetch a PR's threads and hand back the parsed set.
///
/// Shared by the endpoints that need to know what is awaiting an answer *now*.
/// Always refetches straight from the forge: a stale thread list is the one
/// thing this flow must never act on, which is why nothing here caches.
async fn fetch_threads(app: &Arc<AppState>, pr: u64) -> Result<crate::forge::Threads, ApiError> {
    let forge = read_forge(app)?;
    let fetched = tokio::task::spawn_blocking(move || forge.threads(pr))
        .await
        .context("the thread fetch panicked")??;
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

fn pr_from_poll(prs: &[crate::forge::Pr], number: u64) -> Result<crate::forge::Pr, ApiError> {
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
        // answerable, and `fix-pr` is offered rather than required.
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
    // `fix_pr`'s `branch_busy`.
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

/// Start the session that carries out a triaged review.
///
/// The other half of `/post`, and deliberately the same payload: your decisions,
/// resolved through the same [`crate::post::resolve`] the batch uses, so the two
/// paths cannot read one set of answers differently. What changes is who does the
/// work — an agent that adapts a fix to a branch that moved and stops to ask,
/// rather than `git apply` and a refusal.
///
/// It writes no comment and pushes nothing. The session is handed a plan and the
/// worktree; every outward write stays here, on your button.
pub async fn pr_resolve_run(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(batch): Json<crate::post::Batch>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let proposals = {
        let inner = app.inner.read().await;
        inner
            .proposals
            .get(&number)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("PR #{number} has no triage to carry out"))?
    };
    // The same gates the batch refuses on, for the same reasons: uncommitted work
    // of yours would end up in the run's commits, a stopped rebase cannot take a
    // commit at all, and `fix-pr` is rewriting this very history. A run puts an
    // agent in the worktree for minutes, so this is worth refusing before it
    // starts rather than discovering per thread.
    if let Some(ws) = workspace_for(&app, &pr.head_ref).await {
        if let Some(g) = crate::triage::gate(&app, number, &ws).await? {
            return Err(ApiError(anyhow::anyhow!("{}", g.say())));
        }
    }
    // Fetched now, not from the cache: it is what makes the thread ids real and
    // the drift check mean anything.
    let fresh = fetch_threads(&app, number).await?;
    let plan = crate::post::plan(number, &proposals, &fresh, &batch, app.cfg.tracker)?;
    let session = spawn::spawn_resolve_run(&app, number, &pr.head_ref, &plan).await?;
    // Kept so the daemon can answer "what does this thread say" when the session
    // reports a commit. The agent is never told the reply is its to send.
    app.inner
        .write()
        .await
        .with_resolve_runs("run started", |runs| {
            runs.insert(
                number,
                crate::state::ResolveRun {
                    session,
                    plan: plan.clone(),
                    ended: None,
                },
            );
            true
        });
    app.notify().await;

    let answered = sweep_words_only(&app, number, &plan, &fresh).await;
    Ok(Json(json!({
        "session": session,
        "threads": plan.threads.len(),
        "answered": answered,
    })))
}

/// Answer the threads that need no code, now rather than never.
///
/// A words-only thread has nothing for the session to build — the prompt tells it
/// exactly that ("the daemon posts the reply. Move on.") — so no report from the
/// run will ever arrive for it, and `thread_committed` only fires on a commit.
/// Left alone these sat at `WordsOnly` for the life of the run with no button to
/// finish them.
///
/// No per-thread confirmation, unlike a committed thread: that card exists to
/// show the *real* diff beside the drafted reply, because a commit can differ
/// from what triage staged. Here there is no commit and nothing can drift — the
/// words are the ones approved on the card — so asking again would be ceremony.
///
/// One thread failing does not stop the others, for the same reason `post_outward`
/// keeps going: each is an independent write, and a run that answered three of
/// four should say so rather than lose all four.
async fn sweep_words_only(
    app: &Arc<AppState>,
    number: u64,
    plan: &crate::post::Plan,
    fresh: &crate::forge::Threads,
) -> usize {
    use crate::post::{Posted, ThreadStatus};

    let todo: Vec<_> = plan
        .threads
        .iter()
        .filter(|t| t.status == ThreadStatus::WordsOnly)
        .cloned()
        .collect();
    if todo.is_empty() {
        return 0;
    }
    let forge = match write_forge(app) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(
                "resolve run #{number}: no forge to answer words-only threads: {:#}",
                e.0
            );
            return 0;
        }
    };
    let at = app.cfg.main_checkout.clone();

    let mut answered = 0;
    for t in todo {
        // A stance with no words is a bare thumbs up; the two are never both.
        let outcome = match &t.reply {
            Some(reply) => crate::post::post_one(
                app,
                &forge,
                &at,
                number,
                &t.thread_id,
                reply,
                t.story.as_ref(),
                fresh,
            )
            .await
            .map(Some),
            None if t.stance.gives_thumbs_up() => {
                crate::post::react_one(&forge, &at, &t.thread_id, fresh)
                    .await
                    .map(|()| None)
            }
            // Neither words nor a reaction: nothing was ever going to leave for
            // this one, so it is done rather than stuck.
            None => Ok(None),
        };

        match outcome {
            Ok(Some(Posted::HeldNoStory(why))) => {
                mark_thread(app, number, &t.thread_id, |x| {
                    x.note = Some(format!("story not filed — {why}"));
                })
                .await;
            }
            Ok(_) => {
                answered += 1;
                mark_thread(app, number, &t.thread_id, |x| {
                    x.status = ThreadStatus::Replied;
                })
                .await;
            }
            Err(e) => {
                tracing::error!("resolve run #{number}: {} — {e:#}", t.location);
                mark_thread(app, number, &t.thread_id, |x| {
                    x.note = Some(format!("could not answer — {e:#}"));
                })
                .await;
            }
        }
    }
    app.notify().await;
    answered
}

/// Push the branch a run has been committing to.
///
/// Its own button, and deliberately not the end of the run: a run can answer four
/// threads and leave two for you, and pushing that is a judgement about whether
/// what is on the branch is worth showing. `--force-with-lease` only, through the
/// same helper every other push here uses.
pub async fn pr_run_push(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let path = gate_worktree(&app, number).await?;
    let branch = pr.head_ref.clone();
    tokio::task::spawn_blocking(move || crate::git::push_with_lease(&path, &branch))
        .await
        .context("the push panicked")??;
    // Re-measure, or the overview keeps saying there is work to push: `unpushed`
    // is the last reconcile's number, and this is the moment it stopped being true.
    if let Some(ws) = workspace_for(&app, &pr.head_ref).await {
        let _ = app.reconcile(&ws).await;
    }
    app.notify().await;
    Ok(Json(json!({ "pushed": pr.head_ref })))
}

/// Ask for a fresh review, from the reviewers whose threads are all answered.
///
/// Held back per reviewer rather than all-or-nothing: someone with an open thread
/// of their own is not being asked to look again at work that has not answered
/// them. Also its own button, because re-requesting is a claim that you are done.
pub async fn pr_run_rerequest(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let fresh = fetch_threads(&app, number).await?;
    let forge = write_forge(&app)?;
    let at = app.cfg.main_checkout.clone();

    // The threads this run answered, which is what decides whose review can be
    // asked for again. Taken from the run's own record: `Replied` is the only
    // status where the reviewer has actually been told something. `Held` and
    // `NeedsYou` carry a commit but no answer, and a `Manual` thread is yours —
    // each of those rightly holds its author back.
    //
    // This used to be derived from `!is_resolved` instead, and that could never
    // work: resolving is the reviewer's button and the daemon never presses it, so
    // every thread the run had just answered still read as open and nobody was
    // ever ready. See `post::rerequest_all`, which is now the one implementation.
    let done: Vec<String> = {
        let inner = app.inner.read().await;
        inner
            .resolve_runs
            .get(&number)
            .map(|r| {
                r.plan
                    .threads
                    .iter()
                    .filter(|t| t.status == crate::post::ThreadStatus::Replied)
                    .map(|t| t.thread_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    let done: Vec<&str> = done.iter().map(String::as_str).collect();

    let out = crate::post::rerequest_all(&forge, &at, number, &fresh, &done).await;
    app.notify().await;
    Ok(Json(json!({
        "rerequested": out.asked,
        "failed": out.failed.iter().map(|(who, e)| format!("{who}: {e}")).collect::<Vec<_>>(),
        // Said rather than left as silence: "nobody to re-request" and "three
        // people are still waiting on you" are different answers.
        "held_back": out.held.iter().map(|(who, t)| format!("{who} — {t} is still unanswered")).collect::<Vec<_>>(),
    })))
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
/// Where an `open` request wants the session.
#[derive(Deserialize)]
pub struct OpenPr {
    #[serde(rename = "where")]
    pub place: String,
}

/// Start working on a PR, without the review flow.
///
/// A worktree pinned to its head branch, or the main checkout switched onto it —
/// which is what you want when the PR needs the docker stack and `ng-watch` that
/// only main has. Main is refused rather than disturbed: switching a branch out
/// from under uncommitted work, or under a session already sitting there, is not
/// something a right-click menu gets to do.
pub async fn open_pr(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(body): Json<OpenPr>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        inner.prs.iter().find(|p| p.number == number).cloned()
    }
    .ok_or_else(|| anyhow::anyhow!("PR #{number} is not in the current poll"))?;

    let workspace = match body.place.as_str() {
        "worktree" => spawn::ensure_pr_worktree(&app, number, &pr.head_ref).await?,
        "main" => spawn::switch_main_to_pr(&app, &pr.head_ref).await?,
        other => return Err(ApiError(anyhow::anyhow!("unknown place {other}"))),
    };

    // `ensure_pr_worktree` reuses a worktree that already holds the branch, so
    // pressing this twice would otherwise stack a second session in it. (The `main`
    // arm already refused an occupied main inside `switch_main_to_pr`.)
    refuse_if_occupied(&app, &workspace).await?;
    let id = spawn::spawn_session(&app, &workspace, Kind::Interactive, None).await?;
    Ok(Json(json!({ "session": id, "workspace": workspace })))
}

pub async fn resolve_pr(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        inner.prs.iter().find(|p| p.number == number).cloned()
    };
    let pr = pr.ok_or_else(|| anyhow::anyhow!("PR #{number} is not in the current poll"))?;
    // Asked of the threads, not only of `needs_you`. They are different questions
    // and this button is the flow's, not the rail's: `needs_you` drops outdated
    // threads so a PR stops nagging about code that is gone, but the flow can
    // answer those — so a PR whose only unanswered threads were outdated refused
    // a run that had work to do, with a message saying there was none.
    //
    // `needs_you` still passes on its own, because it covers one thing threads
    // cannot: a review that requested changes and left no thread at all. And a
    // failed fetch falls back to it rather than refusing — a network blip is not
    // evidence there is nothing to do.
    let answerable = match fetch_threads(&app, number).await {
        Ok(fresh) => fresh.items.iter().filter(|t| t.answerable).count(),
        Err(e) => {
            tracing::warn!(pr = number, "could not check the threads: {:#}", e.0);
            0
        }
    };
    if answerable == 0 && !pr.needs_you {
        return Err(ApiError(anyhow::anyhow!(
            "PR #{number} has nothing waiting on you: every open thread has your \
             reply or your 👍 on it"
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
    Ok(Json(crate::edit::read(&root, &q.path, &app.cfg.shared_worktree_paths)?))
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
    let out = crate::edit::write(
        &root,
        &body.path,
        &body.content,
        &body.version,
        &app.cfg.shared_worktree_paths,
    )?;
    if matches!(out, crate::edit::WriteOutcome::Written { .. }) {
        // Agents working in this workspace hold a stale copy now, and will
        // overwrite it unless they are told (§5's invalidation, in the
        // direction that actually loses work).
        let resolved = crate::edit::resolve_in_workspace(&root, &body.path, &app.cfg.shared_worktree_paths)?;
        app.record_human_edit(resolved).await;
        // The changed-file pane and the diff must both reflect the write.
        let _ = app.reconcile(&body.workspace).await;
        app.notify().await;
    }
    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// fix-pr (§8) — hand-triggered only
// ---------------------------------------------------------------------------

/// Start a `fix-pr` run for a PR.
///
/// Deliberately **not** automatic. §8 fires it on a PR going red; running it by
/// hand instead means the guard table is a gate you read rather than one that
/// trips behind you, and it is the difference between a tool that helps and one
/// that rebases your branches while you are looking elsewhere.
pub async fn fix_pr(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    use crate::fix_pr::{evaluate, GuardInput, PrAutomation, Verdict};

    let pr = {
        let inner = app.inner.read().await;
        inner.prs.iter().find(|p| p.number == number).cloned()
    }
    .ok_or_else(|| anyhow::anyhow!("PR #{number} is not in the current poll"))?;

    // Asked the way the spawn enforces it, not with a second rule: an idle session
    // on the branch used to pass here and be refused a moment later by
    // `spawn_fix_pr_session`, once the run looked started.
    //
    // The worktree is no longer cut before this. `spawn_fix_pr_session` calls
    // `ensure_pr_worktree` itself, so creating one here only meant a refused run
    // left a worktree behind — and it is what split the two rules apart, since the
    // guard then asked about the empty tree it had just made.
    let branch_busy = spawn::branch_busy(&app, &pr.head_ref).await.is_some();

    let verdict = {
        let inner = app.inner.read().await;
        let running_automations = inner
            .sessions
            .values()
            .filter(|s| s.is_automation() && s.state.is_live())
            .count();

        evaluate(&GuardInput {
            pr: &pr,
            automation: inner.automation.get(number),
            // Your own login, from the poll. The authorship guard no longer
            // compares it to the head repo's owner — it asks whether you can push
            // there — but an empty one still fails the guard closed, so this must
            // stay the real viewer rather than anything derived from the PR.
            viewer: inner.viewer.as_deref().unwrap_or_default(),
            branch_busy,
            running_automations,
        })
    };

    match verdict {
        Verdict::Go => {}
        Verdict::No { reason } => return Err(ApiError(anyhow::anyhow!("{reason}"))),
    };

    let session = spawn::spawn_fix_pr_session(&app, number, &pr.head_ref).await?;
    {
        let mut inner = app.inner.write().await;
        // If this never reaches disk, a restart forgets the run started and the
        // one-run-per-PR cap can re-fire, so the write is not optional.
        inner.with_automation(&format!("PR #{number} run started"), |a| {
            a.by_pr.insert(
                number,
                PrAutomation::Running {
                    session,
                    started: std::time::SystemTime::now(),
                },
            );
            true
        });
    }

    // Exhaustion is recorded by the session's own exit watcher, which is the
    // one place that sees a pty end (`fix_pr::settle`).
    app.notify().await;
    Ok(Json(json!({ "session": session })))
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

    // Refresh the base first, or "behind" is answered from a stale ref. A failed
    // fetch is not fatal — rebasing onto a known-old base is sometimes what you
    // want — but it must not be silent: a network blip would otherwise look
    // identical to a clean rebase onto nothing new, which is the one outcome the
    // button exists to save you from thinking about.
    let warning = match crate::git::fetch_upstream(&app.cfg.main_checkout, &app.cfg.upstream_ref) {
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("rebase {workspace}: upstream fetch failed, base may be stale: {e:#}");
            Some(format!("upstream fetch failed — rebased onto the last-known base ({e})"))
        }
    };

    let upstream = app.cfg.upstream_ref.clone();
    let p = path.clone();
    let result = tokio::task::spawn_blocking(move || crate::git::rebase_onto(&p, &upstream)).await;

    let _ = app.reconcile(&workspace).await;
    app.notify().await;

    match result {
        Ok(Ok(())) => Ok(Json(json!({ "rebased": workspace, "warning": warning }))),
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
