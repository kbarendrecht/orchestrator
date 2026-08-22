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

    /* The agent's own two routes authenticate differently: they carry the
       session's `ask_token` rather than the app token, and the handlers do that
       check because only they know which session the path names. Exempted here
       the way hooks are, and for the same reason — the caller is a local process
       with no Origin and no business holding the key to everything else. */
    let is_ask = path.starts_with("/api/session/")
        && (path.ends_with("/ask") || path.ends_with("/wait") || path.ends_with("/spawn"));

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
    // a suffix nobody notices they have to extend.
    const SPENDS_GITHUB_TOKEN: [&str; 2] = ["/threads", "/review"];
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

    let (number, planned, cwd) = {
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
        (*number, planned, cwd)
    };

    // The real diff, not the one triage staged: what the reviewer is about to be
    // told happened is what actually landed, including whatever the agent had to
    // change to make it apply.
    let (sha, dir) = (body.sha.clone(), cwd.clone());
    let diff = tokio::task::spawn_blocking(move || {
        crate::git::commit_diff(&dir, &sha, crate::proposal::MAX_FIELD)
    })
    .await
    .context("reading the commit panicked")??;

    mark_thread(&app, number, &thread_id, |t| {
        t.commit = Some(body.sha.clone());
        t.status = crate::post::ThreadStatus::Committed;
    })
    .await;

    let Some(reply) = planned.reply.clone() else {
        // Nothing to say: the stance was a bare thumbs up and it is posted with
        // the rest, so there is nothing to confirm here.
        return Ok(Json(json!({ "posted": false, "reason": "this thread posts no reply" })));
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
    let root = fresh
        .root_for(&thread_id)
        .ok_or_else(|| anyhow::anyhow!("thread {thread_id} has no comment to answer"))?;
    let forge = write_forge(&app)?;
    // `gh` runs in the main checkout, the way every other write does, so it picks
    // up the same auth and config. `reply` applies the footer itself; handing it
    // a footed body would post two.
    let at = app.cfg.main_checkout.clone();
    tokio::task::spawn_blocking(move || forge.reply(&at, &root, &reply))
        .await
        .context("the write panicked")??;

    mark_thread(&app, number, &thread_id, |t| {
        t.status = crate::post::ThreadStatus::Replied;
    })
    .await;
    app.notify().await;
    Ok(Json(json!({ "posted": true })))
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
    if let Some(run) = inner.resolve_runs.get_mut(&pr) {
        if let Some(t) = run.plan.threads.iter_mut().find(|t| t.thread_id == thread_id) {
            f(t);
        }
    }
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
            if !crate::store::transcript_exists(s.id, &s.cwd, s.transcript_path.as_deref()) {
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
    let automation = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("no such session {id}"))?
            .is_automation()
    };
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
    // The forge shells curl, so it must not run on the async runtime.
    let forge = read_forge(&app)?;
    let fetched = tokio::task::spawn_blocking(move || forge.threads(number))
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

async fn fetch_threads(app: &Arc<AppState>, pr: u64) -> Result<crate::forge::Threads, ApiError> {
    let forge = read_forge(app)?;
    let fetched = tokio::task::spawn_blocking(move || forge.threads(pr))
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
    // Fetched now, not from the cache: it is what makes the thread ids real and
    // the drift check mean anything.
    let fresh = fetch_threads(&app, number).await?;
    let plan = crate::post::plan(number, &proposals, &fresh, &batch, app.cfg.tracker)?;
    let session = spawn::spawn_resolve_run(&app, number, &pr.head_ref, &plan).await?;
    // Kept so the daemon can answer "what does this thread say" when the session
    // reports a commit. The agent is never told the reply is its to send.
    app.inner.write().await.resolve_runs.insert(
        number,
        crate::state::ResolveRun {
            session,
            plan: plan.clone(),
        },
    );
    app.notify().await;
    Ok(Json(json!({ "session": session, "threads": plan.threads.len() })))
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

    let all: Vec<&str> = fresh
        .items
        .iter()
        .flat_map(|t| t.comments.first().map(|c| c.author.as_str()))
        .filter(|a| *a != fresh.viewer)
        .collect();
    let open: Vec<&str> = fresh
        .items
        .iter()
        .filter(|t| !t.is_resolved)
        .flat_map(|t| t.comments.first().map(|c| c.author.as_str()))
        .filter(|a| *a != fresh.viewer)
        .collect();

    let mut asked = Vec::new();
    let mut failed = Vec::new();
    for login in crate::forge::ready_to_rerequest(&all, &open) {
        let (f, at, who) = (forge.clone(), at.clone(), login.to_string());
        match tokio::task::spawn_blocking(move || f.rerequest(&at, number, &who)).await {
            Ok(Ok(())) => asked.push(login.to_string()),
            Ok(Err(e)) => failed.push(format!("{login}: {e:#}")),
            Err(e) => failed.push(format!("{login}: {e}")),
        }
    }
    app.notify().await;
    Ok(Json(json!({ "rerequested": asked, "failed": failed })))
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
        "main" => {
            if let Some(held) = {
                let inner = app.inner.read().await;
                inner.workspaces.get(MAIN).and_then(|w| w.occupant)
            } {
                return Err(ApiError(anyhow::anyhow!(
                    "a session already holds main ({}); end it before moving the checkout",
                    &held.to_string()[..8]
                )));
            }
            let main = app.cfg.main_checkout.clone();
            let branch = pr.head_ref.clone();
            let path = main.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                if !crate::git::is_clean(&path)? {
                    anyhow::bail!(
                        "the main checkout has uncommitted changes; commit or stash them first"
                    );
                }
                crate::git::switch_branch(&path, &branch)
            })
            .await
            .map_err(|e| anyhow::anyhow!("switch task failed: {e}"))??;
            let _ = app.reconcile(MAIN).await;
            MAIN.to_string()
        }
        other => return Err(ApiError(anyhow::anyhow!("unknown place {other}"))),
    };

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
    if !pr.needs_you {
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

    // The worktree has to exist before the run can be spawned into it.
    let workspace = spawn::ensure_pr_worktree(&app, number, &pr.head_ref).await?;

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

        evaluate(&GuardInput {
            pr: &pr,
            automation: inner.automation.get(number),
            viewer: pr.head_owner.as_deref().unwrap_or_default(),
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
        inner.automation.by_pr.insert(
            number,
            PrAutomation::Running {
                session,
                started: std::time::SystemTime::now(),
            },
        );
        let _ = crate::store::save_automation(&inner.automation);
    }

    // Recording exhaustion belongs to whoever sees the session end.
    watch_fix_pr(app.clone(), number, session);
    app.notify().await;
    Ok(Json(json!({ "session": session })))
}

fn watch_fix_pr(app: Arc<AppState>, number: u64, session: uuid::Uuid) {
    use crate::fix_pr::{ended_red, PrAutomation};
    tokio::spawn(async move {
        let handle = {
            let inner = app.inner.read().await;
            inner.sessions.get(&session).and_then(|s| s.pty.clone())
        };
        if let Some(h) = handle {
            h.wait().await;
        }

        let mut inner = app.inner.write().await;

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
    let _ = crate::git::fetch_upstream(&app.cfg.main_checkout, &app.cfg.upstream_ref);

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
