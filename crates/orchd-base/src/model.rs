use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use uuid::Uuid;

use crate::pty::PtyHandle;

pub type SessionId = Uuid;

/// The eight characters a session goes by in logs and refusals.
pub fn short_id(id: &Uuid) -> String {
    id.to_string()[..8].to_string()
}
/// `"main"` for the privileged checkout, the worktree name otherwise.
pub type WorkspaceId = String;

pub const MAIN: &str = "main";

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "base.d.ts")
)]
pub enum WorkspaceKind {
    /// Docker stack, dev URL, `ng build --watch`.
    Main,
    Worktree {
        name: String,
    },
}

/// What `reconcile` last measured about a workspace's tree.
///
/// One struct on the `Workspace` rather than four maps keyed by workspace id
/// beside it. The maps were the older shape and leaked: teardown removed the
/// workspace but not its entries, and worktree ids are deterministic and reused
/// (`pr-<n>`), so recreating a torn-down workspace served the previous
/// incarnation's file list and counts until the next reconcile overwrote them.
/// Living on the entity, this cannot outlive what it describes.
///
/// `Default` is "measured nothing yet", which is also what a fresh workspace
/// shows before its first reconcile.
#[derive(Debug, Default, Clone)]
pub struct Tree {
    /// Everything changed since the branch point, committed and untracked both —
    /// or the first `state::CHANGED_CAP` of it, sorted by path.
    ///
    /// Truncated here rather than on the way out, so the big `Vec` is never held at
    /// all: this is cloned into every workspace of every snapshot, and a snapshot is
    /// built on every `notify`.
    pub changed: Vec<crate::model::DiffFile>,
    /// What `changed` would have been, uncapped. Equal to `changed.len()` in the
    /// ordinary case; larger is how the pane knows to say so.
    pub changed_total: u32,
    /// What this tree has checked out *now*.
    ///
    /// Not to be confused with `Workspace::branches`, which accumulates every branch
    /// a tree has ever held and is never pruned (§2) — a PR still belongs to the
    /// session that made it after you have moved on. That set cannot answer "is main
    /// free", and asking it anyway is what made the rail offer to *swap* with a main
    /// that was already parked on its base.
    pub branch: Option<String>,
    /// `merge-base(upstream, HEAD)` — the commit `changed` is measured from.
    pub base: Option<String>,
    /// (behind, ahead) against the upstream base.
    pub divergence: (u32, u32),
    /// Commits this branch holds that its own remote does not.
    ///
    /// Not derivable from `divergence`, which is measured against the *base*: a
    /// branch three commits ahead of `upstream/develop` may have pushed all three
    /// or none of them.
    pub unpushed: u32,
    pub rebasing: bool,
    /// Whether `reconcile` has ever filled the fields above.
    ///
    /// **The one thing `Default` could not express.** Every field here is a
    /// measurement whose "not measured yet" value is indistinguishable from a real
    /// answer: no changed files reads as a clean tree, `changed_total` 0 as zero
    /// files, `divergence` (0,0) as up to date. That was harmless while the first
    /// sweep finished before the window opened. It stopped being harmless when the
    /// sweep moved off the critical path, because then the pane is on screen while
    /// the answer is still unknown, and a clean-looking tree is a lie the user
    /// cannot see through. The pane shows a loader on `false`.
    pub measured: bool,
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
    /// Last reconcile's measurements. See [`Tree`] for why this is not a set of
    /// side maps.
    pub tree: Tree,
    /// Uncommitted work parked out of a rebase's way, if any is.
    ///
    /// **Not on [`Tree`], deliberately.** Everything there is measured by the
    /// sweep, and this is not: the daemon knows because it did the banking, and
    /// asking git per workspace per sweep would be an eighth exec on a walk whose
    /// whole cost is execs. A restart re-derives every one of these with a single
    /// `git for-each-ref` on main, since refs are per-repository.
    pub banked: Option<crate::model::Bank>,
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
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "base.d.ts")
)]
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
    /// You cut the turn short — an escape or a `^C` into the pane.
    ///
    /// Its own reason because **`Stop` does not fire on a user interrupt**, so
    /// nothing else ever ends that turn: a session stayed `Working` until its next
    /// completed turn, which for one abandoned mid-thought is never. Main sat that
    /// way for eight minutes with a live, idle agent in it, and the rail said it was
    /// working the whole time.
    ///
    /// Not folded into `TurnComplete`, because the two disagree about what is owed:
    /// see the `interrupted` arm of [`Session::set_state`].
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "base.d.ts")
)]
pub enum State {
    Starting,
    Working,
    YourTurn {
        #[cfg_attr(
            any(test, feature = "test-util"),
            ts(type = "{ secs_since_epoch: number, nanos_since_epoch: number }")
        )]
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
    /// Rail ordering (§9): BuildFailing → YourTurn → Working → Archived.
    ///
    /// One rank per state and nothing else. A run started with a skill used to be
    /// demoted below an interactive `Working`, on the reasoning that it is
    /// unattended by definition; it is an ordinary session now, and it sits where
    /// its state puts it.
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

