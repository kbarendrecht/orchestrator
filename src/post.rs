//! The post batch — the one irreversible action in the review flow.
//!
//! Split at the push, because that is where the consequences change kind.
//! Everything before it is local and undoable: [`crate::patch::write_batch`] runs
//! the check ladder, applies, runs the hooks and folds, and any refusal leaves the
//! branch exactly as it was. Everything after it is public. So the report
//! distinguishes **cannot be unsent** from **still pending** rather than
//! collapsing both into one error.
//!
//! Two properties this module exists to hold:
//!
//! - **The payload cannot carry code.** The SPA sends thread ids and position
//!   *indices*; every patch comes out of `Inner.proposals`, which is what the
//!   human actually read. A client echoing content back could otherwise substitute
//!   a different diff than the one that was reviewed.
//! - **Retry is not a second code path.** Nothing is remembered about what landed;
//!   what is missing is re-derived from a fresh fetch. A ledger can be killed
//!   between the call succeeding and the write, and would then lie in exactly the
//!   case it exists for.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

use crate::forge::{Pr, ThreadRoot, Threads};
use crate::forge::{self, Forge, ForgeImpl};
use crate::patch::{FileStat, Patch, Written};
use crate::proposal::{Mode, Position, Stance};
use crate::state::AppState;

/// One handled thread: which position, and your wording if you changed it.
#[derive(Debug, Clone, Deserialize)]
pub struct Decision {
    pub thread_id: String,
    /// Index into the proposal's `positions`. Never the content — see the module
    /// doc.
    pub position: usize,
    /// Overrides the drafted reply. `None` keeps what triage wrote.
    #[serde(default)]
    pub reply: Option<String>,
    /// Who writes the code this decision implies. Defaults to the agent, so a
    /// payload from before the mode split still means what it did.
    #[serde(default)]
    pub mode: Mode,
}

/// What the manual phase sends back to finish the batch.
///
/// It carries the same `decisions` again rather than the daemon holding them: the
/// durable half of the phase is the commit, which is in git, and re-sending means
/// there is no server-side state to go stale between the two halves.
#[derive(Debug, Clone, Deserialize)]
pub struct Finish {
    pub batch: Batch,
    /// The sha the phase reported. Checked against `HEAD`.
    pub committed: String,
    /// The comment per manual thread. Required, and non-empty — without one a
    /// Manual thread does nothing on GitHub and is indistinguishable from a skip,
    /// except in your own head. The reviewer would get a commit and silence.
    pub comments: std::collections::HashMap<String, String>,
    /// The paths the phase screen showed you, which is what you pressed the button
    /// under. Anything dirty and not in here is refused rather than swept into the
    /// commit — see `patch::write_manual`.
    #[serde(default)]
    pub files: Vec<String>,
}

/// What the final screen sends. A thread absent from `decisions` was skipped.
#[derive(Debug, Clone, Deserialize)]
pub struct Batch {
    /// The head the proposals were generated against, re-checked here: a
    /// force-push in between invalidates every patch.
    pub base_sha: String,
    pub decisions: Vec<Decision>,
}

/// A single outward write, named the way the report renders it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum What {
    /// Filed in the tracker. First in the enum because it is first in the
    /// sequence: the id has to exist before the reply that carries it.
    Story,
    Reply,
    ThumbsUp,
    Rerequest,
}

/// Something that is now true on GitHub.
#[derive(Debug, Clone, Serialize)]
pub struct Landed {
    pub thread_id: String,
    /// `renovate.json5:161 · carol`, for the report's left column.
    pub label: String,
    pub what: What,
    /// It was already there, so nothing was sent. The distinction matters on a
    /// retry: "posted" and "already posted" read the same to a reviewer but not
    /// to someone deciding whether the retry worked.
    pub already: bool,
    /// Set on a `Story` row. Not a bare id on the struct, which would be
    /// meaningless next to a reply or a reaction — and the report needs the URL
    /// too, to show what the reply will actually link to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub story: Option<crate::story::StoryRef>,
}

/// A write that was attempted and refused. `error` is `gh`'s own words.
#[derive(Debug, Clone, Serialize)]
pub struct Failed {
    pub thread_id: String,
    pub label: String,
    pub what: What,
    pub error: String,
}

/// A write that was never tried, and what it is waiting on.
#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub label: String,
    pub what: What,
    pub waiting_on: String,
}

/// What the failure panel renders.
///
/// `refused` is the local half saying no — the ladder found a stale patch, the
/// hooks rewrote a file, pre-commit failed. Nothing was committed and nothing was
/// pushed, so every other field is empty and the screen is panel 7 rather than
/// panel 8.
/// A thread the human said they would handle themselves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualThread {
    pub thread_id: String,
    pub label: String,
    /// The reviewer's own words, so the phase screen needs no second fetch.
    pub comment: String,
    /// What the card's box held. A starting point, not the comment — you cannot
    /// describe work you have not done yet, which is why the real one is written
    /// in the phase.
    pub draft: String,
}

/// The batch stopped to wait for you.
///
/// Reached only when a decision chose `Manual`. The accepted patches are written
/// and committed by then, so you edit a tree that already reflects every other
/// decision — often *why* this thread needed hands. **Nothing has been pushed and
/// nothing posted**, so backing out costs only the local commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualPhase {
    /// The commit the accepted patches landed in. `/manual/done` checks `HEAD`
    /// against it, which is what keeps the phase from resuming onto a branch that
    /// moved underneath it.
    ///
    /// Kept in step with the worktree by `update_phase_head` and written to disk with
    /// the rest of the phase, because `fold_in` rewrites shas in both its arms — after
    /// a fold the old sha is not even an ancestor of `HEAD`, so nothing can re-derive
    /// which commit was ours.
    pub committed: String,
    /// What was already written, for the phase screen's first line.
    pub files: Vec<FileStat>,
    pub amend: Option<String>,
    pub threads: Vec<ManualThread>,
    /// A digest of the decisions half one resolved.
    ///
    /// The resume re-supplies the whole batch and the daemon re-resolves it from
    /// scratch, with only `committed == HEAD` checked — and that says nothing about
    /// *which* decisions produced that commit. A decision half one never saw would
    /// otherwise post a reply describing code that was never applied.
    pub decisions: String,
}

