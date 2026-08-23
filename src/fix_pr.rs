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
        at_head: String,
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
    pub fn reconcile_head(&mut self, pr: u64, head: Option<&str>) {
        let Some(head) = head else { return };
        if let Some(PrAutomation::Exhausted { at_head, .. }) = self.by_pr.get(&pr) {
            if at_head != head {
                self.by_pr.remove(&pr);
            }
        }
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
    /// A live session on this PR's branch that is not merely idle.
    pub branch_busy: bool,
    pub running_automations: usize,
}

/// Cap on concurrent automation runs (§8).
pub const MAX_AUTOMATION: usize = 2;

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

    // Authorship: the head repo must be your fork (§8).
    if let Some(owner) = &pr.head_owner {
        if owner != input.viewer {
            return no(format!(
                "#{} is headed from {owner}, not your fork",
                pr.number
            ));
        }
    }

    // One live run per PR, keyed on the number.
    if let Some(PrAutomation::Running { session, .. }) = input.automation {
        return no(format!(
            "#{} already has a fix-pr session running ({})",
            pr.number,
            &session.to_string()[..8]
        ));
    }

    // Never remediate a PR whose branch is checked out in a workspace holding a
    // live, non-idle session — you are working on it (§8).
    if input.branch_busy {
        return no(format!(
            "you have a live session on {}; fix-pr would fight it",
            pr.head_ref
        ));
    }

    if input.running_automations >= MAX_AUTOMATION {
        return no(format!(
            "{} automation runs already going (cap {MAX_AUTOMATION})",
            input.running_automations
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

/// The `Kind::Automation` command a fix run carries.
///
/// Named rather than spelled at the spawn site and again at the place that reacts
/// to the exit: those two have to agree, and a literal in both is how they stop
/// agreeing without anything failing.
pub const COMMAND: &str = "fix-pr";

/// Write down how a finished run left the PR.
///
/// Called by the session's own exit watcher, which is the *one* observer of a pty
/// ending. This used to be a second `pty.wait()` on the same handle from inside
/// the HTTP layer: both saw the same real event, so it worked, but "is this run
/// over" had two answers maintained independently and the automation record lived
/// nowhere in particular.
pub async fn settle(app: &std::sync::Arc<crate::state::AppState>, pr: u64) {
    let mut inner = app.inner.write().await;
    let found = inner.prs.iter().find(|p| p.number == pr).cloned();
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
        Some(p) if ended_red(p) => Some(PrAutomation::Exhausted {
            at_head: p.head_sha.clone().unwrap_or_default(),
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
            head_owner: Some("kbarendrecht".into()),
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
            running_automations: 0,
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

    #[test]
    fn automation_cap_refuses_rather_than_queue() {
        let p = pr(1);
        let mut i = input(&p);
        i.running_automations = MAX_AUTOMATION;
        assert!(matches!(evaluate(&i), Verdict::No { .. }));
    }

    #[test]
    fn someone_elses_head_repo_is_refused() {
        let mut p = pr(1);
        p.head_owner = Some("someone-else".into());
        assert!(matches!(evaluate(&input(&p)), Verdict::No { .. }));
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
                at_head: "abc".into(),
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
                at_head: "abc".into(),
                at: SystemTime::now(),
            },
        );
        store.reconcile_head(7, None);
        assert!(store.get(7).is_some());
    }

    /// The verdict a finished run leaves behind. Moved here out of a second
    /// `pty.wait()` in the HTTP layer, so it is worth pinning where it now lives.
    /// Tested through `verdict` rather than `settle`: `settle` persists, and a unit
    /// test has no business writing the real `automation.json`.
    #[test]
    fn a_run_that_ends_red_is_remembered_as_exhausted() {
        let p = pr(7); // `pr()` is red: checks Failing
        match verdict(Some(&p)) {
            Some(PrAutomation::Exhausted { at_head, .. }) => assert_eq!(at_head, "abc"),
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
