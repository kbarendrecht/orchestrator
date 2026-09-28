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

use crate::api::{refuse, refuse_busy, ApiError, ApiResult};
use crate::carry::{arrival_notice, carry_record, to_carry};
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
    body: Option<Json<SwapBody>>,
) -> ApiResult<serde_json::Value> {
    let session = body.and_then(|Json(b)| b.session);
    tracing::info!(%workspace, session = ?session, "swap requested");
    let out = swap_with_main_inner(app, workspace.clone(), session).await;
    if let Err(e) = &out {
        // Warn, not error: most of these are the daemon correctly declining, and a
        // refusal you can read is the point rather than a fault to page about.
        tracing::warn!(%workspace, "swap refused: {:#}", e.0);
    }
    out
}

/// The row the swap was pressed from.
///
/// Optional, so a bare `POST` still works: with no session the one live session on
/// the branch goes, and two of them are refused rather than picked between.
#[derive(Default, serde::Deserialize)]
pub struct SwapBody {
    pub session: Option<Uuid>,
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
/// The branches trade places, and so do the conversations about them: the session
/// you pressed swap on is relocated into main, and every live session in main is
/// relocated out to the worktree. Symmetry is the point — a swap that moved one
/// side only left main's session reading a tree that had changed under it.
///
/// Every live session in main goes, empty ones included, because main holds one
/// session and the one left behind would refuse the arrival — after git had
/// already moved, with the arriving session killed on its way in.
///
/// Relocating keeps the session id, so each conversation continues as one rail row
/// rather than gaining a forked sibling; `spawn::relocate_session` has the how, and
/// falls back to a fork only if a resume will not stay up.
///
/// # The refusals
///
/// A stopped rebase, a session **mid-turn** in either, or a second live session on
/// the branch that would have to stay behind. Uncommitted work is not a refusal: it
/// travels with its branch, see `git::swap_branches`.
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
    pressed: Option<Uuid>,
) -> ApiResult<serde_json::Value> {
    if workspace == MAIN {
        refuse!("main cannot be swapped with itself");
    }
    /* **A pooled spare is not a swap target.** It holds no session and no work, so
    there is nothing on its side to exchange: the swap would put main's branch into
    an empty tree and hand main the spare's own `worktree-<name>`, which is not a
    move anybody would ask for. It is refused here rather than allowed and then
    un-pooled, because the pool is not the reason — a workspace with nothing in it
    is.
    Reachable only by calling this route by hand; the rail offers a swap from a
    session's row, and a spare has no row. Refused anyway, because until this the
    swap and the pool did not know about each other, and `Snapshot.spare` would have
    gone on naming a workspace `Snapshot.workspaces` showed with a live occupant
    until the next poll noticed. */
    if app.inner.read().await.spare.ids.contains(&workspace) {
        refuse!("{workspace} is a spare worktree with nothing in it — there is nothing to swap");
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
    let (_, mut outgoing_records) = to_carry(&app, MAIN, &main_was).await;
    let (on_branch, incoming_records) = to_carry(&app, &workspace, &tree_was).await;
    let outgoing = live_in(&app, MAIN).await;
    let several = app.settings().allow_several_in_main;
    let incoming = match pick_incoming(&app, &workspace, &on_branch, pressed, several).await {
        Ok(ids) => ids,
        Err(why) => refuse!("{why}"),
    };
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

    /* **All or nothing, and the undo is the transaction.** Git and a starting
    `claude` cannot share a lock, so a session that will not start at the far end is
    found out after the branches have moved. Every move that landed is written down
    as it lands, and a failure walks the list back: the branches swap back, each
    arrived session goes home, and the one that failed is resumed where it was.
    A fork that stays up is not a failure — the conversation did arrive, under a
    new id — so only a session that could not be started at all undoes the swap. */
    let mut journal = Vec::new();
    let mut into_worktree = Vec::new();
    let mut into_main = Vec::new();
    let mut failure = None;
    let mut records_out = Vec::new();
    'moves: for (phase, (ids, dest, from, landed)) in [
        (outgoing, workspace.as_str(), MAIN, &mut into_worktree),
        (incoming, MAIN, workspace.as_str(), &mut into_main),
    ]
    .into_iter()
    .enumerate()
    {
        /* Main's stopped sessions leave before anything arrives, not after. A session
        auto-resume has not reached yet is one of them, and it holds main
        (`AppState::waiting_in_main`), so while it was still there the arrival was
        refused and every swap in the first seconds after a launch undid itself. */
        if phase == 1 {
            for id in &outgoing_records {
                carry_record(&app, *id, &workspace, &tree, &swapped.worktree_now).await;
            }
            records_out = std::mem::take(&mut outgoing_records);
        }
        for id in ids {
            let was = state_of(&app, id).await;
            match spawn::relocate_session(&app, id, dest, CARRY_GRACE).await {
                Ok(moved) => {
                    journal.push(Landed {
                        now: moved.id,
                        original: id,
                        from: from.to_string(),
                        was,
                    });
                    landed.push(moved);
                }
                Err(e) => {
                    failure = Some((
                        Landed {
                            now: id,
                            original: id,
                            from: from.to_string(),
                            was,
                        },
                        e,
                    ));
                    break 'moves;
                }
            }
        }
    }
    if let Some((failed, e)) = failure {
        let why = format!("{e:#}");
        let restored: Vec<_> = journal.iter().map(|l| l.original).collect();
        let undo = Undo {
            moves: journal,
            records_out,
        };
        match undo_swap(&app, &main, &tree, &workspace, &swapped, undo, failed).await {
            Ok(()) => {
                tracing::warn!(%workspace, %why, ?restored, "swap undone");
                refuse_busy!("swap failed, nothing moved: {why}");
            }
            Err(left) => {
                tracing::error!(%workspace, %why, "swap failed and the undo did not finish: {left:#}");
                refuse!("swap failed and could not be fully undone: {why}. {left:#}");
            }
        }
    }

    // The conversations that were not running. No process work, so these cannot
    // fail the swap and are not reported back as a carry: the rail simply shows
    // them where their branch went.
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
        for moved in moved.iter() {
            let notice = arrival_notice(&app, branch, from, to, into_main);
            let mut inner = app.inner.write().await;
            if let Some(s) = inner.sessions.get_mut(&moved.id) {
                s.arrival_notice = Some(notice);
            }
        }
    }
    app.notify().await;

    // Where to land the pane: main is what you pressed this for, so the session that
    // arrived there wins, and the one that left main is the fallback.
    let select = into_main
        .iter()
        .chain(&into_worktree)
        .next()
        .map(|r| r.id.to_string());
    let landed = |v: &[spawn::Relocated]| -> Vec<SessionId> { v.iter().map(|r| r.id).collect() };

    tracing::info!(
        %workspace,
        main_now = %swapped.main_now,
        worktree_now = %swapped.worktree_now,
        wip_error = ?swapped.wip_error,
        // Which conversations travelled, because "it did not move" can mean the
        // branches or the sessions, and the two have different causes.
        into_main = ?landed(&into_main),
        into_worktree = ?landed(&into_worktree),
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
        // The first of each, for callers that read one; the whole lists beside them,
        // since main now sends every live session out and several may come in.
        "into_main": into_main.first().map_or(serde_json::Value::Null, moved_json),
        "into_worktree": into_worktree.first().map_or(serde_json::Value::Null, moved_json),
        "moved_in": into_main.iter().map(moved_json).collect::<Vec<_>>(),
        "moved_out": into_worktree.iter().map(moved_json).collect::<Vec<_>>(),
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
    let was = state_of(&app, id).await;
    let out = crate::carry::move_out(&app, crate::model::Board::Loud, None).await?;
    let crate::carry::MovedOut {
        name,
        path,
        git: moved,
        untracked,
        records,
    } = &out;

    // The conversation follows its branch: live means a pty to move, and a record
    // that is not running has nothing to respawn, so the record itself travels.
    //
    // All or nothing, like the swap: a session that will not start in the new tree
    // takes the move back, and is resumed in main as it stood.
    let carried = if live {
        match spawn::relocate_session(&app, id, name, CARRY_GRACE).await {
            Ok(moved) => Some(Ok(moved)),
            Err(e) => {
                let why = format!("{e:#}");
                let back = crate::carry::move_back(&app, &out).await;
                /* Home is wherever the branch is now: main when the move came back,
                the new tree when it could not. And an empty session whose fresh
                start failed has no record left — the exit watcher drops a session
                that never had a turn — so there is nothing to bring home, which
                is not a failure of the undo. The swap's undo makes the same check. */
                let to = if back.is_ok() { MAIN } else { name.as_str() };
                let exists = app.inner.read().await.sessions.contains_key(&id);
                let home = if exists {
                    spawn::relocate_back(&app, id, to, CARRY_GRACE, &was)
                        .await
                        .map(|_| ())
                } else {
                    Ok(())
                };
                match (back, home) {
                    (Ok(()), Ok(_)) => {
                        tracing::warn!(session = %id, %why, "move out of main undone");
                        refuse_busy!("move failed, nothing moved: {why}");
                    }
                    (back, home) => {
                        let left: Vec<String> = [
                            back.err()
                                .map(|e| format!("the branch stays in {name}: {e:#}")),
                            home.err().map(|e| {
                                format!("the session is stopped and resumable from the rail: {e:#}")
                            }),
                        ]
                        .into_iter()
                        .flatten()
                        .collect();
                        refuse!(
                            "move failed and could not be fully undone: {why}. {}",
                            left.join("; ")
                        );
                    }
                }
            }
        }
    } else {
        carry_record(&app, id, name, path, &moved.branch).await;
        None
    };

    // Told the same thing the swap tells a carried conversation: the cwd moved under
    // it, and a session Claude Code has isolated somewhere else has to re-anchor.
    if let Some(Ok(landed)) = &carried {
        let notice = arrival_notice(&app, &moved.branch, &main, path, false);
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&landed.id) {
            s.arrival_notice = Some(notice);
        }
    }

    // Its siblings — the past conversations in main about the branch that just
    // left. Leaving them behind points a later resume at main's directory while
    // their work sits in the new tree, which is exactly the pairing the branch
    // field exists to keep.
    for other in records.iter().filter(|r| **r != id) {
        carry_record(&app, *other, name, path, &moved.branch).await;
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

/// One direction of the carry, as the SPA reads it.
///
/// `null` is its own answer and not an error: it means there was nothing in that
/// tree to move, which is the ordinary case for a swap into an empty main.
fn carried_json(r: &Option<anyhow::Result<spawn::Relocated>>) -> serde_json::Value {
    match r {
        None => serde_json::Value::Null,
        Some(Ok(moved)) => moved_json(moved),
        Some(Err(e)) => json!({
            "session": serde_json::Value::Null,
            "degraded": false,
            "error": format!("{e:#}"),
        }),
    }
}

/// One move that landed, as the SPA reads it.
fn moved_json(moved: &spawn::Relocated) -> serde_json::Value {
    json!({
        "session": moved.id.to_string(),
        // A fork, not the move that was promised — the id changed, so the rail is
        // about to show a second row and it is worth saying why.
        "degraded": moved.degraded,
        "error": serde_json::Value::Null,
    })
}

/// What a swap did before it failed, in the order it did it.
struct Undo {
    /// Each live session that landed, oldest move first.
    moves: Vec<Landed>,
    /// Main's stopped sessions, carried out before anything arrived.
    records_out: Vec<SessionId>,
}

/// A session the swap moved, and how to send it back.
struct Landed {
    /// Its id now: the same one after a resume, a new one after a fork or a fresh
    /// start.
    now: SessionId,
    /// Its id before the move, which is what a fork left behind as a stopped record.
    original: SessionId,
    /// The workspace it came from.
    from: String,
    /// Its state before the move, so it comes back at the turn it was sitting on.
    was: crate::model::State,
}

async fn state_of(app: &Arc<AppState>, id: SessionId) -> crate::model::State {
    let inner = app.inner.read().await;
    inner
        .sessions
        .get(&id)
        .map(|s| s.state.clone())
        .unwrap_or(crate::model::State::Starting)
}

/// Put a swap back: the branches, then every session that moved, then the one that
/// failed to.
///
/// Branches first, so each session is resumed into a tree that holds its own branch
/// again. The arrivals go back newest-first, which empties main before main's own
/// sessions return to it. The failed one last: its record is wherever the failure
/// left it — still at home, or at the far end after a start that died — and
/// `relocate_back` moves it home from either.
///
/// Every step is tried even when one before it fails, and the failures are named
/// together: a half-finished undo that stops at the first error hides the rest.
async fn undo_swap(
    app: &Arc<AppState>,
    main: &std::path::Path,
    tree: &std::path::Path,
    workspace: &str,
    swapped: &crate::git::Swap,
    undo: Undo,
    failed: Landed,
) -> anyhow::Result<()> {
    let Undo {
        moves: journal,
        records_out,
    } = undo;
    let mut left = Vec::new();
    let (m, t) = (main.to_path_buf(), tree.to_path_buf());
    match tokio::task::spawn_blocking(move || crate::git::swap_branches(&m, &t)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => left.push(format!(
            "the branches stay swapped ({} in main, {} in {workspace}): {e:#}",
            swapped.main_now, swapped.worktree_now
        )),
        Err(e) => left.push(format!("swapping the branches back panicked: {e}")),
    }
    app.forget_branch(workspace, &swapped.main_now).await;
    app.forget_branch(MAIN, &swapped.worktree_now).await;
    let _ = app.reconcile(MAIN).await;
    let _ = app.reconcile(workspace).await;

    // Main's stopped sessions, carried out before the arrivals, come home first so
    // main is as it was before any live session returns to it.
    for id in records_out {
        carry_record(app, id, MAIN, main, &swapped.worktree_now).await;
    }
    for moved in journal.into_iter().rev() {
        if let Err(e) =
            spawn::relocate_back(app, moved.now, &moved.from, CARRY_GRACE, &moved.was).await
        {
            left.push(format!(
                "{} is stopped outside {}: {e:#}",
                crate::model::short_id(&moved.now),
                moved.from
            ));
        }
        // A fork leaves the original behind at the far end as a stopped record, and it
        // is about the same branch, so it goes home too.
        if moved.now != moved.original {
            // Its branch is the one its home tree holds again after the swap back.
            let branch = if moved.from == MAIN {
                &swapped.worktree_now
            } else {
                &swapped.main_now
            };
            if let Some(path) = app.workspace_path(&moved.from).await {
                carry_record(app, moved.original, &moved.from, &path, branch).await;
            }
        }
    }
    // An empty session that failed to start fresh has no record left: its old one
    // had no turn, so the exit watcher dropped it, and there is nothing to bring back.
    let exists = app.inner.read().await.sessions.contains_key(&failed.now);
    if exists {
        // A fork tried as the fallback and died too sits at the far end as a stopped
        // record about the same branch, so it goes home with the one it forked from.
        let forks: Vec<_> = {
            let inner = app.inner.read().await;
            inner
                .sessions
                .values()
                .filter(|s| s.forked_from == Some(failed.original) && s.workspace != failed.from)
                .map(|s| s.id)
                .collect()
        };
        let branch = if failed.from == MAIN {
            &swapped.worktree_now
        } else {
            &swapped.main_now
        };
        if let Some(path) = app.workspace_path(&failed.from).await {
            for fork in forks {
                carry_record(app, fork, &failed.from, &path, branch).await;
            }
        }
    }
    if exists {
        if let Err(e) =
            spawn::relocate_back(app, failed.now, &failed.from, CARRY_GRACE, &failed.was).await
        {
            left.push(format!(
                "{} could not be resumed in {}; it is stopped and resumable from the rail: {e:#}",
                crate::model::short_id(&failed.now),
                failed.from
            ));
        }
    }
    app.notify().await;
    if left.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", left.join("; "))
    }
}

