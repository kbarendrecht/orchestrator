use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use uuid::Uuid;

use crate::config::{Config, ManagedSpec};
use crate::model::*;
use crate::pty::PtyHandle;
use crate::state::AppState;

pub(crate) const DEFAULT_SIZE: (u16, u16) = (40, 140);

/// Placeholder workspace for a worktree whose name Claude Code has not reported
/// yet. Replaced at `SessionStart`.
pub const PENDING_WORKTREE: &str = "\u{2026}creating";

/// Spawn an interactive Claude session in an existing workspace.
///
/// The daemon spawns every session and never adopts a shell-started one. That
/// is what makes `$ORCH_SESSION_ID` injection and exact hook correlation
/// possible (§2).
pub async fn spawn_session(
    app: &Arc<AppState>,
    workspace: &str,
    kind: Kind,
    resume: Option<Uuid>,
) -> Result<SessionId> {
    let path = app
        .workspace_path(workspace)
        .await
        .with_context(|| format!("unknown workspace {workspace}"))?;

    // Main is exclusive, and the claim is taken before the process starts so a
    // failed spawn cannot leave the lease held.
    let id = resume.unwrap_or_else(Uuid::new_v4);
    if workspace == MAIN {
        app.claim_main(id).await?;
    }

    let settings = Config::hooks_settings_path()?;
    let mut cmd = vec!["claude".to_string()];
    match resume {
        Some(prev) => {
            cmd.push("--resume".into());
            cmd.push(prev.to_string());
        }
        None => {
            // Assigning the id makes the daemon's session id and Claude's own
            // the same value, so resume and transcript lookup need no mapping.
            cmd.push("--session-id".into());
            cmd.push(id.to_string());
        }
    }
    cmd.push("--settings".into());
    cmd.push(settings.to_string_lossy().into_owned());

    let (mut env, unset) = crate::config::transcript_env();
    env.push(("ORCH_SESSION_ID".to_string(), id.to_string()));
    let spawned = PtyHandle::spawn(&cmd, &path, &env, &unset, DEFAULT_SIZE)?;

    let mut session = Session::new(id, workspace.to_string(), path.clone(), kind);
    session.pty = Some(spawned.handle.clone());
    session.pid = spawned.pid;

    {
        let mut inner = app.inner.write().await;
        inner.sessions.insert(id, session);
    }

    watch_session_exit(app.clone(), id, spawned.handle);
    app.notify().await;
    Ok(id)
}

/// Create a worktree and start a session in it.
///
/// Creation is delegated to the repo's own `worktree-create` / `worktree-link`
/// hooks by launching `claude --worktree` rather than running `git worktree add`
/// here (§11) — those hooks already base on a freshly fetched `upstream/develop`
/// and configure triangular push, and reimplementing that would fight them.
///
/// **Spawned with cwd = main**: `worktree-create` refuses to nest a worktree
/// inside a worktree (§2).
pub async fn spawn_worktree_session(app: &Arc<AppState>, name: Option<&str>) -> Result<SessionId> {
    if let Some(name) = name {
        validate_worktree_name(name)?;

        // Worktree names must be unique over time (§2): the projects directory
        // is keyed by path, so reusing an archived worktree's name would
        // interleave the two sessions' transcripts.
        let inner = app.inner.read().await;
        if inner.workspaces.contains_key(name) {
            bail!("a workspace named {name} already exists");
        }
        if inner.sessions.values().any(
            |s| matches!(&s.recovery, Some(ArchiveState::Recoverable { name: n, .. }) if n == name),
        ) {
            bail!(
                "an archived session used the worktree name {name}; pick another (e.g. {name}-2) \
                 so its transcripts do not interleave"
            );
        }
        drop(inner);
        if app.cfg.worktree_path(name).exists() {
            bail!(
                "{} already exists on disk",
                app.cfg.worktree_path(name).display()
            );
        }
    }

    let id = Uuid::new_v4();
    let settings = Config::hooks_settings_path()?;
    let mut cmd = vec!["claude".to_string(), "--worktree".to_string()];
    // With no name, Claude Code generates one. That is also the only path that
    // cannot collide with an archived worktree by construction, since it has
    // never been used before.
    if let Some(name) = name {
        cmd.push(name.to_string());
    }
    cmd.extend([
        "--session-id".to_string(),
        id.to_string(),
        "--settings".to_string(),
        settings.to_string_lossy().into_owned(),
    ]);
    let (mut env, unset) = crate::config::transcript_env();
    env.push(("ORCH_SESSION_ID".to_string(), id.to_string()));

    // cwd is the main checkout, not the worktree-to-be.
    let spawned = PtyHandle::spawn(&cmd, &app.cfg.main_checkout, &env, &unset, DEFAULT_SIZE)?;

    // Without a name the path is not known until `SessionStart` reports the
    // cwd, so the workspace is registered there instead.
    let (workspace, cwd) = match name {
        Some(name) => {
            let expected = app.cfg.worktree_path(name);
            app.register_worktree(name, expected.clone(), Some(format!("worktree-{name}")))
                .await;
            (name.to_string(), expected)
        }
        None => (PENDING_WORKTREE.to_string(), app.cfg.main_checkout.clone()),
    };

    let mut session = Session::new(id, workspace, cwd, Kind::Interactive);
    session.pty = Some(spawned.handle.clone());
    session.pid = spawned.pid;
    {
        let mut inner = app.inner.write().await;
        inner.sessions.insert(id, session);
    }

    watch_session_exit(app.clone(), id, spawned.handle);
    app.notify().await;
    Ok(id)
}