/// The PR pass a session was started to run, when it was started as one.
///
/// **One field rather than two.** `pr` and `command` are inseparable — a pass with
/// no PR means nothing, and a PR with no pass is just a session that happens to be
/// on a branch — so they live in one `Option` and the impossible pair cannot be
/// written down.
///
/// **What reads `command`, and why a skill cannot answer it.** The instructions
/// live in `skills/` now, so this is no longer "which prompt did the daemon type".
/// It is the five things the daemon has to know *around* the agent rather than
/// inside it: which settle path an exit dispatches to (`spawn::watch_session_exit`),
/// which spawn is handed `ORCH_POST_TOKEN` (`triage::posts_proposals`), that a
/// resolve run may not ask questions (`api::ask`), that one triage pass per PR is
/// enough (`triage::is_triage_of`), and which PR the review bar is reporting on.
/// Every one of those happens before the first turn or after the last.
///
/// It carries **no opinion about attention.** `Kind::Automation` used to, and the
/// rail demoted a running pass and painted it teal on the strength of it — which
/// went wrong the moment a pass meant "a pane you are watching" as well as "a run
/// nobody is". A session is a session; its state says whether it wants you.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "base.d.ts")
)]
pub struct Pass {
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub pr: u64,
    pub command: String,
}

/// An outstanding "may this session reach that folder?" question.
///
/// Both halves are needed at the moment the answer lands: the id says the question
/// was the daemon's own rather than one the agent wrote for itself, and the path
/// says what a yes grants. See [`Session::outside_ask`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutsideAsk {
    pub id: Uuid,
    pub path: PathBuf,
}

