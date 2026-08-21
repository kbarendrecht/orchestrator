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
    Worktree {
        name: String,
    },
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
    /// Opened, never prompted. Idle in the sense that matters to the guards —
    /// nothing is running and nothing will until you type — but not idle in the
    /// sense the rail shouts about, because you only just opened it.
    Ready,
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
            // A finished turn outranks a session you have not typed into yet.
            State::YourTurn { reason, .. } => {
                if *reason == TurnReason::Ready {
                    3
                } else {
                    2
                }
            }
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

    /// Whether an agent is actually doing something here.
    ///
    /// Not the same as "live": a session waiting at its prompt, or one stopped
    /// on a red build, is live but idle. Actions that would fight a running
    /// agent — rebasing under it, starting `fix-pr` on its branch — ask this,
    /// not `is_live`.
    pub fn is_busy(&self) -> bool {
        matches!(self, State::Working | State::Starting)
    }

    /// Whether this is idle time worth surfacing as attention.
    ///
    /// A session you opened a moment ago and have not typed into is not an
    /// agent waiting on you, so it does not join the count (§2's metric is
    /// agent-minutes lost, not terminal-minutes open).
    pub fn wants_attention(&self) -> bool {
        match self {
            State::YourTurn { reason, .. } => *reason != TurnReason::Ready,
            State::BuildFailing { .. } | State::Error { .. } => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    Interactive,
    /// An ordinary session whose first prompt is one of the vendored prompts in
    /// `commands/` (§8). Nothing about it needs a separate view.
    ///
    /// `alias = "skill"` so records written before the rename still load: these
    /// were called skills when they resolved from the agent's command path.
    Automation {
        pr: u64,
        #[serde(alias = "skill")]
        command: String,
    },
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
    /// The agent's own credential, and only for asking.
    ///
    /// `ORCHD_TOKEN` opens all 41 API routes, including the ones that post to
    /// GitHub and tear down worktrees. Handing that to the agent would make "the
    /// daemon owns outward writes" a sentence in a prompt rather than something
    /// the API enforces, so a session gets a token of its own that unlocks the
    /// interaction routes and nothing else. Never serialized: it lives as long as
    /// the process it was minted for.
    pub ask_token: String,
    /// What this session is blocked on, waiting for you to answer.
    ///
    /// The one place the daemon holds state *for* a running agent rather than
    /// about it: the agent asks, the SPA renders this from the snapshot, and the
    /// answer releases the tool call the agent is sitting in.
    pub interaction: Option<Interaction>,
    /// Whether the last turn was cut off rather than allowed to finish.
    ///
    /// Every resumed session comes back `YourTurn { Ready }`: `SessionStart`
    /// cannot tell what the conversation was doing before the daemon went down,
    /// so one that was killed mid-sentence and one that had finished look
    /// identical at the prompt. This is the difference, carried across the
    /// restart, and it is what makes "continue" a true thing to say to some of
    /// them and an invented instruction to the rest.
    ///
    /// Maintained by [`Session::set_state`], because a turn starting and a turn
    /// ending are the only two things that change the answer.
    pub interrupted: bool,
    /// The conversation this one was cut from, if it was forked.
    ///
    /// Kept as the id rather than a flag: the fork shares every earlier turn with
    /// it, so knowing *which* one is the difference between two rows that read
    /// identically and two you can tell apart.
    pub forked_from: Option<SessionId>,
    /// Written into the pty once `SessionStart` fires.
    ///
    /// `initialUserMessage` is only honoured in non-interactive mode, and a
    /// `/resolve` session is interactive so you can take it over mid-flight — so
    /// the invocation is typed in instead (§8).
    pub pending_prompt: Option<String>,
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
            interrupted: false,
            forked_from: None,
            pending_prompt: None,
            // Always a real one, so an empty stored token can never match an
            // empty header.
            ask_token: crate::state::random_token(),
            interaction: None,
        }
    }

    pub fn is_automation(&self) -> bool {
        matches!(self.kind, Kind::Automation { .. })
    }

    pub fn set_state(&mut self, state: State) {
        // A turn in flight is a turn that can be lost: closing the app or killing
        // the process takes whatever the agent was part-way through with it. Only
        // a turn that actually ends clears it — `Ready` deliberately does not,
        // since that is the state a resumed session comes back in and the whole
        // point is that it still owes you the rest of the turn.
        match &state {
            State::Working => self.interrupted = true,
            State::YourTurn { reason, .. } if *reason != TurnReason::Ready => {
                self.interrupted = false;
            }
            _ => {}
        }
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
// Interaction
// ---------------------------------------------------------------------------

/// A question a running session is blocked on.
///
/// Hooks are one-way and the only daemon-to-agent path is a pty write, so this
/// is the first thing that travels *back*: the agent posts a question, blocks on
/// a long poll, and the answer you give in the overlay is what its tool call
/// returns. Deliberately structured rather than free text, so the overlay renders
/// buttons instead of asking you to type into a terminal you cannot see.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Interaction {
    pub id: Uuid,
    /// What the agent is working on, so the card can say which thread this is
    /// about without the agent having to repeat it in the question.
    #[serde(default)]
    pub thread_id: Option<String>,
    pub question: String,
    /// Optional context: a diff, a file, the reviewer's words. Rendered as-is.
    #[serde(default)]
    pub detail: Option<String>,
    /// What you may answer. Never empty: an open question with no options is a
    /// prompt for prose the overlay has no box for.
    pub options: Vec<InteractionOption>,
    pub asked_at: SystemTime,
    /// Set when you answer, which is what releases the agent's poll.
    #[serde(default)]
    pub answer: Option<String>,
    /// The words you typed, when the option you picked asked for some.
    #[serde(default)]
    pub answer_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InteractionOption {
    /// What comes back to the agent. Its own vocabulary, not the label, so the
    /// prompt can branch on a stable word while the card stays readable.
    pub value: String,
    pub label: String,
    #[serde(default)]
    pub sub: String,
    /// This option wants words with it: the escape hatch, for when none of the
    /// others fit. The *value* still comes from the agent's own vocabulary, so it
    /// branches on a word it wrote; your text rides along as the reason.
    #[serde(default)]
    pub free: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn your_turn(reason: TurnReason) -> State {
        State::YourTurn {
            since: SystemTime::now(),
            reason,
        }
    }

    /// The rail called four resumed sessions "paused mid-work" when one of them
    /// had finished before the restart. `Ready` cannot tell them apart; this can.
    #[test]
    fn only_a_turn_that_was_cut_off_survives_as_interrupted() {
        let mut s = Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            std::path::Path::new("/tmp").to_path_buf(),
            Kind::Interactive,
        );
        assert!(!s.interrupted, "a session that has done nothing owes no turn");

        s.set_state(State::Working);
        assert!(s.interrupted);

        // Killed mid-turn: `Exited` must not clear it, or the record loses the
        // one fact the restart needs.
        s.set_state(State::Exited);
        assert!(s.interrupted);

        // A resumed one stays interrupted at its prompt, which is what makes it
        // the session "continue" is true of.
        s.set_state(your_turn(TurnReason::Ready));
        assert!(s.interrupted);

        // A turn that ends is not an interrupted one.
        s.set_state(your_turn(TurnReason::TurnComplete));
        assert!(!s.interrupted);
    }

    #[test]
    fn a_freshly_opened_session_is_not_busy() {
        // The bug this exists for: it read as Working, so rebasing in that
        // workspace was refused as "a session is working here".
        assert!(!your_turn(TurnReason::Ready).is_busy());
    }

    #[test]
    fn only_a_running_agent_counts_as_busy() {
        assert!(State::Working.is_busy());
        assert!(State::Starting.is_busy());
        assert!(!your_turn(TurnReason::TurnComplete).is_busy());
        // Stopped on a red build is idle too: it reached Stop and wants you.
        assert!(!State::BuildFailing {
            summary: "x".into()
        }
        .is_busy());
        assert!(!State::Exited.is_busy());
    }

    #[test]
    fn ready_is_idle_without_demanding_attention() {
        assert!(!your_turn(TurnReason::Ready).wants_attention());
        assert!(your_turn(TurnReason::TurnComplete).wants_attention());
        assert!(your_turn(TurnReason::AskedAQuestion).wants_attention());
        assert!(State::BuildFailing {
            summary: "x".into()
        }
        .wants_attention());
        assert!(!State::Working.wants_attention());
    }

    #[test]
    fn a_finished_turn_outranks_one_you_never_typed_into() {
        assert!(your_turn(TurnReason::TurnComplete).rank() < your_turn(TurnReason::Ready).rank());
        assert!(
            State::BuildFailing {
                summary: "x".into()
            }
            .rank()
                < your_turn(TurnReason::TurnComplete).rank()
        );
    }
}
