//! Moves of main's branch, and the sessions that go with it, below `spawn`.
//!
//! Every move of main's branch takes the same steps: the move lock
//! ([`AppState::move_main_or_refuse`]), a re-read under it, who is in main, the git
//! move, and the sessions about the branch carried along. They were repeated by
//! hand in six places, and that cost four bugs. **Two kinds of move, two homes.**
//! The swap and the move out of main relocate *live* sessions, which is `spawn`'s
//! work and needs an undo, so they live in `relocate`. Everything else — parking
//! ([`park_main`]), a PR flow freeing main ([`free_pr_branch`]), a switch of main
//! onto a PR ([`switch_main_to_pr`]) — only ever carries stopped sessions, so it
//! lives here with the shared steps, where `spawn`'s exit watcher can call it
//! without a module cycle.

use anyhow::{bail, Result};
use std::path::PathBuf;
use std::sync::Arc;

use crate::locate::{locate_pr, PrPlace};

use crate::model::*;
use crate::state::AppState;

/// A move of main's branch into a worktree of its own, as it landed.
pub(crate) struct MovedOut {
    pub name: String,
    pub path: PathBuf,
    pub git: crate::git::MovedOut,
    /// Untracked files in main, which `stash create` cannot carry and which stayed.
    pub untracked: Vec<String>,
    /// The stopped sessions in main about the branch that left, read before any
    /// reconcile. Not carried yet: the caller carries them once everything else
    /// has landed, so an undo has nothing of theirs to put back.
    pub records: Vec<SessionId>,
}

/// Move main's branch and its uncommitted work into a new worktree, and put main
/// back on base.
///
/// The git half of every move out of main, and the bookkeeping that has to come
/// with it: the repo's worktree hooks, the new workspace, main forgetting the
/// branch. Holds `AppState::cutting` around the `git worktree add`, which two of
/// the three callers of `move_branch_out` did not, so a move out raced a spare
/// being cut on git's config lock.
///
/// The tree is named for the branch, and uniquified rather than refused, since a
/// tree left behind by earlier work on the same branch is a reason to pick another
/// name, not to stop. Main sitting on base has no branch to hand over, so the tree
/// is named for the work and `move_branch_out` cuts it a branch. `at` overrides
/// both, for a caller whose tree already has a name and a place, as a PR's
/// `pr-<n>` or a recorded tree being cut again where it stood.
pub(crate) async fn move_out(
    app: &Arc<AppState>,
    _held: &crate::state::MainMove<'_>,
    board: Board,
    at: Option<(&str, &std::path::Path)>,
) -> anyhow::Result<MovedOut> {
    let main = app.cfg.main_checkout.clone();
    let base_ref = app.cfg.base();
    let (branch, base_now) = crate::proc::run_blocking("reading main's branch", {
        let (main, base_ref) = (main.clone(), base_ref.clone());
        move || {
            anyhow::Ok((
                crate::git::current_branch(&main)?,
                base_ref.checkout_branch(&main),
            ))
        }
    })
    .await??;
    let stem = if base_now.as_deref() == Some(branch.as_str()) {
        "work".to_string()
    } else {
        branch_leaf(&branch)
    };
    let (name, path) = match at {
        Some((name, path)) => (name.to_string(), path.to_path_buf()),
        None => {
            let name = free_worktree_name(app, &stem);
            let path = app.cfg.worktree_path(&name);
            (name, path)
        }
    };
    crate::worktree::validate_worktree_name(&name)?;
    // The naming Claude Code's own worktrees use, so a branch cut here reads like
    // every other worktree branch in the repo rather than like a special case.
    let new_branch = format!("worktree-{name}");

    let (git, untracked) = {
        let _cutting = app.cutting.lock().await;
        let (main, path, new_branch) = (main.clone(), path.clone(), new_branch.clone());
        let exclude = app.cfg.worktrees_subdir_str();
        crate::proc::run_blocking("moving main's branch out", move || {
            let base = base_ref.checkout_branch(&main).ok_or_else(|| {
                anyhow::anyhow!(
                    "no base branch to put main back on — {base_ref} has not been fetched"
                )
            })?;
            // Listed before the move, because `stash create` cannot carry them and
            // afterwards they are indistinguishable from base's own untracked files.
            let left = crate::git::untracked_in(&main, Some(&exclude))?;
            let moved = crate::git::move_branch_out(&main, &path, &base, &new_branch)?;
            if !left.is_empty() {
                tracing::info!(files = ?left, "untracked files stayed in main");
            }
            anyhow::Ok((moved, left))
        })
        .await??
    };

    // Cut by the daemon, so the repo's WorktreeCreate never fired for it (§ the
    // worktree_setup rule) — the same reason `ensure_pr_worktree` runs these.
    crate::worktree::run_worktree_hooks(app, &path, board).await;
    app.register_worktree(&name, path.clone(), Some(git.branch.clone()))
        .await;
    // Main gave the branch away, and `reconcile` only adds: left in, main would go
    // on claiming a branch that lives in the new tree.
    app.forget_branch(MAIN, &git.branch).await;

    // Read *before* the reconciles: `reconcile` re-stamps a live session's branch
    // from what its tree has checked out now, and the branch has already left.
    let (_, records) = to_carry(app, MAIN, &git.branch).await;

    let _ = app.reconcile(MAIN).await;
    let _ = app.reconcile(&name).await;
    Ok(MovedOut {
        name,
        path,
        git,
        untracked,
        records,
    })
}

