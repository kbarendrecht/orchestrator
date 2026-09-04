//! The forge-agnostic model.
//!
//! These types are what a [`crate::forge::Forge`] hands back, whatever platform
//! it talks to. The GitHub GraphQL/curl details that build them live in
//! [`super::github`]; a future GitLab impl would build the same shapes from its
//! own API. Keeping the types here rather than in the GitHub module is what lets
//! a second forge exist without depending on the first.
//!
//! **[`ThreadRoot`] is the exception worth stating plainly:** it is
//! GitHub-shaped, not truly forge-agnostic — its `comment_id` is a REST
//! `databaseId`. It is shared here anyway, because its *value* is the safety
//! invariant (only [`Threads::root_for`] can mint one), and privacy in Rust is
//! per-module, so the constructor and the type have to share a home. Generalising
//! the id to something a non-GitHub forge could fill is a later pass.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(export, export_to = "../web/snapshot.d.ts"))]
pub enum Checks {
    Passing,
    Failing,
    Pending,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export, export_to = "../web/snapshot.d.ts"))]
pub struct Pr {
    #[cfg_attr(test, ts(type = "number"))]
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
    /// internal attempt — it is an identity for "has the branch changed since
    /// the run gave up", not a provenance record (§8).
    pub head_sha: Option<String>,
    /// GitHub's sense of resolved — a conversation the comment author closed.
    /// Not to be confused with `/api/pr/:n/resolve`, which is *our* flow for
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
    #[cfg_attr(test, ts(type = "Array<number>"))]
    pub children: Vec<u64>,
}

