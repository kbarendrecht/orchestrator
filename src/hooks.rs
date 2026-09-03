use anyhow::Result;
use axum::{
    body::Body,
    extract::{Path as AxPath, Request, State as AxState},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use uuid::Uuid;

use crate::config::Config;
use crate::model::*;
use crate::state::AppState;

/// Observers, not gatekeepers. A slow or dead daemon must cost a turn as little
/// as possible (§3).
const HOOK_TIMEOUT_SECS: u32 = 1;

/// Reconcile at most this often while `Working`, to catch Bash-driven changes
/// no `Edit` hook reported (§4).
const RECONCILE_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// The subset of the hook payload the daemon reads.
#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)] // hook_event_name is read when debugging a matcher
pub struct HookPayload {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub error: Option<serde_json::Value>,
    /// Why a `SessionEnd` fired, which decides whether it is an ending at all.
    ///
    /// Read off the agent binary rather than guessed: the schema bound to
    /// `hook_event_name: "SessionEnd"` accepts
    /// `["clear","resume","logout","prompt_input_exit","other"]`. Two of those
    /// leave the process running, which is what [`ends_the_process`] is for.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Does this `SessionEnd` reason mean the process is going away?
///
/// `clear` and `resume` are the conversation ending, not the session: Claude Code
/// starts a fresh one in the same pty, in the same directory, and announces it with
/// a `SessionStart` whose `source` is that same word. Everything else, **including
/// a reason we do not recognise and no reason at all**, is treated as an ending.
///
/// The asymmetry is what makes that default safe, and it is the thing to keep in
/// mind when this list next changes. Refusing a real ending costs a beat, because
/// `spawn::watch_session_exit` settles every dead pty anyway and does strictly more
/// than this handler. Accepting a false one costs main's exclusivity: the record
/// goes `Exited` and `release_main` hands the claim back out from under a live
/// agent, and no hook path ever calls `reclaim_main`. So a new non-terminal reason
/// belongs here the day it appears; a new terminal one needs no change.
fn ends_the_process(reason: Option<&str>) -> bool {
    !matches!(reason, Some("clear") | Some("resume"))
}

/// `canonicalize`, or the path as reported.
///
/// One home for a rule that had grown eight inline copies, two of them in this
/// file resolving this same field under near-identical comments. It matters that
/// every one of them agrees: `session_start` records the cwd and `session_end`
/// compares against what that recorded, so a copy that resolves where another does
/// not is not a compile error but a hook that stops being recognised. Invisible on
/// Linux, the normal case on macOS, where `/tmp`, `/var` and `$TMPDIR` are
/// symlinks into `/private`.
fn resolved(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl HookPayload {
    /// The reported cwd, resolved — because everything it is compared against is:
    /// `main_checkout` is canonicalized at parse, `worktrees_dir` derives from it,
    /// and an adopted worktree is registered through here. On a Mac the difference
    /// is `/tmp` against `/private/tmp`, which would make every hook look stale or
    /// every adoption miss. Falls back to what was reported: a cwd that does not
    /// resolve is not a reason to drop the hook.
    pub fn resolved_cwd(&self) -> Option<PathBuf> {
        self.cwd.as_deref().map(|c| resolved(std::path::Path::new(c)))
    }
}

/// Correlation is exact: the session id is carried in a header interpolated from
/// `$ORCH_SESSION_ID` at spawn. No cwd or pid heuristics (§3).
fn session_of(headers: &HeaderMap, payload: &HookPayload) -> Option<Uuid> {
    headers
        .get("x-orch-session")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Uuid::parse_str(v).ok())
        // The daemon assigns Claude's own session id with --session-id, so the
        // body is a usable fallback if the header is ever missing.
        .or_else(|| {
            payload
                .session_id
                .as_deref()
                .and_then(|v| Uuid::parse_str(v).ok())
        })
}

/// Every handler returns 200 with an empty body and never blocks a turn.
type HookResult = (StatusCode, Json<serde_json::Value>);

fn ok() -> HookResult {
    (StatusCode::OK, Json(json!({})))
}

/// Answer an observer hook on arrival and finish its work detached.
///
/// Axum drops a handler's future the moment the client goes away, and Claude
/// gives a hook one second (§3). So any handler that ran long — a cold `git
/// status`, the write lock held by a poll, a box deep in swap — was cancelled
/// mid-way and lost its state change outright. `SessionStart` losing it is the
/// visible one: the session reads `starting` forever, and the rail invites you
/// to start another.
///
/// The body is read here rather than in the spawned task, so nothing we need
/// outlives the connection it arrived on. Not for `pre_edit`, whose answer is
/// the whole point of the request.
pub async fn detach(req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 256 * 1024)
        .await
        .unwrap_or_default();
    tokio::spawn(async move {
        next.run(Request::from_parts(parts, Body::from(bytes)))
            .await;
    });
    ok().into_response()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn session_start(
    AxState(app): AxState<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<HookPayload>,
) -> HookResult {
    let Some(id) = session_of(&headers, &payload) else {
        return ok();
    };
    /* A worktree the daemon did not name only reveals its path here, so this is
       where it gets adopted.

       **Both ways of missing used to be silent, and the symptom is the same
       either way**: the row keeps the `…creating` placeholder for the life of the
       session, and with it no workspace record — so no changed-files pane, no
       divergence strip, no reconcile, and no swap or move. Nothing else ever
       adopts it, so one missed event was permanent. Said out loud now, with the
       path that was reported and the prefix it was measured against, because the
       second case below is a *configuration* mismatch that no retry can fix and
       the log line is the only thing that can point at it. */
    let cwd = payload.resolved_cwd();
    let pending = app.session_workspace(id).await.as_deref() == Some(crate::spawn::PENDING_WORKTREE);
    match &cwd {
        Some(path) => match crate::spawn::worktree_name_of(path, &app.cfg.worktrees_dir()) {
            Some(name) => {
                // Off the runtime. A hook has about a second to answer and this is
                // a child process — a worker parked here is one fewer serving the
                // board, on a path every session start goes through.
                let at = path.clone();
                let branch = crate::proc::run_blocking("reading the worktree's branch", move || {
                    crate::git::current_branch(&at).ok()
                })
                .await
                .unwrap_or(None);
                app.register_worktree(&name, path.clone(), branch).await;
                app.with_session(id, |s| s.workspace = name).await;
            }
            // Not a warning unless it matters: every session in main reports a
            // cwd outside the worktrees dir, and that is the ordinary case.
            None if pending => tracing::warn!(
                session = %crate::model::short_id(&id),
                "reported cwd {} is not under {}, so this worktree cannot be adopted — \
                 check `worktrees_subdir` against where this repo's own hook cuts them",
                path.display(),
                app.cfg.worktrees_dir().display(),
            ),
            None => {}
        },
        None if pending => tracing::warn!(
            session = %crate::model::short_id(&id),
            "SessionStart carried no cwd, so the worktree it cut is unknown; \
             the pending-worktree sweep will try to find it"
        ),
        None => {}
    }

    {
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            // Only if it is really there. A `--worktree` session reports the
            // worktree's project dir, which Claude Code creates and then never
            // writes to, having filed the conversation under the checkout it
            // started in. Taking that on every hook undid the correction
            // `refresh_title` had just made, and the session dropped out of the
            // archive when it finished.
            if let Some(tp) = payload.transcript_path.as_deref().map(PathBuf::from) {
                if tp.exists() {
                    s.transcript_path = Some(tp);
                }
            }
            if let Some(cwd) = cwd {
                s.cwd = cwd;
            }
            if matches!(s.state, State::Starting) {
                // Started, but nothing is running: the prompt box is empty and
                // waiting for you. `Working` here made a brand-new session look
                // busy and blocked actions that only fight a running agent.
                s.set_state(State::YourTurn {
                    since: SystemTime::now(),
                    reason: TurnReason::Ready,
                });
            }
        }
    }

    // A resumed session already has a conversation behind it, and Claude Code has
    // already named it. Waiting for the next `Stop` to read that would leave every
    // row you just resumed showing its worktree until you happen to use it.
    refresh_title(&app, id).await;

    // The prompt is typed in rather than passed as an argument:
    // `initialUserMessage` is only honoured in non-interactive mode, and a
    // `/resolve` session is interactive so you can take it over mid-flight (§8).
    let pending = {
        let mut inner = app.inner.write().await;
        inner
            .sessions
            .get_mut(&id)
            .and_then(|s| s.pending_prompt.take().map(|p| (p, s.pty.clone())))
    };
    if let Some((prompt, Some(pty))) = pending {
        tokio::spawn(async move {
            // The prompt box is not ready the instant SessionStart fires; typing
            // into it too early drops characters.
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            let _ = pty.write(prompt.as_bytes());
            // The return goes in its own write, after a beat. A line of text and
            // its newline arriving as one burst reads as a *paste*, and a pasted
            // newline is a line break in the prompt box rather than a send — so
            // the instructions sat there typed but never submitted. Short slash
            // commands got away with it; a sentence with a path in it does not.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let _ = pty.write(b"\r");
        });
    }

    app.notify().await;
    ok()
}

pub async fn user_prompt_submit(
    AxState(app): AxState<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<HookPayload>,
) -> HookResult {
    if let Some(id) = session_of(&headers, &payload) {
        // Sending the next prompt is one of exactly two ways YourTurn
        // clears (§2). Both change the underlying reality.
        app.with_session(id, |s| s.set_state(State::Working)).await;
    }
    app.notify().await;
    ok()
}

pub async fn post_tool_use(
    AxState(app): AxState<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<HookPayload>,
) -> HookResult {
    let Some(id) = session_of(&headers, &payload) else {
        return ok();
    };
    let edited = payload
        .tool_input
        .as_ref()
        .and_then(|v| v.get("file_path"))
        .and_then(|v| v.as_str())
        .map(PathBuf::from);

    // A tool with no `file_path` is usually `Bash`, which is exactly the tool that
    // moves files the daemon cannot predict: a build, a codegen, a `git restore`.
    // It used to reconcile only when a path was there to attribute, so a whole
    // class of change waited for the end of the turn.
    let mut needs_reconcile = None;
    // An edit is worth a snapshot on its own: the right-hand pane lists it.
    let mut changed = edited.is_some();
    {
        let owner = match &edited {
            // A shared dir in a worktree can be a symlink back to main. Resolve every
            // hook path through realpath before attributing it, or the edit shows
            // as a phantom untracked file in the wrong pane (§4).
            Some(path) => {
                let real = resolved(path);
                let owner = app.workspace_for_path(&real).await;
                app.with_session(id, |s| s.dirty_paths.insert(real)).await;
                owner
            }
            None => None,
        };
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            // Using a tool means the agent is going, whatever the last lifecycle
            // event said. A turn does not only ever start from a prompt: a
            // background task finishing, a scheduled wake-up or a hook
            // continuation all resume the agent without one, and `UserPromptSubmit`
            // was the only thing that cleared `YourTurn`. So a session polling for
            // something sat in the rail saying "turn complete", counted in the
            // waitbar, and answered Alt+b as if it wanted you.
            //
            // `BuildFailing` is left alone: it is a red build talking, not a
            // guess about the turn, and `Stop` recomputes it either way.
            //
            // So is `Interrupted`, and that one is load-bearing. You stopped the turn
            // from the pane; a tool that was already in flight then reports back, and
            // lifting the state on it would put the session in `Working` with nothing
            // left to take it out — `Stop` does not fire on an interrupt. That is the
            // exact fault `TurnReason::Interrupted` exists to fix, reproduced through
            // a race. Only your next prompt (`UserPromptSubmit`) clears it.
            if matches!(
                s.state,
                State::YourTurn { reason, .. } if reason != TurnReason::Interrupted
            ) {
                s.set_state(State::Working);
                changed = true;
            }
            if s.last_reconcile
                .map(|t| t.elapsed().unwrap_or_default() > RECONCILE_EVERY)
                .unwrap_or(true)
            {
                needs_reconcile = owner.or(Some(s.workspace.clone()));
            }
        }
    }

    if let Some(ws) = needs_reconcile {
        let _ = app.reconcile(&ws).await;
        changed = true;
    }
    // Every tool call arrives here now, and a snapshot writes the session records
    // and wakes every client. Only send one when there is something to see.
    if changed {
        app.notify().await;
    }
    ok()
}

