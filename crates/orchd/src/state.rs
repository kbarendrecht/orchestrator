use anyhow::{bail, Context as _, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{broadcast, Notify, RwLock};
use uuid::Uuid;

use crate::config::Config;
use crate::git;
use crate::model::*;
use crate::pty::pid_alive;

/// The one refusal for a session id nothing knows.
///
/// **A function because it was eighteen copies of one sentence** — seventeen in
/// `api.rs` and one in `relocate.rs` — and the `orch` CLI and the SPA both match on
/// what it says. Not an axum extractor, which is the shape this looks like it wants
/// and is not: the handlers differ in whether they take the read lock or the write
/// one, and nearly all of them hold the guard across the work that follows, so an
/// extractor could only prove the id exists and leave every caller to look it up
/// again under its own lock.
pub fn no_such_session(id: SessionId) -> anyhow::Error {
    crate::model::Refusal::Missing(format!("no such session {id}")).into()
}

#[derive(Debug, Clone, Serialize, Default)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct Repos {
    /// Where PRs are opened, e.g. `acme/monorepo`.
    pub upstream: Option<String>,
    /// Where branches are pushed, e.g. `you/monorepo` (§6's fork flow).
    pub fork: Option<String>,
}

/// What [`AppState::pr_head`] found about a PR's head.
#[derive(Debug, Default, Clone)]
pub struct PrHead {
    /// Why the local branch named like the head is not this PR's, if it is not.
    pub refusal: Option<String>,
    /// The remote the head repository is, to fetch a missing branch from.
    pub from: Option<String>,
}

/// A run's session has ended.
///
/// Published by [`crate::spawn::watch_session_exit`], which owns the only
/// `pty.wait()` and is therefore the one place a run's end is learned.
#[derive(Debug, Clone)]
pub struct RunExit {
    pub session: SessionId,
    /// What the session was. The command decides who is owed the news.
    pub pass: Pass,
}

/* **Who settles a finished run, told rather than known.**

`spawn` owns the pty, so it is where an exit is learned — and it used to settle
the run itself, by name: `fix_pr::settle`, `fix_pr::start`, and `triage`'s
predicates. That is why `spawn` imported the two modules that call `spawn_run` to
start a run in the first place, and it is the last mutual pair the module ratchet
held.

Inverted here rather than hooked onto `RunSpec`, which is the obvious shape and is
wrong: `RunSpec` is neither persisted nor in `spawn::Carried`, and `auto_resume`
rebuilds a run's session from its `Pass` alone — so a fix run resumed after a
restart would have no hook left and would never settle. This is installed once per
process by the composition root (`orchd_serve::start`), which already wires the
pollers, so a resume finds it exactly as the first spawn did.

A plain `fn` returning a boxed future rather than a channel, so the call keeps its
place in the exit sequence: `settle` used to run before `release_main` and the
hand-off after, and a queue would have made both "some time later". */
pub type RunObserver =
    fn(Arc<AppState>, RunExit) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// How long `notify` waits before the records reach disk.
///
/// A second: long enough that a burst of hooks is one write, short enough that a
/// crash costs a second of bookkeeping rather than a conversation. Nothing waits
/// on it — the snapshot goes out immediately either way.
const PERSIST_COALESCE: std::time::Duration = std::time::Duration::from_secs(1);

pub struct AppState {
    /// The config as it was read at start. The fields [`Settings::needs_restart`]
    /// names are read from here, because the running daemon is built on them.
    ///
    /// [`Settings::needs_restart`]: crate::config::Settings::needs_restart
    pub cfg: Config,
    /// The settings as last saved, which the rest are read from through
    /// [`AppState::settings`] so a save applies them without a restart.
    ///
    /// A `std` lock, because every reader clones and lets go: nothing holds it
    /// across an await.
    settings: std::sync::RwLock<crate::config::Settings>,
    pub repos: Repos,
    /// The story link the tracker implies, set once at boot when it can be
    /// found ([`crate::story`]). [`AppState::story_link`] reads it after Settings.
    pub story_default: std::sync::OnceLock<crate::config::StoryLink>,
    /// Random per-start token embedded in the served SPA and required on the
    /// WebSocket and every mutating endpoint (§12).
    pub token: String,
    /// Named resources currently held by a run (§7 rule 2), taken through
    /// [`AppState::try_claim`] and given back by the [`Claim`]'s `Drop`.
    ///
    /// A `std` mutex beside `inner` rather than a field inside it, because a `Drop`
    /// cannot await. When this lived in `inner` the release was a spawned task, and
    /// a claim taken straight after a `?` returned was refused as still held: the
    /// lock lagged its own guard by one scheduler hop. Never held across an await,
    /// so a blocking lock is the right one.
    pub locks_held: std::sync::Mutex<HashSet<String>>,
    /// A record write is already scheduled; see [`Self::persist_soon`].
    persist_pending: std::sync::atomic::AtomicBool,
    /// One writer at a time. `persist_soon` clears `persist_pending` *before* the
    /// write (so a change arriving during it schedules another flush rather than
    /// being lost), which means two flushes can overlap — and two writers of the
    /// same file are two chances to leave the older snapshot on disk. Serialised
    /// here rather than by trying to make the flag do both jobs.
    persist_writing: tokio::sync::Mutex<()>,
    /// Which sessions the last written `sessions.json` knows about; see
    /// [`Self::persist_when_due`].
    persisted_ids: std::sync::atomic::AtomicU64,
    /// Who to tell when a run's session ends. See [`RunObserver`].
    ///
    /// Empty in a unit test, and that is the degradation on purpose: a test that
    /// spawns a session is not testing what a fix run's exit means.
    run_observer: std::sync::OnceLock<RunObserver>,
    pub inner: RwLock<Inner>,
    /// Held for the length of a swap, so two of them cannot interleave.
    ///
    /// Not a nicety. A swap chooses *who travels* from a snapshot of session state
    /// — by branch, before anything moves — and a relocation already in flight is
    /// mutating exactly that: the conversation is killed, its record is rebuilt
    /// under the same id, and for the moment in between it matches neither tree.
    /// A second swap taken in that window finds nobody to carry and moves a branch
    /// on its own, and the conversation lands in a checkout its branch has already
    /// left. Reproduced by pressing the button twice: two swaps eleven seconds
    /// apart cancelled each other's branches out and left the session in main with
    /// its branch back in the worktree.
    ///
    /// One flag rather than one per workspace, because every swap involves main:
    /// two of them are never independent.
    ///
    /// **Private, and [`AppState::move_main`] is the only way to it.** Six paths
    /// move main's branch, each took this by hand, and one did not: a swap and an
    /// `open PR` could each move main at once. Every function that moves the
    /// branch now asks for the [`MainMove`] this hands out, so forgetting the lock
    /// is a compile error rather than a race found later.
    swapping: tokio::sync::Mutex<()>,
    /// Main is going back to its base, and nobody may claim it until it is there.
    ///
    /// **The move lock alone could not say so.** `park_main` decides main is
    /// empty, takes [`AppState::move_main`] and then runs git for a while; a
    /// session pressed into main in that window claims it through `claim_main`,
    /// which never took the move lock — so the park switched main under an agent
    /// that had just started. Set by [`AppState::reserve_main_for_park`] in the
    /// same critical section as the check, read by `claim_main` under that lock.
    parking_main: std::sync::atomic::AtomicBool,
    /// Fired when a park lets go of main, so a claim that arrived during it can go
    /// on rather than be refused. See `claim_main`.
    park_done: tokio::sync::Notify,
    /// Sessions that have claimed main and are still starting: between the claim
    /// and the record their spawn inserts, which is the time `mise env` takes.
    /// Held by [`StartingInMain`], which `claim_main` hands back.
    starting_in_main: std::sync::Mutex<HashSet<SessionId>>,
    /// Held for the length of a reconcile sweep, so two of them cannot overlap.
    ///
    /// One sweep is seven git runs per workspace and there are as many workspaces
    /// as you keep worktrees — 447 child processes over 64 of them on the machine
    /// this was measured on. Two sweeps at once is that twice, for an answer that
    /// is the same both times, and the second one is pure contention with the
    /// first. It became possible the moment the boot sweep stopped blocking
    /// `start`: the PR poller reconciles on its own first tick, which is
    /// immediately, so boot and poll now race by construction.
    ///
    /// `try_lock`, never `lock`: a sweep that arrives while one is running has
    /// nothing to add, so it is dropped rather than queued behind a job whose
    /// result it would only overwrite with the same numbers.
    pub sweeping: tokio::sync::Mutex<()>,
    /// Held across a `git worktree add`, so two of the daemon's own never overlap.
    ///
    /// **`git worktree add -b <branch> <remote-ref>` writes `.git/config`** — the
    /// upstream tracking for the new branch — and git takes `.git/config.lock`
    /// to do it and **does not retry**. A second add landing inside that window
    /// fails outright with `could not lock config file .git/config: File exists`,
    /// and the create fails with it.
    ///
    /// Nothing made two of them overlap until the spare pool did. It cuts a tree
    /// in the background at boot, so a create arriving in the first seconds of a
    /// daemon's life now races it by construction — which is exactly what
    /// `mise run app-check` does, and it took `bundle` red on one run of a commit
    /// while passing on another run of the same one.
    ///
    /// `lock`, not `try_lock`, unlike [`Self::sweeping`]: a create that arrives
    /// while another is running still has to happen, so it queues. The wait it can
    /// inherit is a whole cut — about 4.7 seconds on the monorepo — and that is
    /// the price of the pool being allowed to work in the background at all.
    pub cutting: tokio::sync::Mutex<()>,
    /// Set the moment shutdown begins.
    ///
    /// A session's exit watcher cannot otherwise tell "you closed this pane" from
    /// "the app is going down", and the two want opposite things: closing the last
    /// session in main parks the checkout back on its base branch, while a restart
    /// must leave it exactly where auto-resume will expect to find it.
    pub shutting_down: std::sync::atomic::AtomicBool,
    /// Set once [`Self::restore_sessions`] has the previous run's records in
    /// memory. Until then the session map is empty because nothing was read yet,
    /// not because there are no sessions, and a `persist` writes that `[]` over the
    /// file boot is about to load. Any `notify` before the load does that, and the
    /// agent probe did, on every start where it beat the upstream fetch.
    pub restored: std::sync::atomic::AtomicBool,
    /// Sessions auto-resume is going to bring back and has not reached yet (#33).
    ///
    /// **A session waiting its turn is not live, and `was_live` is read off live
    /// state.** Auto-resume spawns one at a time with a stagger, so for the first
    /// ten seconds of a start most of the set has no process — and a shutdown in
    /// that window wrote `was_live: false` for every one of them, which is the flag
    /// the *next* start filters on. One quit during the window, and the rail opened
    /// with fewer sessions; three restarts in thirty seconds, which is what an
    /// in-app update does, and it opened with none. The records and the transcripts
    /// were never touched — only the one bit that says "bring this back".
    ///
    /// So the intent outlives the state: an id is in here from the moment
    /// auto-resume decides to resume it until its spawn has answered, and
    /// [`AppState::session_records`] writes `was_live: true` for anything still in
    /// it. A failed spawn stays in — the next start should try again rather than
    /// forget the session, and the pre-filters have already dropped the ones no
    /// start can fix (a `cwd` that is gone, a conversation with no turn).
    ///
    /// **Any spawn or exit of the session ends the intent**, not only auto-resume's
    /// own success — see [`AppState::resume_settled`]. Once something else has
    /// started or stopped it, the session's own state is the truth, and an id left
    /// in here forced `true` over it for the life of the process.
    pub pending_resume: std::sync::Mutex<std::collections::HashSet<SessionId>>,
    /// True while auto-resume is still working through `pending_resume`.
    ///
    /// Which session holds main is only in memory, so until auto-resume reaches the
    /// session that was in main, main reads as empty — and "open in main", a fix-pr
    /// worktree or a new session in main could take it, moving the branch that
    /// session is about to come back to. So a pending session in main holds main
    /// while this is set. Cleared when auto-resume is done, so a resume that failed
    /// does not hold main for the life of the process.
    pub auto_resuming: std::sync::atomic::AtomicBool,
    /// The file pane's preview tokens, keyed by token. For the life of the process:
    /// one per page, so the map grows by the pages somebody previewed and no more.
    pub previews: std::sync::Mutex<HashMap<String, PreviewGrant>>,
    /// Fan-out of state snapshots to connected SPAs.
    pub events: broadcast::Sender<String>,
    /// How the SPA should draw its top bar.
    ///
    /// Fixed at startup rather than set when the window attaches: the webview
    /// begins loading the instant it is built, so a chrome decided afterwards
    /// would race the very first paint and lose, intermittently.
    pub chrome: crate::window::Chrome,
    /// Pulsed whenever an interaction is answered. Every waiting long poll wakes
    /// and re-checks its own question; there are only ever a handful of waiters,
    /// and one notify beats a channel per question.
    pub answered: Arc<Notify>,
    /// Pulsed by the refresh button to make the review poller fetch now rather
    /// than wait out the rest of its period.
    pub review_refresh: Arc<Notify>,
    /// The same, for the PR poller.
    pub pr_refresh: Arc<Notify>,
}

/// How many changed files a workspace reports before the pane starts counting
/// instead of listing.
///
/// Not a display preference — a bound on the snapshot. `changed` is cloned per
/// workspace into every snapshot and `notify` builds one per tool call (
/// `post_tool_use` calls it once per call), so the list's length is multiplied by
/// the busiest loop in the daemon. Measured at ~155-200 bytes a file: 500 costs
/// about 80 KB, 5,000 costs 0.8 MB, and the second number is what an agent that
/// wiped a repository actually produced — several times a second, which took the
/// whole app down.
///
/// Well past what anyone reads down. The number exists so the *pane* stays useful
/// on a big branch, not so the list stays complete.
pub const CHANGED_CAP: usize = 500;

/// What one pass of git said about a tree, carried off the blocking thread.
///
/// A struct rather than a seven-wide tuple: every field here is `Option`, several
/// are numbers, and `(_, _, _, Some(x), _, _)` at the far end is how the wrong
/// two get swapped without anything failing to compile.
struct Measured {
    divergence: Option<(u32, u32)>,
    rebasing: bool,
    branch: Option<String>,
    unpushed: Option<u32>,
    base: Option<String>,
    changed: Option<(Vec<crate::model::DiffFile>, u32)>,
}

#[derive(Default)]
pub struct Inner {
    pub workspaces: HashMap<WorkspaceId, Workspace>,
    pub sessions: HashMap<SessionId, Session>,
    pub prs: Vec<crate::forge::Pr>,
    /// PRs the poll used to report and no longer does, with what became of each.
    ///
    /// **Memory only, and deliberately.** It is a note about the session you are
    /// looking at right now — "that merged, the worktree can go" — and after a
    /// restart the PR is simply gone, which is what every build before this did.
    /// Persisting it would mean deciding when a merged PR stops being news, and
    /// nothing in the product can answer that.
    ///
    /// Newest first, capped: see `remember_ended`.
    pub prs_ended: Vec<crate::forge::EndedPr>,
    /// PRs that have left the open poll and have not said what became of them.
    ///
    /// `(number, head_ref)`. **Asked again on every poll while the branch is still
    /// a workspace's**, which is what bounds it: the question only matters for a
    /// PR that still has a row, and the entry goes when the worktree does. Asking
    /// exactly once instead was the first shape and it loses the answer — a search
    /// that lags by one tick, or any poll between the disappearance and the merge,
    /// spends the only attempt.
    pub pending_outcome: Vec<(u64, String)>,
    /// Files two workspaces both changed, one entry per pair and file
    /// ([`crate::overlap`]). Rebuilt for a workspace each time it reconciles, so a
    /// pair is fresh whenever either side moved. Memory only: every reconcile
    /// after a restart measures it again.
    pub overlaps: Vec<crate::overlap::Pair>,
    /// What the user confirmed for an overlapping pair of workspaces, keyed by the
    /// two ids in order: the label of the option they picked. Memory only, like
    /// the overlaps it is about.
    pub overlap_plans: HashMap<(String, String), String>,
    /// Pairs of workspaces where one branch is built on the other, keyed like the
    /// overlaps, ordered by id ([`crate::overlap::stack_of`]). Their overlap is a
    /// restack, not a conflict to settle. Memory only, like the overlaps.
    pub stacks: HashMap<(String, String), crate::overlap::Stack>,
    /// What each side of a stack was last told, keyed by (its workspace, the
    /// other's), as the lower head it was told about. **Per pair, not per
    /// workspace**: kept per workspace, a branch counted as stacked on four others
    /// overwrote its own record on every refresh and was told all four again
    /// each time, about 1,500 times in an hour.
    pub stack_told: HashMap<(String, String), String>,
    /// PRs whose head is **not** the local branch of the same name, with why —
    /// `git::HeadCheck::refusal` as the last PR poll found it, and kept in step by
    /// every fresh read ([`AppState::pr_head`]).
    ///
    /// A cache for the snapshot only, which cannot run git: it maps each PR row to
    /// a tree and a session by head ref, and a same-named branch of somebody
    /// else's is neither. Every flow that acts on a PR asks fresh instead.
    pub foreign_heads: HashMap<u64, String>,
    /// Your own GitHub login, from the PR poll's `viewer { login }`. The vendored
    /// prompts take it as `{{LOGIN}}`.
    pub viewer: Option<String>,
    /// Last poll failure. A broken poller must read as broken, never as "no
    /// open PRs".
    pub pr_error: Option<String>,
    /// What the agent said on its way out, when a session died before it could
    /// have a turn and we did not stop it.
    ///
    /// **Board-level rather than on the row, because the row is deleted.** A
    /// turnless session is forgotten by design — `spawn::watch_session_exit` says
    /// why — so a notice hung on it would go with it, which is exactly the silence
    /// this exists to end: a `claude` that cannot start takes the pane, the row and
    /// the worktree with it and leaves nothing on screen at all. It is state rather
    /// than an event because it is one: a binary that will not run will not run for
    /// the next press either, until somebody fixes the machine. Cleared by the
    /// first session that reaches a turn.
    pub agent_error: Option<String>,
    /// The agents this checkout's `PATH` has, in [`crate::agent::Agent::ALL`]'s
    /// order. Claude Code alone until the boot probe answers, which is what the
    /// board offered before there was a choice.
    pub agents: Vec<crate::agent::Agent>,
    /// The agent a new session gets when nobody names one: the one named last.
    /// Persisted ([`crate::store::save_last_agent`]), so a restart keeps it.
    pub last_agent: crate::agent::Agent,
    /// What the boot preflight found — see [`crate::machine::check`].
    ///
    /// Set once, by the caller that ran the check, and never changed: every one of
    /// these is a fact about the machine that a running daemon cannot fix. Here
    /// rather than in `AppState::new`'s signature because it is not what a daemon
    /// is *built* from — the check runs before the state exists, and a fourth
    /// constructor argument would be threaded through every test that makes one.
    pub machine: Vec<crate::machine::Warning>,
    pub pr_fetched: Option<SystemTime>,
    /// Bumped once per completed PR poll, so the refresh button can spin until
    /// the fetch it triggered has landed. Mirrors `reviews_poll`.
    pub pr_poll: u64,
    /// A fetch is in flight right now. `pr_poll` says one *landed*, which is what
    /// stops the spinner; this is what starts it when nobody pressed the button.
    pub pr_polling: bool,
    pub token_source: Option<crate::forge::TokenSource>,
    pub reviews: crate::reviews::ReviewState,
    /// Bumped once per completed review poll. The SPA watches it to spin the
    /// refresh button until the fetch its click triggered has actually landed,
    /// rather than until the next unrelated re-render.
    pub reviews_poll: u64,
    /// As `pr_polling`, for the review queue.
    pub reviews_polling: bool,
    /// When the last review poll landed, for the "as of Nm ago" the pane shows.
    /// Mirrors `pr_fetched`.
    pub reviews_fetched: Option<SystemTime>,
    /// Files rewritten through the diff editor, and which sessions have been
    /// told. Conflict detection on save protects you from the agent; this is
    /// the other direction, which is the one that loses work silently.
    pub human_edits: HashMap<PathBuf, HumanEdit>,
    pub automation: Durable<crate::model::AutomationStore>,
    /// The worktree cut in flight, and what its scripts are saying. Cleared to
    /// `running: false` when they finish, and left there — the last create's output
    /// is what the pane shows while the session it belongs to boots.
    pub create_run: Option<crate::model::CreateRun>,
    /// A newer GitHub release than the running build, if the update poller has
    /// found one. Surfaced to the SPA as a dismissible nudge, with a button when
    /// mise is what installed us.
    pub update: Option<crate::model::UpdateInfo>,
    /// The app's own upgrade, while it runs and after it ends. The sibling of
    /// `upgrade_run`, kept apart from it because both can be in flight at once and
    /// a shared slot would let one report the other's outcome.
    pub self_upgrade_run: Option<crate::model::UpgradeRun>,
    /// A newer Claude Code than the one installed, if the agent poller has found
    /// one. Unlike `update` this one is actionable in place: upgrading cannot
    /// disturb a running session, so the SPA offers a button rather than a link.
    pub agent_update: Option<crate::model::AgentUpdate>,
    /// An agent upgrade in flight, or the failure one left behind. Deliberately
    /// not a workspace process — see `update::UpgradeRun`.
    pub upgrade_run: Option<crate::model::UpgradeRun>,
    /// Workspaces the spare pool is holding: cut, based and waiting for the next
    /// unnamed create. See [`crate::spare`].
    ///
    /// **In `Inner` so that taking one is atomic with everything else the write
    /// lock protects.** A claim reads the id and removes it in a single critical
    /// section, because the thing that used to refuse a second claimant was `git
    /// worktree add` failing on an existing path — and a claim does no git at all.
    pub spare: Durable<crate::store::SpareStore>,
    /// Conversations in this checkout's directories that the daemon did not start.
    /// See [`crate::store::ExternalConversation`].
    ///
    /// Polled rather than derived, because the answer is a `read_dir` per
    /// workspace and a snapshot is built for every hook, every keystroke's worth of
    /// state change and every notify. `store::find_transcript` records what one
    /// such scan per session per snapshot would cost: a scan a second, forever.
    pub external: Vec<crate::store::ExternalConversation>,
}

/// Proof that the holder has the one lock every move of main's branch takes.
///
/// Only [`AppState::move_main`] makes one. The runtime's movers of main take it by
/// reference — `carry::move_out` and `move_out_carrying`, `carry::move_back`,
/// `carry::switch_main_to_pr` and `relocate`'s `undo_swap` — so a new path into any
/// of them without the lock does not compile. Every mover lives in `carry` or
/// `relocate`, so those two files are where to look for the next one. **The git
/// functions underneath are not covered**: they live in `orchd-base`, below this
/// type, and are called directly only by the swap and `park_main`, which take the
/// guard themselves. A new caller of those is the one case this cannot catch.
/// Given back on drop.
pub struct MainMove<'a> {
    _held: tokio::sync::MutexGuard<'a, ()>,
}

/// A session that has claimed main and is still starting, until its spawn returns.
///
/// **Marked, not inferred.** "Claimed, with no record yet" was the first reading
/// of this, and it missed a resume: a resume keeps its id, so the stopped record
/// is still there while it starts, and the window stayed open for exactly the
/// sessions most likely to come back into main. A guard the claim hands back is
/// the one thing that knows a start is in progress, and dropping it — the spawn
/// returning, failed or not — is the one thing that ends it.
pub struct StartingInMain<'a> {
    app: &'a AppState,
    session: SessionId,
}

impl std::fmt::Debug for StartingInMain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("StartingInMain")
            .field(&self.session)
            .finish()
    }
}

impl Drop for StartingInMain<'_> {
    fn drop(&mut self) {
        self.app
            .starting_in_main
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.session);
    }
}

/// Main reserved for a park. See [`AppState::reserve_main_for_park`].
pub struct ParkReservation<'a> {
    app: &'a AppState,
}

impl Drop for ParkReservation<'_> {
    fn drop(&mut self) {
        self.app
            .parking_main
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.app.park_done.notify_waiters();
    }
}

/// Who is in a workspace. See [`AppState::occupancy`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Occupancy {
    /// Sessions that occupy it now ([`occupies`]), oldest first.
    pub live: Vec<SessionId>,
    /// A session auto-resume has not reached yet, which holds its tree as much.
    pub waiting: Option<SessionId>,
}

