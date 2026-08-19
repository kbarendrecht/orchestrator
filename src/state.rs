use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{broadcast, Notify, RwLock};
use uuid::Uuid;

use crate::config::Config;
use crate::git;
use crate::model::*;
use crate::pty::pid_alive;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Repos {
    /// Where PRs are opened, e.g. `acme/monorepo`.
    pub upstream: Option<String>,
    /// Where branches are pushed, e.g. `kbarendrecht/acme` (§6's fork flow).
    pub fork: Option<String>,
}

pub struct AppState {
    pub cfg: Config,
    pub repos: Repos,
    /// Random per-start token embedded in the served SPA and required on the
    /// WebSocket and every mutating endpoint (§12).
    pub token: String,
    pub inner: RwLock<Inner>,
    /// Fan-out of state snapshots to connected SPAs.
    pub events: broadcast::Sender<String>,
    /// How the SPA should draw its top bar.
    ///
    /// Fixed at startup rather than set when the window attaches: the webview
    /// begins loading the instant it is built, so a chrome decided afterwards
    /// would race the very first paint and lose, intermittently.
    pub chrome: crate::window::Chrome,
    /// Set by the desktop shell once it has a window; `None` when orchd is
    /// running headless and the UI is a browser tab that owns its own chrome.
    pub window: RwLock<Option<Arc<dyn crate::window::WindowControl>>>,
    /// Pulsed by the refresh button to make the review poller fetch now rather
    /// than wait out the rest of its period.
    /// Pulsed whenever an interaction is answered. Every waiting long poll wakes
    /// and re-checks its own question; there are only ever a handful of waiters,
    /// and one notify beats a channel per question.
    pub answered: Arc<Notify>,
    pub review_refresh: Arc<Notify>,
    /// The same, for the PR poller.
    pub pr_refresh: Arc<Notify>,
}

/// One PR's review threads as of a particular fetch.
pub struct ThreadCache {
    pub fetched: SystemTime,
    pub threads: crate::github::Threads,
}

#[derive(Default)]
pub struct Inner {
    pub workspaces: HashMap<WorkspaceId, Workspace>,
    pub sessions: HashMap<SessionId, Session>,
    pub files: FileSets,
    pub prs: Vec<crate::github::Pr>,
    /// Review threads fetched on demand, per PR number. The 5-minute poll only
    /// counts threads; this holds the bodies the review overlay renders, and
    /// the `head_sha` they were read at — which is what a later post step
    /// re-checks before replying against a diff that may have been force-pushed
    /// away.
    pub threads: HashMap<u64, ThreadCache>,
    /// What the last triage run proposed, per PR. A run costs a full agent pass,
    /// so this outlives the session that produced it — you can close the overlay
    /// and come back. Its absence after a run exits is how a failed run is
    /// detected: the agent reports by POSTing, not by its exit code.
    pub proposals: HashMap<u64, crate::proposal::ProposalSet>,
    /// A batch that stopped for the manual phase, per PR.
    ///
    /// The resume pointer used to live only in the browser, so a reload, a daemon
    /// restart, or opening another PR stranded a batch whose patches were already
    /// committed — recoverable only by hand in git. Not the ledger the post batch
    /// rejects: that rule is about what landed on GitHub, which GitHub can be asked
    /// about. This records a *local* commit, and `fold_in` rewrites shas in both its
    /// arms, so after a fold the old sha is not even an ancestor of HEAD and no
    /// reachability query can prove the new one is ours.
    pub manual: HashMap<u64, crate::post::ManualPhase>,
    /// Stories already filed for a review thread, so a retry reuses one rather
    /// than filing a second. A cache, not a ledger — `crate::story` explains why
    /// losing it costs latency and not correctness.
    pub stories: crate::story::Cache,
    /// Your own GitHub login, from the PR poll's `viewer { login }`. The vendored
    /// prompts take it as `{{LOGIN}}`.
    pub viewer: Option<String>,
    /// Last poll failure. A broken poller must read as broken, never as "no
    /// open PRs".
    pub pr_error: Option<String>,
    pub pr_fetched: Option<SystemTime>,
    /// Bumped once per completed PR poll, so the refresh button can spin until
    /// the fetch it triggered has landed. Mirrors `reviews_poll`.
    pub pr_poll: u64,
    /// A fetch is in flight right now. `pr_poll` says one *landed*, which is what
    /// stops the spinner; this is what starts it when nobody pressed the button.
    pub pr_polling: bool,
    pub token_source: Option<crate::github::TokenSource>,
    pub reviews: crate::reviews::ReviewState,
    /// Bumped once per completed review poll. The SPA watches it to spin the
    /// refresh button until the fetch its click triggered has actually landed,
    /// rather than until the next unrelated re-render.
    pub reviews_poll: u64,
    /// As `pr_polling`, for the review queue.
    pub reviews_polling: bool,
    /// Files rewritten through the diff editor, and which sessions have been
    /// told. Conflict detection on save protects you from the agent; this is
    /// the other direction, which is the one that loses work silently.
    pub human_edits: HashMap<PathBuf, HumanEdit>,
    pub automation: crate::fix_pr::AutomationStore,
    /// (behind, ahead) per workspace against the upstream base.
    pub divergence: HashMap<WorkspaceId, (u32, u32)>,
    /// What each workspace changed since it branched, and the commit that is
    /// measured from. Computed in `reconcile` beside the status, so the two
    /// always describe the same moment.
    pub changed: HashMap<WorkspaceId, Vec<crate::diff::DiffFile>>,
    pub base: HashMap<WorkspaceId, String>,
    /// Workspaces with a rebase stopped part-way.
    pub rebasing: std::collections::HashSet<WorkspaceId>,
    /// Shared resources currently held by a run (§7 rule 2).
    pub locks_held: Vec<String>,
    /// Whether the main checkout's `docker compose` stack has running containers.
    /// `None` before the first probe; the drawer header reads it as up/down.
    pub stack_up: Option<bool>,
    /// A newer GitHub release than the running build, if the update poller has
    /// found one. Surfaced to the SPA as a dismissible nudge; the actual upgrade
    /// is `mise up`.
    pub update: Option<UpdateInfo>,
}