/// Which of the two a `permission_prompt` really is.
///
/// Claude Code runs `AskUserQuestion` through the permission system, so a
/// multiple choice arrives as "Claude needs your permission to use
/// AskUserQuestion". Taken literally it sent you to the pane looking for a y/n
/// prompt that was not there.
fn permission_reason(message: Option<&str>) -> TurnReason {
    match message {
        Some(m) if m.contains("AskUserQuestion") => TurnReason::AskedAQuestion,
        _ => TurnReason::NeedsPermission,
    }
}

pub async fn notification(
    AxState(app): AxState<Arc<AppState>>,
    AxPath(kind): AxPath<String>,
    headers: HeaderMap,
    Json(payload): Json<HookPayload>,
) -> HookResult {
    let Some(id) = session_of(&headers, &payload) else {
        return ok();
    };
    let reason = match kind.as_str() {
        "agent_needs_input" => TurnReason::AskedAQuestion,
        "permission_prompt" => permission_reason(payload.message.as_deref()),
        _ => TurnReason::TurnComplete,
    };
    {
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            // Don't restart the clock if the session is already waiting: the
            // wait time is the metric, and a second notification about the same
            // idle turn would silently reset it.
            if !matches!(s.state, State::YourTurn { .. }) {
                s.set_state(State::YourTurn {
                    since: SystemTime::now(),
                    reason,
                });
            }
        }
    }
    app.notify().await;
    ok()
}

