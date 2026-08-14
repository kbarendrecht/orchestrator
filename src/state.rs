use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{broadcast, RwLock};
use uuid::Uuid;

use crate::config::Config;
use crate::git;
use crate::model::*;
use crate::pty::pid_alive;

pub struct AppState {
    pub cfg: Config,
    /// Random per-start token embedded in the served SPA and required on the
    /// WebSocket and every mutating endpoint (§12).
    pub token: String,
    pub inner: RwLock<Inner>,
    /// Fan-out of state snapshots to connected SPAs.
    pub events: broadcast::Sender<String>,
}

#[derive(Default)]
pub struct Inner {
    pub workspaces: HashMap<WorkspaceId, Workspace>,
    pub sessions: HashMap<SessionId, Session>,
    pub files: FileSets,
}

impl AppState {
    pub fn new(cfg: Config, token: String) -> Arc<Self> {
        let (events, _) = broadcast::channel(64);
        let mut workspaces = HashMap::new();
        workspaces.insert(
            MAIN.to_string(),
            Workspace {
                id: MAIN.to_string(),
                path: cfg.main_checkout.clone(),
                kind: WorkspaceKind::Main,
                branches: HashSet::new(),
                processes: Vec::new(),
                occupant: None,
            },
        );
        Arc::new(AppState {
            cfg,
            token,
            inner: RwLock::new(Inner {
                workspaces,
                sessions: HashMap::new(),
                files: HashMap::new(),
            }),
            events,
        })
    }

    /// Push a fresh snapshot to every connected SPA. State is small enough that
    /// a whole snapshot beats a delta protocol nobody can debug.
    pub async fn notify(&self) {
        self.persist().await;
        let snapshot = self.snapshot().await;
        if let Ok(json) = serde_json::to_string(&snapshot) {
            let _ = self.events.send(json);
        }
    }

    /// Session records are written on every state change, so a daemon that dies
    /// unexpectedly still leaves something to resume from (§2).
    async fn persist(&self) {
        let records: Vec<crate::store::SessionRecord> = {
            let inner = self.inner.read().await;
            inner.sessions.values().map(crate::store::SessionRecord::of).collect()
        };
        if let Err(e) = crate::store::save(&records) {
            tracing::warn!("could not persist session records: {e:#}");
        }
    }

    /// Bring back the previous run's sessions, all of them archived.
    ///
    /// Records for workspaces that no longer exist are kept: the transcript is
    /// still readable, and dropping them would silently lose the history.
    pub async fn restore_sessions(&self, records: Vec<crate::store::SessionRecord>) {
        let mut inner = self.inner.write().await;
        for r in records {
            let s = r.restore();
            inner.sessions.entry(s.id).or_insert(s);
        }
    }

    pub async fn snapshot(&self) -> Snapshot {
        let inner = self.inner.read().await;
        let now = SystemTime::now();

        let mut sessions: Vec<SessionView> = inner
            .sessions
            .values()
            .map(|s| SessionView::of(s, now))
            .collect();
        // BuildFailing → YourTurn (longest wait first) → Working → Automation →
        // Archived (§9). The waiting duration is the number to optimise down.
        sessions.sort_by(|a, b| {
            a.rank
                .cmp(&b.rank)
                .then(b.waiting_ms.unwrap_or(0).cmp(&a.waiting_ms.unwrap_or(0)))
                .then(a.created_ms.cmp(&b.created_ms))
        });

        let mut workspaces: Vec<WorkspaceView> = inner
            .workspaces
            .values()
            .map(|w| WorkspaceView {
                id: w.id.clone(),
                path: w.path.to_string_lossy().into_owned(),
                kind: w.kind.clone(),
                is_main: w.is_main(),
                occupant: w.occupant,
                branches: w.branches.iter().cloned().collect(),
                processes: w
                    .processes
                    .iter()
                    .map(|p| ProcessView {
                        id: p.id.clone(),
                        name: p.name.clone(),
                        kind: p.kind.clone(),
                        health: p.health.clone(),
                        cwd: p.cwd.to_string_lossy().into_owned(),
                        alive: p.pty.as_ref().map(|h| h.is_alive()).unwrap_or(false),
                        exit_code: p.pty.as_ref().and_then(|h| h.exit_code()),
                    })
                    .collect(),
                files: inner.files.get(&w.id).cloned().unwrap_or_default(),
            })
            .collect();
        workspaces.sort_by(|a, b| b.is_main.cmp(&a.is_main).then(a.id.cmp(&b.id)));

        Snapshot {
            workspaces,
            sessions,
        }
    }

    // -----------------------------------------------------------------------
    // Main occupancy
    // -----------------------------------------------------------------------

    /// Main is exclusive: one Claude session at a time (§2). There is no queue —
    /// the UI disables "new session in main" and shows which session holds it.
    pub async fn claim_main(&self, session: SessionId) -> Result<()> {
        let mut inner = self.inner.write().await;
        let held = inner
            .workspaces
            .get(MAIN)
            .and_then(|w| w.occupant)
            .filter(|id| {
                inner
                    .sessions
                    .get(id)
                    .map(|s| s.state.is_live())
                    .unwrap_or(false)
            });
        if let Some(holder) = held {
            if holder != session {
                bail!("main is occupied by session {holder}");
            }
        }
        if let Some(w) = inner.workspaces.get_mut(MAIN) {
            w.occupant = Some(session);
        }
        Ok(())
    }

