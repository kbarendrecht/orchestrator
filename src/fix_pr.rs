use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;
use uuid::Uuid;

use crate::forge::{Checks, Pr};

pub type SessionId = Uuid;

/// Per-PR automation state (§8).
///
/// Retry lives in the prompt, not here: `fix-pr` amends and rebases, so the head
/// SHA changes on every internal attempt and SHA-based provenance is impossible
/// *and* unnecessary. The daemon's job is only to avoid starting a second run
/// and to be honest about a run that gave up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(export, export_to = "../web/snapshot.d.ts"))]
pub enum PrAutomation {
    Running {
        session: SessionId,
        #[cfg_attr(test, ts(type = "{ secs_since_epoch: number, nanos_since_epoch: number }"))]
        started: SystemTime,
    },
    /// The run stopped without turning the PR green. It wants you.
    Exhausted {
        /// The head this exhaustion is measured against, once anything knows it.
        ///
        /// `None` means "not established yet", and the next poll adopts whatever
        /// it finds. Two things wrote a wrong answer here before, and both
        /// cleared the record on the very next poll — erasing the "gave up, wants
        /// you" signal the run had just set. `settle` copied the head from the
        /// *previous* poll, which the run's own force-push had already moved past;
        /// and a crashed run was demoted with `""`, which can never equal a real
        /// sha. Neither could be told apart from you moving the branch.
        #[serde(default)]
        at_head: Option<String>,
        #[cfg_attr(test, ts(type = "{ secs_since_epoch: number, nanos_since_epoch: number }"))]
        at: SystemTime,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutomationStore {
    #[serde(default)]
    pub by_pr: HashMap<u64, PrAutomation>,
}

impl AutomationStore {
    pub fn get(&self, pr: u64) -> Option<&PrAutomation> {
        self.by_pr.get(&pr)
    }

    /// Exhaustion clears when the head moves while no run is alive: nothing of
    /// the daemon's was running and the branch changed, therefore you did it.
    /// No commit markers, no timestamps, no provenance (§8).
    ///
    /// A record with no head yet **adopts** this one rather than clearing. That is
    /// what makes the rule mean what it says: the daemon cannot know the head a
    /// run left — the run's last act is a force-push, after the poll that could
    /// have seen it — so the first poll afterwards establishes the baseline and
    /// only a move *after* that is yours. The cost is one poll interval of grace:
    /// a push of yours landing in that window is absorbed into the baseline
    /// instead of clearing the record. Worth it, since the bug it replaces
    /// cleared the record every single time.
    ///
    /// Returns whether anything changed, so the caller can carry the write.
    pub fn reconcile_head(&mut self, pr: u64, head: Option<&str>) -> bool {
        let Some(head) = head else { return false };
        // Decided before acting, because clearing takes the map mutably while the
        // record it is deciding about is still borrowed.
        let moved = match self.by_pr.get(&pr) {
            Some(PrAutomation::Exhausted { at_head: Some(h), .. }) => h != head,
            Some(PrAutomation::Exhausted { at_head: None, .. }) => false,
            _ => return false,
        };
        if moved {
            self.by_pr.remove(&pr);
        } else if let Some(PrAutomation::Exhausted { at_head, .. }) = self.by_pr.get_mut(&pr) {
            if at_head.is_some() {
                return false;
            }
            *at_head = Some(head.to_string());
        }
        true
    }
}

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
    /// Whether a worktree holding this PR's branch has a live session in it.
    /// `spawn::branch_busy` is the one definition; an idle session counts, because
    /// the spawn refuses one too.
    pub branch_busy: bool,
}

/* **There is no cap on concurrent runs, and 2 was the wrong number for it.**
   `MAX_AUTOMATION` came from two worlds that no longer exist: automation that
   fired on its own (§8's transition rules, never implemented — every run is
   hand-triggered, so passing the cap meant pressing the button three times while
   watching), and the capability subsystem that tracked shared resources, which was
   deleted. At 2 it was neither serialization nor resource protection: `headroom`
   does the latter at spawn with real numbers, `branch_busy` and the one-run-per-PR
   rule above stop the collisions that matter, and the only thing left was a
   runaway API caller — on a route that needs the app token, which agents
   deliberately do not hold. */

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Go,
    /// Refused, with the reason shown verbatim. `fix-pr` is triggered by hand,
    /// so a refusal is something you read, not something swallowed.
    No { reason: String },
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
    // is on the repo.
    //
    // This was "the head repo's owner is your login", which was wrong in both
    // directions. On an org PR the owner is the org, so it refused every PR and
    // `fix-pr` could not run outside a fork layout at all; and a same-repo PR
    // passed it only because you happened to own the repo, having established
    // nothing about a force-push. Scoping the guard to forks instead was the other
    // option and is worse: it would skip the check on exactly the PRs whose branch
    // lives in a shared repo.
    //
    // Both unknowns fail closed. A run that force-pushes must never guess.
    if input.viewer.is_empty() {
        return no(format!(
            "#{} cannot be remediated: your GitHub login is unknown",
            pr.number
        ));
    }
    let where_ = pr.head_repo.as_deref().unwrap_or("an unknown repo");
    match pr.head_pushable {
        Some(true) => {}
        Some(false) => {
            return no(format!(
                "#{} is headed from {where_}, which you cannot push to",
                pr.number
            ))
        }
        None => {
            return no(format!(
                "#{} cannot be remediated: GitHub did not say whether you can push to {where_}",
                pr.number
            ))
        }
    }