/// Start a headless `/green` run pinned to the PR's head branch.
///
/// §8 writes this as `claude -p "/green <pr>" --worktree`, but `--worktree`
/// always cuts a fresh branch from `upstream/develop` while the same section
/// requires a worktree "pinned to that PR's head branch". The branch wins: the
/// worktree is created here and `claude -p` runs inside it.
pub async fn spawn_green_session(
    app: &Arc<AppState>,
    pr: u64,
    head_ref: &str,
) -> Result<SessionId> {
    let workspace = ensure_pr_worktree(app, pr, head_ref).await?;
    let path = app
        .workspace_path(&workspace)
        .await
        .context("worktree vanished")?;

    // Headed, not `-p`: a run you can watch, answer and take over mid-flight, the
    // same shape as /resolve. The guard table is what decides whether the run may
    // start (§8); it never depended on the run being invisible.
    //
    // The prompt is a file the session is told to read rather than a slash
    // command, so nothing depends on `you/commands` being installed and
    // nothing is written into the checkout being driven.
    let prompt_file = vendored_prompt_file(app, pr, "green").await?;

    let id = Uuid::new_v4();
    let settings = Config::hooks_settings_path()?;
    let cmd = vec![
        "claude".to_string(),
        "--session-id".to_string(),
        id.to_string(),
        "--settings".to_string(),
        settings.to_string_lossy().into_owned(),
    ];

    let (mut env, unset) = crate::config::transcript_env();
    env.push(("ORCH_SESSION_ID".to_string(), id.to_string()));
    // Parallel runs collide on ports and docker resource names, so each gets
    // its own compose project and port base (§8).
    env.push(("COMPOSE_PROJECT_NAME".to_string(), format!("orchd-pr-{pr}")));
    env.push((
        "ORCHD_PORT_BASE".to_string(),
        (20000 + (pr % 1000) * 20).to_string(),
    ));

    let spawned = PtyHandle::spawn(&cmd, &path, &env, &unset, DEFAULT_SIZE)?;
    let mut session = Session::new(
        id,
        workspace,
        path,
        Kind::Automation {
            pr,
            command: "green".to_string(),
        },
    );
    session.pty = Some(spawned.handle.clone());
    session.pid = spawned.pid;
    session.pending_prompt = Some(format!(
        "Read {} and follow it. Those are your instructions for PR {pr}.",
        prompt_file.display()
    ));
    {
        let mut inner = app.inner.write().await;
        inner.sessions.insert(id, session);
    }

    watch_session_exit(app.clone(), id, spawned.handle);
    app.notify().await;
    Ok(id)
}