/// [`move_out`], with the stopped sessions about the branch carried into the new
/// tree as it lands.
///
/// What a park, a PR flow freeing main, and a switch of main all do, and did by
/// hand three times: they relocate no live session, so nothing can need undoing
/// after the carry and there is no reason to wait. The move out of main and the
/// swap carry their records last instead, because they can still undo.
pub(crate) async fn move_out_carrying(
    app: &Arc<AppState>,
    held: &crate::state::MainMove<'_>,
    board: Board,
    at: Option<(&str, &std::path::Path)>,
) -> Result<MovedOut> {
    let out = move_out(app, held, board, at).await?;
    for id in &out.records {
        carry_record(app, *id, &out.name, &out.path, &out.git.branch).await;
    }
    Ok(out)
}

/// Undo a [`move_out`]: the branch and its work go back into main, and the tree
/// and its workspace are gone.
///
/// The repo's worktree hooks already ran and cannot be taken back; everything they
/// made is inside the tree, which is removed.
pub(crate) async fn move_back(
    app: &Arc<AppState>,
    _held: &crate::state::MainMove<'_>,
    out: &MovedOut,
) -> anyhow::Result<()> {
    let main = app.cfg.main_checkout.clone();
    let (path, git) = (
        out.path.clone(),
        crate::git::MovedOut {
            branch: out.git.branch.clone(),
            base: out.git.base.clone(),
            created: out.git.created,
            wip_error: None,
        },
    );
    let res = crate::proc::run_blocking("moving the branch back into main", move || {
        crate::git::move_branch_back(&main, &path, &git)
    })
    .await
    .and_then(|r| r);
    // The workspace goes with the tree, whichever way the undo ended: a failure after
    // the removal used to leave it registered for a directory that was gone.
    if !out.path.exists() {
        app.inner.write().await.workspaces.remove(&out.name);
    }
    let _ = app.reconcile(MAIN).await;
    if out.path.exists() {
        let _ = app.reconcile(&out.name).await;
    }
    app.notify().await;
    res
}

/// A directory-safe stem from a branch name.
///
/// `feature/some-thing` is `some-thing`: the leaf is what tells two of your
/// branches apart, and the prefix is the same on all of them.
pub(crate) fn branch_leaf(branch: &str) -> String {
    let leaf = branch.rsplit('/').next().unwrap_or(branch);
    let cleaned: String = leaf
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let stem = cleaned.trim_matches('-');
    let stem = if stem.is_empty() { "work" } else { stem };
    stem.chars().take(48).collect()
}

/// `stem`, or the first `stem-N` with no directory of that name.
///
/// Suffixed rather than refused: a tree from earlier work on the same branch may
/// still be sitting there, which is a reason to pick another name, not to stop.
pub(crate) fn free_worktree_name(app: &Arc<AppState>, stem: &str) -> String {
    for n in 1..100 {
        let name = if n == 1 {
            stem.to_string()
        } else {
            format!("{stem}-{n}")
        };
        if !app.cfg.worktree_path(&name).exists() {
            return name;
        }
    }
    stem.to_string()
}

