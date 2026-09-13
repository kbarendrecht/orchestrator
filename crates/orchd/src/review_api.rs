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
    ask_token_ok, fetch_threads, mark_thread, pr_from_poll, proposal_token_ok, refuse,
    refuse_if_occupied, write_forge, ApiError, ApiResult,
};
use crate::model::*;
use crate::spawn;
use crate::state::{AppState, TriageProgress};

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

/// How far the triage pass has read. One POST per thread, from the skill.
///
/// Nothing here is durable: see [`crate::state::TriageProgress`]. A post for a PR
/// whose session has gone is kept anyway, because the run that ends by posting its
/// proposals is the ordinary case and the bar reads `posted` to say so.
pub async fn pr_triage_progress(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<TriageProgressBody>,
) -> ApiResult<serde_json::Value> {
    proposal_token_ok(&app, number, &headers).await?;
    let session = {
        let inner = app.inner.read().await;
        // The run reading this PR right now, so the bar can refuse to caption a
        // pane that belongs to somebody else.
        inner
            .sessions
            .values()
            .find(|s| s.state.is_live() && crate::triage::is_triage_of(&s.pass, number))
            .map(|s| s.id)
    };
    let Some(session) = session else {
        refuse!("no triage run for PR #{number}");
    };
    {
        let mut inner = app.inner.write().await;
        let at = inner
            .triage_progress
            .entry(number)
            .or_insert(TriageProgress {
                done: 0,
                total: body.total,
                posted: false,
                session,
            });
        at.done = body.done;
        at.total = body.total;
        at.session = session;
    }
    app.notify().await;
    Ok(Json(json!({ "ok": true })))
}

/// One thread read, out of how many this pass means to read.
#[derive(serde::Deserialize)]
pub struct TriageProgressBody {
    pub done: u32,
    pub total: u32,
}

/// Start a triage run.
///
/// Refuses on the worktree gates rather than starting a run whose output could
/// not be applied. The threads are fetched first so the viewer login is current
/// and the run has something to triage.
pub async fn pr_triage(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    start_posting_run(app, number, PostingRun::Triage).await
}

/// Work a PR's review threads in a pane, with a person watching.
///
/// **The default review verb**, and deliberately the older shape: one agent in the
/// PR's worktree running `/orchd:handle-review`, asking with `AskUserQuestion`,
/// drafting replies and posting nothing without a go. The overlay flow
/// (`pr_triage` into the cards, then a resolve run) stays a menu item away — the
/// cards are not good enough to be the only way through a review yet.
///
/// **The same worktree gates as the other review verb**, because the pass writes
/// into that tree: a rebase stopped part-way cannot take a commit, a running
/// `fix-pr` is rewriting the same history, and a dirty tree means the first thing
/// this agent amends is work somebody else left there.
///
/// It takes you to a live session already on the branch when there is one rather
/// than refusing, which is why the gate is asked *after* that. Both live in
/// `spawn_command_session`: the route asked the same two questions over again to
/// decide what that function decides three lines later.
pub async fn pr_handle_review(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let session = crate::spawn::spawn_command_session(
        &app,
        number,
        &pr.head_ref,
        crate::spawn::HANDLE_REVIEW_COMMAND,
    )
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
    start_posting_run(app, number, PostingRun::Review).await
}

/// Which of the two proposal-posting runs a route is asking for.
enum PostingRun {
    Triage,
    Review,
}

