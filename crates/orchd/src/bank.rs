//! The rebase button, and the bank it puts your work in.
//!
//! **Split out of `api` because none of it is HTTP.** Five handlers sat among the
//! session and PR routes, and four of them re-derived the same preamble by hand:
//! resolve the workspace path, require a bank, refuse a working session, run one
//! git call off the runtime, reconcile, notify. That is a workflow, not a route
//! table — and `api.rs` had become the place such a thing lands because nothing
//! decided where else it should go. `review_api` is the same move, made earlier.
//!
//! The rules are unchanged and so are the routes; only the address moved. What is
//! new is [`banked_at`], which the three `wip_*` verbs now share instead of each
//! spelling the same two refusals.
//!
//! **Why a bank exists at all.** A rebase cannot take uncommitted work with it, so
//! the button parks it on a ref of its own (`refs/orchd/wip/<workspace>`) and puts
//! it back afterwards. `docs/traps/daemon.md` carries what the alternative cost.

use axum::{
    extract::{Path, State},
    Json,
};
use serde_json::json;
use std::sync::Arc;

use crate::api::{refuse, refuse_busy, type_user_turn, ApiError, ApiResult};
use crate::state::AppState;

/// The workspace's path and its bank, or the refusal that says which is missing.
///
/// The two questions every `wip_*` verb asks first, and each used to ask them
/// itself. `rebase` does not use it: it is the verb that *creates* a bank, so
/// "nothing banked" is its normal case rather than its refusal.
async fn banked_at(
    app: &Arc<AppState>,
    workspace: &str,
) -> Result<(std::path::PathBuf, crate::model::Bank), ApiError> {
    let path = app
        .workspace_path(workspace)
        .await
        .ok_or_else(|| ApiError(anyhow::anyhow!("unknown workspace {workspace}")))?;
    let Some(bank) = app.workspace_banked(workspace).await else {
        return Err(ApiError(anyhow::anyhow!("{workspace} has nothing banked")));
    };
    Ok((path, bank))
}

// ---------------------------------------------------------------------------
// Rebase onto the upstream base
// ---------------------------------------------------------------------------

