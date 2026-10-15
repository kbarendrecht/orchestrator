use serde::Serialize;
use std::time::SystemTime;

use crate::forge::Pr;
use crate::model::{PrAutomation, SessionId};

// ---------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------

/// Everything the trigger needs to know, gathered so the decision itself stays
/// a pure function.
pub struct GuardInput<'a> {
    pub pr: &'a Pr,
    pub automation: Option<&'a PrAutomation>,
    /// Your login, for the authorship guard.
    pub viewer: &'a str,
}

/* **There is no cap on concurrent runs, and 2 was the wrong number for it.**
`MAX_AUTOMATION` came from two worlds that no longer exist: automation that
fired on its own (§8's transition rules, never implemented — every run is
hand-triggered, so passing the cap meant pressing the button three times while
watching), and the capability subsystem that tracked shared resources, which was
deleted. At 2 it was neither serialization nor resource protection: `headroom`
does the latter at spawn with real numbers, the press hand-off and the
one-run-per-PR rule stop the collisions that matter, and the only thing left was a
runaway API caller — on a route that needs the app token, which agents
deliberately do not hold. */

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Go,
    /// Refused, with the reason shown verbatim. `fix-pr` is triggered by hand,
    /// so a refusal is something you read, not something swallowed.
    No {
        reason: String,
    },
}

/// Decide whether a hand-triggered `fix-pr` may start.
///
/// Nothing here fires on its own: the user asked for manual-trigger only, so
/// the transition rules from §8 ("fires immediately on checks failing") are
/// deliberately not implemented. Every other guard in that table still applies,
/// because they protect the machine and the repo rather than the schedule.
pub fn evaluate(input: &GuardInput) -> Verdict {
    let pr = input.pr;

    // Authorship: you must be able to push to the head repo (§8). A `fix-pr` run
    // rebases and force-pushes, so what matters is write access — never whose name
    // is on the repo. `Pr::push_refusal` is the rule and says why it is this one;
    // the review spawns ask it too, and the skills read its answer.
    if let Some(why) = pr.push_refusal(input.viewer) {
        return no(why);
    }

    // One live run per PR, keyed on the number.
    if let Some(PrAutomation::Running { session, .. }) = input.automation {
        return no(format!(
            "#{} already has a fix-pr session running ({})",
            pr.number,
            crate::model::short_id(session)
        ));
    }

    Verdict::Go
}

fn no(reason: String) -> Verdict {
    Verdict::No { reason }
}