/// Every live session in a workspace, oldest first.
///
/// Main's side of a swap, where the branch does not matter: whatever is running in
/// main has the tree replaced under it, so all of it goes.
async fn live_in(app: &Arc<AppState>, workspace: &str) -> Vec<SessionId> {
    let inner = app.inner.read().await;
    let mut live: Vec<_> = inner
        .sessions
        .values()
        .filter(|s| s.workspace == workspace && s.state.is_live())
        .map(|s| (s.created_at, s.id))
        .collect();
    live.sort_by_key(|(created, _)| *created);
    live.into_iter().map(|(_, id)| id).collect()
}

/// Which of the worktree's live sessions go into main.
///
/// The one you pressed swap on, and nobody else: main holds one session, so a
/// second live session on the branch would stay in a tree whose branch just left.
/// That is refused and named rather than picked for you. With
/// `allow_several_in_main` main takes them all. With no pressed session, the one
/// live session on the branch goes, and two are refused the same way.
async fn pick_incoming(
    app: &Arc<AppState>,
    workspace: &str,
    on_branch: &[SessionId],
    pressed: Option<Uuid>,
    several: bool,
) -> Result<Vec<SessionId>, String> {
    let inner = app.inner.read().await;
    // The label to recognise it by, and the id to find it by whatever it is called.
    let name = |id: &SessionId| {
        let short = crate::model::short_id(id);
        match inner.sessions.get(id).and_then(|s| s.label()) {
            Some(label) => format!("{label}, {short}"),
            None => short,
        }
    };
    if let Some(id) = pressed {
        match inner.sessions.get(&id) {
            Some(s) if s.workspace == workspace => {}
            _ => {
                return Err(format!(
                    "{} is not a session in {workspace}",
                    crate::model::short_id(&id)
                ))
            }
        }
    }
    if several {
        return Ok(on_branch.to_vec());
    }
    let others: Vec<_> = on_branch
        .iter()
        .filter(|id| Some(**id) != pressed)
        .collect();
    if let Some(other) = others.first() {
        if pressed.is_some() || others.len() > 1 {
            return Err(format!(
                "another live session is on this branch in {workspace} ({}); main takes one \
                 session, so close it or swap from its row",
                name(other)
            ));
        }
    }
    Ok(match pressed {
        Some(id) if on_branch.contains(&id) => vec![id],
        Some(_) => Vec::new(),
        None => on_branch.to_vec(),
    })
}