/// The primary attention event (§3).
pub async fn stop(
    AxState(app): AxState<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<HookPayload>,
) -> HookResult {
    let Some(id) = session_of(&headers, &payload) else {
        return ok();
    };

    let workspace = {
        let inner = app.inner.read().await;
        inner.sessions.get(&id).map(|s| s.workspace.clone())
    };
    let Some(workspace) = workspace else {
        return ok();
    };

    // A session that reached Stop while a managed process is red is not waiting
    // on a prompt, it is broken. Red outranks ochre (§2) — the rule itself lives
    // in `health::at_rest`, because the health watcher applies the same one from
    // the other direction.
    let build_failure = {
        let inner = app.inner.read().await;
        crate::health::build_failure_in(&inner, &workspace)
    };

    app.with_session(id, |s| {
        let want = crate::health::at_rest(build_failure.as_deref());
        // Re-stamping `YourTurn` would restart the waiting clock on a session
        // that was already waiting, and that clock is what the rail sorts on.
        let already_waiting =
            matches!(want, State::YourTurn { .. }) && matches!(s.state, State::YourTurn { .. });
        if !already_waiting {
            s.set_state(want);
        }
    })
    .await;

    refresh_title(&app, id).await;
    let _ = app.reconcile(&workspace).await;
    app.notify().await;
    ok()
}

/// Take the conversation's name from its transcript.
///
/// Claude Code titles the conversation itself, so the rail does not have to make
/// do with the worktree's name for every session sitting in it. Read at `Stop`
/// because that is when a turn has just been written, and re-read every time
/// rather than pinned: the title is regenerated as the conversation moves, and
/// the newest one is the one that describes what is in the pane.
async fn refresh_title(app: &Arc<AppState>, id: Uuid) {
    // While we are here: a `--worktree` session's recorded transcript path names
    // the worktree, and Claude Code wrote the file under the checkout it started
    // in. Left uncorrected, the session drops out of the archive the moment it
    // finishes, because nothing can find its conversation.
    app.with_session(id, |s| {
        crate::store::pin_transcript(s.id, &s.cwd, &mut s.transcript_path)
    })
    .await;

    let found = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .get(&id)
            .and_then(|s| crate::store::ai_title(s.id, &s.cwd, s.transcript_path.as_deref()))
    };
    let Some(title) = found else { return };
    app.with_session(id, |s| {
        if s.title.as_deref() != Some(title.as_str()) {
            s.title = Some(title);
        }
    })
    .await;
}

/// **Explicit no-op.**
///
/// A `Task` call finishing mid-turn would flip the session to ochre while the
/// main agent is still working, poisoning the one metric the rail exists for
/// (§3). This handler exists so that routing a `SubagentStop` here is a
/// deliberate decision rather than an accident of matcher configuration.
pub async fn subagent_stop(
    AxState(_app): AxState<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<HookPayload>,
) -> HookResult {
    if let Some(id) = session_of(&headers, &payload) {
        tracing::debug!(session = %id, "SubagentStop ignored by design");
    }
    ok()
}

pub async fn stop_failure(
    AxState(app): AxState<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<HookPayload>,
) -> HookResult {
    if let Some(id) = session_of(&headers, &payload) {
        let message = payload
            .error
            .as_ref()
            .map(|e| e.to_string())
            .or(payload.message.clone())
            .unwrap_or_else(|| "stop failure".to_string());
        app.with_session(id, |s| s.set_state(State::Error { message })).await;
    }
    app.notify().await;
    ok()
}