/// Take in `upstream/develop` by rebasing, never merging: history stays linear.
///
/// Refuses rather than half-doing it — a dirty tree or a working session would
/// both turn a one-click rebase into a mess someone has to unpick.
pub async fn rebase(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    let path = app
        .workspace_path(&workspace)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?;

    // `is_clean` is a full `git status` scan on a worktree without fsmonitor, so
    // both checks go off the runtime with the rest of this handler.
    let (mid_rebase, clean, conflicted) = {
        let p = path.clone();
        crate::proc::run_blocking("checking the tree before a rebase", move || {
            (
                crate::git::rebase_in_progress(&p),
                crate::git::is_clean(&p).unwrap_or(false),
                crate::git::unmerged(&p).unwrap_or_default(),
            )
        })
        .await?
    };
    if mid_rebase {
        refuse_busy!("a rebase is already stopped part-way here; finish or abort it first");
    }
    /* **Unmerged paths are the one dirty tree this cannot bank.** `git stash
    create` refuses them outright ("Cannot save the current index state"), so
    without this the press would fail three lines down with git's sentence about
    the index rather than with the reason: there is a conflict here that somebody
    has to settle before anything else happens to this tree. */
    if !conflicted.is_empty() {
        refuse!(
            "{} still has conflicts ({}) — settle them first",
            workspace,
            conflicted
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if let Some(who) = app.busy_session_in(&workspace).await {
        refuse_busy!("{who} is working here; rebasing under it would fight it");
    }
    /* **One bank at a time, or the second press loses the first.** `update-ref`
    overwrites, which would leave the earlier WIP commit unreferenced with
    nothing naming it; and on a *clean* tree there is nothing to bank, so the
    record would be cleared below while the ref stayed on disk — the strip gone
    and the work reachable only after a restart. The pane hides the button while
    a strip is up, and this route is one curl away from anybody. */
    if let Some(b) = app.workspace_banked(&workspace).await {
        refuse!(
            "{} file(s) from an earlier rebase are still banked at {} — put them back or \
             discard them first",
            b.files,
            crate::git::wip_ref(&workspace)
        );
    }

    /* **A dirty tree is banked rather than refused, and that is the whole change.**
    It used to say "commit or stash before rebasing", which is a refusal you
    answer by doing the same thing by hand — and by hand it lands on
    `refs/stash`, which every worktree of this repo shares. The bank is a ref of
    our own, written before the tree is reset and dropped only once the work is
    back, so no exit from here leaves the work anywhere but in one piece. */
    let banked = if clean {
        None
    } else {
        let (at, ws) = (path.clone(), workspace.clone());
        let bank = crate::proc::run_blocking("banking this tree's work", move || {
            crate::git::bank_wip(&at, &ws)
        })
        .await??;
        if let Some(b) = &bank {
            tracing::info!(%workspace, files = b.files, "banked the tree's work at {}", b.sha);
        }
        bank
    };
    app.set_banked(&workspace, banked.clone()).await;

    // Refresh the base first, or "behind" is answered from a stale ref. A failed
    // fetch is not fatal — rebasing onto a known-old base is sometimes what you
    // want — but it must not be silent: a network blip would otherwise look
    // identical to a clean rebase onto nothing new, which is the one outcome the
    // button exists to save you from thinking about.
    // Off the runtime, like the `rebase_onto` two lines below it and the tree
    // checks above. This is a *network* round trip, measured at ~1.5s on the
    // monorepo.
    let warning = {
        let (main, upstream) = (app.cfg.main_checkout.clone(), app.cfg.upstream_ref.clone());
        let fetched = crate::proc::run_blocking("the upstream fetch", move || {
            crate::git::fetch_upstream(&main, &upstream)
        })
        .await;
        match fetched {
            Ok(Ok(_)) => None,
            // A panic in the fetch reads the same way here as a failed fetch: the
            // base may be stale and the caller is told so.
            Ok(Err(e)) | Err(e) => {
                tracing::warn!(
                    "rebase {workspace}: upstream fetch failed, base may be stale: {e:#}"
                );
                Some(format!(
                    "upstream fetch failed — rebased onto the last-known base ({e})"
                ))
            }
        }
    };

    let upstream = app.cfg.upstream_ref.clone();
    let p = path.clone();
    let result = tokio::task::spawn_blocking(move || crate::git::rebase_onto(&p, &upstream)).await;
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            let _ = app.reconcile(&workspace).await;
            return Err(ApiError(anyhow::anyhow!("rebase task failed: {e}")));
        }
    };

    let wip = settle_bank(&app, &workspace, &path, banked.as_ref(), &result).await;
    let _ = app.reconcile(&workspace).await;
    app.notify().await;

    match result {
        Ok(()) => Ok(Json(json!({
            "rebased": workspace,
            /* **A rebase that could not put the work back is not a plain success.**
               The pane toasts the verb on a 200 and reads `warning` for anything
               else the call has to say, so a bank left standing rides that channel
               too: the strip alone is a signal in a different part of the screen
               from the one you just pressed. Joined rather than replaced, because a
               stale base is worth knowing about at the same time. */
            "warning": match (&warning, wip.as_deref()) {
                (w, Some("conflicted")) => Some(match w {
                    Some(w) => format!("{w} · your uncommitted work did not go back; it is banked"),
                    None => "your uncommitted work did not go back; it is banked".to_string(),
                }),
                (w, _) => w.clone(),
            },
            "wip": wip,
            "banked_files": banked.as_ref().map(|b| b.files),
        }))),
        // The refusal still carries git's own account of what stopped it. What the
        // bank did is in the snapshot the pane is about to redraw from, and in the
        // sentence appended here when the work is not where the caller left it.
        Err(e) => Err(ApiError(match wip.as_deref() {
            Some("banked") => anyhow::anyhow!(
                "{e:#} — your uncommitted work is banked at {}, and goes back when this ends",
                crate::git::wip_ref(&workspace)
            ),
            Some("conflicted") => anyhow::anyhow!(
                "{e:#} — and your uncommitted work conflicts with the tree it came back to; \
                 it is banked at {}",
                crate::git::wip_ref(&workspace)
            ),
            _ => e,
        })),
    }
}

