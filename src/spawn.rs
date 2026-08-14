use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use uuid::Uuid;

use crate::config::{Config, ManagedSpec};
use crate::model::*;
use crate::pty::PtyHandle;
use crate::state::AppState;

const DEFAULT_SIZE: (u16, u16) = (40, 140);

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

    let (mut env, unset) = crate::config::transcript_env(app.cfg.persist_transcripts);
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
pub async fn spawn_worktree_session(app: &Arc<AppState>, name: &str) -> Result<SessionId> {
    validate_worktree_name(name)?;

    // Worktree names must be unique over time (§2): the projects directory is
    // keyed by path, so reusing an archived worktree's name would interleave
    // the two sessions' transcripts.
    {
        let inner = app.inner.read().await;
        if inner.workspaces.contains_key(name) {
            bail!("a workspace named {name} already exists");
        }
        if inner
            .sessions
            .values()
            .any(|s| matches!(&s.recovery, Some(ArchiveState::Recoverable { name: n, .. }) if n == name))
        {
            bail!(
                "an archived session used the worktree name {name}; pick another (e.g. {name}-2) \
                 so its transcripts do not interleave"
            );
        }
    }
    if app.cfg.worktree_path(name).exists() {
        bail!(
            "{} already exists on disk",
            app.cfg.worktree_path(name).display()
        );
    }

    let id = Uuid::new_v4();
    let settings = Config::hooks_settings_path()?;
    let cmd = vec![
        "claude".to_string(),
        "--worktree".to_string(),
        name.to_string(),
        "--session-id".to_string(),
        id.to_string(),
        "--settings".to_string(),
        settings.to_string_lossy().into_owned(),
    ];
    let (mut env, unset) = crate::config::transcript_env(app.cfg.persist_transcripts);
    env.push(("ORCH_SESSION_ID".to_string(), id.to_string()));

    // cwd is the main checkout, not the worktree-to-be.
    let spawned = PtyHandle::spawn(&cmd, &app.cfg.main_checkout, &env, &unset, DEFAULT_SIZE)?;

    let expected = app.cfg.worktree_path(name);
    app.register_worktree(name, expected.clone(), Some(format!("worktree-{name}")))
        .await;

    let mut session = Session::new(id, name.to_string(), expected, Kind::Interactive);
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
        // Closing Claude is a moment the changed-file pane must be right about:
        // whatever the last turn left behind is now the whole story.
        if let Some(ws) = workspace {
            let _ = app.reconcile(&ws).await;
        }
        app.notify().await;
    });
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// Start a managed process declared in config for this workspace.
pub async fn start_managed(app: &Arc<AppState>, workspace: &str, spec: &ManagedSpec) -> Result<String> {
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
        let v = verdict(&ng(), "\x1b[31mError:\x1b[0m src/app/x.ts:42:7 - error TS2304: nope");
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