/// Everything in a workspace that belongs to `branch`, split by what moving it costs.
///
/// `.0` is every live session on the branch, each of which has to be *relocated*:
/// killed, re-filed and resumed in the destination. Which of them may go is the
/// caller's call, because main holds one session and a worktree holds any number.
/// `.1` is every other session recorded there on the same branch, which is a field
/// update and a file move with no process in it.
///
/// **Both halves matter, and the second one is the ordinary case.** The old version
/// of this returned only the live pick, so a swap made while nothing was running
/// moved the branch and carried no conversation at all. That is not a rare race:
/// you swap between pieces of work, and the session you are swapping away from is
/// usually the one you just finished with. The symptom was a pane whose transcript
/// and whose changed files were about different stories, days later, with nothing
/// saying why.
///
/// Selection is by branch rather than by recency, which is the point of the field:
/// a worktree outlives the branches that pass through it, so "newest here" and
/// "about the work that is leaving" are different questions. A session with no
/// recorded branch answers neither and stays put.
///
/// A session started as a pass travels too, because a swap is you asking for the
/// whole piece of work. It used to stay behind so that a force-pushing agent never
/// sat in main, which left the branch in main and its conversation in the old
/// worktree. The push guard already refuses a push to the base branch from anywhere.
pub(crate) async fn to_carry(
    app: &Arc<AppState>,
    workspace: &str,
    branch: &str,
) -> (Vec<SessionId>, Vec<SessionId>) {
    let inner = app.inner.read().await;
    let mut live = Vec::new();
    let mut records = Vec::new();
    for s in inner
        .sessions
        .values()
        .filter(|s| s.workspace == workspace)
        .filter(|s| s.branch.as_deref() == Some(branch))
    {
        if s.state.is_live() {
            live.push((s.created_at, s.id));
        } else if s.recovery.is_none() {
            // A recovery record describes a worktree that was torn down, so the
            // session is not *in* either tree here and which branch sits where
            // has nothing to do with it. `worktree::branch_drift` already says
            // its piece when one of those is resumed.
            records.push(s.id);
        }
    }
    // Oldest first, so the order sessions arrive in is the order they were made.
    live.sort_by_key(|(created, _)| *created);
    (live.into_iter().map(|(_, id)| id).collect(), records)
}

/// What an agent is told when its conversation has been moved.
///
/// Two halves on purpose. The factual one the daemon can always say: which branch
/// you are on, where it is now, where you were reading before. The second is
/// `workspace_notes`, which is the only part that knows anything about a particular
/// repo (that main is where the dev stack runs, say), and it comes from that
/// repo's config rather than from anything here.
///
/// Written as an instruction rather than a status line because that is what it is:
/// the paths the agent has been using are stale from this point on, and it will
/// reach for one on its very next tool call unless it is told not to.
pub(crate) fn arrival_notice(
    app: &Arc<AppState>,
    branch: &str,
    from: &std::path::Path,
    to: &std::path::Path,
    into_main: bool,
) -> String {
    let mut note = format!(
        "This conversation has been moved. The branch {branch} was carried into another \
         checkout and this session followed it: your working directory is now {}, and \
         until this move it was {}. What travelled is that branch and its uncommitted \
         work. If you had been editing on a *different* branch, those edits are still \
         in the tree you came from — check before you assume either way, and say what \
         you find rather than moving anything.",
        to.display(),
        from.display(),
    );
    /* Claude Code pins worktree isolation in the transcript (a `worktree-state`
    record, re-appended every turn), so a conversation started by `claude
    --worktree` would go on refusing every git command aimed anywhere but that
    original worktree — including the tree it has just been moved into.
    `store::clear_worktree_pin` has already released it by the time this is read:
    `spawn_session` calls it on resume whenever the pin disagrees with the cwd, and
    a relocation is a resume.

    So this says what happened and asks for nothing. It used to tell the agent to
    call `ExitWorktree` first, on the belief that the daemon could not clear the pin
    from outside. That belief was wrong — see `clear_worktree_pin`, measured against
    128 transcripts — and the instruction outlived it, with two costs. The tool
    answers "No-op: there is no active EnterWorktree session to exit", which is the
    truth and reads as a failure; and an agent that has just been told it is
    isolated, and then told it is not, concludes the relocation did not happen. One
    went looking for its work in the old worktree and started moving by hand what
    it thought had been left. Telling it not to call the tool is the point of naming
    the tool at all.

    What this must *not* do is over-correct into reassurance. A first draft said the
    files had come with it and there was nothing to move — and a real session then
    proved that wrong: it followed the branch it was recorded on while its own edits
    sat on another branch in the tree it left. The daemon knows which branch it
    moved; it does not know which branch the agent was editing. So this states the
    first and asks the agent to establish the second. */
    note.push_str(
        " Claude Code's worktree isolation for this session has already been released, \
         so do not call `ExitWorktree` or `EnterWorktree`, and do not re-run the move: \
         git in this directory works now. Treat remembered absolute paths as stale and \
         re-read anything you are about to change.",
    );
    if let Some(extra) = app.settings().workspace_notes.for_main(into_main) {
        note.push(' ');
        note.push_str(extra);
    }
    note
}