/// Spawn an interactive session pinned to a PR's head branch, and type a slash
/// command into it once it is ready.
///
/// The default answer to the rail's review button: a `claude` session in the PR
/// worktree running `/resolve <pr>` in the pane, the agent doing the reading,
/// fixing, pushing and posting itself while you supervise. The daemon does no
/// irreversible writes here — the agent does, in a shell you can take over. This is
/// the robust path; the native overlay is the opt-in alternative.
pub async fn spawn_command_session(
    app: &Arc<AppState>,
    pr: u64,
    head_ref: &str,
    command: &str,
) -> Result<SessionId> {
    // If the branch already has a worktree with a live session, take you there
    // rather than spawning a second one (§8).
    let existing = {
        let inner = app.inner.read().await;
        inner
            .workspaces
            .values()
            .find(|w| w.branches.iter().any(|b| b == head_ref))
            .map(|w| w.id.clone())
    };
    if let Some(ws) = existing {
        let live = app.live_sessions_in(&ws).await;
        if let Some(id) = live.first() {
            return Ok(*id);
        }
        return start_with_prompt(app, &ws, pr, command).await;
    }

    // Otherwise pin a worktree to that branch. `git worktree add` directly,
    // because the WorktreeCreate hook always cuts a new branch from
    // upstream/develop; `worktree-link` still runs at SessionStart.
    {
        let name = format!("pr-{pr}");
        let inner = app.inner.read().await;
        if inner.sessions.values().any(|s| {
            matches!(&s.recovery, Some(ArchiveState::Recoverable { name: n, .. }) if n == &name)
        }) {
            bail!(
                "an archived session used the worktree name {name}; remove it or rename before \
                 reusing the name, or the two transcripts interleave"
            );
        }
    }
    let name = ensure_pr_worktree(app, pr, head_ref).await?;
    start_with_prompt(app, &name, pr, command).await
}

async fn start_with_prompt(
    app: &Arc<AppState>,
    workspace: &str,
    pr: u64,
    command: &str,
) -> Result<SessionId> {
    let prompt_file = vendored_prompt_file(app, pr, command).await?;
    let id = spawn_session(
        app,
        workspace,
        Kind::Automation {
            pr,
            command: command.to_string(),
        },
        None,
    )
    .await?;
    let mut inner = app.inner.write().await;
    if let Some(s) = inner.sessions.get_mut(&id) {
        // One line, because it is typed into the prompt box: the instructions
        // themselves are in the file. Typing the whole prompt would submit at the
        // first newline, and `/{command}` would resolve from the agent's command
        // path, which is the dependency the vendored prompts exist to remove.
        s.pending_prompt = Some(format!(
            "Read {} and follow it. Those are your instructions for PR {pr}.",
            prompt_file.display()
        ));
    }
    Ok(id)
}

/// Render the vendored prompt for `command` and leave it somewhere the session can
/// read it.
///
/// Under the daemon's own config dir, like `story`'s scratch and for the same two
/// reasons: the repo's `worktree-edit-boundary` hook blocks a write under the main
/// checkout that lands outside the worktree, and a file *inside* the worktree would
/// make the tree dirty — which the review flow then checks. Nothing of the
/// daemon's is written into the checkout it is driving.
async fn vendored_prompt_file(app: &Arc<AppState>, pr: u64, command: &str) -> Result<PathBuf> {
    let template = match command {
        "resolve" => crate::prompt::RESOLVE,
        "green" => crate::prompt::GREEN,
        other => bail!("no vendored prompt for /{other}"),
    };
    let (owner, repo) =
        crate::resolve_repo(app).context("no GitHub repo configured and none on the remote")?;
    let login = {
        let inner = app.inner.read().await;
        inner.viewer.clone()
    }
    .context("no GitHub login yet — the PR poller has not run")?;
    let body = crate::prompt::render(
        template,
        &crate::prompt::Vars {
            pr,
            owner,
            repo,
            login,
            upstream: app.cfg.upstream_ref.clone(),
            upstream_remote: app.cfg.upstream_remote.clone(),
            // The review flow's template uses none of triage's or story's vars.
            ..Default::default()
        },
    )?;

    let dir = Config::config_dir()?.join(format!("{command}-{pr}"));
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("prompt.md");
    // Rewritten per run: the login and the repo are resolved fresh, and a stale
    // copy would be read as this run's instructions.
    std::fs::write(&file, body).with_context(|| format!("writing {}", file.display()))?;
    Ok(file)
}