/// Start a run: the guard table, the spawn, and the record, in that order.
///
/// One function because the guard table is the whole of what makes starting one
/// safe, and a second call site carrying its own copy of the checks is how one of
/// them ends up a version behind, quietly. The only caller is the rail's `fix`
/// button: a review used to hand its CI on here when it exited, and went with the
/// overlay.
///
/// The refusal comes back as the error, verbatim: `fix-pr` refusals are written to
/// be read by whoever asked.
pub async fn start(
    app: &std::sync::Arc<crate::state::AppState>,
    number: u64,
) -> anyhow::Result<SessionId> {
    /* **Claimed before anything is read, because the guard cannot close this
    window on its own.** `Running` is only recorded *after*
    `spawn_fix_pr_session` returns, and that is hundreds of milliseconds of git —
    so two POSTs arriving together both saw no run, both passed the table, and
    both spawned. Two runs then rebase and force-push the same branch, and
    `settle` (keyed on the PR) clears the record when the first of them exits.

    `branch_busy` could not cover it either: `live_sessions_in` filters on
    `pid_alive`, and `insert_and_spawn` records with `pid: None` before the pty
    exists, so the first run is invisible to it for exactly the window that
    matters. Held until this function returns, by which point `Running` is
    written and the ordinary guard takes over. */
    let _claim = app.try_claim(format!("fix-pr:{number}")).ok_or_else(|| {
        anyhow::anyhow!("a fix run for PR #{number} is already starting; wait for it")
    })?;

    let pr = {
        let inner = app.inner.read().await;
        inner.pr(number).cloned()
    }
    .ok_or_else(|| anyhow::anyhow!("PR #{number} is not in the current poll"))?;

    // Asked the way the spawn enforces it, not with a second rule: an idle session
    // on the branch used to pass here and be refused a moment later by
    // `spawn_fix_pr_session`, once the run looked started.
    //
    // The worktree is not cut before this. `spawn_fix_pr_session` calls
    // `ensure_pr_worktree` itself, so creating one here only meant a refused run
    // left a worktree behind — and it is what split the two rules apart, since the
    // guard then asked about the empty tree it had just made.
    let branch_busy = crate::spawn::branch_busy(app, number, &pr.head_ref)
        .await
        .is_some();

    let verdict = {
        let inner = app.inner.read().await;
        evaluate(&GuardInput {
            pr: &pr,
            automation: inner.automation.get(number),
            // Your own login, from the poll. The authorship guard no longer
            // compares it to the head repo's owner — it asks whether you can push
            // there — but an empty one still fails the guard closed, so this must
            // stay the real viewer rather than anything derived from the PR.
            viewer: inner.viewer.as_deref().unwrap_or_default(),
        })
    };

    if let Verdict::No { reason } = verdict {
        anyhow::bail!("{reason}");
    }

    /* **A session already on the branch is asked, not refused.** The run used to
    bail here ("fix-pr would fight it"), so the row hid `fix` whenever a session
    was open, and the one PR you were working on was the one you could not press
    it for. You pressing the button is the answer the old comment said the
    daemon could not give on its own: that session is to fix the build. It is
    typed the skill a fresh run gets, which works in any session because each
    variable it reads has a fallback, and no automation record is written: no
    unattended run started, the session you were in did. */
    // Every rule above still answers first, so a PR you cannot push to is
    // refused before any session is asked to try.
    if branch_busy {
        if let Some(id) = crate::triage::hand_to_live_session(
            app,
            number,
            &pr.head_ref,
            crate::model::Pass::FIX_PR,
        )
        .await?
        {
            app.notify().await;
            return Ok(id);
        }
    }

    /* **The tree is gated before anything touches it**, like the review flows. A
    fix run rebases, amends and force-pushes, so a worktree that already holds
    the branch with somebody's work in it was handed to a run that rewrites it.
    This never gated at all. After the press hand-off above, which
    changes no tree: the session there is yours, and its own skill stops on a
    dirty tree. */
    if let Some(g) = crate::triage::tree_gate(app, &pr.head_ref).await? {
        anyhow::bail!("{}", g.say());
    }

    // Recorded *before* the process can exist, under the id it will spawn with:
    // `spawn::spawn_run` says why. A `claude` that dies at once is reaped before a
    // write placed after the spawn, and the exit watcher then finds no run of its
    // own to close — so the record named a corpse as running until a restart,
    // and `evaluate` refused every press in between.
    //
    // If this never reaches disk, a restart forgets the run started: auto-resume
    // brings the run back with nothing guarding it, and the one-run-per-PR rule
    // can start a second beside it. So the write is not optional.
    let session = uuid::Uuid::new_v4();
    app.inner
        .write()
        .await
        .with_automation(&format!("PR #{number} run started"), |a| {
            a.by_pr.insert(
                number,
                PrAutomation::Running {
                    session,
                    started: SystemTime::now(),
                },
            );
            true
        });
    if let Err(e) = crate::spawn::spawn_fix_pr_session(app, number, &pr.head_ref, session).await {
        // Nothing started, so nothing may be left claiming to run. Straight back
        // out rather than through `settle`, which would read the PR's checks and
        // write down a verdict for a run that never looked at them.
        app.inner
            .write()
            .await
            .with_automation(&format!("PR #{number} never started"), |a| {
                a.by_pr.remove(&number).is_some()
            });
        return Err(e);
    }

    // The record is cleared by the session's own exit watcher, which is the one
    // place that sees a pty end (`settle`).
    app.notify().await;
    Ok(session)
}

/// Forget a finished run, so the PR may be run on again.
///
/// Called by the session's own exit watcher, which is the *one* observer of a pty
/// ending — for a resumed run too, which takes its record back on the way up
/// (`AutomationStore::take_back`), so its exit settles here like the first one's.
/// A run that ended red is not remembered: nothing read that record, and the rail's
/// `fix` button and the PR's own checks say what there is to say. This used to be a second `pty.wait()` on the same handle from inside
/// the HTTP layer: both saw the same real event, so it worked, but "is this run
/// over" had two answers maintained independently and the automation record lived
/// nowhere in particular.
pub async fn settle(app: &std::sync::Arc<crate::state::AppState>, pr: u64, session: SessionId) {
    let mut inner = app.inner.write().await;
    settle_in(&mut inner, pr, session);
}