/// Move a session that is not running. The record follows its branch, and the
/// transcript follows the record.
///
/// Separate from `spawn::relocate_session` because there is no pty to kill and no
/// `--resume` to watch stay up, so none of that machinery applies and none of its
/// failure modes exist. Auto-resume brings this conversation back in the directory
/// recorded here, which is the whole reason the record has to move at all.
///
/// The transcript move is best effort for the reason `store::move_transcript`
/// documents: `--resume` resolves a conversation by id wherever the file sits, so a
/// failure costs the slug lookup its cheap path and nothing else.
pub(crate) async fn carry_record(
    app: &Arc<AppState>,
    id: SessionId,
    dest: &str,
    dest_path: &std::path::Path,
    branch: &str,
) {
    let src_cwd = {
        let inner = app.inner.read().await;
        match inner.sessions.get(&id) {
            Some(s) => s.cwd.clone(),
            None => return,
        }
    };
    // Off the runtime: a rename is cheap, but the fallback across filesystems is a
    // whole-file copy, and a transcript is megabytes of turns.
    let moved = {
        let (from, to) = (src_cwd.clone(), dest_path.to_path_buf());
        crate::proc::run_blocking("re-filing the transcript", move || {
            crate::store::move_transcript(id, &from, &to)
        })
        .await
        .unwrap_or_else(Err)
    };
    let refiled = match moved {
        Ok(moved) => moved,
        Err(e) => {
            tracing::warn!(
                session = %id,
                "could not re-file the transcript under {}; the record moves anyway: {e:#}",
                dest_path.display()
            );
            None
        }
    };
    let notice = arrival_notice(app, branch, &src_cwd, dest_path, dest == MAIN);
    let mut inner = app.inner.write().await;
    if let Some(s) = inner.sessions.get_mut(&id) {
        s.workspace = dest.to_string();
        s.cwd = dest_path.to_path_buf();
        // The branch it was carried *for*, written now rather than left for the next
        // reconcile to infer from whatever the tree holds.
        //
        // `to_carry` selects on this field, so until it is right the record names the
        // branch this conversation just left. A second move inside that window asks
        // "who here was working on the branch that is leaving" and gets the wrong
        // answer — it matches a session whose branch has already gone, or fails to
        // match the one that should travel and strands it. That is how a conversation
        // ends up in one checkout with its uncommitted work in another, which is the
        // whole failure this pairing exists to prevent.
        s.branch = Some(branch.to_string());
        // Only on a move that happened. Left pointing at the old slug otherwise,
        // which is still where the file is.
        if let Some(path) = refiled {
            s.transcript_path = Some(path);
        }
        // Waits here until auto-resume starts the conversation again, which is the
        // first moment there is an agent to tell.
        s.arrival_notice = Some(notice);
    }
}

