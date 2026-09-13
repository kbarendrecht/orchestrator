//! The review overlay's routes: triage, the batch, a resolve run, and the
//! hand-off to `fix-pr`.
//!
//! **Split out of `api` because it is one flow, not one layer.** Twenty-odd
//! handlers carry a PR from "read the threads" through "post the replies", with a
//! run's credential, the worktree gates and the phase record between them. They
//! read as a thousand lines in the middle of the route table.
//!
//! The forge helpers stay in [`crate::api`] — `write_forge`, `fetch_threads` and
//! `pr_from_poll` are used by the thread routes and by `open_file` as well, and
//! taking them along would have made the two modules import each other. This one
//! imports `api`; `api` does not import this one, because the routes are built in
//! `orchd-serve`.

use anyhow::Context;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

use crate::api::{
    ask_token_ok, fetch_threads, pr_from_poll, proposal_token_ok, refuse, refuse_if_occupied,
    write_forge, ApiError, ApiResult,
};
use crate::model::*;
use crate::spawn;
use crate::state::AppState;

/// What the vendored `triage` skill needs to know before it can read anything.
///
/// **A skill is static; the prompt it replaced was rendered per run.**
/// `commands/triage.md` carried seven substitutions a renderer filled in, and a
/// file handed to every session cannot have any of them. So the values come from
/// here instead, which is also what makes the skill work when a person types
/// `/orchd:triage` by hand rather than the daemon typing it. `review` reads the
/// same answer; every other run takes its values out of the environment, and
/// `commands/` and its renderer are gone.
///
/// Carries the run credential like the proposals route, and answers the same way
/// to the app token, so the SPA can look at it too.
pub async fn pr_triage_context(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    headers: axum::http::HeaderMap,
) -> ApiResult<serde_json::Value> {
    proposal_token_ok(&app, number, &headers).await?;
    let (owner, repo) =
        crate::resolve_repo(&app).context("no GitHub repo configured and none on the remote")?;
    // The viewer, from the same fetch every other caller takes it from: the skill
    // uses it to spot a thread it has already answered, and `gh api user` from the
    // agent would be a second source for a fact the daemon just fetched.
    let login = fetch_threads(&app, number).await.map(|f| f.viewer).ok();
    let base = format!("http://127.0.0.1:{}/api/pr/{number}", app.cfg.port);
    Ok(Json(json!({
        "pr": number,
        "owner": owner,
        "repo": repo,
        // Whose PR it is, which is how the skill spots a thread it has already
        // answered. `None` when the poll has not seen this PR, and the skill then
        // asks GitHub itself rather than guessing.
        "login": login,
        "language": app.cfg.default_language,
        // Whether `story+reply` may be offered at all: an option the daemon would
        // refuse should never reach a card.
        "tracker": app.cfg.tracker.is_some(),
        // The host a story URL it reports has to be on. The skill needs it because
        // it may not assemble a URL itself — `StoryRef::consistent` checks the
        // authority — and it cannot be a constant in the file now that a tracker is
        // config (`config::Tracker`).
        "tracker_host": app.cfg.tracker.as_ref().map(|t| t.host.clone()),
        "proposals_url": format!("{base}/proposals"),
        "progress_url": format!("{base}/triage/progress"),
        // The ref the branch is measured against, for the review session's "CI red
        // or behind the base → stop, that is fix-pr's job". The PR's *own* base
        // where the poller knows it, which is what `rebase_target` decides, so the
        // two passes cannot disagree about what "behind" means.
        "upstream": crate::spawn::rebase_target(
            &app.cfg.upstream_ref,
            &app.cfg.upstream_remote,
            app.inner.read().await.pr(number).map(|p| p.base_ref.clone()).as_deref(),
        ),
    })))
}

pub struct TriageProgressBody {
    pub done: u32,
    pub total: u32,
}

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

/// Start the overlay review session.
///
/// The single-session flow: it posts proposals like triage, then stays alive to
/// take the human's decisions over the ask channel and carry out the change and
/// the post. Same worktree gates as triage — it writes into this tree, so it must
/// start clean.
pub async fn pr_review_session(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    start_posting_run(app, number).await
}

