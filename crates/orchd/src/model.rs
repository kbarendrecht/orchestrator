//! The daemon's own domain model: workspaces, sessions, processes, and the state
//! a session moves through.
//!
//! **Here rather than in `orchd-base`, because none of it is a primitive.**
//! `Session` holds a live `PtyHandle`, an ask token and an interaction; `State`
//! ranks the rail and answers `wants_attention`. `orchd-base`'s own criterion was
//! "nothing that keeps runtime state", and this half never met it — it was down
//! there only because `git` needs the file and diff value types, which stay.
//!
//! The glob below is what makes that split cost no call site a rename:
//! `crate::model::DiffFile` and `crate::model::Session` still resolve through one
//! path, and `use crate::model::*` still brings in both halves.

pub use orchd_base::model::*;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
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
// Cutting a worktree
// ---------------------------------------------------------------------------

/// What a worktree cut is doing, while it does it.
///
/// **Cutting a tree is other people's scripts, and they are the slow part.** The
/// repo's `WorktreeCreate` fetches, checks out 18k files and warms git; the
/// configured `worktree_init` and `worktree_setup` run on top. The board said
/// `creating a worktree` for all of it, so ten seconds of real work looked
/// identical to a button that had done nothing.
///
/// One slot per daemon rather than one per create. Two cuts at once is possible
/// through the API and the board already refuses it (`asTheOnlyCreate`), so a map
/// would buy a key nobody reads for a case nobody can reach from the page.
///
/// Here in `model` because `orchd`'s `Snapshot` carries it and `state` may
/// not import the modules that fill it — a shape lives in `model`, the module that
/// fills it depends on `model`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct CreateRun {
    /// The worktree being made. Carried so the pane can name it once the create
    /// has moved on to a step that does not.
    pub name: String,
    /// The script running now, by the name it is configured under:
    /// `WorktreeCreate`, `worktree init`, `worktree setup`.
    pub step: String,
    /// False once the scripts are done. The session's own boot follows, and the
    /// board keeps its overlay up for that — so this going false is not the end of
    /// the create, only the end of what this reports on.
    pub running: bool,
    /// What the scripts said, oldest first, capped at [`CREATE_LINES`].
    pub lines: Vec<String>,
    /// The step that failed, and how. A worktree hook is never fatal, so a run can
    /// carry on past this — it is a note on the output, not a terminal state.
    pub failed: Option<String>,
}

/// How much of a create's output is kept.
///
/// A tail, not a log: this rides every snapshot while a create runs, and the pane
/// shows the last few lines of it. A script that prints a build is not something
/// to accumulate in memory and push over a websocket.
pub const CREATE_LINES: usize = 200;

/// Whether a cut reports to the board, or only to the log.
///
/// **[`CreateRun`] is one slot, not one per workspace.** A cut nobody asked for —
/// filling the spare pool — runs at the same time as one somebody did, and every
/// reporting call on the way down (`create_begin`, `create_step`, `create_lines`,
/// `create_failed`, `create_end`) writes that single slot without checking whose
/// it is. So a quiet cut would overwrite the step name, interleave its hook's
/// output into somebody's overlay, and flip `running` to false while their own
/// `WorktreeCreate` was still fetching.
///
/// Carried as an argument rather than inferred, because the two cuts are the same
/// code: the only thing that separates them is whether a person is waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Board {
    /// A person asked for this tree and is watching the overlay.
    Loud,
    /// The daemon decided to cut this tree. `tracing` is the only audience.
    Quiet,
}

impl Board {
    pub fn is_loud(self) -> bool {
        self == Board::Loud
    }
}