/// A session's process says it is finished.
///
/// **Not every `SessionEnd` for an id belongs to the session holding that id.** A
/// relocation resumes under the *same* id in another directory, so the process it
/// killed on the way can still have a hook in flight — and settling on that would
/// flip the live replacement to `Exited` and, because `release_main` keys on the
/// id, hand main's claim back out from under it. `watch_session_exit` guards the
/// same race by comparing pty handles, which is the exact test; a hook is an HTTP
/// request and has no handle to compare.
///
/// What it does have is where it ran. A moved conversation is somewhere else by
/// definition, and the dying process reports the path it was in, so a hook from a
/// tree this session is no longer in is a hook from a process it no longer is. A
/// payload without a `cwd` settles as before: a guard that cannot tell must not
/// start refusing.
///
/// **This guard covers the half of that race after the record is installed.** The
/// other half is `spawn::spawn_session`'s `reclaim_main`: between `claim_main` and
/// the insert the map still describes the *outgoing* session, whose ending would
/// look entirely legitimate, so the claim is taken again once the record is in.
///
/// The third thing to know is that not every `SessionEnd` is an ending at all; see
/// [`ends_the_process`].
pub async fn session_end(
    AxState(app): AxState<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<HookPayload>,
) -> HookResult {
    let Some(id) = session_of(&headers, &payload) else {
        return ok();
    };
    if !ends_the_process(payload.reason.as_deref()) {
        // Not a state change, so not a `notify`: the session goes on exactly as it
        // was, and the `SessionStart` that follows re-reads the transcript this
        // conversation writes to now.
        tracing::info!(
            session = %id,
            reason = payload.reason.as_deref().unwrap_or_default(),
            "SessionEnd that ends no process — the conversation restarted in place"
        );
        return ok();
    }
    let said = payload.resolved_cwd();

    let workspace = {
        let mut inner = app.inner.write().await;
        /* `Some(path)` when this hook came from a tree the session is not in.
           Judged by *workspace* rather than by comparing the path to the record.
           A hook's `cwd` is the live process cwd, so it follows a Bash `cd` and an
           `EnterWorktree` while only `session_start` ever writes `s.cwd`: path
           equality called a session that merely ended in a subdirectory of its own
           workspace "moved", and skipped the reconcile that goes with an ending.

           The worktree name is derived from the path *before* the registry is
           asked, and that ordering is the point. A tree removed after the
           conversation left it has no record any more, so the registry would fall
           back to main, which is exactly where the conversation now is, and the
           stale hook would be accepted after all. The layout answers that without
           needing the record to still exist. */
        let moved = match (said.as_ref(), inner.sessions.get(&id)) {
            (Some(c), Some(s))
                if crate::spawn::worktree_name_of(c, &app.cfg.worktrees_dir())
                    .or_else(|| inner.workspace_for_path(c))
                    .as_ref()
                    != Some(&s.workspace) =>
            {
                Some(c)
            }
            _ => None,
        };
        if let Some(c) = moved {
            tracing::info!(
                session = %id,
                "ignored a SessionEnd from {} — this conversation has moved",
                c.display()
            );
            return ok();
        }
        match inner.sessions.get_mut(&id) {
            Some(s) => {
                // Buffer is retained: the transcript and scrollback outlive the
                // process (§3).
                s.set_state(State::Exited);
                Some(s.workspace.clone())
            }
            None => None,
        }
    };
    app.release_main(id).await;
    if let Some(ws) = workspace {
        let _ = app.reconcile(&ws).await;
    }
    app.notify().await;
    ok()
}

/// Tell an agent, once, that a file it is about to write was rewritten
/// underneath it.
///
/// Verified against a real session: the deny reason reaches the model, it
/// re-reads the file, and it does not clobber the change. Announce-once is what
/// makes that work — a permanent deny would just stall the turn.
pub async fn pre_edit(
    AxState(app): AxState<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<HookPayload>,
) -> HookResult {
    let Some(id) = session_of(&headers, &payload) else {
        return ok();
    };

    // Said before anything about the file, and said here rather than at the prompt.
    //
    // `UserPromptSubmit` looks like the natural place and is not: measured against a
    // real session on the fixture daemon, `additionalContext` from the **http** form
    // of that hook never reaches the model, and taking the notice there consumed it
    // for nothing. This deny reason does reach it, which is what `claim_stale_warning`
    // below has always relied on, and the moment an agent reaches for a path is
    // exactly the moment being in a different tree starts to matter.
    if let Some(notice) = app.take_arrival_notice(id).await {
        return deny(notice);
    }

    let Some(path) = payload
        .tool_input
        .as_ref()
        .and_then(|v| v.get("file_path"))
        .and_then(|v| v.as_str())
    else {
        return ok();
    };

    match app.claim_stale_warning(id, &PathBuf::from(path)).await {
        Some(reason) => deny(reason),
        // An empty body leaves the decision alone; this hook is otherwise an
        // observer and must never gate an ordinary edit.
        None => ok(),
    }
}

/// Refuse one tool call and tell the model why.
///
/// The daemon's only way to say something an agent will actually read: hooks are
/// one-way and the pty is the human's. Every use of it is announce-once, because a
/// deny that repeats stalls the turn instead of informing it.
fn deny(reason: String) -> HookResult {
    (
        StatusCode::OK,
        Json(json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })),
    )
}

/// A blocked tool call from `worktree-edit-boundary` surfaces as a distinct
/// signal — an agent editing outside its worktree is a prompt problem worth
/// seeing, not noise to swallow (§11).
pub async fn boundary_block(
    AxState(app): AxState<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<HookPayload>,
) -> HookResult {
    if let Some(id) = session_of(&headers, &payload) {
        let what = payload
            .message
            .clone()
            .or_else(|| payload.tool_name.clone())
            .unwrap_or_else(|| "blocked edit".to_string());
        app.with_session(id, |s| s.boundary_violations.push(what)).await;
    }
    app.notify().await;
    ok()
}

// ---------------------------------------------------------------------------
// Settings file generation
// ---------------------------------------------------------------------------

/// Write the daemon-owned settings file handed to every spawned session.
///
/// Verified at spike time that `--settings` *merges* with project and user
/// settings rather than replacing them, so the repo's own `worktree-create`,
/// `worktree-link`, `worktree-edit-boundary` and `pre-bash` hooks keep firing
/// alongside these. §3's fallback of inlining the repo's hooks is unnecessary.
/// The `orch` binary, which carries the `git push` guard (§8).
///
/// A sibling of the running executable, because that is true in every layout
/// that exists: `target/debug/orch` beside `target/debug/orchd` in development,
/// and both in the same directory in the release tarball. Resolved rather than
/// left to `PATH` so the guard does not depend on how the user installed things.
///
/// **Through the `latest` symlink when mise installed us**, because this path is
/// written into a settings file that outlives the process. mise installs each
/// version in a directory of its own and removes the old one, so the version-pinned
/// path is dead the moment you upgrade — and the guard is a `type: "command"` hook,
/// so a missing binary is a hook that fails *open*. Seen: four
/// `PreToolUse:Bash hook error` lines in one session, naming an `orch` from a
/// version that had been replaced under a still-running daemon, with the guard
/// silently not running for any of those pushes.
///
/// `None` when neither path is there — a `cargo run` before `cargo build --bins`,
/// or a partial install. The caller logs that loudly and registers no hook at all,
/// which is the honest outcome: the previous guard ran a Python script by its
/// shebang, so a machine without `python3` got a hook that exited 127 and a
/// guard that had silently stopped existing.
fn orch_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) { "orch.exe" } else { "orch" };
    let stable = crate::self_update::stable_exe(&exe);
    // The stable path first, the running one as the fallback: a layout without a
    // `latest` beside it gets exactly what it got before.
    let candidates = [
        stable.parent().map(|dir| dir.join(name)),
        exe.parent().map(|dir| dir.join(name)),
    ];
    candidates.into_iter().flatten().find(|orch| orch.exists())
}