/// How to rebuild a torn-down worktree so an archived session can be resumed (§2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "recovery", rename_all = "snake_case")]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "base.d.ts")
)]
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
    /// The PR pass this session runs, if it was started as one. See [`Pass`].
    pub pass: Option<Pass>,
    pub pty: Option<Arc<PtyHandle>>,
    pub pid: Option<u32>,
    pub cwd: PathBuf,
    pub title: Option<String>,
    /// A name you typed, which stands in for `title` for as long as it is set.
    ///
    /// Its own field rather than a write into `title`, because `Stop` refreshes the
    /// ai-title on every turn (`hooks::stop`) — a rename stored there would hold
    /// until the agent finished its next turn and then silently revert. Cleared by
    /// renaming to nothing, which hands the row back to Claude Code's own naming.
    pub name: Option<String>,
    pub transcript_path: Option<PathBuf>,
    pub archived_transcript: Option<PathBuf>,
    /// True once archiving has been settled: either the transcript was copied,
    /// or there was none to copy. A session killed before its first turn never
    /// gets a `.jsonl`, and must not block teardown forever.
    pub transcript_archived: bool,
    pub recovery: Option<ArchiveState>,
    pub created_at: SystemTime,
    pub state_since: SystemTime,
    /// The branch this conversation is about, as opposed to the tree it sits in.
    ///
    /// Those are the same thing right up until a swap, which exchanges what two
    /// trees have checked out. Without this the daemon can only pair a
    /// conversation with a *directory*, so a swap moved the branch and left the
    /// conversation behind: a pane whose transcript and whose changed files were
    /// about different pieces of work, with nothing saying so.
    ///
    /// Read from the tree when a session is created and carried across a resume or
    /// a fork, because a conversation already knows its branch and the tree it
    /// comes back to may have drifted. `None` for a record written before this
    /// existed, which is why nothing here ever treats it as an assertion: an
    /// unknown branch travels nowhere.
    pub branch: Option<String>,
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
    /// The folders you said this session may run git in, beyond its own worktree.
    ///
    /// `guard::isolation` refuses every other one by default, and the refusal
    /// tells the agent to ask; `orch outside <path>` puts the question to you
    /// through the ordinary ask box and this is what a yes appends.
    ///
    /// **One folder per yes, not one per session.** It was a `bool`, so the first
    /// grant — a `git -C` at one checkout you had a reason for — let the session
    /// reach *every* checkout for the rest of the conversation, and the question
    /// that named a folder was answered about all of them. A grant covers the
    /// folder it names and what is under it ([`Session::outside_granted`]), which
    /// is the smallest thing that still answers the case the ask is raised for.
    ///
    /// **Deliberately not on `store::SessionRecord`**: these are
    /// decisions about the conversation in front of you, and a restart is exactly
    /// the moment to ask again rather than to assume.
    ///
    /// Compared textually, like the rule that reads them: `guard::resolve` folds
    /// `.` and `..` and follows no symlink, and a grant that canonicalised would
    /// stop matching the paths the refusal names.
    pub outside_grants: Vec<PathBuf>,
    /// The ask that would grant one, and the folder it would grant.
    ///
    /// The id is here so only the daemon's own question can grant: without it the
    /// grant would key on an option *value*, and an agent can write any value it
    /// likes into `orch ask` — it would be asking itself. The path is here because
    /// the answer route sees an id and an option, and the folder being granted is
    /// no longer derivable from either.
    pub outside_ask: Option<OutsideAsk>,
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
    /// Whether a turn has *ever* started in this session.
    ///
    /// A session that spawned and was never typed into owns a headers-only
    /// transcript, which `claude --resume` opens and then exits — so "is there a
    /// file" is the wrong question before a resume or a fork, and this is the right
    /// one. Set once, when the first `Working` is entered, and never cleared: the
    /// point is only whether the conversation ever became real. Read in place of a
    /// per-snapshot file stat, so the answer is free on the hot path.
    pub had_a_turn: bool,
    /// The conversation this one was cut from, if it was forked.
    ///
    /// Kept as the id rather than a flag: the fork shares every earlier turn with
    /// it, so knowing *which* one is the difference between two rows that read
    /// identically and two you can tell apart.
    pub forked_from: Option<SessionId>,
    /// The session whose `orch new` created this one.
    ///
    /// What makes `orch kill` an undo rather than a remote control: the ask token
    /// opens asking and spawning, so the matching destroy must reach exactly the
    /// sessions that same token brought into being and nothing else. Without this
    /// an agent that misread an id could end the conversation you were sitting in.
    pub spawned_by: Option<SessionId>,
    /// Whether that same `orch new` cut the worktree this session sits in.
    ///
    /// Separate from [`Session::spawned_by`] because it decides a *destructive*
    /// step: discarding a session the daemon cut a tree for should take the tree
    /// with it, and discarding one that was put into a tree you already had must
    /// leave that tree alone. Teardown's preflight would refuse a dirty or unpushed
    /// tree either way, but a clean worktree of yours is not the agent's to remove.
    pub spawn_cut_worktree: bool,
    /// A one-off message for the agent, delivered at its next prompt.
    ///
    /// The daemon can move a conversation between checkouts, and until this the
    /// agent was never told: it came back believing it was in the tree it had been
    /// reading all along, and the first thing it did with a remembered path was
    /// wrong. `UserPromptSubmit` is the moment to say so: it is the only hook that
    /// can hand the agent context, and until the human speaks to the session there
    /// is nothing for the note to change.
    ///
    /// Taken, not read: delivered exactly once. Carried across a resume and
    /// persisted, because a session moved while it was not running is told when
    /// auto-resume brings it back, which may be days later.
    pub arrival_notice: Option<String>,
    /// Written into the pty once `SessionStart` fires.
    ///
    /// `initialUserMessage` is only honoured in non-interactive mode, and a
    /// `/resolve` session is interactive so you can take it over mid-flight — so
    /// the invocation is typed in instead (§8).
    pub pending_prompt: Option<String>,
    /// Set when a review session reports it is done and the PR still has checks to
    /// watch, so its *exit* starts a `fix-pr` run (`api::session_handoff`).
    ///
    /// The exit, and not the call, because the run cannot start while this session
    /// is alive: `fix_pr::evaluate` refuses a branch with a live session on it, and
    /// it is right to — two agents in one worktree is the thing that guard exists
    /// for. So the handoff kills the pty and `spawn::watch_session_exit`, the one
    /// observer of a pty ending, starts the run.
    ///
    /// Not persisted: it describes a window that ends with the pty, and a daemon
    /// that went down inside it has lost the review session anyway — restoring a
    /// flag that force-pushes on the strength of a call nothing can now attribute is
    /// exactly what must not happen. It *is* in the snapshot, as
    /// `SessionView::handed_off`, because the overlay has to know a review ended
    /// this way before the run it handed to exists.
    pub fix_pr_on_exit: bool,
}