/// What the two posting-run routes do, which is everything but which run they
/// start.
///
/// The pair had drifted to being identical, refusal text included, while
/// `triage::spawn_posting_run` was already the one function they both funnel into
/// and already runs the worktree gates. Only the "nothing to answer" refusal
/// belongs here, because it needs the fetch this makes.
async fn start_posting_run(
    app: Arc<AppState>,
    number: u64,
    which: PostingRun,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let fetched = fetch_threads(&app, number).await?;
    if fetched.answerable_count() == 0 {
        refuse!("PR #{number} has no threads awaiting an answer");
    }
    let session = match which {
        PostingRun::Triage => crate::triage::spawn(&app, number, &pr.head_ref).await?,
        PostingRun::Review => crate::triage::spawn_review(&app, number, &pr.head_ref).await?,
    };
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

/// The batch — the one irreversible action.
///
/// The threads are refetched here rather than read from the cache, and that fetch
/// does four jobs at once: the head-sha staleness check, the comment ids the
/// writes are aimed at, which replies are already posted, and which threads are
/// still open for the re-request pass. See `post::run` for the order.
///
/// A refusal from the local half comes back **200 with `refused` set**, not as an
/// error: nothing was written, and the overlay renders it as a panel with the
/// decisions still staged rather than as a failed request.
pub async fn pr_post(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(batch): Json<crate::post::Batch>,
) -> ApiResult<crate::post::PostReport> {
    // One batch per PR at a time, refused rather than queued — same shape as
    // `fix_pr`'s `branch_busy`.
    //
    // This was invisible while every write was idempotent: two concurrent batches
    // would post the same reply twice and GitHub would collapse the reaction. A
    // story is neither. Both would search, both would find nothing, both would
    // create — so the check-then-file is a plain race on the one action that cannot
    // be undone.
    // Held for the whole batch and released however it ends, including a panic in
    // the middle: a leaked lock would make the PR unpostable until a restart.
    let released = app.try_claim(format!("post:{number}")).ok_or_else(|| {
        anyhow::anyhow!(
            "a batch for PR #{number} is already running; wait for it rather than \
             sending a second one"
        )
    })?;

    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let fresh = fetch_threads(&app, number).await?;
    let report = crate::post::run(&app, &pr, &fresh, batch).await?;
    drop(released);
    app.notify().await;
    Ok(Json(report))
}

/// Start the session that carries out a triaged review.
///
/// The other half of `/post`, and deliberately the same payload: your decisions,
/// resolved through the same [`crate::post::resolve`] the batch uses, so the two
/// paths cannot read one set of answers differently. What changes is who does the
/// work — an agent that adapts a fix to a branch that moved and stops to ask,
/// rather than `git apply` and a refusal.
///
/// It writes no comment and pushes nothing. The session is handed a plan and the
/// worktree; every outward write stays here, on your button.
pub async fn pr_resolve_run(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(batch): Json<crate::post::Batch>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let proposals = {
        let inner = app.inner.read().await;
        inner
            .proposals
            .get(&number)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("PR #{number} has no triage to carry out"))?
    };
    // The same gates the batch refuses on, for the same reasons: uncommitted work
    // of yours would end up in the run's commits, a stopped rebase cannot take a
    // commit at all, and `fix-pr` is rewriting this very history. A run puts an
    // agent in the worktree for minutes, so this is worth refusing before it
    // starts rather than discovering per thread.
    if let Some(ws) = app.workspace_for(&pr.head_ref).await {
        if let Some(g) = crate::triage::gate(&app, number, &ws).await? {
            refuse!("{}", g.say());
        }
    }
    // Fetched now, not from the cache: it is what makes the thread ids real and
    // the drift check mean anything.
    let fresh = fetch_threads(&app, number).await?;
    let plan = crate::post::plan(
        number,
        &proposals,
        &fresh,
        &batch,
        app.cfg.tracker.is_some(),
    )?;
    // Kept so the daemon can answer "what does this thread say" when the session
    // reports a commit. The agent is never told the reply is its to send.
    //
    // Written before the spawn, under the id it will run as: `spawn::spawn_run`
    // has the account. A run whose `claude` died at once was reaped before this
    // write, and the exit watcher — the only thing that sets `ended` — matched on
    // a record that was not there yet, so the run read as in flight until a
    // restart and every thread in it stayed `pending`.
    let session = Uuid::new_v4();
    app.inner
        .write()
        .await
        .with_resolve_runs("run started", |runs| {
            runs.insert(
                number,
                crate::state::ResolveRun {
                    session,
                    plan: plan.clone(),
                    ended: None,
                },
            );
            true
        });
    if let Err(e) = spawn::spawn_resolve_run(&app, number, &pr.head_ref, &plan, session).await {
        app.inner
            .write()
            .await
            .with_resolve_runs("run never started", |runs| runs.remove(&number).is_some());
        return Err(e.into());
    }
    app.notify().await;

    let answered = sweep_words_only(&app, number, &plan, &fresh).await;
    Ok(Json(json!({
        "session": session,
        "threads": plan.threads.len(),
        "answered": answered,
    })))
}