/// A stable fingerprint of what a batch decided.
///
/// Only the parts that change what gets written or posted: the thread, the position
/// index, and whether the wording was overridden. Sorted, because the client's order
/// is not a decision.
///
/// **FNV-1a rather than `DefaultHasher`.** The digest is written to disk with the
/// phase, so it has to mean the same thing in the next process — and `Hasher`'s output
/// is explicitly not guaranteed stable across Rust releases. A toolchain upgrade would
/// otherwise refuse every in-flight phase, and the message would blame the decisions.
fn digest_of(batch: &Batch) -> String {
    let mut parts: Vec<String> = batch
        .decisions
        .iter()
        .map(|d| {
            format!(
                "{}:{}:{}",
                d.thread_id,
                d.position,
                // A distinct marker for "not overridden", so an empty override and no
                // override are not the same fingerprint.
                d.reply.as_deref().unwrap_or("\u{1}none")
            )
        })
        .collect();
    parts.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in batch
        .base_sha
        .as_bytes()
        .iter()
        .chain(parts.join("\n").as_bytes())
    {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PostReport {
    pub refused: Option<String>,
    /// Whether pressing the same button again could succeed once you have acted on
    /// `refused`.
    ///
    /// A stray file or a failing hook is something you fix and retry; the branch
    /// having moved under an open phase is not — that sha will never match again, so
    /// restoring the phase for it pins you to a screen whose only button is
    /// guaranteed to fail. The SPA needs to tell those apart and the message alone
    /// cannot.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub retryable: bool,
    /// Set when the batch is waiting on you. Everything else is empty: the outward
    /// half has not run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual: Option<ManualPhase>,
    /// The complete file list the batch wrote, from `git apply --numstat`.
    pub files: Vec<FileStat>,
    /// How the fold resolved, including when it fell back to a HEAD amend.
    pub amend: Option<String>,
    /// The sha now on origin. `Some` means the code cannot be taken back.
    pub pushed: Option<String>,
    pub landed: Vec<Landed>,
    pub failed: Vec<Failed>,
    pub skipped: Vec<Skipped>,
    pub rerequested: Vec<String>,
    pub held_back: Vec<Skipped>,
}

impl PostReport {
    /// A refusal you can act on and press again.
    fn refused(why: impl Into<String>) -> Self {
        PostReport {
            refused: Some(why.into()),
            retryable: true,
            ..Default::default()
        }
    }

    /// A refusal that ends the attempt: pressing again cannot succeed.
    fn refused_finally(why: impl Into<String>) -> Self {
        PostReport {
            refused: Some(why.into()),
            retryable: false,
            ..Default::default()
        }
    }

    /// Did anything at all fail to land? Drives the header's "posted with errors".
    pub fn ok(&self) -> bool {
        self.refused.is_none() && self.failed.is_empty()
    }
}

/// One resolved decision: the position, plus what it is aimed at.
#[derive(Debug)]
pub(crate) struct Handled {
    thread_id: String,
    label: String,
    root: ThreadRoot,
    stance: Stance,
    mode: Mode,
    patch: Option<String>,
    reply: Option<String>,
    /// The story to file, for a `story+reply` position. Taken from the proposal
    /// the human read, never from the payload — same rule as the patch.
    story: Option<crate::proposal::StoryDraft>,
    /// The thread's own GitHub URL, which goes into the story body so a later run
    /// can find the story instead of filing a second one.
    permalink: String,
    /// The reviewer's opening comment, and what the card's box held. Both only for
    /// the manual phase screen, which has to show the words being answered and the
    /// draft to finish — and carrying them here saves it a second fetch.
    reviewer_said: String,
    draft: String,
    /// `(path, line)` for the blame that picks the fixup target. Only for a
    /// position that writes code.
    touched: Option<(String, u32)>,
}

/// Turn the payload into resolved work, refusing anything the daemon cannot do.
///
/// Every lookup here is a containment check: the position must exist in the
/// proposal the human read, the thread must be in the fresh fetch, and the
/// comment id comes from [`Threads::root_for`] rather than from the payload.
pub(crate) fn resolve(
    positions: &crate::proposal::ProposalSet,
    fresh: &Threads,
    batch: &Batch,
    tracker: crate::config::Tracker,
    // `None` on the first half, `Some` on the resume.
    //
    // Deliberately an `Option` around the map rather than an empty map: with a map
    // either way, an *absent* key is indistinguishable from "the phase has not
    // opened yet", and the resume path then treats a manual thread with no comment
    // as one that simply has nothing to post — sailing past the requirement and
    // posting every other reply. That is not hypothetical; it is what this code did,
    // and it posted four replies to a live PR before the mistake was visible.
    comments: Option<&std::collections::HashMap<String, String>>,
) -> Result<Vec<Handled>> {
    let mut out = Vec::new();
    for d in &batch.decisions {
        let proposal = positions
            .proposals
            .iter()
            .find(|p| p.thread_id == d.thread_id)
            .with_context(|| format!("no triage proposal for thread {}", d.thread_id))?;
        let pos: &Position = proposal.positions.get(d.position).with_context(|| {
            format!(
                "thread {}: position {} of {}",
                d.thread_id,
                d.position,
                proposal.positions.len()
            )
        })?;

        if pos.stance.files_story() && !tracker.is_configured() {
            bail!(
                "thread {}: no tracker is configured, so there is nowhere to file this. \
                 Set `tracker` in the config, pick another position, or skip.",
                d.thread_id
            );
        }

        let thread = fresh
            .items
            .iter()
            .find(|t| t.id == d.thread_id)
            .with_context(|| {
                format!(
                    "thread {} is no longer among this PR's threads — re-triage",
                    d.thread_id
                )
            })?;
        let root = fresh
            .root_for(&d.thread_id)
            .with_context(|| format!("thread {} has no comment to answer", d.thread_id))?;

        let touched = if d.mode == Mode::Manual {
            // The reviewer's own anchor, used to blame at in the phase. Absent on a
            // review summary, and that is allowed: answering one by hand is
            // legitimate, and with no line the fold degrades to a HEAD amend and
            // says so rather than refusing.
            match (thread.path.clone(), thread.line.or(thread.original_line)) {
                (Some(p), Some(l)) => Some((p, l)),
                _ => None,
            }
        } else if pos.writes_code() {
            let path = thread.path.clone().with_context(|| {
                format!(
                    "thread {}: a patch was accepted on a thread with no file",
                    d.thread_id
                )
            })?;
            // An outdated thread has no current `line`; blame falls back to where
            // the reviewer was looking. If that lands on a different commit than
            // the batch's other patches, `amend_target` degrades to a HEAD amend
            // and says so, which is the right outcome rather than a guess.
            let line = thread
                .line
                .or(thread.original_line)
                .with_context(|| format!("thread {}: a patch with no line", d.thread_id))?;
            Some((path, line))
        } else {
            None
        };

        // A manual thread's comment is written in the phase, after the work exists —
        // so on the way *in* the card's box is only a draft and may be empty. The
        // requirement is enforced where it can actually be met.
        let reply = match (d.mode, pos.stance.writes_reply(), comments) {
            // Manual and wordless. Now that mode is its own axis this is
            // reachable: agreeing with the reviewer and fixing it by hand posts a
            // thumbs up and nothing else, so there is no comment to demand.
            (Mode::Manual, false, _) => None,
            // The first half: the phase has not opened, so there is nothing to post
            // yet and the card's box was only a draft.
            (Mode::Manual, true, None) => None,
            // The resume. A comment is required, and *absent* has to fail exactly
            // like blank — anything else lets the thread through in silence.
            (Mode::Manual, true, Some(map)) => {
                let text = map.get(&d.thread_id).map(String::as_str).unwrap_or("");
                if text.trim().is_empty() {
                    bail!(
                        "thread {}: a manual thread needs a comment. Without one the reviewer \
                         gets a commit and silence, which is indistinguishable from being \
                         ignored.",
                        d.thread_id
                    );
                }
                Some(text.to_string())
            }
            _ => resolve_reply(pos, d, &d.thread_id)?,
        };
        // The one agent-or-human string the validator never sized. It is about to
        // carry a public URL, so it is bounded here rather than at the API edge.
        if let Some(r) = &reply {
            anyhow::ensure!(
                r.len() <= crate::proposal::MAX_FIELD,
                "thread {}: the reply exceeds {} bytes",
                d.thread_id,
                crate::proposal::MAX_FIELD
            );
        }
        // `{story}` is what the id is substituted into, so a story reply without
        // it would file a story and never link to it. The draft is validated on
        // arrival; this catches the human deleting the token while editing. Refused
        // here, which is before the local half and long before anything is filed.
        if pos.stance.files_story() {
            let text = reply.as_deref().unwrap_or_default();
            if !text.contains(crate::proposal::STORY_TOKEN) {
                bail!(
                    "thread {}: this reply files a story but no longer contains {} — \
                     it would be filed with nothing linking to it. Put the token back, \
                     or pick another position.",
                    d.thread_id,
                    crate::proposal::STORY_TOKEN
                );
            }
        } else if let Some(text) = &reply {
            // The reverse: a token in a position that files nothing would post the
            // literal braces to GitHub.
            if text.contains(crate::proposal::STORY_TOKEN) {
                bail!(
                    "thread {}: this reply contains {} but the position files no story, \
                     so there would be no id to put there",
                    d.thread_id,
                    crate::proposal::STORY_TOKEN
                );
            }
        }

        out.push(Handled {
            thread_id: d.thread_id.clone(),
            label: label_for(thread),
            root,
            stance: pos.stance,
            mode: d.mode,
            // A staged fix belongs to the agent. Under `Manual` you are writing it
            // yourself, so carrying the patch here would apply it behind you.
            patch: pos.patch.clone().filter(|_| d.mode == Mode::Agent),
            reply,
            story: pos.story.clone().filter(|_| pos.stance.files_story()),
            permalink: thread
                .comments
                .first()
                .map(|c| c.url.clone())
                .unwrap_or_default(),
            reviewer_said: thread
                .comments
                .first()
                .map(|c| c.body.clone())
                .unwrap_or_default(),
            draft: pos.reply.clone().unwrap_or_default(),
            touched,
        });
    }
    Ok(out)
}

/// The reply a non-manual position will post: your wording if you typed one.
fn resolve_reply(pos: &Position, d: &Decision, thread_id: &str) -> Result<Option<String>> {
    match (pos.stance.writes_reply(), &d.reply) {
        // Your wording wins over the draft. Empty is a refusal, not a blank
        // comment: a reply-only position that posts nothing does nothing.
        (true, Some(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
        (true, Some(_)) => {
            bail!("thread {thread_id}: this position posts a reply and the text is empty")
        }
        (true, None) => Ok(Some(
            pos.reply
                .clone()
                .filter(|r| !r.trim().is_empty())
                .with_context(|| format!("thread {thread_id}: no reply text to post"))?,
        )),
        (false, _) => Ok(None),
    }
}

/// `renovate.json5:161 · carol`, or `review summary · acme-bot` for a
/// thread with no file.
fn label_for(t: &crate::forge::Thread) -> String {
    let who = t.author().unwrap_or("ghost");
    match (&t.path, t.line.or(t.original_line)) {
        (Some(p), Some(l)) => format!("{p}:{l} · {who}"),
        (Some(p), None) => format!("{p} · {who}"),
        (None, _) => format!("review summary · {who}"),
    }
}

/// Where one thread of a run has got to.
///
/// The states a thread can actually be in, rather than done/not-done: a run that
/// answered four threads, held one back and could not apply a sixth has five
/// different outcomes to account for, and an overview that says "5 of 6" about it
/// is hiding the only rows worth reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// Not reached yet.
    Pending,
    /// The session committed for it, and you have not decided about the reply.
    Committed,
    /// Committed and the reviewer has been answered. Done.
    Replied,
    /// Committed, and you kept the reply back to write yourself.
    Held,
    /// Handed to you at triage; the session did not touch it.
    Manual,
    /// Words only: nothing to build, so the reply is the whole of it.
    WordsOnly,
    /// The session could not finish it and said why.
    NeedsYou,
}

/// One thread as the implementing session sees it.
///
/// Derived from the same [`resolve`] the batch path uses, so the session and the
/// daemon are working from one reading of your decisions: the drift checks, the
/// story/tracker refusal and the reply resolution have all already happened by
/// the time this is written out.
#[derive(Debug, Clone, Serialize)]
pub struct PlannedThread {
    pub thread_id: String,
    /// `path:line`, or the thread's label when it is a review summary.
    pub location: String,
    pub reviewer_said: String,
    pub stance: crate::proposal::Stance,
    pub mode: crate::proposal::Mode,
    /// The words the daemon will post once the work lands. The session does not
    /// post them; it is told them so its commit message and its own reasoning
    /// match what the reviewer will read.
    pub reply: Option<String>,
    /// The fix triage staged, for the session to apply and adapt. Absent under
    /// `manual`, where you are writing it.
    pub patch: Option<String>,
    pub story: Option<crate::proposal::StoryDraft>,
    /// Where this thread has got to. Not sent to the session — it is the daemon's
    /// account of the run, and the session is told one thread at a time.
    #[serde(skip_serializing)]
    pub status: ThreadStatus,
    /// The commit the session made for it, once it reports one.
    #[serde(skip_serializing)]
    pub commit: Option<String>,
    /// Why it stopped, when it did.
    #[serde(skip_serializing)]
    pub note: Option<String>,
}

/// The whole run, in the order the threads should be worked.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub pr: u64,
    /// The head the decisions were taken against. The session re-checks it before
    /// touching anything, because a force-push in between invalidates every patch.
    pub base_sha: String,
    pub threads: Vec<PlannedThread>,
}

/// Turn accepted decisions into the plan a session works from.
pub fn plan(
    pr: u64,
    positions: &crate::proposal::ProposalSet,
    fresh: &Threads,
    batch: &Batch,
    tracker: crate::config::Tracker,
) -> Result<Plan> {
    let handled = resolve(positions, fresh, batch, tracker, None)?;
    Ok(Plan {
        pr,
        base_sha: batch.base_sha.clone(),
        threads: handled
            .into_iter()
            .map(|h| PlannedThread {
                location: h.label.clone(),
                // What it starts as, rather than a blanket `Pending`: a thread the
                // session will never touch should not sit in the overview looking
                // like one it has not got to yet.
                status: match (h.mode, h.patch.is_some()) {
                    (crate::proposal::Mode::Manual, _) => ThreadStatus::Manual,
                    (_, false) => ThreadStatus::WordsOnly,
                    (_, true) => ThreadStatus::Pending,
                },
                thread_id: h.thread_id,
                reviewer_said: h.reviewer_said,
                stance: h.stance,
                mode: h.mode,
                reply: h.reply.or(Some(h.draft).filter(|d| !d.is_empty())),
                patch: h.patch,
                story: h.story,
                commit: None,
                note: None,
            })
            .collect(),
    })
}

/// Run the batch.
///
/// `fresh` must be a fetch taken *now*, not the cache: it supplies the staleness
/// check, the comment ids, and the set of threads still open — which is what makes
/// retry idempotent and what lets a thread that arrived mid-session hold its
/// author back on its own.
pub async fn run(
    app: &Arc<AppState>,
    pr: &Pr,
    fresh: &Threads,
    batch: Batch,
) -> Result<PostReport> {
    run_inner(app, pr, fresh, batch, None).await
}

/// Finish a batch that stopped for the manual phase.
///
/// The same pipeline, resumed: the accepted patches are already committed, so the
/// local half folds *your* edits instead of applying a patch, and everything from
/// the push onward runs exactly as it would have.
pub async fn finish(
    app: &Arc<AppState>,
    pr: &Pr,
    fresh: &Threads,
    done: Finish,
) -> Result<PostReport> {
    let batch = done.batch.clone();
    run_inner(app, pr, fresh, batch, Some(done)).await
}

async fn run_inner(
    app: &Arc<AppState>,
    pr: &Pr,
    fresh: &Threads,
    batch: Batch,
    resume: Option<Finish>,
) -> Result<PostReport> {
    // The gates again. A review can sit open for hours: the tree can go dirty, a
    // rebase can stop, or fix-pr can start between opening the cards and pushing.
    let workspace = crate::api::workspace_for(app, &pr.head_ref)
        .await
        .with_context(|| format!("no worktree holding {} — re-triage", pr.head_ref))?;
    let gated = match &resume {
        // The tree is dirty because the phase asked you to edit it, so that one
        // gate has to stand down. The other two still hold.
        Some(_) => crate::triage::gate_allowing_your_edits(app, pr.number, &workspace).await?,
        None => crate::triage::gate(app, pr.number, &workspace).await?,
    };
    if let Some(g) = gated {
        bail!("{}", g.say());
    }
    let path = app
        .workspace_path(&workspace)
        .await
        .context("the worktree vanished")?;

    // The branch must start level with origin, on the first half only.
    //
    // Not a nicety: `git push` sends the whole branch, so once this batch commits
    // anything, any commit already sitting unpushed rides along with it — and no
    // push *condition* can prevent that, only refusing up front can. The case that
    // makes it concrete is a manual phase somebody backed out of: it leaves a commit
    // on the branch deliberately, and without this a later reply-only batch would
    // force-push that commit having written nothing itself.
    //
    // Skipped on the resume, where being ahead of origin is exactly what half one
    // was for.
    let local_head = crate::git::head_sha(&path)?;
    if !level_with_origin(&local_head, fresh.head_sha.as_deref(), resume.is_some()) {
        return Ok(PostReport::refused(format!(
            "this branch has work origin does not: local is {} and origin is {}. Pushing \
             anything would push that too, so push or drop it first — a manual phase you \
             backed out of leaves a commit here on purpose.",
            short(&local_head),
            short(fresh.head_sha.as_deref().unwrap_or_default())
        )));
    }

    if let Some(head) = &fresh.head_sha {
        // On the resume, origin already holding our own commit means the push landed
        // and only the response was lost — a post-receive hook, a dropped connection,
        // the daemon being killed. Without this arm that is a permanent refusal with
        // the code public and not one reply posted, which is the same family of bug
        // as the dead end this whole round is closing.
        let ours = resume.is_some() && head == &local_head;
        if head != &batch.base_sha && !ours {
            return Ok(PostReport::refused(format!(
                "the branch moved since triage ({} → {}). The patches were generated against \
                 code that is no longer there; re-triage rather than write over it.",
                short(&batch.base_sha),
                short(head)
            )));
        }
    }

    let proposals = app
        .inner
        .read()
        .await
        .proposals
        .get(&pr.number)
        .cloned()
        .with_context(|| format!("PR #{} has no triage proposals to post", pr.number))?;
    let comments = resume.as_ref().map(|r| &r.comments);
    let handled = resolve(&proposals, fresh, &batch, app.cfg.tracker, comments)?;

    // Hoisted above the local write: the recovery path below needs it to carry the
    // phase forward, and a phase whose `threads` is empty renders a list with no rows
    // and a live continue button asking for no comment.
    let waiting: Vec<ManualThread> = handled
        .iter()
        .filter(|h| h.mode == Mode::Manual)
        .map(|h| ManualThread {
            thread_id: h.thread_id.clone(),
            label: h.label.clone(),
            comment: h.reviewer_said.clone(),
            draft: h.draft.clone(),
        })
        .collect();

    // --- half one: local, undoable -----------------------------------------
    let mut report = match &resume {
        None => match write_local(&path, app, &handled).await? {
            Ok(w) => w,
            Err(refusal) => return Ok(refusal),
        },
        Some(done) => {
            // The phase's own staleness check, and the reason there is no
            // server-side pending state to go stale: what the first half produced
            // is a commit, so git is the record.
            let head = crate::git::head_sha(&path)?;
            if head != done.committed {
                return Ok(PostReport::refused_finally(format!(
                    "the branch moved while the manual phase was open ({} → {}). The accepted \
                     patches are still committed on it and nothing has been pushed or posted; \
                     whatever moved it may not account for your edits, so look at the branch \
                     before continuing.",
                    short(&done.committed),
                    short(&head)
                )));
            }
            // The digest half one recorded. The batch is re-supplied and re-resolved
            // from scratch on the resume, so without this a decision half one never
            // saw would post a reply describing code that was never applied.
            if let Some(phase) = app.inner.read().await.manual.get(&pr.number) {
                if phase.decisions != digest_of(&batch) {
                    return Ok(PostReport::refused_finally(
                        "these are not the decisions the phase was opened with. Start the \
                         batch again from the cards rather than finishing one it did not \
                         produce."
                            .to_string(),
                    ));
                }
            }
            match write_manual(&path, app, &handled, done).await? {
                Ok(w) => w,
                Err(refusal) => return Ok(carry_phase(app, pr.number, &path, refusal).await),
            }
        }
    };

    // --- the phase: stop here and wait for a human -------------------------
    //
    // After the local commit, before the push. So you edit a tree that already
    // reflects every other decision — often why the thread needed hands — and
    // nothing has left the machine yet if you walk away.
    if resume.is_none() && !waiting.is_empty() {
        let phase = ManualPhase {
            committed: crate::git::head_sha(&path)?,
            files: std::mem::take(&mut report.files),
            amend: report.amend.take(),
            threads: waiting,
            decisions: digest_of(&batch),
        };
        // Durable, so a reload or a restart does not strand a batch whose patches are
        // already committed.
        {
            let mut inner = app.inner.write().await;
            inner.manual.insert(pr.number, phase.clone());
            persist_phases(&inner);
        }
        report.manual = Some(phase);
        return Ok(report);
    }

    // --- the push: everything past here is public --------------------------
    //
    // The branch started level with origin — the gate above says so — so anything
    // HEAD has beyond the remote head is this batch's, across both halves. Reading
    // `report.files` instead was wrong twice over: half one moves that list into the
    // phase with `mem::take`, and `write_manual` legitimately writes nothing when a
    // Manual thread changed no code. Both left an unpushed commit while every reply
    // went out saying a fix had landed.
    let head_now = crate::git::head_sha(&path)?;
    let unpushed = match &fresh.head_sha {
        Some(remote) => &head_now != remote,
        // No remote head to compare against — the same case that skips the staleness
        // check above. Ask git rather than the report: on a resume `report.files` is
        // routinely empty (the fold already happened, or you changed nothing), and
        // reading it there would post replies saying a fix landed without pushing it.
        None => crate::git::has_unpushed(&path, &pr.head_ref),
    };
    if unpushed {
        let branch = pr.head_ref.clone();
        let p = path.clone();
        let pushed = tokio::task::spawn_blocking(move || crate::git::push_with_lease(&p, &branch))
            .await
            .context("the push panicked")?;
        if let Err(e) = pushed {
            // **HEAD has moved by now**, on both halves — the local write committed.
            // Returning an `Err` here 500s the request, the SPA only toasts, and the
            // next attempt finds a HEAD that no longer matches the phase and calls it
            // terminal. So the batch dies with a local commit and no route forward.
            // A push that failed is the most recoverable state there is, and it has to
            // be reported as one.
            report.refused = Some(format!("{e:#}"));
            report.retryable = true;
            return Ok(carry_phase(app, pr.number, &path, report).await);
        }
        report.pushed = Some(crate::git::head_sha(&path)?);
        // Recorded before anything outward runs: if the story call or a reply then
        // fails, a retry must know the push already landed.
        update_phase_head(app, pr.number, &path).await;
    }

    // --- the stories, before the replies that carry their ids ---------------
    //
    // After the push on purpose. A local refusal has to keep "nothing external
    // happened" true, which is what makes the pre-commit panel what it is. And of
    // the two half-done states, a pushed commit with an unfiled story is the one a
    // retry fixes; a filed story with an unpushed commit is not.
    let wanted: Vec<crate::story::Wanted> = handled
        .iter()
        .filter_map(|h| {
            h.story.as_ref().map(|draft| crate::story::Wanted {
                thread_id: h.thread_id.clone(),
                draft: draft.clone(),
                permalink: h.permalink.clone(),
            })
        })
        .collect();
    let filed = crate::story::file_all(app, pr.number, &wanted).await;

    let (owner, name) =
        crate::resolve_repo(app).context("no GitHub repo configured and none on the remote")?;
    // Writes shell `gh`, so no read token; `path` is the worktree it runs in.
    let forge = ForgeImpl::for_kind(app.cfg.forge, owner, name, String::new());
    post_outward(&forge, &path, pr.number, fresh, &handled, &filed, &mut report).await;

    // The batch is over. Keeping the phase would offer to finish something that
    // already finished — and a later batch on this PR would inherit its digest.
    // Anything still failed is retried through the report, not the phase.
    if report.failed.is_empty() {
        let mut inner = app.inner.write().await;
        if inner.manual.remove(&pr.number).is_some() {
            persist_phases(&inner);
        }
    } else {
        update_phase_head(app, pr.number, &path).await;
    }
    Ok(report)
}

/// May a batch start here, or is the branch already ahead of origin?
///
/// `git push` sends the whole branch, so once a batch commits anything, a commit
/// already sitting unpushed rides along with it. No push *condition* can prevent
/// that — only refusing to start can. The case that makes it concrete is a manual
/// phase somebody backed out of: it leaves a commit on the branch on purpose, and
/// without this a later reply-only batch would force-push it having written nothing
/// itself.
///
/// The resume is exempt: being ahead of origin is precisely what half one was for.
fn level_with_origin(local: &str, remote: Option<&str>, resuming: bool) -> bool {
    if resuming {
        return true;
    }
    match remote {
        Some(r) => local == r,
        // Nothing to compare against — the same case that skips the staleness check.
        None => true,
    }
}

/// Attach the stored phase to a report, so a stumble mid-batch stays resumable.
///
/// The phase's `committed` is refreshed from the worktree first: whatever went wrong,
/// the local write may already have moved `HEAD`, and a phase pointing at the old sha
/// is what turns a recoverable failure into a permanent one. `files` and `amend` keep
/// half one's values — they describe the accepted patches, and substituting the
/// hand-edits would have the screen call your own work "accepted changes".
async fn carry_phase(
    app: &Arc<AppState>,
    pr: u64,
    path: &Path,
    mut report: PostReport,
) -> PostReport {
    update_phase_head(app, pr, path).await;
    if let Some(phase) = app.inner.read().await.manual.get(&pr) {
        report.manual = Some(phase.clone());
        report.retryable = true;
    }
    report
}

/// Point the stored phase at the worktree's current `HEAD`.
async fn update_phase_head(app: &Arc<AppState>, pr: u64, path: &Path) {
    let Ok(head) = crate::git::head_sha(path) else {
        return;
    };
    let mut inner = app.inner.write().await;
    let Some(phase) = inner.manual.get_mut(&pr) else {
        return;
    };
    if phase.committed == head {
        return;
    }
    phase.committed = head;
    persist_phases(&inner);
}

/// Write the phases out. Best effort by design: failing to persist costs the resume
/// after a restart, and turning that into a failed batch would be worse than the thing
/// it protects against.
fn persist_phases(inner: &crate::state::Inner) {
    if let Err(e) = crate::store::save_manual(&inner.manual) {
        tracing::warn!("could not save manual.json: {e:#}");
    }
}

/// The lines half one blames to pick a fixup target.
///
/// Only lines a patch actually writes. A `Manual` thread carries an anchor too — the
/// phase blames at it later — and including it here let a thread whose code does not
/// exist yet decide the target for one that does: `amend_target` saw two commits,
/// said "the changes span P and Q", and degraded the accepted patch to a plain HEAD
/// amend.
fn blame_lines(handled: &[Handled]) -> Vec<(String, u32)> {
    handled
        .iter()
        .filter(|h| h.patch.is_some())
        .filter_map(|h| h.touched.clone())
        .collect()
}

/// The manual phase's local half: fold what *you* wrote.
///
/// No ladder and no apply — the edits are already on disk, which is the state the
/// propose-only path refuses to work in. `patch::write_manual` does the rest, and
/// derives the file list from `git status` rather than from a patch, which is what
/// keeps "only what you approved" true when nobody declared what was touched.
async fn write_manual(
    path: &Path,
    app: &Arc<AppState>,
    handled: &[Handled],
    done: &Finish,
) -> Result<std::result::Result<PostReport, PostReport>> {
    // Blamed at the lines the reviewers pointed at. If your edits span commits the
    // fold degrades to a HEAD amend and says so, which is the honest answer.
    let touched: Vec<(String, u32)> = handled
        .iter()
        .filter(|h| h.mode == Mode::Manual)
        .filter_map(|h| h.touched.clone())
        .collect();

    let cwd = path.to_path_buf();
    let upstream = app.cfg.upstream_ref.clone();
    let approved = done.files.clone();
    // The anchors were validated against the PR head at triage; if half one has
    // moved HEAD off it, they no longer say which commit owns them.
    let anchored_at = done.batch.base_sha.clone();
    let written = tokio::task::spawn_blocking(move || -> Result<Written> {
        let base = crate::git::merge_base(&cwd, &upstream)?;
        let email = crate::git::user_email(&cwd);
        crate::patch::write_manual(&cwd, &base, &email, &touched, &approved, &anchored_at)
    })
    .await
    .context("the manual write panicked")??;

    Ok(match written {
        Written::Committed { files, amend } => Ok(PostReport {
            files,
            amend: Some(amend),
            ..Default::default()
        }),
        // You changed nothing, which a Manual thread is allowed to mean.
        Written::NothingToWrite => Ok(PostReport::default()),
        Written::Refused(why) => Err(PostReport::refused(why)),
    })
}

/// The local half: blame, ladder, apply, hooks, fold. `Err` on the inner result
/// is a refusal to report, not an error — the branch is untouched either way.
async fn write_local(
    path: &Path,
    app: &Arc<AppState>,
    handled: &[Handled],
) -> Result<std::result::Result<PostReport, PostReport>> {
    let patches: Vec<Patch> = handled
        .iter()
        .filter_map(|h| {
            h.patch.as_ref().map(|diff| Patch {
                thread_id: h.thread_id.clone(),
                diff: diff.clone(),
            })
        })
        .collect();
    if patches.is_empty() {
        // Reply-only decisions still have posting to do. Not a no-op.
        return Ok(Ok(PostReport::default()));
    }
    let touched = blame_lines(handled);

    let cwd = path.to_path_buf();
    let upstream = app.cfg.upstream_ref.clone();
    let written = tokio::task::spawn_blocking(move || -> Result<Written> {
        let base = crate::git::merge_base(&cwd, &upstream)?;
        let email = crate::git::user_email(&cwd);
        crate::patch::write_batch(&cwd, &base, &email, &patches, &touched)
    })
    .await
    .context("the write batch panicked")??;

    Ok(match written {
        Written::Committed { files, amend } => Ok(PostReport {
            files,
            amend: Some(amend),
            ..Default::default()
        }),
        Written::NothingToWrite => Ok(PostReport::default()),
        Written::Refused(why) => Err(PostReport::refused(why)),
    })
}

/// What became of one thread's outward words.
#[derive(Debug, PartialEq, Eq)]
pub enum Posted {
    Sent,
    /// The identical body is already on the thread, so this was a retry rather
    /// than a second post.
    AlreadyThere,
    /// The story the reply links to could not be filed, so nothing was said. A
    /// literal `{story}` on a reviewer's thread is worse than silence.
    HeldNoStory(String),
}

/// File the story a reply links to, substitute the token, and post it — once.
///
/// The single-thread twin of [`post_outward`]'s per-thread body, extracted so the
/// resolve run cannot drift from the batch on the three rules that matter:
/// a story is filed *before* the reply that links to it, [`STORY_TOKEN`] never
/// reaches GitHub, and a repeated call does not post twice.
///
/// [`STORY_TOKEN`]: crate::proposal::STORY_TOKEN
///
/// `fresh` must be a fetch from *now*: it supplies the comment ids the write
/// needs, the permalink a story is keyed on, and the answer to "did I already say
/// this". Everything the batch reports per thread (`landed`/`failed`/`skipped`)
/// stays the batch's business — a run answers one thread at a time and has the
/// card in front of you instead.
pub(crate) async fn post_one(
    app: &Arc<AppState>,
    forge: &ForgeImpl,
    at: &Path,
    pr: u64,
    thread_id: &str,
    reply: &str,
    story: Option<&crate::proposal::StoryDraft>,
    fresh: &Threads,
) -> Result<Posted> {
    let root = fresh
        .root_for(thread_id)
        .with_context(|| format!("thread {thread_id} has no comment to answer"))?;

    let mut text = reply.to_string();
    if let Some(draft) = story {
        // Keyed on the thread's own URL, which is what makes a retry find the
        // story it already filed rather than open a second one.
        let permalink = fresh
            .items
            .iter()
            .find(|t| t.id == thread_id)
            .and_then(|t| t.comments.first())
            .map(|c| c.url.clone())
            .unwrap_or_default();
        let wanted = [crate::story::Wanted {
            thread_id: thread_id.to_string(),
            draft: draft.clone(),
            permalink,
        }];
        let filed = crate::story::file_all(app, pr, &wanted).await;
        match filed.get(thread_id) {
            Some(Ok(f)) => {
                // `replace`, not `replacen`: a reply that names the story twice
                // stays consistent.
                text = text.replace(crate::proposal::STORY_TOKEN, &f.story.link());
            }
            other => {
                return Ok(Posted::HeldNoStory(match other {
                    Some(Err(e)) => e.clone(),
                    // `file_all` answers for every thread it is given, so this is
                    // unreachable rather than expected.
                    _ => "the story run answered nothing for this thread".to_string(),
                }));
            }
        }
    }

    if already_replied(fresh, thread_id, &text) {
        return Ok(Posted::AlreadyThere);
    }
    blocking(forge, at, &root, Send::Reply(text)).await?;
    Ok(Posted::Sent)
}

/// 👍 one thread, the plain-adoption case.
///
/// The reaction half of [`post_one`]'s job, split off because the two are chosen
/// by stance and never both: `Agree` reacts and says nothing, `Reply`/`Story`
/// write words. Shared for the same reason — so a run and a batch adopt a
/// reviewer's point identically.
pub(crate) async fn react_one(
    forge: &ForgeImpl,
    at: &Path,
    thread_id: &str,
    fresh: &Threads,
) -> Result<()> {
    let root = fresh
        .root_for(thread_id)
        .with_context(|| format!("thread {thread_id} has no comment to react to"))?;
    blocking(forge, at, &root, Send::ThumbsUp).await
}

/// The outward writes, in the order the design settled: stories, then replies,
/// then reactions, then re-requests.
///
/// A failure does **not** stop the rest. Each write is independent, and the two
/// dependencies that do exist are expressed rather than sequenced: a re-request
/// asserting "everything of yours is addressed" falls out of the failed thread
/// counting as still open, and a reply that needs a story id is skipped when the
/// story is not there, because posting a literal `{story}` is worse than posting
/// nothing.
async fn post_outward(
    forge: &ForgeImpl,
    at: &Path,
    pr: u64,
    fresh: &Threads,
    handled: &[Handled],
    filed: &crate::story::Results,
    report: &mut PostReport,
) {
    let mut done: Vec<&str> = Vec::new();

    for h in handled {
        let mut all_landed = true;
        let mut reply = h.reply.clone();

        if h.story.is_some() {
            match filed.get(&h.thread_id) {
                Some(Ok(f)) => {
                    report.landed.push(Landed {
                        thread_id: h.thread_id.clone(),
                        label: h.label.clone(),
                        what: What::Story,
                        already: f.reused,
                        story: Some(f.story.clone()),
                    });
                    // The whole reason the token exists: the id could not be known
                    // when the reply was drafted. `replace` rather than `replacen`,
                    // so a reply that mentions it twice is consistent.
                    reply = reply.map(|t| t.replace(crate::proposal::STORY_TOKEN, &f.story.link()));
                }
                other => {
                    all_landed = false;
                    report.failed.push(Failed {
                        thread_id: h.thread_id.clone(),
                        label: h.label.clone(),
                        what: What::Story,
                        error: match other {
                            Some(Err(e)) => e.clone(),
                            // `file_all` answers for every thread it was given, so
                            // this is unreachable rather than expected.
                            _ => "the story run answered nothing for this thread".to_string(),
                        },
                    });
                    report.skipped.push(Skipped {
                        label: h.label.clone(),
                        what: What::Reply,
                        waiting_on: "the story it links to was not filed".to_string(),
                    });
                    reply = None;
                }
            }
        }

        if let Some(text) = &reply {
            match already_replied(fresh, &h.thread_id, text) {
                true => report.landed.push(Landed {
                    thread_id: h.thread_id.clone(),
                    label: h.label.clone(),
                    what: What::Reply,
                    already: true,
                    story: None,
                }),
                false => match blocking(forge, at, &h.root, Send::Reply(text.clone())).await {
                    Ok(()) => report.landed.push(Landed {
                        thread_id: h.thread_id.clone(),
                        label: h.label.clone(),
                        what: What::Reply,
                        already: false,
                        story: None,
                    }),
                    Err(e) => {
                        all_landed = false;
                        report.failed.push(Failed {
                            thread_id: h.thread_id.clone(),
                            label: h.label.clone(),
                            what: What::Reply,
                            error: format!("{e:#}"),
                        });
                    }
                },
            }
        }

        if h.stance.gives_thumbs_up() {
            // Not re-derivable: the thread query does not select reactions.
            // GitHub is expected to treat one as unique per (user, content) and
            // return the existing one, so a retry is a no-op rather than a
            // duplicate. Unverified until the scratch PR settles it; the fallback
            // is selecting `reactions` in the thread query, never a ledger.
            match blocking(forge, at, &h.root, Send::ThumbsUp).await {
                Ok(()) => report.landed.push(Landed {
                    thread_id: h.thread_id.clone(),
                    label: h.label.clone(),
                    what: What::ThumbsUp,
                    already: false,
                    story: None,
                }),
                Err(e) => {
                    all_landed = false;
                    report.failed.push(Failed {
                        thread_id: h.thread_id.clone(),
                        label: h.label.clone(),
                        what: What::ThumbsUp,
                        error: format!("{e:#}"),
                    });
                }
            }
        }

        if all_landed {
            done.push(&h.thread_id);
        }
    }

    rerequest(forge, at, pr, fresh, &done, report).await;
}

/// The reviewers of a PR, split by whether anything of theirs is still open.
struct Split<'a> {
    /// Everyone with an answerable thread on this PR.
    all: Vec<&'a str>,
    /// Those with at least one thread this batch did not settle.
    open: Vec<&'a str>,
    /// Each of those, with a thread of theirs to name.
    holding: Vec<(&'a str, String)>,
}

/// Work out who can be re-requested, from the fresh fetch alone.
///
/// "Theirs" is the threads they opened. `done` is the threads this batch settled
/// completely — so a thread that arrived mid-session, one that was skipped, and
/// one whose reply failed all count the same way, holding their author back with
/// no special handling for any of them. That is the whole reason re-request is
/// derived rather than tracked.
fn split_reviewers<'a>(fresh: &'a Threads, done: &[&str]) -> Split<'a> {
    let mut s = Split {
        all: Vec::new(),
        open: Vec::new(),
        holding: Vec::new(),
    };
    for t in fresh.items.iter().filter(|t| t.answerable) {
        let Some(who) = t.author() else { continue };
        // Your own comment is not a review of yourself, and GitHub refuses to
        // request review from the PR's author anyway.
        if who == fresh.viewer {
            continue;
        }
        s.all.push(who);
        if !done.contains(&t.id.as_str()) {
            s.open.push(who);
            s.holding.push((who, label_for(t)));
        }
    }
    s
}

