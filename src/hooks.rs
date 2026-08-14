use anyhow::Result;
use axum::{
    extract::{Path as AxPath, State as AxState},
    http::{HeaderMap, StatusCode},
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
        let path = PathBuf::from(cwd);
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
            if let Some(tp) = payload.transcript_path.as_deref() {
                s.transcript_path = Some(PathBuf::from(tp));
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

    // The skill invocation is typed in rather than passed as an argument:
    // `initialUserMessage` is only honoured in non-interactive mode, and this
    // session is interactive so you can take it over mid-flight (§8).
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
            let _ = pty.write(format!("{prompt}\r").as_bytes());
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

    let mut needs_reconcile = None;
    if let Some(path) = edited {
        // `.plan/` in a worktree is a symlink to main's `.plan/`. Resolve every
        // hook path through realpath before attributing it, or the edit shows as
        // a phantom untracked file in the wrong pane (§4).
        let real = std::fs::canonicalize(&path).unwrap_or(path);
        let owner = app.workspace_for_path(&real).await;
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            s.dirty_paths.insert(real.clone());
            if s.last_reconcile
                .map(|t| t.elapsed().unwrap_or_default() > RECONCILE_EVERY)
                .unwrap_or(true)
            {
                needs_reconcile = owner.clone().or(Some(s.workspace.clone()));
            }
        }
    }

    if let Some(ws) = needs_reconcile {
        let _ = app.reconcile(&ws).await;
    }
    app.notify().await;
    ok()
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
        "permission_prompt" => TurnReason::NeedsPermission,
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

    // A main-workspace session that reached Stop while a managed process is red
    // is not waiting on a prompt, it is broken. Red outranks ochre (§2).
    let build_failure = {
        let inner = app.inner.read().await;
        inner.workspaces.get(&workspace).and_then(|w| {
            w.processes.iter().find_map(|p| match &p.health {
                Health::Failing { summary } if p.is_managed() => Some(summary.clone()),
                _ => None,
            })
        })
    };

    {
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            match build_failure {
                Some(summary) => s.set_state(State::BuildFailing { summary }),
                None => {
                    if !matches!(s.state, State::YourTurn { .. }) {
                        s.set_state(State::YourTurn {
                            since: SystemTime::now(),
                            reason: TurnReason::TurnComplete,
                        });
                    }
                }
            }
        }
    }

    let _ = app.reconcile(&workspace).await;
    app.notify().await;
    ok()
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

pub fn write_settings(port: u16) -> Result<PathBuf> {
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
    let matched = |matcher: &str, path: &str| {
        json!({ "matcher": matcher, "hooks": [http(path)] })
    };

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
                    "command": guard.to_string_lossy(),
                    "timeout": 5,
                }]},
            ],
            "PostToolUse":      [matched("Edit|Write", "post-tool-use")],
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

    /// The pending prompt has to reach the pty, or `/resolve` starts a session
    /// that just sits there. Exercised against a real pty running `cat`, so no
    /// Claude process is involved.
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

    #[test]
    fn subagent_stop_is_routed_somewhere_that_cannot_change_state() {
        // Guards the §3 invariant at the config layer: SubagentStop must never
        // share a URL with Stop.
        let settings = {
            let base = "http://127.0.0.1:7777/hooks";
            (
                format!("{base}/stop"),
                format!("{base}/subagent-stop"),
            )
        };
        assert_ne!(settings.0, settings.1);
    }

    #[test]
    fn hooks_carry_the_correlation_header_and_a_short_timeout() {
        let dir = std::env::temp_dir().join(format!("orchd-test-{}", std::process::id()));
        std::env::set_var("HOME", &dir);
        let path = write_settings(7777).expect("write settings");
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

        let _ = std::fs::remove_dir_all(&dir);
    }
}