/// What became of the banked work once the rebase answered, as one word for the
/// response and the log.
///
/// Three outcomes and a rule for each, and the rule that matters is the middle
/// one: a rebase left **stopped part-way** owns the tree, so the bank waits for it
/// to be finished or aborted. A rebase that failed without starting does not own
/// anything, and the work goes straight back rather than being left parked behind
/// an error the caller has already read.
async fn settle_bank(
    app: &Arc<AppState>,
    workspace: &str,
    path: &std::path::Path,
    banked: Option<&crate::model::Bank>,
    result: &anyhow::Result<()>,
) -> Option<String> {
    banked?;
    let (at, ws) = (path.to_path_buf(), workspace.to_string());
    let mid_rebase = {
        let p = at.clone();
        crate::proc::run_blocking("looking for a stopped rebase", move || {
            crate::git::rebase_in_progress(&p)
        })
        .await
        .unwrap_or(false)
    };
    if result.is_err() && mid_rebase {
        return Some("banked".to_string());
    }
    let put_back = crate::proc::run_blocking("putting the banked work back", move || {
        crate::git::restore_wip(&at, &ws)
    })
    .await;
    match put_back {
        Ok(Ok(())) => {
            app.set_banked(workspace, None).await;
            Some("reapplied".to_string())
        }
        Ok(Err(e)) | Err(e) => {
            tracing::warn!(%workspace, "the banked work did not go back: {e:#}");
            Some("conflicted".to_string())
        }
    }
}

pub async fn rebase_abort(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    let path = app
        .workspace_path(&workspace)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {workspace}"))?;
    crate::proc::run_blocking("aborting the rebase", move || {
        crate::git::rebase_abort(&path)
    })
    .await??;
    /* **An abort means undo, so the banked work comes home with it.** The press
    that banked it is the press being undone, and leaving the strip up after the
    tree has gone back to where it started would be the pane insisting on a state
    nobody is in. A conflict here keeps the bank, like every other apply. */
    let restored = match (
        app.workspace_banked(&workspace).await,
        app.workspace_path(&workspace).await,
    ) {
        (Some(_), Some(at)) => {
            let ws = workspace.clone();
            let put_back = crate::proc::run_blocking("putting the banked work back", move || {
                crate::git::restore_wip(&at, &ws)
            })
            .await;
            match put_back {
                Ok(Ok(())) => {
                    app.set_banked(&workspace, None).await;
                    Some("reapplied")
                }
                Ok(Err(e)) | Err(e) => {
                    tracing::warn!(%workspace, "the banked work did not go back: {e:#}");
                    Some("conflicted")
                }
            }
        }
        _ => None,
    };
    let _ = app.reconcile(&workspace).await;
    app.notify().await;
    Ok(Json(json!({ "aborted": workspace, "wip": restored })))
}

