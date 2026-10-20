//! The forge-agnostic model.
//!
//! These types are what a [`crate::forge::Forge`] hands back, whatever platform
//! it talks to. The GitHub GraphQL/curl details that build them live in
//! [`super::github`]; a future GitLab impl would build the same shapes from its
//! own API. Keeping the types here rather than in the GitHub module is what lets
//! a second forge exist without depending on the first.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub enum Checks {
    Passing,
    Failing,
    Pending,
    Unknown,
}

/// How a PR ended, for a session row that still points at one.
///
/// **The poll cannot see this, which is the whole reason the type exists.** It
/// asks GitHub for `is:pr is:open author:@me`, so a PR that merges or closes
/// simply stops being in the answer — and a row that showed `#41 open` falls back
/// to the session's own state with nothing ever saying which of the two happened.
/// Merged is the one worth saying: it is the moment the worktree can go.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub enum PrOutcome {
    Merged,
    Closed,
}

/// A PR the poll used to report and no longer does, with what became of it.
///
/// Carries `head_ref` because that is the only thing tying it back to a
/// workspace — the snapshot resolves the same way it does for an open PR, and the
/// branch is what both sides match on.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub struct EndedPr {
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub number: u64,
    pub title: String,
    pub url: String,
    pub head_ref: String,
    pub outcome: PrOutcome,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub struct Pr {
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub number: u64,
    pub title: String,
    pub url: String,
    pub head_ref: String,
    /// The head branch's repo, `owner/name`, for naming it in a refusal.
    pub head_repo: Option<String>,
    /// May you push to that repo?
    ///
    /// This is what the `fix-pr` authorship guard asks, and it replaced "is the
    /// head repo's owner your login". That test was wrong in both directions: on
    /// an org PR the owner is the org, so it refused every PR and made `fix-pr`
    /// unusable outside a fork layout — while a same-repo PR passed it only
    /// because you happened to own the repo, not because it had established
    /// anything about a force-push.
    ///
    /// Repo-level, so it does not promise a given *branch* is force-pushable;
    /// `src/guard.rs` is the other half of that pair (`--force-with-lease` only,
    /// never the base branch). `None` means GitHub did not say, and the guard fails
    /// closed on it.
    pub head_pushable: Option<bool>,
    pub base_ref: String,
    pub is_draft: bool,
    /// `MERGEABLE` / `CONFLICTING` / `UNKNOWN`.
    pub mergeable: String,
    pub merge_state: String,
    pub checks: Checks,
    /// Head commit. `fix-pr` amends and rebases, so this moves on every
    /// internal attempt — it says where the branch is now, never which run put
    /// it there (§8). The review checks a force-push against it.
    pub head_sha: Option<String>,
    /// GitHub's sense of resolved — a conversation the comment author closed.
    /// Not to be confused with `/api/pr/:number/resolve-run`, which is *our* flow for
    /// answering threads and deliberately never closes one.
    pub unresolved: u32,
    /// True when `reviewThreads` had another page, so `unresolved` is a floor.
    /// Rendered as `50+` rather than `50`, so an under-count cannot silently
    /// hide work (§6).
    pub unresolved_capped: bool,
    /// Of those, the ones whose last word is not yours and which you have not
    /// 👍'd. This is the number the rail acts on; `unresolved` is GitHub's.
    pub awaiting_you: u32,
    pub changes_requested: bool,
    /// Whether this PR is waiting on you, decided here rather than in four
    /// places that each got it slightly differently.
    ///
    /// A thread you have answered is not your turn even though GitHub still
    /// calls it unresolved — closing it is the reviewer's button. A
    /// changes-requested review counts only when there are no threads to answer,
    /// which is the shape of an objection written in the review body: with
    /// threads present it stays set until the reviewer looks again, and treating
    /// that as your turn is what made an answered PR sit there amber.
    pub needs_you: bool,
    /// PRs stacked directly on this one.
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "Array<number>"))]
    pub children: Vec<u64>,
}

