//! Moving a branch, its worktree and its conversation between checkouts.
//!
//! The swap, the move out of main, and the one rule they share: **a conversation
//! travels with its branch**. `to_carry` decides which session that is, and
//! `carry_record` moves the record, the transcript and the arrival notice
//! together — so a branch never lands in one checkout with its agent still
//! pointed at another.
//!
//! **Split out of `api` because almost none of it is HTTP.** Two handlers sit on
//! top of a multi-step git transaction with seven helpers of its own, and it read
//! as eight hundred lines in the middle of the route table. Nothing here is new;
//! the handlers keep their paths.

use axum::{
    extract::{Path, State},
    Json,
};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

use crate::api::{refuse, ApiError, ApiResult};
use crate::model::*;
use crate::spawn;
use crate::state::AppState;

/// Log every outcome, not only the good one.
///
/// A swap has six ways to refuse — main occupied, an agent mid-turn, a stopped
/// rebase, an unknown workspace, a concurrent swap, git itself — and every one of
/// them used to reach you as a toast and leave nothing behind. So "it did not move
/// the worktree that time, and worked when I pressed it again" had no record to
/// read afterwards, which is the one thing needed to tell a refusal from a bug.
pub async fn swap_with_main(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    tracing::info!(%workspace, "swap requested");
    let out = swap_with_main_inner(app, workspace.clone()).await;
    if let Err(e) = &out {
        // Warn, not error: most of these are the daemon correctly declining, and a
        // refusal you can read is the point rather than a fault to page about.
        tracing::warn!(%workspace, "swap refused: {:#}", e.0);
    }
    out
}