impl CreateRun {
    /// Add a line, dropping the oldest once the cap is reached.
    pub fn push(&mut self, line: String) {
        if self.lines.len() >= CREATE_LINES {
            self.lines.remove(0);
        }
        self.lines.push(line);
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
    ts(export, export_to = "snapshot.d.ts")
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
    ts(export, export_to = "snapshot.d.ts")
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

    /// Whether a respawn now costs nothing but the scrollback.
    ///
    /// **A question on screen is not idle, and that is the rule's one subtle
    /// half.** `--resume` reopens the conversation at its prompt and does not
    /// re-ask, so a restart under `AskedAQuestion` or `NeedsPermission` throws the
    /// question away while the agent waits on the answer to it. `Working` and
    /// `Starting` would lose the turn itself. Everything else has reached the
    /// prompt: a finished turn, a pane nobody has typed into, a turn you cut
    /// short, a red build, an error.
    pub fn safe_to_restart(&self) -> bool {
        match self {
            State::YourTurn { reason, .. } => !matches!(
                reason,
                TurnReason::AskedAQuestion | TurnReason::NeedsPermission
            ),
            State::BuildFailing { .. } | State::Error { .. } => true,
            State::Starting | State::Working | State::Exited | State::Archived { .. } => false,
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
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct Pass {
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub pr: u64,
    pub command: String,
}

impl Pass {
    /* **The six commands a run can carry, in one place.**

    Each is also the directory its vendored skill lives in, because the first turn
    is typed as `/orchd:<command> <pr>` — `skills_are_named_after_commands` asserts
    exactly that pair. A literal at the spawn site and another at the place that
    reacts to the exit is how the two stop agreeing without anything failing, which
    is why these were constants at all.

    They were one per feature module, and that is what made `spawn` import the two
    modules that import `spawn`: the spawn records the command, the route finds it,
    the rail colours by it and the exit watcher settles on it, so every one of those
    reached into `fix_pr` or `triage` for a string. The vocabulary belongs to the
    shape. */
    /// The run that rebases and force-pushes until CI is green (§8).
    pub const FIX_PR: &'static str = "fix-pr";
    /// The overlay review session: proposes, then carries out what you decide.
    pub const REVIEW: &'static str = "review";
    /// The pane pass over a PR's review threads, and the rail's default review verb.
    pub const HANDLE_REVIEW: &'static str = "handle-review";
    /// The tracker filer.
    pub const STORY: &'static str = "story";

    /// Does this run post proposals, and so need the credential for it?
    ///
    /// Asked by the *resume* path, which is the only caller that cannot see how
    /// the run was started.
    pub fn posts_proposals(command: &str) -> bool {
        command == Self::REVIEW
    }
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
    ts(export, export_to = "snapshot.d.ts")
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
    /// The idle state a respawn under the same id comes back in, instead of `Ready`.
    ///
    /// `SessionStart` turns a `Starting` record into `Ready`, which is right for a
    /// session you just opened and wrong for one the daemon restarted under you: a
    /// restart onto the installed Claude Code turned "turn complete" into "ready" and
    /// reset the wait clock to zero, and the rail then said you owed it nothing.
    /// Taken by that hook, so it applies once.
    pub comes_back_as: Option<State>,
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
    /// Respawn this session once it is safe to interrupt, so it picks up the
    /// `claude` installed now rather than the one it started with.
    ///
    /// **On the record, and cleared by nothing.** The restart rebuilds the record
    /// from [`Session::new`] under the same id, and that fresh record carries
    /// `false` — so the flag is gone the moment the respawn lands, and a restart
    /// that fails leaves it set for the watcher's next look.
    ///
    /// Not persisted, deliberately: a daemon that goes down respawns every
    /// session at boot, which is the restart this was waiting to do.
    pub restart_queued: bool,
    /// The file this session's agent was executed from, symlinks resolved — see
    /// [`orchd_base::pty::which`]. `None` until the pty exists.
    ///
    /// Not persisted: every spawn records its own, and a record restored from disk
    /// is respawned before anything asks.
    pub agent_exe: Option<std::path::PathBuf>,
    /// Whether that file is no longer what a new spawn here would run — the agent
    /// was upgraded underneath this session. Set by `update::mark_stale`, and
    /// cleared the same way a restart clears [`Session::restart_queued`]: the
    /// respawn's fresh record carries `false`.
    pub agent_stale: bool,
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
            comes_back_as: None,
            had_a_turn: false,
            forked_from: None,
            spawned_by: None,
            spawn_cut_worktree: false,
            outside_grants: Vec::new(),
            outside_ask: None,
            pending_prompt: None,
            fix_pr_on_exit: false,
            restart_queued: false,
            agent_exe: None,
            agent_stale: false,
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

    /// Start waiting for you, unless this session already is.
    ///
    /// **The wait clock is the metric, so a second notice about one idle turn must
    /// not restart it.** The rail sorts on how long a session has been waiting and
    /// the waitbar counts it, and Claude Code sends more than one notification for
    /// a single stop — a permission prompt and then the stop itself, say. Re-stamping
    /// `YourTurn` there says the session became free just now, and the row you have
    /// been ignoring longest moves to the bottom.
    ///
    /// **A method because the rule was written twice**, in `hooks.rs`, in two
    /// shapes — `if !matches!(s.state, State::YourTurn { .. })` in `notification`
    /// and an `already_waiting` local in `stop`. Two spellings of one rule is how
    /// the third site gets it wrong, and the state is this type's to own.
    pub fn wait_for(&mut self, reason: TurnReason) {
        if matches!(self.state, State::YourTurn { .. }) {
            return;
        }
        self.set_state(State::YourTurn {
            since: SystemTime::now(),
            reason,
        });
    }
}

/// A refusal the daemon knows the *kind* of.
///
/// **Everything used to be `400`,** including a git command that blew up — so a
/// session that is not there, a branch somebody else is working on and a genuine
/// daemon failure all said "your request was wrong", which for the last of those is
/// simply false. The sentence was the only signal, and matching on English is what
/// a status code exists to stop.
///
/// Two kinds, because two are what the daemon can tell apart without re-judging
/// every one of its forty-odd refusals: **`Missing`** is nothing here by that name,
/// and **`Busy`** is here, but held. Everything else stays `400`, which is what
/// most refusals honestly are — a request the daemon will never accept as written.
///
/// **Here rather than in `api`, because the kind is the daemon's and the status is
/// the HTTP layer's.** It started in `api` and the module gate refused it within
/// the hour: `state::no_such_session` builds one, so `state` imported `api`, which
/// imports everything — a ten-module cycle out of one `use`.
///
/// Carried as an `anyhow::Error` so `?` keeps working at every call site: the kind
/// is recovered by `api::ApiError::into_response` downcasting, which sees through a
/// `.context()` chain.
#[derive(Debug)]
pub enum Refusal {
    Missing(String),
    Busy(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Missing(what) | Refusal::Busy(what) => f.write_str(what),
        }
    }
}

impl std::error::Error for Refusal {}

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
    ts(export, export_to = "snapshot.d.ts")
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
    ts(export, export_to = "snapshot.d.ts")
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
    ts(export, export_to = "snapshot.d.ts")
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
    ts(export, export_to = "snapshot.d.ts")
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
// What an update looks like
// ---------------------------------------------------------------------------
//
// Three shapes the snapshot carries and `update` fills in. Here rather than in
// `update` because `state::Inner` holds all three, and a feature module that owns
// the shape its own caller stores is how `state` and `update` became one module
// with a line through it. `git::Bank` and `diff::DiffFile` moved for the same
// reason.

/// A newer agent build than the one installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct AgentUpdate {
    /// The mise tool name to upgrade — `claude-code` or `claude`, whichever this
    /// checkout pins. Carried rather than assumed so the button upgrades the tool
    /// that actually provides the binary.
    pub tool: String,
    pub current: String,
    pub latest: String,
}

/// An upgrade the daemon is running, or the failure it left behind.
///
/// The run used to be a process in main's drawer, which was the wrong home twice:
/// the drawer is *this workspace's* processes, and upgrading the agent belongs to
/// no workspace — so from any worktree the run was invisible, and main's drawer
/// grew a tab that was not a process of main's at all. It reports through the same
/// bar that offered the button instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct UpgradeRun {
    /// The version being installed. Carried so the bar can say it even after the
    /// check that found it has been refreshed away.
    pub to: String,
    pub running: bool,
    /// The tail of the output, for a run that failed. Empty while it runs, and
    /// empty on success — which, with `running` false, is how the bar tells the two
    /// finished states apart.
    ///
    /// Success used to clear the run outright, on the reasoning that the nudge
    /// going away *is* the report. It is not: the sessions you have open go on
    /// printing Claude Code's own upgrade notice, because they really are still the
    /// old build, so a bar that vanishes silently against a terminal that still
    /// says "update available" reads as a button that did nothing. It is reported,
    /// and dismissed like any other.
    pub tail: String,
}

/// A release newer than what is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub url: String,
    /// The mise tool that installed this binary, when one did.
    ///
    /// Still here because it is what `mise upgrade` is given, and because it is the
    /// one install kind that cannot be read off a path — `update::app_providing_tool`
    /// asks mise itself. What the *bar* branches on is [`Offer`], which this is only
    /// one input to.
    pub tool: Option<String>,
    /// What the bar may offer, and it is the whole of what the page decides from.
    ///
    /// The page used to derive this from `tool` being `None`, and got it wrong for
    /// everything that is not mise: it told a `.deb`, a cask, an AppImage and a
    /// checkout alike to "Run mise up". The daemon knows which install it is, so
    /// the daemon says what can be done about it.
    pub offer: Offer,
}