/// The worktree for a PR's head branch, created if absent.
pub async fn ensure_pr_worktree(app: &Arc<AppState>, pr: u64, head_ref: &str) -> Result<String> {
    if let Some(ws) = {
        let inner = app.inner.read().await;
        inner
            .workspaces
            .values()
            .find(|w| w.branches.iter().any(|b| b == head_ref))
            .map(|w| w.id.clone())
    } {
        return Ok(ws);
    }

    let name = format!("pr-{pr}");
    validate_worktree_name(&name)?;
    let path = app.cfg.worktree_path(&name);
    if !path.exists() {
        crate::git::worktree_add_existing(&app.cfg.main_checkout, &path, head_ref)?;
    }
    app.register_worktree(&name, path, Some(head_ref.to_string()))
        .await;
    Ok(name)
}

/// Worktree names become directory names and branch names (`worktree-<name>`),
/// so anything that would escape the worktrees dir is refused outright.
pub fn validate_worktree_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("worktree name is empty");
    }
    if name.len() > 64 {
        bail!("worktree name is too long");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("worktree name may only contain letters, digits, '-', '_' and '.'");
    }
    if name.starts_with('.') || name.contains("..") {
        bail!("worktree name may not start with '.' or contain '..'");
    }
    Ok(())
}

fn watch_session_exit(app: Arc<AppState>, id: SessionId, handle: Arc<PtyHandle>) {
    tokio::spawn(async move {
        handle.wait().await;
        let workspace = {
            let mut inner = app.inner.write().await;
            match inner.sessions.get_mut(&id) {
                Some(s) => {
                    if s.state.is_live() {
                        s.set_state(State::Exited);
                    }
                    Some(s.workspace.clone())
                }
                None => None,
            }
        };
        app.release_main(id).await;
        if let Some(ws) = workspace {
            // The drawer's processes belong to whoever was working here. Only
            // once the last session in the workspace is gone, though: two
            // sessions can share a worktree, and closing one of them must not
            // pull the shells out from under the other.
            if app.live_sessions_in(&ws).await.is_empty() {
                let killed = app.kill_processes_in(&ws).await;
                if killed > 0 {
                    tracing::info!(workspace = %ws, "stopped {killed} process(es) with the last session");
                }
            }
            // Closing Claude is a moment the changed-file pane must be right
            // about: whatever the last turn left behind is now the whole story.
            let _ = app.reconcile(&ws).await;
        }
        app.notify().await;
    });
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// Start a managed process declared in config for this workspace.
pub async fn start_managed(
    app: &Arc<AppState>,
    workspace: &str,
    spec: &ManagedSpec,
) -> Result<String> {
    let path = app
        .workspace_path(workspace)
        .await
        .with_context(|| format!("unknown workspace {workspace}"))?;
    let spawned = PtyHandle::spawn(&spec.command, &path, &[], &[], DEFAULT_SIZE)?;
    // Subscribe here rather than inside the watcher task: a process that prints
    // its first lines immediately would otherwise have them delivered before
    // the task runs, and a build that was green from its very first line would
    // sit at `Starting` forever.
    let rx = spawned.handle.subscribe();
    let proc_id = format!("{workspace}:{}", spec.name);

    let process = Process {
        id: proc_id.clone(),
        name: spec.name.clone(),
        kind: ProcKind::Managed {
            command: spec.command.clone(),
        },
        health: Health::Starting,
        cwd: path,
        pty: Some(spawned.handle.clone()),
        pid: spawned.pid,
        started_at: SystemTime::now(),
    };

    {
        let mut inner = app.inner.write().await;
        if let Some(w) = inner.workspaces.get_mut(workspace) {
            w.processes.retain(|p| p.id != proc_id);
            w.processes.push(process);
        }
    }

    watch_health(
        app.clone(),
        workspace.to_string(),
        proc_id.clone(),
        spec.clone(),
        spawned.handle,
        rx,
    );
    app.notify().await;
    Ok(proc_id)
}

/// A plain `$SHELL` in the selected workspace's directory — the same directory
/// as the Claude session above it.
///
/// This is what makes the drawer agnostic: it hosts whatever pty you point at
/// it, and `ng-watch` is just the one main happens to declare (§2).
pub async fn spawn_shell(app: &Arc<AppState>, workspace: &str) -> Result<String> {
    let path = app
        .workspace_path(workspace)
        .await
        .with_context(|| format!("unknown workspace {workspace}"))?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let spawned = PtyHandle::spawn(&[shell], &path, &[], &[], DEFAULT_SIZE)?;
    let proc_id = format!("{workspace}:shell:{}", Uuid::new_v4().simple());

    let process = Process {
        id: proc_id.clone(),
        name: "shell".to_string(),
        kind: ProcKind::Shell { exit_code: None },
        // Shells get no health parsing and no restart policy.
        health: Health::Ok,
        cwd: path,
        pty: Some(spawned.handle.clone()),
        pid: spawned.pid,
        started_at: SystemTime::now(),
    };

    {
        let mut inner = app.inner.write().await;
        if let Some(w) = inner.workspaces.get_mut(workspace) {
            w.processes.push(process);
        }
    }

    // Ctrl+D means close. A shell that exits cleanly is removed outright rather
    // than left as a corpse tab you have to hunt down an × to clear.
    //
    // §2 says a dead shell keeps its buffer "until dismissed", and that is still
    // true of the case it was written for: a shell that died on its own, with a
    // non-zero code, keeps its output so the failure is not swallowed.
    let app2 = app.clone();
    let ws = workspace.to_string();
    let pid2 = proc_id.clone();
    tokio::spawn(async move {
        let code = spawned.handle.wait().await;
        {
            let mut inner = app2.inner.write().await;
            if let Some(w) = inner.workspaces.get_mut(&ws) {
                if code == 0 {
                    w.processes.retain(|p| p.id != pid2);
                } else if let Some(p) = w.processes.iter_mut().find(|p| p.id == pid2) {
                    p.kind = ProcKind::Shell {
                        exit_code: Some(code),
                    };
                    p.health = Health::Dead;
                }
            }
        }
        app2.notify().await;
    });

    app.notify().await;
    Ok(proc_id)
}

/// Parse health from a managed process's output.
///
/// `ng-watch` matches Angular error blocks and the first error line becomes the
/// summary shown in the rail (§2).
fn watch_health(
    app: Arc<AppState>,
    workspace: String,
    proc_id: String,
    spec: ManagedSpec,
    handle: Arc<PtyHandle>,
    rx: tokio::sync::broadcast::Receiver<bytes::Bytes>,
) {
    tokio::spawn(async move {
        let mut rx = rx;
        // Anything already buffered between spawn and subscribe.
        let mut pending = String::from_utf8_lossy(&handle.snapshot()).into_owned();
        scan(&app, &workspace, &proc_id, &spec, &mut pending).await;
        loop {
            let chunk = match rx.recv().await {
                Ok(c) => c,
                // A lagged consumer only misses health lines, and the next
                // build will restate them; resubscribing beats tearing down.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            pending.push_str(&String::from_utf8_lossy(&chunk));
            scan(&app, &workspace, &proc_id, &spec, &mut pending).await;
        }

        {
            let mut inner = app.inner.write().await;
            if let Some(w) = inner.workspaces.get_mut(&workspace) {
                if let Some(p) = w.processes.iter_mut().find(|p| p.id == proc_id) {
                    p.health = Health::Dead;
                }
            }
        }
        app.notify().await;
    });
}

/// What one line of output says about a managed process's health.
///
/// Split out from the scan loop because this is where the whole health model
/// actually lives, and it is the part worth pinning down in tests.
fn verdict(spec: &ManagedSpec, raw: &str) -> Option<Health> {
    let line = strip_ansi(raw.trim_end());
    if line.trim().is_empty() {
        return None;
    }
    if spec.failure_patterns.iter().any(|p| line.contains(p)) {
        return Some(Health::Failing {
            summary: line.trim().to_string(),
        });
    }
    if spec.ok_patterns.iter().any(|p| line.contains(p)) {
        return Some(Health::Ok);
    }
    None
}

/// Drain whole lines out of `pending` and apply the health verdict they imply.
async fn scan(
    app: &Arc<AppState>,
    workspace: &str,
    proc_id: &str,
    spec: &ManagedSpec,
    pending: &mut String,
) {
    let mut changed = None;
    while let Some(nl) = pending.find('\n') {
        let line: String = pending.drain(..=nl).collect();
        match verdict(spec, &line) {
            // First error line wins; later ones are the same block.
            Some(Health::Failing { summary }) => {
                changed = Some(Health::Failing { summary });
                break;
            }
            Some(h) => changed = Some(h),
            None => {}
        }
    }
    // Guard against a single unterminated line growing without bound.
    if pending.len() > 64 * 1024 {
        pending.clear();
    }

    let Some(health) = changed else { return };
    let mut dirty = false;
    {
        let mut inner = app.inner.write().await;
        if let Some(w) = inner.workspaces.get_mut(workspace) {
            if let Some(p) = w.processes.iter_mut().find(|p| p.id == proc_id) {
                if p.health != health {
                    p.health = health.clone();
                    dirty = true;
                }
            }
        }
        // A red build outranks a finished turn: promote any session in this
        // workspace that is merely waiting, and demote it back when the build
        // recovers (§2).
        if dirty {
            for s in inner.sessions.values_mut() {
                if s.workspace != workspace {
                    continue;
                }
                match (&health, &s.state) {
                    (Health::Failing { summary }, State::YourTurn { .. }) => {
                        s.set_state(State::BuildFailing {
                            summary: summary.clone(),
                        });
                    }
                    (Health::Ok, State::BuildFailing { .. }) => {
                        // The turn is still finished, so it goes back to being
                        // an idle agent rather than to nothing at all.
                        s.set_state(State::YourTurn {
                            since: SystemTime::now(),
                            reason: TurnReason::TurnComplete,
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    if dirty {
        app.notify().await;
    }
}

/// Strip CSI/OSC escape sequences so pattern matching sees the plain text.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // CSI runs until a byte in 0x40..=0x7E.
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC runs until BEL or ST.
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

/// Resolve the worktree a session actually landed in.
///
/// `claude --worktree` reports the real path at `SessionStart`; until then the
/// daemon only knows where it asked for the worktree to be created.
pub fn worktree_name_of(path: &PathBuf, worktrees_dir: &PathBuf) -> Option<String> {
    path.strip_prefix(worktrees_dir)
        .ok()
        .and_then(|rest| rest.components().next())
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colour_codes_before_matching() {
        let line = "\x1b[31mError:\x1b[0m TS2304: cannot find name";
        assert_eq!(strip_ansi(line), "Error: TS2304: cannot find name");
    }

    #[test]
    fn strips_osc_titles() {
        assert_eq!(strip_ansi("\x1b]0;title\x07done"), "done");
    }

    #[test]
    fn rejects_worktree_names_that_would_escape_the_worktrees_dir() {
        assert!(validate_worktree_name("invoice-export").is_ok());
        assert!(validate_worktree_name("../../etc").is_err());
        assert!(validate_worktree_name("a/b").is_err());
        assert!(validate_worktree_name(".hidden").is_err());
        assert!(validate_worktree_name("").is_err());
    }

    fn ng() -> ManagedSpec {
        ManagedSpec {
            name: "ng-watch".into(),
            command: vec!["true".into()],
            failure_patterns: vec!["Error:".into(), "error TS".into()],
            ok_patterns: vec!["Build at:".into(), "watching for file changes".into()],
            restart: crate::config::RestartPolicy::Never,
            autostart: false,
        }
    }

    #[test]
    fn an_angular_error_block_becomes_the_summary() {
        let v = verdict(
            &ng(),
            "\x1b[31mError:\x1b[0m src/app/x.ts:42:7 - error TS2304: nope",
        );
        match v {
            Some(Health::Failing { summary }) => {
                // Colour codes stripped, so the rail shows readable text.
                assert_eq!(summary, "Error: src/app/x.ts:42:7 - error TS2304: nope");
            }
            other => panic!("expected Failing, got {other:?}"),
        }
    }

    #[test]
    fn a_successful_build_line_reads_as_healthy() {
        assert_eq!(verdict(&ng(), "Build at: 2026-08-14"), Some(Health::Ok));
        assert_eq!(
            verdict(&ng(), "watching for file changes..."),
            Some(Health::Ok)
        );
    }

    #[test]
    fn ordinary_output_says_nothing_either_way() {
        assert_eq!(verdict(&ng(), "compiling 412 files"), None);
        assert_eq!(verdict(&ng(), "   "), None);
    }

    #[test]
    fn reads_the_worktree_name_out_of_a_path() {
        let dir = PathBuf::from("/repo/.claude/worktrees");
        let p = PathBuf::from("/repo/.claude/worktrees/invoice/src/Foo.php");
        assert_eq!(worktree_name_of(&p, &dir), Some("invoice".to_string()));
        assert_eq!(worktree_name_of(&PathBuf::from("/repo/src"), &dir), None);
    }
}