/// Move PR `pr`'s branch out of main into the tree `name` at `path`, because main
/// holding it is the one case where cutting the tree and freeing main are the same
/// act.
///
/// `park_main` leaves a dirty main where it is when no session is about the branch
/// — correctly, there is no conversation to keep the work beside — and the branch
/// then sat there for days making every PR flow for it impossible, which is the
/// state the PR flows used to bail out of and send you off to stash. Moving it out
/// loses nothing: the branch *and* its uncommitted work land in the tree the flow
/// was about to create anyway. Untracked files stay in main, which
/// `git::move_branch_out` documents and this cannot help.
///
/// **It moves main, so it takes the move lock**, and asks the whole question again
/// under it: a swap this refuses may have just finished, and then a worktree
/// usually holds the branch — cutting `pr-N` beside it would put the branch in two
/// trees, which git refuses. Refused rather than queued, like the swap.
///
/// Answers the workspace that holds the branch afterwards — the tree it moved
/// into, registered by [`move_out`], or the worktree a finished swap put it in —
/// and `None` when main no longer holds it and no tree does, so the caller cuts one.
///
/// **Call it only once a read has put the branch in main.** Taking the lock for
/// every PR flow would refuse an ordinary worktree cut — which touches main not at
/// all — for the length of any swap.
pub(crate) async fn free_pr_branch(
    app: &Arc<AppState>,
    pr: u64,
    head_ref: &str,
    name: &str,
    path: &std::path::Path,
) -> Result<Option<String>> {
    let held = app.move_main_or_refuse()?;
    match locate_pr(app, pr, head_ref).await?.place {
        PrPlace::Worktree { ws, path } if path.is_dir() => return Ok(Some(ws)),
        PrPlace::Main => {}
        _ => return Ok(None),
    }
    refuse_if_main_is_busy(app, pr, head_ref).await?;
    let out = move_out_carrying(app, &held, Board::Loud, Some((name, path))).await?;
    /* A log line rather than something in the response, for `park_main`'s reason:
    the checkout under every worktree just changed and that is worth recording, but
    there are several callers of this and threading a warning up through all of
    them buys little. `wip_error` is close to impossible here anyway — the work is
    re-applied onto a fresh checkout of the branch it came from, so the apply lands
    on the tree it was taken from, which is the same argument `swap_branches` makes. */
    tracing::info!(
        %head_ref, wip_error = ?out.git.wip_error, sessions = out.records.len(),
        "main was on #{pr}'s branch, so it moved into {name} and main went back to {}",
        out.git.base
    );
    Ok(Some(name.to_string()))
}

/// Refuse a PR flow only when main holds something that cannot be moved.
///
/// A **live session** is that thing: the checkout it is working in is not ours to
/// change under it, and no amount of git makes that safe.
///
/// Uncommitted changes used to be refused here too, which is what made a closed
/// session with a dirty tree a dead end — the branch stayed in main, and every PR
/// flow for it was impossible until you went and stashed by hand. They are carried
/// now; see [`free_pr_branch`].
async fn refuse_if_main_is_busy(app: &Arc<AppState>, pr: u64, head_ref: &str) -> Result<()> {
    if app.main_in_use_now().await.is_none() {
        return Ok(());
    }
    bail!(
        "the main checkout is on {head_ref} and a session is still open there, so the branch \
         cannot move out of it — and git will not check a branch out twice, which leaves no \
         worktree to cut for #{pr}. Close that session, or move it out of main from its context \
         menu, and try again."
    )
}