/// What the update bar may offer for this install.
///
/// Three arms because there are three honest answers, not because there are three
/// install kinds: something the app can run for you, something only you can run,
/// and nothing beyond the release link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub enum Offer {
    /// A button, and `command` is what pressing it runs — named so the tooltip
    /// does not have to rebuild the string the daemon already built.
    Button { command: String },
    /// No button: this needs a terminal, so the bar names the command instead of
    /// pretending it can run it.
    Advice { command: String },
    /// Neither. The release link is the whole offer, which is what a downloaded
    /// file or a checkout has always had.
    LinkOnly,
}

// ---------------------------------------------------------------------------
// A filed story, and what has been filed
// ---------------------------------------------------------------------------
//
// The shapes, here rather than in `story`, because `state::Inner` holds the cache
// and `state` may not import the module that fills it. `StoryRef`'s guard travels
// with it: the fields stay private and [`StoryRef::new`] stays the only way in, so
// what that constructor refuses is still unconstructible from outside this file.

/// A story that exists in the tracker.
///
/// Both halves come from the tool response and neither is ever constructed by
/// `format!`: the org slug in the URL belongs to your tracker workspace and the daemon has no
/// business knowing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct StoryRef {
    /// Short form, `sc-12345`. What the report shows.
    ///
    /// Private, with [`StoryRef::new`] the only way in, because the pair is agent
    /// text that ends up as a link in a public comment: a value that has not been
    /// through [`StoryRef::consistent`] must not be constructible outside this
    /// module — `model` now, with `story` one of its callers. Serde is the
    /// exception it cannot police — a `stories.json` written before the id was
    /// checked deserializes straight past the constructor, which is why
    /// [`Cache::get`] re-checks on the way out.
    id: String,
    /// The clickable one, `https://app.shortcut.com/<org>/story/12345`.
    url: String,
}