impl Session {
    pub fn new(id: SessionId, workspace: WorkspaceId, cwd: PathBuf, pass: Option<Pass>) -> Self {
        let now = SystemTime::now();
        Session {
            id,
            workspace,
            state: State::Starting,
            pass,
            pty: None,
            pid: None,
            cwd,
            title: None,
            name: None,
            transcript_path: None,
            archived_transcript: None,
            transcript_archived: false,
            recovery: None,
            created_at: now,
            state_since: now,
            branch: None,
            arrival_notice: None,
            dirty_paths: HashSet::new(),
            boundary_violations: Vec::new(),
            last_reconcile: None,
            interrupted: false,
            had_a_turn: false,
            forked_from: None,
            spawned_by: None,
            spawn_cut_worktree: false,
            outside_grants: Vec::new(),
            outside_ask: None,
            pending_prompt: None,
            fix_pr_on_exit: false,
            // Always a real one, so an empty stored token can never match an
            // empty header.
            ask_token: crate::secret::random_token(),
            interaction: None,
        }
    }

    /// What to call this session: the name you gave it, else Claude Code's own.
    ///
    /// Every place that names a session in words goes through here, so a rename
    /// shows up in the refusals too — "main already has a live session (X)" naming
    /// a title you no longer use is the same bug as a rail row doing it.
    pub fn label(&self) -> Option<&str> {
        self.name.as_deref().or(self.title.as_deref())
    }

    /// May this session run git in `path`, because you said so?
    ///
    /// A grant covers the folder it names and everything under it, which is what
    /// makes one yes enough for the command that raised the question: an agent
    /// refused at `/repo` is usually aimed at `/repo` and then at something inside
    /// it. Prefix matching, textual, like [`crate::guard`]'s own rule.
    pub fn outside_granted(&self, path: &std::path::Path) -> bool {
        self.outside_grants.iter().any(|g| path.starts_with(g))
    }

    /// Whether resuming this session would find a conversation to continue.
    ///
    /// Two conditions, both load-bearing. `TranscriptOnly` means the branch and
    /// sha are gone, so the worktree cannot be rebuilt to resume *into*. And
    /// `had_a_turn` is the same strong question fork, nudge and prune all ask: a
    /// turnless session owns a headers-only transcript, so `resumable: true` on
    /// one promises a `--resume` that finds nothing and exits instantly. The rail
    /// offered that resume, which is the weak "is there a file" answer this bit
    /// replaced everywhere else.
    ///
    /// The single reader of both fields, because three call sites computed it
    /// apart and only one of them would have been fixed.
    pub fn resumable(&self) -> bool {
        self.had_a_turn && !matches!(self.recovery, Some(ArchiveState::TranscriptOnly))
    }