/// Send the main checkout back to the base branch once nobody is working in it.
///
/// "Open in main" moves the checkout onto a PR's branch and, before this, nothing
/// ever moved it off: main would stand on a feature branch for days, quietly
/// making every PR flow for that branch impossible, because the worktree those
/// flows need cannot be cut while main holds the branch.
///
/// Deliberately at the *last* session's exit rather than at the moment a PR flow
/// wants the branch. Both fix the collision; this one stops it existing, and it
/// moves the checkout when you have just finished with it rather than in the
/// middle of something else.
///
/// Stays put when a session is still open in main, which is someone still using
/// it, and when main is dirty with no session about its branch, since uncommitted
/// work nobody is talking about is not ours to move. A dirty main *with* sessions
/// about the branch moves out with them instead. Silence is the right answer to
/// the refusals: nothing was promised, and [`refuse_if_main_is_busy`] explains it
/// if a PR flow later needs the branch.
pub(crate) async fn park_main(app: &Arc<AppState>) {
    // A restart is not "you are done with main". Auto-resume brings that session
    // back expecting the branch it was working on, and moving the checkout here
    // would hand it someone else's code.
    if app.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    // Nor while auto-resume is still bringing a session back into main: it expects
    // the branch it left on, the same reason a shutdown does not park.
    if app.main_in_use_now().await.is_some() {
        return;
    }
    /* **This moves main's branch, and possibly a worktree's, so it is a swap.**
    `AppState::swapping`'s own doc has the rule: every swap involves main, so two
    are never independent — and this one reclaims the base from a worktree
    (`git::release_branch`) as well, which is a second pair a swap of that tree
    would race.

    **Skipped, not refused, and that is not a style choice.** This runs from a
    detached exit watcher: there is no request to answer and nobody to read a
    refusal, so waiting would park main long after the session that prompted it,
    against state the swap has since changed. A skip costs nothing that is not
    already recoverable — the branch stays in main until the next session there
    closes, and the swap holding the lock is itself moving main. */
    let Some(held) = app.move_main() else {
        tracing::info!("main stays where it is: a swap is moving it");
        return;
    };
    // Asked again now that the lock is held, and reserved in the same step: the
    // check above is a cheap early out, and a session could claim main between it
    // and here. Held to the end of the park.
    let Some(_reserved) = app.reserve_main_for_park().await else {
        return;
    };
    let path = app.cfg.main_checkout.clone();
    let base_ref = app.cfg.base();
    let exclude = app.cfg.worktrees_subdir_str();

    /* **The base has to be free, and main is the only checkout that may hold it.**
    Git allows one checkout per branch, so a worktree sitting on `develop` makes
    "main goes back to base" impossible — not refused, *impossible* — and every
    flow that needs main on base is blocked until somebody notices. A swap is how
    it happens: main resting on base, a worktree swapped in, and base goes out as
    the exchange. Reported as `fatal: 'develop' is already used by worktree at …`
    from four calls deep, days later.

    Reclaimed rather than prevented at the swap, because pressing swap twice has
    to stay the undo — the exchange is the feature. So the branch comes home at
    the moment it is needed, and only from a tree **nobody is working in**: the
    content does not move (`release_branch` cuts at the same commit), but a name
    changing under a live agent is a surprise the log cannot undo. A tree that is
    busy leaves main where it is, and says so.

    **Asked in the same breath as "can main park at all", and that order is the
    whole of it.** Releasing first and finding out afterwards renames a
    worktree's branch for a park that then does not happen — `park_on_base`
    returns `Ok(None)` on a dirty main, silently and by design, so a session
    closed after editing files in main would have undone the swap for nothing
    and said only that the tree "is on worktree-w now". Nothing moves unless
    everything can. */
    let plan = {
        let (at, base_ref, exclude) = (path.clone(), base_ref.clone(), exclude.clone());
        tokio::task::spawn_blocking(move || {
            let base = base_ref.checkout_branch(&at)?;
            // Already there: nothing to park and nothing to clear the way for.
            let branch = crate::git::current_branch(&at).ok()?;
            if branch == base {
                return None;
            }
            let clean = crate::git::is_clean_excluding(&at, Some(&exclude)).ok()?;
            let holder = crate::git::holder_of_branch(&at, &base)
                .ok()
                .flatten()
                .filter(|h| h != &at);
            Some((base, branch, clean, holder))
        })
        .await
        .ok()
        .flatten()
    };
    // Nothing parkable: already on base, or no base ref fetched yet.
    let Some((base, branch, clean, holder)) = plan else {
        return;
    };
    /* **The sessions about the branch go with it, and then so does the branch.**
    The session that just closed is one of them: it is a stopped record by now,
    still in main and still about this branch. Parking main on base under them left
    each one to be resumed later on base, with nothing to say so, and the next
    reconcile erased which branch it had been about. So when any session is about
    the branch, the branch moves out into a tree of its own the way "move out of
    main" moves it, uncommitted work included, and they follow.
    With nobody about it, the branch stays a branch and main parks as before — and a
    dirty main with nobody about it is still left alone: there is no conversation to
    keep the work beside, and moving it would be moving your edits unasked. */
    let (_, about) = to_carry(app, MAIN, &branch).await;
    if !clean && about.is_empty() {
        return;
    }
    let holder = holder.map(|tree| (tree, base.clone()));
    if let Some((tree, base)) = holder {
        let ws = app.workspace_for_path(&tree).await;
        let busy = match &ws {
            Some(id) => !app.live_sessions_in(id).await.is_empty(),
            // No workspace of ours: a tree the daemon does not manage, and moving a
            // branch in it is not the daemon's business at all.
            None => true,
        };
        if busy {
            tracing::warn!(
                "main stays off {base}: {} has it checked out{}",
                tree.display(),
                match ws {
                    Some(_) => " and a session is live there",
                    None => ", and it is not a worktree this daemon manages",
                }
            );
            return;
        }
        let stem = format!(
            "worktree-{}",
            tree.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        let at = tree.clone();
        match tokio::task::spawn_blocking(move || crate::git::release_branch(&at, &stem)).await {
            Ok(Ok(fresh)) => {
                /* The record follows the branch, because `reconcile` only ever
                *adds* to a workspace's set: left in, that tree would go on
                claiming the base for good, and two workspaces claiming it is
                what `AppState::worktree_holding` and the snapshot's PR lookup both read.
                `move_out_of_main` does the same for the same reason. */
                if let Some(id) = &ws {
                    app.forget_branch(id, &base).await;
                    /* And the conversations in that tree, for the same reason one
                    level down: a stopped record there still says it is about the
                    base, and `to_carry` selects on that field — so the next move of
                    main would match it as the base's conversation and carry it
                    into main, away from the tree its work is in. The tree's
                    content did not move, only its branch's name, so the record
                    takes the new name. Persisted by the notify. */
                    {
                        let mut inner = app.inner.write().await;
                        for s in inner.sessions.values_mut() {
                            if s.workspace == *id && s.branch.as_deref() == Some(base.as_str()) {
                                s.branch = Some(fresh.clone());
                            }
                        }
                    }
                    app.notify().await;
                }
                tracing::info!(
                    "{} was on {base} and main needs it; it is on {fresh} now",
                    tree.display()
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    "main stays off {base}: {} could not let go of it: {e:#}",
                    tree.display()
                );
                return;
            }
            Err(e) => {
                tracing::warn!("releasing {base} panicked: {e}");
                return;
            }
        }
    }

    if !about.is_empty() {
        match move_out_carrying(app, &held, Board::Quiet, None).await {
            Ok(out) => {
                tracing::info!(
                    branch = %out.git.branch, tree = %out.name, sessions = out.records.len(),
                    wip_error = ?out.git.wip_error,
                    "the last session in main closed; its branch moved out with its sessions and main is back on {}",
                    out.git.base
                );
                app.notify().await;
            }
            Err(e) => tracing::warn!("main stays on {branch}: moving it out failed: {e:#}"),
        }
        return;
    }

    /* Whatever main holds, not only a branch `open_pr(main)` put there. It used to
    park on provenance — a mark set by that one flow — so a swapped-in branch or
    a hand-checkout stayed in main for good, on the reasoning that parking it
    would undo the swap. But nobody is working it: the last session just left, and
    a branch parked in main with no session on it blocks every PR flow that needs
    main on base, which is the thing that goes wrong far away from here. The
    branch is not lost either — it is still a branch, and `move_branch_out` is how
    it gets a tree if you want one.

    `park_on_base` re-checks the dirty tree itself, which is not a duplicate
    worth removing: the plan above was read before the base was reclaimed, and
    this is the check that runs against the tree as it stands now. */
    let moved = tokio::task::spawn_blocking(move || {
        match crate::git::park_on_base(&path, &base, Some(&exclude)) {
            Ok(was) => was,
            Err(e) => {
                tracing::warn!("main could not go back to {base}: {e:#}");
                None
            }
        }
    })
    .await
    .ok()
    .flatten();

    if let Some(was) = moved {
        // Said out loud: the checkout under every worktree just changed, and the
        // rail showing `develop` with no explanation is a worse surprise than a
        // line in the log.
        tracing::info!(from = %was, "the last session in main closed; main is back on its base");
        let _ = app.reconcile(MAIN).await;
    }
}

/// Move the main checkout onto a PR's branch, so a session can open there.
///
/// The sibling of [`crate::spawn::ensure_pr_worktree`]: both answer "get me onto this PR's
/// code", and they returned asymmetric shapes only because this one was inlined
/// in the handler while the other was a call. Refuses rather than half-doing it
/// — main is exclusive, and switching the one tree every worktree is cut from
/// under uncommitted work is not recoverable by pressing back.
pub(crate) async fn switch_main_to_pr(
    app: &Arc<AppState>,
    held: &crate::state::MainMove<'_>,
    pr: u64,
    head_ref: &str,
) -> Result<String> {
    // From the one answer, as `ensure_pr_worktree` starts: a switch by name would
    // put main on somebody else's branch of that name, and a branch a worktree
    // holds cannot be checked out twice — refused by name here rather than by git.
    let located = locate_pr(app, pr, head_ref).await?;
    match &located.place {
        PrPlace::Foreign(why) => bail!("{}", cannot_open_pr(pr, why)),
        PrPlace::Worktree { ws, .. } => {
            bail!("#{pr}'s branch is in the worktree {ws}; open it there, or swap it into main")
        }
        PrPlace::Main | PrPlace::Nowhere => {}
    }
    // The one question every move of main asks (`AppState::main_in_use`): a live
    // session, one auto-resume is bringing back, or one claimed and still starting.
    if let Some(held) = app.main_in_use_now().await {
        bail!(
            "a session already holds main ({}); end it before moving the checkout",
            crate::model::short_id(&held)
        );
    }

    let path = app.cfg.main_checkout.clone();
    let branch = head_ref.to_string();
    // Excluding the worktrees dir: main contains it, so plain `is_clean` reads main
    // as dirty on any repo that has not gitignored it, and this refused forever.
    let exclude = app.cfg.worktrees_subdir_str();

    /* **Refused before anything moves.** The check used to come after the move out
    below, and a move out leaves untracked files in main, which this check counts:
    so an untracked file moved your branch into a new tree and *then* refused. */
    let clean = {
        let (at, exclude) = (path.clone(), exclude.clone());
        crate::proc::run_blocking("checking main is clean", move || {
            crate::git::is_clean_excluding(&at, Some(&exclude))
        })
        .await??
    };
    if !clean {
        bail!("the main checkout has uncommitted changes; commit or stash them first");
    }

    /* **The branch main is leaving goes with the sessions about it.** A stopped
    session in main about the branch that is on it now would otherwise stay in main
    while main switches to someone else's branch under it. So when one is there, the
    branch moves into a tree of its own first, as a park would have moved it, and
    main is on base when the switch below runs. Base itself is not work and stays. */
    let (on, base) = {
        let (at, base_ref) = (path.clone(), app.cfg.base());
        crate::proc::run_blocking("reading main's branch", move || {
            (
                crate::git::current_branch(&at).ok(),
                base_ref.checkout_branch(&at),
            )
        })
        .await?
    };
    if let Some(on) = on.filter(|on| Some(on) != base.as_ref() && on != head_ref) {
        let (_, about) = to_carry(app, MAIN, &on).await;
        if !about.is_empty() {
            let out = move_out_carrying(app, held, Board::Loud, None).await?;
            tracing::info!(
                branch = %out.git.branch, tree = %out.name, sessions = out.records.len(),
                "main is leaving {on} for {head_ref}, so {on} moved out with its sessions"
            );
        }
    }

    let from = located.from;
    tokio::task::spawn_blocking(move || -> Result<()> {
        crate::git::switch_branch_from(&path, &branch, from.as_deref())
    })
    .await
    .map_err(|e| anyhow::anyhow!("switch task failed: {e}"))??;

    // The pane must be right about what is checked out the moment it changes.
    let _ = app.reconcile(MAIN).await;
    Ok(MAIN.to_string())
}