impl StoryRef {
    /// A story reference, or `None` when the id and the URL do not hang together.
    ///
    /// The one constructor, so [`StoryRef::link`] cannot be handed a pair nobody
    /// checked.
    pub fn new(id: &str, url: &str, host: &str) -> Option<Self> {
        let s = StoryRef {
            id: id.trim().to_string(),
            url: url.trim().to_string(),
        };
        s.consistent(host).then_some(s)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// What `{story}` becomes in the posted reply.
    ///
    /// A markdown link rather than either half alone: the skill's rule is "never
    /// a bare number, always the full URL" because a colleague has to be able to
    /// click it, and a naked URL mid-sentence reads badly in prose. The
    /// substitution is deterministic given `(id, url)`, so `already_replied`'s
    /// exact match still recognises a reply it posted before.
    pub fn link(&self) -> String {
        format!("[{}]({})", self.id, self.url)
    }

    /// Does the URL actually point at this id, **on the tracker's own host**?
    ///
    /// The agent hands back both, and an id it invented for a story it never
    /// created would put a permanent public link to *somebody else's* story into
    /// a reply. The id's number appearing in the URL is what ties the two together.
    ///
    /// Matched as a whole path segment rather than as a substring, because
    /// Shortcut hands out URLs both bare and with a title slug on the end, and a
    /// slug can carry digits of its own.
    ///
    /// **The host and the scheme are checked too, and that is not paranoia.** This
    /// used to accept any URL carrying the number as a segment, so
    /// `http://attacker.example/12345` passed — and both halves of the pair come
    /// out of agent output whose *input* is third-party review comments, with the
    /// result posted publicly as a link somebody is meant to click. So: `https`
    /// only, an exact host match (which also rules out `app.shortcut.com.evil.com`
    /// and a userinfo prefix, since the authority is compared whole), and the
    /// number as a path segment.
    fn consistent(&self, host: &str) -> bool {
        // The id is agent text too, and it lands inside markdown link syntax.
        // "Ends in digits" was the whole check, so `x](https://evil.example)
        // [sc-12345` with a legitimate URL passed and rendered as a clickable link
        // to an arbitrary host — the hole the URL check closes, through the other
        // field. So the id has to be a tracker prefix and a number, nothing else.
        let Some(number) = well_formed_id(&self.id) else {
            return false;
        };
        // Scheme, then authority, then path — no URL crate, because the shapes
        // being refused are exactly the ones a hand-rolled split gets right when
        // it compares the whole authority rather than searching inside it.
        let Some(rest) = self.url.strip_prefix("https://") else {
            return false;
        };
        let Some((authority, path)) = rest.split_once('/') else {
            return false;
        };
        // Hosts are case-insensitive; everything else here is not.
        if !authority.eq_ignore_ascii_case(host) {
            return false;
        }
        // The query and fragment are not path, and a number in either proves
        // nothing about which story this is.
        let path = path
            .split_once(['?', '#'])
            .map_or(path, |(before, _)| before);
        /* A segment equal to the number, **or to the whole id**.

        Which of the two a tracker uses is not a detail: Shortcut's URLs carry
        the bare number (`/story/12345` for `sc-12345`), while Linear's and
        Jira's carry the whole key (`/issue/ENG-123`, `/browse/ABC-123`). The
        digits-only match this replaced would have refused every story either of
        those files — and refused it as "the agent reported an id and URL that
        disagree", which reads like the agent's fault rather than a rule that
        only ever fitted one tracker.
        Still two exact comparisons against one path segment, so the decoy the
        test names (a slug with digits of its own) is refused exactly as before.
        Case-insensitive for the id because a key is conventionally uppercase and
        an agent writing prose around it may not be; a number has no case. */
        path.split('/')
            .any(|seg| seg == number || seg.eq_ignore_ascii_case(&self.id))
    }
}

/// The number in a story id of the shape `sc-12345`: one to eight letters, an
/// optional dash, one to twelve digits. Anything else — a bracket, a newline, a
/// second id, an unbounded string — is `None`, and the caller refuses it.
fn well_formed_id(id: &str) -> Option<&str> {
    let letters = id.len()
        - id.trim_start_matches(|c: char| c.is_ascii_alphabetic())
            .len();
    if !(1..=8).contains(&letters) {
        return None;
    }
    let rest = id[letters..].strip_prefix('-').unwrap_or(&id[letters..]);
    let digits = (1..=12).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_digit());
    digits.then_some(rest)
}