impl Occupancy {
    /// Whoever a refusal should name: the oldest live session, else the one
    /// coming back.
    pub fn holder(&self) -> Option<SessionId> {
        self.live.first().copied().or(self.waiting)
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty() && self.waiting.is_none()
    }
}

/// Whether a session occupies its workspace: its state is live, and its process
/// is not known to be dead.
///
/// **A dead pid does not count, and a missing one does.** The first is §8b: after
/// a crash the in-memory state is exactly what cannot be trusted, and teardown
/// asks this before it deletes a tree. The second is a session the spawn has
/// recorded before its pty exists — `insert_and_spawn` writes `pid: None` first —
/// and treating that as nobody let two presses inside the window both pass.
pub fn occupies(s: &Session) -> bool {
    s.state.is_live() && s.pid.is_none_or(pid_alive)
}

/// Releases a lock taken with [`AppState::try_claim`] when it goes out of scope.
///
/// A guard rather than a release at each exit, because the work these protect has
/// several `?` between the claim and the finish and every one of them would
/// otherwise leak the lock — which, for a once-per-PR lock, means the button never
/// works again until a restart.
///
/// The one guard for every named lock. `api.rs` carried a second copy of this for
/// the post lock, and both released through a spawned task; see
/// [`AppState::locks_held`] for why the release is synchronous now.
pub struct Claim {
    app: Arc<AppState>,
    lock: String,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Ok(mut held) = self.app.locks_held.lock() {
            held.remove(&self.lock);
        }
    }
}

/// A cheap stand-in for "which sessions exist": their count, and their ids mixed
/// together. Two different sets colliding would delay one write by a second, which
/// is why a hash is enough and a clone of every id is not needed.
///
/// Free rather than a method, so `persist` can compute it from the guard it already
/// holds instead of taking the lock a second time — the two reads disagreeing is
/// what made a brand-new session's record read as already written.
fn mixed_session_ids(inner: &Inner) -> u64 {
    let mut mixed = inner.sessions.len() as u64;
    for id in inner.sessions.keys() {
        let (hi, lo) = id.as_u64_pair();
        mixed = mixed.rotate_left(7) ^ hi ^ lo;
    }
    mixed
}

impl Inner {
    /// Refuse an agent this checkout cannot run.
    ///
    /// **Every spawn asks, not only a new one.** A resume, a fork, a restart and
    /// auto-resume continue a conversation in the agent that holds it, and only
    /// the new-session path used to check: the rest opened a pane that died with
    /// `command not found`, and a restart killed the session first. A turnless
    /// pane that dies is forgotten with its worktree, so the one sentence about it
    /// was in the log.
    pub fn installed(&self, agent: crate::agent::Agent) -> Result<(), crate::model::Refusal> {
        if self.agents.contains(&agent) {
            Ok(())
        } else {
            Err(crate::model::Refusal::Missing(format!(
                "{agent} is not on this checkout's PATH"
            )))
        }
    }

    /// Record what became of a PR the poll has stopped reporting.
    ///
    /// **Replaces rather than appends on a repeat**, because a PR can end twice:
    /// closed, reopened, merged. The last answer is the true one, and two rows for
    /// one number would be the rail arguing with itself.
    ///
    /// Capped at [`ENDED_MAX`], newest first. The cap is what stops a long-lived
    /// daemon on a busy repo from carrying every PR it ever saw; the rail only
    /// ever draws the handful whose worktrees are still here.
    pub fn remember_ended(&mut self, ended: crate::forge::EndedPr) {
        // The sessions keep it past the ten this list holds, and past a restart.
        for s in self.sessions.values_mut() {
            for p in s.prs.iter_mut().filter(|p| p.number == ended.number) {
                p.outcome = Some(ended.outcome);
                // A PR the session only knew by number, from a record older than
                // the list: the answer names it.
                if p.title.is_empty() {
                    p.title.clone_from(&ended.title);
                }
                if p.url.is_empty() {
                    p.url.clone_from(&ended.url);
                }
            }
        }
        self.prs_ended.retain(|p| p.number != ended.number);
        self.prs_ended.insert(0, ended);
        self.prs_ended.truncate(ENDED_MAX);
    }

    /// Write each session's open PR onto it, matched by branch.
    ///
    /// By the session's own branch rather than its workspace, because the
    /// workspace is what a swap or a reap changes and the branch is what the
    /// conversation was about. A head the checkout cannot vouch for
    /// (`foreign_heads`) is skipped: a fork's PR on a same-named branch is not
    /// this session's. Called after every successful poll; the notify that
    /// follows the poll writes the records.
    ///
    /// Every branch the session held, not only the one it started on, so an agent
    /// that opens a second PR gets it on its record: each is appended to
    /// `Session::prs` once and refreshed while open. `Session::pr` stays the last
    /// one seen, for the archive row.
    pub fn note_session_prs(&mut self) {
        for s in self.sessions.values_mut() {
            // A record from before the list knew only `pr`: that one is the start
            // of it, named once the poll or `session_prs_to_ask` answers for it.
            if s.prs.is_empty() {
                if let Some(n) = s.pr {
                    s.prs.push(crate::model::SessionPr {
                        number: n,
                        title: String::new(),
                        url: String::new(),
                        outcome: None,
                    });
                }
            }
            let heads: Vec<&str> = s
                .branch
                .iter()
                .chain(s.branches.iter())
                .map(String::as_str)
                .collect();
            if heads.is_empty() {
                continue;
            }
            let mut found: Vec<&crate::forge::Pr> = self
                .prs
                .iter()
                .filter(|p| {
                    heads.contains(&p.head_ref.as_str())
                        && !self.foreign_heads.contains_key(&p.number)
                })
                .collect();
            // Opened order is number order on one repo.
            found.sort_by_key(|p| p.number);
            for p in &found {
                match s.prs.iter_mut().find(|k| k.number == p.number) {
                    Some(k) => {
                        k.title.clone_from(&p.title);
                        k.url.clone_from(&p.url);
                        // Open in the poll again, so a reopened PR is open again.
                        k.outcome = None;
                    }
                    None => s.prs.push(crate::model::SessionPr {
                        number: p.number,
                        title: p.title.clone(),
                        url: p.url.clone(),
                        outcome: None,
                    }),
                }
            }
            // Number order is opened order on one repo, and the poll finds them one
            // at a time rather than oldest first.
            s.prs.sort_by_key(|k| k.number);
            if let Some(p) = found.last() {
                s.pr = Some(p.number);
            }
        }
    }

    /// Live sessions' PRs that are neither in the poll nor known to have ended.
    ///
    /// **The PRs that ended while the daemon was down.** `outcomes_to_ask` only
    /// sees a PR leave the open list between two polls of one run, and the ended
    /// list is memory, so after a restart a merged PR was simply gone from its
    /// session. The caller asks each number once per run (`asked`), so a PR the
    /// forge will not answer for costs one request, not one per poll.
    pub fn session_prs_to_ask(
        &self,
        fresh: &[crate::forge::Pr],
        asked: &mut HashSet<u64>,
    ) -> Vec<u64> {
        let mut out = Vec::new();
        for s in self.sessions.values().filter(|s| s.state.is_live()) {
            for p in s.prs.iter().filter(|p| p.outcome.is_none()) {
                if !fresh.iter().any(|f| f.number == p.number) && asked.insert(p.number) {
                    out.push(p.number);
                }
            }
        }
        out
    }

    /// Branches of live sessions with no PR known on them, to ask the forge about.
    ///
    /// **A PR that opened and ended between two runs left no number to ask
    /// about**, only the branch it was on. Each branch is asked once per run
    /// (`asked`); one with an open PR in the poll needs no question.
    pub fn session_heads_to_ask(
        &self,
        fresh: &[crate::forge::Pr],
        asked: &mut HashSet<String>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for s in self.sessions.values().filter(|s| s.state.is_live()) {
            for b in s.branch.iter().chain(s.branches.iter()) {
                if fresh.iter().any(|p| &p.head_ref == b) {
                    continue;
                }
                if asked.insert(b.clone()) {
                    out.push(b.clone());
                }
            }
        }
        out
    }

    /// An ended PR found by its branch: onto every session that held the branch,
    /// in number order, and into the ended list like any other.
    pub fn note_ended_on_head(&mut self, ended: crate::forge::EndedPr) {
        for s in self.sessions.values_mut() {
            let held = s.branch.as_deref() == Some(ended.head_ref.as_str())
                || s.branches.contains(&ended.head_ref);
            if held && !s.prs.iter().any(|p| p.number == ended.number) {
                let at = s.prs.partition_point(|p| p.number < ended.number);
                s.prs.insert(
                    at,
                    crate::model::SessionPr {
                        number: ended.number,
                        title: ended.title.clone(),
                        url: ended.url.clone(),
                        outcome: Some(ended.outcome),
                    },
                );
            }
        }
        self.remember_ended(ended);
    }

    /// What `workspace` should hear about the stacks it is in, as one note per
    /// pair it has not been told about at this lower head; the pair is marked
    /// told. `force` says it again regardless (the card's "Ask it to restack").
    ///
    /// **Both sides hear it**, so the two agents can agree who updates which PR
    /// when the stack needs it: the upper one restacks, and the lower one has to
    /// say when it pushes or rewrites, which nothing else tells the upper one.
    /// Only the nearest stack of an upper branch is spoken about, and only with a
    /// running session on the other side to talk to.
    pub fn stack_notes(&mut self, workspace: &str, force: bool) -> Vec<String> {
        let nearest = self.stacked_on(workspace).map(|(lower, _)| lower);
        let mut due = Vec::new();
        for ((a, b), st) in &self.stacks {
            let (lower, upper) = if st.lower_is_a { (a, b) } else { (b, a) };
            let (me_upper, other) = if upper == workspace {
                (true, lower)
            } else if lower == workspace {
                (false, upper)
            } else {
                continue;
            };
            if me_upper && nearest.as_deref() != Some(other.as_str()) {
                continue;
            }
            let key = (workspace.to_string(), other.clone());
            if !force && self.stack_told.get(&key) == Some(&st.lower_tip) {
                continue;
            }
            due.push((me_upper, other.clone(), st.clone(), key));
        }
        let mut notes = Vec::new();
        for (me_upper, other, st, key) in due {
            let Some(x) = self
                .sessions
                .values()
                .find(|x| x.workspace == other && x.state.is_live())
            else {
                continue;
            };
            let name = x.label().unwrap_or(&x.workspace).to_string();
            let id = x.id;
            let branch = self
                .workspaces
                .get(&other)
                .and_then(|w| w.tree.branch.clone())
                .unwrap_or_else(|| other.clone());
            let note = if me_upper {
                let conflicts = self
                    .overlaps
                    .iter()
                    .filter(|p| {
                        p.found.conflict && p.involves(workspace) && p.other(workspace) == other
                    })
                    .count();
                let behind = match (st.behind, conflicts) {
                    (0, _) => " You have all of its commits.".to_string(),
                    (n, 0) => format!(" It has {n} commit{} you do not have yet; restacking should apply cleanly.", if n == 1 { "" } else { "s" }),
                    (n, c) => format!(" It has {n} commit{} you do not have yet, and {c} file{} will conflict when you restack.", if n == 1 { "" } else { "s" }, if c == 1 { "" } else { "s" }),
                };
                format!(
                    "orchd: your branch is stacked on `{branch}`, the branch of session `{name}` \
                     (id {id}): its commits are in yours.{behind} When it changes you restack \
                     onto it. Agree with that session who updates which PR, with `orch tell {id} \
                     --text \"...\"`, rather than both doing it."
                )
            } else {
                format!(
                    "orchd: session `{name}` (id {id}, branch `{branch}`) is stacked on your \
                     branch: your commits are in its branch. When you push new commits or \
                     rewrite yours, tell it with `orch tell {id} --text \"...\"` so it can \
                     restack, and agree with it who updates which PR."
                )
            };
            self.stack_told.insert(key, st.lower_tip.clone());
            notes.push(note);
        }
        notes
    }

    /// A PR merged: every live session whose workspace shares a file with the
    /// workspace that held its branch is told to rebase, through its inbox.
    /// Returns those sessions, for the caller to deliver to once the lock is gone.
    ///
    /// **This is where the CI conflict happens**, not when the two trees first
    /// collided: the second PR is now behind a base that already has the other
    /// half. Said once, at the merge, while the files are still listed: the
    /// overlap goes the next time either side reconciles.
    pub fn tell_overlaps_merged(
        &mut self,
        ended: &crate::forge::EndedPr,
        base: &str,
    ) -> Vec<SessionId> {
        let Some(merged) = self
            .workspaces
            .values()
            .find(|w| {
                w.branches.contains(&ended.head_ref)
                    || w.tree.branch.as_deref() == Some(ended.head_ref.as_str())
            })
            .map(|w| w.id.clone())
        else {
            return Vec::new();
        };
        let mut told = Vec::new();
        let others: Vec<String> = self
            .overlaps
            .iter()
            // A file that merges cleanly is not a reason to stop and rebase, unless
            // the merged branch is the one this is stacked on.
            .filter(|p| (p.found.conflict || self.stacked(p)) && p.involves(&merged))
            .map(|p| p.other(&merged).to_string())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        for ws in others {
            let files: Vec<String> = self
                .overlaps
                .iter()
                .filter(|p| {
                    (p.found.conflict || self.stacked(p))
                        && p.involves(&merged)
                        && p.other(&merged) == ws
                })
                .map(|p| format!("- {}", p.found.path))
                .collect();
            for s in self
                .sessions
                .values_mut()
                .filter(|s| s.workspace == ws && s.state.is_live())
            {
                s.inbox.push(format!(
                    "orchd: PR #{} ({}) just merged, and it changed files you changed too:\n{}\n\
                     Rebase onto {base} now, before you build further on the old base, and \
                     settle the conflicts in your own tree.",
                    ended.number,
                    ended.title,
                    files.join("\n")
                ));
                told.push(s.id);
            }
        }
        told
    }

    /// Forget an ended PR, for the one case that un-ends: a reopened number
    /// coming back in the poll. Without this the row would keep saying `closed`
    /// about a PR that is open again, and the open list and the ended list would
    /// both claim it.
    pub fn forget_ended(&mut self, number: u64) {
        self.prs_ended.retain(|p| p.number != number);
        self.pending_outcome.retain(|(n, _)| *n != number);
    }

    /// Which vanished PRs are still worth asking about, and queue the new ones.
    ///
    /// Called with the poll's fresh list. A PR that left it joins the queue; one
    /// that came back leaves it, open and ended both. What is returned is every
    /// queued number whose head branch a workspace still holds — the bound on this
    /// whole mechanism, since a PR with no worktree left has no row to tell.
    pub fn outcomes_to_ask(&mut self, fresh: &[crate::forge::Pr]) -> Vec<u64> {
        for p in fresh {
            self.forget_ended(p.number);
        }
        let gone: Vec<(u64, String)> = self
            .prs
            .iter()
            .filter(|p| !fresh.iter().any(|f| f.number == p.number))
            .map(|p| (p.number, p.head_ref.clone()))
            .collect();
        for entry in gone {
            if !self.pending_outcome.iter().any(|(n, _)| *n == entry.0) {
                self.pending_outcome.push(entry);
            }
        }
        // The worktree is what makes this worth a request. Dropped here rather than
        // when the worktree goes, because this is the one place that walks the list.
        let held = |branch: &String| {
            self.workspaces
                .values()
                .any(|w| w.branches.contains(branch))
        };
        self.pending_outcome.retain(|(_, head)| held(head));
        self.pending_outcome.iter().map(|(n, _)| *n).collect()
    }
}

/// How many ended PRs to carry. Ten is more than the open list ever shows and far
/// more than have worktrees still on disk.
const ENDED_MAX: usize = 10;

impl Inner {
    /// Whether a workspace's tree can have changed since it was last measured,
    /// which is what the timer sweep measures and what the snapshot carries in
    /// full.
    ///
    /// **One rule for both, because they answer one question.** A tree changes
    /// when something works in it: a live session, a process in its drawer. Main
    /// is always in, because the context bar reads it with nothing selected, and
    /// so is one never measured, which has nothing to show yet. Everything else
    /// was measured and holds still: on the monorepo this is developed against,
    /// 194 of 200 workspaces.
    pub fn is_active_workspace(&self, id: &str) -> bool {
        id == crate::model::MAIN
            || self
                .sessions
                .values()
                .any(|s| s.workspace == id && s.state.is_live())
            || self.workspaces.get(id).is_some_and(|w| {
                !w.tree.measured
                    || w.processes
                        .iter()
                        .any(|p| p.pty.as_ref().is_some_and(|h| h.is_alive()))
            })
    }

    /// The brief that makes the session in `mine` settle its conflicts with the
    /// workspaces in `with`, and the sessions it names. Appended to its next
    /// prompt by [`AppState::overlap_note`]: automatic, and never a turn of its
    /// own, so nothing the person is doing gets interrupted.
    pub fn solve_brief(&self, mine: &str, with: &[String]) -> (String, Vec<SessionId>) {
        let mut partners = Vec::new();
        let mut sections = Vec::new();
        for ws in with {
            let Some(b) = self
                .sessions
                .values()
                .find(|s| &s.workspace == ws && s.state.is_live())
            else {
                continue;
            };
            let paths: Vec<&str> = self
                .overlaps
                .iter()
                .filter(|p| {
                    p.found.conflict && !self.stacked(p) && p.involves(mine) && p.other(mine) == ws
                })
                .map(|p| p.found.path.as_str())
                .collect();
            let name = b.label().unwrap_or(&b.workspace).to_string();
            let about = b
                .title
                .as_deref()
                .map(|t| format!("\n  Their conversation is about: {t}."))
                .unwrap_or_default();
            let branch = self
                .workspaces
                .get(ws)
                .and_then(|w| w.tree.branch.clone())
                .map(|br| format!(", branch `{br}`"))
                .unwrap_or_default();
            sections.push(format!(
                "- session `{name}` (id {}, workspace `{ws}`{branch}): {}{about}",
                b.id,
                paths.join(", ")
            ));
            partners.push(b.id);
        }
        let several = partners.len() > 1;
        let brief = format!(
            "orchd: git cannot merge files you changed with what {} changed:\n{}\n\n\
             Before you go on with this prompt, settle it with them; you lead.\n\
             1. Talk it through with `orch tell <id> --text \"...\"`. They read it as their \
             next turn and can answer the same way. {TELL_CAP} messages each way.\n\
             2. Agree on one strategy{}: adapt (one of you changes your code to fit the \
             other's), merge-first (one PR merges first and the other rebases after it), \
             stack (one branch is rebased onto the other and its PR targets that branch), \
             or hand-over (one of you leaves the file to the other).\n\
             3. Ask the user to confirm with `orch ask`, the agreed strategy as the first \
             option, each value starting `overlap-` (overlap-adapt, overlap-merge-first, \
             overlap-stack, overlap-hand-over), a label that says who does what, and the \
             reasoning in --detail.\n\
             4. Do your half, tell them the answer, then go on with the prompt.\n\
             Change only your own worktree; the others change theirs.",
            if several {
                "these sessions"
            } else {
                "this session"
            },
            sections.join("\n"),
            if several { " with each of them" } else { "" },
        );
        (brief, partners)
    }

    /// Whether the overlap `p` is between two branches of one stack, which is a
    /// restack rather than a conflict to settle.
    pub fn stacked(&self, p: &crate::overlap::Pair) -> bool {
        self.stacks.contains_key(&(p.a.clone(), p.b.clone()))
    }

    /// The stack `workspace` sits on top of, as the lower workspace and the stack.
    /// On a branch built on two (on one that is itself built on another), the
    /// nearer one: the lower branch with the most commits.
    pub fn stacked_on(&self, workspace: &str) -> Option<(String, &crate::overlap::Stack)> {
        self.stacks
            .iter()
            .filter_map(|((a, b), s)| {
                let (lower, upper) = if s.lower_is_a { (a, b) } else { (b, a) };
                (upper == workspace).then(|| (lower.clone(), s))
            })
            .max_by_key(|(_, s)| s.lower_commits)
    }

    /// Whether a session that is not archived works in this workspace.
    ///
    /// Narrower than [`Self::is_active_workspace`] on purpose: an overlap is only
    /// worth saying while somebody can still act on both sides, and main or a
    /// drawer process with nobody in it is not that.
    pub fn has_live_session(&self, id: &str) -> bool {
        self.sessions
            .values()
            .any(|s| s.workspace == id && s.state.is_live())
    }

    /// Which workspace an absolute path belongs to, without taking the lock.
    ///
    /// The body of [`AppState::workspace_for_path`], which delegates here. Split
    /// out because a caller that already holds the write guard cannot ask the
    /// `AppState` version for the answer: `RwLock` is not reentrant, so the read
    /// it takes would deadlock against the write it is called under.
    pub fn workspace_for_path(&self, path: &Path) -> Option<WorkspaceId> {
        self.workspaces
            .values()
            .filter(|w| path.starts_with(&w.path))
            .max_by_key(|w| w.path.as_os_str().len())
            .map(|w| w.id.clone())
    }

    /// The workspace that has `head_ref` checked out **now**, among those `allow`
    /// accepts.
    ///
    /// **Not the branch set.** `Workspace::branches` is every branch a tree has
    /// ever held, and only a swap ever removes one, so asking it "who has this
    /// branch" answered with history: main parked off a PR branch and went on
    /// claiming the PR, and the row's session button then took you to whoever was
    /// in main next, working on something else.
    ///
    /// Ordered by id, because `workspaces` is a `HashMap`: two trees on one branch
    /// is rare, and a tie that changes its answer on a rehash is worse than either
    /// answer.
    pub fn workspace_on(
        &self,
        head_ref: &str,
        allow: impl Fn(&Workspace) -> bool,
    ) -> Option<WorkspaceId> {
        self.workspaces
            .values()
            .filter(|w| allow(w) && w.tree.branch.as_deref() == Some(head_ref))
            .min_by(|a, b| a.id.cmp(&b.id))
            .map(|w| w.id.clone())
    }

    /// The live session whose conversation is about `head_ref`.
    ///
    /// By [`Session::branch`], the branch the conversation is about, rather than by
    /// who is in the tree: that is what a swap already carries sessions by, and the
    /// PR row's button promises "the session on this branch". The newest wins when
    /// several share one, which is main with sharing on.
    pub fn session_on(&self, head_ref: &str) -> Option<SessionId> {
        self.sessions
            .values()
            .filter(|s| s.state.is_live() && s.branch.as_deref() == Some(head_ref))
            .max_by_key(|s| (s.created_at, s.id))
            .map(|s| s.id)
    }

    /// The PR by number, as the last poll saw it.
    pub fn pr(&self, number: u64) -> Option<&crate::forge::Pr> {
        self.prs.iter().find(|p| p.number == number)
    }

    /// Mutating the stores that outlive the daemon.
    ///
    /// `sessions.json` is written by every `notify()`, so a session record cannot be
    /// changed without being persisted. `automation` and `spare` had no
    /// such guarantee: each was durable only because every mutation site remembered
    /// to call the matching `store::save_*` afterwards. That held, but it made
    /// durability a property of the caller's memory — and a lost automation write in
    /// particular defeats the one-run-per-PR cap rather than merely losing a label.
    ///
    /// So the write lives with the mutation. `why` is the caller's own context, kept
    /// because "could not persist automation" is far less useful than knowing which
    /// PR and which moment. The closure returns whether it changed anything, so a
    /// no-op does not rewrite the file.
    ///
    /// Reads still go straight at the fields: they are many, harmless, and requiring
    /// an accessor for each would be noise. It is *mutation* that has to carry the
    /// write with it.
    /// [`crate::model::AutomationStore::take_back`], carrying its own write.
    pub fn take_back_fix_run(
        &mut self,
        pr: u64,
        session: crate::model::SessionId,
        why: &str,
    ) -> Result<(), crate::model::SessionId> {
        let mut out = Ok(());
        self.with_automation(why, |a| match a.take_back(pr, session) {
            Ok(wrote) => wrote,
            Err(other) => {
                out = Err(other);
                false
            }
        });
        out
    }

    pub fn with_automation(
        &mut self,
        why: &str,
        f: impl FnOnce(&mut crate::model::AutomationStore) -> bool,
    ) -> bool {
        let changed = f(&mut self.automation.0);
        if changed {
            if let Err(e) = crate::store::save_automation(&self.automation) {
                tracing::error!("could not persist automation ({why}): {e:#}");
            }
        }
        changed
    }

