//! Carrying sessions with their branch, below `spawn`.
//!
//! Every move of main's branch has to take the sessions about it along: the swap,
//! the move out of main, main going back to base, and the PR routes that move a
//! branch out of main. The swap and the move out relocate live sessions, which is
//! `spawn`'s work, so they live in `relocate`. Parking and the PR routes run from
//! inside `spawn` and only ever carry stopped sessions, so what they share with the
//! swap lives here, where `spawn` can call it without a module cycle.

use std::path::PathBuf;
use std::sync::Arc;

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
/// is named for the work and `move_branch_out` cuts it a branch.
pub(crate) async fn move_out(app: &Arc<AppState>, board: Board) -> anyhow::Result<MovedOut> {
    let main = app.cfg.main_checkout.clone();
    let base_ref = app.cfg.upstream_ref.clone();
    let (branch, base_now) = crate::proc::run_blocking("reading main's branch", {
        let (main, base_ref) = (main.clone(), base_ref.clone());
        move || {
            anyhow::Ok((
                crate::git::current_branch(&main)?,
                crate::git::base_checkout_branch(&main, &base_ref),
            ))
        }
    })
    .await??;
    let stem = if base_now.as_deref() == Some(branch.as_str()) {
        "work".to_string()
    } else {
        branch_leaf(&branch)
    };
    let name = free_worktree_name(app, &stem);
    crate::worktree::validate_worktree_name(&name)?;
    let path = app.cfg.worktree_path(&name);
    // The naming Claude Code's own worktrees use, so a branch cut here reads like
    // every other worktree branch in the repo rather than like a special case.
    let new_branch = format!("worktree-{name}");

    let (git, untracked) = {
        let _cutting = app.cutting.lock().await;
        let (main, path, new_branch) = (main.clone(), path.clone(), new_branch.clone());
        let exclude = app.cfg.worktrees_subdir_str();
        crate::proc::run_blocking("moving main's branch out", move || {
            let base = crate::git::base_checkout_branch(&main, &base_ref).ok_or_else(|| {
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

/// Undo a [`move_out`]: the branch and its work go back into main, and the tree
/// and its workspace are gone.
///
/// The repo's worktree hooks already ran and cannot be taken back; everything they
/// made is inside the tree, which is removed.
pub(crate) async fn move_back(app: &Arc<AppState>, out: &MovedOut) -> anyhow::Result<()> {
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
    crate::proc::run_blocking("moving the branch back into main", move || {
        crate::git::move_branch_back(&main, &path, &git)
    })
    .await??;
    app.inner.write().await.workspaces.remove(&out.name);
    let _ = app.reconcile(MAIN).await;
    app.notify().await;
    Ok(())
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