/// A release newer than what is running. `mise` does the upgrade; this only tells
/// you it is there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct HumanEdit {
    pub at: SystemTime,
    /// Sessions already interrupted about this edit. Each is told exactly once,
    /// so the retry after re-reading goes through.
    pub told: std::collections::HashSet<SessionId>,
}

impl AppState {
    pub fn new(cfg: Config, token: String, chrome: crate::window::Chrome) -> Arc<Self> {
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
        let repos = Repos {
            upstream: crate::github::remote_url(&cfg.main_checkout, &cfg.upstream_remote)
                .and_then(|u| crate::github::repo_from_remote(&u))
                .map(|(o, n)| format!("{o}/{n}")),
            fork: crate::github::remote_url(&cfg.main_checkout, "origin")
                .and_then(|u| crate::github::repo_from_remote(&u))
                .map(|(o, n)| format!("{o}/{n}")),
        };
        Arc::new(AppState {
            cfg,
            repos,
            token,
            inner: RwLock::new(Inner {
                workspaces,
                sessions: HashMap::new(),
                files: HashMap::new(),
                prs: Vec::new(),
                threads: HashMap::new(),
                proposals: HashMap::new(),
                manual: HashMap::new(),
                stories: Default::default(),
                viewer: None,
                pr_error: None,
                pr_fetched: None,
                pr_poll: 0,
                pr_polling: false,
                token_source: None,
                reviews: Default::default(),
                reviews_poll: 0,
                reviews_polling: false,
                human_edits: HashMap::new(),
                automation: Default::default(),
                divergence: HashMap::new(),
                changed: HashMap::new(),
                base: HashMap::new(),
                rebasing: Default::default(),
                locks_held: Vec::new(),
                stack_up: None,
                update: None,
            }),
            events,
            chrome,
            window: RwLock::new(None),
            answered: Arc::new(Notify::new()),
            review_refresh: Arc::new(Notify::new()),
            pr_refresh: Arc::new(Notify::new()),
        })
    }