    /// The spare pool's ids, and the write that keeps `spare.json` agreeing with
    /// them. A pool that failed to persist costs one abandoned tree after the next
    /// restart, never a session.
    pub fn with_spare(
        &mut self,
        why: &str,
        f: impl FnOnce(&mut crate::store::SpareStore) -> bool,
    ) -> bool {
        let changed = f(&mut self.spare.0);
        if changed {
            if let Err(e) = crate::store::save_spare(&self.spare) {
                tracing::warn!("could not save spare.json ({why}): {e:#}");
            }
        }
        changed
    }
}

/// How long a [`HumanEdit`] is kept before [`AppState::record_human_edit`] prunes
/// it. See there for why a bound at all.
const HUMAN_EDIT_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone)]
pub struct HumanEdit {
    pub at: SystemTime,
    /// Sessions already interrupted about this edit. Each is told exactly once,
    /// so the retry after re-reading goes through.
    pub told: std::collections::HashSet<SessionId>,
}

/// A store that is on disk as well as in memory.
///
/// **Read through it freely; the only way to change it is an `Inner::with_*`.**
/// `Deref` and deliberately no `DerefMut` is how that is said to the compiler:
/// every reader goes on writing `inner.automation.get(pr)` unchanged, and
/// `inner.automation.insert(…)` outside this module stops compiling.
///
/// The rule it replaces was a paragraph — "mutating a durable store carries its
/// own write" — and the failure it guards is the quiet one: a record changed in
/// memory and never written, which looks correct until a restart drops it. There
/// were no offenders when this went in, which is the argument for it rather than
/// against: nothing would have reported the first one.
#[derive(Debug, Default, Clone)]
pub struct Durable<T>(T);

impl<T> Durable<T> {
    /// Wrap what was just read off disk. The only other writer is a `with_*`.
    pub fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> std::ops::Deref for Durable<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl AppState {
    /// The settings as last saved. A copy, so the lock is never held past the
    /// line that reads it.
    /// The story link a ref opens through: the one set in Settings, else the one
    /// the tracker implies. `None` leaves a ref as plain text.
    pub fn story_link(&self) -> Option<crate::config::StoryLink> {
        self.settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .story_link
            .clone()
            .filter(|s| s.usable())
            .or_else(|| self.story_default.get().cloned())
    }

    pub fn settings(&self) -> crate::config::Settings {
        self.settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Take a saved `next` in place of the current settings, and say whether
    /// the daemon has to restart for all of it to apply.
    pub fn replace_settings(&self, next: crate::config::Settings) -> bool {
        let restart = crate::config::Settings::of(&self.cfg).needs_restart(&next);
        *self
            .settings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
        restart
    }

    /// Hand the boot preflight's findings to the state, once.
    ///
    /// Separate from the constructor because the check runs *before* the state
    /// exists — it reads the config and the machine, not the daemon — and because
    /// nothing else ever writes this: a missing `gh` is missing until somebody
    /// installs one, and this daemon will not see that happen.
    pub async fn set_machine_warnings(&self, warnings: Vec<crate::machine::Warning>) {
        self.inner.write().await.machine = warnings;
    }

    /// Find which agents this checkout can run, through its own environment.
    ///
    /// The checkout's `PATH` and not the daemon's, because that is the one a session
    /// gets: a repo that pins Codex through mise has it on no other `PATH`, and a
    /// launcher-started daemon has hardly any `PATH` at all. Once, at boot, for the
    /// same reason as the preflight: an agent installed later is a restart away.
    pub async fn probe_agents(self: &Arc<Self>) {
        let (main, source) = (self.cfg.main_checkout.clone(), self.cfg.env_source);
        // The last pick is read here too, off the runtime: `AppState::new` runs on
        // a tokio worker, where a `std::fs` read is not allowed.
        let (found, last) = crate::proc::run_blocking("looking for the agents", move || {
            let env = crate::env_source::read(source, &main);
            let found = crate::agent::Agent::ALL
                .into_iter()
                .filter(|a| crate::pty::which(a.program(), &main, &env, &[]).is_ok())
                .collect::<Vec<_>>();
            (found, crate::store::load_last_agent(), env)
        })
        .await
        .map(|(found, last, env)| {
            // Asked now, while nothing waits on it: the hook trust is an
            // `app-server` round trip, cached after the first, and otherwise the
            // first Codex spawn of every start paid it before its pane opened.
            // Detached, because auto-resume waits on this probe and must not wait
            // on that round trip as well.
            if found.contains(&crate::agent::Agent::Codex) {
                let main = self.cfg.main_checkout.clone();
                // A refusal here is the one every Codex spawn will give, so it is
                // said once at boot too, where the reason is easiest to find.
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = crate::codex::flags(&main, &env, &[]) {
                        tracing::warn!("Codex sessions will be refused: {e:#}");
                    }
                });
            }
            (found, last)
        })
        .unwrap_or_default();
        tracing::info!(?found, "agents on this checkout's PATH");
        let mut inner = self.inner.write().await;
        inner.last_agent = last;
        if !found.is_empty() {
            inner.agents = found;
        }
        drop(inner);
        self.notify().await;
    }

    /// The agent a new session gets: the one asked for, which is then remembered,
    /// or the one asked for last.
    ///
    /// Refuses one this checkout does not have, because the alternative is a pane
    /// that opens and dies with `command not found`.
    pub async fn agent_for(
        &self,
        asked: Option<crate::agent::Agent>,
    ) -> Result<crate::agent::Agent, crate::model::Refusal> {
        let mut inner = self.inner.write().await;
        let Some(agent) = asked else {
            return Ok(self.default_agent(&inner));
        };
        inner.installed(agent)?;
        if inner.last_agent != agent {
            inner.last_agent = agent;
            tokio::task::spawn_blocking(move || {
                if let Err(e) = crate::store::save_last_agent(agent) {
                    tracing::warn!("could not remember the agent: {e:#}");
                }
            });
        }
        Ok(agent)
    }

    /// What a session started without naming an agent runs: the configured
    /// default when it is installed, else the one picked last.
    fn default_agent(&self, inner: &Inner) -> crate::agent::Agent {
        let installed = |a: &crate::agent::Agent| inner.agents.contains(a);
        // The last pick is a fallback, not a promise: it defaults to Claude Code,
        // and a checkout with only Codex on PATH would start `claude` and die.
        // The field alone: `settings()` clones all of them, and this runs per snapshot.
        let configured = self
            .settings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .default_agent;
        configured
            .filter(installed)
            .or(Some(inner.last_agent).filter(installed))
            .or(inner.agents.first().copied())
            .unwrap_or(inner.last_agent)
    }

    /// Change one thing on one session under the write lock; a session that is
    /// gone is not an error. The shape every hook handler has, written once.
    pub async fn with_session<R>(
        &self,
        id: SessionId,
        f: impl FnOnce(&mut Session) -> R,
    ) -> Option<R> {
        let mut inner = self.inner.write().await;
        inner.sessions.get_mut(&id).map(f)
    }