/// Answer the threads that need no code, now rather than never.
///
/// A words-only thread has nothing for the session to build — the prompt tells it
/// exactly that ("the daemon posts the reply. Move on.") — so no report from the
/// run will ever arrive for it, and `thread_committed` only fires on a commit.
/// Left alone these sat at `WordsOnly` for the life of the run with no button to
/// finish them.
///
/// No per-thread confirmation, unlike a committed thread: that card exists to
/// show the *real* diff beside the drafted reply, because a commit can differ
/// from what triage staged. Here there is no commit and nothing can drift — the
/// words are the ones approved on the card — so asking again would be ceremony.
///
/// One thread failing does not stop the others, for the same reason `post_outward`
/// keeps going: each is an independent write, and a run that answered three of
/// four should say so rather than lose all four.
async fn sweep_words_only(
    app: &Arc<AppState>,
    number: u64,
    plan: &crate::post::Plan,
    fresh: &crate::forge::Threads,
) -> usize {
    use crate::post::{Posted, ThreadStatus};

    let todo: Vec<_> = plan
        .threads
        .iter()
        .filter(|t| t.status == ThreadStatus::WordsOnly)
        .cloned()
        .collect();
    if todo.is_empty() {
        return 0;
    }
    let forge = match write_forge(app) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(
                "resolve run #{number}: no forge to answer words-only threads: {:#}",
                e.0
            );
            return 0;
        }
    };
    let at = app.cfg.main_checkout.clone();

    let mut answered = 0;
    for t in todo {
        // A stance with no words is a bare thumbs up; the two are never both.
        let outcome = match &t.reply {
            Some(reply) => crate::post::post_one(
                app,
                &forge,
                &at,
                number,
                &t.thread_id,
                reply,
                t.story.as_ref(),
                fresh,
            )
            .await
            .map(Some),
            None if t.stance.gives_thumbs_up() => {
                crate::post::react_one(&forge, &at, &t.thread_id, fresh)
                    .await
                    .map(|()| None)
            }
            // Neither words nor a reaction: nothing was ever going to leave for
            // this one, so it is done rather than stuck.
            None => Ok(None),
        };

        match outcome {
            Ok(Some(Posted::HeldNoStory(why))) => {
                mark_thread(app, number, &t.thread_id, |x| {
                    x.note = Some(format!("story not filed — {why}"));
                })
                .await;
            }
            Ok(_) => {
                answered += 1;
                mark_thread(app, number, &t.thread_id, |x| {
                    x.status = ThreadStatus::Replied;
                })
                .await;
            }
            Err(e) => {
                tracing::error!("resolve run #{number}: {} — {e:#}", t.location);
                mark_thread(app, number, &t.thread_id, |x| {
                    x.note = Some(format!("could not answer — {e:#}"));
                })
                .await;
            }
        }
    }
    app.notify().await;
    answered
}

/// `--force-with-lease` a branch, off the runtime, with its base read on the same
/// hop.
///
/// One function because the two callers had it written out identically and each
/// disagreed with itself a few lines away: both reached for a bare
/// `spawn_blocking` and a hand-written `.context("the push panicked")` while their
/// neighbours used [`crate::proc::run_blocking`], which exists so the `JoinError`
/// is named the same way everywhere. `push_with_lease` re-states the base-branch
/// rule itself, because a daemon push never passes through the `PreToolUse` hook.
/// Push the branch a run has been committing to.
///
/// Its own button, and deliberately not the end of the run: a run can answer four
/// threads and leave two for you, and pushing that is a judgement about whether
/// what is on the branch is worth showing. `--force-with-lease` only, through the
/// same helper every other push here uses.
pub async fn pr_run_push(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let path = gate_worktree(&app, number).await?;
    app.push_branch(path, pr.head_ref.clone()).await?;
    // Re-measure, or the overview keeps saying there is work to push: `unpushed`
    // is the last reconcile's number, and this is the moment it stopped being true.
    if let Some(ws) = app.workspace_for(&pr.head_ref).await {
        let _ = app.reconcile(&ws).await;
    }
    app.notify().await;
    Ok(Json(json!({ "pushed": pr.head_ref })))
}