/// Ask every reviewer with nothing of theirs left open to look again.
async fn rerequest(
    forge: &ForgeImpl,
    at: &Path,
    pr: u64,
    fresh: &Threads,
    done: &[&str],
    report: &mut PostReport,
) {
    let Split { all, open, holding } = split_reviewers(fresh, done);

    for login in forge::ready_to_rerequest(&all, &open) {
        match blocking_rerequest(forge, at, pr, login).await {
            Ok(()) => report.rerequested.push(login.to_string()),
            Err(e) => report.failed.push(Failed {
                thread_id: String::new(),
                label: format!("re-request {login}"),
                what: What::Rerequest,
                error: format!("{e:#}"),
            }),
        }
    }
    // One row per held-back reviewer, naming the first thread of theirs that is
    // still open — the whole point is that it is answerable, not a count.
    let mut named: Vec<&str> = Vec::new();
    for (who, thread) in holding {
        if named.contains(&who) {
            continue;
        }
        named.push(who);
        report.held_back.push(Skipped {
            label: format!("re-request {who}"),
            what: What::Rerequest,
            waiting_on: format!("{thread} is still unanswered"),
        });
    }
}

/// Has this exact reply already been posted?
///
/// Exact match on `with_footer`'s output, by the viewer. Reliable because we
/// control the text: the footer makes our own comments identifiable and the body
/// is byte-for-byte what would be sent. A reply the human edited between attempts
/// posts again, which is correct — it is different words.
fn already_replied(fresh: &Threads, thread_id: &str, body: &str) -> bool {
    let want = forge::with_footer(body);
    fresh
        .items
        .iter()
        .find(|t| t.id == thread_id)
        .is_some_and(|t| {
            t.comments
                .iter()
                .any(|c| c.author == fresh.viewer && c.body.trim() == want.trim())
        })
}

