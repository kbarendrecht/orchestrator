use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use uuid::Uuid;

use crate::pty::PtyHandle;

pub type SessionId = Uuid;
/// `"main"` for the privileged checkout, the worktree name otherwise.
pub type WorkspaceId = String;

pub const MAIN: &str = "main";

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceKind {
    /// Docker stack, dev URL, `ng build --watch`.
    Main,
    Worktree { name: String },
}

pub struct Workspace {
    pub id: WorkspaceId,
    pub path: PathBuf,
    pub kind: WorkspaceKind,
    pub branches: HashSet<String>,
    pub processes: Vec<Process>,
    /// Main only: the exclusivity mutex. The dev URL is bound to main, so
    /// occupancy *is* the lease — there is no separate mechanism (§2).
    pub occupant: Option<SessionId>,
}

impl Workspace {
    pub fn is_main(&self) -> bool {
        matches!(self.kind, WorkspaceKind::Main)
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// Why a session is waiting on you.
///
/// Running in auto-accept mode, permission prompts almost never fire; what
/// actually gates progress is Claude finishing a turn (§2). A completed turn is
/// not a quiet success, it is an idle agent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnReason {
    /// `Stop` — the common case under auto-accept.
    TurnComplete,
    AskedAQuestion,
    /// Rare in auto mode.
    NeedsPermission,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    Starting,
    Working,
    YourTurn {
        since: SystemTime,
        reason: TurnReason,
    },
    /// A main-workspace session that reached `Stop` while `ng-watch` is red is
    /// not waiting on a prompt, it is broken. Red outranks ochre (§2).
    BuildFailing {
        summary: String,
    },
    Error {
        message: String,
    },
    Exited,
    Archived {
        resumable: bool,
    },
}

impl State {
    /// Rail ordering (§9): BuildFailing → YourTurn → Working → Automation →
    /// Archived. Automation is handled by the caller, since it depends on the
    /// session's kind rather than its state.
    pub fn rank(&self) -> u8 {
        match self {
            State::BuildFailing { .. } => 0,
            State::Error { .. } => 1,
            State::YourTurn { .. } => 2,
            State::Working | State::Starting => 3,
            State::Exited => 5,
            State::Archived { .. } => 6,
        }
    }

    pub fn waiting_since(&self) -> Option<SystemTime> {
        match self {
            State::YourTurn { since, .. } => Some(*since),
            _ => None,
        }
    }

    pub fn is_live(&self) -> bool {
        !matches!(self, State::Exited | State::Archived { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    Interactive,
    /// An ordinary session whose first prompt is a skill invocation (§8).
    /// Nothing about it needs a separate view.
    Automation { pr: u64, skill: String },
}

/// How to rebuild a torn-down worktree so an archived session can be resumed (§2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "recovery", rename_all = "snake_case")]
pub enum ArchiveState {
    Recoverable {
        name: String,
        branch: String,
        head_sha: String,
    },
    /// Branch gone and sha unreachable — the transcript is readable, the
    /// session cannot be continued.
    TranscriptOnly,
}

pub struct Session {
    /// Also `$ORCH_SESSION_ID` and the `--session-id` handed to Claude, so the
    /// daemon's id and the Claude session id are the same value.
    pub id: SessionId,
    pub workspace: WorkspaceId,
    pub state: State,
    pub kind: Kind,
    pub pty: Option<Arc<PtyHandle>>,
    pub pid: Option<u32>,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub transcript_path: Option<PathBuf>,
    pub archived_transcript: Option<PathBuf>,
    /// True once archiving has been settled: either the transcript was copied,
    /// or there was none to copy. A session killed before its first turn never
    /// gets a `.jsonl`, and must not block teardown forever.
    pub transcript_archived: bool,
    pub recovery: Option<ArchiveState>,
    pub created_at: SystemTime,
    pub state_since: SystemTime,
    /// Paths reported dirty by `PostToolUse` since the last reconcile (§4).
    pub dirty_paths: HashSet<PathBuf>,
    /// Blocked tool calls surfaced by `worktree-edit-boundary` (§11) — an agent
    /// editing outside its worktree is a prompt problem worth seeing.
    pub boundary_violations: Vec<String>,
    pub last_reconcile: Option<SystemTime>,
}

impl Session {
    pub fn new(id: SessionId, workspace: WorkspaceId, cwd: PathBuf, kind: Kind) -> Self {
        let now = SystemTime::now();
        Session {
            id,
            workspace,
            state: State::Starting,
            kind,
            pty: None,
            pid: None,
            cwd,
            title: None,
            transcript_path: None,
            archived_transcript: None,
            transcript_archived: false,
            recovery: None,
            created_at: now,
            state_since: now,
            dirty_paths: HashSet::new(),
            boundary_violations: Vec::new(),
            last_reconcile: None,
        }
    }

    pub fn is_automation(&self) -> bool {
        matches!(self.kind, Kind::Automation { .. })
    }

    pub fn set_state(&mut self, state: State) {
        if self.state != state {
            tracing::debug!(session = %self.id, from = ?self.state, to = ?state, "state");
            self.state = state;
            self.state_since = SystemTime::now();
        }
    }

    /// Sort key for the rail. Automation sits second-from-bottom on purpose: a
    /// run in progress is unattended by definition. It promotes back into the
    /// attention band only on failure (§9).
    pub fn sort_rank(&self) -> u8 {
        let base = self.state.rank();
        if self.is_automation() && base >= 2 && !matches!(self.state, State::Archived { .. }) {
            // Working automation ranks below interactive Working; a failing or
            // waiting one keeps its own rank.
            if matches!(self.state, State::Working | State::Starting) {
                return 4;
            }
        }
        base
    }
}

// ---------------------------------------------------------------------------
// Process
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "health", rename_all = "snake_case")]
pub enum Health {
    Starting,
    Ok,
    Failing { summary: String },
    Dead,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcKind {
    /// Declared per workspace in config. Health is parsed from output.
    Managed { command: Vec<String> },
    /// A plain `$SHELL` opened on demand in any workspace. No health parsing,
    /// no restart policy, no rail entry — this is what makes the drawer
    /// agnostic (§2).
    Shell { exit_code: Option<i32> },
}

/// Any non-Claude pty owned by a workspace. Same hosting as a session pty —
/// ring buffer, reattach — but no hook lifecycle and no rail entry.
#[allow(dead_code)] // pid feeds the /proc preflight; started_at the dead-shell footer
pub struct Process {
    pub id: String,
    pub name: String,
    pub kind: ProcKind,
    pub health: Health,
    /// Always the owning workspace's path — the same directory as the Claude
    /// session above it.
    pub cwd: PathBuf,
    pub pty: Option<Arc<PtyHandle>>,
    pub pid: Option<u32>,
    /// Retained so a dead shell keeps its buffer and shows its exit code until
    /// dismissed.
    pub started_at: SystemTime,
}

impl Process {
    pub fn is_managed(&self) -> bool {
        matches!(self.kind, ProcKind::Managed { .. })
    }
}

// ---------------------------------------------------------------------------
// Changed files (§4)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Staged,
    Unstaged,
    Untracked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub status: FileStatus,
    /// Two-letter XY code from `git status --porcelain=v2`, kept verbatim.
    pub code: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileSet {
    pub staged: Vec<ChangedFile>,
    pub unstaged: Vec<ChangedFile>,
    pub untracked: Vec<ChangedFile>,
}

pub type FileSets = HashMap<WorkspaceId, FileSet>;