/// Stories filed per PR, keyed by the thread they answer.
///
/// Nested rather than keyed on a tuple because JSON object keys are strings, and
/// a `(u64, String)` key would have to be encoded and parsed back — a format to
/// get wrong for no gain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cache {
    #[serde(default)]
    pub by_pr: HashMap<u64, HashMap<String, StoryRef>>,
}

impl Cache {
    /// A stored story, if it is still one this daemon would be willing to write.
    ///
    /// The file is durable and the check on the id is newer than some of what is in
    /// it: an entry written when "ends in digits" was the whole test could carry
    /// `x](https://evil.example) [sc-12345`, which [`StoryRef::link`] renders as a
    /// clickable link to somebody else's host in a comment on a colleague's review.
    /// Validating only what the agent reports leaves that entry served from disk
    /// for as long as the file lives, so the way out re-checks too.
    ///
    /// The id is checked unconditionally because it is the half that escapes the
    /// markdown; the URL's host only when the caller knows one. A cache hit
    /// deliberately works with no tracker configured, and there is then nothing to
    /// compare a host against.
    pub fn get(&self, pr: u64, thread_id: &str, host: Option<&str>) -> Option<&StoryRef> {
        let hit = self.by_pr.get(&pr)?.get(thread_id)?;
        let ok = match host {
            Some(h) => hit.consistent(h),
            None => well_formed_id(&hit.id).is_some(),
        };
        if !ok {
            tracing::warn!(
                pr,
                thread_id,
                "a cached story does not hold together; refusing to link it"
            );
            return None;
        }
        Some(hit)
    }

    pub fn put(&mut self, pr: u64, thread_id: &str, story: StoryRef) {
        self.by_pr
            .entry(pr)
            .or_default()
            .insert(thread_id.to_string(), story);
    }

    /// Never pruned by PR. A merged PR's stories still matter to a late retry,
    /// and the whole file is a handful of ids.
    pub fn len(&self) -> usize {
        self.by_pr.values().map(HashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// A PR the daemon is fixing
// ---------------------------------------------------------------------------
//
// The record, here rather than in `fix_pr`, because `state::Inner` holds the store
// and `state` may not import the module that fills it. `fix_pr` keeps the guards,
// the verdict and the run.

/// Per-PR automation state (§8).
///
/// Retry lives in the prompt, not here: `fix-pr` amends and rebases, so the head
/// SHA changes on every internal attempt and SHA-based provenance is impossible
/// *and* unnecessary. The daemon's job is only to avoid starting a second run
/// and to be honest about a run that gave up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub enum PrAutomation {
    Running {
        session: SessionId,
        #[cfg_attr(
            any(test, feature = "test-util"),
            ts(type = "{ secs_since_epoch: number, nanos_since_epoch: number }")
        )]
        started: SystemTime,
    },
    /// The run stopped without turning the PR green. It wants you.
    Exhausted {
        /// The head this exhaustion is measured against, once anything knows it.
        ///
        /// `None` means "not established yet", and the next poll adopts whatever
        /// it finds. Two things wrote a wrong answer here before, and both
        /// cleared the record on the very next poll — erasing the "gave up, wants
        /// you" signal the run had just set. `settle` copied the head from the
        /// *previous* poll, which the run's own force-push had already moved past;
        /// and a crashed run was demoted with `""`, which can never equal a real
        /// sha. Neither could be told apart from you moving the branch.
        #[serde(default)]
        at_head: Option<String>,
        #[cfg_attr(
            any(test, feature = "test-util"),
            ts(type = "{ secs_since_epoch: number, nanos_since_epoch: number }")
        )]
        at: SystemTime,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutomationStore {
    #[serde(default)]
    pub by_pr: HashMap<u64, PrAutomation>,
}

impl AutomationStore {
    pub fn get(&self, pr: u64) -> Option<&PrAutomation> {
        self.by_pr.get(&pr)
    }