    pub async fn release_main(&self, session: SessionId) {
        let mut inner = self.inner.write().await;
        if let Some(w) = inner.workspaces.get_mut(MAIN) {
            if w.occupant == Some(session) {
                w.occupant = None;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Workspaces
    // -----------------------------------------------------------------------

    pub async fn register_worktree(&self, name: &str, path: PathBuf, branch: Option<String>) {
        let mut inner = self.inner.write().await;
        let mut branches = HashSet::new();
        if let Some(b) = branch {
            branches.insert(b);
        }
        inner.workspaces.entry(name.to_string()).or_insert(Workspace {
            id: name.to_string(),
            path,
            kind: WorkspaceKind::Worktree {
                name: name.to_string(),
            },
            branches,
            processes: Vec::new(),
            occupant: None,
        });
    }

    pub async fn workspace_path(&self, id: &str) -> Option<PathBuf> {
        self.inner
            .read()
            .await
            .workspaces
            .get(id)
            .map(|w| w.path.clone())
    }

    /// Which workspace an absolute path belongs to.
    ///
    /// Longest match wins so a path inside `.claude/worktrees/<name>` is
    /// attributed to that worktree rather than to main, which contains it.
    pub async fn workspace_for_path(&self, path: &Path) -> Option<WorkspaceId> {
        let inner = self.inner.read().await;
        inner
            .workspaces
            .values()
            .filter(|w| path.starts_with(&w.path))
            .max_by_key(|w| w.path.as_os_str().len())
            .map(|w| w.id.clone())
    }

    /// Sessions in a workspace that are neither `Exited` nor `Archived`, checked
    /// against `/proc` rather than in-memory state (§8b).
    pub async fn live_sessions_in(&self, workspace: &str) -> Vec<SessionId> {
        let inner = self.inner.read().await;
        inner
            .sessions
            .values()
            .filter(|s| s.workspace == workspace && s.state.is_live())
            .filter(|s| s.pid.map(pid_alive).unwrap_or(false))
            .map(|s| s.id)
            .collect()
    }

    // -----------------------------------------------------------------------
    // Reconcile
    // -----------------------------------------------------------------------

    /// Re-read changed files for a workspace from git.
    ///
    /// Hooks are the primary signal (§4); this catches the Bash-driven changes
    /// no `Edit` hook reported — codegen, builds, git ops.
    pub async fn reconcile(&self, workspace: &str) -> Result<()> {
        let (path, is_main) = {
            let inner = self.inner.read().await;
            let w = inner
                .workspaces
                .get(workspace)
                .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?;
            (w.path.clone(), w.is_main())
        };
        let set = git::status(&path, is_main)?;
        let mut inner = self.inner.write().await;
        inner.files.insert(workspace.to_string(), set);
        for s in inner.sessions.values_mut() {
            if s.workspace == workspace {
                s.dirty_paths.clear();
                s.last_reconcile = Some(SystemTime::now());
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub workspaces: Vec<WorkspaceView>,
    pub sessions: Vec<SessionView>,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceView {
    pub id: String,
    pub path: String,
    pub kind: WorkspaceKind,
    pub is_main: bool,
    pub occupant: Option<Uuid>,
    pub branches: Vec<String>,
    pub processes: Vec<ProcessView>,
    pub files: FileSet,
}

#[derive(Debug, Serialize)]
pub struct ProcessView {
    pub id: String,
    pub name: String,
    pub kind: ProcKind,
    pub health: Health,
    pub cwd: String,
    pub alive: bool,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct SessionView {
    pub id: Uuid,
    pub workspace: String,
    pub state: State,
    pub kind: Kind,
    pub title: Option<String>,
    pub cwd: String,
    pub rank: u8,
    /// How long this session has been waiting on you. With 4-6 sessions the
    /// cost of the whole tool is measured in agent-minutes spent idle (§2).
    pub waiting_ms: Option<u64>,
    pub created_ms: u64,
    pub alive: bool,
    pub dirty_count: usize,
    pub boundary_violations: Vec<String>,
    pub resumable: bool,
}

impl SessionView {
    fn of(s: &Session, now: SystemTime) -> Self {
        let waiting_ms = s.state.waiting_since().map(|since| {
            now.duration_since(since)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        });
        SessionView {
            id: s.id,
            workspace: s.workspace.clone(),
            state: s.state.clone(),
            kind: s.kind.clone(),
            title: s.title.clone(),
            cwd: s.cwd.to_string_lossy().into_owned(),
            rank: s.sort_rank(),
            waiting_ms,
            created_ms: now
                .duration_since(s.created_at)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            alive: s.pty.as_ref().map(|h| h.is_alive()).unwrap_or(false),
            dirty_count: s.dirty_paths.len(),
            boundary_violations: s.boundary_violations.clone(),
            resumable: matches!(
                s.recovery,
                Some(ArchiveState::Recoverable { .. }) | None
            ) && !matches!(s.recovery, Some(ArchiveState::TranscriptOnly)),
        }
    }
}

pub fn random_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| {
            let n: u8 = rng.gen_range(0..36);
            if n < 10 {
                (b'0' + n) as char
            } else {
                (b'a' + n - 10) as char
            }
        })
        .collect()
}