/// Exchange the branches checked out in main and a worktree.
///
/// What it is *for*: main is where the managed processes and the dev stack live,
/// so work that needs them has to be *in* main. Before this the only way was to
/// commit, push, and check the branch out by hand.
///
/// # What it is not
///
/// Not a swap of roles. Worktrees live inside main, so one cannot become the
/// primary checkout without containing its own parent, and git will not move the
/// main worktree anyway. Only what each has checked out is exchanged, and both
/// directories stay exactly where they are.
///
/// # Both ways
///
/// The branches trade places, and so do the conversations about them: the
/// worktree's session is relocated into main and main's own session, if it had one,
/// is relocated out to the worktree. Symmetry is the point — a swap that moved one
/// side only left main's session reading a tree that had changed under it.
///
/// Relocating keeps the session id, so each conversation continues as one rail row
/// rather than gaining a forked sibling; `spawn::relocate_session` has the how, and
/// falls back to a fork only if a resume will not stay up.
///
/// # The refusals
///
/// An unclean tree, a stopped rebase, or a session **mid-turn** in either.
///
/// Mid-turn, not merely open: swapping is a regular move, and a session sitting at
/// its prompt in main is the normal state to do it from. Refusing on any live
/// session would make the ordinary case fail. An idle session's next turn re-reads
/// the tree; an agent that is *writing* into one that changes under it is the case
/// worth stopping, and it is the same distinction `nudge_sessions` already draws
/// with `is_busy`.
///
/// Main having no session at all is equally fine — nothing here requires one.
async fn swap_with_main_inner(
    app: Arc<AppState>,
    workspace: String,
) -> ApiResult<serde_json::Value> {
    if workspace == MAIN {
        refuse!("main cannot be swapped with itself");
    }
    // One swap at a time, and refused rather than queued: a swap kills and respawns
    // the conversations that follow the branches, and it decides which ones those
    // are *before* anything moves. A second swap taken while the first is still
    // landing reads session state mid-relocation, matches nobody, and moves a branch
    // without its conversation — which is how a double click left a session in main
    // with its branch back in the worktree. Queueing would run the second one on the
    // strength of what you saw before the first, which is not what you would ask for
    // if you could see the result.
    let _swap = app.swapping.try_lock().map_err(|_| {
        ApiError(anyhow::anyhow!(
            "a swap is already running; it moves whole checkouts and their \
             conversations, so give it a moment and look at the rail before asking again"
        ))
    })?;
    let tree = app
        .workspace_path(&workspace)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?;
    let main = app.cfg.main_checkout.clone();

    // Sessions first: the cheapest refusal. Only the ones actually working — an
    // idle session at its prompt is the normal place to swap from.
    for (label, ws) in [("main", MAIN), ("this worktree", workspace.as_str())] {
        if let Some(who) = app.busy_session_in(ws).await {
            refuse!(
                "{label} has an agent mid-turn ({who}); the swap replaces every file \
                 under it, so let that turn finish first"
            );
        }
    }

    /* **Who travels is decided from state read before anything moves.**
    `to_carry` matches a session by `Session::branch`, and `AppState::reconcile`
    re-stamps that field for every *live* session from whatever its tree has
    checked out now — the right rule everywhere else, and exactly wrong in the
    window this flow opens.
    It used to be read after the exchange, which is safe against the reconcile
    *this* function runs and not against the background sweep, which holds
    `AppState::sweeping` rather than `swapping` and so runs straight through a
    swap. On the monorepo this was written against, a sweep is ~9s over 78
    worktrees and they run back to back, so that window is open almost always:
    observed live, a swap moved both branches and carried nothing
    (`into_main=None into_worktree=None`), and the next swap put the branches
    back and moved the conversation — leaving a session in main whose branch had
    gone home, with only a WARN to say so.
    Reading first removes the race rather than narrowing it: who was working on
    the branch that is about to leave is knowable before it leaves, and once the
    ids are captured a re-stamp cannot change the answer. */
    let (m0, t0) = (main.clone(), tree.clone());
    let (main_was, tree_was) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(String, String)> {
            Ok((
                crate::git::current_branch(&m0)?,
                crate::git::current_branch(&t0)?,
            ))
        })
        .await
        .map_err(|e| anyhow::anyhow!("reading the branches panicked: {e}"))??;
    // By branch, not by address: each side asks "who here is working on the branch
    // that is about to move out". Both are picked before either moves, because
    // choosing as we go would let the second choice see the session the first one
    // just delivered — for the moment in between, both live in the worktree — and
    // send it straight back.
    let (outgoing, outgoing_records) = to_carry(&app, MAIN, &main_was).await;
    let (incoming, incoming_records) = to_carry(&app, &workspace, &tree_was).await;
    // Counted here because the loops below consume the vectors, and the log at the
    // end is the only place this number is ever read.
    let carried_records = outgoing_records.len() + incoming_records.len();

    // Uncommitted work is carried, not refused — see `git::swap_branches`. Only a
    // stopped rebase is still a refusal: a tree mid-rebase cannot switch at all.
    let (m, t) = (main.clone(), tree.clone());
    let (swapped, untracked) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(crate::git::Swap, Vec<String>)> {
            for (label, path) in [("the main checkout", &m), ("this worktree", &t)] {
                if crate::git::rebase_in_progress(path) {
                    anyhow::bail!(
                        "{label} has a rebase stopped part-way; finish or abort it first"
                    );
                }
            }
            // Listed before the swap, because afterwards they are indistinguishable
            // from whatever the other branch leaves untracked. `stash create` does
            // not take untracked files, so these stay put and are named rather than
            // quietly not moving.
            let left = crate::git::untracked_in(&t, None)?;
            let swapped = crate::git::swap_branches(&m, &t)?;
            Ok((swapped, left))
        })
        .await
        .map_err(|e| anyhow::anyhow!("the swap task panicked: {e}"))??;

    /* The identity a swap is: what main holds now is what the worktree held, and
    the other way round. If that does not hold, something moved between the read
    above and the exchange — a hand-typed `git checkout` in either tree — and the
    carriers were picked against a world that has since changed. Said out loud
    rather than guarded, because the exchange has already happened and the
    branches are where git says they are; what is uncertain is only who should
    follow them. */
    if swapped.main_now != tree_was || swapped.worktree_now != main_was {
        tracing::warn!(
            %workspace,
            "the branches moved between reading them ({main_was} in main, {tree_was} here) \
             and the exchange ({} in main, {} here), so a conversation may have stayed \
             behind; check the rail",
            swapped.main_now,
            swapped.worktree_now
        );
    }

    // Each tree gave a branch away, and `reconcile` only adds. Left in, the
    // worktree would go on claiming the branch main now holds, and a PR flow for it
    // would be pointed at the wrong tree — found by driving this against a real
    // daemon, not by reading it.
    //
    // This and everything after it runs even when the WIP did not re-apply. The
    // branches moved; refusing to record that would leave the daemon describing a
    // world git no longer agrees with, which is worse than the failure itself.
    app.forget_branch(&workspace, &swapped.main_now).await;
    app.forget_branch(MAIN, &swapped.worktree_now).await;

    // Both panes describe a tree whose every file just changed.
    let _ = app.reconcile(MAIN).await;
    let _ = app.reconcile(&workspace).await;

    // The conversations follow their branches, in both directions, or the swap only
    // moves half of what you meant.
    //
    // Main's session travels too: its branch is in the worktree now, and a
    // conversation left staring at a tree that changed under it is the half of the
    // old behaviour that made this a one-way move rather than a swap.

    // Out of main **first**, and the order is load-bearing rather than tidy: main
    // holds one session at a time, so while the outgoing one is still sitting there
    // the arrival is refused outright ("main is occupied by …"). Vacating makes the
    // room. Found by driving a two-way swap against a real daemon, not by reading it.

    let into_worktree = match outgoing {
        Some(id) => Some(spawn::relocate_session(&app, id, &workspace, CARRY_GRACE).await),
        None => None,
    };
    let into_main = match incoming {
        Some(id) => Some(spawn::relocate_session(&app, id, MAIN, CARRY_GRACE).await),
        None => None,
    };

    // The conversations that were not running. No process work, so these cannot
    // fail the swap and are not reported back as a carry: the rail simply shows
    // them where their branch went.
    for id in outgoing_records {
        carry_record(&app, id, &workspace, &tree, &swapped.worktree_now).await;
    }
    for id in incoming_records {
        carry_record(&app, id, MAIN, &main, &swapped.main_now).await;
    }

    // The relocated ones are running, so they are told the same thing the records
    // are. Set after the resume, because `spawn_session` rebuilds the record under
    // the same id and would overwrite a notice left on the session it replaced.
    for (moved, branch, from, to, into_main) in [
        (&into_worktree, &swapped.worktree_now, &main, &tree, false),
        (&into_main, &swapped.main_now, &tree, &main, true),
    ] {
        let Some(Ok(moved)) = moved else { continue };
        let notice = arrival_notice(&app, branch, from, to, into_main);
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&moved.id) {
            s.arrival_notice = Some(notice);
        }
    }
    app.notify().await;

    // Where to land the pane: main is what you pressed this for, so the session that
    // arrived there wins, and the one that left main is the fallback.
    let select = into_main
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .or_else(|| into_worktree.as_ref().and_then(|r| r.as_ref().ok()))
        .map(|r| r.id.to_string());

    tracing::info!(
        %workspace,
        main_now = %swapped.main_now,
        worktree_now = %swapped.worktree_now,
        wip_error = ?swapped.wip_error,
        // Which conversations travelled, because "it did not move" can mean the
        // branches or the sessions, and the two have different causes.
        into_main = ?into_main.as_ref().and_then(|r| r.as_ref().ok()).map(|r| r.id),
        into_worktree = ?into_worktree.as_ref().and_then(|r| r.as_ref().ok()).map(|r| r.id),
        carried_records,
        "swapped branches with main"
    );
    // Both trees have been reconciled by here, so their branches are current.
    check_moves_landed(&app, "the swap").await;
    Ok(Json(json!({
        "main": swapped.main_now,
        "worktree": swapped.worktree_now,
        "workspace": workspace,
        "select": select,
        "into_main": carried_json(&into_main),
        "into_worktree": carried_json(&into_worktree),
        // A partial success, said as one: the branches moved, this did not. The
        // message names the WIP commit the work is still in.
        "wip_error": swapped.wip_error,
        // Named, not counted: knowing *which* files stayed behind is the difference
        // between going to fetch them and wondering what you lost.
        "untracked_left": untracked,
    })))
}