    /// Which workspace a session believes it is in, if the daemon still has it.
    ///
    /// Read rather than changed, and under the *read* lock: a hook that only
    /// wants to know whether this session is still on the pending placeholder
    /// should not queue behind writers to find out.
    pub async fn session_workspace(&self, id: SessionId) -> Option<String> {
        self.inner
            .read()
            .await
            .sessions
            .get(&id)
            .map(|s| s.workspace.clone())
    }

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
                tree: Default::default(),
                banked: None,
            },
        );
        let repos = Repos {
            /* **`cfg.repo` first, the same order [`crate::resolve_repo`] uses.** It
            did not, and the two then disagreed: a checkout that pins its repo in
            config polled that repo while the snapshot reported no upstream at all,
            so the pane naming it had nothing to name. Found by hiding the forge
            panes on this field and watching them vanish from a checkout whose PRs
            were being fetched. */
            /* The one ladder, shared with `resolve_repo` and `api::repo_of`. It was
            written out here instead, and the three then disagreed — most visibly
            once the SPA began hiding two panes on this field while the pollers used
            another answer. */
            upstream: crate::forge::upstream_repo(
                &cfg.main_checkout,
                &cfg.upstream_remote,
                cfg.repo.as_deref(),
            )
            .map(|(o, n)| format!("{o}/{n}")),
            fork: crate::forge::remote_url(&cfg.main_checkout, "origin")
                .and_then(|u| crate::forge::repo_from_remote(&u))
                .map(|(o, n)| format!("{o}/{n}")),
        };
        Arc::new(AppState {
            settings: std::sync::RwLock::new(crate::config::Settings::of(&cfg)),
            cfg,
            repos,
            token,
            story_default: std::sync::OnceLock::new(),
            locks_held: std::sync::Mutex::new(HashSet::new()),
            persist_pending: std::sync::atomic::AtomicBool::new(false),
            persist_writing: tokio::sync::Mutex::new(()),
            persisted_ids: std::sync::atomic::AtomicU64::new(0),
            run_observer: std::sync::OnceLock::new(),
            inner: RwLock::new(Inner {
                workspaces,
                sessions: HashMap::new(),
                prs: Vec::new(),
                prs_ended: Vec::new(),
                pending_outcome: Vec::new(),
                overlaps: Vec::new(),
                overlap_plans: HashMap::new(),
                stacks: HashMap::new(),
                stack_told: HashMap::new(),
                foreign_heads: HashMap::new(),
                spare: Default::default(),
                viewer: None,
                pr_error: None,
                agent_error: None,
                agents: vec![crate::agent::Agent::Claude],
                last_agent: crate::agent::Agent::default(),
                machine: Vec::new(),
                pr_fetched: None,
                pr_poll: 0,
                pr_polling: false,
                token_source: None,
                reviews: Default::default(),
                reviews_poll: 0,
                reviews_polling: false,
                reviews_fetched: None,
                human_edits: HashMap::new(),
                automation: Default::default(),
                create_run: None,
                update: None,
                self_upgrade_run: None,
                agent_update: None,
                upgrade_run: None,
                external: Vec::new(),
            }),
            swapping: tokio::sync::Mutex::new(()),
            parking_main: std::sync::atomic::AtomicBool::new(false),
            park_done: tokio::sync::Notify::new(),
            starting_in_main: std::sync::Mutex::new(HashSet::new()),
            sweeping: tokio::sync::Mutex::new(()),
            cutting: tokio::sync::Mutex::new(()),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            restored: std::sync::atomic::AtomicBool::new(false),
            pending_resume: std::sync::Mutex::new(std::collections::HashSet::new()),
            auto_resuming: std::sync::atomic::AtomicBool::new(false),
            previews: std::sync::Mutex::new(HashMap::new()),
            events,
            chrome,
            answered: Arc::new(Notify::new()),
            review_refresh: Arc::new(Notify::new()),
            pr_refresh: Arc::new(Notify::new()),
        })
    }

    /// Re-read this checkout's transcript directories for conversations the daemon
    /// did not start, and publish what changed.
    ///
    /// One body for the poller and for the button, deliberately: the fold rescans
    /// when you open it, and two copies of "which directories, which filter, what
    /// counts as changed" is how the list you are looking at comes to disagree with
    /// the count on the caret you opened.
    ///
    /// Notifies only on a change. The common answer is the previous answer, and a
    /// notify pushes a whole snapshot to every connected page.
    pub async fn rescan_external(self: &Arc<Self>) -> usize {
        let (workspaces, known, previous) = {
            let inner = self.inner.read().await;
            (
                inner
                    .workspaces
                    .values()
                    .map(|w| (w.id.clone(), w.path.clone()))
                    .collect::<Vec<_>>(),
                // Codex's ids as well as the daemon's: a Codex session the daemon
                // started is filed under the id Codex chose, and without this it
                // would be listed as somebody else's conversation.
                inner
                    .sessions
                    .values()
                    .flat_map(|s| {
                        let theirs = s
                            .agent_session_id
                            .as_deref()
                            .and_then(|t| Uuid::parse_str(t).ok());
                        std::iter::once(s.id).chain(theirs)
                    })
                    .collect::<std::collections::HashSet<_>>(),
                inner.external.clone(),
            )
        };
        // `std::fs` off the runtime, which is the rule every directory read here
        // follows.
        let scanned = crate::proc::run_blocking("scanning for outside conversations", move || {
            crate::store::external_conversations(&workspaces, &known, &previous)
        })
        .await;
        /* A scan that did not answer is not an empty archive. `unwrap_or_default`
        here emptied the fold on a panicked or cancelled blocking task, and the
        next poll a minute later put it back — a list that vanishes and returns
        is worse than one that is a minute stale, and it is the daemon's own
        fault either way. */
        let found = match scanned {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!("could not scan for outside conversations: {e:#}");
                return self.inner.read().await.external.len();
            }
        };
        let n = found.len();
        // Scoped, so the write guard is not held across the notify below.
        let changed = {
            let mut inner = self.inner.write().await;
            let changed = inner.external != found;
            if changed {
                inner.external = found;
            }
            changed
        };
        if changed {
            self.notify().await;
        }
        n
    }

    /// Push a fresh snapshot to every connected SPA. State is small enough that
    /// a whole snapshot beats a delta protocol nobody can debug.
    pub async fn notify(self: &Arc<Self>) {
        self.persist_when_due().await;
        let snapshot = self.snapshot().await;
        if let Ok(json) = serde_json::to_string(&snapshot) {
            let _ = self.events.send(json);
        }
    }

    /// Start reporting on a worktree cut, and clear whatever the last one said.
    ///
    /// The three create helpers live here rather than beside the scripts they
    /// report on, because `state` may not import `spawn` or `worktree` — the same
    /// reason [`crate::model::CreateRun`] is a `model` shape.
    pub async fn create_begin(self: &Arc<Self>, name: &str) {
        self.inner.write().await.create_run = Some(crate::model::CreateRun {
            name: name.to_string(),
            running: true,
            ..Default::default()
        });
        self.notify().await;
    }

    /// Name the script that is running now.
    pub async fn create_step(self: &Arc<Self>, step: &str) {
        {
            let mut inner = self.inner.write().await;
            let Some(run) = inner.create_run.as_mut() else {
                return;
            };
            run.step = step.to_string();
        }
        self.notify().await;
    }

    /// Append what a script just said.
    ///
    /// A batch rather than a line, because the caller coalesces: a chatty script
    /// must not push a snapshot per line down every socket.
    pub async fn create_lines(self: &Arc<Self>, lines: Vec<String>) {
        {
            let mut inner = self.inner.write().await;
            let Some(run) = inner.create_run.as_mut() else {
                return;
            };
            for line in lines {
                run.push(line);
            }
        }
        self.notify().await;
    }

    /// Note a step that did not succeed.
    ///
    /// Only ever set, never cleared: a later step succeeding does not undo an
    /// earlier one having failed, and this note is what explains a tree that came
    /// up half-configured. A worktree hook is never fatal, so the cut carries on
    /// past it and this is a mark on the output rather than a terminal state.
    pub async fn create_failed(self: &Arc<Self>, why: String) {
        {
            let mut inner = self.inner.write().await;
            let Some(run) = inner.create_run.as_mut() else {
                return;
            };
            if run.failed.is_none() {
                run.failed = Some(why);
            }
        }
        self.notify().await;
    }

    /// The scripts are done.
    pub async fn create_end(self: &Arc<Self>) {
        {
            let mut inner = self.inner.write().await;
            let Some(run) = inner.create_run.as_mut() else {
                return;
            };
            run.running = false;
        }
        self.notify().await;
    }

    /// The **worktree** holding this branch, if one does.
    ///
    /// Main is never the answer, even when it has this ref checked out: a fix or a
    /// review run sent there would rebase and force-push the one tree every
    /// worktree is cut from.
    ///
    /// A read of the head poller's cache, so up to two seconds stale. Fine for a
    /// worktree, which nothing is cut from; main is git's to answer, and
    /// [`crate::locate::locate_pr`] is where the two are put in order.
    pub async fn worktree_holding(&self, head_ref: &str) -> Option<String> {
        self.inner
            .read()
            .await
            .workspace_on(head_ref, |w| !w.is_main())
    }

    /// Whether PR `pr`'s head may be taken for the local branch of its name, read
    /// fresh, and the remote to fetch it from when it is not local.
    ///
    /// **Every PR flow asks this before it maps the PR onto a tree**, because the
    /// three lookups that do the mapping — [`Self::worktree_holding`],
    /// [`Inner::workspace_on`], `git::locate_branch` — know a branch only by name,
    /// and a PR's head is a repository *and* a name. `git::HeadCheck` has the rule.
    /// A PR not in the poll answers "nothing known", which is the old behaviour.
    pub async fn pr_head(&self, pr: u64) -> PrHead {
        let head = {
            let inner = self.inner.read().await;
            inner
                .pr(pr)
                .map(|p| (p.head_ref.clone(), p.head_repo.clone()))
        };
        let Some((head_ref, head_repo)) = head else {
            return PrHead::default();
        };
        let main = self.cfg.main_checkout.clone();
        let check = crate::proc::run_blocking("reading whose branch a PR's head is", move || {
            crate::git::HeadCheck::read(&main)
        })
        .await
        .unwrap_or_default();
        let head = PrHead {
            refusal: check.refusal(&head_ref, head_repo.as_deref()),
            from: check.fetch_from(head_repo.as_deref()),
        };
        let mut inner = self.inner.write().await;
        match &head.refusal {
            Some(why) => inner.foreign_heads.insert(pr, why.clone()),
            None => inner.foreign_heads.remove(&pr),
        };
        head
    }

    /// Push a worktree's branch, with a lease, against the configured base.
    ///
    /// The base read is a git run too, so it rides the same blocking hop as the
    /// push. Here rather than in `api` because a question about the daemon's own
    /// state is not an HTTP question: the review's posting flow pushes, and it
    /// should not have to call a handler module to do it.
    pub async fn push_branch(
        &self,
        path: std::path::PathBuf,
        branch: String,
    ) -> anyhow::Result<()> {
        let (main, upstream, protect) = (
            self.cfg.main_checkout.clone(),
            self.cfg.base(),
            self.cfg.protect_base,
        );
        crate::proc::run_blocking("the push", move || {
            // Off, the base is no different from any branch, as in the hook.
            let base = protect.then(|| upstream.checkout_branch(&main)).flatten();
            crate::git::push_with_lease(&path, &branch, base.as_deref())
        })
        .await?
    }

    /// The resume set as it stands: one record per session, live state included.
    ///
    /// Shutdown takes this **before** it kills anything, because `was_live` is read
    /// off session state and the exit watchers are about to rewrite it. Writing this
    /// verbatim afterwards is what lets the kills be awaited at all — see
    /// `orchd_serve::Server::shutdown`, which is the only caller.
    pub async fn session_records(&self) -> Vec<crate::store::SessionRecord> {
        self.records_of(&*self.inner.read().await)
    }

    /// Every session as a record, with auto-resume's intent folded in.
    ///
    /// **Both writers go through here, and that is the point** (#33). Shutdown is
    /// one of them; the ordinary `persist` is the other, and it runs on a timer
    /// while auto-resume is still staggering its spawns. Overriding in shutdown
    /// alone would have fixed the graceful quit and left the same loss behind a
    /// crash or a hard kill — the file on disk would already say `was_live: false`
    /// for everything not yet reached.
    fn records_of(&self, inner: &Inner) -> Vec<crate::store::SessionRecord> {
        /* Poisoned only if a holder panicked while editing the set, and the set is
        two operations long — but a lost resume is the whole of #33, so read through
        the poison rather than lose it here. */
        let pending = self
            .pending_resume
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        inner
            .sessions
            .values()
            .map(|s| {
                let mut r = crate::store::SessionRecord::of(s);
                // Auto-resume means to bring this one back and has not got to it
                // yet, so the record has to say so — see `pending_resume`.
                if pending.contains(&s.id) {
                    r.was_live = true;
                }
                r
            })
            .collect()
    }

    /// Write the records now if the *set* of sessions changed, and soon otherwise.
    ///
    /// The split is a contract, not an optimisation. A record has to exist as soon
    /// as its session does: the e2e agent waits to see itself in `sessions.json`
    /// before it speaks, `restore` reads that file and nothing else, and a session
    /// created a moment before a crash is one the rail cannot offer to resume.
    /// Everything *else* a notify carries — a state flip, a title, a waiting clock
    /// — is a field on a record that already exists, and those are what arrive
    /// dozens at a time.
    async fn persist_when_due(self: &Arc<Self>) {
        let now = self.session_ids().await;
        if now != self.persisted_ids.load(std::sync::atomic::Ordering::SeqCst) {
            self.persist().await;
        } else {
            self.persist_soon();
        }
    }

    /// A cheap stand-in for "which sessions exist" — see [`mixed_session_ids`].
    async fn session_ids(&self) -> u64 {
        mixed_session_ids(&*self.inner.read().await)
    }

    /// Ask for the records to be written, soon and at most once a second.
    ///
    /// `notify` used to write them **before every snapshot**, and it is called from
    /// about seventy places — every hook, every poll, every state change. That is a
    /// serialise-and-write of the whole record set, tens of kilobytes once a
    /// session board has been used for a while, on a tokio worker thread, several
    /// times a second while an agent is working.
    ///
    /// Coalescing costs at most a second of records on a *crash*. An ordinary exit
    /// loses nothing: `orchd_serve::Server::shutdown` captures the resume set before it
    /// kills anything and writes that set itself, and the flush below always writes
    /// the state as it is when it runs rather than as it was when the flag was set.
    /// What it buys is that a burst of hooks writes the file once rather than ten
    /// times.
    fn persist_soon(self: &Arc<Self>) {
        // Already scheduled: the pending flush will see whatever this change made,
        // because it reads the records when it runs.
        if self
            .persist_pending
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let app = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PERSIST_COALESCE).await;
            app.persist_pending
                .store(false, std::sync::atomic::Ordering::SeqCst);
            app.persist().await;
        });
    }

    /// Session records are written on every state change, so a daemon that dies
    /// unexpectedly still leaves something to resume from (§2).
    async fn persist(&self) {
        /* **Nothing writes live state once shutdown has begun.** Shutdown captures
        the resume set before it kills anything and writes that set verbatim at
        the end, precisely because every dying pty wakes an exit watcher that
        flips `was_live` to false. Without this the last word on disk belonged to
        whichever watcher ran last, or to a `persist_soon` timer landing a second
        later — and auto-resume then restored nothing.

        This is what replaced "no await point after the kills". The rule cost
        shutdown its `SIGHUP` → `SIGKILL` escalation, since escalating means
        waiting. */
        if self.shutting_down.load(std::sync::atomic::Ordering::SeqCst)
            || !self.restored.load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        // Serialised, so two overlapping flushes cannot write this file at once.
        let _writing = self.persist_writing.lock().await;
        /* **The records and the id hash come from one guard.** They used to be two
        reads: the records under one, then `session_ids()` taking the lock again.
        A session inserted between them was hashed into "already persisted"
        while its record was not in the set just written — so
        `persist_when_due` read the set as unchanged and *deferred* the write
        that had to be immediate, which is the one thing its contract promises
        (the e2e agent waits to see itself in `sessions.json` before speaking). */
        let (records, ids) = {
            let inner = self.inner.read().await;
            // `records_of`, not `SessionRecord::of`: a session auto-resume has not
            // reached yet is not live and still has to be written as one (#33).
            (self.records_of(&inner), mixed_session_ids(&inner))
        };
        // Recorded before the write, and unconditionally: a failed write is not a
        // reason to keep answering "the set changed" and writing on every notify.
        self.persisted_ids
            .store(ids, std::sync::atomic::Ordering::SeqCst);
        // Off the runtime: this serialises every record and writes the file, tens
        // of kilobytes once a board has been used, and it hangs off `notify` —
        // about seventy call sites.
        let written = crate::proc::run_blocking("persisting the session records", move || {
            crate::store::save(&records)
        })
        .await;
        match written {
            Ok(Err(e)) | Err(e) => {
                tracing::warn!("could not persist session records: {e:#}")
            }
            Ok(Ok(())) => {}
        }
    }

    /// Bring back the previous run's sessions, all of them archived.
    ///
    /// Records for workspaces that no longer exist are kept: the transcript is
    /// still readable, and dropping them would silently lose the history.
    pub async fn restore_sessions(&self, records: Vec<crate::store::SessionRecord>) {
        let mut inner = self.inner.write().await;
        for r in records {
            let was_live = r.was_live;
            let mut s = r.restore();
            // A worktree session's recorded path can name a directory Claude Code
            // never wrote to; see `store::find_transcript`. Corrected once here,
            // where it costs one scan per restored session at startup, rather than
            // per snapshot forever.
            crate::store::pin_transcript(s.id, &s.cwd, &mut s.transcript_path);
            // Records written before the daemon read titles have none, and an
            // archived session never fires the `Stop` that would fill one in. Its
            // conversation is over, so the answer cannot change: read it once here
            // rather than leaving the whole archive reading as worktree names.
            if s.title.is_none() {
                s.title = crate::store::ai_title(s.id, &s.cwd, s.transcript_path.as_deref());
            }
            // A turn that ended with a watcher still out owes you the rest as much
            // as one cut off mid-way, and the restart just killed the watcher. Only
            // for a session that was live, since nothing else had one running.
            //
            // **The `Stop` hook's answer first, the transcript only without one.**
            // The hook names monitors and scheduled wake-ups, which the transcript
            // patterns miss, and leaves out a plain shell, which they count, so a
            // dev server alone no longer reads as work cut off. A record from before
            // the field has `None`, and the transcript still answers for it, but
            // only behind the cheap checks: it is a tail read per session, under
            // this write lock, and most records are archived ones.
            let left = || match s.left_work {
                Some(left) => left,
                None => {
                    crate::store::left_background_work(s.id, &s.cwd, s.transcript_path.as_deref())
                }
            };
            if was_live && s.had_a_turn && !s.interrupted && left() {
                s.interrupted = true;
            }
            inner.sessions.entry(s.id).or_insert(s);
        }
        self.restored
            .store(true, std::sync::atomic::Ordering::SeqCst);
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

        /* Asked once per workspace: the predicate walks every session, and it
        was asked twice per workspace on every push. */
        let active_ids: std::collections::HashSet<&str> = inner
            .workspaces
            .keys()
            .filter(|id| inner.is_active_workspace(id))
            .map(String::as_str)
            .collect();
        let active = |id: &str| active_ids.contains(id);
        let live_ids: std::collections::HashSet<&str> = inner
            .workspaces
            .keys()
            .filter(|id| inner.has_live_session(id))
            .map(String::as_str)
            .collect();
        let mut workspaces: Vec<WorkspaceView> = inner
            .workspaces
            .values()
            .map(|w| WorkspaceView {
                id: w.id.clone(),
                path: w.path.to_string_lossy().into_owned(),
                kind: w.kind.clone(),
                is_main: w.is_main(),
                occupant: w.occupant,
                held_by: if w.is_main() {
                    self.main_holder(&inner)
                } else {
                    None
                },
                /* Sorted, because a `HashSet` hands them back in whatever order
                the last rehash left — and the page compares snapshots by their
                JSON text to decide whether to repaint. A reorder that says
                nothing new then tears down every session row and every PR row
                under the pointer. Measured: one rail rebuild in 35 idle
                seconds, this and nothing else. */
                branches: {
                    let mut b: Vec<String> = w.branches.iter().cloned().collect();
                    b.sort();
                    b
                },
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
                // A managed process that died stays in `processes` as a dead tab
                // with its own restart button, so matching on the name — not on
                // liveness — is what keeps it from being listed twice.
                stopped_processes: self
                    .cfg
                    .processes_for(&w.id)
                    .iter()
                    .filter(|spec| !w.processes.iter().any(|p| p.name == spec.name))
                    .map(|spec| spec.name.clone())
                    .collect(),
                banked: w.banked.as_ref().map(|b| BankedView {
                    files: b.files,
                    at: crate::git::wip_ref(&w.id),
                }),
                branch: w.tree.branch.clone(),
                /* **In full only where it can be moving.** The lists were 486 KB of
                a 775 KB snapshot, for 200 workspaces, and every push carried all of
                them to a page that draws one. An idle workspace's list is held
                here unchanged and handed over by `/api/workspace/:id/changed`
                when its pane asks; `changed_omitted` says to ask. */
                changed: if active(&w.id) {
                    w.tree.changed.clone()
                } else {
                    Vec::new()
                },
                changed_omitted: !active(&w.id) && !w.tree.changed.is_empty(),
                changed_total: w.tree.changed_total,
                changed_since: w.tree.base.clone(),
                behind: w.tree.divergence.0,
                ahead: w.tree.divergence.1,
                unpushed: w.tree.unpushed,
                rebasing: w.tree.rebasing,
                measured: w.tree.measured,
                // Sent for every workspace, idle or not: it is a handful of rows,
                // and the rail marks a row whose pane is not open. Only between two
                // workspaces with a live session each: an archived or sessionless
                // side is nobody's to settle, and a side that has gone is dropped
                // here rather than at teardown. And only a conflict: a file both
                // changed that git merges cleanly is nothing anybody has to do.
                overlaps: inner
                    .overlaps
                    .iter()
                    .filter(|p| p.found.conflict && !inner.stacked(p))
                    .filter(|p| p.involves(&w.id) && live_ids.contains(w.id.as_str()))
                    .filter(|p| live_ids.contains(p.other(&w.id)))
                    .map(|p| OverlapView {
                        path: p.found.path.clone(),
                        with: p.other(&w.id).to_string(),
                        conflict: p.found.conflict,
                        plan: inner
                            .overlap_plans
                            .get(&(p.a.clone(), p.b.clone()))
                            .cloned(),
                    })
                    .collect(),
                // The upper side of a stack: what it is built on, and how far the
                // lower branch has moved past it. Only between live sessions, as
                // above.
                stacked_on: inner
                    .stacked_on(&w.id)
                    .filter(|(lower, _)| {
                        live_ids.contains(w.id.as_str()) && live_ids.contains(lower.as_str())
                    })
                    .map(|(lower, s)| StackView {
                        conflicts: inner
                            .overlaps
                            .iter()
                            .filter(|p| {
                                p.found.conflict && p.involves(&w.id) && p.other(&w.id) == lower
                            })
                            .count() as u32,
                        on: lower,
                        behind: s.behind,
                    }),
            })
            .collect();
        workspaces.sort_by(|a, b| b.is_main.cmp(&a.is_main).then(a.id.cmp(&b.id)));

        // A PR belongs to the tree that has its head checked out, and to the
        // session whose conversation is about it — two lookups, because after a
        // tree is deleted under a live session the second can answer without the
        // first.
        let mut prs: Vec<PrView> = inner
            .prs
            .iter()
            .map(|p| {
                // A head that is some other repository's branch of the same name
                // belongs to no tree and no conversation here.
                if inner.foreign_heads.contains_key(&p.number) {
                    return PrView {
                        pr: p.clone(),
                        rank: p.rank(),
                        workspace: None,
                        session: None,
                        press: inner.press_for(p, None),
                    };
                }
                let session = inner.session_on(&p.head_ref);
                let workspace = inner.workspace_on(&p.head_ref, |_| true).or_else(|| {
                    session
                        .and_then(|id| inner.sessions.get(&id))
                        .map(|s| s.workspace.clone())
                });
                PrView {
                    pr: p.clone(),
                    rank: p.rank(),
                    workspace,
                    session,
                    press: inner.press_for(p, session),
                }
            })
            .collect();
        prs.sort_by(|a, b| a.rank.cmp(&b.rank).then(b.pr.number.cmp(&a.pr.number)));

        // The same branch-set lookup the open PRs get, for the same reason: the
        // head ref is the only thing tying a PR to a workspace.
        let prs_ended: Vec<EndedPrView> = inner
            .prs_ended
            .iter()
            .map(|p| EndedPrView {
                pr: p.clone(),
                workspace: inner
                    .workspaces
                    .values()
                    .find(|w| w.branches.contains(&p.head_ref))
                    .map(|w| w.id.clone()),
            })
            .collect();

        // Already newest-first and already capped, by the poller that filled it.
        let external = inner
            .external
            .iter()
            .map(|c| ExternalView {
                id: c.id,
                workspace: c.workspace.clone(),
                title: c.title.clone(),
                last_used_ms: now
                    .duration_since(c.last_used)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                may_be_live: crate::store::recently_active(c.last_used),
                agent: c.agent,
            })
            .collect();

        Snapshot {
            tracker_server: self.cfg.tracker.as_ref().map(|t| t.mcp_server.clone()),
            story_prefix: self.story_link().map(|s| s.prefix.trim().to_string()),
            spare: inner.spare.ids.clone(),
            workspaces,
            sessions,
            external,
            prs,
            prs_ended,
            pr_error: inner.pr_error.clone(),
            agent_error: inner.agent_error.clone(),
            agents: inner.agents.clone(),
            default_agent: self.default_agent(&inner),
            forge: self.cfg.forge,
            machine: inner.machine.clone(),
            pr_age_ms: inner
                .pr_fetched
                .and_then(|t| now.duration_since(t).ok().map(|d| d.as_millis() as u64)),
            pr_poll: inner.pr_poll,
            pr_polling: inner.pr_polling,
            token_source: inner.token_source,
            reviews: inner.reviews.clone(),
            reviews_age_ms: inner
                .reviews_fetched
                .and_then(|t| now.duration_since(t).ok().map(|d| d.as_millis() as u64)),
            reviews_poll: inner.reviews_poll,
            reviews_polling: inner.reviews_polling,
            automation: inner
                .automation
                .by_pr
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect(),
            repos: self.repos.clone(),
            several_in_main: self.settings().allow_several_in_main,
            upstream_ref: self.cfg.upstream_ref.clone(),
            create_run: inner.create_run.clone(),
            update: inner.update.clone(),
            self_upgrade_run: inner.self_upgrade_run.clone(),
            agent_update: inner.agent_update.clone(),
            upgrade_run: inner.upgrade_run.clone(),
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
        // The map only ever grew. Each entry is small, but an edit from days ago is
        // no longer news to anybody: every session that could have read the old
        // file has either been told or has ended. A day is generous; the point is
        // a bound, not a tuning.
        inner
            .human_edits
            .retain(|_, e| e.at.elapsed().map_or(true, |d| d < HUMAN_EDIT_TTL));
        inner.human_edits.insert(
            real,
            HumanEdit {
                at: SystemTime::now(),
                told: Default::default(),
            },
        );
    }

    /// What this session's agent should hear about files another workspace also
    /// changed, or `None` when there is nothing it has not heard.
    ///
    /// Read at the next prompt and handed over as `additionalContext`, which both
    /// agents put in front of the model (measured on Claude Code 2.1.318 and Codex
    /// 0.166.0). **A note, never a turn**: typing it into the pty would start work
    /// nobody asked for, which is the automatic hand-on the review flow removed.
    pub async fn overlap_note(&self, session: SessionId) -> Option<String> {
        let mut inner = self.inner.write().await;
        let ws = inner.sessions.get(&session)?.workspace.clone();
        // The stacks first, on this prompt like the conflicts and never a turn of
        // their own: a note typed in as a turn is what flooded six sessions.
        let stacks = inner.stack_notes(&ws, false).join("\n\n");
        let conflicts = Self::conflict_note(&mut inner, session, &ws);
        match (stacks.is_empty(), conflicts) {
            (true, None) => None,
            (true, Some(c)) => Some(c),
            (false, None) => Some(stacks),
            (false, Some(c)) => Some(format!("{stacks}\n\n{c}")),
        }
    }

    /// The conflict half of [`Self::overlap_note`]: the brief for the first side
    /// to reach a prompt, the follower's note for the other.
    fn conflict_note(inner: &mut Inner, session: SessionId, ws: &str) -> Option<String> {
        let ws = ws.to_string();
        let new: Vec<(String, String, bool)> = inner
            .overlaps
            .iter()
            .filter(|p| {
                // Only a conflict is worth the agent's attention, as on the board.
                p.found.conflict
                    && !inner.stacked(p)
                    && p.involves(&ws)
                    && inner.workspaces.contains_key(p.other(&ws))
                    && inner.has_live_session(p.other(&ws))
            })
            .map(|p| (p.found.path.clone(), p.other(&ws).to_string(), true))
            .filter(|k| {
                !inner
                    .sessions
                    .get(&session)
                    .is_some_and(|s| s.told_overlaps.contains(k))
            })
            .collect();
        if new.is_empty() {
            return None;
        }
        inner
            .sessions
            .get_mut(&session)?
            .told_overlaps
            .extend(new.iter().cloned());
        /* **Somebody is already settling it: follow.** Two agents each told to
        lead would both open the negotiation and defer to each other politely; the
        first to reach a prompt leads, and this is the other one. */
        let leader = inner
            .sessions
            .values()
            .find(|x| x.id != session && x.state.is_live() && x.handling.contains(&session))
            .map(|x| (x.id, x.label().unwrap_or(&x.workspace).to_string()));
        if let Some((id, name)) = leader {
            let files: Vec<String> = new.iter().map(|(p, _, _)| format!("- {p}")).collect();
            return Some(format!(
                "orchd: git cannot merge files you changed with session `{name}` (id {id}):\n{}\n\
                 That session is settling it with you and leads. Answer its `orch tell` \
                 messages with `orch tell {id} --text \"...\"`, and do not start a plan of your \
                 own. Until it is settled, leave these files alone.",
                files.join("\n")
            ));
        }
        let partner_ws: Vec<String> = new
            .iter()
            .map(|(_, w, _)| w.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let (brief, partners) = inner.solve_brief(&ws, &partner_ws);
        if partners.is_empty() {
            return None;
        }
        for &partner in &partners {
            if let Some(x) = inner.sessions.get_mut(&partner) {
                x.told.remove(&session);
                if !x.handling.contains(&session) {
                    x.handling.push(session);
                }
            }
        }
        if let Some(x) = inner.sessions.get_mut(&session) {
            for p in &partners {
                x.told.remove(p);
                if !x.handling.contains(p) {
                    x.handling.push(*p);
                }
            }
        }
        Some(brief)
    }

    /// Queue `text` from session `from` for session `to`, and type it in now if
    /// `to` is idle. Answers whether it was delivered rather than queued.
    ///
    /// **Only between two sessions whose workspaces share a file.** This is the
    /// channel for settling an overlap, not a way for one agent to steer any
    /// other; a pair the daemon has not seen collide is refused.
    ///
    /// **Capped per direction at [`TELL_CAP`].** Two agents in auto mode can keep
    /// each other busy for as long as there is budget, and past a few messages the
    /// question is one for the person anyway, so the refusal says to ask them.
    pub async fn message_session(
        &self,
        from: SessionId,
        to: SessionId,
        text: &str,
    ) -> anyhow::Result<bool> {
        {
            let mut inner = self.inner.write().await;
            if from == to {
                anyhow::bail!("that is you");
            }
            let me = inner
                .sessions
                .get(&from)
                .ok_or_else(|| no_such_session(from))?;
            let them = inner.sessions.get(&to).ok_or_else(|| no_such_session(to))?;
            if !them.state.is_live() {
                anyhow::bail!("{} is not running", crate::model::short_id(&to));
            }
            let (mine, theirs) = (me.workspace.clone(), them.workspace.clone());
            if !inner
                .overlaps
                .iter()
                .any(|p| p.involves(&mine) && p.other(&mine) == theirs)
            {
                anyhow::bail!(
                    "{} has changed no file you changed; `orch tell` is for settling an overlap",
                    crate::model::short_id(&to)
                );
            }
            let sent = me.told.get(&to).copied().unwrap_or(0);
            if sent >= TELL_CAP {
                anyhow::bail!(
                    "you have sent {} {TELL_CAP} messages already; ask the user with `orch ask` instead",
                    crate::model::short_id(&to)
                );
            }
            let who = me.label().unwrap_or(&me.workspace).to_string();
            let branch = inner
                .workspaces
                .get(&mine)
                .and_then(|w| w.tree.branch.clone())
                .map(|b| format!(", branch `{b}`"))
                .unwrap_or_default();
            let left = TELL_CAP - sent - 1;
            let note = format!(
                "orchd: a message from session `{who}` (id {from}, workspace `{mine}`{branch}), \
                 which changes files you changed too:\n\n{}\n\n\
                 Reply with `orch tell {from} --text \"...\"`. You can send it {TELL_CAP} \
                 messages in all; past that, ask the user with `orch ask`.",
                text.trim()
            );
            if let Some(s) = inner.sessions.get_mut(&from) {
                *s.told.entry(to).or_insert(0) += 1;
            }
            if let Some(s) = inner.sessions.get_mut(&to) {
                s.inbox.push(note);
            }
            tracing::info!(%from, %to, left, "queued a message between overlapping sessions");
        }
        Ok(self.deliver_inbox(to).await)
    }

    /// The card's "Ask it to restack": tell `session` about the branch it is
    /// stacked on again, now, even if it was told about this head already.
    pub async fn ask_restack(&self, session: SessionId) -> anyhow::Result<bool> {
        {
            let mut inner = self.inner.write().await;
            let ws = inner
                .sessions
                .get(&session)
                .map(|s| s.workspace.clone())
                .ok_or_else(|| no_such_session(session))?;
            match inner.stacked_on(&ws) {
                Some((_, s)) if s.behind > 0 => {}
                Some(_) => anyhow::bail!("it already has every commit of the branch under it"),
                None => anyhow::bail!("this session's branch is not stacked on another"),
            }
            let notes = inner.stack_notes(&ws, true);
            let Some(x) = inner.sessions.get_mut(&session) else {
                anyhow::bail!("that session is gone");
            };
            // Only its own half: the button is about this row.
            if let Some(note) = notes
                .into_iter()
                .find(|n| n.starts_with("orchd: your branch is stacked"))
            {
                x.inbox.push(note);
            }
        }
        Ok(self.deliver_inbox(session).await)
    }

    /// Record the user's answer to an overlap's `orch ask` as the plan for the
    /// pair `session` was handling. Anything else is not a plan and is ignored.
    pub(crate) fn record_overlap_plan(
        inner: &mut Inner,
        session: SessionId,
        value: &str,
        label: &str,
    ) {
        if !value.starts_with("overlap-") {
            return;
        }
        let Some((mine, others)) = inner
            .sessions
            .get(&session)
            .map(|s| (s.workspace.clone(), s.handling.clone()))
        else {
            return;
        };
        for other in others {
            let Some(theirs) = inner.sessions.get(&other).map(|s| s.workspace.clone()) else {
                continue;
            };
            let key = if mine < theirs {
                (mine.clone(), theirs)
            } else {
                (theirs, mine.clone())
            };
            inner.overlap_plans.insert(key, label.to_string());
        }
    }

    /// Type everything waiting in `id`'s inbox in now, whatever the state says:
    /// the overlap card's "Send now", pressed by somebody looking at the pane.
    ///
    /// The state check in [`Self::type_user_turn`] is there for a keystroke landing
    /// on a question or a permission prompt. The state is the daemon's guess, and
    /// a turn that ended on an escape leaves it guessing wrong with nothing to
    /// correct it; the person can see the prompt, so the person decides.
    pub async fn deliver_inbox_now(&self, id: SessionId) -> anyhow::Result<()> {
        let (pty, waiting) = {
            let mut inner = self.inner.write().await;
            let s = inner
                .sessions
                .get_mut(&id)
                .ok_or_else(|| no_such_session(id))?;
            let Some(pty) = s.pty.clone().filter(|p| p.is_alive()) else {
                anyhow::bail!("that session is not running");
            };
            if s.inbox.is_empty() {
                anyhow::bail!("no message is waiting");
            }
            (pty, std::mem::take(&mut s.inbox))
        };
        pty.type_and_send(
            waiting.join("\n\n").as_bytes(),
            std::time::Duration::from_millis(500),
        );
        Ok(())
    }

    /// Type everything waiting in `id`'s inbox in as one turn, if `id` can take a
    /// turn now. Called when a message arrives and when a turn ends (`Stop`).
    ///
    /// A refusal from [`Self::type_user_turn`] (mid-turn, a permission prompt, an
    /// open question) puts the messages back for the next `Stop`.
    pub async fn deliver_inbox(&self, id: SessionId) -> bool {
        let waiting = {
            let mut inner = self.inner.write().await;
            match inner.sessions.get_mut(&id) {
                Some(s) if !s.inbox.is_empty() => std::mem::take(&mut s.inbox),
                _ => return false,
            }
        };
        match self.type_user_turn(id, &waiting.join("\n\n")).await {
            Ok(()) => true,
            Err(_) => {
                let mut inner = self.inner.write().await;
                if let Some(s) = inner.sessions.get_mut(&id) {
                    let newer = std::mem::replace(&mut s.inbox, waiting);
                    s.inbox.extend(newer);
                }
                false
            }
        }
    }

    /// The one-off notice for a session the daemon moved, taken so it is said once.
    ///
    /// Lives beside [`Self::claim_stale_warning`] because it is the same idea and the
    /// same delivery: the daemon has something the agent must know, and a refused
    /// tool call is the only way to say it.
    pub async fn take_arrival_notice(&self, session: SessionId) -> Option<String> {
        let mut inner = self.inner.write().await;
        inner.sessions.get_mut(&session)?.arrival_notice.take()
    }

    /// Whether this session should be interrupted before writing `path`.
    ///
    /// Returns the message once per session per edit; afterwards the write is
    /// allowed, so the agent's retry after re-reading succeeds.
    ///
    /// **Only a session that could have read the old file.** The message says
    /// "after you last read it", and for a session started after the edit that is
    /// false: whatever it read was already the new file. It used to be refused all
    /// the same, on its first write to anything you had touched days before, with a
    /// message about a rewrite it never saw. So an edit older than the session is
    /// no edit to it.
    pub async fn claim_stale_warning(&self, session: SessionId, path: &Path) -> Option<String> {
        let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut inner = self.inner.write().await;
        let started = inner.sessions.get(&session).map(|s| s.created_at);
        let edit = inner.human_edits.get_mut(&real)?;
        if started.is_some_and(|since| edit.at < since) {
            return None;
        }
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

    /// Who holds main, for the page: the snapshot's `held_by`.
    ///
    /// The live claimant when there is one, else [`AppState::main_in_use`], which a
    /// move of main's branch asks directly. Never the bare `occupant` field:
    /// `release_main` can lag a session's death, and `switch_main_to_pr` once
    /// trusted the field and gave the opposite answer to `claim_main` for the same
    /// question.
    pub fn main_holder(&self, inner: &Inner) -> Option<SessionId> {
        let recorded = inner
            .workspaces
            .get(MAIN)
            .and_then(|w| w.occupant)
            .filter(|id| inner.sessions.get(id).is_some_and(occupies));
        // The recorded claimant is named first, because it is the one the page has
        // drawn in main; anybody else in main — with `allow_several_in_main` a
        // live session the single field does not name, one coming back, one still
        // starting — is `main_in_use`'s answer.
        recorded.or_else(|| self.main_in_use(inner))
    }

    /// Auto-resume's intent for this session is over: it has been spawned or has
    /// exited, so its own state says whether it is live. See
    /// [`AppState::pending_resume`].
    pub fn resume_settled(&self, id: SessionId) {
        self.pending_resume
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    /// Reserve main for `park_main`: `None` when anybody is in it, else a guard that
    /// keeps every `claim_main` out until it drops.
    ///
    /// Asked under the state lock, so the check and the reservation are one step
    /// against `claim_main`, which reads the flag under the same lock. A claim
    /// whose session is not inserted yet counts: the claim is written before the
    /// spawn inserts the record.
    pub async fn reserve_main_for_park(&self) -> Option<ParkReservation<'_>> {
        let inner = self.inner.write().await;
        if self.main_in_use(&inner).is_some() {
            return None;
        }
        self.parking_main
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Some(ParkReservation { app: self })
    }

    /// Who is in main, or about to be: the one question every move of main's branch
    /// asks before it moves anything.
    ///
    /// [`AppState::occupancy`] of main, **and a session still starting there**
    /// ([`StartingInMain`]). A spawn into main claims first and is live only once
    /// its environment is built — `mise env`, a few hundred milliseconds — and
    /// only the park counted that window. A press of `handle` or "open in main"
    /// inside it moved main's branch under an agent that was starting, which is the
    /// bug the park's reservation was written for, still open on the other two.
    pub fn main_in_use(&self, inner: &Inner) -> Option<SessionId> {
        self.main_in_use_except(inner, None)
    }

    /// [`AppState::main_in_use`], not counting `except`: what `claim_main` asks,
    /// since a session never holds main against its own claim or resume.
    fn main_in_use_except(&self, inner: &Inner, except: Option<SessionId>) -> Option<SessionId> {
        let occupancy = self.occupancy_in(inner, MAIN);
        let other = |id: &SessionId| Some(*id) != except;
        occupancy
            .live
            .iter()
            .copied()
            .find(other)
            .or(occupancy.waiting.filter(other))
            .or_else(|| self.starting_main(except))
    }

    /// [`AppState::main_in_use`], for a caller not already holding the state.
    pub async fn main_in_use_now(&self) -> Option<SessionId> {
        let inner = self.inner.read().await;
        self.main_in_use(&inner)
    }

    /// Take the lock for moving main's branch, or `None` while another move holds
    /// it. Refused rather than queued at every caller: a second move would run on
    /// the strength of what was read before the first.
    pub fn move_main(&self) -> Option<MainMove<'_>> {
        self.swapping
            .try_lock()
            .ok()
            .map(|held| MainMove { _held: held })
    }

    /// [`AppState::move_main`] for a request: the one refusal every move of main
    /// says when another holds the lock.
    ///
    /// It was four sentences, each written beside its own `try_lock`, and the e2e
    /// flows that wait out a move match on this wording ("a swap is already
    /// running"), so it has one home. The watchers that move main on their own —
    /// a park, a restart — skip rather than refuse, and call `move_main`.
    ///
    /// A [`Refusal::Busy`](crate::model::Refusal), so the route answers 409: the
    /// same request will go through once the move in progress lands.
    pub fn move_main_or_refuse(&self) -> anyhow::Result<MainMove<'_>> {
        self.move_main().ok_or_else(|| {
            crate::model::Refusal::Busy(
                "a swap is already running; it moves main's checkout, so give it a moment \
                 and look at the rail before asking again"
                    .to_string(),
            )
            .into()
        })
    }

    /// Who is in `workspace`, by the daemon's one rule.
    ///
    /// **Every question about occupancy asks this**: refusing a second session,
    /// handing a PR verb to the session already there, what a swap or a move out
    /// of main carries, whether main may be parked or claimed, teardown's "nobody
    /// in it". There were eight answers, and they disagreed: some checked the
    /// process and some did not, and only main's counted a session auto-resume had
    /// not reached — which is how a worktree took a second agent in the resume
    /// stagger, and `orch new` a second one beside its caller.
    pub fn occupancy_in(&self, inner: &Inner, workspace: &str) -> Occupancy {
        let mut live: Vec<&Session> = inner
            .sessions
            .values()
            .filter(|s| s.workspace == workspace && occupies(s))
            .collect();
        // Oldest first, and deterministic, so a refusal names the same session
        // twice running.
        live.sort_by_key(|s| (s.created_at, s.id));
        Occupancy {
            live: live.into_iter().map(|s| s.id).collect(),
            waiting: self.waiting(inner, workspace),
        }
    }

    /// [`AppState::occupancy_in`], for a caller not already holding the state.
    pub async fn occupancy(&self, workspace: &str) -> Occupancy {
        let inner = self.inner.read().await;
        self.occupancy_in(&inner, workspace)
    }

    /// The session in `workspace` that auto-resume has not reached yet.
    ///
    /// See [`AppState::auto_resuming`]: it holds its tree until auto-resume is
    /// done, or a resume that failed would hold it for good.
    fn waiting(&self, inner: &Inner, workspace: &str) -> Option<SessionId> {
        if !self.auto_resuming.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let pending = self
            .pending_resume
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        inner
            .sessions
            .values()
            .filter(|s| s.workspace == workspace && !s.state.is_live() && pending.contains(&s.id))
            .min_by_key(|s| (s.created_at, s.id))
            .map(|s| s.id)
    }

    /// Take a named lock, or `None` when somebody already holds it.
    ///
    /// The wording of the refusal belongs to the caller, so this answers with an
    /// `Option` rather than an error. Hold the [`Claim`] for as long as the thing
    /// it protects is in flight: it is given back on drop, which is what makes it
    /// safe across the `?`s in between.
    ///
    /// `async` for its callers' sake only; nothing here awaits. The set is its own
    /// `std` mutex (see [`AppState::locks_held`]), so taking and releasing are both
    /// immediate and a claim taken right after a release sees the release.
    pub fn try_claim(self: &Arc<Self>, lock: impl Into<String>) -> Option<Claim> {
        let lock = lock.into();
        let mut held = self.locks_held.lock().ok()?;
        if !held.insert(lock.clone()) {
            return None;
        }
        Some(Claim {
            app: self.clone(),
            lock,
        })
    }

    /// Main is exclusive: one Claude session at a time (§2). There is no queue —
    /// the UI disables "new session in main" and shows which session holds it.
    ///
    /// `allow_several_in_main` turns the refusal off, and only the refusal. The
    /// claim is still recorded, because everything else keyed on it — the swap's
    /// hand-back, `reclaim_main`, the rail's label — is about the tree rather than
    /// about exclusivity.
    pub async fn claim_main(&self, session: SessionId) -> Result<StartingInMain<'_>> {
        let several = self.settings().allow_several_in_main;
        /* **A park in progress is waited out, not refused.** Closing the last
        session in main and opening another straight away is the ordinary way to
        start over, and the close is what starts the park — so refusing made that
        press fail whenever git was slower than the person, which on a CI runner was
        every time (e2e flow 47). The park is a few git commands; the wait is
        bounded, and only a park that hangs turns into a refusal.
        Registered before the flag is read, so a park that ends in between still
        wakes this, and read under the state lock, which is where the park sets it. */
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut inner = loop {
            let done = self.park_done.notified();
            tokio::pin!(done);
            done.as_mut().enable();
            let inner = self.inner.write().await;
            if !self.parking_main.load(std::sync::atomic::Ordering::SeqCst) {
                break inner;
            }
            drop(inner);
            if tokio::time::timeout_at(deadline, done).await.is_err() {
                bail!("main is still going back to its base; try again when it has");
            }
        };
        /* **The recorded claim, and then anybody live in main**, the fallback
        `main_holder` has too. The setting applies on save now, so it can be
        turned off while two sessions share main, and then the claim names only
        the last one to take it. When that one exits the slot reads empty while
        the other is still working, and a third came in with the setting off. The
        swap is not refused by this: it moves the outgoing session out before the
        incoming one claims, and a resume claims under its own id. */
        let held = inner
            .workspaces
            .get(MAIN)
            .and_then(|w| w.occupant)
            .filter(|id| *id != session && inner.sessions.get(id).is_some_and(occupies))
            .or_else(|| self.main_in_use_except(&inner, Some(session)));
        if let Some(holder) = held {
            if holder != session && !several {
                bail!("main is occupied by session {holder}");
            }
        }
        if let Some(w) = inner.workspaces.get_mut(MAIN) {
            w.occupant = Some(session);
        }
        // Marked under the same lock as the claim, so nothing can see one without
        // the other.
        self.starting_in_main
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session);
        Ok(StartingInMain { app: self, session })
    }

    /// A session that has claimed main and is still starting, other than `except`.
    /// Lowest id first, so a refusal names the same one twice running.
    fn starting_main(&self, except: Option<SessionId>) -> Option<SessionId> {
        self.starting_in_main
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .filter(|id| Some(*id) != except)
            .min()
    }

    /// Take main's claim again, for a session that already holds it, if it has been
    /// dropped in the meantime.
    ///
    /// `claim_main` runs before anything is created, so a refusal costs no worktree
    /// and no pty. But a relocation resumes under the *same session id*, so between
    /// that claim and the record being installed the map still describes the *old*
    /// session — and its exit watcher is entitled to settle it, which calls
    /// `release_main`, which keys on the id and so hands back the claim the incoming
    /// session had already taken. Main then holds a live agent with no occupant
    /// recorded, and that is the value `switch_main_to_pr` reads to decide it may
    /// move the checkout under it. Found by driving a two-way swap end to end; it
    /// reproduced about one run in four.
    ///
    /// Only an *empty* slot is filled. A slot held by somebody else is a real
    /// conflict that `claim_main` was supposed to catch, and quietly stealing it
    /// would turn one bug into a worse one.
    pub async fn reclaim_main(&self, session: SessionId) {
        let mut inner = self.inner.write().await;
        if let Some(w) = inner.workspaces.get_mut(MAIN) {
            if w.occupant.is_none() {
                w.occupant = Some(session);
            }
        }
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
        if let Some(b) = branch.clone() {
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
                // A re-registered id starts from nothing measured, which is the
                // whole point of the tree living here: the old incarnation's
                // numbers went with it. Except the branch, which the caller just
                // read off git: the sweep that measures the rest runs off the boot
                // path, and a fix run asking who holds a branch before it would cut
                // a second tree for a branch this one has checked out.
                tree: crate::model::Tree {
                    branch,
                    ..Default::default()
                },
                // Adopted from the refs at boot, not guessed here: a tree this
                // daemon has never seen may still have work banked in it.
                banked: None,
            });
    }

    /// Install the run observer. Once per process, by the composition root.
    ///
    /// A second call is ignored rather than refused: nothing here is worth failing
    /// a start over, and the first caller is the only one that ever runs.
    pub fn observe_runs(&self, f: RunObserver) {
        let _ = self.run_observer.set(f);
    }

    /// Tell whoever started this run that it is over. Nothing to do if nobody is
    /// listening, which is every unit test.
    pub async fn run_ended(self: &Arc<Self>, exit: RunExit) {
        if let Some(f) = self.run_observer.get() {
            f(Arc::clone(self), exit).await;
        }
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
        self.inner.read().await.workspace_for_path(path)
    }

    /// Drop a branch a workspace gave away in a swap.
    ///
    /// `reconcile` only ever *adds* to a workspace's branch set. Nothing asks the
    /// set who holds a branch any more — that is [`Inner::workspace_on`], off what
    /// is checked out now — but teardown's preflight reads it for unpushed work,
    /// and a branch that moved to main with its commits is main's to lose now.
    pub async fn forget_branch(&self, workspace: &str, branch: &str) {
        let mut inner = self.inner.write().await;
        if let Some(w) = inner.workspaces.get_mut(workspace) {
            w.branches.remove(branch);
        }
    }

    /// Who is mid-turn in this workspace, named the way the rail names them.
    ///
    /// The refusal every route that writes into a tree owes: staging under a
    /// working agent, swapping the checkout beneath it or rebasing while it works
    /// all change what its next command sees. Three routes each spelled this
    /// filter out by hand, and the distinction it turns on — `is_busy`, not
    /// `is_live` — has been got wrong here before.
    ///
    /// A name rather than a bool because two of the three say who, and a refusal
    /// that names the session is the one you can act on.
    pub async fn busy_session_in(&self, workspace: &str) -> Option<String> {
        let inner = self.inner.read().await;
        inner
            .sessions
            .values()
            .find(|s| s.workspace == workspace && s.state.is_busy())
            .map(|s| {
                s.label()
                    .map(str::to_owned)
                    .unwrap_or_else(|| crate::model::short_id(&s.id))
            })
    }

    /// Record what a workspace has banked, or that it has nothing.
    ///
    /// The daemon's copy of a fact that lives in a git ref. Written by whoever
    /// moved the ref, in the same breath, so the strip in the pane and the object
    /// in the repository cannot disagree for longer than one request.
    pub async fn set_banked(&self, workspace: &str, banked: Option<crate::model::Bank>) {
        let mut inner = self.inner.write().await;
        if let Some(w) = inner.workspaces.get_mut(workspace) {
            w.banked = banked;
        }
    }

    /// What this workspace has banked, as the daemon last knew it.
    pub async fn workspace_banked(&self, workspace: &str) -> Option<crate::model::Bank> {
        let inner = self.inner.read().await;
        inner
            .workspaces
            .get(workspace)
            .and_then(|w| w.banked.clone())
    }

    /// The live half of [`AppState::occupancy`], oldest first.
    pub async fn live_sessions_in(&self, workspace: &str) -> Vec<SessionId> {
        self.occupancy(workspace).await.live
    }

    /// Everything running in a workspace nobody is in any more, by name and pty.
    ///
    /// The shells and the hand-started processes exist for the session that
    /// opened them: once it is gone they are output nobody reads and a prompt
    /// nobody types into, still holding the port and the CPU. Anything config
    /// autostarts is spared, see `is_autostart`.
    ///
    /// **Named rather than killed here**, because stopping a managed process
    /// means running its `stop_command` first and that lives in `spawn`, one layer
    /// up. This used to send a bare `SIGHUP` to each pty instead: no escalation, so
    /// anything that traps it stayed, and no `stop_command`, which is the whole
    /// reason `crate::managed::stop_managed` exists — killing the pty leaves the real
    /// process running with nothing pointing at it. Containers are still not
    /// reached, the same as at shutdown: `docker compose up` has already detached
    /// by the time its pty dies.
    pub async fn processes_to_stop(
        &self,
        workspace: &str,
    ) -> Vec<(String, Arc<crate::pty::PtyHandle>)> {
        let inner = self.inner.read().await;
        inner
            .workspaces
            .get(workspace)
            .map(|w| {
                w.processes
                    .iter()
                    .filter(|p| !self.is_autostart(workspace, &p.name))
                    .filter_map(|p| Some((p.name.clone(), p.pty.clone()?)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Does config start this process by itself?
    ///
    /// Such a process was never opened by a session, so it is not a session's to
    /// take down with it: `ng-watch` in main is meant to be running whenever the
    /// daemon is, not only while somebody happens to have a session open there.
    fn is_autostart(&self, workspace: &str, name: &str) -> bool {
        self.cfg
            .processes_for(workspace)
            .iter()
            .any(|s| s.autostart && s.name == name)
    }

    // -----------------------------------------------------------------------
    // Reconcile
    // -----------------------------------------------------------------------

    /// Re-read changed files for a workspace from git.
    ///
    /// The git half runs on a blocking thread — see the comment on it. Everything
    /// after that is the write into state, which is what has to stay here.
    ///
    /// Hooks are the primary signal (§4); this catches the Bash-driven changes
    /// no `Edit` hook reported — codegen, builds, git ops.
    pub async fn reconcile(&self, workspace: &str) -> Result<()> {
        /* **Who was live here before git was asked**, because only they may take
        the branch it answers. A session spawned while the git below runs has
        already read its branch from the tree, later than this measurement, and
        stamping the older answer over it filed the conversation under the branch
        main was on a moment before. `to_carry` then carried nothing. Seen on
        macOS, where the boot sweep's git is slow enough to straddle a spawn. */
        let (path, is_main, before) = {
            let inner = self.inner.read().await;
            let w = inner
                .workspaces
                .get(workspace)
                .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?;
            let before: std::collections::HashSet<SessionId> = inner
                .sessions
                .values()
                .filter(|s| s.workspace == workspace && s.state.is_live())
                .map(|s| s.id)
                .collect();
            (w.path.clone(), w.is_main(), before)
        };
        // Main's tree contains every worktree, so drop paths under the worktrees
        // dir; a worktree sees only its own. The prefix follows a relocated layout.
        let exclude = is_main.then(|| self.cfg.worktrees_subdir_str());
        let upstream = self.cfg.upstream_ref.clone();

        /* **Seven git processes, and they belong on a blocking thread.** They ran
        straight on the async runtime, which the rest of the codebase does not
        do — `spawn.rs` and the pollers in `lib.rs` all wrap their git calls —
        so this was an oversight rather than a decision. What it costs is not
        the pty write path, which never comes through here; it is the API and
        the snapshot push, so it read as the whole board freezing. The workspace
        watcher calls this every 15 seconds, for every workspace.

        It got worse, not better, when the boot sweep stopped being awaited:
        the sweep used to finish before `axum::serve` was even spawned, so its
        blocking was invisible. Now it runs *while* the server answers, so a
        worker held for the length of a `diff::summary` against the merge-base
        is a worker not answering requests.

        One `spawn_blocking` for the lot rather than seven, because they
        describe one moment: the file list and the divergence are meant to
        agree, and interleaving them with other work is what would let them
        disagree. */
        let measure = {
            let path = path.clone();
            let exclude = exclude.clone();
            tokio::task::spawn_blocking(move || -> Result<Measured> {
                let set = git::status(&path, exclude.as_deref(), git::Untracked::Collapsed)?;
                // Recomputed alongside the file list, so the two always describe the
                // same moment.
                let divergence = git::divergence(&path, &upstream).ok();
                let rebasing = git::rebase_in_progress(&path);
                // Branches accumulate and are never removed (§2): a PR still belongs to
                // the session that made it after you have moved on to another branch.
                let branch = git::current_branch(&path).ok();
                // Needs the branch by name — `origin/HEAD` is the base, not this branch's
                // remote — so it is measured here rather than beside the divergence.
                let unpushed = branch
                    .as_deref()
                    .map(|b| git::unpushed_count(&path, b, &upstream));

                // What this workspace changed since it branched: committed work and
                // uncommitted both, which is the question the changed-files pane asks.
                // `git status` cannot answer it — a session that commits would empty its
                // own list — so it is a diff against the merge-base, plus the untracked
                // files a diff never sees.
                //
                // Failure is empty rather than fatal: a worktree whose upstream ref has
                // not been fetched yet still has a status worth showing.
                let base = git::merge_base(&path, &upstream).ok();
                // Sorted before the truncation, so what survives is a stable prefix rather
                // than whatever order git answered in — the pane's first 500 stay the same
                // 500 across reconciles, and a file does not appear and vanish while you
                // are looking at it.
                let changed = base.as_deref().map(|b| {
                    let mut files = crate::diff::summary(&path, b)
                        .map(|s| s.files)
                        .unwrap_or_default();
                    // Which of these rows is *also* uncommitted, from the status
                    // already read above. The pane's git verbs turn on this: the
                    // list is a diff against the merge base, so most rows on a PR
                    // branch differ because of a commit and have nothing to stage
                    // or discard.
                    crate::diff::mark_worktree_state(&mut files, &set);
                    files.extend(set.untracked.iter().map(crate::model::DiffFile::untracked));
                    files.sort_by(|a, b| a.path.cmp(&b.path));
                    let total = files.len() as u32;
                    files.truncate(CHANGED_CAP);
                    (files, total)
                });
                Ok(Measured {
                    divergence,
                    rebasing,
                    branch,
                    unpushed,
                    base,
                    changed,
                })
            })
        };
        // A panic in there would otherwise surface as a `JoinError` nobody reads.
        let Measured {
            divergence,
            rebasing,
            branch,
            unpushed,
            base,
            changed,
        } = measure.await.context("measuring the tree")??;

        let mut inner = self.inner.write().await;
        if let Some(w) = inner.workspaces.get_mut(workspace) {
            if let Some(b) = branch.clone() {
                w.branches.insert(b.clone());
                // The one that is true *now*, beside the set of every one it has
                // ever held. Kept when git could not answer, like the measurements
                // below: a tree mid-rebase has no branch, and blanking it would read
                // as "this checkout is free".
                w.tree.branch = Some(b);
            }
            // Each measurement keeps its previous value when git could not
            // answer, so a transient failure (an unfetched upstream, a lock
            // contended by a session) leaves the pane as it was rather than
            // blanking it. `rebasing` is the exception: it is a plain yes/no
            // that `rebase_in_progress` always answers.
            if let Some(b) = base {
                w.tree.base = Some(b);
            }
            if let Some(c) = changed {
                let (files, total) = c;
                w.tree.changed = files;
                w.tree.changed_total = total;
            }
            if let Some(d) = divergence {
                w.tree.divergence = d;
            }
            if let Some(u) = unpushed {
                w.tree.unpushed = u;
            }
            w.tree.rebasing = rebasing;
            // Last, and unconditionally. Every measurement above keeps its
            // previous value when git could not answer, so a workspace whose
            // first reconcile half-failed is still *measured*: the pane has the
            // real answer for what git could say and defaults for the rest, which
            // is the state the fields were always allowed to be in. Holding the
            // loader up for it would leave a tree that cannot be measured showing
            // a spinner for the life of the daemon.
            w.tree.measured = true;
        }
        /* A worktree's own branch history, for a session that has none yet: one
        recorded before `Session::branches` existed. A worktree is cut for one
        conversation, so what it held is that conversation's. Never main's, whose
        history is every branch anybody ever checked out there. Sorted, because the
        set has no order and the list is read oldest first. */
        let history: Vec<String> = inner
            .workspaces
            .get(workspace)
            .filter(|w| !w.is_main())
            .map(|w| {
                let mut h: Vec<String> = w.branches.iter().cloned().collect();
                h.sort();
                h
            })
            .unwrap_or_default();
        for s in inner.sessions.values_mut() {
            if s.workspace == workspace {
                s.dirty_paths.clear();
                s.last_reconcile = Some(SystemTime::now());
                // Live only, and that is the whole of the rule. A running agent
                // that checks out another branch really has changed what its
                // conversation is about, so the record follows it. An *archived*
                // one is history: re-stamping it from the tree would erase the
                // difference between "this conversation was about that branch" and
                // "that branch happens to be here now", which is the difference a
                // swap depends on to know who travels.
                if s.state.is_live() && before.contains(&s.id) {
                    if s.branches.is_empty() {
                        s.branches.clone_from(&history);
                    }
                    if let Some(b) = branch.clone() {
                        // Kept as well as followed: `branch` is only ever the
                        // current one, and the PRs on the ones before it are still
                        // this conversation's.
                        if !s.branches.contains(&b) {
                            s.branches.push(b.clone());
                        }
                        s.branch = Some(b);
                    }
                }
            }
        }
        drop(inner);
        self.refresh_overlaps(workspace).await;
        Ok(())
    }

    /// Where `workspace`'s changes meet every other workspace's, rebuilt from the
    /// lists the reconcile just measured.
    ///
    /// Each pair is ordered by id and compared in that order, so the line stamps
    /// mean the same thing whichever side refreshed. Git runs on a blocking
    /// thread, bounded by [`crate::overlap::LINE_BUDGET`] per refresh.
    pub async fn refresh_overlaps(&self, workspace: &str) {
        type Tree = (String, PathBuf, Option<String>, Vec<crate::model::DiffFile>);
        let upstream = self.cfg.upstream_ref.clone();
        let (me, others, known, known_stacks) = {
            let inner = self.inner.read().await;
            let Some(w) = inner.workspaces.get(workspace) else {
                return;
            };
            let tree = |w: &crate::model::Workspace| -> Tree {
                (
                    w.id.clone(),
                    w.path.clone(),
                    w.tree.base.clone(),
                    w.tree.changed.clone(),
                )
            };
            let others: Vec<Tree> = inner
                .workspaces
                .values()
                .filter(|o| o.id != workspace && o.tree.measured && !o.tree.changed.is_empty())
                .map(tree)
                .collect();
            let known: Vec<crate::overlap::Pair> = inner
                .overlaps
                .iter()
                .filter(|p| p.involves(workspace))
                .cloned()
                .collect();
            let known_stacks: HashMap<(String, String), crate::overlap::Stack> = inner
                .stacks
                .iter()
                .filter(|((a, b), _)| a == workspace || b == workspace)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            (tree(w), others, known, known_stacks)
        };
        let (fresh, stacks) = if me.3.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            orchd_base::proc::run_blocking("comparing changed files", move || {
                let mut budget = crate::overlap::LINE_BUDGET;
                let mut pairs = Vec::new();
                let mut stacks = Vec::new();
                for other in &others {
                    let (a, b) = if me.0 < other.0 {
                        (&me, other)
                    } else {
                        (other, &me)
                    };
                    let base = a.2.as_deref().filter(|_| a.2 == b.2);
                    let before: Vec<crate::overlap::Found> = known
                        .iter()
                        .filter(|p| p.a == a.0 && p.b == b.0)
                        .map(|p| p.found.clone())
                        .collect();
                    let found =
                        crate::overlap::compare(&a.1, &a.3, &b.1, &b.3, base, &before, &mut budget);
                    /* Whether the pair is a stack, asked only of a pair that shares
                    files, and asked again only when what they share moved: it is
                    five git calls, and a reconcile runs every few seconds. */
                    let key = (a.0.clone(), b.0.clone());
                    if !found.is_empty() {
                        let stack = match known_stacks.get(&key) {
                            Some(s) if found == before => Some(s.clone()),
                            prev => crate::overlap::stack_of(&a.1, &b.1, &upstream, prev),
                        };
                        if let Some(s) = stack {
                            stacks.push((key, s));
                        }
                    }
                    for found in found {
                        pairs.push(crate::overlap::Pair {
                            a: a.0.clone(),
                            b: b.0.clone(),
                            found,
                        });
                    }
                }
                (pairs, stacks)
            })
            .await
            .unwrap_or_default()
        };
        let mut inner = self.inner.write().await;
        inner.overlaps.retain(|p| !p.involves(workspace));
        inner.overlaps.extend(fresh);
        inner
            .stacks
            .retain(|(a, b), _| a != workspace && b != workspace);
        inner.stacks.extend(stacks);
        inner
            .overlaps
            .sort_by(|x, y| (&x.found.path, &x.a, &x.b).cmp(&(&y.found.path, &y.a, &y.b)));
        /* **No note from here.** A note sent from the refresh went out as a turn
        on every refresh, to a session the check wrongly found stacked on four
        branches: about 1,500 in an hour. Stack notes ride on the next prompt now
        (`overlap_note`), once per pair and lower head. */
    }
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// What the SPA receives on every tick.
///
/// `TS` is derived under `cfg(test)` only, so `ts-rs` is a dev-dependency and
/// nothing about it reaches the shipped binary. `cargo test` writes
/// `web/snapshot.d.ts` from these definitions, and `web/app.js` type-checks
/// against it — which is what stops a field being renamed here and read by the
/// old name there, the way `pr_age_ms` sat unread and the divergence strip named
/// a ref it had not measured.
#[derive(Debug, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct Snapshot {
    /// The configured tracker's MCP server name, or `None` for no tracker.
    ///
    /// Here rather than on `/api/config` because it is read-only to the SPA: a
    /// tracker is three fields (`config::Tracker`), one of them a per-site host, so
    /// the settings pane shows it and does not offer it — and `Settings` leaves it
    /// out so a write of the whole struct cannot replace a hand-edited one.
    pub tracker_server: Option<String>,
    /// The story ref prefix the page underlines, `sc-`, when a usable
    /// `story_link` is set; `None` leaves story refs as plain text. The URL stays
    /// in the daemon, which builds it on the click (`/api/open/ref`).
    pub story_prefix: Option<String>,
    /// Workspaces the spare pool is holding — cut, based, and not yet anybody's.
    ///
    /// Here so the pool can be *seen*. A spare is deliberately an ordinary
    /// workspace, which makes it indistinguishable on the board from one nobody
    /// has opened; without this, the only way to tell whether a create was handed
    /// a pre-cut tree would be to time it, which is the assertion
    /// `docs/traps/e2e.md` warns against. The e2e flow reads this, and so can a
    /// person wondering why the pool is empty.
    pub spare: Vec<String>,
    pub workspaces: Vec<WorkspaceView>,
    pub sessions: Vec<SessionView>,
    /// Past conversations in this checkout that orchd never started, newest first.
    /// The rail lists them under the same fold as its own archive.
    pub external: Vec<ExternalView>,
    pub prs: Vec<PrView>,
    /// PRs that have merged or closed since this daemon started, so a session row
    /// can say which rather than silently losing its PR.
    pub prs_ended: Vec<EndedPrView>,
    /// Set when the last poll failed; the pane says so rather than showing an
    /// empty list.
    pub pr_error: Option<String>,
    /// The agent exited before its first turn and we did not stop it; see
    /// `Inner::agent_error`. The board shows it in a bar, since the session it
    /// happened to no longer exists.
    pub agent_error: Option<String>,
    /// The agents a new session can be started on. The SPA offers a choice only
    /// when there is more than one.
    pub agents: Vec<crate::agent::Agent>,
    /// What a plain `+` starts.
    pub default_agent: crate::agent::Agent,
    /// Which forge the checkout is on, so the page can name it: "open on GitHub"
    /// rather than "open on forge".
    pub forge: crate::config::ForgeKind,
    /// What the boot preflight found; see `Inner::machine`. The board shows it in
    /// a bar, because these are the causes behind panes that read `unavailable`
    /// and `off` and there is nowhere else in the window they appear.
    pub machine: Vec<crate::machine::Warning>,
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub pr_age_ms: Option<u64>,
    /// Monotonic counter of completed PR polls; see `Inner::pr_poll`.
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub pr_poll: u64,
    /// A PR fetch is running. The pane spins its refresh icon while it is,
    /// however the fetch was started.
    pub pr_polling: bool,
    pub token_source: Option<crate::forge::TokenSource>,
    pub reviews: crate::reviews::ReviewState,
    /// Age of the last completed review poll, in ms. `None` before the first.
    /// Mirrors `pr_age_ms`.
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub reviews_age_ms: Option<u64>,
    /// Monotonic counter of completed review polls; see `Inner::reviews_poll`.
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub reviews_poll: u64,
    pub reviews_polling: bool,
    /// Ordered, for the same reason `WorkspaceView::branches` is sorted: the page
    /// decides whether to repaint by comparing snapshot text, and a `HashMap`
    /// rehash reorders these keys without changing a word of what they say.
    #[cfg_attr(
        any(test, feature = "test-util"),
        ts(as = "std::collections::HashMap<String, crate::model::PrAutomation>")
    )]
    pub automation: BTreeMap<u64, crate::model::PrAutomation>,
    pub repos: Repos,
    /// Main may hold more than one live session (`allow_several_in_main`).
    ///
    /// Sent because the rail decides whether to offer `+` on main, and the reason
    /// it may is a setting the SPA cannot see any other way.
    pub several_in_main: bool,
    /// The base `behind`/`ahead` are measured against, e.g. `upstream/develop`.
    ///
    /// Sent because it is a setting: the divergence strip used to print the
    /// default as a literal, so on any repo that had edited it the UI named a ref
    /// the numbers did not come from.
    pub upstream_ref: String,
    /// The worktree cut in flight: which step, and what it has printed. See
    /// [`crate::model::CreateRun`].
    pub create_run: Option<crate::model::CreateRun>,
    /// A newer release than the running build, or `None`.
    pub update: Option<crate::model::UpdateInfo>,
    /// The app's own upgrade run: `running` while `mise upgrade` goes, then a tail
    /// that is empty on success. Success means *installed*, not applied — this
    /// process is still the old build, so the bar then asks for a restart.
    pub self_upgrade_run: Option<crate::model::UpgradeRun>,
    /// A newer Claude Code than the installed one, or `None`. Actionable in the
    /// UI: the upgrade cannot disturb a session already running.
    pub agent_update: Option<crate::model::AgentUpdate>,
    /// The upgrade the update bar reports on, while it runs and after it fails.
    pub upgrade_run: Option<crate::model::UpgradeRun>,
    /// The running build's own version, for the settings panel. Always here,
    /// unlike `update`, which only appears when there is something newer: "which
    /// build am I on" is a question worth answering when the answer is "the
    /// latest one".
    pub version: &'static str,
}

/// A past conversation orchd never started, as the archive draws it.
///
/// Deliberately not a [`SessionView`] with the fields blanked. There is no state
/// machine behind one, no pty, no branch and no claim on a workspace, and a row
/// that carries those as defaults is a row every reader has to remember is
/// lying. `has_transcript: false` on a conversation that plainly has one is
/// exactly the shape `isConversation` filters away.
#[derive(Debug, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct ExternalView {
    pub id: Uuid,
    /// The workspace whose directory holds the transcript, which is where a resume
    /// would put it.
    pub workspace: String,
    /// Claude Code's own `ai-title`. `None` reads as untitled in the rail: there is
    /// no name you could have given one of these, and no workspace name worth
    /// borrowing, since the workspace is on the row already.
    pub title: Option<String>,
    /// Age of the last turn. Named `_ms` for the same reason every other age is:
    /// `paintSig` drops those keys, so a clock ticking in the snapshot does not
    /// rebuild the rail.
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub last_used_ms: u64,
    /// Something was writing to this conversation a moment ago, so it may still be
    /// open in a terminal ([`crate::store::recently_active`]).
    ///
    /// Sent so the rail can ask before it resumes one rather than after: the daemon
    /// refuses an unforced resume of a live conversation, and a refusal the page
    /// could have predicted is a toast with nothing you can do about it. It is as
    /// fresh as the last scan, so the two can disagree for up to a poll — the
    /// daemon's own check at the moment of the call is the one that decides.
    pub may_be_live: bool,
    pub agent: crate::agent::Agent,
}