/// Ask for a fresh review, from the reviewers whose threads are all answered.
///
/// Held back per reviewer rather than all-or-nothing: someone with an open thread
/// of their own is not being asked to look again at work that has not answered
/// them. Also its own button, because re-requesting is a claim that you are done.
pub async fn pr_run_rerequest(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let fresh = fetch_threads(&app, number).await?;
    let forge = write_forge(&app)?;
    let at = app.cfg.main_checkout.clone();

    // The threads this run answered, which is what decides whose review can be
    // asked for again. Taken from the run's own record: `Replied` is the only
    // status where the reviewer has actually been told something. `Held` and
    // `NeedsYou` carry a commit but no answer, and a `Manual` thread is yours —
    // each of those rightly holds its author back.
    //
    // This used to be derived from `!is_resolved` instead, and that could never
    // work: resolving is the reviewer's button and the daemon never presses it, so
    // every thread the run had just answered still read as open and nobody was
    // ever ready. See `post::rerequest_all`, which is now the one implementation.
    let done: Vec<String> = {
        let inner = app.inner.read().await;
        inner
            .resolve_runs
            .get(&number)
            .map(|r| {
                r.plan
                    .threads
                    .iter()
                    .filter(|t| t.status == crate::post::ThreadStatus::Replied)
                    .map(|t| t.thread_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    let done: Vec<&str> = done.iter().map(String::as_str).collect();

    let out = crate::post::rerequest_all(&forge, &at, number, &fresh, &done).await;
    app.notify().await;
    Ok(Json(json!({
        "rerequested": out.asked,
        "failed": out.failed.iter().map(|(who, e)| format!("{who}: {e}")).collect::<Vec<_>>(),
        // Said rather than left as silence: "nobody to re-request" and "three
        // people are still waiting on you" are different answers.
        "held_back": out.held.iter().map(|(who, t)| format!("{who} — {t} is still unanswered")).collect::<Vec<_>>(),
    })))
}

/// What you have edited since the manual phase opened.
///
/// The tree against `HEAD`, which after the phase's own commit is exactly your
/// hand-written work and nothing else — the ordering is what makes that true. Its
/// own endpoint rather than the diff viewer's, because the phase wants one refresh
/// call returning both the file list and the patch text, and because `git diff`
/// being the source is the point: nobody declared these files, so the list cannot
/// be wrong about them.
pub async fn pr_manual(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
) -> ApiResult<serde_json::Value> {
    let path = gate_worktree(&app, number).await?;
    let (files, diff) = tokio::task::spawn_blocking(move || crate::patch::worktree_change(&path))
        .await
        .context("reading the worktree diff panicked")??;
    Ok(Json(json!({ "files": files, "diff": diff })))
}

/// Finish a batch that stopped for the manual phase.
///
/// The same lock and the same pipeline as `/post`; the difference is that the local
/// half folds *your* edits rather than applying a patch, and the clean-tree gate
/// stands down because the phase is what asked you to make it dirty.
pub async fn pr_manual_done(
    State(app): State<Arc<AppState>>,
    Path(number): Path<u64>,
    Json(done): Json<crate::post::Finish>,
) -> ApiResult<crate::post::PostReport> {
    let released = app.try_claim(format!("post:{number}")).ok_or_else(|| {
        anyhow::anyhow!(
            "a batch for PR #{number} is already running; wait for it rather than \
             sending a second one"
        )
    })?;

    let pr = {
        let inner = app.inner.read().await;
        pr_from_poll(&inner.prs, number)?
    };
    let fresh = fetch_threads(&app, number).await?;
    let report = crate::post::finish(&app, &pr, &fresh, done).await?;
    drop(released);
    app.notify().await;
    Ok(Json(report))
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
            Some(Pass { pr, command }) if command == crate::triage::COMMAND => *pr,
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
                command: crate::triage::COMMAND.to_string(),
            })
        };

        let (rid, rtok, r) = put(review(pr_num));
        let (fid, ftok, f) = put(Some(Pass {
            pr: pr_num,
            command: crate::fix_pr::COMMAND.to_string(),
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