impl Pr {
    /// Sort key for the rail PR group (§9):
    /// needs-resolving → failing → open and clean → draft.
    pub fn rank(&self) -> u8 {
        if self.is_draft {
            return 4;
        }
        if self.needs_you {
            return 0;
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
/// word and you 👍'd it, which is the convention `commands/resolve.md` writes for
/// "applied as asked, nothing to add".
///
/// One function because two existed: the poll read the reaction and the resolve
/// flow could not, so a thread the flow had answered with a 👍 stayed answerable
/// and the overlay kept offering it. Takes the two facts rather than a thread,
/// because the poll judges raw GraphQL and the flow judges a parsed [`Thread`] —
/// the shapes differ, the rule must not.
pub fn answered(last_author: &str, viewer_thumbed: bool, viewer: &str) -> bool {
    last_author == viewer || viewer_thumbed
}

/// One comment in a review thread.
#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    /// REST id. The reply endpoint is keyed on this, not on the GraphQL node id.
    pub database_id: u64,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub url: String,
    /// The anchored patch text. GitHub hangs it off every comment; only the
    /// first one's is worth rendering, so `Thread::diff_hunk` reads that.
    pub diff_hunk: Option<String>,
    /// You have 👍'd this comment.
    ///
    /// Fetched because the poll and the resolve flow have to agree on what a
    /// thumbs-up means, and only the poll could see one: the detailed thread query
    /// did not ask for reactions, so `is_answerable` re-offered every thread the
    /// flow had already answered with a 👍 — the review's §3 finding.
    pub viewer_thumbed: bool,
}

/// An unresolved conversation on a PR.
#[derive(Debug, Clone, Serialize)]
pub struct Thread {
    /// `PRRT_…`. **Not** a resolve target — closing a thread is the comment
    /// author's button, never ours — but the join key between a thread and the
    /// finding the triage agent returns for it.
    pub id: String,
    pub path: Option<String>,
    /// `null` on an outdated thread; the finding can still stand.
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub original_line: Option<u32>,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub comments: Vec<Comment>,
    /// [`Thread::is_answerable`] against the fetch's own viewer, resolved by
    /// [`Threads::mark_answerable`] so the SPA does not have to reimplement the
    /// rule. Always false straight out of parsing, which has no viewer to judge
    /// by.
    pub answerable: bool,
}

impl Thread {
    /// The patch the thread is anchored to, off the opening comment.
    pub fn diff_hunk(&self) -> Option<&str> {
        self.comments.first()?.diff_hunk.as_deref()
    }

    /// Who opened it.
    pub fn author(&self) -> Option<&str> {
        self.comments.first().map(|c| c.author.as_str())
    }

    /// Whether this thread still wants something from you.
    ///
    /// Resolved threads are done. **Outdated ones are not skipped** — the code
    /// moved, but the point may still stand. Everything else is [`answered`],
    /// which the poll's count uses too, so the two cannot drift.
    ///
    /// This deliberately still differs from the poll on one thing, and the
    /// difference is not an oversight: the poll drops outdated threads from
    /// `awaiting_you` because they should not nag, while this includes them
    /// because they can be answered. The two questions are "should this PR ask for
    /// your attention" and "is there anything here you could say" — see
    /// `count_open`, which says the same thing from the other side.
    pub fn is_answerable(&self, viewer: &str) -> bool {
        if self.is_resolved {
            return false;
        }
        match self.comments.last() {
            Some(last) => !answered(&last.author, last.viewer_thumbed, viewer),
            // A thread with no comments cannot be answered.
            None => false,
        }
    }
}

/// Everything one on-demand thread fetch yields.
#[derive(Debug, Clone, Serialize)]
pub struct Threads {
    /// Which PR this was fetched for. Carried so [`Threads::root_for`] can stamp
    /// it onto a [`ThreadRoot`] — the reply endpoint is nested under a PR number,
    /// and taking it from anywhere but the fetch that produced the comment id
    /// would let the two disagree.
    pub pr: u64,
    /// Your own login, for `Thread::is_answerable` and the triage prompt.
    pub viewer: String,
    /// Head at fetch time. A force-push between triage and posting invalidates
    /// every proposal derived from the earlier diff, so this is recorded and
    /// re-checked rather than trusted.
    pub head_sha: Option<String>,
    pub items: Vec<Thread>,
}

/// A comment id proven to belong to a specific PR's review threads.
///
/// Every write in [`super::github_write`] takes one of these instead of a bare
/// `u64`. The fields are private and the only constructor is
/// [`Threads::root_for`], so an id the triage agent invented — or one lifted out
/// of a review comment someone else wrote — cannot reach `gh` on your
/// credential. Same shape as `resolve_in_workspace` (`src/edit.rs`): hand back a
/// safe value or nothing, rather than checking at each call site.
///
/// It lives here rather than beside the write methods because privacy in Rust is
/// per-module: only the module holding [`Threads`] can build one from a lookup,
/// which is exactly the property wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRoot {
    pr: u64,
    comment_id: u64,
}

impl ThreadRoot {
    pub fn pr(&self) -> u64 {
        self.pr
    }
    pub fn comment_id(&self) -> u64 {
        self.comment_id
    }
}

impl Threads {
    /// Resolve each thread's [`Thread::answerable`] against this fetch's viewer.
    /// `pub(crate)` so the GitHub fetch can call it once paging is done.
    pub(crate) fn mark_answerable(&mut self) {
        // Destructured so the viewer read and the thread writes are disjoint
        // borrows of self rather than an overlapping one.
        let Threads { viewer, items, .. } = self;
        for t in items {
            t.answerable = t.is_answerable(viewer);
        }
    }

    /// Put the threads in GitHub's Files-changed order.
    ///
    /// The API returns them **chronologically** — the Conversation tab's order,
    /// jumping between files — but people review in the Files tab, whose order is
    /// the diff's, and a diff's file order is a plain path sort. So sorting by
    /// `(path, line)` reproduces the view they already read in, and puts two
    /// comments a few lines apart in one file back to back, where the second
    /// usually changes what the first deserves as an answer.
    ///
    /// Two traps: an outdated thread has no `line` and must sort on the line it
    /// was originally left at; and the comparison is plain byte order on the full
    /// path, matching git (`-` is 0x2D and `/` is 0x2F, so `foo-baz.ts` sorts
    /// before `foo/bar.ts` — which looks wrong and is what git does). A thread
    /// with no path at all is a review summary: it is about the PR rather than a
    /// file, so it sorts last.
    pub(crate) fn sort_for_review(&mut self) {
        self.items.sort_by(|a, b| {
            let key = |t: &Thread| {
                (
                    t.path.is_none(),
                    t.path.clone().unwrap_or_default(),
                    t.line.or(t.original_line).unwrap_or(0),
                )
            };
            key(a).cmp(&key(b))
        });
    }

