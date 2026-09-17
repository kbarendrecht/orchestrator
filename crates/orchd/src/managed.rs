//! Managed processes: the dev servers and watchers a checkout declares.
//!
//! **Split out of `spawn` because it is not a session.** `spawn` grew to 3,157
//! lines over four unrelated workflows, and this was the one that shared nothing
//! with the others: a managed process is an arbitrary command from `config.json`
//! with a health rule over its output, not a Claude agent with a transcript, a
//! branch and a state machine. The two met only at "the daemon owns a pty for it",
//! which is `orchd-base::pty`, not a reason to share a file.
//!
//! Nothing here changed in the move. The order in [`stop_managed`] — the
//! configured stop first, then the pty — and the health watcher's reading of a
//! process's own output are the same rules, at a new address.

use anyhow::{Context, Result};
use std::sync::Arc;
use uuid::Uuid;

use crate::config::ManagedSpec;
use crate::model::*;
use crate::pty::{PtyHandle, DEFAULT_SIZE};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// Stop a managed process: its `stop_command` first, then the pty.
///
/// **The order is the point.** For an ordinary process the pty child *is* the
/// process and this is just a kill. For a client — `docker compose exec` is the
/// case that produced it — killing the pty leaves the real process running with
/// nothing pointing at it, so the configured stop runs first and the kill takes
/// the client afterwards.
///
/// The spec is read from config by name rather than carried on the record: the
/// record holds what was *started*, and a stop command is a thing you fix after
/// discovering you need it. Settings restart the daemon anyway, so the two cannot
/// drift far.
///
/// Never fails. A stop command that errors, times out or is missing still gets the
/// pty killed, because the alternative is a process the daemon has stopped showing
/// you but has not stopped.
pub async fn stop_managed(app: &Arc<AppState>, workspace: &str, name: &str, pty: &Arc<PtyHandle>) {
    let spec = app.cfg.managed_spec(workspace, name);
    if let Some(spec) = spec.filter(|s| !s.stop_command.is_empty()) {
        if let Some(cwd) = app.workspace_path(workspace).await {
            let argv = spec.stop_command.clone();
            let label = format!("stop {workspace}:{name}");
            // Short: this runs on the way out, and a restart of the app waits on it.
            let out = tokio::task::spawn_blocking(move || {
                crate::proc::run_bounded(&cwd, STOP_TIMEOUT_SECS, &argv, &label)
            })
            .await;
            match out {
                Ok(Ok(o)) if !o.status.success() => tracing::warn!(
                    %workspace, %name, "stop_command exited {}: {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Ok(Err(e)) => tracing::warn!(%workspace, %name, "stop_command failed: {e:#}"),
                Err(e) => tracing::warn!(%workspace, %name, "stop_command panicked: {e}"),
                Ok(Ok(_)) => tracing::info!(%workspace, %name, "stopped through its stop_command"),
            }
        }
    }
    // Escalating, because the caller is replacing or removing this process and a
    // survivor would hold its port or its container exec open.
    pty.kill_gracefully().await;
}

/// How long a `stop_command` may take. Short, because closing the window waits on
/// it and a stop that hangs would hold the app open.
const STOP_TIMEOUT_SECS: u64 = 10;

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
    };

    {
        let mut inner = app.inner.write().await;
        if let Some(w) = inner.workspaces.get_mut(workspace) {
            // Killed, not merely forgotten. `retain` used to drop the old record
            // and its `PtyHandle` with it, and `PtyHandle` has no `Drop`, so the
            // child went on running with nothing left pointing at it. The API path
            // cannot reach this — `start` refuses a process that is already
            // running and `restart` kills first — but `autostart_processes` on a
            // daemon restart goes straight through, which is exactly when a
            // previous run's watcher is most likely to still be there.
            for old in w.processes.iter().filter(|p| p.id == proc_id) {
                if let Some(pty) = &old.pty {
                    if let Err(e) = pty.kill() {
                        tracing::warn!(%proc_id, "could not kill the process being replaced: {e:#}");
                    }
                }
            }
            w.processes.retain(|p| p.id != proc_id);
            // The `stop_command` is deliberately *not* run here. This arm only
            // fires when a record for this name is still in the map at start —
            // autostart after a daemon restart — where the process it names died
            // with the daemon that owned it. Running a stop then would kill a
            // remote the new one is about to talk to. The paths that mean "stop
            // this", the drawer's close and restart, go through `stop_managed`.
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
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let path = app
        .workspace_path(workspace)
        .await
        .with_context(|| format!("unknown workspace {workspace}"))?;
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
            /* The child exiting, not merely the end of its output. The `Process`
            record holds the pty handle, so the broadcast sender outlives the
            child and `rx.recv()` never errors — which meant this loop never
            reached the `Dead` below it, and a managed process that had exited
            sat at `Starting` with a live-looking dot in the drawer. Measured:
            a spec that exits immediately still read `starting, alive: false,
            exit 1` a minute later.

            `wait()` clones its own receiver off a watch channel, so it is safe
            to poll here and there is still one observer of this pty. */
            let chunk = tokio::select! {
                r = rx.recv() => match r {
                    Ok(c) => c,
                    // A lagged consumer only misses health lines, and the next
                    // build will restate them; resubscribing beats tearing down.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },
                _ = handle.wait() => break,
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

/// Drain whole lines out of `pending` and apply the health verdict they imply.
async fn scan(
    app: &Arc<AppState>,
    workspace: &str,
    proc_id: &str,
    spec: &ManagedSpec,
    pending: &mut String,
) {
    let Some(health) = crate::health::scan_lines(spec, pending) else {
        return;
    };
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
                // Only sessions already at rest: a working agent is not waiting
                // on anything, so a red build is not its news yet. Keyed on the
                // health that just changed rather than a workspace-wide scan —
                // see `health::build_failure_in`, which is what `hooks.rs` asks
                // when a turn ends.
                match (&health, &s.state) {
                    (Health::Failing { summary }, State::YourTurn { .. }) => {
                        s.set_state(crate::health::at_rest(Some(summary)));
                    }
                    (Health::Ok, State::BuildFailing { .. }) => {
                        s.set_state(crate::health::at_rest(None));
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