#[derive(Debug, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct PrView {
    #[serde(flatten)]
    pub pr: crate::forge::Pr,
    pub rank: u8,
    /// The workspace that has this PR's head checked out ([`Inner::workspace_on`]).
    pub workspace: Option<String>,
    /// The live session working on this PR's head ([`Inner::session_on`]), so the
    /// row can act as a jump link.
    pub session: Option<Uuid>,
    /// What a press on the row's `handle` or `fix` would do. See [`Press`].
    pub press: Press,
}

/// What a press on a PR row's `handle` or `fix` would do, decided here so the page
/// draws it rather than re-deriving the daemon's rules.
///
/// **The page used to carry a copy of each rule**, `canPress` among them, and it
/// had to change in step with `triage` every time the hand-off learned something —
/// a session in main became pressable on 2026-11-01, in two files at once. The
/// copy also missed one entirely: a PR you may not push to showed both buttons and
/// refused on the press.
///
/// From the snapshot's own state, so no git: the press itself still locates the
/// PR afresh (`locate::locate_pr`) and refuses on what it finds, and this is what
/// the row offers until then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub enum Press {
    /// A press starts the job, or hands it to the conversation on the branch.
    Open,
    /// A pass is already working on this PR, and the row goes to it: `fix-pr` for
    /// a fix run, `handle-review` for a review pane.
    Running {
        #[cfg_attr(any(test, feature = "test-util"), ts(type = "string"))]
        session: Uuid,
        command: String,
    },
    /// A press would be refused, and this is the sentence it would be refused with.
    Refused { why: String },
}