    /// The comment a reply or reaction for this thread must be aimed at.
    ///
    /// Always the **opening** comment: the replies sub-resource threads under it,
    /// and it is the only comment in the conversation we ever legitimately post
    /// to. `None` for an unknown thread id or an empty thread, so a bad id
    /// becomes a refusal rather than a write somewhere unintended.
    pub fn root_for(&self, thread_id: &str) -> Option<ThreadRoot> {
        let t = self.items.iter().find(|t| t.id == thread_id)?;
        Some(ThreadRoot {
            pr: self.pr,
            comment_id: t.comments.first()?.database_id,
        })
    }

    /// How many threads still want something from you.
    pub fn answerable_count(&self) -> usize {
        self.items.iter().filter(|t| t.answerable).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No comments: these tests are about where a thread sits, not who spoke.
    fn thread_at(path: Option<&str>, line: Option<u32>) -> Thread {
        Thread {
            comments: Vec::new(),
            ..crate::testutil::thread(
                &format!("PRRT_{}_{:?}", path.unwrap_or("none"), line),
                path,
                line,
                "anyone",
            )
        }
    }

    fn said(author: &str, thumbed: bool) -> Comment {
        Comment {
            viewer_thumbed: thumbed,
            ..crate::testutil::comment(1, author, "b")
        }
    }

    /// The §3 symptom, from the side that had it wrong: the flow's own predicate
    /// could not see a 👍, so the overlay kept offering a thread `/resolve` had
    /// already answered that way — while the poll counted the same thread handled.
    #[test]
    fn a_thread_you_thumbed_is_not_still_asking() {
        let mut t = thread_at(Some("a.ts"), Some(1));
        t.comments = vec![said("me", false), said("them", false)];
        assert!(t.is_answerable("me"), "they had the last word and nothing else");

        t.comments = vec![said("me", false), said("them", true)];
        assert!(!t.is_answerable("me"), "a 👍 is an answer — the flow writes it as one");
        // And the poll's rule agrees, which is the whole point of sharing it.
        assert!(answered("them", true, "me"));
        assert!(answered("me", false, "me"));
        assert!(!answered("them", false, "me"));

        // Outdated stays answerable here on purpose: the code moved, the point may
        // not have. The poll drops it from `awaiting_you` instead, so a PR whose
        // only open thread is outdated does not nag while the flow can still work.
        t.comments = vec![said("them", false)];
        t.is_outdated = true;
        assert!(t.is_answerable("me"));
        t.is_resolved = true;
        assert!(!t.is_answerable("me"), "resolved is done, whoever spoke last");
    }

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
    fn threads_sort_into_files_changed_order() {
        // The API hands these back chronologically; people review in the Files
        // tab, whose order is the diff's.
        let mut t = Threads {
            pr: 10001,
            viewer: "viewer".into(),
            head_sha: None,
            items: vec![
                thread_at(Some("b.ts"), Some(9)),
                thread_at(None, None), // review summary: no file
                thread_at(Some("a.ts"), Some(20)),
                thread_at(Some("a.ts"), Some(3)),
                thread_at(Some("a-z.ts"), Some(1)),
            ],
        };
        t.sort_for_review();
        let order: Vec<(Option<&str>, Option<u32>)> = t
            .items
            .iter()
            .map(|x| (x.path.as_deref(), x.line))
            .collect();
        assert_eq!(
            order,
            vec![
                // Plain byte order on the path, matching git: '-' (0x2D) sorts
                // before '/' and before 'a'..'z' continues.
                (Some("a-z.ts"), Some(1)),
                (Some("a.ts"), Some(3)),
                (Some("a.ts"), Some(20)),
                (Some("b.ts"), Some(9)),
                // A summary is about the PR, not a file, so it goes last.
                (None, None),
            ]
        );
    }

    #[test]
    fn an_outdated_thread_sorts_on_the_line_it_was_left_at() {
        // `line` is null once the code moved; without the fallback it would sort
        // to the top of its file instead of where the reviewer was looking.
        let mut t = Threads {
            pr: 10001,
            viewer: "viewer".into(),
            head_sha: None,
            items: vec![thread_at(Some("a.ts"), Some(5)), {
                let mut o = thread_at(Some("a.ts"), None);
                o.original_line = Some(2);
                o.is_outdated = true;
                o
            }],
        };
        t.sort_for_review();
        assert_eq!(t.items[0].original_line, Some(2), "outdated :2 sorts first");
        assert_eq!(t.items[1].line, Some(5));
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
    }
}