    /// Exhaustion clears when the head moves while no run is alive: nothing of
    /// the daemon's was running and the branch changed, therefore you did it.
    /// No commit markers, no timestamps, no provenance (§8).
    ///
    /// A record with no head yet **adopts** this one rather than clearing. That is
    /// what makes the rule mean what it says: the daemon cannot know the head a
    /// run left — the run's last act is a force-push, after the poll that could
    /// have seen it — so the first poll afterwards establishes the baseline and
    /// only a move *after* that is yours. The cost is one poll interval of grace:
    /// a push of yours landing in that window is absorbed into the baseline
    /// instead of clearing the record. Worth it, since the bug it replaces
    /// cleared the record every single time.
    ///
    /// Returns whether anything changed, so the caller can carry the write.
    pub fn reconcile_head(&mut self, pr: u64, head: Option<&str>) -> bool {
        let Some(head) = head else { return false };
        // Decided before acting, because clearing takes the map mutably while the
        // record it is deciding about is still borrowed.
        let moved = match self.by_pr.get(&pr) {
            Some(PrAutomation::Exhausted {
                at_head: Some(h), ..
            }) => h != head,
            Some(PrAutomation::Exhausted { at_head: None, .. }) => false,
            _ => return false,
        };
        if moved {
            self.by_pr.remove(&pr);
        } else if let Some(PrAutomation::Exhausted { at_head, .. }) = self.by_pr.get_mut(&pr) {
            if at_head.is_some() {
                return false;
            }
            *at_head = Some(head.to_string());
        }
        true
    }
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

