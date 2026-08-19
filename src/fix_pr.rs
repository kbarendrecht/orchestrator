use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;
use uuid::Uuid;

use crate::capability::{CapabilityReport, Isolation, Trust};
use crate::github::{Checks, Pr};

pub type SessionId = Uuid;

/// Per-PR automation state (§8).
///
/// Retry lives in the prompt, not here: `fix-pr` amends and rebases, so the head
/// SHA changes on every internal attempt and SHA-based provenance is impossible
/// *and* unnecessary. The daemon's job is only to avoid starting a second run
/// and to be honest about a run that gave up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PrAutomation {
    Running {
        session: SessionId,
        started: SystemTime,
    },
    /// The run stopped without turning the PR green. It wants you.
    Exhausted {
        at_head: String,
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
    pub capability: &'a CapabilityReport,
    /// Your login, for the authorship guard.
    pub viewer: &'a str,
    /// A live session on this PR's branch that is not merely idle.
    pub branch_busy: bool,
    pub running_automations: usize,
    /// Which session holds main, if any.
    /// Shared resources currently held.
    pub locks_held: &'a [String],
}

/// Cap on concurrent automation runs (§8).
pub const MAX_AUTOMATION: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Go {
        /// Shared resources to take before starting.
        locks: Vec<String>,
    },
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

    // §7 rule 1, plus rule 3: a Stale result is one fix-pr must not act on
    // either, because it was frozen at copy time.
    for c in &input.capability.capabilities {
        if !c.runnable {
            return no(format!("{:?} has no command configured", c.suite));
        }
        match c.trust {
            Trust::Untrusted => {
                return no(match &c.note {
                    Some(n) => format!("{:?} needs main — {n}", c.suite),
                    None => format!("{:?} is untrusted here — this PR needs main", c.suite),
                });
            }
            Trust::Stale => {
                // The note already says what to do; repeating it just makes the
                // refusal harder to read.
                return no(match &c.note {
                    Some(n) => format!("{:?}: {n}", c.suite),
                    None => format!("{:?} is stale — re-link from main first", c.suite),
                });
            }
            Trust::Verified => {}
        }
    }

    if input.running_automations >= MAX_AUTOMATION {
        return no(format!(
            "{} automation runs already going (cap {MAX_AUTOMATION})",
            input.running_automations
        ));
    }
    // Shared resources (§7 rule 2). The conflict is between *runs*: two of them
    // taking `main:instances` would fight over one instances dir.
    //
    // A session merely occupying main used to refuse this too, on the grounds
    // that e2e teardown reaches into the main checkout. It costs more than it
    // buys: the run happens in the PR's own worktree, a session in main is
    // normally just editing code, and the rule turned "somebody has main open"
    // into "no fix run may start anywhere".
    let mut locks = Vec::new();
    for c in &input.capability.capabilities {
        if let Isolation::SharedResource { resource } = &c.isolation {
            if input.locks_held.iter().any(|h| h == resource) {
                return no(format!("{resource} is already held by another run"));
            }
            if !locks.contains(resource) {
                locks.push(resource.clone());
            }
        }
    }

    Verdict::Go { locks }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Capability, Suite};

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

    fn cap(trust: Trust, iso: Isolation) -> CapabilityReport {
        CapabilityReport {
            workspace: "x".into(),
            is_main: false,
            capabilities: vec![Capability {
                suite: Suite::Unit,
                runnable: true,
                trust,
                isolation: iso,
                command: vec!["true".into()],
                note: None,
            }],
            deps: vec![],
            autoload: crate::capability::AutoloadProbe::Skipped {
                reason: "test".into(),
            },
            container_path: None,
            fix_pr_eligible: true,
            fix_pr_blockers: vec![],
            locks_required: vec![],
        }
    }

    fn input<'a>(p: &'a Pr, c: &'a CapabilityReport) -> GuardInput<'a> {
        GuardInput {
            pr: p,
            automation: None,
            capability: c,
            viewer: "kbarendrecht",
            branch_busy: false,
            running_automations: 0,
            locks_held: &[],
        }
    }

    #[test]
    fn a_clean_verified_pr_may_run() {
        let p = pr(1);
        let c = cap(Trust::Verified, Isolation::Isolated);
        assert_eq!(evaluate(&input(&p, &c)), Verdict::Go { locks: vec![] });
    }

    #[test]
    fn an_untrusted_suite_refuses_and_says_it_needs_main() {
        let p = pr(1);
        let c = cap(Trust::Untrusted, Isolation::Isolated);
        match evaluate(&input(&p, &c)) {
            Verdict::No { reason } => assert!(reason.contains("needs main"), "{reason}"),
            other => panic!("expected No, got {other:?}"),
        }
    }

    #[test]
    fn a_stale_suite_refuses_too() {
        // §7 rule 3: a result frozen at copy time is not one to act on.
        let p = pr(1);
        let c = cap(Trust::Stale, Isolation::Isolated);
        match evaluate(&input(&p, &c)) {
            Verdict::No { reason } => assert!(reason.contains("stale"), "{reason}"),
            other => panic!("expected No, got {other:?}"),
        }
    }

    #[test]
    fn a_second_run_for_the_same_pr_is_refused() {
        let p = pr(1);
        let c = cap(Trust::Verified, Isolation::Isolated);
        let a = PrAutomation::Running {
            session: Uuid::new_v4(),
            started: SystemTime::now(),
        };
        let mut i = input(&p, &c);
        i.automation = Some(&a);
        assert!(matches!(evaluate(&i), Verdict::No { .. }));
    }

    #[test]
    fn a_branch_you_are_working_on_is_left_alone() {
        let p = pr(1);
        let c = cap(Trust::Verified, Isolation::Isolated);
        let mut i = input(&p, &c);
        i.branch_busy = true;
        match evaluate(&i) {
            Verdict::No { reason } => assert!(reason.contains("would fight it"), "{reason}"),
            other => panic!("expected No, got {other:?}"),
        }
    }

    #[test]
    fn a_shared_resource_is_taken_as_a_lock_when_main_is_free() {
        let p = pr(1);
        let c = cap(
            Trust::Verified,
            Isolation::SharedResource {
                resource: "main:instances".into(),
            },
        );
        assert_eq!(
            evaluate(&input(&p, &c)),
            Verdict::Go {
                locks: vec!["main:instances".into()]
            }
        );
    }

    #[test]
    fn working_in_main_does_not_block_a_run_in_a_worktree() {
        // The run happens in the PR's own worktree. Refusing it because a session
        // has main open turned "somebody is working" into "nothing may start".
        let p = pr(1);
        let c = cap(
            Trust::Verified,
            Isolation::SharedResource {
                resource: "main:instances".into(),
            },
        );
        assert_eq!(
            evaluate(&input(&p, &c)),
            Verdict::Go {
                locks: vec!["main:instances".into()]
            }
        );
    }

    #[test]
    fn a_lock_already_held_is_refused() {
        let p = pr(1);
        let c = cap(
            Trust::Verified,
            Isolation::SharedResource {
                resource: "main:instances".into(),
            },
        );
        let held = vec!["main:instances".to_string()];
        let mut i = input(&p, &c);
        i.locks_held = &held;
        assert!(matches!(evaluate(&i), Verdict::No { .. }));
    }

    #[test]
    fn automation_cap_refuses_rather_than_queue() {
        let p = pr(1);
        let c = cap(Trust::Verified, Isolation::Isolated);

        let mut i = input(&p, &c);
        i.running_automations = MAX_AUTOMATION;
        assert!(matches!(evaluate(&i), Verdict::No { .. }));
    }

    #[test]
    fn someone_elses_head_repo_is_refused() {
        let mut p = pr(1);
        p.head_owner = Some("someone-else".into());
        let c = cap(Trust::Verified, Isolation::Isolated);
        assert!(matches!(evaluate(&input(&p, &c)), Verdict::No { .. }));
    }

    #[test]
    fn a_draft_is_treated_as_a_normal_pr() {
        // Single-run-per-PR and exhaustion carry the load instead (§8).
        let mut p = pr(1);
        p.is_draft = true;
        let c = cap(Trust::Verified, Isolation::Isolated);
        assert!(matches!(evaluate(&input(&p, &c)), Verdict::Go { .. }));
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
}