/// [`settle`] under a lock the caller already holds.
///
/// **Only this session's run.** The record is keyed on the PR, and a second run
/// on the same PR replaces it: clearing it on the exit of the one it replaced
/// would leave the live run unguarded.
fn settle_in(inner: &mut crate::state::Inner, pr: u64, session: SessionId) {
    // The write rides with the mutation: a lost one leaves a finished run looking
    // live after a restart, refusing every press until boot prunes it.
    inner.with_automation(&format!("PR #{pr} at run end"), |a| a.let_go(pr, session));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::Checks;

    /// A PR a run would legitimately be started for: red, and yours to push to.
    fn pr(number: u64) -> Pr {
        Pr {
            head_repo: Some("me/monorepo".into()),
            head_pushable: Some(true),
            checks: Checks::Failing,
            head_sha: Some("abc".into()),
            ..crate::testutil::pr(number)
        }
    }

    fn input(p: &Pr) -> GuardInput<'_> {
        GuardInput {
            pr: p,
            automation: None,
            viewer: "kbarendrecht",
        }
    }

    #[test]
    fn a_clean_pr_may_run() {
        let p = pr(1);
        assert_eq!(evaluate(&input(&p)), Verdict::Go);
    }

    #[test]
    fn a_second_run_for_the_same_pr_is_refused() {
        let p = pr(1);
        let a = PrAutomation::Running {
            session: uuid::Uuid::new_v4(),
            started: SystemTime::now(),
        };
        let mut i = input(&p);
        i.automation = Some(&a);
        assert!(matches!(evaluate(&i), Verdict::No { .. }));
    }

    #[test]
    fn the_guard_asks_whether_you_can_push_not_who_owns_it() {
        // A repo that is not yours, and you cannot push to it.
        let mut p = pr(1);
        p.head_repo = Some("someone-else/monorepo".into());
        p.head_pushable = Some(false);
        match evaluate(&input(&p)) {
            Verdict::No { reason } => assert!(reason.contains("someone-else/monorepo"), "{reason}"),
            other => panic!("expected No, got {other:?}"),
        }

        // An org's repo you *can* push to. The old guard refused this one.
        p.head_repo = Some("acme/monorepo".into());
        p.head_pushable = Some(true);
        assert!(matches!(evaluate(&input(&p)), Verdict::Go));

        // GitHub did not say. Fails closed rather than guessing about a force-push.
        p.head_pushable = None;
        match evaluate(&input(&p)) {
            Verdict::No { reason } => assert!(reason.contains("did not say"), "{reason}"),
            other => panic!("expected No, got {other:?}"),
        }
    }

    /// Fails closed: an unknown login must not authorise a force-pushing run.
    #[test]
    fn an_unknown_viewer_is_refused() {
        let p = pr(1);
        let mut i = input(&p);
        i.viewer = "";
        match evaluate(&i) {
            Verdict::No { reason } => assert!(reason.contains("login is unknown"), "{reason}"),
            other => panic!("expected No, got {other:?}"),
        }
    }

    #[test]
    fn a_draft_is_treated_as_a_normal_pr() {
        // Single-run-per-PR carries the load instead (§8).
        let mut p = pr(1);
        p.is_draft = true;
        assert!(matches!(evaluate(&input(&p)), Verdict::Go));
    }

    /// A finished run is forgotten however it ended, and only that run: red used
    /// to leave an `Exhausted` record nothing read. Through `AutomationStore`
    /// rather than `settle`, which persists — and a unit test has no business
    /// writing the real `automation.json`.
    #[test]
    fn a_finished_run_is_forgotten_and_a_newer_one_is_not() {
        let (first, second) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let mut store = crate::model::AutomationStore::default();
        let _ = store.take_back(7, second);
        assert!(!store.let_go(7, first), "the replaced run's exit");
        assert!(store.get(7).is_some(), "left the live run unguarded");
        assert!(store.let_go(7, second));
        assert!(store.get(7).is_none());
    }
}