impl Inner {
    /// [`Press`] for one PR, given the session [`Inner::session_on`] found on it.
    ///
    /// In the order the press asks: a run already going is "go to it", not a
    /// refusal; then the two things no press can get past, someone else's branch
    /// and a branch you may not push to; then another pass holding the tree, which
    /// `triage::hand_to_live_session` refuses by name.
    pub fn press_for(&self, p: &crate::forge::Pr, session: Option<SessionId>) -> Press {
        if let Some(PrAutomation::Running { session, .. }) = self.automation.get(p.number) {
            return Press::Running {
                session: *session,
                command: Pass::FIX_PR.to_string(),
            };
        }
        let handling = self.sessions.values().find(|s| {
            s.state.is_live()
                && s.pass
                    .as_ref()
                    .is_some_and(|pass| pass.pr == p.number && pass.command == Pass::HANDLE_REVIEW)
        });
        if let Some(s) = handling {
            return Press::Running {
                session: s.id,
                command: Pass::HANDLE_REVIEW.to_string(),
            };
        }
        if let Some(why) = self.foreign_heads.get(&p.number) {
            return Press::Refused {
                why: cannot_open_pr(p.number, why),
            };
        }
        if let Some(why) = p.push_refusal(self.viewer.as_deref().unwrap_or_default()) {
            return Press::Refused { why };
        }
        let other_pass = session
            .and_then(|id| self.sessions.get(&id))
            .and_then(|s| s.pass.as_ref().map(|pass| (pass, &s.workspace)));
        if let Some((pass, ws)) = other_pass {
            return Press::Refused {
                why: Pass::holds_tree(&pass.command, ws),
            };
        }
        Press::Open
    }
}

/// An ended PR, resolved to a workspace the same way [`PrView`] is.
///
/// A separate view rather than a flag on `PrView`, because the two answer
/// different questions and nearly every reader of `prs` means "open PRs" — the
/// pane that lists them, the automation's head reconciliation, the review queue.
/// Folding ended ones into that list would have made every one of those readers
/// filter, and the one that forgot would act on a merged PR.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct EndedPrView {
    #[serde(flatten)]
    pub pr: crate::forge::EndedPr,
    /// The workspace whose branch set contains this PR's head ref, if it is still
    /// here. A merged PR whose worktree has already gone resolves to `None` and
    /// nothing draws it, which is the right answer: there is no row to tell.
    pub workspace: Option<String>,
}

/// Work banked out of a rebase's way, as the pane needs it.
///
/// The ref is sent rather than derived in the SPA, because it is the recovery a
/// person runs by hand (`git stash apply <at>`) and a second spelling of the name
/// is a second thing to keep in step.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct BankedView {
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub files: u32,
    pub at: String,
}

#[derive(Debug, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct WorkspaceView {
    pub id: String,
    pub path: String,
    pub kind: WorkspaceKind,
    pub is_main: bool,
    /// The recorded claim, as `claim_main` last wrote it. **Not** whether main is
    /// held: it can name a session that has died, and name none of several live
    /// ones. Sent for the flows that watch the claim itself.
    pub occupant: Option<Uuid>,
    /// Who holds main, by the rule the daemon refuses on ([`AppState::main_holder`]):
    /// the live claimant, else the oldest live session in main, else one auto-resume
    /// is about to bring back, else one still starting. `None` on every other
    /// workspace. The "+ main" button
    /// reads this, because a page that decided from `occupant` offered a session
    /// the daemon then refused.
    pub held_by: Option<Uuid>,
    /// Every branch this workspace has ever held, never pruned (§2).
    ///
    /// Not "what is checked out" — see `branch` for that. The two were conflated
    /// once, and the rail offered to swap with a main that was sitting on its base.
    pub branches: Vec<String>,
    /// What it has checked out now, or `None` before the first reconcile.
    pub branch: Option<String>,
    pub processes: Vec<ProcessView>,
    /// Managed processes this workspace declares that are not running — config
    /// order, names only.
    ///
    /// The drawer built its tabs from `processes` alone, so anything with
    /// `autostart: false` had no way into the app at all: the restart button the
    /// config comment points at is drawn *on a tab*, and there was no tab. A
    /// `docker compose up` you deliberately do not autostart still has to be
    /// startable.
    pub stopped_processes: Vec<String>,
    /// Uncommitted work the rebase button parked, if any is parked.
    pub banked: Option<BankedView>,
    /// Every file this workspace changed since it branched, committed work
    /// included, plus anything untracked. What the changed-files pane lists.
    ///
    /// **Capped at [`CHANGED_CAP`]**, with the real number in `changed_total`.
    /// That constant carries the measurement and the incident behind it.
    pub changed: Vec<crate::model::DiffFile>,
    /// `changed` was left out to keep the snapshot small: this workspace is idle,
    /// and its list is at `/api/workspace/:id/changed`. See
    /// [`Inner::is_active_workspace`].
    pub changed_omitted: bool,
    /// How many there really are, when `changed` is a prefix of them.
    ///
    /// Sent rather than inferred from the length, so the pane can say "500 of
    /// 5,214" instead of quietly presenting a truncation as the whole answer —
    /// the same honesty `unresolved_capped` buys the PR pane (§6).
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub changed_total: u32,
    /// The commit the above is measured from: `merge-base(upstream, HEAD)`.
    pub changed_since: Option<String>,
    /// Commits on `upstream/develop` this branch does not have. Drives the
    /// rebase affordance.
    pub behind: u32,
    pub ahead: u32,
    /// Commits its own remote does not have yet. The changes pane's pushed view
    /// fetches again when this moves, since a push changes nothing else here.
    pub unpushed: u32,
    pub rebasing: bool,
    /// Whether the four fields above have ever been measured for this workspace
    /// — see [`crate::model::Tree::measured`].
    ///
    /// The pane needs it because the first sweep no longer finishes before the
    /// window opens: without it an unmeasured worktree renders as "nothing
    /// changed", which is the one thing a changed-files pane must not say when it
    /// does not know.
    pub measured: bool,
    /// Files this workspace changed that another workspace changed too, sorted by
    /// path and then by the other workspace. See [`crate::overlap`].
    pub overlaps: Vec<OverlapView>,
    /// What this workspace is stacked on, when it is the upper half of a stack.
    pub stacked_on: Option<StackView>,
}

/// How many messages one session may send another through `orch tell`.
pub const TELL_CAP: u32 = 3;

/// One file another workspace changed as well.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct OverlapView {
    pub path: String,
    /// The other workspace's id.
    pub with: String,
    /// Whether git cannot merge the two sides of the file, not only that both
    /// changed it.
    pub conflict: bool,
    /// What the user confirmed for this pair, when the two agents settled it.
    pub plan: Option<String>,
}

/// The branch a workspace is built on, when it is the upper half of a stack.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct StackView {
    /// The lower workspace's id.
    pub on: String,
    /// Commits on the lower branch this one does not have yet.
    pub behind: u32,
    /// Files a restack onto it will stop on.
    pub conflicts: u32,
}

#[derive(Debug, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
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
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
pub struct SessionView {
    pub id: Uuid,
    pub workspace: String,
    pub state: State,
    pub pass: Option<Pass>,
    /// What to call this session — the name you gave it, else Claude Code's own
    /// ai-title (`Session::label`). The panes read this and stay out of the
    /// precedence.
    pub title: Option<String>,
    /// Only the name you typed, so the rename prompt can offer it back and an
    /// empty answer can mean "go back to the ai-title" rather than "freeze it".
    pub name: Option<String>,
    pub cwd: String,
    /// The branch this conversation is about ([`Session::branch`]), not the tree it
    /// sits in. Sent so `orch ls` can answer the question its columns could not:
    /// whether two sessions can safely work in parallel. A shared `cwd` means one
    /// git index, and two rows on one branch means one branch — and neither was
    /// visible from id, workspace and state.
    pub branch: Option<String>,
    pub rank: u8,
    /// Whether this is idle time worth counting. A session you just opened is
    /// idle but not waiting on you in the sense the rail exists to surface.
    pub wants_attention: bool,
    /// How long this session has been waiting on you. With 4-6 sessions the
    /// cost of the whole tool is measured in agent-minutes spent idle (§2).
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub waiting_ms: Option<u64>,
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub created_ms: u64,
    pub alive: bool,
    pub dirty_count: usize,
    pub boundary_violations: Vec<String>,
    pub resumable: bool,
    /// Whether a turn ever happened here — the SPA gates Fork and the nudge on it,
    /// and lists an archived row only when it is true. Named for the file it used to
    /// stat; it is now the `had_a_turn` bit, which answers the same question the
    /// callers actually meant without a per-snapshot read.
    pub has_transcript: bool,
    /// What this session is blocked on, waiting for you. The ask box over the
    /// terminal renders it; everything else ignores it.
    pub interaction: Option<Interaction>,
    /// The conversation this one was cut from. A fork inherits its parent's
    /// title, so without this two rows read the same and neither says why.
    pub forked_from: Option<crate::model::SessionId>,
    /// The PR this conversation was about, open or long since merged. The
    /// archive row shows it, because the forge no longer remembers it.
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number | null"))]
    pub pr: Option<u64>,
    /// Every PR this session opened, oldest first. The rail names the worst of
    /// them and counts the rest; the pane header draws one chip each.
    pub prs: Vec<crate::model::SessionPr>,
    /// Whether this session still owes you the rest of a turn. The nudge is only
    /// true of these; every other resumed session is sitting at its prompt done.
    pub interrupted: bool,
    /// Waiting to be respawned on the `claude` installed now — see
    /// [`Session::restart_queued`]. The row says so, because a session that
    /// restarts itself the moment you finish a turn is a surprise otherwise.
    pub restart_queued: bool,
    /// Messages from other agents waiting for this session to be idle
    /// (`orch tell`). Shown so a message the daemon cannot deliver is not
    /// invisible: a turn that ended on an escape sends no `Stop`, and the state
    /// can then say busy while the pane is at its prompt.
    pub inbox: usize,
    /// Running an older Claude Code than a new session here would get — see
    /// [`Session::agent_stale`]. What the agent bar counts when it offers to
    /// restart the sessions an upgrade left behind.
    pub agent_stale: bool,
    /// Which agent runs this conversation. The pane header names it.
    pub agent: crate::agent::Agent,
    /// The id the agent knows this conversation by ([`Session::conversation_id`]),
    /// which "copy id" copies: `id` is the daemon's, and Codex has never heard of
    /// it. `None` for a Codex session that has not said its id yet.
    pub conversation_id: Option<String>,
    /// Whether the rail may offer rewind ([`crate::agent::Agent::can_rewind`]).
    /// The daemon refuses on the same answer; this only decides the menu.
    pub can_rewind: bool,
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
            pass: s.pass.clone(),
            title: s.label().map(str::to_owned),
            name: s.name.clone(),
            cwd: s.cwd.to_string_lossy().into_owned(),
            branch: s.branch.clone(),
            rank: s.state.rank(),
            wants_attention: s.state.wants_attention(),
            waiting_ms,
            created_ms: now
                .duration_since(s.created_at)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            alive: s.pty.as_ref().map(|h| h.is_alive()).unwrap_or(false),
            dirty_count: s.dirty_paths.len(),
            boundary_violations: s.boundary_violations.clone(),
            // Whether a turn ever happened, not whether a file exists — forking or
            // resuming a session that never had one offers something Claude answers
            // with "no conversation found". Read straight off the session rather
            // than stat'ing the transcript, so it costs nothing on a hot path that
            // runs for every session on every notify.
            has_transcript: s.has_conversation(),
            resumable: s.resumable(),
            interaction: s.interaction.clone(),
            forked_from: s.forked_from,
            pr: s.pr,
            prs: s.prs.clone(),
            interrupted: s.interrupted,
            restart_queued: s.restart_queued,
            inbox: s.inbox.len(),
            agent_stale: s.agent_stale,
            agent: s.agent,
            conversation_id: s.conversation_id(),
            can_rewind: s.agent.can_rewind(),
        }
    }
}