    pub fn set_state(&mut self, state: State) {
        // A turn in flight is a turn that can be lost: closing the app or killing
        // the process takes whatever the agent was part-way through with it. Only
        // a turn that actually ends clears it — `Ready` deliberately does not,
        // since that is the state a resumed session comes back in and the whole
        // point is that it still owes you the rest of the turn.
        match &state {
            State::Working => {
                self.interrupted = true;
                // `Working` is only ever entered off a prompt (`UserPromptSubmit`,
                // or the nudge that resumes a polling agent), so it is the moment a
                // conversation stops being an empty pane. Latched, never cleared.
                self.had_a_turn = true;
            }
            // `Interrupted` joins `Ready` in *not* clearing it, and for the same
            // reason: both are turns that stopped without finishing, so the
            // conversation still owes you the rest of one. Clearing it here would
            // take the resume nudge away from the sessions that most need it — the
            // ones you cut off yourself.
            State::YourTurn { reason, .. }
                if !matches!(reason, TurnReason::Ready | TurnReason::Interrupted) =>
            {
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
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "base.d.ts")
)]
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
    #[cfg_attr(
        any(test, feature = "test-util"),
        ts(type = "{ secs_since_epoch: number, nanos_since_epoch: number }")
    )]
    pub asked_at: SystemTime,
    /// Set when you answer, which is what releases the agent's poll.
    #[serde(default)]
    pub answer: Option<String>,
    /// The words you typed, when the option you picked asked for some.
    #[serde(default)]
    pub answer_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "base.d.ts")
)]
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
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "base.d.ts")
)]
pub enum Health {
    Starting,
    Ok,
    Failing { summary: String },
    Dead,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "base.d.ts")
)]
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
pub struct Process {
    pub id: String,
    pub name: String,
    pub kind: ProcKind,
    pub health: Health,
    /// Always the owning workspace's path — the same directory as the Claude
    /// session above it.
    pub cwd: PathBuf,
    pub pty: Option<Arc<PtyHandle>>,
}

impl Process {
    pub fn is_managed(&self) -> bool {
        matches!(self.kind, ProcKind::Managed { .. })
    }
}

// ---------------------------------------------------------------------------
// Changed files (§4)
//
// None of the three below carries a `ts_rs` export: `git::status` still produces
// them, but nothing reaches them from `Snapshot` any more — the changed-files
// pane reads `changed`, and `WorkspaceView.files` was sent to every client on
// every tick and read by none.
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

// **Here rather than in `diff`, which is what measures it.** `Tree::changed` is
// a `Vec` of these, so a type in `diff` meant the data model importing the
// module that fills it while that module imported the model back — one of the
// seventeen mutual pairs `mise run check-modules` counts. The rule the two
// halves now follow: a *shape* lives here, and the module that produces it
// depends on this one.
//
// A plain comment, not a doc one: `ts-rs` copies doc comments into
// `snapshot.d.ts`, and an argument about Rust module layering is not something
// the SPA's type file should carry.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "base.d.ts")
)]
pub struct DiffFile {
    pub path: String,
    /// Verbatim from `--name-status`: M, A, D, R…, C…
    pub status: String,
    pub added: u32,
    pub deleted: u32,
    pub binary: bool,
    /// Whether the client should fetch hunks without being asked.
    pub eager: bool,
    /// Present for renames.
    pub old_path: Option<String>,
    /// Whether this file has changes in the **index**, and whether it has changes
    /// in the **working tree** — `git status`'s two answers, joined on by path.
    ///
    /// **Not derivable from `status` above, and that is the point.** This list is
    /// `git diff <merge-base>`, so most rows on a PR branch differ from the base
    /// because of a *commit* and are otherwise clean. Offering "discard changes"
    /// against that list would be offering to throw away nothing on some rows and
    /// a commit's content on others, from a menu that cannot tell them apart. The
    /// pane's git verbs are drawn from these two instead, so what is offered is
    /// exactly what exists: staged → unstage, working-tree → stage, discard.
    ///
    /// Both `false` is the ordinary case (changed in a commit, clean on disk) and
    /// gets no verbs at all.
    pub staged: bool,
    pub unstaged: bool,
}