enum Send {
    Reply(String),
    ThumbsUp,
}

/// `gh` is a subprocess, so every write goes through the blocking pool rather
/// than stalling the runtime for the length of an HTTP round trip.
async fn blocking(forge: &ForgeImpl, at: &Path, root: &ThreadRoot, send: Send) -> Result<()> {
    let (f, at, root) = (forge.clone(), at.to_path_buf(), root.clone());
    tokio::task::spawn_blocking(move || match send {
        Send::Reply(body) => f.reply(&at, &root, &body),
        Send::ThumbsUp => f.thumbs_up(&at, &root),
    })
    .await
    .context("the write panicked")?
}

async fn blocking_rerequest(forge: &ForgeImpl, at: &Path, pr: u64, login: &str) -> Result<()> {
    let (f, at, login) = (forge.clone(), at.to_path_buf(), login.to_string());
    tokio::task::spawn_blocking(move || f.rerequest(&at, pr, &login))
        .await
        .context("the re-request panicked")?
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The story arms only exist with a tracker configured, so the tests that are
    /// not about that run with one.
    const TRACKER: crate::config::Tracker = crate::config::Tracker::Stub;

    /// The first half: no phase has opened.
    const FIRST_HALF: Option<&std::collections::HashMap<String, String>> = None;
    use crate::forge::{Comment, Thread};
    use crate::proposal::{Proposal, ProposalSet, StoryDraft};

    fn comment(id: u64, author: &str, body: &str) -> Comment {
        Comment {
            database_id: id,
            author: author.into(),
            body: body.into(),
            created_at: "2026-08-17T00:00:00Z".into(),
            url: "u".into(),
            diff_hunk: None,
        }
    }

    fn thread(id: &str, path: Option<&str>, line: Option<u32>, author: &str) -> Thread {
        Thread {
            id: id.into(),
            path: path.map(str::to_string),
            line,
            start_line: None,
            original_line: None,
            is_resolved: false,
            is_outdated: false,
            comments: vec![comment(100, author, "you call this twice")],
            answerable: true,
        }
    }

    fn fetched(items: Vec<Thread>) -> Threads {
        Threads {
            pr: 10001,
            viewer: "kars".into(),
            head_sha: Some("abc123".into()),
            items,
        }
    }

    fn position(stance: Stance) -> Position {
        with_patch(stance, false)
    }

    /// Stance and "does it carry a fix" are separate axes now, so the helper
    /// takes both.
    fn with_patch(stance: Stance, patch: bool) -> Position {
        Position {
            label: "Apply".into(),
            sub: String::new(),
            stance,
            patch: patch.then(|| "--- a/f\n+++ b/f\n".to_string()),
            reply: stance.writes_reply().then(|| "Resolved.".to_string()),
            story: stance.files_story().then(|| StoryDraft {
                title: "t".into(),
                body: "b".into(),
            }),
        }
    }

    /// A batch whose one decision is written by hand rather than by the agent.
    fn manual_batch(thread_id: &str, position: usize, reply: Option<&str>) -> Batch {
        batch_mode(thread_id, position, reply, Mode::Manual)
    }

    fn proposed(thread_id: &str, positions: Vec<Position>) -> ProposalSet {
        ProposalSet {
            base_sha: "abc123".into(),
            proposals: vec![Proposal {
                thread_id: thread_id.into(),
                continued: false,
                read: "they are right".into(),
                verified: Some("grepped it".into()),
                recommend: 0,
                positions,
            }],
        }
    }

    fn batch(thread_id: &str, position: usize, reply: Option<&str>) -> Batch {
        batch_mode(thread_id, position, reply, Mode::Agent)
    }

    fn batch_mode(thread_id: &str, position: usize, reply: Option<&str>, mode: Mode) -> Batch {
        Batch {
            base_sha: "abc123".into(),
            decisions: vec![Decision {
                thread_id: thread_id.into(),
                position,
                reply: reply.map(str::to_string),
                mode,
            }],
        }
    }

    #[test]
    fn a_patch_position_resolves_to_its_diff_and_a_blame_target() {
        let set = proposed("PRRT_1", vec![with_patch(Stance::Agree, true)]);
        let fresh = fetched(vec![thread("PRRT_1", Some("src/Foo.php"), Some(42), "john")]);
        let got = resolve(&set, &fresh, &batch("PRRT_1", 0, None), TRACKER, FIRST_HALF).unwrap();

        assert_eq!(got.len(), 1);
        assert!(got[0].patch.is_some());
        assert_eq!(got[0].touched, Some(("src/Foo.php".into(), 42)));
        // change+thumbsup posts a reaction, not words.
        assert!(got[0].reply.is_none());
        assert!(got[0].stance.gives_thumbs_up());
        assert_eq!(got[0].root.comment_id(), 100);
    }

    #[test]
    fn your_wording_overrides_the_draft() {
        let set = proposed("PRRT_1", vec![position(Stance::Reply)]);
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(1), "john")]);

        let kept = resolve(&set, &fresh, &batch("PRRT_1", 0, None), TRACKER, FIRST_HALF).unwrap();
        assert_eq!(kept[0].reply.as_deref(), Some("Resolved."));

        let mine = resolve(
            &set,
            &fresh,
            &batch("PRRT_1", 0, Some("Nee, want …")),
            TRACKER,
            FIRST_HALF,
        )
        .unwrap();
        assert_eq!(mine[0].reply.as_deref(), Some("Nee, want …"));
    }

    #[test]
    fn a_reply_position_with_an_emptied_box_is_refused_not_posted_blank() {
        // `Say something else` starts empty; sending it untouched would post a
        // blank comment that cannot be deleted from here.
        let set = proposed("PRRT_1", vec![position(Stance::Reply)]);
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(1), "john")]);
        let err = resolve(
            &set,
            &fresh,
            &batch("PRRT_1", 0, Some("   ")),
            TRACKER,
            FIRST_HALF,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("empty"), "{err}");
    }

    /// A story position, on the review-summary thread that is its canonical home.
    fn story_set() -> ProposalSet {
        ProposalSet {
            base_sha: "abc123".into(),
            proposals: vec![Proposal {
                thread_id: "PRRT_1".into(),
                continued: false,
                read: "fair, out of scope".into(),
                verified: None,
                recommend: 0,
                positions: vec![Position {
                    reply: Some("Tracked as {story}.".into()),
                    ..position(Stance::Story)
                }],
            }],
        }
    }

    /// The proposal side of a hand-written thread. Nothing about the position
    /// says "manual" any more — that is the decision's `mode`.
    fn manual_set(draft: &str) -> ProposalSet {
        proposed(
            "PRRT_1",
            vec![Position {
                label: "Apply".into(),
                sub: String::new(),
                stance: Stance::Reply,
                patch: None,
                reply: Some(draft.into()),
                story: None,
            }],
        )
    }

    #[test]
    fn a_batch_will_not_start_on_a_branch_that_is_ahead_of_origin() {
        assert!(level_with_origin("abc", Some("abc"), false));
        // An abandoned phase's commit, or any local WIP.
        assert!(!level_with_origin("def", Some("abc"), false));
        // ...but the resume is ahead by construction.
        assert!(level_with_origin("def", Some("abc"), true));
        // Nothing to compare against.
        assert!(level_with_origin("def", None, false));
    }

    #[test]
    fn half_one_blames_only_lines_a_patch_writes() {
        // A mixed batch: one patch, one thread to be written by hand. The Manual
        // anchor must not reach the fixup decision — with it, `amend_target` sees two
        // owning commits and amends HEAD instead of folding the patch into the
        // commit that owns it, because of code that does not exist yet.
        let fresh = fetched(vec![
            thread("PRRT_1", Some("a.ts"), Some(10), "john"),
            thread("PRRT_2", Some("b.ts"), Some(99), "dave"),
        ]);
        let mut set = proposed("PRRT_1", vec![with_patch(Stance::Reply, true)]);
        set.proposals.push(Proposal {
            thread_id: "PRRT_2".into(),
            continued: false,
            read: "wants hands".into(),
            verified: None,
            recommend: 0,
            positions: vec![Position {
                label: "Apply".into(),
                sub: String::new(),
                stance: Stance::Reply,
                patch: None,
                reply: Some("Resolved.".into()),
                story: None,
            }],
        });
        let batch = Batch {
            base_sha: "abc123".into(),
            decisions: vec![
                Decision {
                    thread_id: "PRRT_1".into(),
                    position: 0,
                    reply: None,
                    mode: Mode::Agent,
                },
                // Hand-written: it still gets an anchor for the phase to blame
                // at, but nothing to apply.
                Decision {
                    thread_id: "PRRT_2".into(),
                    position: 0,
                    reply: None,
                    mode: Mode::Manual,
                },
            ],
        };
        let handled = resolve(&set, &fresh, &batch, TRACKER, FIRST_HALF).unwrap();

        // Both carry an anchor...
        assert_eq!(handled[1].touched, Some(("b.ts".into(), 99)));
        // ...but only the one with a patch is blamed.
        assert_eq!(blame_lines(&handled), vec![("a.ts".to_string(), 10)]);
    }

    /// The overview has to be able to tell six outcomes apart, and the ones that
    /// were never the session's work must not read as work it has not reached.
    #[test]
    fn a_plan_starts_each_thread_where_it_actually_is() {
        use crate::proposal::{Mode, Stance};
        let fresh = fetched(vec![
            thread("PRRT_1", Some("a.ts"), Some(10), "john"),
            thread("PRRT_2", Some("b.ts"), Some(99), "john"),
            thread("PRRT_3", Some("c.ts"), Some(12), "john"),
        ]);
        let mut set = proposed("PRRT_1", vec![with_patch(Stance::Reply, true)]);
        for (id, pos) in [
            ("PRRT_2", with_patch(Stance::Reply, true)),
            ("PRRT_3", position(Stance::Reply)),
        ] {
            set.proposals.push(Proposal {
                thread_id: id.into(),
                continued: false,
                read: "r".into(),
                verified: Some("v".into()),
                recommend: 0,
                positions: vec![pos],
            });
        }
        let batch = Batch {
            base_sha: "abc123".into(),
            decisions: vec![
                // a fix for the agent
                Decision { thread_id: "PRRT_1".into(), position: 0, reply: None, mode: Mode::Agent },
                // the same, but you are writing it
                Decision { thread_id: "PRRT_2".into(), position: 0, reply: None, mode: Mode::Manual },
                // words only: nothing to build at all
                Decision { thread_id: "PRRT_3".into(), position: 0, reply: None, mode: Mode::Agent },
            ],
        };
        let plan = plan(4812, &set, &fresh, &batch, TRACKER).expect("planned");
        let got: Vec<ThreadStatus> = plan.threads.iter().map(|t| t.status).collect();
        assert_eq!(
            got,
            vec![
                ThreadStatus::Pending,
                ThreadStatus::Manual,
                ThreadStatus::WordsOnly
            ]
        );
        // And the manual one carries no patch for the session to apply behind you.
        assert!(plan.threads[1].patch.is_none());
        // The words-only one carries the words: `sweep_words_only` answers it off
        // the plan, so a `WordsOnly` thread with nothing to say would be a thread
        // nothing ever posts.
        assert!(plan.threads[2].reply.is_some());
    }

    /// The two branches `sweep_words_only` chooses between. Every stance takes
    /// exactly one of them, which is what makes the sweep total: `Agree` reacts
    /// and says nothing, `Reply`/`Story` write words and do not react.
    #[test]
    fn a_words_only_thread_is_either_words_or_a_reaction_never_neither() {
        for stance in [Stance::Agree, Stance::Reply, Stance::Story] {
            assert!(
                stance.writes_reply() ^ stance.gives_thumbs_up(),
                "{stance:?} would be answered twice or not at all"
            );
        }
    }

    #[test]
    fn a_manual_thread_carries_no_comment_into_the_phase() {
        // The card's box is a draft: you cannot describe work you have not done, so
        // the real comment is written in the phase and there is nothing to post yet.
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(12), "john")]);
        let got = resolve(
            &manual_set(""),
            &fresh,
            &manual_batch("PRRT_1", 0, None),
            TRACKER,
            FIRST_HALF,
        )
        .unwrap();
        assert_eq!(got[0].mode, Mode::Manual);
        assert!(got[0].reply.is_none(), "nothing to post on the way in");
        // The reviewer's anchor rides along, for the phase to blame at.
        assert_eq!(got[0].touched, Some(("a.ts".into(), 12)));
        // And the words being answered, so the phase needs no second fetch.
        assert_eq!(got[0].reviewer_said, "you call this twice");
    }

    #[test]
    fn a_manual_thread_will_not_finish_without_a_comment() {
        // Without one the reviewer gets a commit and silence, which is
        // indistinguishable from being ignored.
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(12), "john")]);

        // **An absent key must fail exactly like a blank one.** This is the case
        // that got away: with an empty map standing in for "the phase has not
        // opened", a resume carrying no comment at all looked like the first half,
        // the manual thread was quietly given nothing to post, and every *other*
        // reply in the batch went out to a live PR. Blank was tested; absent was
        // not, and absent is what a client actually sends.
        let none: std::collections::HashMap<String, String> = Default::default();
        let err = resolve(
            &manual_set(""),
            &fresh,
            &manual_batch("PRRT_1", 0, None),
            TRACKER,
            Some(&none),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("needs a comment"), "absent key: {err}");

        let blank: std::collections::HashMap<String, String> =
            [("PRRT_1".to_string(), "   ".to_string())].into();
        let err = resolve(
            &manual_set(""),
            &fresh,
            &manual_batch("PRRT_1", 0, None),
            TRACKER,
            Some(&blank),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("needs a comment"), "{err}");

        let written: std::collections::HashMap<String, String> = [(
            "PRRT_1".to_string(),
            "Moved to the repository.".to_string(),
        )]
        .into();
        let got = resolve(
            &manual_set(""),
            &fresh,
            &manual_batch("PRRT_1", 0, None),
            TRACKER,
            Some(&written),
        )
        .unwrap();
        assert_eq!(
            got[0].reply.as_deref(),
            Some("Moved to the repository.")
        );
    }

    #[test]
    fn manual_on_a_review_summary_has_no_line_to_blame_and_is_allowed() {
        // No anchor, so the fold degrades to a HEAD amend and says so — rather than
        // refusing an answer that is perfectly legitimate.
        let fresh = fetched(vec![thread("PRRT_1", None, None, "acme-bot")]);
        let got = resolve(
            &manual_set(""),
            &fresh,
            &manual_batch("PRRT_1", 0, None),
            TRACKER,
            FIRST_HALF,
        )
        .unwrap();
        assert_eq!(got[0].touched, None);
    }

    #[test]
    fn a_story_resolves_to_its_draft_and_keeps_the_token() {
        let fresh = fetched(vec![thread("PRRT_1", None, None, "acme-bot")]);
        let got = resolve(
            &story_set(),
            &fresh,
            &batch("PRRT_1", 0, None),
            TRACKER,
            FIRST_HALF,
        )
        .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].story.as_ref().map(|s| s.title.as_str()), Some("t"));
        // The token survives to the outward step, which is what substitutes it.
        assert!(got[0].reply.as_deref().unwrap().contains("{story}"));
        // A story writes no code and posts no reaction.
        assert!(got[0].patch.is_none());
        assert!(!got[0].stance.gives_thumbs_up());
    }

    #[test]
    fn a_story_with_no_tracker_configured_is_refused_by_name() {
        // The overlay hides the option when `tracker` is off, but the payload is
        // not the overlay — so the daemon says so rather than filing into nowhere.
        let fresh = fetched(vec![thread("PRRT_1", None, None, "acme-bot")]);
        let err = resolve(
            &story_set(),
            &fresh,
            &batch("PRRT_1", 0, None),
            crate::config::Tracker::None,
            FIRST_HALF,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no tracker is configured"), "{err}");
    }

    #[test]
    fn deleting_the_token_while_editing_refuses_before_anything_is_filed() {
        // The draft is validated on arrival, so only a human edit can lose the
        // token. Filing a story that nothing links to is worse than not filing it,
        // and this refusal lands before the local half — nothing written, nothing
        // filed, decisions kept.
        let fresh = fetched(vec![thread("PRRT_1", None, None, "acme-bot")]);
        let err = resolve(
            &story_set(),
            &fresh,
            &batch("PRRT_1", 0, Some("Goed punt, komt later.")),
            TRACKER,
            FIRST_HALF,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("{story}"), "{err}");
        assert!(err.contains("nothing linking to it"), "{err}");
    }

    #[test]
    fn a_token_in_a_position_that_files_nothing_is_refused() {
        // The reverse mistake: there would be no id to substitute, so the literal
        // braces would be posted to GitHub.
        let set = proposed("PRRT_1", vec![position(Stance::Reply)]);
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(1), "john")]);
        let err = resolve(
            &set,
            &fresh,
            &batch("PRRT_1", 0, Some("Tracked as {story}.")),
            TRACKER,
            FIRST_HALF,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("files no story"), "{err}");
    }

    #[test]
    fn an_oversized_reply_override_is_refused() {
        // The one agent-or-human string the validator never sized, and it is about
        // to carry a public URL.
        let set = proposed("PRRT_1", vec![position(Stance::Reply)]);
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(1), "john")]);
        let huge = "x".repeat(crate::proposal::MAX_FIELD + 1);
        let err = resolve(
            &set,
            &fresh,
            &batch("PRRT_1", 0, Some(&huge)),
            TRACKER,
            FIRST_HALF,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn a_decision_the_human_never_saw_is_refused() {
        let set = proposed("PRRT_1", vec![position(Stance::Reply)]);
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(1), "john")]);

        // An index past the list: the payload carries indices, so this is how a
        // bad client would try to reach content nobody reviewed.
        assert!(resolve(&set, &fresh, &batch("PRRT_1", 7, None), TRACKER, FIRST_HALF).is_err());
        // A thread with no proposal behind it.
        assert!(resolve(&set, &fresh, &batch("PRRT_9", 0, None), TRACKER, FIRST_HALF).is_err());
        // A thread that has since gone from the PR.
        let gone = fetched(vec![thread("PRRT_2", Some("a.ts"), Some(1), "john")]);
        assert!(resolve(&set, &gone, &batch("PRRT_1", 0, None), TRACKER, FIRST_HALF).is_err());
    }

    #[test]
    fn a_patch_needs_a_line_to_blame() {
        // A review summary has no file, so there is nowhere for a diff to go — that
        // case goes through the manual phase, not through a guessed anchor.
        let set = proposed("PRRT_1", vec![with_patch(Stance::Reply, true)]);
        let fresh = fetched(vec![thread("PRRT_1", None, None, "acme-bot")]);
        let err = resolve(&set, &fresh, &batch("PRRT_1", 0, None), TRACKER, FIRST_HALF)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no file"), "{err}");
    }

    #[test]
    fn a_reply_already_in_the_thread_is_not_posted_twice() {
        // This is the whole of retry: derive what is missing from a fresh fetch
        // rather than remember what was sent.
        let mut t = thread("PRRT_1", Some("a.ts"), Some(1), "john");
        t.comments.push(comment(
            101,
            "kars",
            &forge::with_footer("Resolved."),
        ));
        let fresh = fetched(vec![t]);

        assert!(already_replied(&fresh, "PRRT_1", "Resolved."));
        // Different words are a different reply, and should go.
        assert!(!already_replied(&fresh, "PRRT_1", "Fixed in the mapper."));
    }

    #[test]
    fn someone_elses_identical_comment_does_not_count_as_our_reply() {
        let mut t = thread("PRRT_1", Some("a.ts"), Some(1), "john");
        t.comments
            .push(comment(101, "john", &forge::with_footer("Resolved.")));
        assert!(!already_replied(&fetched(vec![t]), "PRRT_1", "Resolved."));
    }

    #[test]
    fn a_reviewer_is_held_back_by_their_own_unsettled_thread() {
        let fresh = fetched(vec![
            thread("PRRT_1", Some("a.ts"), Some(10), "carol"),
            thread("PRRT_2", Some("a.ts"), Some(20), "carol"),
            thread("PRRT_3", Some("b.ts"), Some(1), "dave"),
        ]);

        // Dave's one thread is settled; one of carol's is not.
        let split = split_reviewers(&fresh, &["PRRT_1", "PRRT_3"]);
        assert_eq!(
            forge::ready_to_rerequest(&split.all, &split.open),
            vec!["dave"]
        );
        assert_eq!(split.holding.len(), 1);
        assert_eq!(split.holding[0].0, "carol");
        // The row names the thread, not a count — it is the one to go and answer.
        assert_eq!(split.holding[0].1, "a.ts:20 · carol");

        // With everything settled, both go.
        let split = split_reviewers(&fresh, &["PRRT_1", "PRRT_2", "PRRT_3"]);
        assert_eq!(
            forge::ready_to_rerequest(&split.all, &split.open),
            vec!["carol", "dave"]
        );
        assert!(split.holding.is_empty());
    }

    #[test]
    fn a_thread_that_arrived_mid_session_holds_its_author_back_on_its_own() {
        // No proposal, no decision, so it cannot be in `done` — and that is enough.
        // The re-request assertion "everything of yours is addressed" stays true
        // without any mechanism for noticing the arrival.
        let fresh = fetched(vec![
            thread("PRRT_1", Some("a.ts"), Some(10), "carol"),
            thread("PRRT_new", Some("c.ts"), Some(3), "carol"),
        ]);
        let split = split_reviewers(&fresh, &["PRRT_1"]);
        assert!(forge::ready_to_rerequest(&split.all, &split.open).is_empty());
    }

    #[test]
    fn your_own_thread_never_asks_you_to_review_yourself() {
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(1), "kars")]);
        let split = split_reviewers(&fresh, &[]);
        assert!(split.all.is_empty());
        assert!(split.holding.is_empty());
    }

    #[test]
    fn a_summary_thread_is_labelled_by_what_it_is() {
        assert_eq!(
            label_for(&thread("PRRT_1", None, None, "acme-bot")),
            "review summary · acme-bot"
        );
        let mut outdated = thread("PRRT_2", Some("a.ts"), None, "john");
        outdated.original_line = Some(9);
        outdated.is_outdated = true;
        // Outdated: no current line, so it is named by where the reviewer looked.
        assert_eq!(label_for(&outdated), "a.ts:9 · alice");
    }

    async fn app() -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!("orchd-post-one-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: crate::config::Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7799}}"#,
            dir.display()
        ))
        .unwrap();
        AppState::new(cfg, "t".into(), crate::window::Chrome::None)
    }

    /// A forge is needed to call `post_one`, but these cases all return before any
    /// write, so it is never used — an empty repo is enough and nothing goes out.
    fn no_write_forge() -> ForgeImpl {
        ForgeImpl::for_kind(
            crate::config::ForgeKind::GitHub,
            "o",
            "n",
            String::new(),
        )
    }

    /// The resolve run reaches GitHub one thread at a time, so idempotency cannot
    /// be a batch-level property: a retried `…/committed` must not say it twice.
    #[tokio::test]
    async fn a_reply_already_on_the_thread_is_not_posted_again() {
        let app = app().await;
        let mut t = thread("PRRT_1", Some("a.ts"), Some(1), "john");
        // The viewer's own answer, footed the way the forge writes it.
        t.comments
            .push(comment(101, "kars", &crate::forge::with_footer("Resolved.")));
        let fresh = fetched(vec![t]);

        let out = post_one(
            &app,
            &no_write_forge(),
            &app.cfg.main_checkout.clone(),
            10001,
            "PRRT_1",
            "Resolved.",
            None,
            &fresh,
        )
        .await
        .unwrap();
        assert_eq!(out, Posted::AlreadyThere);
    }

    /// The token is why the story is filed first. A cached story answers without
    /// spawning the filer, which is what makes this testable at all — and the
    /// substituted text is what the idempotency check then compares, so seeing
    /// `AlreadyThere` proves the link went in.
    #[tokio::test]
    async fn the_story_token_is_replaced_by_the_link_before_anything_is_posted() {
        let app = app().await;
        let story = crate::story::StoryRef {
            id: "sc-1".into(),
            url: "https://tracker/story/1".into(),
        };
        app.inner
            .write()
            .await
            .stories
            .put(10001, "PRRT_1", story.clone());

        let mut t = thread("PRRT_1", Some("a.ts"), Some(1), "john");
        t.comments.push(comment(
            101,
            "kars",
            &crate::forge::with_footer(&format!("Tracked: {}", story.link())),
        ));
        let fresh = fetched(vec![t]);

        let draft = StoryDraft {
            title: "t".into(),
            body: "b".into(),
        };
        let out = post_one(
            &app,
            &no_write_forge(),
            &app.cfg.main_checkout.clone(),
            10001,
            "PRRT_1",
            &format!("Tracked: {}", crate::proposal::STORY_TOKEN),
            Some(&draft),
            &fresh,
        )
        .await
        .unwrap();
        // Matched the substituted body, so `{story}` never reached the write.
        assert_eq!(out, Posted::AlreadyThere);
    }

    /// A thread the fetch no longer carries has no comment to answer, and that is
    /// an error rather than a silent skip.
    #[tokio::test]
    async fn a_vanished_thread_refuses_rather_than_posting_nowhere() {
        let app = app().await;
        let fresh = fetched(vec![thread("PRRT_9", Some("a.ts"), Some(1), "john")]);
        let err = post_one(
            &app,
            &no_write_forge(),
            &app.cfg.main_checkout.clone(),
            10001,
            "PRRT_1",
            "Resolved.",
            None,
            &fresh,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("PRRT_1"), "{err}");
    }
}