impl AppState {
    /// Type text at a session as an ordinary user turn, or say why a keystroke there
    /// would mean something else.
    ///
    /// The three refusals are the states where the pty is listening for an answer
    /// rather than for a prompt: mid-turn (Claude Code submits whatever is half
    /// typed), a permission prompt (consent) and an open question (the highlighted
    /// choice). `nudge_sessions` learned them first, the drawer's hand-off second, and
    /// the rebase button's is the third caller — which is what made this one function
    /// instead of three copies of the table.
    ///
    /// **On the state, not in `api`, because it is about a session and its pty
    /// and nothing else.** It lived in `api` while only routes called it; the
    /// review hand-off in `triage` then made `api -> fix_pr -> triage -> api` a
    /// cycle the module ratchet refuses.
    pub(crate) async fn type_user_turn(&self, id: SessionId, text: &str) -> anyhow::Result<()> {
        let pty = {
            let inner = self.inner.read().await;
            let s = inner
                .sessions
                .get(&id)
                .ok_or_else(|| crate::state::no_such_session(id))?;
            let name = s.label().unwrap_or(&s.workspace).to_string();
            let Some(pty) = s.pty.clone().filter(|p| p.is_alive()) else {
                anyhow::bail!("{name} is not running — resume it first");
            };
            match &s.state {
                crate::model::State::Starting => {
                    anyhow::bail!("{name} is still starting")
                }
                // A red build is drawn over a wait, and a question under it is
                // still a question.
                crate::model::State::YourTurn { reason, .. }
                | crate::model::State::BuildFailing { covers: reason, .. } => match reason {
                    // Both take a keystroke as an answer rather than as a prompt.
                    crate::model::TurnReason::NeedsPermission => {
                        anyhow::bail!("{name} is waiting on a permission prompt; answer that first")
                    }
                    crate::model::TurnReason::AskedAQuestion => {
                        anyhow::bail!("{name} is asking you something; answer that first")
                    }
                    _ => {}
                },
                // Working, and everything else that is not a prompt: mid-turn.
                other => {
                    if other.is_busy() {
                        anyhow::bail!("{name} is mid-turn; wait for it to finish")
                    }
                }
            }
            pty
        };

        // The same two-step every typed line uses: write, wait, then send. Claude
        // Code's prompt box drops a `\r` that arrives in the same breath as the text.
        pty.type_and_send(text.as_bytes(), std::time::Duration::from_millis(500));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn app() -> Arc<AppState> {
        crate::testutil::app("state").0
    }

    /// The same fixture with `allow_several_in_main` on.
    async fn app_sharing_main() -> Arc<AppState> {
        crate::testutil::app_with("share", r#""allow_several_in_main":true"#).0
    }

    /// Put a live session in main, the way a spawn does.
    async fn live_in_main(app: &Arc<AppState>) -> SessionId {
        let id = Uuid::new_v4();
        let mut inner = app.inner.write().await;
        let mut s = Session::new(id, MAIN.to_string(), std::path::PathBuf::from("/tmp"), None);
        s.set_state(State::Working);
        inner.sessions.insert(id, s);
        id
    }

    /// **The fallback is held to what is installed too.** The last pick defaults
    /// to Claude Code, so a checkout with only Codex on PATH started `claude`.
    #[tokio::test]
    async fn a_plain_press_never_starts_an_agent_that_is_not_installed() {
        let app = app().await;
        {
            let mut inner = app.inner.write().await;
            inner.agents = vec![crate::agent::Agent::Codex];
            inner.last_agent = crate::agent::Agent::Claude;
        }
        assert_eq!(
            app.agent_for(None).await.ok(),
            Some(crate::agent::Agent::Codex)
        );
    }

    /// **Nothing is written over the store before boot has read it.** The agent
    /// probe notified ahead of the load, and every restart where it beat the
    /// upstream fetch wrote `[]` over the records boot then read back as none.
    #[tokio::test]
    async fn a_notify_before_the_records_are_restored_writes_nothing() {
        let app = AppState::new(
            crate::testutil::test_config(),
            "t".into(),
            crate::window::Chrome::None,
        );
        let _ = live_in_main(&app).await;
        // `persisted_ids` is what a write records, read rather than the file: the
        // test binary shares one scratch store, and other tests write it too.
        let written = || app.persisted_ids.load(std::sync::atomic::Ordering::SeqCst);

        app.notify().await;
        assert_eq!(
            written(),
            0,
            "an empty board that has not loaded yet is not an empty board"
        );

        app.restore_sessions(Vec::new()).await;
        app.notify().await;
        assert_ne!(written(), 0, "and once loaded it writes");
    }

    /// **The record a shutdown writes has to carry auto-resume's intent** (#33).
    ///
    /// A session waiting its turn in the resume list has no process, so its state
    /// is not live — and `was_live` is what the *next* start filters on. Shutdown
    /// used to write `false` for every one of them, which is how a quit inside the
    /// resume window emptied the rail with no error anywhere.
    #[tokio::test]
    async fn a_session_waiting_to_be_resumed_is_recorded_as_live() {
        let app = app().await;
        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), std::path::PathBuf::from("/tmp"), None);
            // Exactly what a restored record looks like before its spawn: known,
            // and not running.
            s.set_state(State::Exited);
            inner.sessions.insert(id, s);
        }

        let before = app.session_records().await;
        assert_eq!(before.len(), 1);
        assert!(
            !before[0].was_live,
            "a session nothing intends to resume is not live"
        );

        app.pending_resume
            .lock()
            .expect("the pending set")
            .insert(id);
        let during = app.session_records().await;
        assert!(
            during[0].was_live,
            "a session auto-resume has not reached yet must still be written as live"
        );