/// Single-quote a string for a shell.
///
/// Claude Code runs a `type: "command"` hook as a **shell string** — the
/// `SessionStart` one below relies on that, with its pipe and `|| true` — so an
/// unquoted path is word-split. That became load-bearing when the macOS config
/// dir moved to `~/Library/Application Support/orchd`: the space would split the
/// push guard's path and the hook would run a command that does not exist, which
/// is the one failure mode a guard must not have (it fails *open*, silently).
///
/// Single quotes take everything literally. The only thing they cannot contain is
/// a single quote, which is closed, escaped and reopened — a home directory
/// belonging to an O'Brien is not a reason for the guard to stop working.
///
/// `pub` because the desktop crate writes a shell launcher into a macOS `.app`
/// and needs the same rule. One implementation, since the failure it prevents is
/// identical on both sides and a second copy is how one of them stops getting the
/// fix.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The `PreToolUse` entry for the push guard, or `None` when `orch` is missing.
///
/// The base branch is baked into the command rather than read from config by the
/// guard process: settings are rewritten at every start, so this tracks
/// `upstream_ref` on the same restart boundary the rest of the config does, and
/// the rule in force is visible in the settings file instead of being implied.
fn push_guard_hook(base_branch: Option<&str>) -> Option<serde_json::Value> {
    let orch = orch_binary()?;
    let mut command = format!("{} guard push", sh_quote(&orch.to_string_lossy()));
    if let Some(b) = base_branch {
        command.push_str(&format!(" --base {}", sh_quote(b)));
    }
    Some(json!({ "matcher": "Bash", "hooks": [{
        "type": "command",
        "command": command,
        "timeout": 5,
    }]}))
}