/// Start the overlay review session, or say why not.
///
/// **The pair this collapsed from is the batch's half going.** There were two
/// posting runs — the headless triage pass and this one — and a `PostingRun` enum
/// picking between them, while everything else about the two routes was identical
/// down to the refusal text. One run is left, so the enum is the thing that was
/// really describing the difference and it goes with it.
///
/// The "nothing to answer" refusal belongs here rather than in the spawner,
/// because it needs the fetch this makes.
async fn start_posting_run(app: Arc<AppState>, number: u64) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let fetched = fetch_threads(&app, number).await?;
    if fetched.answerable_count() == 0 {
        refuse!("PR #{number} has no threads awaiting an answer");
    }
    let session = crate::triage::spawn_review(&app, number, &pr.head_ref).await?;
    Ok(Json(json!({ "session": session })))
}

/// The agent's hand-off. **The only endpoint a subprocess calls.**
///
/// Treated as hostile input: the agent's own input includes review comments other
/// people wrote, so nothing here is trusted for having parsed. The shape is
/// checked by `ProposalSet::validate`, the thread ids against a **fresh** fetch,
/// and `base_sha` against the branch as it stands — a force-push during the run
/// invalidates every patch it produced.
pub async fn pr_proposals(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<crate::proposal::ProposalSet>,
) -> ApiResult<serde_json::Value> {
    // Before the fetch: this route spends the GitHub token on `fetch_threads`, so
    // an unauthenticated caller could otherwise drive outbound traffic and burn
    // the rate limit without ever getting past validation.
    proposal_token_ok(&app, number, &headers).await?;
    let fetched = fetch_threads(&app, number).await?;

    if let Some(head) = &fetched.head_sha {
        if head != &body.base_sha {
            refuse!(
                "the branch moved during triage ({} → {head}); its patches no longer apply",
                crate::git::short(&body.base_sha)
            );
        }
    }

    let answerable: Vec<String> = fetched
        .items
        .iter()
        .filter(|t| t.answerable)
        .map(|t| t.id.clone())
        .collect();
    let validated = body.validate(&answerable)?;
    let count = validated.proposals.len();

    {
        let mut inner = app.inner.write().await;
        inner.proposals.insert(number, validated);
        // What turns the review bar from a count into "your turn". Only when a
        // pass reported progress: a run that posted without one leaves nothing to
        // caption, and inventing an entry here would caption a pane with a total
        // nobody counted.
        if let Some(at) = inner.triage_progress.get_mut(&number) {
            at.posted = true;
            at.done = at.total;
        }
    }
    app.notify().await;
    Ok(Json(json!({ "accepted": count })))
}

/// Everything the overlay needs in one call: the threads, what triage proposed,
/// and whether the worktree can be written to.
pub async fn pr_review(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let fetched = fetch_threads(&app, number).await?;

    let gate = match app.workspace_for(&pr.head_ref).await {
        Some(ws) => crate::triage::gate(&app, number, &ws).await?,
        // No worktree yet means nothing to be dirty; triage creates one.
        None => None,
    };
    let (proposals, manual) = {
        let inner = app.inner.read().await;
        (
            inner.proposals.get(&number).cloned(),
            // A closed record is a "we pushed this" marker, not a phase to resume;
            // serving one put the overlay on an empty manual screen with its push
            // button enabled.
            inner.manual.get(&number).filter(|p| p.open).cloned(),
        )
    };

    Ok(Json(json!({
        // The overlay's header reads all three, each behind a fallback — so they
        // were never missed: the title read "review", the branch read "this
        // branch", and the GitHub button was hidden by its own `if`. Found by
        // typing the payload against the structs it is built from.
        "title": pr.title,
        "url": pr.url,
        "head_ref": pr.head_ref,
        "viewer": fetched.viewer,
        "head_sha": fetched.head_sha,
        "answerable": fetched.answerable_count(),
        "threads": fetched.items,
        "proposals": proposals,
        // A batch that stopped for the manual phase. Served so a reload or a restart
        // resumes it rather than stranding a branch whose patches are committed.
        "manual": manual,
        "gate": gate,
        // Shown in the header, never gating: a red or conflicting PR is still
        // answerable, and `fix-pr` is offered rather than required.
        "checks": pr.checks,
        "mergeable": pr.mergeable,
        // Whether a `story+reply` position can be acted on at all. The overlay
        // should not be welded to Shortcut, so with no tracker it hides the
        // option rather than offering something that would be refused.
        "tracker": app.cfg.tracker.is_some(),
    })))
}

#[derive(Deserialize)]
pub struct CommitBody {
    pub message: String,
}