/// Put banked work back by hand — the strip's own button.
///
/// The retry after you have cleared whatever the apply hit the first time, and the
/// reason the bank is not thrown away on a conflict. Refused under a working agent
/// for `file_verb`'s reason: the tree changing beneath a turn is the one thing that
/// makes an agent's next command read a file nobody wrote.
pub async fn wip_restore(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    let (path, _) = banked_at(&app, &workspace).await?;
    if let Some(who) = app.busy_session_in(&workspace).await {
        refuse_busy!("{who} is working here; wait for the turn to finish");
    }
    /* Git cannot apply anything onto unmerged paths, and its own refusal is about
    the index rather than about the conflict sitting in front of you. Which is
    usually *this* bank's conflict: the press that put the strip there is what
    left those markers. */
    let p = path.clone();
    let conflicted = crate::proc::run_blocking("looking for conflicts", move || {
        crate::git::unmerged(&p).unwrap_or_default()
    })
    .await?;
    if !conflicted.is_empty() {
        refuse!(
            "settle the conflict in {} first — git cannot apply anything over unmerged paths",
            conflicted
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let (at, ws) = (path.clone(), workspace.clone());
    let put_back = crate::proc::run_blocking("putting the banked work back", move || {
        crate::git::restore_wip(&at, &ws)
    })
    .await?;
    let _ = app.reconcile(&workspace).await;
    match put_back {
        Ok(()) => {
            app.set_banked(&workspace, None).await;
            app.notify().await;
            Ok(Json(json!({ "restored": workspace })))
        }
        // The bank stands, so the pane keeps its strip and the sentence says where
        // the work is rather than only that this did not work.
        Err(e) => {
            app.notify().await;
            Err(ApiError(anyhow::anyhow!(
                "{e:#} — it is still banked at {}",
                crate::git::wip_ref(&workspace)
            )))
        }
    }
}

/// Forget banked work.
///
/// The one destructive verb here, and the SPA confirms it by name: git keeps no
/// reflog for a ref nobody else points at, so once the object is collected the
/// content is gone. Deliberately allowed while a session is working — this touches
/// a ref and never the tree.
pub async fn wip_discard(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    let (path, bank) = banked_at(&app, &workspace).await?;
    let ws = workspace.clone();
    crate::proc::run_blocking("dropping the bank", move || {
        crate::git::discard_wip(&path, &ws)
    })
    .await??;
    app.set_banked(&workspace, None).await;
    // Said out loud with the sha, because for a little while longer this is still
    // recoverable by hand and nothing else will ever name it again.
    tracing::info!(%workspace, "dropped the bank; it was {}", bank.sha);
    app.notify().await;
    /* **And answered with it, not only logged.** `update-ref -d` drops the ref and
    leaves the commit object dangling until gc collects it, so for about two
    weeks `git show <sha>` still has the work — but only for somebody holding the
    sha, and it was going to a log a launcher-started app has no terminal for.
    The pane puts it in the toast, which is the one place the person who just
    pressed Discard is looking. */
    Ok(Json(json!({ "discarded": workspace, "was": bank.sha })))
}

/// Hand the conflict to the session that is already in this workspace.
///
/// **It tells, it does not spawn.** The changed-files pane belongs to a selected
/// session, so by the time this button is on screen there is one; a workspace with
/// no live session is told to open one rather than having an agent started for it,
/// because a press that says "resolve" should not also be the press that starts an
/// agent you did not ask for.
///
/// It lands as an ordinary user turn, which is what it is: you pointed at a
/// conflict and asked for it to be sorted out.
pub async fn wip_resolve(
    State(app): State<Arc<AppState>>,
    Path(workspace): Path<String>,
) -> ApiResult<serde_json::Value> {
    let Some(bank) = app.workspace_banked(&workspace).await else {
        refuse!("{workspace} has nothing banked");
    };
    let Some(id) = app.live_sessions_in(&workspace).await.first().copied() else {
        refuse!("no live session in {workspace} — open one here and press again");
    };
    let text = format!(
        "The rebase left my uncommitted work conflicting with the new base. Both sides are in \
         the working tree as conflict markers, and my work as it was is banked at {} \
         (`git stash show -p {}` to read it). Please resolve the conflicts, keep both intents \
         where they can both stand, leave the result uncommitted, and tell me what you kept.",
        crate::git::wip_ref(&workspace),
        bank.sha
    );
    type_user_turn(&app, id, &text).await?;
    Ok(Json(json!({ "told": id })))
}