    /// Hand the daemon a window to drive.
    ///
    /// Called once, from the desktop shell's `setup`, as soon as the webview
    /// exists. Until then `/api/window/*` has nothing to talk to and says so.
    pub async fn attach_window(&self, control: Arc<dyn crate::window::WindowControl>) {
        *self.window.write().await = Some(control);
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

    /// Write the session records without pushing a snapshot.
    ///
    /// Shutdown wants this: `was_live` is read off session state, and killing
    /// the ptys flips that state from under you a moment later. Persisting
    /// first is what lets auto-resume rebuild the rail next launch.
    pub async fn persist_now(&self) {
        self.persist().await;
    }

    /// Session records are written on every state change, so a daemon that dies
    /// unexpectedly still leaves something to resume from (§2).
    async fn persist(&self) {
        let records: Vec<crate::store::SessionRecord> = {
            let inner = self.inner.read().await;
            inner
                .sessions
                .values()
                .map(crate::store::SessionRecord::of)
                .collect()
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
                changed: inner.changed.get(&w.id).cloned().unwrap_or_default(),
                changed_since: inner.base.get(&w.id).cloned(),
                behind: inner.divergence.get(&w.id).map(|d| d.0).unwrap_or(0),
                ahead: inner.divergence.get(&w.id).map(|d| d.1).unwrap_or(0),
                rebasing: inner.rebasing.contains(&w.id),
            })
            .collect();
        workspaces.sort_by(|a, b| b.is_main.cmp(&a.is_main).then(a.id.cmp(&b.id)));

        // A PR belongs to a workspace when its head ref is in that workspace's
        // branch set (§2). Many-to-many, so this is a lookup rather than a
        // field on either side.
        let mut prs: Vec<PrView> = inner
            .prs
            .iter()
            .map(|p| {
                let workspace = inner
                    .workspaces
                    .values()
                    .find(|w| w.branches.contains(&p.head_ref))
                    .map(|w| w.id.clone());
                let session = workspace.as_ref().and_then(|ws| {
                    inner
                        .sessions
                        .values()
                        .filter(|s| &s.workspace == ws && s.state.is_live())
                        .map(|s| s.id)
                        .next()
                });
                PrView {
                    pr: p.clone(),
                    rank: p.rank(),
                    workspace,
                    session,
                }
            })
            .collect();
        prs.sort_by(|a, b| a.rank.cmp(&b.rank).then(b.pr.number.cmp(&a.pr.number)));

        Snapshot {
            workspaces,
            sessions,
            prs,
            pr_error: inner.pr_error.clone(),
            pr_age_ms: inner
                .pr_fetched
                .and_then(|t| now.duration_since(t).ok().map(|d| d.as_millis() as u64)),
            pr_poll: inner.pr_poll,
            pr_polling: inner.pr_polling,
            token_source: inner.token_source,
            reviews: inner.reviews.clone(),
            reviews_poll: inner.reviews_poll,
            reviews_polling: inner.reviews_polling,
            automation: inner.automation.by_pr.clone(),
            repos: self.repos.clone(),
            stack_up: inner.stack_up,
            update: inner.update.clone(),
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    /// Record a rewrite made through the editor, so agents can be told about it.
    ///
    /// Re-recording clears `told`: a second rewrite is news again even to a
    /// session that already heard about the first.
    pub async fn record_human_edit(&self, path: PathBuf) {
        let real = std::fs::canonicalize(&path).unwrap_or(path);
        let mut inner = self.inner.write().await;
        inner.human_edits.insert(
            real,
            HumanEdit {
                at: SystemTime::now(),
                told: Default::default(),
            },
        );
    }

    /// Whether this session should be interrupted before writing `path`.
    ///
    /// Returns the message once per session per edit; afterwards the write is
    /// allowed, so the agent's retry after re-reading succeeds.
    pub async fn claim_stale_warning(&self, session: SessionId, path: &Path) -> Option<String> {
        let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut inner = self.inner.write().await;
        let edit = inner.human_edits.get_mut(&real)?;
        if !edit.told.insert(session) {
            return None;
        }
        let ago = edit.at.elapsed().map(|d| d.as_secs()).unwrap_or(0);
        Some(format!(
            "STALE BUFFER: this file was rewritten in the orchestrator's editor {ago}s ago, \
             after you last read it. Re-read {} before writing, or you will overwrite that change.",
            real.display()
        ))
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
        inner
            .workspaces
            .entry(name.to_string())
            .or_insert(Workspace {
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

    /// Kill everything running in a workspace nobody is in any more.
    ///
    /// The shells and the hand-started processes exist for the session that
    /// opened them: once it is gone they are output nobody reads and a prompt
    /// nobody types into, still holding the port and the CPU. Their own exit
    /// watchers do the bookkeeping, so this only has to pull the trigger.
    /// Anything config autostarts is spared, see `is_autostart`.
    ///
    /// Containers are not reached, the same as at shutdown: `docker compose up`
    /// has already detached by the time its pty dies.
    pub async fn kill_processes_in(&self, workspace: &str) -> usize {
        let handles: Vec<_> = {
            let inner = self.inner.read().await;
            inner
                .workspaces
                .get(workspace)
                .map(|w| {
                    w.processes
                        .iter()
                        .filter(|p| !self.is_autostart(workspace, &p.name))
                        .filter_map(|p| p.pty.clone())
                        .collect()
                })
                .unwrap_or_default()
        };
        for h in &handles {
            let _ = h.kill();
        }
        handles.len()
    }

    /// Does config start this process by itself?
    ///
    /// Such a process was never opened by a session, so it is not a session's to
    /// take down with it: `ng-watch` in main is meant to be running whenever the
    /// daemon is, not only while somebody happens to have a session open there.
    fn is_autostart(&self, workspace: &str, name: &str) -> bool {
        let specs = if workspace == MAIN {
            &self.cfg.main_processes
        } else {
            &self.cfg.worktree_processes
        };
        specs.iter().any(|s| s.autostart && s.name == name)
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
        let set = git::status(&path, is_main, git::Untracked::Collapsed)?;
        // Recomputed alongside the file list, so the two always describe the
        // same moment.
        let divergence = git::divergence(&path, &self.cfg.upstream_ref).ok();
        let rebasing = git::rebase_in_progress(&path);
        // Branches accumulate and are never removed (§2): a PR still belongs to
        // the session that made it after you have moved on to another branch.
        let branch = git::current_branch(&path).ok();

        // What this workspace changed since it branched: committed work and
        // uncommitted both, which is the question the changed-files pane asks.
        // `git status` cannot answer it — a session that commits would empty its
        // own list — so it is a diff against the merge-base, plus the untracked
        // files a diff never sees.
        //
        // Failure is empty rather than fatal: a worktree whose upstream ref has
        // not been fetched yet still has a status worth showing.
        let base = git::merge_base(&path, &self.cfg.upstream_ref).ok();
        let changed = base.as_deref().map(|b| {
            let mut files = crate::diff::summary(&path, b)
                .map(|s| s.files)
                .unwrap_or_default();
            files.extend(set.untracked.iter().map(crate::diff::DiffFile::untracked));
            files.sort_by(|a, b| a.path.cmp(&b.path));
            files
        });

        let mut inner = self.inner.write().await;
        if let (Some(b), Some(w)) = (branch, inner.workspaces.get_mut(workspace)) {
            w.branches.insert(b);
        }
        inner.files.insert(workspace.to_string(), set);
        if let Some(b) = base {
            inner.base.insert(workspace.to_string(), b);
        }
        if let Some(c) = changed {
            inner.changed.insert(workspace.to_string(), c);
        }
        if let Some(d) = divergence {
            inner.divergence.insert(workspace.to_string(), d);
        }
        if rebasing {
            inner.rebasing.insert(workspace.to_string());
        } else {
            inner.rebasing.remove(workspace);
        }
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
    pub prs: Vec<PrView>,
    /// Set when the last poll failed; the pane says so rather than showing an
    /// empty list.
    pub pr_error: Option<String>,
    pub pr_age_ms: Option<u64>,
    /// Monotonic counter of completed PR polls; see `Inner::pr_poll`.
    pub pr_poll: u64,
    /// A PR fetch is running. The pane spins its refresh icon while it is,
    /// however the fetch was started.
    pub pr_polling: bool,
    pub token_source: Option<crate::github::TokenSource>,
    pub reviews: crate::reviews::ReviewState,
    /// Monotonic counter of completed review polls; see `Inner::reviews_poll`.
    pub reviews_poll: u64,
    pub reviews_polling: bool,
    pub automation: HashMap<u64, crate::fix_pr::PrAutomation>,
    pub repos: Repos,
    /// `docker compose` stack has running containers; `None` before first probe.
    pub stack_up: Option<bool>,
    /// A newer release than the running build, or `None`.
    pub update: Option<UpdateInfo>,
    /// The running build's own version, for the settings panel. Always here,
    /// unlike `update`, which only appears when there is something newer: "which
    /// build am I on" is a question worth answering when the answer is "the
    /// latest one".
    pub version: &'static str,
}

#[derive(Debug, Serialize)]
pub struct PrView {
    #[serde(flatten)]
    pub pr: crate::github::Pr,
    pub rank: u8,
    /// The workspace whose branch set contains this PR's head ref.
    pub workspace: Option<String>,
    /// A live session in that workspace, so the row can act as a jump link.
    pub session: Option<Uuid>,
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
    /// Every file this workspace changed since it branched, committed work
    /// included, plus anything untracked. What the changed-files pane lists.
    pub changed: Vec<crate::diff::DiffFile>,
    /// The commit the above is measured from: `merge-base(upstream, HEAD)`.
    pub changed_since: Option<String>,
    /// Commits on `upstream/develop` this branch does not have. Drives the
    /// rebase affordance.
    pub behind: u32,
    pub ahead: u32,
    pub rebasing: bool,
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
    /// Whether this is idle time worth counting. A session you just opened is
    /// idle but not waiting on you in the sense the rail exists to surface.
    pub wants_attention: bool,
    /// How long this session has been waiting on you. With 4-6 sessions the
    /// cost of the whole tool is measured in agent-minutes spent idle (§2).
    pub waiting_ms: Option<u64>,
    pub created_ms: u64,
    pub alive: bool,
    pub dirty_count: usize,
    pub boundary_violations: Vec<String>,
    pub resumable: bool,
    /// Whether there is a conversation on disk to come back to. Only asked of
    /// sessions that have finished, so the cost is one stat per archived row
    /// rather than one per session on every snapshot.
    pub has_transcript: bool,
    /// What this session is blocked on, waiting for you. The overlay renders it;
    /// everything else ignores it.
    pub interaction: Option<Interaction>,
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
            wants_attention: s.state.wants_attention(),
            waiting_ms,
            created_ms: now
                .duration_since(s.created_at)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            alive: s.pty.as_ref().map(|h| h.is_alive()).unwrap_or(false),
            dirty_count: s.dirty_paths.len(),
            boundary_violations: s.boundary_violations.clone(),
            // Just the file question. It used to be ANDed with "and it has
            // finished", which the one caller checks for itself anyway, and a
            // live session needs the honest answer too: forking one that has
            // not had a turn yet is offering something Claude answers with "no
            // conversation found".
            has_transcript: crate::store::transcript_exists(
                s.id,
                &s.cwd,
                s.transcript_path.as_deref(),
            ),
            resumable: matches!(s.recovery, Some(ArchiveState::Recoverable { .. }) | None)
                && !matches!(s.recovery, Some(ArchiveState::TranscriptOnly)),
            interaction: s.interaction.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    async fn app() -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!("orchd-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7798}}"#,
            dir.display()
        ))
        .unwrap();
        AppState::new(cfg, "t".into(), crate::window::Chrome::None)
    }

    #[tokio::test]
    async fn an_agent_is_warned_once_then_allowed_through() {
        let app = app().await;
        let dir = std::env::temp_dir().join(format!("orchd-warn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        std::fs::write(&f, "x").unwrap();

        let s1 = Uuid::new_v4();
        // Nothing recorded yet, so an ordinary edit is never gated.
        assert!(app.claim_stale_warning(s1, &f).await.is_none());

        app.record_human_edit(f.clone()).await;
        let first = app.claim_stale_warning(s1, &f).await;
        assert!(first.is_some(), "the agent must be told");
        assert!(first.unwrap().contains("STALE BUFFER"));

        // The retry after re-reading has to go through, or the turn stalls.
        assert!(app.claim_stale_warning(s1, &f).await.is_none());

        // A different session has not heard about it yet.
        let s2 = Uuid::new_v4();
        assert!(app.claim_stale_warning(s2, &f).await.is_some());

        // A second rewrite is news again, even to a session already told.
        app.record_human_edit(f.clone()).await;
        assert!(app.claim_stale_warning(s1, &f).await.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_file_nobody_rewrote_is_never_gated() {
        let app = app().await;
        let other = std::env::temp_dir().join("orchd-untouched.txt");
        std::fs::write(&other, "x").unwrap();
        assert!(app
            .claim_stale_warning(Uuid::new_v4(), &other)
            .await
            .is_none());
        let _ = std::fs::remove_file(&other);
    }
}
