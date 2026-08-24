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
    // A worktree the daemon did not name only reveals its path here, so this is
    // where it gets adopted.
    if let Some(cwd) = payload.cwd.as_deref() {
        // Resolved, because it is compared against `worktrees_dir` — which comes
        // from `main_checkout` and is resolved at parse — and because the path
        // registered here is what `workspace_for_path` matches resolved hook
        // paths against. An agent reporting `/var/…` where the config resolved to
        // `/private/var/…` would be adopted as no worktree at all, or as one whose
        // edits never attribute. Falls back to what was reported: a cwd that does
        // not resolve is not a reason to drop the adoption.
        let path = PathBuf::from(cwd);
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if let Some(name) = crate::spawn::worktree_name_of(&path, &app.cfg.worktrees_dir()) {
            let branch = crate::git::current_branch(&path).ok();
            app.register_worktree(&name, path.clone(), branch).await;
            let mut inner = app.inner.write().await;
            if let Some(s) = inner.sessions.get_mut(&id) {
                s.workspace = name;
            }
        }
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
            if let Some(cwd) = payload.cwd.as_deref() {
                s.cwd = PathBuf::from(cwd);
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
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            // Sending the next prompt is one of exactly two ways YourTurn
            // clears (§2). Both change the underlying reality.
            s.set_state(State::Working);
        }
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
                let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                let owner = app.workspace_for_path(&real).await;
                let mut inner = app.inner.write().await;
                if let Some(s) = inner.sessions.get_mut(&id) {
                    s.dirty_paths.insert(real);
                }
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
            if matches!(s.state, State::YourTurn { .. }) {
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

    {
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            let want = crate::health::at_rest(build_failure.as_deref());
            // Re-stamping `YourTurn` would restart the waiting clock on a session
            // that was already waiting, and that clock is what the rail sorts on.
            let already_waiting =
                matches!(want, State::YourTurn { .. }) && matches!(s.state, State::YourTurn { .. });
            if !already_waiting {
                s.set_state(want);
            }
        }
    }

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
    {
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            crate::store::pin_transcript(s.id, &s.cwd, &mut s.transcript_path);
        }
    }

    let found = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .get(&id)
            .and_then(|s| crate::store::ai_title(s.id, &s.cwd, s.transcript_path.as_deref()))
    };
    let Some(title) = found else { return };
    let mut inner = app.inner.write().await;
    if let Some(s) = inner.sessions.get_mut(&id) {
        if s.title.as_deref() != Some(title.as_str()) {
            s.title = Some(title);
        }
    }
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
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            s.set_state(State::Error { message });
        }
    }
    app.notify().await;
    ok()
}

pub async fn session_end(
    AxState(app): AxState<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<HookPayload>,
) -> HookResult {
    let Some(id) = session_of(&headers, &payload) else {
        return ok();
    };
    let workspace = {
        let mut inner = app.inner.write().await;
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
    let Some(path) = payload
        .tool_input
        .as_ref()
        .and_then(|v| v.get("file_path"))
        .and_then(|v| v.as_str())
    else {
        return ok();
    };

    match app.claim_stale_warning(id, &PathBuf::from(path)).await {
        Some(reason) => (
            StatusCode::OK,
            Json(json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                }
            })),
        ),
        // An empty body leaves the decision alone; this hook is otherwise an
        // observer and must never gate an ordinary edit.
        None => ok(),
    }
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
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            s.boundary_violations.push(what);
        }
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
/// Blast-radius guards for `git push` (§8), kept as a real script in the repo
/// rather than a string literal so it can be read, tested and diffed.
const PUSH_GUARD: &str = include_str!("../guards/push.py");

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
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn write_push_guard() -> Result<PathBuf> {
    let path = Config::config_dir()?.join("guard-push.py");
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, PUSH_GUARD)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(path)
}

pub fn write_settings(port: u16, tracker: Option<&str>) -> Result<PathBuf> {
    let guard = write_push_guard()?;
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

    let settings = json!({
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
                matched("Edit|Write", "pre-edit"),
                // Additive to the repo's own `pre-bash`: any hook exiting 2
                // blocks, so both sets of rules apply (§11).
                { "matcher": "Bash", "hooks": [{
                    "type": "command",
                    "command": sh_quote(&guard.to_string_lossy()),
                    "timeout": 5,
                }]},
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

    let path = Config::hooks_settings_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, serde_json::to_string_pretty(&settings)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let path = write_settings(7777, Some("shortcut")).expect("write settings");
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

        // The guard is a shell string, so its path must arrive quoted whatever
        // the config dir is called. Unquoted, the macOS default's space would
        // split it and the guard would silently not run.
        let guard = v["hooks"]["PreToolUse"]
            .as_array()
            .expect("PreToolUse")
            .iter()
            .find_map(|e| e["hooks"][0]["command"].as_str())
            .expect("a guard command");
        assert!(
            guard.starts_with('\'') && guard.ends_with('\''),
            "the guard path must be shell-quoted, got {guard}"
        );
        assert!(
            std::path::Path::new(guard.trim_matches('\'')).exists(),
            "the quoted path must name the script that was written"
        );

        let _ = std::fs::remove_dir_all(&dir);
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
        let script = dir.join("guard-push.py");
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