    /// A second notice about one idle turn must not restart the wait clock.
    ///
    /// **That clock is the metric.** The rail sorts on how long a session has been
    /// waiting and the waitbar counts it, and Claude Code sends more than one
    /// notification for a single stop — a permission prompt, then the stop. Restamp
    /// it and the row you have been ignoring longest moves to the bottom. The rule
    /// was written twice in `hooks.rs`, in two shapes, and tested in neither.
    #[test]
    fn a_second_notice_does_not_restart_the_wait_clock() {
        let mut s = Session::new(
            Uuid::new_v4(),
            "wt".into(),
            std::path::PathBuf::from("/tmp"),
            None,
        );
        s.set_state(State::Working);
        s.wait_for(TurnReason::AskedAQuestion);
        let State::YourTurn { since: first, .. } = s.state else {
            panic!("the first notice did not start a wait: {:?}", s.state);
        };
        let began = s.state_since;

        s.wait_for(TurnReason::TurnComplete);
        let State::YourTurn { since, reason } = s.state else {
            panic!("the second notice ended the wait: {:?}", s.state);
        };
        assert_eq!(since, first, "the clock restarted");
        assert_eq!(s.state_since, began, "so did the one the rail sorts on");
        assert_eq!(
            reason,
            TurnReason::AskedAQuestion,
            "the first notice is what the session is waiting for"
        );

        /* And a turn that really starts again waits afresh: the rule is about a
        second notice, not about ever leaving the state.

        **Asserted on the reason, not on the clock.** The obvious assertion is that
        the new `since` differs from the old one, and it fails on macOS: two
        `SystemTime::now()` calls this close together return the *same* value there,
        where a Linux clock ticks between them. The test went out green and CI's
        macos-14 runner caught it, which is what that runner is for. The reason
        changing is the same fact and depends on no clock — a skipped `wait_for`
        leaves the old reason standing, as the assertions above require. */
        s.set_state(State::Working);
        s.wait_for(TurnReason::TurnComplete);
        let State::YourTurn { reason, .. } = s.state else {
            panic!("a finished turn did not wait: {:?}", s.state);
        };
        assert_eq!(
            reason,
            TurnReason::TurnComplete,
            "a new turn's notice was skipped as though the old wait were still on"
        );
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

    // -----------------------------------------------------------------------
    // A filed story, and what has been filed
    // -----------------------------------------------------------------------

    fn story() -> StoryRef {
        StoryRef {
            id: "sc-12345".into(),
            url: "https://app.shortcut.com/acme/story/12345".into(),
        }
    }

    #[test]
    fn the_substitution_is_clickable_and_short() {
        assert_eq!(
            story().link(),
            "[sc-12345](https://app.shortcut.com/acme/story/12345)"
        );
    }

    /// A tracker's host, as `config::Tracker::host` carries it.
    const HOST: &str = "app.shortcut.com";

    #[test]
    fn an_id_that_does_not_match_its_url_is_refused() {
        // The agent hands back both. If they disagree, one of them is invented,
        // and posting the link would point a colleague at someone else's story.
        assert!(story().consistent(HOST));

        let mut swapped = story();
        swapped.url = "https://app.shortcut.com/acme/story/99999".into();
        assert!(!swapped.consistent(HOST));

        // Shortcut hands out both forms; a title slug on the end is still the
        // same story.
        let mut slugged = story();
        slugged.url = "https://app.shortcut.com/acme/story/12345/document-the-schedules".into();
        assert!(slugged.consistent(HOST));

        // ...and a slug carrying digits of its own must not stand in for the id.
        let mut decoy = story();
        decoy.id = "sc-777".into();
        decoy.url = "https://app.shortcut.com/acme/story/12345/fix-777-errors".into();
        assert!(
            !decoy.consistent(HOST),
            "matched a slug instead of the id segment"
        );

        let mut empty = story();
        empty.id = "sc-".into();
        assert!(!empty.consistent(HOST));
    }

    /// The other two trackers anyone is likely to point this at, whose URLs carry
    /// the whole key rather than the bare number.
    ///
    /// Looked up rather than guessed: Linear is `linear.app/<workspace>/issue/ENG-123/<slug>`
    /// and Jira is `<site>.atlassian.net/browse/ABC-123`. Both parse as an id here
    /// already — `well_formed_id` takes one to eight letters and a number — so the
    /// only thing that refused them was the path match.
    #[test]
    fn a_key_in_the_path_agrees_with_its_id_too() {
        let linear = StoryRef::new(
            "ENG-123",
            "https://linear.app/acme/issue/ENG-123/stop-the-flaky-poller",
            "linear.app",
        );
        assert!(linear.is_some(), "a Linear issue URL was refused");

        let jira = StoryRef::new(
            "ABC-123",
            "https://acme.atlassian.net/browse/ABC-123",
            "acme.atlassian.net",
        );
        assert!(jira.is_some(), "a Jira browse URL was refused");

        // The pair still has to agree: a different key in the path is a different
        // issue, whichever tracker it is.
        assert!(
            StoryRef::new(
                "ENG-123",
                "https://linear.app/acme/issue/ENG-999/other",
                "linear.app",
            )
            .is_none(),
            "a mismatched key passed"
        );
        // And the host rule is untouched by any of it.
        assert!(
            StoryRef::new(
                "ENG-123",
                "https://evil.example/acme/issue/ENG-123",
                "linear.app",
            )
            .is_none(),
            "a foreign host passed"
        );
    }

    /// **The URL is agent output, and its input is third-party review text.** The
    /// pair ends up as a permanent public link in a reply, so a number appearing
    /// somewhere in the string was never enough: every URL below carries the right
    /// story number and every one of them must still be refused.
    /// The other field. Every id below carries the right number *and* a legitimate
    /// URL, and every one must still be refused: the id is rendered verbatim into
    /// `[id](url)`, so a bracket in it closes the link text and opens another.
    #[test]
    fn an_id_that_is_not_a_prefix_and_a_number_is_refused() {
        let with = |id: &str| {
            let mut s = story();
            s.id = id.into();
            s.consistent(HOST)
        };
        assert!(with("sc-12345"), "the ordinary shape");
        assert!(with("SC12345"), "no dash is fine");
        assert!(
            !with("x](https://evil.example) [sc-12345"),
            "a link injected through the id"
        );
        assert!(!with("sc-12345\nsee also"), "a newline");
        assert!(!with("sc-12345 sc-12345"), "two ids");
        assert!(!with("12345"), "no prefix");
        assert!(!with("sc-"), "no number");
        assert!(
            !with("storyprefix-12345"),
            "a prefix too long to be a tracker's"
        );
        assert!(
            !with("sc-1234567890123"),
            "a number too long to be a story's"
        );
        assert!(!with(""), "empty");
    }

    #[test]
    fn a_url_off_the_trackers_host_is_refused() {
        let with = |url: &str| {
            let mut s = story();
            s.url = url.into();
            s.consistent(HOST)
        };

        assert!(
            !with("http://attacker.example/12345"),
            "another host entirely"
        );
        assert!(
            !with("https://attacker.example/story/12345"),
            "https, still not ours"
        );
        // The shapes a substring check on the host would have let through.
        assert!(
            !with("https://app.shortcut.com.evil.example/story/12345"),
            "suffixed host"
        );
        assert!(
            !with("https://evil.example/app.shortcut.com/story/12345"),
            "host in the path"
        );
        assert!(
            !with("https://app.shortcut.com@evil.example/story/12345"),
            "userinfo pointing elsewhere"
        );
        // Scheme matters: a link somebody clicks should not be downgradeable.
        assert!(
            !with("http://app.shortcut.com/acme/story/12345"),
            "plain http"
        );
        assert!(!with("//app.shortcut.com/acme/story/12345"), "no scheme");
        // A number in the query or the fragment is not a path segment.
        assert!(
            !with("https://app.shortcut.com/acme/story/999?id=12345"),
            "query"
        );
        assert!(
            !with("https://app.shortcut.com/acme/story/999#12345"),
            "fragment"
        );
        // And the host on its own, with no path, names no story.
        assert!(!with("https://app.shortcut.com"), "no path at all");

        // The real thing still passes, including a differently-cased host.
        assert!(with("https://app.shortcut.com/acme/story/12345"));
        assert!(
            with("https://APP.Shortcut.COM/acme/story/12345"),
            "hosts are case-insensitive"
        );
    }

    #[test]
    fn the_cache_is_keyed_by_pr_and_thread() {
        let mut c = Cache::default();
        assert!(c.is_empty());
        c.put(10001, "PRRT_1", story());
        assert_eq!(c.get(10001, "PRRT_1", Some(HOST)), Some(&story()));
        // Same thread id under a different PR is a different story.
        assert_eq!(c.get(10004, "PRRT_1", Some(HOST)), None);
        assert_eq!(c.get(10001, "PRRT_2", Some(HOST)), None);
        assert_eq!(c.len(), 1);
    }

    /// The file outlives the rule. An entry written when "ends in digits" was the
    /// whole check carries an id that breaks out of `[id](url)` markdown, and
    /// validating only what the agent reports left it served from disk for good.
    #[test]
    fn a_poisoned_cache_entry_is_refused_on_the_way_out() {
        let mut c = Cache::default();
        // Straight into the map, the way a `stories.json` from an older build
        // deserializes: past the constructor, which is the point.
        let poisoned = StoryRef {
            id: "x](https://evil.example) [sc-12345".into(),
            url: "https://app.shortcut.com/acme/story/12345".into(),
        };
        c.put(10001, "PRRT_1", poisoned);
        assert_eq!(
            c.get(10001, "PRRT_1", Some(HOST)),
            None,
            "the host is known"
        );
        assert_eq!(
            c.get(10001, "PRRT_1", None),
            None,
            "and the id alone is enough to refuse it, with no tracker configured"
        );

        // A sound entry still comes back either way.
        c.put(10002, "PRRT_1", story());
        assert_eq!(c.get(10002, "PRRT_1", Some(HOST)), Some(&story()));
        assert_eq!(c.get(10002, "PRRT_1", None), Some(&story()));
    }

    #[test]
    fn the_cache_survives_a_round_trip() {
        let mut c = Cache::default();
        c.put(10001, "PRRT_1", story());
        let back: Cache = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.get(10001, "PRRT_1", Some(HOST)), Some(&story()));
    }