        // And the intent ends with the spawn, or a session you closed by hand
        // during the window would come back on the next start.
        app.pending_resume
            .lock()
            .expect("the pending set")
            .remove(&id);
        assert!(!app.session_records().await[0].was_live);
    }

    /// Until auto-resume reaches the session that was in main, that session holds
    /// main: nothing else may claim it or move its branch. And only until
    /// auto-resume is done, or a resume that failed would hold main for good.
    #[tokio::test]
    async fn a_session_waiting_to_resume_in_main_holds_main_until_auto_resume_ends() {
        use std::sync::atomic::Ordering::SeqCst;

        let app = app().await;
        let waiting = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(
                waiting,
                MAIN.to_string(),
                std::path::PathBuf::from("/tmp"),
                None,
            );
            s.set_state(State::Exited);
            inner.sessions.insert(waiting, s);
        }
        app.pending_resume
            .lock()
            .expect("the pending set")
            .insert(waiting);
        app.auto_resuming.store(true, SeqCst);

        assert_eq!(app.main_in_use_now().await, Some(waiting));
        app.claim_main(Uuid::new_v4())
            .await
            .expect_err("a new session must not take main from one coming back");
        app.claim_main(waiting)
            .await
            .expect("its own resume takes main");
        app.release_main(waiting).await;

        app.auto_resuming.store(false, SeqCst);
        assert_eq!(
            app.main_in_use_now().await,
            None,
            "auto-resume is done, so main is free"
        );
        app.claim_main(Uuid::new_v4())
            .await
            .expect("a resume that failed does not hold main for good");
    }

    /// Two snapshots of an unchanged daemon have to be the same bytes.
    ///
    /// **The page decides whether to repaint by comparing snapshot text.** So a
    /// field that serialises in a different order each time is a rail torn down
    /// under the pointer for a snapshot carrying no news — every session row and
    /// every PR row rebuilt, and the hover highlight on the row you are aiming at
    /// never finishing its fade.
    ///
    /// Two fields did exactly that and were fixed by hand: `WorkspaceView::branches`
    /// came off a `HashSet` unsorted, and `automation` was a `HashMap` serialised
    /// in rehash order. Fixing the third one by hand is what this test exists to
    /// stop. It is deliberately about the *whole* snapshot rather than those two,
    /// because the next one will be a field nobody is thinking about — measured
    /// idle before the fix: one rail rebuild in 35 seconds, this and nothing else.
    ///
    /// Populated rather than empty: an empty collection serialises the same way
    /// whatever holds it, so a bare fixture would pass while proving nothing.
    #[tokio::test]
    async fn a_snapshot_of_an_unchanged_daemon_is_the_same_bytes_twice() {
        let app = app().await;
        let id = live_in_main(&app).await;
        {
            let mut inner = app.inner.write().await;
            // Several branches, inserted in an order that is not their sorted one,
            // so an unsorted collection has something to get wrong.
            if let Some(w) = inner.workspaces.get_mut(MAIN) {
                for b in ["zebra", "alpha", "middle", "beta"] {
                    w.branches.insert(b.to_string());
                }
            }
            // Sixteen, inserted out of order: a `HashMap` holds its order steady
            // within one process until it rehashes, so *repeating* the snapshot
            // cannot catch one on its own — what catches it is asking whether the
            // order is **defined**, and sixteen keys coming back ascending by
            // chance is not a thing that happens.
            for pr in [
                77_u64, 3, 512, 9, 41, 8, 630, 12, 205, 1, 99, 34, 7, 888, 56, 23,
            ] {
                inner.automation.0.by_pr.insert(
                    pr,
                    crate::model::PrAutomation::Running {
                        session: uuid::Uuid::nil(),
                        started: std::time::UNIX_EPOCH,
                    },
                );
            }
        }

        /* **Compared the way the page compares them**, which means without the
        `_ms` fields. Those are durations measured when the snapshot is taken,
        so they differ on every build by construction — `paintSig` drops every
        key ending `_ms` for exactly that reason, and each one reaches the page
        as a `data-clock` node that ticks in place. Comparing raw text here
        would only ever be re-testing that a clock moves. */
        let first = serde_json::to_string(&app.snapshot().await).unwrap();
        let stable = |json: &str| {
            let mut v: serde_json::Value = serde_json::from_str(json).unwrap();
            drop_durations(&mut v);
            v
        };
        let before = stable(&first);
        for _ in 0..8 {
            let again = serde_json::to_string(&app.snapshot().await).unwrap();
            assert_eq!(
                before,
                stable(&again),
                "nothing but the clocks changed between these two snapshots, so the \
                 page must see the same thing — a collection serialising in rehash \
                 order rebuilds every row the pointer is on"
            );
        }

        /* **And the order has to be *defined*, not merely repeated.** Both halves
        are needed and neither replaces the other: the loop above catches a
        value that differs per call, and this catches a container that happens
        to be steady in one process and reorders in the next — which is what a
        `HashMap` does, and is why putting `automation` back to one still
        passed the loop. */
        let snap = app.snapshot().await;
        let ws = snap.workspaces.iter().find(|w| w.id == MAIN).unwrap();
        let mut sorted = ws.branches.clone();
        sorted.sort();
        assert_eq!(ws.branches, sorted, "branches are sorted, not hash-ordered");

        // Read the key order out of the text, because parsing it back would sort
        // the keys for us and answer a question nobody asked.
        let keys = automation_keys_in_order(&first);
        let mut ascending = keys.clone();
        ascending.sort_unstable();
        assert_eq!(
            keys, ascending,
            "the automation map has to serialise in a defined order — a `HashMap` \
             here reorders on a rehash and rebuilds every row for a snapshot that \
             says the same thing"
        );
        assert_eq!(
            keys.len(),
            16,
            "the fixture's records all have to be in there"
        );
        let _ = id;
    }

    /// Strip every `_ms` field, the way `paintSig` does before the page compares.
    fn drop_durations(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(map) => {
                map.retain(|k, _| !k.ends_with("_ms"));
                for inner in map.values_mut() {
                    drop_durations(inner);
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(drop_durations),
            _ => {}
        }
    }

    /// The `automation` keys, in the order the JSON actually lists them.
    ///
    /// Read out of the text rather than parsed, because `serde_json` would sort
    /// the keys on the way in and answer a question nobody asked. Depth-counted,
    /// because each value is itself an object — splitting on the first `}` finds
    /// one key and the length assertion above is what says so.
    fn automation_keys_in_order(json: &str) -> Vec<u64> {
        let mark = "\"automation\":{";
        let at = json.find(mark).expect("a snapshot carries automation");
        let body = &json[at + mark.len()..];
        let mut keys = Vec::new();
        let mut depth = 0i32;
        let mut rest = body;
        while let Some(i) = rest.find(['{', '}', '"']) {
            match rest.as_bytes()[i] {
                b'{' => {
                    depth += 1;
                    rest = &rest[i + 1..];
                }
                b'}' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    rest = &rest[i + 1..];
                }
                _ => {
                    let after = &rest[i + 1..];
                    let end = after.find('"').unwrap_or(0);
                    // A key at the top level of this object, not one inside a value.
                    if depth == 0 && after[end + 1..].starts_with(':') {
                        if let Ok(n) = after[..end].parse() {
                            keys.push(n);
                        }
                    }
                    rest = &after[end + 1..];
                }
            }
        }
        keys
    }

    #[tokio::test]
    async fn main_is_exclusive_by_default_and_shared_when_the_setting_says_so() {
        let app = app().await;
        let first = live_in_main(&app).await;
        app.claim_main(first).await.unwrap();
        assert!(
            app.claim_main(Uuid::new_v4()).await.is_err(),
            "one checkout, one index: the default has to refuse"
        );

        let shared = app_sharing_main().await;
        let first = live_in_main(&shared).await;
        shared.claim_main(first).await.unwrap();
        let second = live_in_main(&shared).await;
        shared
            .claim_main(second)
            .await
            .expect("allow_several_in_main lifts the refusal");
        assert_eq!(
            shared.inner.read().await.workspaces[MAIN].occupant,
            Some(second),
            "the claim is still recorded; only the refusal is off"
        );
    }

    /// Turning the setting off with two sessions in main must not let a third in
    /// once the one the claim names has gone.
    #[tokio::test]
    async fn turning_sharing_off_keeps_a_third_session_out() {
        let shared = app_sharing_main().await;
        let one = live_in_main(&shared).await;
        let two = live_in_main(&shared).await;
        shared.claim_main(one).await.unwrap();
        shared.claim_main(two).await.unwrap();

        let mut off = shared.settings();
        off.allow_several_in_main = false;
        assert!(
            !shared.replace_settings(off),
            "this setting needs no restart"
        );

        {
            let mut inner = shared.inner.write().await;
            if let Some(s) = inner.sessions.get_mut(&two) {
                s.set_state(State::Exited);
            }
        }
        shared.release_main(two).await;
        assert!(
            shared.claim_main(Uuid::new_v4()).await.is_err(),
            "`one` is still working in main, and sharing is off"
        );
        // And the session still in there may take the claim back.
        assert!(shared.claim_main(one).await.is_ok());
    }

    /// What must *not* be relaxed. `switch_main_to_pr` and the swap ask
    /// `main_in_use` before moving the checkout, and with several sessions the
    /// single `occupant` field can name none of the live ones — a released claim
    /// with somebody still working there would have read as an empty main.
    #[tokio::test]
    async fn a_session_in_main_holds_it_even_when_the_claim_names_nobody() {
        let shared = app_sharing_main().await;
        let one = live_in_main(&shared).await;
        let two = live_in_main(&shared).await;
        shared.claim_main(one).await.unwrap();
        shared.claim_main(two).await.unwrap();

        // The first one exits and hands the claim back, which it does by id.
        {
            let mut inner = shared.inner.write().await;
            if let Some(s) = inner.sessions.get_mut(&two) {
                s.set_state(State::Exited);
            }
        }
        shared.release_main(two).await;
        assert_eq!(shared.inner.read().await.workspaces[MAIN].occupant, None);

        assert_eq!(
            shared.main_in_use_now().await,
            Some(one),
            "somebody is still working in main, so the checkout must not move"
        );
    }

    /// The three cases `reclaim_main` has to tell apart.
    ///
    /// It exists because a relocation reuses the session id, so the outgoing
    /// session's exit watcher releases the claim the incoming one has already
    /// taken — leaving a live agent in main with no occupant recorded, which is
    /// what `switch_main_to_pr` reads before moving the checkout. Filling an empty
    /// slot repairs that. Taking one that somebody else holds would be a worse bug
    /// than the one it fixes, so it does not.
    #[tokio::test]
    async fn reclaiming_main_fills_an_empty_slot_and_never_takes_someone_elses() {
        let app = app().await;
        let (mine, theirs) = (Uuid::new_v4(), Uuid::new_v4());

        // The case this is for: the claim was dropped from under a live session.
        app.reclaim_main(mine).await;
        assert_eq!(app.inner.read().await.workspaces[MAIN].occupant, Some(mine));

        // Already ours, so nothing to do and nothing changes.
        app.reclaim_main(mine).await;
        assert_eq!(app.inner.read().await.workspaces[MAIN].occupant, Some(mine));

        // Held by somebody else: left alone, because that is a real conflict and
        // `claim_main` is the thing that refuses it.
        app.reclaim_main(theirs).await;
        assert_eq!(
            app.inner.read().await.workspaces[MAIN].occupant,
            Some(mine),
            "reclaim must never steal main"
        );
    }

    /// A stale occupant — the session gone but `release_main` not yet run — must
    /// read as unoccupied, the same answer `claim_main` gives, so the two never
    /// disagree about whether main is free.
    #[tokio::test]
    async fn a_dead_occupant_leaves_main_free() {
        let app = app().await;
        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), std::path::PathBuf::from("/tmp"), None);
            s.set_state(State::Exited); // gone, but still the recorded occupant
            inner.sessions.insert(id, s);
            if let Some(w) = inner.workspaces.get_mut(MAIN) {
                w.occupant = Some(id);
            }
        }
        assert_eq!(
            app.main_in_use_now().await,
            None,
            "a dead occupant does not hold main"
        );
        // And a claim succeeds, proving the two readers agree main is free.
        assert!(app.claim_main(Uuid::new_v4()).await.is_ok());
    }

    /// The one occupancy rule, in every workspace alike: live state with no pid
    /// yet occupies (the spawn window), live state with a dead pid does not (§8b),
    /// and a session auto-resume has not reached occupies until auto-resume ends.
    #[tokio::test]
    async fn occupancy_is_one_rule_for_every_workspace() {
        use std::sync::atomic::Ordering::SeqCst;

        // A pid that has certainly exited: a child we started and reaped.
        let dead = {
            let mut child = std::process::Command::new("true")
                .spawn()
                .expect("true runs");
            let pid = child.id();
            child.wait().expect("reaped");
            pid
        };
        let app = app().await;
        let (spawning, crashed, waiting) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        {
            let mut inner = app.inner.write().await;
            for (id, ws, state, pid) in [
                (spawning, "a", State::Starting, None),
                (crashed, "b", State::Working, Some(dead)),
                (waiting, "c", State::Exited, None),
            ] {
                let mut s =
                    Session::new(id, ws.to_string(), std::path::PathBuf::from("/tmp"), None);
                s.set_state(state);
                s.pid = pid;
                inner.sessions.insert(id, s);
            }
        }
        app.pending_resume
            .lock()
            .expect("the pending set")
            .insert(waiting);
        app.auto_resuming.store(true, SeqCst);

        assert_eq!(
            app.occupancy("a").await.live,
            vec![spawning],
            "a session the spawn has recorded holds its tree before its pty exists"
        );
        assert!(
            app.occupancy("b").await.is_empty(),
            "a live state over a dead process holds nothing"
        );
        assert_eq!(app.occupancy("c").await.holder(), Some(waiting));

        app.auto_resuming.store(false, SeqCst);
        assert!(
            app.occupancy("c").await.is_empty(),
            "a resume that never came holds nothing once auto-resume is over"
        );
    }

    /// Main is in use while a session in it is live, being resumed, or claimed and
    /// still starting — the window where only the park used to look, and `handle`
    /// or "open in main" could move main's branch under an agent coming up.
    #[tokio::test]
    async fn main_is_in_use_from_the_claim_not_from_the_insert() {
        let app = app().await;
        assert_eq!(app.main_in_use_now().await, None);

        let starting = Uuid::new_v4();
        let start = app.claim_main(starting).await.expect("main is free");
        assert_eq!(
            app.main_in_use_now().await,
            Some(starting),
            "a claimed session still building its environment holds main"
        );
        app.claim_main(Uuid::new_v4())
            .await
            .expect_err("a second claim must not overwrite one still starting");
        drop(start);
        app.release_main(starting).await;
        assert_eq!(
            app.main_in_use_now().await,
            None,
            "a finished start holds nothing"
        );

        // A resume keeps its id, so the stopped record is there while it starts —
        // the case reading "no record yet" as "starting" missed.
        let back = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(back, MAIN.to_string(), "/tmp".into(), None);
            s.set_state(State::Exited);
            inner.sessions.insert(back, s);
        }
        let resuming = app.claim_main(back).await.expect("main is free");
        assert_eq!(app.main_in_use_now().await, Some(back));
        drop(resuming);
        app.release_main(back).await;

        let live = live_in_main(&app).await;
        assert_eq!(app.main_in_use_now().await, Some(live));
    }

    /// A park reserves main in the same step as it finds main empty, and a claim
    /// waits until it is done — not refused, because closing the last session in
    /// main and opening another at once is ordinary. A claim already written, whose
    /// session is not inserted yet, keeps the park out — the window the park used to
    /// switch main in.
    #[tokio::test]
    async fn a_park_and_a_claim_cannot_both_have_main() {
        let app = app().await;
        let reserved = app.reserve_main_for_park().await.expect("main is empty");
        let claimant = Uuid::new_v4();
        let claim = {
            let app = app.clone();
            tokio::spawn(async move { app.claim_main(claimant).await.map(drop) })
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !claim.is_finished(),
            "a claim during the park must wait for it"
        );
        drop(reserved);
        claim
            .await
            .expect("the claim task ran")
            .expect("the park is over, so the waiting claim goes through");
        let start = app.claim_main(claimant).await.expect("its own claim again");
        assert!(
            app.reserve_main_for_park().await.is_none(),
            "a claim whose session is still being spawned holds main"
        );
        drop(start);
    }

    /// What a PR row offers, in the order a press asks: a run already going is
    /// "go to it", then someone else's branch and a branch you may not push to are
    /// refused, then another pass holding the tree, and only then is it open.
    #[tokio::test]
    async fn a_pr_row_offers_what_a_press_would_do() {
        let app = app().await;
        let pr = |n: u64, pushable: bool| crate::forge::Pr {
            head_ref: format!("feature/{n}"),
            head_repo: Some("acme/repo".into()),
            head_pushable: Some(pushable),
            ..crate::testutil::pr(n)
        };
        let mut inner = app.inner.write().await;
        inner.viewer = Some("me".into());
        assert_eq!(inner.press_for(&pr(1, true), None), Press::Open);
        assert!(
            matches!(inner.press_for(&pr(2, false), None), Press::Refused { why } if why.contains("cannot push")),
            "a branch you may not push to offered a button"
        );
        inner
            .foreign_heads
            .insert(3, "its head is someone else's".into());
        assert!(matches!(
            inner.press_for(&pr(3, true), None),
            Press::Refused { .. }
        ));

        // A review pane on #4 is "go to it", whatever else holds the branch.
        let pane = Uuid::new_v4();
        let mut s = Session::new(
            pane,
            "pr-4".into(),
            "/tmp".into(),
            Some(Pass {
                pr: 4,
                command: Pass::HANDLE_REVIEW.into(),
            }),
        );
        s.set_state(State::Working);
        inner.sessions.insert(pane, s);
        assert_eq!(
            inner.press_for(&pr(4, true), Some(pane)),
            Press::Running {
                session: pane,
                command: Pass::HANDLE_REVIEW.into()
            }
        );
        // The same pane holding #5's branch is another pass: refused by name.
        assert!(
            matches!(inner.press_for(&pr(5, true), Some(pane)), Press::Refused { why } if why.contains("handle-review")),
        );
    }

    /// The page is handed the holder the daemon refuses on, not the bare claim: a
    /// dead claimant holds nothing, and a live session the claim does not name
    /// holds main all the same.
    #[tokio::test]
    async fn the_snapshot_says_who_holds_main_by_the_daemons_rule() {
        let app = app().await;
        let held = |snap: Snapshot| {
            snap.workspaces
                .into_iter()
                .find(|w| w.is_main)
                .and_then(|w| w.held_by)
        };
        let dead = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(
                dead,
                MAIN.to_string(),
                std::path::PathBuf::from("/tmp"),
                None,
            );
            s.set_state(State::Exited);
            inner.sessions.insert(dead, s);
            if let Some(w) = inner.workspaces.get_mut(MAIN) {
                w.occupant = Some(dead);
            }
        }
        assert_eq!(
            held(app.snapshot().await),
            None,
            "a dead claim holds nothing"
        );

        let live = live_in_main(&app).await;
        assert_eq!(
            held(app.snapshot().await),
            Some(live),
            "unclaimed, and live"
        );
        assert_eq!(held(app.snapshot().await), app.main_in_use_now().await);
    }

    #[tokio::test]
    async fn an_agent_is_warned_once_then_allowed_through() {
        let app = app().await;
        let dir = crate::testutil::scratch("warn");
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

    /// The warning is about a file the session read before you rewrote it. A
    /// session that started *after* the rewrite read the new file, so it is owed
    /// nothing; it used to be refused its first edit of anything you had touched
    /// days earlier.
    #[tokio::test]
    async fn a_session_started_after_the_edit_is_not_warned() {
        let app = app().await;
        let dir = crate::testutil::scratch("warn-late");
        let f = dir.join("a.txt");
        std::fs::write(&f, "x").unwrap();

        app.record_human_edit(f.clone()).await;
        // Created after the edit, and recorded so the daemon knows when.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let late = Session::new(Uuid::new_v4(), MAIN.to_string(), dir.clone(), None);
        let late_id = late.id;
        app.inner.write().await.sessions.insert(late_id, late);
        assert!(
            app.claim_stale_warning(late_id, &f).await.is_none(),
            "a session that never saw the old file is not told about it"
        );

        // A session that predates the edit still is. Written straight into the map
        // with an old clock so the order is not left to timing.
        let mut early = Session::new(Uuid::new_v4(), MAIN.to_string(), dir.clone(), None);
        early.created_at = SystemTime::now() - std::time::Duration::from_secs(60);
        let early_id = early.id;
        app.inner.write().await.sessions.insert(early_id, early);
        assert!(app.claim_stale_warning(early_id, &f).await.is_some());

        // Old entries are pruned when a new one is recorded.
        {
            let mut inner = app.inner.write().await;
            let e = inner
                .human_edits
                .get_mut(&std::fs::canonicalize(&f).unwrap())
                .unwrap();
            e.at = SystemTime::now() - HUMAN_EDIT_TTL - std::time::Duration::from_secs(1);
        }
        app.record_human_edit(dir.join("b.txt")).await;
        assert!(
            !app.inner
                .read()
                .await
                .human_edits
                .contains_key(&std::fs::canonicalize(&f).unwrap()),
            "an edit past its TTL is forgotten"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The named lock: refused while held, free the moment the guard drops, and
    /// free after an early `?` return, which is the case the guard exists for.
    #[tokio::test]
    async fn a_claim_is_exclusive_and_released_by_its_guard() {
        let app = app().await;
        let held = app.try_claim("post:1").expect("first claim");
        assert!(app.try_claim("post:1").is_none(), "refused while held");
        assert!(
            app.try_claim("post:2").is_some(),
            "another name is independent"
        );
        drop(held);
        // Synchronous release: no scheduler hop between the drop and this claim.
        assert!(
            app.try_claim("post:1").is_some(),
            "free once the guard is dropped"
        );

        async fn bails_early(app: &Arc<AppState>) -> anyhow::Result<()> {
            let _claim = app
                .try_claim("post:3")
                .ok_or_else(|| anyhow::anyhow!("held"))?;
            Err::<(), _>(anyhow::anyhow!("something went wrong"))?;
            Ok(())
        }
        assert!(bails_early(&app).await.is_err());
        assert!(
            app.try_claim("post:3").is_some(),
            "the early return let go of it"
        );
    }

    /// A changeset larger than the pane can usefully list is cut down, and says so.
    ///
    /// The bound is on the *snapshot*, not the pane: `changed` is cloned per
    /// workspace into every snapshot and `notify` builds one per tool call, so an
    /// uncapped list is multiplied by the busiest loop in the daemon. A wiped
    /// repository put roughly 0.8 MB through that path several times a second.
    ///
    /// `changed_total` is asserted separately because the honesty is the point: a
    /// truncated list that reports its own length reads as a complete answer.
    #[tokio::test]
    async fn a_huge_changeset_is_capped_and_counted() {
        let dir = crate::testutil::scratch_repo("cap");
        let sh = |args: &[&str]| crate::testutil::git(&dir, args);
        std::fs::write(dir.join("seed"), "1").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-qm", "base"]);
        // The base the changeset is measured from. `upstream_ref` defaults to a
        // remote ref, so it is pointed at a local branch this test can make.
        sh(&["branch", "base"]);
        let n = CHANGED_CAP + 120;
        for i in 0..n {
            std::fs::write(dir.join(format!("f{i:05}.ts")), "x").unwrap();
        }
        sh(&["add", "-A"]);
        sh(&["commit", "-qm", "many"]);

        let app = crate::testutil::app_at(&dir, r#""upstream_ref":"base""#);
        app.reconcile(MAIN).await.expect("reconcile");

        let inner = app.inner.read().await;
        let tree = &inner.workspaces.get(MAIN).unwrap().tree;
        assert_eq!(tree.changed.len(), CHANGED_CAP, "the list is bounded");
        assert_eq!(
            tree.changed_total, n as u32,
            "and the real number survives it"
        );
        // Sorted before truncation, so the prefix is stable across reconciles
        // rather than whatever order git answered in.
        assert_eq!(tree.changed[0].path, "f00000.ts");

        drop(inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A tree says whether it has been measured, and the pane needs it.
    ///
    /// Every other field here defaults to something that reads as a real answer —
    /// no changed files is a clean tree, `(0,0)` divergence is up to date — which
    /// was harmless only while the first sweep finished before the window opened.
    /// It runs in the background now, so "not counted yet" has to be a state the
    /// snapshot can express or the pane reports a clean worktree it has never
    /// looked at.
    #[tokio::test]
    async fn a_tree_is_unmeasured_until_it_is_reconciled() {
        let dir = crate::testutil::scratch_repo("measured");
        let sh = |args: &[&str]| crate::testutil::git(&dir, args);
        std::fs::write(dir.join("seed"), "1").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-qm", "base"]);

        let app = crate::testutil::app_at(&dir, "");
        assert!(
            !app.inner
                .read()
                .await
                .workspaces
                .get(MAIN)
                .unwrap()
                .tree
                .measured,
            "a workspace starts having measured nothing"
        );

        app.reconcile(MAIN).await.expect("reconcile");
        assert!(
            app.inner
                .read()
                .await
                .workspaces
                .get(MAIN)
                .unwrap()
                .tree
                .measured,
            "and says so once it has"
        );
        // It reaches the SPA, which is the only place it is read.
        let snap = app.snapshot().await;
        let w = snap.workspaces.iter().find(|w| w.is_main).expect("main");
        assert!(w.measured, "the flag has to survive the view");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What a tree has checked out now, beside every branch it has ever held.
    ///
    /// The bug this pins: the rail asked `branches` whether main was free, and that
    /// set is never pruned — so one visit from any other branch made main look
    /// occupied for good, and a worktree row went on offering "swap branch with
    /// main" over a main already parked on its base. The two answers have to be
    /// separately readable, which is the whole of the fix.
    #[tokio::test]
    async fn a_workspace_says_what_it_holds_now_as_well_as_what_it_has_held() {
        let dir = crate::testutil::scratch_repo("branchnow");
        let sh = |args: &[&str]| crate::testutil::git(&dir, args);
        std::fs::write(dir.join("f"), "1").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-qm", "base"]);

        let app = crate::testutil::app_at(&dir, r#""upstream_ref":"main""#);

        app.reconcile(MAIN).await.expect("reconcile");
        {
            let inner = app.inner.read().await;
            let w = inner.workspaces.get(MAIN).unwrap();
            assert_eq!(w.tree.branch.as_deref(), Some("main"));
        }

        // Visit another branch and come back. `branches` keeps both for good, which
        // is deliberate; `branch` must follow the checkout.
        sh(&["switch", "-qc", "feature/x"]);
        app.reconcile(MAIN).await.expect("reconcile");
        sh(&["switch", "-q", "main"]);
        app.reconcile(MAIN).await.expect("reconcile");

        let inner = app.inner.read().await;
        let w = inner.workspaces.get(MAIN).unwrap();
        assert_eq!(
            w.tree.branch.as_deref(),
            Some("main"),
            "it is back on its base"
        );
        assert!(
            w.branches.contains("feature/x") && w.branches.contains("main"),
            "and it still remembers having held the other one"
        );
        drop(inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An idle workspace's changed list stays out of the snapshot and is flagged,
    /// so the pane fetches it; a workspace with a live session keeps its list.
    /// The lists were 486 KB of a 775 KB snapshot pushed on every change.
    #[tokio::test]
    async fn an_idle_workspace_leaves_its_changed_list_out_of_the_snapshot() {
        let app = app().await;
        let path = std::env::temp_dir().join("orchd-omit");
        let file = || {
            crate::model::DiffFile::untracked(&crate::model::ChangedFile {
                path: "a.rs".into(),
                status: crate::model::FileStatus::Untracked,
                code: "??".into(),
            })
        };
        for ws in ["idle", "busy"] {
            app.register_worktree(ws, path.clone(), None).await;
        }
        {
            let mut inner = app.inner.write().await;
            for ws in ["idle", "busy"] {
                let w = inner.workspaces.get_mut(ws).unwrap();
                w.tree.changed = vec![file()];
                w.tree.measured = true;
            }
            let mut s =
                crate::model::Session::new(uuid::Uuid::new_v4(), "busy".into(), path.clone(), None);
            s.state = crate::model::State::Working;
            inner.sessions.insert(s.id, s);
        }
        let snap = app.snapshot().await;
        let by = |id: &str| snap.workspaces.iter().find(|w| w.id == id).unwrap();
        assert!(by("idle").changed.is_empty(), "the idle list is left out");
        assert!(by("idle").changed_omitted, "and says so, so the pane asks");
        assert_eq!(by("busy").changed.len(), 1, "a live one keeps its list");
        assert!(!by("busy").changed_omitted);
    }

    /// Worktree ids are deterministic and reused (`pr-<n>`), so the measurements
    /// have to die with the workspace. They used to live in maps keyed by id
    /// beside it, and teardown removed only the workspace — so recreating the
    /// same id served the previous incarnation's file list and counts.
    #[tokio::test]
    async fn a_recreated_workspace_does_not_inherit_the_old_ones_measurements() {
        let app = app().await;
        let path = std::env::temp_dir().join("orchd-tree-reuse");

        app.register_worktree("pr-1", path.clone(), Some("feat/a".into()))
            .await;
        {
            let mut inner = app.inner.write().await;
            let w = inner.workspaces.get_mut("pr-1").unwrap();
            w.tree.changed = vec![crate::model::DiffFile::untracked(
                &crate::model::ChangedFile {
                    path: "only-in-the-old-one.rs".into(),
                    status: crate::model::FileStatus::Untracked,
                    code: "??".into(),
                },
            )];
            w.tree.base = Some("deadbeef".into());
            w.tree.divergence = (3, 4);
            w.tree.rebasing = true;
        }

        // Re-registering a workspace that is still there keeps what was measured
        // — the same live worktree, not a new one.
        app.register_worktree("pr-1", path.clone(), None).await;
        {
            let inner = app.inner.read().await;
            assert_eq!(inner.workspaces["pr-1"].tree.divergence, (3, 4));
        }

        // Teardown takes the workspace, and with it the tree.
        {
            let mut inner = app.inner.write().await;
            inner.workspaces.remove("pr-1");
        }
        app.register_worktree("pr-1", path, Some("feat/b".into()))
            .await;

        let inner = app.inner.read().await;
        let tree = &inner.workspaces["pr-1"].tree;
        assert!(tree.changed.is_empty(), "stale file list survived teardown");
        assert_eq!(tree.base, None, "stale base survived teardown");
        assert_eq!(tree.divergence, (0, 0), "stale counts survived teardown");
        assert!(!tree.rebasing, "stale rebase flag survived teardown");
    }

    /// **A branch a tree once held is not a branch it holds.**
    ///
    /// Main's branch set only ever grows, so a PR whose head main once visited
    /// stays in it for good. That used to decide where a PR's branch was — the PR row then
    /// pointed at whoever was in main next — and only one word of one closure kept
    /// it from answering [`AppState::worktree_holding`] too, which would have aimed
    /// a fix or a review run at the checkout every worktree is cut from.
    #[tokio::test]
    async fn a_branch_is_held_by_the_tree_that_has_it_checked_out_now() {
        let app = app().await;
        {
            let mut inner = app.inner.write().await;
            let main = inner
                .workspaces
                .get_mut(MAIN)
                .expect("main is always there");
            main.branches.insert("feature/x".to_string());
            main.tree.branch = Some("develop".to_string());
        }
        let held = |app: &Arc<AppState>| {
            let app = app.clone();
            async move { app.inner.read().await.workspace_on("feature/x", |_| true) }
        };
        assert_eq!(held(&app).await, None, "main left it");

        // On it now: main has it, and is still never the worktree for it.
        app.inner
            .write()
            .await
            .workspaces
            .get_mut(MAIN)
            .expect("main")
            .tree
            .branch = Some("feature/x".to_string());
        assert_eq!(held(&app).await.as_deref(), Some(MAIN));
        assert_eq!(
            app.worktree_holding("feature/x").await,
            None,
            "a run would have been sent into main"
        );

        // And a worktree that really holds it answers both, from the branch it
        // registered with, before any sweep has measured it.
        app.register_worktree(
            "pr-7",
            std::path::PathBuf::from("/tmp/pr-7"),
            Some("feature/x".to_string()),
        )
        .await;
        assert_eq!(
            app.worktree_holding("feature/x").await.as_deref(),
            Some("pr-7")
        );
    }

    /// The PR row's session is the one working on the PR's branch, not whoever
    /// sits in the tree that used to hold it — the report this was written from:
    /// main parked off a PR branch, a new session opened in main on another one,
    /// and the row's `session` button went to it.
    #[tokio::test]
    async fn a_pr_points_at_the_session_on_its_branch_not_the_one_in_its_old_tree() {
        let app = app().await;
        let newcomer = live_in_main(&app).await;
        {
            let mut inner = app.inner.write().await;
            let main = inner.workspaces.get_mut(MAIN).expect("main");
            main.branches.insert("story-1".to_string());
            main.tree.branch = Some("story-2".to_string());
            inner
                .sessions
                .get_mut(&newcomer)
                .expect("the session")
                .branch = Some("story-2".to_string());
            inner.prs.push(crate::forge::Pr {
                head_ref: "story-1".into(),
                ..crate::testutil::pr(1)
            });
        }
        let snap = app.snapshot().await;
        let row = snap.prs.iter().find(|p| p.pr.number == 1).expect("the PR");
        assert_eq!(row.workspace, None, "no tree has story-1 checked out");
        assert_eq!(row.session, None, "and nobody is working on it");

        // Once the newcomer's own branch has a PR, that PR is the one it points at.
        app.inner.write().await.prs.push(crate::forge::Pr {
            head_ref: "story-2".into(),
            ..crate::testutil::pr(2)
        });
        let snap = app.snapshot().await;
        let row = snap.prs.iter().find(|p| p.pr.number == 2).expect("the PR");
        assert_eq!(row.workspace.as_deref(), Some(MAIN));
        assert_eq!(row.session, Some(newcomer));
    }

    /// A session keeps the PR its branch had, after the PR leaves the open list.
    ///
    /// The archive row reads it weeks later, when the forge answers for open PRs
    /// only and the daemon's ended list holds ten. A head another repository owns
    /// on a same-named branch is not this session's PR.
    #[tokio::test]
    async fn a_session_keeps_the_pr_of_its_branch_after_the_pr_is_gone() {
        let app = app().await;
        let id = live_in_main(&app).await;
        let mut inner = app.inner.write().await;
        inner.sessions.get_mut(&id).expect("the session").branch = Some("story-7".into());

        // Someone else's fork, on a branch of the same name: not ours.
        inner.prs = vec![crate::forge::Pr {
            head_ref: "story-7".into(),
            ..crate::testutil::pr(70)
        }];
        inner.foreign_heads.insert(70, "another repository".into());
        inner.note_session_prs();
        assert_eq!(
            inner.sessions[&id].pr, None,
            "a foreign head is not this session's PR"
        );

        inner.foreign_heads.clear();
        inner.prs = vec![crate::forge::Pr {
            head_ref: "story-7".into(),
            ..crate::testutil::pr(7)
        }];
        inner.note_session_prs();
        assert_eq!(inner.sessions[&id].pr, Some(7));

        // Merged: the poll no longer lists it, and the session still says #7.
        inner.prs.clear();
        inner.note_session_prs();
        assert_eq!(
            inner.sessions[&id].pr,
            Some(7),
            "the PR outlives the open list"
        );
    }

    /// Stack notes go to both sides, once per pair and lower head, and an upper
    /// branch built on two hears only about the nearer one. The test for the
    /// flood: a branch counted as stacked on several was told all of them again
    /// on every refresh.
    #[tokio::test]
    async fn a_stack_is_said_once_per_pair_and_move_to_both_sides() {
        let app = app().await;
        let mut inner = app.inner.write().await;
        let add = |inner: &mut Inner, ws: &str| {
            inner.workspaces.insert(
                ws.to_string(),
                crate::model::Workspace {
                    id: ws.to_string(),
                    path: format!("/tmp/{ws}").into(),
                    kind: crate::model::WorkspaceKind::Worktree {
                        name: ws.to_string(),
                    },
                    branches: Default::default(),
                    processes: Vec::new(),
                    occupant: None,
                    tree: Default::default(),
                    banked: None,
                },
            );
            let id = Uuid::new_v4();
            inner.sessions.insert(
                id,
                Session::new(id, ws.to_string(), format!("/tmp/{ws}").into(), None),
            );
            id
        };
        add(&mut inner, "base");
        add(&mut inner, "middle");
        add(&mut inner, "top");
        let stack = |lower_is_a: bool, tip: &str, commits: u32| crate::overlap::Stack {
            lower_is_a,
            behind: 0,
            lower_tip: tip.into(),
            anchor: tip.into(),
            lower_commits: commits,
        };
        // `top` is built on `middle`, which is built on `base`; `top` carries
        // `base`'s commits too, so it is in a stack with both.
        inner
            .stacks
            .insert(("base".into(), "middle".into()), stack(true, "b1", 5));
        inner
            .stacks
            .insert(("base".into(), "top".into()), stack(true, "b1", 5));
        inner
            .stacks
            .insert(("middle".into(), "top".into()), stack(true, "m1", 6));

        let top = inner.stack_notes("top", false);
        assert_eq!(
            top.len(),
            1,
            "only the nearer of its two lower branches: {top:?}"
        );
        assert!(top[0].contains("stacked on `middle`"));
        assert!(inner.stack_notes("top", false).is_empty(), "and only once");

        let base = inner.stack_notes("base", false);
        assert_eq!(
            base.len(),
            2,
            "the lower side hears about each branch on it: {base:?}"
        );
        assert!(base.iter().all(|n| n.contains("is stacked on your branch")));

        // The lower branch moves: said again, once.
        inner
            .stacks
            .get_mut(&("middle".to_string(), "top".to_string()))
            .unwrap()
            .lower_tip = "m2".into();
        assert_eq!(inner.stack_notes("top", false).len(), 1);
        assert!(inner.stack_notes("top", false).is_empty());
    }

    /// A merge tells the other side of each overlap to rebase, and nobody else.
    #[tokio::test]
    async fn a_merge_tells_the_other_side_of_an_overlap_to_rebase() {
        let app = app().await;
        let merged_side = live_in_main(&app).await;
        let mut inner = app.inner.write().await;
        inner.workspaces.get_mut(MAIN).expect("main").tree.branch = Some("story-1".into());
        let other = "feature-x".to_string();
        inner.workspaces.insert(
            other.clone(),
            crate::model::Workspace {
                id: other.clone(),
                path: "/tmp/x".into(),
                kind: crate::model::WorkspaceKind::Worktree {
                    name: other.clone(),
                },
                branches: Default::default(),
                processes: Vec::new(),
                occupant: None,
                tree: Default::default(),
                banked: None,
            },
        );
        let there = Uuid::new_v4();
        inner.sessions.insert(
            there,
            Session::new(there, other.clone(), "/tmp/x".into(), None),
        );
        inner.overlaps = vec![crate::overlap::Pair {
            a: MAIN.into(),
            b: other.clone(),
            found: crate::overlap::Found {
                path: "src/a.rs".into(),
                conflict: true,
                stamp: [0; 5],
            },
        }];
        let ended = crate::forge::EndedPr {
            number: 11,
            title: "the first half".into(),
            url: String::new(),
            head_ref: "story-1".into(),
            outcome: crate::forge::PrOutcome::Merged,
        };

        let told = inner.tell_overlaps_merged(&ended, "upstream/develop");
        assert_eq!(
            told,
            [there],
            "the other side, not the session whose PR merged"
        );
        let note = inner.sessions[&there].inbox.join("");
        assert!(
            note.contains("#11") && note.contains("src/a.rs") && note.contains("upstream/develop")
        );
        assert!(inner.sessions[&merged_side].inbox.is_empty());

        // A PR on a branch no workspace holds tells nobody.
        let elsewhere = crate::forge::EndedPr {
            head_ref: "gone".into(),
            ..ended.clone()
        };
        assert!(inner
            .tell_overlaps_merged(&elsewhere, "upstream/develop")
            .is_empty());

        // And a file that merges cleanly is no reason to stop and rebase.
        inner.overlaps[0].found.conflict = false;
        assert!(inner
            .tell_overlaps_merged(&ended, "upstream/develop")
            .is_empty());
    }

    /// A session that moved on to a second branch keeps the PR of its first.
    ///
    /// `branch` follows the tree, so matching on it alone dropped the first PR the
    /// moment the agent cut the next branch, and the rail fell back to whatever
    /// had merged. The list keeps both, in the order they were opened, and an end
    /// is written onto the one that ended.
    #[tokio::test]
    async fn a_session_keeps_every_pr_of_the_branches_it_held() {
        let app = app().await;
        let id = live_in_main(&app).await;
        let mut inner = app.inner.write().await;
        {
            let s = inner.sessions.get_mut(&id).expect("the session");
            s.branches = vec!["story-1".into(), "story-2".into()];
            s.branch = Some("story-2".into());
        }
        inner.prs = vec![
            crate::forge::Pr {
                head_ref: "story-2".into(),
                ..crate::testutil::pr(12)
            },
            crate::forge::Pr {
                head_ref: "story-1".into(),
                ..crate::testutil::pr(11)
            },
            crate::forge::Pr {
                head_ref: "someone-else".into(),
                ..crate::testutil::pr(99)
            },
        ];
        inner.note_session_prs();
        let numbers: Vec<u64> = inner.sessions[&id].prs.iter().map(|p| p.number).collect();
        assert_eq!(
            numbers,
            [11, 12],
            "both of its PRs, oldest first, and no other"
        );
        assert_eq!(inner.sessions[&id].pr, Some(12));

        // The first one merges and leaves the poll.
        inner.prs.retain(|p| p.number != 11);
        inner.remember_ended(crate::forge::EndedPr {
            number: 11,
            title: "first".into(),
            url: String::new(),
            head_ref: "story-1".into(),
            outcome: crate::forge::PrOutcome::Merged,
        });
        inner.note_session_prs();
        let prs = &inner.sessions[&id].prs;
        assert_eq!(prs.len(), 2, "an ended PR stays on the list");
        assert_eq!(prs[0].outcome, Some(crate::forge::PrOutcome::Merged));
        assert_eq!(prs[1].outcome, None, "the second is still open");
    }

    /// A PR that ended while the daemon was down is asked about once.
    ///
    /// The ended list is memory and only sees a PR leave between two polls, so a
    /// session whose PR merged overnight came back with no PR at all. A record from
    /// before the list starts it from `pr`.
    #[tokio::test]
    async fn a_pr_that_ended_while_down_is_asked_about_once() {
        let app = app().await;
        let id = live_in_main(&app).await;
        let mut inner = app.inner.write().await;
        // Only the number survived, from a record older than the list.
        inner.sessions.get_mut(&id).expect("the session").pr = Some(40);
        inner.note_session_prs();
        assert_eq!(inner.sessions[&id].prs.len(), 1, "started from `pr`");

        let mut asked = HashSet::new();
        let open = [crate::testutil::pr(41)];
        assert_eq!(inner.session_prs_to_ask(&open, &mut asked), [40]);
        assert!(
            inner.session_prs_to_ask(&open, &mut asked).is_empty(),
            "once per run, not once per poll"
        );

        inner.remember_ended(crate::forge::EndedPr {
            number: 40,
            title: "the old one".into(),
            url: "https://example.test/40".into(),
            head_ref: "story-40".into(),
            outcome: crate::forge::PrOutcome::Merged,
        });
        let p = &inner.sessions[&id].prs[0];
        assert_eq!(p.outcome, Some(crate::forge::PrOutcome::Merged));
        assert_eq!(
            p.title, "the old one",
            "the answer names a PR known only by number"
        );
    }

    /// A PR that opened and ended between runs is found by its branch.
    ///
    /// No number was ever written down for it, so the branch is the only question
    /// left to ask. Asked once per run, never for a branch with an open PR, and the
    /// answer lands on the session in number order.
    #[tokio::test]
    async fn an_ended_pr_is_found_by_the_branch_it_was_on() {
        let app = app().await;
        let id = live_in_main(&app).await;
        let mut inner = app.inner.write().await;
        {
            let s = inner.sessions.get_mut(&id).expect("the session");
            s.branches = vec!["story-1".into(), "story-2".into()];
            s.branch = Some("story-2".into());
        }
        inner.prs = vec![crate::forge::Pr {
            head_ref: "story-2".into(),
            ..crate::testutil::pr(22)
        }];
        inner.note_session_prs();

        let mut asked = HashSet::new();
        let fresh = inner.prs.clone();
        assert_eq!(
            inner.session_heads_to_ask(&fresh, &mut asked),
            ["story-1"],
            "the branch with an open PR needs no question"
        );
        assert!(inner.session_heads_to_ask(&fresh, &mut asked).is_empty());

        inner.note_ended_on_head(crate::forge::EndedPr {
            number: 21,
            title: "first".into(),
            url: "u21".into(),
            head_ref: "story-1".into(),
            outcome: crate::forge::PrOutcome::Merged,
        });
        let numbers: Vec<u64> = inner.sessions[&id].prs.iter().map(|p| p.number).collect();
        assert_eq!(numbers, [21, 22], "in the order they were opened");
        assert_eq!(
            inner.sessions[&id].prs[0].outcome,
            Some(crate::forge::PrOutcome::Merged)
        );
    }
}