/// How long a relocated session has to prove it stayed up.
///
/// A grace window, not a health check: what has to be ruled out is the *instant*
/// exit of a `--resume` that found nothing, because the fork fallback hangs on it.
const CARRY_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Move a session out of main: its branch gets a worktree, main goes back to base.
///
/// The gesture the swap could not offer, because a swap needs a second branch to
/// exchange and this has none: you started something in main, it turned into real
/// work, and now it wants a tree of its own so main is free again.
///
/// One session, not the whole tree. The branch travels because the conversation is
/// about it (`git::move_branch_out` carries the uncommitted work with it), and main
/// is left on base rather than detached.
///
/// The refusals are the swap's, for the same reasons: an agent mid-turn in main
/// would have the tree replaced under it, and a stopped rebase cannot switch at
/// all. `move_branch_out` re-checks the rebase itself, since it is the half a test
/// can drive.
pub async fn move_out_of_main(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<serde_json::Value> {
    // Main's checkout moves here, so this holds the swap's lock for the swap's
    // reason: two moves of main at once each read "who was on the branch that just
    // left" and each relocate on the strength of it. Nothing in the SPA stops the
    // two buttons being pressed together. Refused rather than queued, like the swap.
    let _swap = app.swapping.try_lock().map_err(|_| {
        ApiError(anyhow::anyhow!(
            "a swap is already running; it moves main's checkout, so give it a moment \
             and look at the rail before asking again"
        ))
    })?;
    let (busy, live) = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .ok_or_else(|| crate::state::no_such_session(id))?;
        if s.workspace != MAIN {
            refuse!(
                "{} is not in main, so there is nothing to move it out of",
                crate::model::short_id(&id)
            );
        }
        (s.state.is_busy(), s.state.is_live())
    };
    if busy {
        refuse!(
            "that agent is mid-turn; the move replaces every file under it, so let \
             the turn finish first"
        );
    }

    let main = app.cfg.main_checkout.clone();
    let base_ref = app.cfg.upstream_ref.clone();
    // Named for the branch, which is what the worktree is *for* — and uniquified
    // rather than refused, since a tree left behind by earlier work on the same
    // branch is a reason to pick another name, not to stop.
    let branch = tokio::task::spawn_blocking({
        let main = main.clone();
        move || crate::git::current_branch(&main)
    })
    .await
    .map_err(|e| anyhow::anyhow!("reading main's branch failed: {e}"))??;
    // Main sitting on base has no branch to hand over, so the tree is named for the
    // work rather than for a branch, and `move_branch_out` cuts it one.
    let base_now = tokio::task::spawn_blocking({
        let (main, base_ref) = (main.clone(), base_ref.clone());
        move || crate::git::base_checkout_branch(&main, &base_ref)
    })
    .await
    .map_err(|e| anyhow::anyhow!("reading the base branch failed: {e}"))?;
    let stem = if base_now.as_deref() == Some(branch.as_str()) {
        "work".to_string()
    } else {
        branch_leaf(&branch)
    };
    let name = free_worktree_name(&app, &stem);
    spawn::validate_worktree_name(&name)?;
    let path = app.cfg.worktree_path(&name);
    // The naming Claude Code's own worktrees use, so a branch cut here reads like
    // every other worktree branch in the repo rather than like a special case.
    let new_branch = format!("worktree-{name}");

    let (moved, untracked) = tokio::task::spawn_blocking({
        let (main, path, new_branch) = (main.clone(), path.clone(), new_branch.clone());
        let exclude = app.cfg.worktrees_subdir_str();
        move || -> anyhow::Result<(crate::git::MovedOut, Vec<String>)> {
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
            /* **Carried out, not only logged**, the way the swap beside it already
            does. This was computed and dropped, so the only notice that untracked
            files do not travel was the sentence in a confirm box — and when that
            box went, the product stopped saying it at all. The files are still in
            main, on base, where they are indistinguishable from base's own. */
            Ok((moved, left))
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("the move task panicked: {e}"))??;

    // Cut by the daemon, so the repo's WorktreeCreate never fired for it (§ the
    // worktree_setup rule) — the same reason `ensure_pr_worktree` runs these.
    crate::worktree::run_worktree_hooks(&app, &path, crate::model::Board::Loud).await;
    app.register_worktree(&name, path.clone(), Some(moved.branch.clone()))
        .await;
    // Main gave the branch away, and `reconcile` only adds: left in, main would go
    // on claiming a branch that lives in the new tree.
    app.forget_branch(MAIN, &moved.branch).await;
    let moved_branch = moved.branch.clone();

    // Read *before* the reconciles, for the swap's reason: `reconcile` re-stamps a
    // live session's branch from what its tree has checked out now, and the branch
    // has already left — asking afterwards would find nobody who was on it.
    let (_, records) = to_carry(&app, MAIN, &moved.branch).await;

    let _ = app.reconcile(MAIN).await;
    let _ = app.reconcile(&name).await;

    // The conversation follows its branch: live means a pty to move, and a record
    // that is not running has nothing to respawn, so the record itself travels.
    let carried = if live {
        Some(spawn::relocate_session(&app, id, &name, CARRY_GRACE).await)
    } else {
        carry_record(&app, id, &name, &path, &moved.branch).await;
        None
    };

    // Told the same thing the swap tells a carried conversation: the cwd moved under
    // it, and a session Claude Code has isolated somewhere else has to re-anchor.
    if let Some(Ok(moved)) = &carried {
        let notice = arrival_notice(&app, &moved_branch, &main, &path, false);
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&moved.id) {
            s.arrival_notice = Some(notice);
        }
    }

    // Its siblings — the past conversations in main about the branch that just
    // left. Leaving them behind points a later resume at main's directory while
    // their work sits in the new tree, which is exactly the pairing the branch
    // field exists to keep.
    for other in records.into_iter().filter(|r| *r != id) {
        carry_record(&app, other, &name, &path, &moved.branch).await;
    }
    app.notify().await;

    check_moves_landed(&app, "the move out of main").await;
    Ok(Json(json!({
        "workspace": name,
        "branch": moved.branch,
        "created": moved.created,
        "main": moved.base,
        "session": carried_json(&carried),
        "wip_error": moved.wip_error,
        // Named, not counted, for the reason the swap gives: which files stayed is
        // the difference between fetching them and wondering what you lost.
        "untracked_left": untracked,
    })))
}

/// A directory-safe stem from a branch name.
///
/// `feature/some-thing` is `some-thing`: the leaf is what tells two of your
/// branches apart, and the prefix is the same on all of them.
fn branch_leaf(branch: &str) -> String {
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
fn free_worktree_name(app: &Arc<AppState>, stem: &str) -> String {
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

/// One direction of the carry, as the SPA reads it.
///
/// `null` is its own answer and not an error: it means there was nothing in that
/// tree to move, which is the ordinary case for a swap into an empty main.
fn carried_json(r: &Option<anyhow::Result<spawn::Relocated>>) -> serde_json::Value {
    match r {
        None => serde_json::Value::Null,
        Some(Ok(moved)) => json!({
            "session": moved.id.to_string(),
            // A fork, not the move that was promised — the id changed, so the rail
            // is about to show a second row and it is worth saying why.
            "degraded": moved.degraded,
            "error": serde_json::Value::Null,
        }),
        Some(Err(e)) => json!({
            "session": serde_json::Value::Null,
            "degraded": false,
            "error": format!("{e:#}"),
        }),
    }
}

/// Everything in a workspace that belongs to `branch`, split by what moving it costs.
///
/// `.0` is the one live conversation, which has to be *relocated*: killed, re-filed
/// and resumed in the destination. `.1` is every other session recorded there on the
/// same branch, which is a field update and a file move with no process in it.
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
/// A session started as a pass is left alone deliberately: a fix or resolve run
/// belongs to its PR's worktree, and moving one into main would put an agent that
/// rebases and force-pushes on the tree every worktree is cut from.
async fn to_carry(
    app: &Arc<AppState>,
    workspace: &str,
    branch: &str,
) -> (Option<SessionId>, Vec<SessionId>) {
    // Read out under one guard, decided outside it: `has_conversation` below is a
    // file read, and the lock is fair, so a writer queued behind it would block
    // every reader queued behind that in turn.
    let (mut live, records) = {
        let inner = app.inner.read().await;
        let mine = inner
            .sessions
            .values()
            .filter(|s| s.workspace == workspace)
            .filter(|s| s.pass.is_none())
            .filter(|s| s.branch.as_deref() == Some(branch));
        let mut live = Vec::new();
        let mut records = Vec::new();
        for s in mine {
            if s.state.is_live() {
                live.push((s.created_at, s.id, s.cwd.clone(), s.transcript_path.clone()));
            } else if s.recovery.is_none() {
                // A recovery record describes a worktree that was torn down, so the
                // session is not *in* either tree here and which branch sits where
                // has nothing to do with it. `worktree::branch_drift` already says
                // its piece when one of those is resumed.
                records.push(s.id);
            }
        }
        live.sort_by_key(|(created, ..)| std::cmp::Reverse(*created));
        (live, records)
    };
    // A file does not prove a conversation — a session that started but never spoke
    // owns a file of headers — so this asks `has_conversation`. Newest-first,
    // stopping at the first hit, so the usual cost is one read rather than one per
    // session to then discard all but the newest.
    // Off the runtime, because each of those reads opens a file, and a swap runs
    // this three times.
    let carried = crate::proc::run_blocking("looking for the conversation to carry", move || {
        live.drain(..)
            .find(|(_, id, cwd, recorded)| {
                crate::store::has_conversation(*id, cwd, recorded.as_deref())
            })
            .map(|(_, id, ..)| id)
    })
    .await
    .unwrap_or(None);
    (carried, records)
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
fn arrival_notice(
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
    if let Some(extra) = app.cfg.workspace_notes.for_main(into_main) {
        note.push(' ');
        note.push_str(extra);
    }
    note
}

/// Prove a move left every conversation in the tree that holds its branch.
///
/// A post-condition, not a guard: the move has already happened, and the point is
/// that a mismatch is *said* rather than discovered days later by an agent that
/// cannot find its own work. This has stranded a conversation more than once — the
/// record in one checkout, its uncommitted edits in another — and each time the only
/// symptom was the agent eventually noticing.
///
/// Live sessions with no pass only. An archived one is history and is allowed to
/// name a branch that has since moved; a pass is pinned to its PR's branch by
/// construction.
async fn check_moves_landed(app: &Arc<AppState>, what: &str) {
    let inner = app.inner.read().await;
    for s in inner.sessions.values() {
        if !s.state.is_live() || s.pass.is_some() {
            continue;
        }
        let Some(mine) = s.branch.as_deref() else {
            continue;
        };
        let Some(w) = inner.workspaces.get(&s.workspace) else {
            continue;
        };
        // Unknown before the first reconcile, which is not a mismatch.
        let Some(holds) = w.tree.branch.as_deref() else {
            continue;
        };
        if holds != mine {
            tracing::warn!(
                session = %s.id,
                "after {what}: this conversation is recorded on {mine} but {} holds \
                 {holds}, so its work is not where the conversation is. Nothing is \
                 lost — the branch and its uncommitted changes are wherever {mine} \
                 went — but the two have to be brought back together by hand.",
                s.workspace,
            );
        }
    }
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
async fn carry_record(
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A conversation travels with its branch, and the *record's* branch is what
    /// decides that.
    ///
    /// **The reason every spawner has to record one.** `spawn_worktree_session`
    /// did not, so a worktree session's record said `branch: None` until a
    /// reconcile of its workspace happened to run — and a swap pressed before that
    /// left the conversation behind while its branch moved into main, with no
    /// error anywhere. It surfaced as the swap e2e flows failing about one run in
    /// three, which reads as a slow resume and is not.
    #[tokio::test]
    async fn a_session_with_no_recorded_branch_is_not_carried() {
        use crate::model::{Session, State as S};

        let (app, dir) = crate::testutil::app("carry");
        /* Archived rather than live, and that is what makes this test able to
        fail: a live session only becomes a carry once `has_conversation` finds
        a turn on disk, and neither of these has a file — so both would answer
        `None` whatever the filter did. The *records* half asks the same
        question about the branch and nothing else. */
        let with = |branch: Option<&str>| {
            let id = Uuid::new_v4();
            let mut s = Session::new(id, "invoice".to_string(), dir.clone(), None);
            s.branch = branch.map(str::to_string);
            s.had_a_turn = true;
            s.set_state(S::Archived { resumable: true });
            (id, s)
        };
        let (unknown, blank) = with(None);
        let (known, stamped) = with(Some("worktree-invoice"));
        {
            let mut inner = app.inner.write().await;
            inner.sessions.insert(unknown, blank);
            inner.sessions.insert(known, stamped);
        }

        let (_, records) = to_carry(&app, "invoice", "worktree-invoice").await;
        assert!(
            records.contains(&known),
            "the session on that branch was not carried"
        );
        assert!(
            !records.contains(&unknown),
            "a session whose record names no branch was carried anyway"
        );
    }

    /// The bug this is here for. Two swaps moved three branches between three trees
    /// and carried no conversation at all, because every session involved happened
    /// to be archived at the time, so days later a pane's transcript was about one
    /// story and its changed files about another.
    ///
    /// `is_live` was the whole filter, and "not running" is not the rare case: you
    /// swap between pieces of work, and the one you are swapping away from is
    /// usually the one you just stopped.
    #[tokio::test]
    async fn a_swap_carries_the_conversation_that_was_not_running() {
        use crate::model::{ArchiveState, Session, State};

        let (app, dir) = crate::testutil::app("carry-archived");
        let put = |ws: &str, branch: Option<&str>, state: State, recovery: Option<ArchiveState>| {
            let mut s = Session::new(Uuid::new_v4(), ws.to_string(), dir.clone(), None);
            s.branch = branch.map(str::to_string);
            s.had_a_turn = true;
            s.recovery = recovery;
            s.state = state;
            s
        };
        let archived = || State::Archived { resumable: true };

        let (mine, others, unknown, torn, live_elsewhere) = {
            let mut inner = app.inner.write().await;
            let mine = put("wt", Some("feature/a"), archived(), None);
            // Same tree, different branch: a worktree outlives the branches that
            // pass through it, so "in this directory" was never the question.
            let others = put("wt", Some("feature/b"), archived(), None);
            // Written before the branch was recorded. An unknown branch answers
            // nothing, so it travels nowhere.
            let unknown = put("wt", None, archived(), None);
            // Its worktree was torn down, so it is not *in* either tree here and
            // which branch sits where has nothing to do with it.
            let torn = put(
                "wt",
                Some("feature/a"),
                archived(),
                Some(ArchiveState::Recoverable {
                    name: "gone".into(),
                    branch: "feature/a".into(),
                    head_sha: "abc".into(),
                }),
            );
            let live_elsewhere = put("other", Some("feature/a"), archived(), None);
            let ids = (mine.id, others.id, unknown.id, torn.id, live_elsewhere.id);
            for s in [mine, others, unknown, torn, live_elsewhere] {
                inner.sessions.insert(s.id, s);
            }
            ids
        };

        let (live, records) = to_carry(&app, "wt", "feature/a").await;
        assert_eq!(live, None, "nothing was running, so nothing is relocated");
        assert_eq!(
            records,
            vec![mine],
            "only the conversation whose branch left"
        );
        for stranded in [others, unknown, torn, live_elsewhere] {
            assert!(!records.contains(&stranded));
        }
    }

    /// Selection is by branch, not by recency, which is the whole point of
    /// recording it. The old picker took the newest session in the directory, so a
    /// swap could carry a conversation about work that was not moving and leave the
    /// one that was.
    #[tokio::test]
    async fn the_newest_conversation_is_not_the_one_that_travels() {
        use crate::model::{Session, State};

        let (app, dir) = crate::testutil::app("carry-newest");
        let (wanted, newer) = {
            let mut inner = app.inner.write().await;
            let mut wanted = Session::new(Uuid::new_v4(), "wt".into(), dir.clone(), None);
            wanted.branch = Some("feature/a".into());
            wanted.had_a_turn = true;
            wanted.state = State::Archived { resumable: true };

            let mut newer = Session::new(Uuid::new_v4(), "wt".into(), dir.clone(), None);
            newer.branch = Some("feature/b".into());
            newer.had_a_turn = true;
            newer.state = State::Archived { resumable: true };
            newer.created_at = wanted.created_at + std::time::Duration::from_secs(60);

            let ids = (wanted.id, newer.id);
            inner.sessions.insert(wanted.id, wanted);
            inner.sessions.insert(newer.id, newer);
            ids
        };

        let (_, records) = to_carry(&app, "wt", "feature/a").await;
        assert_eq!(records, vec![wanted]);
        assert!(!records.contains(&newer), "recency is not the question");
    }

    /// Moving the record is only half of it: the conversation comes back believing
    /// it is in the tree it was reading all along, so it is told once, at the next
    /// prompt, and the project's own note about the destination rides along.
    #[tokio::test]
    async fn a_carried_conversation_is_told_where_it_now_is() {
        use crate::model::{Session, MAIN};

        // The note is the half only the project knows. orchd supplies the facts
        // about the move; this sentence is the repo's business and comes from its
        // own config.
        let (app, dir) = crate::testutil::app_with(
            "carry-notice",
            r#""workspace_notes":{"main":"the dev stack only runs here"}"#,
        );

        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, "wt".to_string(), dir.join("wt"), None);
            // Recorded on the branch it is *leaving*, which is the state a second
            // move finds a session in: the first carry moved it and nothing had
            // re-stamped the record yet. `carry_record` has to correct that itself,
            // because `to_carry` selects on this field.
            s.branch = Some("feature/stale".into());
            s.had_a_turn = true;
            s.state = crate::model::State::Archived { resumable: true };
            inner.sessions.insert(id, s);
        }

        carry_record(&app, id, MAIN, &dir, "feature/a").await;

        let inner = app.inner.read().await;
        let s = &inner.sessions[&id];
        assert_eq!(s.workspace, MAIN, "the record follows its branch");
        assert_eq!(s.cwd, dir);
        assert_eq!(
            s.branch.as_deref(),
            Some("feature/a"),
            "the record names the branch it was carried for, not the one it left"
        );
        let notice = s.arrival_notice.as_deref().expect("it has to be told");
        assert!(notice.contains("feature/a"), "which branch moved: {notice}");
        assert!(
            notice.contains(&dir.display().to_string()),
            "where it is now: {notice}"
        );
        assert!(
            notice.contains(&dir.join("wt").display().to_string()),
            "where it was reading before: {notice}"
        );
        assert!(
            notice.contains("the dev stack only runs here"),
            "and what the project says main is for: {notice}"
        );
    }
}