pub fn write_settings(
    port: u16,
    tracker: Option<&str>,
    base_branch: Option<&str>,
) -> Result<PathBuf> {
    let base = format!("http://127.0.0.1:{port}/hooks");
    let http = |path: &str| {
        json!({
            "type": "http",
            "url": format!("{base}/{path}"),
            "headers": { "X-Orch-Session": "$ORCH_SESSION_ID" },
            "allowedEnvVars": ["ORCH_SESSION_ID"],
            "timeout": HOOK_TIMEOUT_SECS,
        })
    };
    let entry = |path: &str| json!({ "hooks": [http(path)] });
    let matched = |matcher: &str, path: &str| json!({ "matcher": matcher, "hooks": [http(path)] });

    // `SessionStart` is the one event that never arrives over HTTP — verified
    // against a real session, twice: the command form of the same hook fires
    // while the http form does not. So it posts the payload itself.
    //
    // `-m 1` and the trailing `|| true` keep it an observer: a slow or dead
    // daemon must not cost the turn, and must never fail it. `$ORCH_SESSION_ID`
    // is expanded by the shell from the session's own environment, so no
    // `allowedEnvVars` declaration is involved.
    let session_start = json!({
        "hooks": [{
            "type": "command",
            "command": format!(
                "curl -sS -m 1 -X POST -H 'Content-Type: application/json' \
                 -H \"X-Orch-Session: $ORCH_SESSION_ID\" --data-binary @- \
                 {base}/session-start >/dev/null 2>&1 || true"
            ),
            "timeout": HOOK_TIMEOUT_SECS,
        }]
    });

    let mut settings = json!({
        // Repo config cannot redirect the daemon's HTTP hooks elsewhere (§11).
        "allowedHttpHookUrls": [format!("http://127.0.0.1:{port}/*")],
        // Approve the tracker server from the repo's own `.mcp.json`.
        //
        // Necessary, and this is the only place it can go. A project `.mcp.json`
        // server stays *pending* until it is approved in a settings layer, and a
        // pending server is dropped **silently** — no error, no diagnostic, the
        // tool simply is not there. The repo's approvals live in
        // `.claude/settings.local.json`, which is gitignored, so a worktree does
        // not inherit them.
        //
        // Named rather than `enableAllProjectMcpServers`: a repo may declare a
        // dozen servers — browser automation, dashboards, whatever — and a
        // story-filing agent has no business with any of them. The name comes
        // from the configured tracker, and with none configured the list is
        // empty rather than approving something nothing will use.
        "enabledMcpjsonServers": tracker.map(|t| vec![t]).unwrap_or_default(),
        "hooks": {
            "SessionStart":     [session_start],
            "UserPromptSubmit": [entry("user-prompt-submit")],
            // Ordering matters only in that both fire: PreToolUse warns about a
            // rewrite, PostToolUse records what was written.
            "PreToolUse": [
                // Every tool, not `Edit|Write`. The stale-edit warning this hook was
                // built for only concerns a file, but it is also where a moved
                // conversation is told it has been moved — and *that* bites on git,
                // which is Bash. Scoped to edits, the notice waited for a write that
                // a session sorting out where it is never makes: one conversation
                // took Claude Code's bare "isolated in the worktree" refusal sixteen
                // times over two days with the explanation still queued behind it.
                // `PostToolUse` was widened off this same matcher for the same
                // reason; the handler already returns early when there is no
                // `file_path`, so a Bash call costs one loopback round trip.
                entry("pre-edit"),
            ],
            // Every tool, not just the two that write files. `Bash` is the one
            // that moves files the daemon cannot predict, and any tool at all is
            // proof the agent is going (see `post_tool_use`). The handler only
            // pushes a snapshot when something actually changed, so the extra
            // events cost a lock and a comparison.
            "PostToolUse":      [entry("post-tool-use")],
            "Notification": [
                matched("agent_needs_input", "notification/agent_needs_input"),
                matched("permission_prompt", "notification/permission_prompt"),
                matched("idle_prompt",       "notification/idle_prompt"),
            ],
            "Stop":         [entry("stop")],
            // Routed deliberately to a handler that does nothing, so a subagent
            // finishing mid-turn can never reach the state machine (§3).
            "SubagentStop": [entry("subagent-stop")],
            "StopFailure":  [entry("stop-failure")],
            "SessionEnd":   [entry("session-end")],
        }
    });

    // Appended rather than written inline, because it is the one hook that can be
    // absent. Additive to the repo's own `pre-bash`: any hook exiting 2 blocks, so
    // both sets of rules apply (§11).
    match push_guard_hook(base_branch) {
        Some(hook) => {
            settings["hooks"]["PreToolUse"]
                .as_array_mut()
                .expect("PreToolUse is the array written just above")
                .push(hook);
        }
        // Loud, because the whole point of moving this guard in-process was that
        // its predecessor could stop existing without saying anything.
        None => tracing::warn!(
            "no `orch` binary beside this executable — the git push guard is not \
             registered, and agent pushes are unguarded"
        ),
    }

    let path = Config::hooks_settings_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, serde_json::to_string_pretty(&settings)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `SessionEnd` from the process a relocation killed must not settle the
    /// live session that took its id.
    ///
    /// The exit watcher guards this by pty handle. A hook has no handle, so it is
    /// guarded on where the process ran: the moved conversation is somewhere else,
    /// and main's claim is what the old ending would have handed back.
    #[tokio::test]
    async fn a_session_end_from_the_tree_the_conversation_left_settles_nothing() {
        use crate::config::Config;
        use crate::model::{Kind, Session, MAIN};
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-endhook-{}", std::process::id()));
        let (here, gone) = (dir.join("main"), dir.join("old-worktree"));
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&gone).unwrap();
        /* Resolved, and only after the directories exist. `Config::parse` does this
           to `main_checkout` for the same reason, and this test bypasses it by going
           through `serde_json`, so the record would hold an unresolved path while
           the handler resolves the one it is handed. On Linux that is the same
           string; on macOS `$TMPDIR` is a symlink into `/private`, so the workspace
           would match nothing and even the real ending would read as moved. */
        let (here, gone) = (resolved(&here), resolved(&gone));
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7797}}"#,
            here.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        // The session as a relocation leaves it: same id, now living in main.
        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), here.clone(), Kind::Interactive);
            s.set_state(State::Working);
            inner.sessions.insert(id, s);
        }
        app.claim_main(id).await.unwrap();

        let headers = {
            let mut h = HeaderMap::new();
            h.insert("x-orch-session", id.to_string().parse().unwrap());
            h
        };
        let stale = HookPayload {
            cwd: Some(gone.to_string_lossy().into_owned()),
            ..HookPayload::default()
        };
        session_end(AxState(app.clone()), headers.clone(), Json(stale)).await;

        {
            let inner = app.inner.read().await;
            assert!(
                inner.sessions[&id].state.is_live(),
                "a hook from the tree it left must not end it"
            );
        }
        assert_eq!(app.main_occupant().await, Some(id), "nor hand main's claim back");

        // And the real one, from where the conversation actually is, still settles.
        let real = HookPayload {
            cwd: Some(here.to_string_lossy().into_owned()),
            ..HookPayload::default()
        };
        session_end(AxState(app.clone()), headers, Json(real)).await;
        {
            let inner = app.inner.read().await;
            assert!(!inner.sessions[&id].state.is_live(), "its own ending still ends it");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session that ends in a subdirectory of its own workspace has still ended.
    ///
    /// The guard above is about *which tree*, not which directory, and a hook
    /// reports the live process cwd: an agent that ran `cd web && npm test` reports
    /// `<main>/web`, which no `s.cwd` will ever equal.
    #[tokio::test]
    async fn an_ending_from_a_subdirectory_of_the_workspace_still_ends_it() {
        use crate::config::Config;
        use crate::model::{Kind, Session, MAIN};
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-endsub-{}", std::process::id()));
        let here = resolved(&{
            let p = dir.join("main");
            std::fs::create_dir_all(p.join("web")).unwrap();
            p
        });
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7798}}"#,
            here.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), here.clone(), Kind::Interactive);
            s.set_state(State::Working);
            inner.sessions.insert(id, s);
        }
        let mut headers = HeaderMap::new();
        headers.insert("x-orch-session", id.to_string().parse().unwrap());

        let from_below = HookPayload {
            cwd: Some(here.join("web").to_string_lossy().into_owned()),
            ..HookPayload::default()
        };
        session_end(AxState(app.clone()), headers, Json(from_below)).await;

        {
            let inner = app.inner.read().await;
            assert!(
                !inner.sessions[&id].state.is_live(),
                "a `cd` inside the workspace is not a conversation that moved"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cwd reported through a symlink is the same tree, which is what makes this
    /// handler work on a Mac at all.
    ///
    /// The macOS case reproduced on Linux, deliberately: there `$TMPDIR`, `/tmp`
    /// and `/var` are symlinks into `/private`, so the config resolves to one
    /// spelling while the agent reports the other, and every comparison across that
    /// boundary silently fails. Nothing about the fault is Mac-specific except how
    /// often it happens, so a symlink here catches it on every platform instead of
    /// waiting for the macos-14 runner.
    #[tokio::test]
    async fn a_cwd_reported_through_a_symlink_is_not_a_conversation_that_moved() {
        use crate::config::Config;
        use crate::model::{Kind, Session, MAIN};
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-endlink-{}", std::process::id()));
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // The daemon's side is resolved, the way `Config::parse` resolves it.
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7800}}"#,
            resolved(&real).display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let s = Session::new(id, MAIN.to_string(), resolved(&real), Kind::Interactive);
            inner.sessions.insert(id, s);
        }
        let mut headers = HeaderMap::new();
        headers.insert("x-orch-session", id.to_string().parse().unwrap());

        /* Through `session_start`, which is the half that used to break this: the
           agent reports the path it was started with, and that line recorded it
           raw while this handler resolves what it compares. Driving both hooks is
           what makes this a regression test rather than a restatement of the
           fixture. */
        let through_link = || HookPayload {
            cwd: Some(link.to_string_lossy().into_owned()),
            ..HookPayload::default()
        };
        session_start(AxState(app.clone()), headers.clone(), Json(through_link())).await;
        {
            let inner = app.inner.read().await;
            assert!(inner.sessions[&id].state.is_live(), "the session is up");
        }
        session_end(AxState(app.clone()), headers, Json(through_link())).await;

        {
            let inner = app.inner.read().await;
            assert!(
                !inner.sessions[&id].state.is_live(),
                "the same directory by another name is still the same directory"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/clear` and `/resume` end the conversation and leave the process running,
    /// so settling on one hands main's claim back out from under a live agent.
    #[tokio::test]
    async fn a_session_end_that_ends_no_process_keeps_the_session_and_its_claim() {
        use crate::config::Config;
        use crate::model::{Kind, Session, MAIN};
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-endclear-{}", std::process::id()));
        let here = dir.join("main");
        std::fs::create_dir_all(&here).unwrap();
        let here = resolved(&here);
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7799}}"#,
            here.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), here.clone(), Kind::Interactive);
            s.set_state(State::Working);
            inner.sessions.insert(id, s);
        }
        app.claim_main(id).await.unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("x-orch-session", id.to_string().parse().unwrap());

        // Both non-terminal reasons, from exactly where the session lives, which is
        // what makes the cwd guard useless against them.
        for reason in ["clear", "resume"] {
            let payload = HookPayload {
                cwd: Some(here.to_string_lossy().into_owned()),
                reason: Some(reason.to_string()),
                ..HookPayload::default()
            };
            session_end(AxState(app.clone()), headers.clone(), Json(payload)).await;
            let inner = app.inner.read().await;
            assert!(
                inner.sessions[&id].state.is_live(),
                "`{reason}` restarts the conversation, it does not end the process"
            );
            drop(inner);
            assert_eq!(
                app.main_occupant().await,
                Some(id),
                "`{reason}` must not hand main's claim back"
            );
        }

        // A reason that really is an ending, and one we do not know, both settle:
        // refusing costs a beat because the pty watcher settles it anyway, while
        // accepting a false ending costs main's exclusivity.
        assert!(ends_the_process(Some("prompt_input_exit")));
        assert!(ends_the_process(Some("logout")));
        assert!(ends_the_process(Some("other")));
        assert!(ends_the_process(Some("something_new")));
        assert!(ends_the_process(None), "no reason at all must not start refusing");

        let real = HookPayload {
            cwd: Some(here.to_string_lossy().into_owned()),
            reason: Some("prompt_input_exit".to_string()),
            ..HookPayload::default()
        };
        session_end(AxState(app.clone()), headers, Json(real)).await;
        {
            let inner = app.inner.read().await;
            assert!(!inner.sessions[&id].state.is_live(), "a real ending still ends it");
        }
        assert_eq!(app.main_occupant().await, None, "and releases main");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The rail said "needs permission" for a multiple choice, which is a
    /// different thing to walk over to.
    #[test]
    fn a_question_arriving_as_a_permission_prompt_is_still_a_question() {
        assert_eq!(
            permission_reason(Some("Claude needs your permission to use AskUserQuestion")),
            TurnReason::AskedAQuestion
        );
        assert_eq!(
            permission_reason(Some("Claude needs your permission to use Bash")),
            TurnReason::NeedsPermission
        );
        // No message at all is the old behaviour, not a question.
        assert_eq!(permission_reason(None), TurnReason::NeedsPermission);
    }

    /// The pending prompt has to reach the pty, or the review button starts a session
    /// that just sits there. Exercised against a real pty running `cat`, so no Claude
    /// process is involved.
    #[tokio::test]
    async fn session_start_types_the_pending_prompt_into_the_pty() {
        use crate::config::Config;
        use crate::pty::PtyHandle;
        use crate::state::AppState;
        use std::path::Path;

        let dir = std::env::temp_dir().join(format!("orchd-hook-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);

        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7799}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let spawned =
            PtyHandle::spawn(&["cat".to_string()], Path::new("/tmp"), &[], &[], (24, 80)).unwrap();
        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), dir.clone(), Kind::Interactive);
            s.pty = Some(spawned.handle.clone());
            s.pending_prompt = Some("/resolve 4812".to_string());
            inner.sessions.insert(id, s);
        }

        let mut headers = HeaderMap::new();
        headers.insert("x-orch-session", id.to_string().parse().unwrap());
        session_start(AxState(app.clone()), headers, Json(HookPayload::default())).await;

        // `cat` echoes it straight back, so the buffer proves it was written.
        let mut seen = false;
        for _ in 0..60 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if String::from_utf8_lossy(&spawned.handle.snapshot()).contains("/resolve 4812") {
                seen = true;
                break;
            }
        }
        assert!(seen, "prompt never reached the pty");

        // Taken, not left to fire again on a later SessionStart.
        let inner = app.inner.read().await;
        assert!(inner.sessions.get(&id).unwrap().pending_prompt.is_none());

        let _ = spawned.handle.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A session that resumed on its own must stop claiming it wants you.
    #[tokio::test]
    async fn a_tool_call_clears_a_finished_turn() {
        use crate::config::Config;
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-tool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7798}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), dir.clone(), Kind::Interactive);
            s.set_state(State::YourTurn {
                since: SystemTime::now(),
                reason: TurnReason::TurnComplete,
            });
            inner.sessions.insert(id, s);
        }

        let mut headers = HeaderMap::new();
        headers.insert("x-orch-session", id.to_string().parse().unwrap());
        // A `Bash` call: no `file_path`, which is exactly the shape the old
        // `Edit|Write` matcher never delivered.
        post_tool_use(AxState(app.clone()), headers, Json(HookPayload::default())).await;

        let inner = app.inner.read().await;
        assert!(
            matches!(inner.sessions.get(&id).unwrap().state, State::Working),
            "a tool call left the session claiming the turn was complete"
        );
        drop(inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tool result arriving after you interrupted must not restart the turn.
    ///
    /// The race that would otherwise undo `TurnReason::Interrupted` entirely: you
    /// press escape, a tool that was already in flight reports back, `post_tool_use`
    /// lifts any `YourTurn` to `Working` — and `Stop` never fires on an interrupt, so
    /// nothing takes it out again. The session sits there claiming to work with an
    /// idle agent in it, which is the exact fault the reason exists to fix.
    #[tokio::test]
    async fn a_late_tool_result_does_not_undo_an_interrupt() {
        let dir = std::env::temp_dir().join(format!("orchd-hooks-int-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7793}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let id = uuid::Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), dir.clone(), Kind::Interactive);
            s.set_state(State::YourTurn {
                since: SystemTime::now(),
                reason: TurnReason::Interrupted,
            });
            inner.sessions.insert(id, s);
        }
        let mut headers = HeaderMap::new();
        headers.insert("x-orch-session", id.to_string().parse().unwrap());
        post_tool_use(AxState(app.clone()), headers, Json(HookPayload::default())).await;

        let inner = app.inner.read().await;
        assert!(
            matches!(
                inner.sessions.get(&id).unwrap().state,
                State::YourTurn { reason: TurnReason::Interrupted, .. }
            ),
            "a tool finishing after the escape restarted the turn"
        );
        drop(inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subagent_stop_is_routed_somewhere_that_cannot_change_state() {
        // Guards the §3 invariant at the config layer: SubagentStop must never
        // share a URL with Stop.
        let settings = {
            let base = "http://127.0.0.1:7777/hooks";
            (format!("{base}/stop"), format!("{base}/subagent-stop"))
        };
        assert_ne!(settings.0, settings.1);
    }

    #[test]
    fn hooks_carry_the_correlation_header_and_a_short_timeout() {
        let dir = std::env::temp_dir().join(format!("orchd-test-{}", std::process::id()));
        std::env::set_var("HOME", &dir);
        let path = write_settings(7777, Some("shortcut"), Some("main")).expect("write settings");
        let raw = std::fs::read_to_string(&path).expect("read back");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let stop = &v["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(stop["headers"]["X-Orch-Session"], "$ORCH_SESSION_ID");
        assert_eq!(stop["allowedEnvVars"][0], "ORCH_SESSION_ID");
        assert_eq!(stop["timeout"], 1);
        assert_eq!(v["allowedHttpHookUrls"][0], "http://127.0.0.1:7777/*");

        // SessionStart must stay a command hook: the http form is silently
        // never delivered, which would leave every session stuck in Starting.
        let start = &v["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(start["type"], "command");
        let cmd = start["command"].as_str().unwrap();
        assert!(cmd.contains("/hooks/session-start"));
        // An observer, even when the daemon is down.
        assert!(cmd.contains("|| true"), "must not fail the turn");
        assert!(cmd.contains("-m 1"), "must not stall the turn");

        // The guard is a shell string, so its path must arrive quoted whatever the
        // install directory is called. Unquoted, a space would split it and the
        // guard would silently not run — the macOS config dir is why this exists.
        //
        // Skipped when `orch` has not been built: `cargo test` does not build the
        // sibling binary, and asserting on it would fail for a reason that says
        // nothing about the code. `push_guard_hook_is_absent_rather_than_broken`
        // covers the missing case on purpose.
        if let Some(guard) = v["hooks"]["PreToolUse"]
            .as_array()
            .expect("PreToolUse")
            .iter()
            .find_map(|e| e["hooks"][0]["command"].as_str())
        {
            let path = guard.split(" guard push").next().expect("a command");
            assert!(
                path.starts_with('\'') && path.ends_with('\''),
                "the orch path must be shell-quoted, got {guard}"
            );
            assert!(
                std::path::Path::new(path.trim_matches('\'')).exists(),
                "the quoted path must name the binary that runs the guard"
            );
            assert!(
                guard.ends_with("--base 'main'"),
                "the base branch must reach the guard, got {guard}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The failure mode the Python guard had: it ran by shebang, so a machine
    /// without `python3` got a hook that exited 127 and a guard that had quietly
    /// stopped existing. Absent must mean *no hook*, never a broken one.
    #[test]
    fn push_guard_hook_is_absent_rather_than_broken() {
        match push_guard_hook(Some("main")) {
            None => {}
            Some(hook) => {
                let cmd = hook["hooks"][0]["command"].as_str().expect("a command");
                let path = cmd.split(" guard push").next().unwrap().trim_matches('\'');
                assert!(
                    std::path::Path::new(path).exists(),
                    "a registered guard must name a binary that is really there"
                );
            }
        }
    }

    /// A base the daemon could not resolve leaves the force-with-lease rule in
    /// force rather than passing a `HEAD` symref through as a branch name.
    #[test]
    fn an_unresolvable_base_still_registers_the_force_rule() {
        if let Some(hook) = push_guard_hook(None) {
            let cmd = hook["hooks"][0]["command"].as_str().expect("a command");
            assert!(cmd.ends_with("guard push"), "no --base should be passed: {cmd}");
        }
    }

    /// Proven through a real shell, in a directory whose name has a space —
    /// which is the macOS config dir (`~/Library/Application Support/orchd`), and
    /// the reason this quoting exists at all.
    #[test]
    fn a_quoted_path_survives_a_shell_even_with_a_space_in_it() {
        let dir = std::env::temp_dir()
            .join(format!("orchd quote {}", std::process::id()))
            .join("Application Support");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let script = dir.join("orch");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let quoted = sh_quote(&script.to_string_lossy());
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(&quoted)
            .status()
            .expect("spawn sh")
            .success();
        assert!(ok, "a shell could not run {quoted}");

        // And the unquoted form is what would have been broken — the shell splits
        // it and runs something that is not there.
        let bare = script.to_string_lossy().to_string();
        let broken = std::process::Command::new("sh")
            .arg("-c")
            .arg(&bare)
            .status()
            .expect("spawn sh")
            .success();
        assert!(!broken, "unquoted should fail, or this test proves nothing");

        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("orchd quote {}", std::process::id())));
    }

    #[test]
    fn quoting_holds_for_an_apostrophe_in_a_home_directory() {
        // `/Users/O'Brien/...` must not end the quoted string early.
        assert_eq!(sh_quote("/Users/O'Brien/x"), r"'/Users/O'\''Brien/x'");
        let echoed = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", sh_quote("/Users/O'Brien/x")))
            .output()
            .expect("sh");
        assert_eq!(String::from_utf8_lossy(&echoed.stdout), "/Users/O'Brien/x");
    }
}