/// The gate's `commit…` button: commit the worktree as it stands.
pub async fn pr_commit(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(body): Json<CommitBody>,
) -> ApiResult<serde_json::Value> {
    let path = gate_worktree(&app, number).await?;
    let message = body.message.clone();
    crate::proc::run_blocking("the gate's commit", move || {
        crate::git::commit_all(&path, &message)
    })
    .await??;
    app.notify().await;
    Ok(Json(json!({ "committed": true })))
}

/// The gate's `stash` button. Never popped automatically — see `git::stash`.
pub async fn pr_stash(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let path = gate_worktree(&app, number).await?;
    crate::proc::run_blocking("the gate's stash", move || crate::git::stash(&path)).await??;
    app.notify().await;
    Ok(Json(json!({ "stashed": true })))
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

    let workspace = match body.place.as_str() {
        "worktree" => spawn::ensure_pr_worktree(&app, number, &pr.head_ref).await?,
        "main" => {
            // Moves main's checkout, so under the swap's lock — see `move_out_of_main`.
            let _swap = app.swapping.try_lock().map_err(|_| {
                ApiError(anyhow::anyhow!(
                    "a swap is already running; it moves main's checkout, so give it a \
                     moment and look at the rail before asking again"
                ))
            })?;
            spawn::switch_main_to_pr(&app, &pr.head_ref).await?
        }
        other => refuse!("unknown place {other}"),
    };

    // `ensure_pr_worktree` reuses a worktree that already holds the branch, so
    // pressing this twice would otherwise stack a second session in it. (The `main`
    // arm already refused an occupied main inside `switch_main_to_pr`.)
    refuse_if_occupied(&app, &workspace).await?;
    let id = spawn::spawn_session(&app, &workspace, None, None).await?;
    Ok(Json(json!({ "session": id, "workspace": workspace })))
}