impl Pr {
    /// Why you may not rewrite this PR's branch, or `None` when you may.
    ///
    /// **The one authorship rule**, asked by the daemon before every run that
    /// rebases, amends or force-pushes — `fix-pr`, the overlay's review and the
    /// `handle-review` pane — and handed to the skill as its answer, so no skill
    /// works it out a second way. What matters is write access to the head repo,
    /// never whose name is on it.
    ///
    /// It was "the head repo's owner is your login", which was wrong in both
    /// directions. On an org PR the owner is the org, so it refused every PR and
    /// `fix-pr` could not run outside a fork layout at all; and a same-repo PR
    /// passed it only because you happened to own the repo, having established
    /// nothing about a force-push. Scoping the guard to forks instead was the other
    /// option and is worse: it would skip the check on exactly the PRs whose branch
    /// lives in a shared repo. The four skills went on asking the owner question
    /// after the daemon stopped, so a run the daemon had cleared stopped itself on
    /// every org PR.
    ///
    /// Both unknowns fail closed: a run that force-pushes must never guess.
    pub fn push_refusal(&self, viewer: &str) -> Option<String> {
        if viewer.is_empty() {
            return Some(format!(
                "#{} cannot be worked on: your GitHub login is unknown",
                self.number
            ));
        }
        let where_ = self.head_repo.as_deref().unwrap_or("an unknown repo");
        match self.head_pushable {
            Some(true) => None,
            Some(false) => Some(format!(
                "#{} is headed from {where_}, which you cannot push to",
                self.number
            )),
            None => Some(format!(
                "#{} cannot be worked on: GitHub did not say whether you can push to {where_}",
                self.number
            )),
        }
    }

    /// Sort key for the rail PR group (§9):
    /// needs-resolving → failing → open and clean → draft.
    ///
    /// **`needs_you` is asked before `is_draft`, and that order is the fix for a
    /// row that contradicted itself.** `is_draft` used to sink a PR first, so a
    /// draft carrying a thread that waited on you sorted to the bottom with the
    /// quiet drafts while the rail labelled it *"N waiting on you"* — the daemon
    /// ordering the list by one precedence and the page labelling it by another.
    /// A draft nobody is waiting on still sinks, which is what a draft is for.
    pub fn rank(&self) -> u8 {
        if self.needs_you {
            return 0;
        }
        if self.is_draft {
            return 4;
        }
        if self.checks == Checks::Failing || self.mergeable == "CONFLICTING" {
            return 1;
        }
        if self.checks == Checks::Pending {
            return 2;
        }
        3
    }
}

// ---------------------------------------------------------------------------
// Review threads
// ---------------------------------------------------------------------------

/// Has the last word on a thread been dealt with?
///
/// The one definition, over the last comment's author and whether you reacted to
/// it. Either you replied — nothing came after you — or the reviewer had the last
/// word and you 👍'd it, which is how the review skill says "applied as asked,
/// nothing to add". **The last comment, never any comment**: a 👍 on the opening
/// comment says nothing about the reviewer's follow-up under it, and a thread that
/// came back to you must ask again.
pub fn answered(last_author: &str, viewer_thumbed: bool, viewer: &str) -> bool {
    last_author == viewer || viewer_thumbed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, head: &str, base: &str) -> Pr {
        Pr {
            number,
            title: String::new(),
            url: String::new(),
            head_ref: head.into(),
            head_repo: None,
            head_pushable: None,
            base_ref: base.into(),
            is_draft: false,
            mergeable: "MERGEABLE".into(),
            merge_state: "CLEAN".into(),
            checks: Checks::Passing,
            head_sha: None,
            unresolved: 0,
            unresolved_capped: false,
            awaiting_you: 0,
            changes_requested: false,
            needs_you: false,
            children: vec![],
        }
    }

    #[test]
    fn ranks_needing_you_above_failing_and_drafts_last() {
        let mut needs = pr(1, "a", "develop");
        // Two open threads, one of them still waiting on a word from you.
        needs.unresolved = 2;
        needs.awaiting_you = 1;
        needs.needs_you = true;
        let mut failing = pr(2, "b", "develop");
        failing.checks = Checks::Failing;
        let clean = pr(3, "c", "develop");
        let mut draft = pr(4, "d", "develop");
        draft.is_draft = true;
        draft.checks = Checks::Failing;

        assert!(needs.rank() < failing.rank());
        assert!(failing.rank() < clean.rank());
        // A draft stays at the bottom even when it is red.
        assert!(draft.rank() > clean.rank());

        /* **But a draft that is waiting on you sorts with the rows that want
        you**, not with the drafts. `is_draft` used to be asked first, so this PR
        sank to the bottom while the rail labelled it "1 waiting on you" — the
        list ordered by one precedence and labelled by another, and the row that
        contradicted itself was the one asking for something. */
        let mut draft_wants_you = pr(5, "e", "develop");
        draft_wants_you.is_draft = true;
        draft_wants_you.awaiting_you = 1;
        draft_wants_you.needs_you = true;
        assert_eq!(draft_wants_you.rank(), needs.rank());
        assert!(draft_wants_you.rank() < draft.rank());
    }
}
