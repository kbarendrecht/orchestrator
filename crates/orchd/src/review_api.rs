//! The PR routes that start work, and the editable file pane's reads and writes.
//!
//! **Split out of `api` because they were one flow, not one layer.** This module
//! held the review overlay's twenty-odd handlers, from "read the threads" through
//! "post the replies". The overlay is gone and `handle-review` in a pane is the one
//! review verb, so what is left is the pane's spawn, a plain session on a PR's
//! branch, and the file routes that had always lived here beside them.
//!
//! The forge helpers stay in [`crate::api`]. This module imports `api`; `api` does
//! not import this one, because the routes are built in `orchd-serve`.

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::api::{pr_from_poll, refuse, refuse_busy, refuse_if_occupied, ApiError, ApiResult};
use crate::model::*;
use crate::spawn;
use crate::state::AppState;

/// Start `handle-review` in a pane: the rail's review verb, one agent with a person
/// watching.
pub async fn pr_handle_review(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let session =
        crate::triage::spawn_command_session(&app, number, &pr.head_ref, Pass::HANDLE_REVIEW)
            .await?;
    Ok(Json(json!({ "session": session })))
}

/// Where an `open` request wants the session.
#[derive(Deserialize)]
pub struct OpenPr {
    #[serde(rename = "where")]
    pub place: String,
}

/// Start working on a PR, without the review flow.
///
/// A worktree pinned to its head branch, or the main checkout switched onto it —
/// which is what you want when the PR needs the docker stack and `ng-watch` that
/// only main has. Main is refused rather than disturbed: switching a branch out
/// from under uncommitted work, or under a session already sitting there, is not
/// something a right-click menu gets to do.
pub async fn open_pr(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(body): Json<OpenPr>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        inner.pr(number).cloned()
    }
    .ok_or_else(|| anyhow::anyhow!("PR #{number} is not in the current poll"))?;

    /* Moves main's checkout, so under the swap's lock — see `move_out_of_main`.
    **Held until the session has claimed main, not only across the switch.** It used
    to drop at the end of the arm, which left main on the PR's branch with nobody
    recorded in it while the spawn was still on its way: `park_main`, run by any
    session's exit, reads exactly that as "the last session left" and puts main back
    on base, and a swap or a move out reads it as free. The spawn below is the
    claim, so the guard lives as long as this function. */
    let mut _swap = None;
    let workspace = match body.place.as_str() {
        "worktree" => spawn::ensure_pr_worktree(&app, number, &pr.head_ref).await?,
        "main" => {
            _swap = Some(app.swapping.try_lock().map_err(|_| {
                ApiError(anyhow::anyhow!(
                    "a swap is already running; it moves main's checkout, so give it a \
                     moment and look at the rail before asking again"
                ))
            })?);
            spawn::switch_main_to_pr(&app, number, &pr.head_ref).await?
        }
        other => refuse!("unknown place {other}"),
    };

    // `ensure_pr_worktree` reuses a worktree that already holds the branch, so
    // pressing this twice would otherwise stack a second session in it. (The `main`
    // arm already refused an occupied main inside `switch_main_to_pr`.)
    refuse_if_occupied(&app, &workspace).await?;
    let agent = app.agent_for(None).await?;
    let id = spawn::spawn_session(&app, &workspace, None, spawn::Source::New(agent)).await?;
    /* Named for the PR (#36). Until Claude Code titled it, the row fell back to the
    workspace, which is `pr-1234` — the number again and nothing else. A name and
    not a title, so Claude Code's own does not replace it a turn later; the same
    cap the rename route has, so the row stays one line. */
    let name: String = format!("#{number} - {}", pr.title)
        .chars()
        .take(80)
        .collect();
    app.with_session(id, |s| s.name = Some(name)).await;
    // Persists on the way out, as a rename does.
    app.notify().await;
    Ok(Json(json!({ "session": id, "workspace": workspace })))
}

// ---------------------------------------------------------------------------
// Editable right pane (§5, step 9)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct FileQuery {
    pub workspace: String,
    pub path: String,
    /// Read the file as it was at this base instead of from the working tree.
    /// Used for the read-only left pane, and never writable.
    #[serde(default)]
    pub base: Option<crate::diff::Base>,
    #[serde(default)]
    pub pr_base: Option<String>,
}