/// The worktree the gate buttons act on, refusing when there is not one.
async fn gate_worktree(app: &Arc<AppState>, number: u64) -> Result<std::path::PathBuf, ApiError> {
    let head_ref = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?.head_ref
    };
    let ws = app
        .workspace_for(&head_ref)
        .await
        .ok_or_else(|| anyhow::anyhow!("no worktree for PR #{number} yet"))?;
    app.workspace_path(&ws)
        .await
        .ok_or_else(|| ApiError(anyhow::anyhow!("the worktree for PR #{number} vanished")))
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
        }));
    }
    // Off the runtime: a file read, which is disk and can be a large file.
    let (file, shared) = (q.path.clone(), app.cfg.shared_worktree_paths.clone());
    Ok(Json(
        crate::proc::run_blocking("reading a file", move || {
            crate::edit::read(&root, &file, &shared)
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
    let out = crate::edit::write(
        &root,
        &body.path,
        &body.content,
        &body.version,
        &app.cfg.shared_worktree_paths,
    )?;
    if matches!(out, crate::edit::WriteOutcome::Written { .. }) {
        // Agents working in this workspace hold a stale copy now, and will
        // overwrite it unless they are told (§5's invalidation, in the
        // direction that actually loses work).
        let resolved =
            crate::edit::resolve_in_workspace(&root, &body.path, &app.cfg.shared_worktree_paths)?;
        app.record_human_edit(resolved).await;
        // The changed-file pane and the diff must both reflect the write.
        let _ = app.reconcile(&body.workspace).await;
        app.notify().await;
    }
    Ok(Json(out))
}

// ---------------------------------------------------------------------------
// The review's hand-off
// ---------------------------------------------------------------------------

/// What one thread's reply carries. Absent words mean a reaction and nothing else.
#[derive(Deserialize)]
pub struct ThreadReply {
    /// The words to post, as the human edited them. `None` is the bare 👍 a thread
    /// gets when it was applied exactly as asked and there is nothing to add.
    #[serde(default)]
    pub reply: Option<String>,
    /// Filed before the reply, so the reply can carry the id. The reply must then
    /// contain `{story}`, which is substituted once the story exists.
    #[serde(default)]
    pub story: Option<crate::proposal::StoryDraft>,
}

/// Post one thread's reply, on behalf of the session that read it.
///
/// **The four rules the agent would otherwise be asked to remember.** A review
/// session posts its own replies — it holds the decisions and it is the thing with
/// a worktree — and it could do it with `gh` in two lines. What it cannot do in two
/// lines is what [`crate::post::post_one`] does around the write: refuse a reply
/// already on the thread, file the story first and substitute `{story}` into the
/// words, refuse a story position with no tracker configured, and append the
/// footer. Three of those are idempotency, and the fourth is load-bearing for
/// something else entirely — `forge::acknowledged` reads `(via orchestrator)` to
/// decide whether a thread still awaits you, so a reply posted without it is a
/// thread that reads unanswered for ever.
///
/// So the words come from the agent and the writing stays here, where the rules are
/// enforced rather than remembered and eleven tests say so.
///
/// The PR is the session's own, from its `Pass` — not a number in the body, which
/// would let a session holding one review's token post on another's threads.
pub async fn thread_reply(
    State(app): State<Arc<AppState>>,
    Path((id, thread_id)): Path<(Uuid, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ThreadReply>,
) -> ApiResult<serde_json::Value> {
    ask_token_ok(&app, id, &headers).await?;

    let number = {
        let inner = app.inner.read().await;
        match inner.sessions.get(&id).and_then(|s| s.pass.as_ref()) {
            Some(Pass { pr, command }) if command == Pass::REVIEW => *pr,
            _ => refuse!("only a review session posts a reply, and {id} is not one"),
        }
    };

    // Fetched now rather than reused: the thread must still be there, and the
    // comment id the write needs is this fetch's, not the one the read pass saw.
    let fresh = fetch_threads(&app, number).await?;
    let forge = write_forge(&app)?;
    let main = app.cfg.main_checkout.clone();

    let Some(reply) = body
        .reply
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    else {
        // A thread applied exactly as asked, with nothing to add. The reaction is
        // the whole of what is said, and it is still a write — so it goes through
        // the same seam rather than being left to the agent.
        crate::post::react_one(&forge, &main, &thread_id, &fresh).await?;
        app.notify().await;
        return Ok(Json(json!({ "posted": false, "reacted": true })));
    };

    let posted = crate::post::post_one(
        &app,
        &forge,
        &main,
        number,
        &thread_id,
        reply,
        body.story.as_ref(),
        &fresh,
    )
    .await?;

    app.notify().await;
    Ok(Json(json!({
        "posted": !matches!(posted, crate::post::Posted::HeldNoStory(_)),
        // Already there is a success: a retry after a failed re-request must not
        // post the same words twice, and the agent has no way to tell otherwise.
        "already": posted == crate::post::Posted::AlreadyThere,
        "held": match &posted {
            crate::post::Posted::HeldNoStory(why) => json!(why),
            _ => serde_json::Value::Null,
        },
    })))
}

/// A review session reporting that its own work is finished.
///
/// The last thing `skills/review/SKILL.md` does. Its phase 3 ends with the code
/// pushed and the replies posted, and the prompt then forbids the one job that is
/// left — "CI still red or the branch behind → say so and stop. That is `fix-pr`'s
/// job". This is how it says so to something that can act on it.
///
/// **The agent does not decide, and is not believed.** It says it is done; the
/// daemon reads the PR out of its own poll and decides whether anything is left to
/// watch, exactly as [`crate::fix_pr::settle`] re-reads the check state instead of
/// trusting a run's report. All this route takes from the caller is the fact that
/// the review reached its end.
///
/// Answers before it acts, on the hooks' rule: the caller is the session this is
/// about to kill, so finishing the work inside the request would cut the answer off
/// at the wire it is travelling on.
pub async fn session_handoff(
    State(app): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
) -> ApiResult<serde_json::Value> {
    ask_token_ok(&app, id, &headers).await?;

    // Only a review session, and only about its own PR. The ask token already says
    // *which* session is calling; this says it is one entitled to hand anything on.
    // Both reads under one lock: which PR this session is about, and what the poll
    // last said about it.
    let hand_on = {
        let inner = app.inner.read().await;
        let pr = match inner.sessions.get(&id).and_then(|s| s.pass.as_ref()) {
            Some(Pass { pr, command }) if command == Pass::REVIEW => *pr,
            _ => {
                refuse!("only a review session hands over, and {id} is not one")
            }
        };
        // No poll to read is not a reason to start a force-pushing run. Same
        // direction as `Checks::Unknown`: when the daemon cannot see, it does
        // nothing.
        (pr, inner.pr(pr).is_some_and(crate::fix_pr::wants_watching))
    };
    let (pr, hand_on) = hand_on;

    {
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            s.fix_pr_on_exit = hand_on;
        }
    }

    // Detached, so the answer is already on its way out when the pty dies under the
    // curl that asked. The review is over either way — the overlay reads the session
    // ending as its report — and `watch_session_exit` starts the run if the flag says
    // to, because a run cannot start while this session still holds the branch.
    let app2 = app.clone();
    tokio::spawn(async move {
        let handle = {
            let inner = app2.inner.read().await;
            inner.sessions.get(&id).and_then(|s| s.pty.clone())
        };
        if let Some(h) = handle {
            // Escalating, because the hand-off is *armed on the exit*: a review
            // that trapped `SIGHUP` would keep the flag set and the branch held,
            // and the run it asked for could never start.
            if h.kill_gracefully().await.is_none() {
                tracing::warn!(session = %id, "review handed over but would not close");
            }
        }
    });

    tracing::info!(session = %id, pr, hand_on, "review handed over");
    Ok(Json(json!({ "fix_pr": hand_on })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-off is the review's, and only about a PR that still has something
    /// to watch.
    ///
    /// Both halves matter for the same reason: what this arms is a run that rebases
    /// and force-pushes, and it arms it to fire on a pty exit, where nobody is
    /// watching for a refusal. So the narrowing is done here, at the call, rather
    /// than left to the guard table alone.
    #[tokio::test]
    async fn only_a_review_hands_over_and_only_when_the_pr_needs_watching() {
        use crate::forge::{Checks, Pr};
        use crate::model::{Pass, Session};

        let (app, dir) = crate::testutil::app("handoff");
        let pr_num = 10001u64;

        // Red and pushable: a PR there is something to hand over about.
        let mut red = Pr {
            head_repo: Some("me/repo".into()),
            head_pushable: Some(true),
            checks: Checks::Failing,
            head_sha: Some("abc".into()),
            ..crate::testutil::pr(pr_num)
        };

        let put = |pass: Option<Pass>| {
            let s = Session::new(Uuid::new_v4(), "wt".to_string(), dir.clone(), pass);
            (s.id, s.ask_token.clone(), s)
        };
        let review = |pr: u64| {
            Some(Pass {
                pr,
                command: Pass::REVIEW.to_string(),
            })
        };

        let (rid, rtok, r) = put(review(pr_num));
        let (fid, ftok, f) = put(Some(Pass {
            pr: pr_num,
            command: Pass::FIX_PR.to_string(),
        }));
        let (iid, itok, i) = put(None);
        {
            let mut inner = app.inner.write().await;
            inner.prs = vec![red.clone()];
            for s in [r, f, i] {
                inner.sessions.insert(s.id, s);
            }
        }
        let hdr = |v: &str| {
            let mut h = axum::http::HeaderMap::new();
            h.insert("x-orch-ask", v.parse().unwrap());
            h
        };
        let flag = |id: Uuid| {
            let app = app.clone();
            async move { app.inner.read().await.sessions[&id].fix_pr_on_exit }
        };

        // A fix run and an ordinary pane are refused, on their own valid tokens:
        // the token says who is calling, not what they are entitled to hand on.
        for (id, tok) in [(fid, ftok), (iid, itok.clone())] {
            let said = match session_handoff(State(app.clone()), Path(id), hdr(&tok)).await {
                Ok(_) => panic!("only a review hands over"),
                Err(e) => e.0.to_string(),
            };
            assert!(said.contains("not one"), "{said}");
        }
        // And a review on somebody else's token is not a review calling.
        assert!(session_handoff(State(app.clone()), Path(rid), hdr(&itok))
            .await
            .is_err());

        // The real thing: red PR, so the checks are handed on.
        let out = match session_handoff(State(app.clone()), Path(rid), hdr(&rtok)).await {
            Ok(o) => o,
            Err(e) => panic!("the review may hand over: {}", e.0),
        };
        assert_eq!(out.0["fix_pr"], true);
        assert!(flag(rid).await, "the exit must start a run");

        // Green and mergeable: the review is simply over. It still answers, because
        // ending the session is the other half of what this call is for — the
        // overlay reads that as its report either way.
        red.checks = Checks::Passing;
        app.inner.write().await.prs = vec![red];
        let out = match session_handoff(State(app.clone()), Path(rid), hdr(&rtok)).await {
            Ok(o) => o,
            Err(e) => panic!("still answers: {}", e.0),
        };
        assert_eq!(out.0["fix_pr"], false);
        assert!(!flag(rid).await, "nothing to watch, nothing armed");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