/// Prove a move left every conversation in the tree that holds its branch.
///
/// A post-condition, not a guard: the move has already happened, and the point is
/// that a mismatch is *said* rather than discovered days later by an agent that
/// cannot find its own work. This has stranded a conversation more than once — the
/// record in one checkout, its uncommitted edits in another — and each time the only
/// symptom was the agent eventually noticing.
///
/// Live sessions only. An archived one is history and is allowed to name a branch
/// that has since moved. Passes are checked too: they travel with their branch now,
/// so a pass left behind is exactly the mismatch this is here to say out loud.
async fn check_moves_landed(app: &Arc<AppState>, what: &str) {
    let inner = app.inner.read().await;
    for s in inner.sessions.values() {
        if !s.state.is_live() {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A spare is a workspace with no session, so a swap has nothing to exchange
    /// with it — and before this refusal the two features did not know about each
    /// other at all.
    #[tokio::test]
    async fn a_swap_refuses_a_pooled_spare() {
        let (app, _dir) = crate::testutil::app("swap-spare");
        {
            let mut inner = app.inner.write().await;
            inner.with_spare("test", |s| {
                s.ids.push("idle-tree".into());
                true
            });
        }
        let err = swap_with_main_inner(app.clone(), "idle-tree".into(), None)
            .await
            .expect_err("a spare is not a swap target");
        assert!(
            format!("{:#}", err.0).contains("nothing to swap"),
            "the refusal says why: {:#}",
            err.0
        );

        // And an ordinary workspace is refused for its own reasons, not this one —
        // the guard must not swallow every unknown name.
        let err = swap_with_main_inner(app, "not-pooled".into(), None)
            .await
            .expect_err("an unknown workspace is still refused");
        assert!(
            !format!("{:#}", err.0).contains("nothing to swap"),
            "but not as a spare: {:#}",
            err.0
        );
    }

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
        assert!(
            live.is_empty(),
            "nothing was running, so nothing is relocated"
        );
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

    /// Who goes into main, one row per case. Main holds one session, so a second
    /// live session on the branch is refused and named rather than left behind in a
    /// tree whose branch just left.
    #[tokio::test]
    async fn the_pressed_row_goes_into_main_and_a_second_one_refuses() {
        use crate::model::{Session, State};

        let (app, dir) = crate::testutil::app("pick-incoming");
        let live = |ws: &str| {
            let mut s = Session::new(Uuid::new_v4(), ws.into(), dir.clone(), None);
            s.branch = Some("feature/a".into());
            s.state = State::Starting;
            s
        };
        let (a, b, elsewhere) = {
            let mut inner = app.inner.write().await;
            let (a, b, elsewhere) = (live("wt"), live("wt"), live("other"));
            let ids = (a.id, b.id, elsewhere.id);
            for s in [a, b, elsewhere] {
                inner.sessions.insert(s.id, s);
            }
            ids
        };

        let pick = |on: Vec<SessionId>, pressed, several| {
            let app = app.clone();
            async move { pick_incoming(&app, "wt", &on, pressed, several).await }
        };
        assert_eq!(pick(vec![a], Some(a), false).await, Ok(vec![a]));
        assert_eq!(pick(vec![a], None, false).await, Ok(vec![a]));
        assert_eq!(pick(vec![], None, false).await, Ok(vec![]));

        let refused = pick(vec![a, b], Some(a), false)
            .await
            .expect_err("b would stay");
        assert!(
            refused.contains(&crate::model::short_id(&b)),
            "names b: {refused}"
        );
        pick(vec![a, b], None, false)
            .await
            .expect_err("two with nobody pressed is not picked between");

        assert_eq!(pick(vec![a, b], Some(a), true).await, Ok(vec![a, b]));
        pick(vec![a], Some(elsewhere), false)
            .await
            .expect_err("a session from another tree is not this swap's");
    }

    /// A fix or resolve run is still the conversation about its branch, so it goes
    /// where the branch goes. It used to be filtered out, and swapping the PR it was
    /// fixing moved the branch into main and left the run behind.
    #[tokio::test]
    async fn a_pass_travels_with_its_branch() {
        use crate::model::{Pass, Session, State};

        let (app, dir) = crate::testutil::app("carry-pass");
        let id = {
            let mut inner = app.inner.write().await;
            let pass = Pass {
                pr: 4242,
                command: Pass::FIX_PR.to_string(),
            };
            let mut s = Session::new(Uuid::new_v4(), "wt".into(), dir.clone(), Some(pass));
            s.branch = Some("feature/a".into());
            s.had_a_turn = true;
            s.state = State::Archived { resumable: true };
            let id = s.id;
            inner.sessions.insert(id, s);
            id
        };

        let (_, records) = to_carry(&app, "wt", "feature/a").await;
        assert_eq!(records, vec![id]);
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