impl DiffFile {
    /// A file git has never seen. `git diff` cannot report one, so the pane's
    /// list would be missing exactly the files a session just created.
    ///
    /// No line counts: counting them means reading every new file on every
    /// reconcile, and an untracked file is entirely new by definition — the
    /// number would only ever say "all of it".
    pub fn untracked(f: &crate::model::ChangedFile) -> Self {
        DiffFile {
            path: f.path.clone(),
            status: "?".to_string(),
            added: 0,
            deleted: 0,
            binary: false,
            // Nothing to diff against, so there are no hunks to fetch.
            eager: false,
            old_path: None,
            // Untracked is neither: nothing of it is in the index, and there is no
            // tracked version for the working tree to differ from.
            staged: false,
            unstaged: false,
        }
    }
}

/// Work parked out of the way of a rebase, and how much of it there is.
// Here for the same reason as `DiffFile` above: `Workspace::banked` is one of
// these, and `git` is the module that makes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bank {
    /// The WIP commit. Named in every message about it, because
    /// `git stash apply <sha>` is the recovery a person can run without us.
    pub sha: String,
    /// Tracked files in it, for a strip that says "3 changed files are banked".
    pub files: u32,
}

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
            None,
        );
        assert!(
            !s.interrupted,
            "a session that has done nothing owes no turn"
        );

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

    /// An interrupted turn is a turn still owed, so the flag the resume nudge reads
    /// has to survive it.
    ///
    /// The trap this guards: `set_state` clears `interrupted` on *every* `YourTurn`
    /// but `Ready`, so giving the interrupt any other reason would have ended the
    /// turn correctly in the rail and quietly dropped the session out of the one
    /// button that offers to finish it.
    #[test]
    fn an_interrupted_turn_is_still_owed() {
        let mut s = Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            std::path::Path::new("/tmp").to_path_buf(),
            None,
        );
        s.set_state(State::Working);
        assert!(s.interrupted);

        s.set_state(your_turn(TurnReason::Interrupted));
        assert!(s.interrupted, "you cut it off; it still owes you the rest");
        // And it is idle, so nothing that refuses to run under a working agent is
        // held back by a turn nobody is taking.
        assert!(!s.state.is_busy());
        assert!(s.state.wants_attention(), "stopped, and it is your move");

        // A turn that really finished clears it, which is the contrast that makes
        // the reason worth having.
        s.set_state(State::Working);
        s.set_state(your_turn(TurnReason::TurnComplete));
        assert!(!s.interrupted);
    }

    /// A yes about one folder is a yes about that folder, and the sibling next to
    /// it is a different question.
    ///
    /// Pinned because the field was a `bool`: the first grant let the session reach
    /// every checkout for the rest of the conversation, and the question that named
    /// a folder had answered about all of them.
    #[test]
    fn a_grant_covers_one_folder_and_what_is_under_it() {
        let mut s = Session::new(
            uuid::Uuid::new_v4(),
            "wt".into(),
            std::path::Path::new("/repo/.worktrees/invoice").to_path_buf(),
            None,
        );
        assert!(
            !s.outside_granted(std::path::Path::new("/repo")),
            "nothing is granted yet"
        );

        s.outside_grants.push(PathBuf::from("/repo"));
        assert!(s.outside_granted(std::path::Path::new("/repo")));
        // Under it, which is where an agent refused at a checkout aims next.
        assert!(s.outside_granted(std::path::Path::new("/repo/apps/web")));
        // A sibling is not under it, and neither is a name that merely starts the
        // same way — `starts_with` compares components, not characters.
        assert!(!s.outside_granted(std::path::Path::new("/other")));
        assert!(!s.outside_granted(std::path::Path::new("/repo-two")));
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