    // One live run per PR, keyed on the number.
    if let Some(PrAutomation::Running { session, .. }) = input.automation {
        return no(format!(
            "#{} already has a fix-pr session running ({})",
            pr.number,
            crate::model::short_id(session)
        ));
    }

    // Never remediate a PR whose branch is checked out in a workspace holding a
    // live session — you are working on it (§8). Idle counts: the session is open,
    // and a rebase under it is the thing this refuses.
    if input.branch_busy {
        return no(format!(
            "you have a live session on {}; fix-pr would fight it",
            pr.head_ref
        ));
    }

    Verdict::Go
}

fn no(reason: String) -> Verdict {
    Verdict::No { reason }
}

/// Whether a finished run left the PR in a state that counts as exhausted.
///
/// A run that ends with the PR still red means the run is asking for you (§8).
pub fn ended_red(pr: &Pr) -> bool {
    pr.checks == Checks::Failing || pr.mergeable == "CONFLICTING"
}

/// Whether a PR still has something a fix run would watch.
///
/// Asked at the end of a review, which has just force-pushed: the checks are
/// running against a head that did not exist a minute ago, so `Pending` counts —
/// that *is* the watching. Green and mergeable means the review is simply over and
/// there is nothing to hand on.
///
/// Deliberately wider than [`ended_red`] by exactly one case, and phrased as such so
/// the two cannot drift on what counts as red.
///
/// `Unknown` does not count. It is the answer when the poll could not say, and a run
/// rebases and force-pushes: the one thing this must not do is start one because it
/// could not tell.
pub fn wants_watching(pr: &Pr) -> bool {
    ended_red(pr) || pr.checks == Checks::Pending
}

/// The `Kind::Automation` command a fix run carries.
///
/// Named rather than spelled at the spawn site and again at the place that reacts
/// to the exit: those two have to agree, and a literal in both is how they stop
/// agreeing without anything failing.
pub const COMMAND: &str = "fix-pr";

/// Start a run: the guard table, the spawn, and the record, in that order.
///
/// One function because there are two callers now — the rail's button and a review
/// handing on what it is not allowed to finish — and the guard table is the whole
/// of what makes starting one safe. A second call site carrying its own copy of the
/// checks is how one of them ends up a version behind, quietly.
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
    let _claim = app
        .try_claim(format!("fix-pr:{number}"))
        .await
        .ok_or_else(|| {
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
    let branch_busy = crate::spawn::branch_busy(app, &pr.head_ref).await.is_some();

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
            branch_busy,
        })
    };

    if let Verdict::No { reason } = verdict {
        anyhow::bail!("{reason}");
    }

    let session = crate::spawn::spawn_fix_pr_session(app, number, &pr.head_ref).await?;
    {
        let mut inner = app.inner.write().await;
        // If this never reaches disk, a restart forgets the run started and the
        // one-run-per-PR cap can re-fire, so the write is not optional.
        inner.with_automation(&format!("PR #{number} run started"), |a| {
            a.by_pr.insert(
                number,
                PrAutomation::Running {
                    session,
                    started: SystemTime::now(),
                },
            );
            true
        });
    }

    // Exhaustion is recorded by the session's own exit watcher, which is the
    // one place that sees a pty end (`settle`).
    app.notify().await;
    Ok(session)
}