pub async fn read_file(
    State(app): State<Arc<AppState>>,
    Query(q): Query<FileQuery>,
) -> ApiResult<crate::edit::FileContents> {
    let root = app
        .workspace_path(&q.workspace)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {}", q.workspace))?;

    if let Some(base) = q.base {
        // Off the runtime, and one hop for the pair: a `resolve_base` exec followed
        // by a `git show` of the whole file.
        let (at, file) = (root.clone(), q.path.clone());
        let (upstream, pr_base) = (app.cfg.upstream_ref.clone(), q.pr_base.clone());
        let content = crate::proc::run_blocking("reading a file at a revision", move || {
            let rev = crate::diff::resolve_base(&at, base, &upstream, pr_base.as_deref())?;
            crate::diff::show_at(&at, &rev, &file)
        })
        .await??;
        return Ok(Json(crate::edit::FileContents {
            path: q.path.clone(),
            bytes: content.len() as u64,
            // No version: a historical revision is never written back.
            version: String::new(),
            content,
            read_only: false,
        }));
    }
    // Off the runtime: a file read, which is disk and can be a large file.
    let (file, checkout) = (q.path.clone(), app.cfg.main_checkout.clone());
    Ok(Json(
        crate::proc::run_blocking("reading a file", move || {
            crate::edit::read(&root, &file, &checkout)
        })
        .await??,
    ))
}

#[derive(Deserialize)]
pub struct WriteBody {
    pub workspace: String,
    pub path: String,
    pub content: String,
    /// The version the buffer was loaded at. A mismatch means an agent edited
    /// the file underneath you, and the write is refused (§5).
    pub version: String,
}

pub async fn write_file(
    State(app): State<Arc<AppState>>,
    Json(body): Json<WriteBody>,
) -> ApiResult<crate::edit::WriteOutcome> {
    let root = app
        .workspace_path(&body.workspace)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown workspace {}", body.workspace))?;
    /* Refused mid-turn, like the file verbs. The version check catches an agent
    that already wrote the file, not one about to: it reads, you write, and its
    next edit lands on a copy that no longer exists. The diff's revert arrow made
    that a click away. */
    if let Some(who) = app.busy_session_in(&body.workspace).await {
        refuse_busy!("{who} is mid-turn in {}, wait for it", body.workspace);
    }
    let out = crate::edit::write(
        &root,
        &body.path,
        &body.content,
        &body.version,
        &app.cfg.main_checkout,
    )?;
    if matches!(out, crate::edit::WriteOutcome::Written { .. }) {
        // Agents working in this workspace hold a stale copy now, and will
        // overwrite it unless they are told (§5's invalidation, in the
        // direction that actually loses work).
        let resolved =
            crate::edit::resolve_in_workspace(&root, &body.path, &app.cfg.main_checkout)?;
        app.record_human_edit(resolved).await;
        // The changed-file pane and the diff must both reflect the write.
        let _ = app.reconcile(&body.workspace).await;
        app.notify().await;
    }
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// A write is refused while an agent in that workspace is mid-turn, and
    /// goes through once it is not. The version check alone cannot see an
    /// agent that read the file and has not written it yet.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_waits_for_an_agent_mid_turn() {
        use crate::model::{Session, State as S, MAIN};

        let (app, dir) = crate::testutil::app("write-busy");
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        let version = crate::edit::read(&dir, "f.txt", &dir).unwrap().version;
        let go = |content: &str| {
            let (app, content, version) = (app.clone(), content.to_string(), version.clone());
            async move {
                write_file(
                    State(app),
                    Json(WriteBody {
                        workspace: MAIN.to_string(),
                        path: "f.txt".into(),
                        content,
                        version,
                    }),
                )
                .await
            }
        };

        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), dir.clone(), None);
            s.set_state(S::Working);
            inner.sessions.insert(id, s);
        }
        let e = go("two\n").await.expect_err("an agent is working");
        assert!(format!("{:#}", e.0).contains("mid-turn"));
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "one\n");

        app.inner.write().await.sessions.remove(&id);
        assert!(go("two\n").await.is_ok());
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "two\n");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