    // -----------------------------------------------------------------------
    // A PR the daemon is fixing
    // -----------------------------------------------------------------------

    #[test]
    fn exhaustion_clears_only_when_the_head_moves() {
        let mut store = AutomationStore::default();
        store.by_pr.insert(
            7,
            PrAutomation::Exhausted {
                at_head: Some("abc".into()),
                at: SystemTime::now(),
            },
        );
        store.reconcile_head(7, Some("abc"));
        assert!(store.get(7).is_some(), "same head must stay exhausted");
        store.reconcile_head(7, Some("def"));
        assert!(store.get(7).is_none(), "a moved head means you touched it");
    }

    #[test]
    fn an_unknown_head_does_not_clear_exhaustion() {
        let mut store = AutomationStore::default();
        store.by_pr.insert(
            7,
            PrAutomation::Exhausted {
                at_head: Some("abc".into()),
                at: SystemTime::now(),
            },
        );
        store.reconcile_head(7, None);
        assert!(store.get(7).is_some());
    }

    /// A record with no head yet is the *normal* end of a run, and the next poll
    /// gives it one instead of throwing it away.
    ///
    /// This is the whole of the bug: the run's last act is a force-push, so the
    /// head every already-taken poll knows is one the branch has left. Recording
    /// that stale sha — or the `""` a crashed run used to be demoted with — made
    /// the next poll read "the head moved, therefore you moved it" and erase the
    /// only signal saying the run gave up.
    #[test]
    fn a_run_that_left_an_unknown_head_adopts_the_next_poll_rather_than_clearing() {
        let mut store = AutomationStore::default();
        store.by_pr.insert(
            7,
            PrAutomation::Exhausted {
                at_head: None,
                at: SystemTime::now(),
            },
        );
        assert!(
            store.reconcile_head(7, Some("post-push")),
            "the baseline is news"
        );
        assert!(store.get(7).is_some(), "adopting must not clear the record");
        // Adopted, so it is now the thing a later move is judged against — and a
        // second poll at the same head changes nothing and writes nothing.
        assert!(!store.reconcile_head(7, Some("post-push")));
        assert!(store.get(7).is_some());
        assert!(
            store.reconcile_head(7, Some("yours")),
            "now it really moved"
        );
        assert!(store.get(7).is_none());
    }

    // -----------------------------------------------------------------------
    // What a run carries
    // -----------------------------------------------------------------------

    /// Which runs the resume path has to re-credential.
    ///
    /// `spawn::spawn_session` rebuilds a resumed session's environment and asks
    /// this. It answered wrong by not existing: a resumed review run kept its
    /// ask channel and lost its post token, so it reported the variable missing
    /// and then asked the human a question the overlay had no card for. The
    /// spelling is recorded by `triage::spawn_review`, so a rename turns the bug
    /// straight back on.
    #[test]
    fn the_posting_run_is_recognised_and_no_others() {
        assert!(Pass::posts_proposals(Pass::REVIEW));
        // A fix run posts nothing itself, and handing it the credential would
        // widen what a run reading third-party comments can reach.
        assert!(!Pass::posts_proposals(Pass::FIX_PR));
        assert!(!Pass::posts_proposals("resolve"));
    }
}