/// Write down how a finished run left the PR.
///
/// Called by the session's own exit watcher, which is the *one* observer of a pty
/// ending. This used to be a second `pty.wait()` on the same handle from inside
/// the HTTP layer: both saw the same real event, so it worked, but "is this run
/// over" had two answers maintained independently and the automation record lived
/// nowhere in particular.
pub async fn settle(app: &std::sync::Arc<crate::state::AppState>, pr: u64) {
    let mut inner = app.inner.write().await;
    let found = inner.pr(pr).cloned();
    let v = verdict(found.as_ref());
    // The write rides with the mutation: a lost one makes a restart mis-remember
    // whether this PR is exhausted, which defeats the one-run-per-PR cap.
    inner.with_automation(&format!("PR #{pr} at run end"), |a| {
        match v {
            Some(v) => {
                a.by_pr.insert(pr, v);
            }
            None => {
                a.by_pr.remove(&pr);
            }
        }
        true
    });
}

/// What a finished run means for the record: `Some` to remember, `None` to forget.
///
/// Split from [`settle`] so the decision can be tested without an `AppState` — and
/// without a test writing `automation.json` into the real config directory, which
/// is what `settle` does by design and what a unit test must never do.
///
/// `None` covers two different clean endings: the PR went green, or it left the
/// poll entirely (merged, closed, no longer yours). Neither is evidence a run gave
/// up, so neither is worth remembering.
fn verdict(found: Option<&Pr>) -> Option<PrAutomation> {
    match found {
        // No head: the poll that produced `p` ran *before* the run's last
        // force-push, so `p.head_sha` is a sha this branch has already left. The
        // next poll establishes the real one — see `reconcile_head`.
        Some(p) if ended_red(p) => Some(PrAutomation::Exhausted {
            at_head: None,
            at: SystemTime::now(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64) -> Pr {
        Pr {
            number,
            title: "t".into(),
            url: String::new(),
            head_ref: "feature/x".into(),
            head_repo: Some("me/monorepo".into()),
            head_pushable: Some(true),
            base_ref: "develop".into(),
            is_draft: false,
            mergeable: "MERGEABLE".into(),
            merge_state: "CLEAN".into(),
            checks: Checks::Failing,
            head_sha: Some("abc".into()),
            unresolved: 0,
            unresolved_capped: false,
            awaiting_you: 0,
            changes_requested: false,
            needs_you: false,
            children: vec![],
        }
    }

    fn input(p: &Pr) -> GuardInput<'_> {
        GuardInput {
            pr: p,
            automation: None,
            viewer: "kbarendrecht",
            branch_busy: false,
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
            session: Uuid::new_v4(),
            started: SystemTime::now(),
        };
        let mut i = input(&p);
        i.automation = Some(&a);
        assert!(matches!(evaluate(&i), Verdict::No { .. }));
    }

    #[test]
    fn a_branch_you_are_working_on_is_left_alone() {
        let p = pr(1);
        let mut i = input(&p);
        i.branch_busy = true;
        match evaluate(&i) {
            Verdict::No { reason } => assert!(reason.contains("would fight it"), "{reason}"),
            other => panic!("expected No, got {other:?}"),
        }
    }

    /// What a finishing review asks before handing anything on.
    ///
    /// `Pending` is the case worth stating: a review's last act in phase 2 is a
    /// force-push, so the checks it is asking about were queued seconds ago and are
    /// almost never anything else. Reading pending as "fine" would make the hand-off
    /// fire approximately never, which is a feature that does nothing rather than one
    /// that fails.
    #[test]
    fn a_review_hands_on_a_pr_with_something_left_to_watch() {
        let mut p = pr(1);
        for (checks, mergeable, want) in [
            (Checks::Pending, "MERGEABLE", true),
            (Checks::Failing, "MERGEABLE", true),
            (Checks::Passing, "CONFLICTING", true),
            (Checks::Passing, "MERGEABLE", false),
            // Not "probably fine" and not "probably red": it is the poll saying it
            // could not tell, and a run rebases and force-pushes.
            (Checks::Unknown, "MERGEABLE", false),
        ] {
            p.checks = checks.clone();
            p.mergeable = mergeable.into();
            assert_eq!(wants_watching(&p), want, "{checks:?} / {mergeable}");
        }
    }

    /// The guard asks about push access, not about whose name is on the repo.
    ///
    /// Both readings agreed on a fork you own and disagreed everywhere else: the
    /// old owner-versus-login test refused an org PR you have write on (so `fix-pr`
    /// could not run outside a fork layout at all) and passed a same-repo PR merely
    /// because you owned the repo. Each direction is asserted, because getting only
    /// one of them right is what shipped.
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
        // Single-run-per-PR and exhaustion carry the load instead (§8).
        let mut p = pr(1);
        p.is_draft = true;
        assert!(matches!(evaluate(&input(&p)), Verdict::Go));
    }

    #[test]
    fn exhaustion_clears_only_when_the_head_moves() {
        let mut store = AutomationStore::default();
        store.by_pr.insert(
            7,
            PrAutomation::Exhausted {
                at_head: Some("abc".into()),
                at: SystemTime::now(),
            },
        );
        store.reconcile_head(7, Some("abc"));
        assert!(store.get(7).is_some(), "same head must stay exhausted");
        store.reconcile_head(7, Some("def"));
        assert!(store.get(7).is_none(), "a moved head means you touched it");
    }

    #[test]
    fn an_unknown_head_does_not_clear_exhaustion() {
        let mut store = AutomationStore::default();
        store.by_pr.insert(
            7,
            PrAutomation::Exhausted {
                at_head: Some("abc".into()),
                at: SystemTime::now(),
            },
        );
        store.reconcile_head(7, None);
        assert!(store.get(7).is_some());
    }

    /// A record with no head yet is the *normal* end of a run, and the next poll
    /// gives it one instead of throwing it away.
    ///
    /// This is the whole of the bug: the run's last act is a force-push, so the
    /// head every already-taken poll knows is one the branch has left. Recording
    /// that stale sha — or the `""` a crashed run used to be demoted with — made
    /// the next poll read "the head moved, therefore you moved it" and erase the
    /// only signal saying the run gave up.
    #[test]
    fn a_run_that_left_an_unknown_head_adopts_the_next_poll_rather_than_clearing() {
        let mut store = AutomationStore::default();
        store.by_pr.insert(
            7,
            PrAutomation::Exhausted { at_head: None, at: SystemTime::now() },
        );
        assert!(store.reconcile_head(7, Some("post-push")), "the baseline is news");
        assert!(store.get(7).is_some(), "adopting must not clear the record");
        // Adopted, so it is now the thing a later move is judged against — and a
        // second poll at the same head changes nothing and writes nothing.
        assert!(!store.reconcile_head(7, Some("post-push")));
        assert!(store.get(7).is_some());
        assert!(store.reconcile_head(7, Some("yours")), "now it really moved");
        assert!(store.get(7).is_none());
    }

    /// The verdict a finished run leaves behind. Moved here out of a second
    /// `pty.wait()` in the HTTP layer, so it is worth pinning where it now lives.
    /// Tested through `verdict` rather than `settle`: `settle` persists, and a unit
    /// test has no business writing the real `automation.json`.
    #[test]
    fn a_run_that_ends_red_is_remembered_as_exhausted() {
        let p = pr(7); // `pr()` is red: checks Failing
        match verdict(Some(&p)) {
            // Deliberately *not* `p.head_sha`: that is the pre-push poll's answer.
            Some(PrAutomation::Exhausted { at_head, .. }) => assert_eq!(at_head, None),
            other => panic!("red at the end must be remembered, got {other:?}"),
        }
    }

    #[test]
    fn a_conflicting_pr_counts_as_exhausted_too() {
        let mut p = pr(7);
        p.checks = Checks::Passing;
        p.mergeable = "CONFLICTING".into();
        assert!(verdict(Some(&p)).is_some(), "green checks but unmergeable is still stuck");
    }

    #[test]
    fn a_run_that_ends_green_is_forgotten() {
        let mut p = pr(7);
        p.checks = Checks::Passing;
        assert_eq!(verdict(Some(&p)), None, "green leaves nothing to remember");
    }

    /// A PR that fell out of the poll while the run was going is not evidence of
    /// exhaustion — merged, closed, or simply not ours any more.
    #[test]
    fn a_pr_gone_from_the_poll_is_forgotten_too() {
        assert_eq!(verdict(None), None);
    }
}
