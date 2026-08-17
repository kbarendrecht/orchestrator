//! The post batch — the one irreversible action in the review flow.
//!
//! Split at the push, because that is where the consequences change kind.
//! Everything before it is local and undoable: [`crate::patch::write_batch`] runs
//! the check ladder, applies, runs the hooks and folds, and any refusal leaves the
//! branch exactly as it was. Everything after it is public. So the report
//! distinguishes **cannot be unsent** from **still pending** rather than
//! collapsing both into one error.
//!
//! Two properties this module exists to hold:
//!
//! - **The payload cannot carry code.** The SPA sends thread ids and position
//!   *indices*; every patch comes out of `Inner.proposals`, which is what the
//!   human actually read. A client echoing content back could otherwise substitute
//!   a different diff than the one that was reviewed.
//! - **Retry is not a second code path.** Nothing is remembered about what landed;
//!   what is missing is re-derived from a fresh fetch. A ledger can be killed
//!   between the call succeeding and the write, and would then lie in exactly the
//!   case it exists for.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

use crate::github::{Pr, ThreadRoot, Threads};
use crate::github_write::{self, Target};
use crate::patch::{FileStat, Patch, Written};
use crate::proposal::{Does, Position};
use crate::state::AppState;

/// One handled thread: which position, and your wording if you changed it.
#[derive(Debug, Clone, Deserialize)]
pub struct Decision {
    pub thread_id: String,
    /// Index into the proposal's `positions`. Never the content — see the module
    /// doc.
    pub position: usize,
    /// Overrides the drafted reply. `None` keeps what triage wrote.
    #[serde(default)]
    pub reply: Option<String>,
}

/// What the final screen sends. A thread absent from `decisions` was skipped.
#[derive(Debug, Clone, Deserialize)]
pub struct Batch {
    /// The head the proposals were generated against, re-checked here: a
    /// force-push in between invalidates every patch.
    pub base_sha: String,
    pub decisions: Vec<Decision>,
}

/// A single outward write, named the way the report renders it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum What {
    Reply,
    ThumbsUp,
    Rerequest,
}

/// Something that is now true on GitHub.
#[derive(Debug, Clone, Serialize)]
pub struct Landed {
    pub thread_id: String,
    /// `renovate.json5:161 · carol`, for the report's left column.
    pub label: String,
    pub what: What,
    /// It was already there, so nothing was sent. The distinction matters on a
    /// retry: "posted" and "already posted" read the same to a reviewer but not
    /// to someone deciding whether the retry worked.
    pub already: bool,
}

/// A write that was attempted and refused. `error` is `gh`'s own words.
#[derive(Debug, Clone, Serialize)]
pub struct Failed {
    pub thread_id: String,
    pub label: String,
    pub what: What,
    pub error: String,
}

/// A write that was never tried, and what it is waiting on.
#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub label: String,
    pub what: What,
    pub waiting_on: String,
}

/// What the failure panel renders.
///
/// `refused` is the local half saying no — the ladder found a stale patch, the
/// hooks rewrote a file, pre-commit failed. Nothing was committed and nothing was
/// pushed, so every other field is empty and the screen is panel 7 rather than
/// panel 8.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PostReport {
    pub refused: Option<String>,
    /// The complete file list the batch wrote, from `git apply --numstat`.
    pub files: Vec<FileStat>,
    /// How the fold resolved, including when it fell back to a HEAD amend.
    pub amend: Option<String>,
    /// The sha now on origin. `Some` means the code cannot be taken back.
    pub pushed: Option<String>,
    pub landed: Vec<Landed>,
    pub failed: Vec<Failed>,
    pub skipped: Vec<Skipped>,
    pub rerequested: Vec<String>,
    pub held_back: Vec<Skipped>,
}

impl PostReport {
    fn refused(why: impl Into<String>) -> Self {
        PostReport {
            refused: Some(why.into()),
            ..Default::default()
        }
    }

    /// Did anything at all fail to land? Drives the header's "posted with errors".
    pub fn ok(&self) -> bool {
        self.refused.is_none() && self.failed.is_empty()
    }
}

/// One resolved decision: the position, plus what it is aimed at.
#[derive(Debug)]
struct Handled {
    thread_id: String,
    label: String,
    root: ThreadRoot,
    does: Does,
    patch: Option<String>,
    reply: Option<String>,
    /// The story to file, for a `story+reply` position. Taken from the proposal
    /// the human read, never from the payload — same rule as the patch.
    story: Option<crate::proposal::StoryDraft>,
    /// `(path, line)` for the blame that picks the fixup target. Only for a
    /// position that writes code.
    touched: Option<(String, u32)>,
}

/// Turn the payload into resolved work, refusing anything the daemon cannot do.
///
/// Every lookup here is a containment check: the position must exist in the
/// proposal the human read, the thread must be in the fresh fetch, and the
/// comment id comes from [`Threads::root_for`] rather than from the payload.
fn resolve(
    positions: &crate::proposal::ProposalSet,
    fresh: &Threads,
    batch: &Batch,
    tracker: crate::config::Tracker,
) -> Result<Vec<Handled>> {
    let mut out = Vec::new();
    for d in &batch.decisions {
        let proposal = positions
            .proposals
            .iter()
            .find(|p| p.thread_id == d.thread_id)
            .with_context(|| format!("no triage proposal for thread {}", d.thread_id))?;
        let pos: &Position = proposal.positions.get(d.position).with_context(|| {
            format!(
                "thread {}: position {} of {}",
                d.thread_id,
                d.position,
                proposal.positions.len()
            )
        })?;

        // Named rather than silently skipped: a thread that quietly did nothing is
        // indistinguishable from one that was handled, which is the whole failure
        // this flow is built to avoid.
        if pos.does == Does::Manual {
            bail!(
                "thread {}: the manual phase is not built yet — it has to open after the \
                 accepted patches are committed. Pick another position or skip.",
                d.thread_id
            );
        }
        if pos.does.files_story() && !tracker.is_configured() {
            bail!(
                "thread {}: no tracker is configured, so there is nowhere to file this. \
                 Set `tracker` in the config, pick another position, or skip.",
                d.thread_id
            );
        }

        let thread = fresh
            .items
            .iter()
            .find(|t| t.id == d.thread_id)
            .with_context(|| {
                format!(
                    "thread {} is no longer among this PR's threads — re-triage",
                    d.thread_id
                )
            })?;
        let root = fresh
            .root_for(&d.thread_id)
            .with_context(|| format!("thread {} has no comment to answer", d.thread_id))?;

        let touched = if pos.does.writes_code() {
            let path = thread.path.clone().with_context(|| {
                format!(
                    "thread {}: a patch was accepted on a thread with no file",
                    d.thread_id
                )
            })?;
            // An outdated thread has no current `line`; blame falls back to where
            // the reviewer was looking. If that lands on a different commit than
            // the batch's other patches, `amend_target` degrades to a HEAD amend
            // and says so, which is the right outcome rather than a guess.
            let line = thread
                .line
                .or(thread.original_line)
                .with_context(|| format!("thread {}: a patch with no line", d.thread_id))?;
            Some((path, line))
        } else {
            None
        };

        let reply = match (pos.does.writes_reply(), &d.reply) {
            // Your wording wins over the draft. Empty is a refusal, not a blank
            // comment: a reply-only position that posts nothing does nothing.
            (true, Some(text)) if !text.trim().is_empty() => Some(text.clone()),
            (true, Some(_)) => bail!(
                "thread {}: this position posts a reply and the text is empty",
                d.thread_id
            ),
            (true, None) => Some(
                pos.reply
                    .clone()
                    .filter(|r| !r.trim().is_empty())
                    .with_context(|| format!("thread {}: no reply text to post", d.thread_id))?,
            ),
            (false, _) => None,
        };
        // The one agent-or-human string the validator never sized. It is about to
        // carry a public URL, so it is bounded here rather than at the API edge.
        if let Some(r) = &reply {
            anyhow::ensure!(
                r.len() <= crate::proposal::MAX_FIELD,
                "thread {}: the reply exceeds {} bytes",
                d.thread_id,
                crate::proposal::MAX_FIELD
            );
        }
        // `{story}` is what the id is substituted into, so a story reply without
        // it would file a story and never link to it. The draft is validated on
        // arrival; this catches the human deleting the token while editing. Refused
        // here, which is before the local half and long before anything is filed.
        if pos.does.files_story() {
            let text = reply.as_deref().unwrap_or_default();
            if !text.contains(crate::proposal::STORY_TOKEN) {
                bail!(
                    "thread {}: this reply files a story but no longer contains {} — \
                     it would be filed with nothing linking to it. Put the token back, \
                     or pick another position.",
                    d.thread_id,
                    crate::proposal::STORY_TOKEN
                );
            }
        } else if let Some(text) = &reply {
            // The reverse: a token in a position that files nothing would post the
            // literal braces to GitHub.
            if text.contains(crate::proposal::STORY_TOKEN) {
                bail!(
                    "thread {}: this reply contains {} but the position files no story, \
                     so there would be no id to put there",
                    d.thread_id,
                    crate::proposal::STORY_TOKEN
                );
            }
        }

        out.push(Handled {
            thread_id: d.thread_id.clone(),
            label: label_for(thread),
            root,
            does: pos.does,
            patch: pos.patch.clone().filter(|_| pos.does.writes_code()),
            reply,
            story: pos.story.clone().filter(|_| pos.does.files_story()),
            touched,
        });
    }
    Ok(out)
}

/// `renovate.json5:161 · carol`, or `review summary · acme-bot` for a
/// thread with no file.
fn label_for(t: &crate::github::Thread) -> String {
    let who = t.author().unwrap_or("ghost");
    match (&t.path, t.line.or(t.original_line)) {
        (Some(p), Some(l)) => format!("{p}:{l} · {who}"),
        (Some(p), None) => format!("{p} · {who}"),
        (None, _) => format!("review summary · {who}"),
    }
}

/// Run the batch.
///
/// `fresh` must be a fetch taken *now*, not the cache: it supplies the staleness
/// check, the comment ids, and the set of threads still open — which is what makes
/// retry idempotent and what lets a thread that arrived mid-session hold its
/// author back on its own.
pub async fn run(
    app: &Arc<AppState>,
    pr: &Pr,
    fresh: &Threads,
    batch: Batch,
) -> Result<PostReport> {
    // The gates again. A review can sit open for hours: the tree can go dirty, a
    // rebase can stop, or /green can start between opening the cards and pushing.
    let workspace = crate::api::workspace_for(app, &pr.head_ref)
        .await
        .with_context(|| format!("no worktree holding {} — re-triage", pr.head_ref))?;
    if let Some(g) = crate::triage::gate(app, pr.number, &workspace).await? {
        bail!("{}", g.say());
    }
    let path = app
        .workspace_path(&workspace)
        .await
        .context("the worktree vanished")?;

    if let Some(head) = &fresh.head_sha {
        if head != &batch.base_sha {
            return Ok(PostReport::refused(format!(
                "the branch moved since triage ({} → {}). The patches were generated against \
                 code that is no longer there; re-triage rather than write over it.",
                short(&batch.base_sha),
                short(head)
            )));
        }
    }

    let proposals = app
        .inner
        .read()
        .await
        .proposals
        .get(&pr.number)
        .cloned()
        .with_context(|| format!("PR #{} has no triage proposals to post", pr.number))?;
    let handled = resolve(&proposals, fresh, &batch, app.cfg.tracker)?;

    // --- half one: local, undoable -----------------------------------------
    let mut report = match write_local(&path, app, &handled).await? {
        Ok(w) => w,
        Err(refusal) => return Ok(refusal),
    };

    // --- the push: everything past here is public --------------------------
    if !report.files.is_empty() {
        let branch = pr.head_ref.clone();
        let p = path.clone();
        tokio::task::spawn_blocking(move || crate::git::push_with_lease(&p, &branch))
            .await
            .context("the push panicked")??;
        report.pushed = Some(crate::git::head_sha(&path)?);
    }

    let (owner, name) =
        crate::resolve_repo(app).context("no GitHub repo configured and none on the remote")?;
    let target = Target {
        cwd: path,
        owner,
        name,
    };
    post_outward(&target, pr.number, fresh, &handled, &mut report).await;
    Ok(report)
}

/// The local half: blame, ladder, apply, hooks, fold. `Err` on the inner result
/// is a refusal to report, not an error — the branch is untouched either way.
async fn write_local(
    path: &Path,
    app: &Arc<AppState>,
    handled: &[Handled],
) -> Result<std::result::Result<PostReport, PostReport>> {
    let patches: Vec<Patch> = handled
        .iter()
        .filter_map(|h| {
            h.patch.as_ref().map(|diff| Patch {
                thread_id: h.thread_id.clone(),
                diff: diff.clone(),
            })
        })
        .collect();
    if patches.is_empty() {
        // Reply-only decisions still have posting to do. Not a no-op.
        return Ok(Ok(PostReport::default()));
    }
    let touched: Vec<(String, u32)> = handled.iter().filter_map(|h| h.touched.clone()).collect();

    let cwd = path.to_path_buf();
    let upstream = app.cfg.upstream_ref.clone();
    let written = tokio::task::spawn_blocking(move || -> Result<Written> {
        let base = crate::git::merge_base(&cwd, &upstream)?;
        let email = crate::git::user_email(&cwd);
        crate::patch::write_batch(&cwd, &base, &email, &patches, &touched)
    })
    .await
    .context("the write batch panicked")??;

    Ok(match written {
        Written::Committed { files, amend } => Ok(PostReport {
            files,
            amend: Some(amend),
            ..Default::default()
        }),
        Written::NothingToWrite => Ok(PostReport::default()),
        Written::Refused(why) => Err(PostReport::refused(why)),
    })
}

/// The outward writes, in the order the design settled: replies, then reactions,
/// then re-requests.
///
/// A failure does **not** stop the rest. Each write is independent, and the one
/// dependency that does exist — a re-request asserting "everything of yours is
/// addressed" — is expressed by the failed thread counting as still open, so its
/// author lands in `held_back` on its own.
async fn post_outward(
    target: &Target,
    pr: u64,
    fresh: &Threads,
    handled: &[Handled],
    report: &mut PostReport,
) {
    let mut done: Vec<&str> = Vec::new();

    for h in handled {
        let mut all_landed = true;

        if let Some(text) = &h.reply {
            match already_replied(fresh, &h.thread_id, text) {
                true => report.landed.push(Landed {
                    thread_id: h.thread_id.clone(),
                    label: h.label.clone(),
                    what: What::Reply,
                    already: true,
                }),
                false => match blocking(target, &h.root, Send::Reply(text.clone())).await {
                    Ok(()) => report.landed.push(Landed {
                        thread_id: h.thread_id.clone(),
                        label: h.label.clone(),
                        what: What::Reply,
                        already: false,
                    }),
                    Err(e) => {
                        all_landed = false;
                        report.failed.push(Failed {
                            thread_id: h.thread_id.clone(),
                            label: h.label.clone(),
                            what: What::Reply,
                            error: format!("{e:#}"),
                        });
                    }
                },
            }
        }

        if h.does.gives_thumbs_up() {
            // Not re-derivable: the thread query does not select reactions.
            // GitHub is expected to treat one as unique per (user, content) and
            // return the existing one, so a retry is a no-op rather than a
            // duplicate. Unverified until the scratch PR settles it; the fallback
            // is selecting `reactions` in the thread query, never a ledger.
            match blocking(target, &h.root, Send::ThumbsUp).await {
                Ok(()) => report.landed.push(Landed {
                    thread_id: h.thread_id.clone(),
                    label: h.label.clone(),
                    what: What::ThumbsUp,
                    already: false,
                }),
                Err(e) => {
                    all_landed = false;
                    report.failed.push(Failed {
                        thread_id: h.thread_id.clone(),
                        label: h.label.clone(),
                        what: What::ThumbsUp,
                        error: format!("{e:#}"),
                    });
                }
            }
        }

        if all_landed {
            done.push(&h.thread_id);
        }
    }

    rerequest(target, pr, fresh, &done, report).await;
}

/// The reviewers of a PR, split by whether anything of theirs is still open.
struct Split<'a> {
    /// Everyone with an answerable thread on this PR.
    all: Vec<&'a str>,
    /// Those with at least one thread this batch did not settle.
    open: Vec<&'a str>,
    /// Each of those, with a thread of theirs to name.
    holding: Vec<(&'a str, String)>,
}

/// Work out who can be re-requested, from the fresh fetch alone.
///
/// "Theirs" is the threads they opened. `done` is the threads this batch settled
/// completely — so a thread that arrived mid-session, one that was skipped, and
/// one whose reply failed all count the same way, holding their author back with
/// no special handling for any of them. That is the whole reason re-request is
/// derived rather than tracked.
fn split_reviewers<'a>(fresh: &'a Threads, done: &[&str]) -> Split<'a> {
    let mut s = Split {
        all: Vec::new(),
        open: Vec::new(),
        holding: Vec::new(),
    };
    for t in fresh.items.iter().filter(|t| t.answerable) {
        let Some(who) = t.author() else { continue };
        // Your own comment is not a review of yourself, and GitHub refuses to
        // request review from the PR's author anyway.
        if who == fresh.viewer {
            continue;
        }
        s.all.push(who);
        if !done.contains(&t.id.as_str()) {
            s.open.push(who);
            s.holding.push((who, label_for(t)));
        }
    }
    s
}

/// Ask every reviewer with nothing of theirs left open to look again.
async fn rerequest(
    target: &Target,
    pr: u64,
    fresh: &Threads,
    done: &[&str],
    report: &mut PostReport,
) {
    let Split { all, open, holding } = split_reviewers(fresh, done);

    for login in github_write::ready_to_rerequest(&all, &open) {
        match blocking_rerequest(target, pr, login).await {
            Ok(()) => report.rerequested.push(login.to_string()),
            Err(e) => report.failed.push(Failed {
                thread_id: String::new(),
                label: format!("re-request {login}"),
                what: What::Rerequest,
                error: format!("{e:#}"),
            }),
        }
    }
    // One row per held-back reviewer, naming the first thread of theirs that is
    // still open — the whole point is that it is answerable, not a count.
    let mut named: Vec<&str> = Vec::new();
    for (who, thread) in holding {
        if named.contains(&who) {
            continue;
        }
        named.push(who);
        report.held_back.push(Skipped {
            label: format!("re-request {who}"),
            what: What::Rerequest,
            waiting_on: format!("{thread} is still unanswered"),
        });
    }
}

/// Has this exact reply already been posted?
///
/// Exact match on `with_footer`'s output, by the viewer. Reliable because we
/// control the text: the footer makes our own comments identifiable and the body
/// is byte-for-byte what would be sent. A reply the human edited between attempts
/// posts again, which is correct — it is different words.
fn already_replied(fresh: &Threads, thread_id: &str, body: &str) -> bool {
    let want = github_write::with_footer(body);
    fresh
        .items
        .iter()
        .find(|t| t.id == thread_id)
        .is_some_and(|t| {
            t.comments
                .iter()
                .any(|c| c.author == fresh.viewer && c.body.trim() == want.trim())
        })
}

enum Send {
    Reply(String),
    ThumbsUp,
}

/// `gh` is a subprocess, so every write goes through the blocking pool rather
/// than stalling the runtime for the length of an HTTP round trip.
async fn blocking(target: &Target, root: &ThreadRoot, send: Send) -> Result<()> {
    let (t, root) = (target.clone(), root.clone());
    tokio::task::spawn_blocking(move || match send {
        Send::Reply(body) => t.reply(&root, &body).map(|_| ()),
        Send::ThumbsUp => t.thumbs_up(&root).map(|_| ()),
    })
    .await
    .context("the write panicked")?
}

async fn blocking_rerequest(target: &Target, pr: u64, login: &str) -> Result<()> {
    let (t, login) = (target.clone(), login.to_string());
    tokio::task::spawn_blocking(move || t.rerequest(pr, &login))
        .await
        .context("the re-request panicked")?
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The story arms only exist with a tracker configured, so the tests that are
    /// not about that run with one.
    const TRACKER: crate::config::Tracker = crate::config::Tracker::Stub;
    use crate::github::{Comment, Thread};
    use crate::proposal::{Proposal, ProposalSet, StoryDraft};

    fn comment(id: u64, author: &str, body: &str) -> Comment {
        Comment {
            database_id: id,
            author: author.into(),
            body: body.into(),
            created_at: "2026-08-17T00:00:00Z".into(),
            url: "u".into(),
            diff_hunk: None,
        }
    }

    fn thread(id: &str, path: Option<&str>, line: Option<u32>, author: &str) -> Thread {
        Thread {
            id: id.into(),
            path: path.map(str::to_string),
            line,
            start_line: None,
            original_line: None,
            is_resolved: false,
            is_outdated: false,
            comments: vec![comment(100, author, "you call this twice")],
            answerable: true,
        }
    }

    fn fetched(items: Vec<Thread>) -> Threads {
        Threads {
            pr: 10001,
            viewer: "kars".into(),
            head_sha: Some("abc123".into()),
            items,
        }
    }

    fn position(does: Does) -> Position {
        Position {
            label: "Apply".into(),
            sub: String::new(),
            does,
            patch: does.writes_code().then(|| "--- a/f\n+++ b/f\n".to_string()),
            reply: does.writes_reply().then(|| "Resolved.".to_string()),
            story: does.files_story().then(|| StoryDraft {
                title: "t".into(),
                body: "b".into(),
            }),
        }
    }

    fn proposed(thread_id: &str, positions: Vec<Position>) -> ProposalSet {
        ProposalSet {
            base_sha: "abc123".into(),
            proposals: vec![Proposal {
                thread_id: thread_id.into(),
                continued: false,
                read: "they are right".into(),
                verified: Some("grepped it".into()),
                recommend: 0,
                positions,
            }],
        }
    }

    fn batch(thread_id: &str, position: usize, reply: Option<&str>) -> Batch {
        Batch {
            base_sha: "abc123".into(),
            decisions: vec![Decision {
                thread_id: thread_id.into(),
                position,
                reply: reply.map(str::to_string),
            }],
        }
    }

    #[test]
    fn a_patch_position_resolves_to_its_diff_and_a_blame_target() {
        let set = proposed("PRRT_1", vec![position(Does::ChangeThumbsUp)]);
        let fresh = fetched(vec![thread("PRRT_1", Some("src/Foo.php"), Some(42), "john")]);
        let got = resolve(&set, &fresh, &batch("PRRT_1", 0, None), TRACKER).unwrap();

        assert_eq!(got.len(), 1);
        assert!(got[0].patch.is_some());
        assert_eq!(got[0].touched, Some(("src/Foo.php".into(), 42)));
        // change+thumbsup posts a reaction, not words.
        assert!(got[0].reply.is_none());
        assert!(got[0].does.gives_thumbs_up());
        assert_eq!(got[0].root.comment_id(), 100);
    }

    #[test]
    fn your_wording_overrides_the_draft() {
        let set = proposed("PRRT_1", vec![position(Does::Reply)]);
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(1), "john")]);

        let kept = resolve(&set, &fresh, &batch("PRRT_1", 0, None), TRACKER).unwrap();
        assert_eq!(kept[0].reply.as_deref(), Some("Resolved."));

        let mine = resolve(
            &set,
            &fresh,
            &batch("PRRT_1", 0, Some("Nee, want …")),
            TRACKER,
        )
        .unwrap();
        assert_eq!(mine[0].reply.as_deref(), Some("Nee, want …"));
    }

    #[test]
    fn a_reply_position_with_an_emptied_box_is_refused_not_posted_blank() {
        // `Say something else` starts empty; sending it untouched would post a
        // blank comment that cannot be deleted from here.
        let set = proposed("PRRT_1", vec![position(Does::Reply)]);
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(1), "john")]);
        let err = resolve(&set, &fresh, &batch("PRRT_1", 0, Some("   ")), TRACKER)
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty"), "{err}");
    }

    /// A story position, on the review-summary thread that is its canonical home.
    fn story_set() -> ProposalSet {
        ProposalSet {
            base_sha: "abc123".into(),
            proposals: vec![Proposal {
                thread_id: "PRRT_1".into(),
                continued: false,
                read: "fair, out of scope".into(),
                verified: None,
                recommend: 0,
                positions: vec![Position {
                    reply: Some("Tracked as {story}.".into()),
                    ..position(Does::StoryReply)
                }],
            }],
        }
    }

    #[test]
    fn manual_is_still_named_rather_than_silently_skipped() {
        // A thread that quietly did nothing is indistinguishable from one that was
        // handled — which is the failure this whole flow exists to prevent.
        let fresh = fetched(vec![thread("PRRT_1", None, None, "acme-bot")]);
        let manual = proposed(
            "PRRT_1",
            vec![Position {
                label: "Manual".into(),
                sub: String::new(),
                does: Does::Manual,
                patch: None,
                reply: Some("done".into()),
                story: None,
            }],
        );
        let err = resolve(&manual, &fresh, &batch("PRRT_1", 0, None), TRACKER)
            .unwrap_err()
            .to_string();
        assert!(err.contains("manual phase"), "{err}");
        assert!(err.contains("not built yet"), "{err}");
    }

    #[test]
    fn a_story_resolves_to_its_draft_and_keeps_the_token() {
        let fresh = fetched(vec![thread("PRRT_1", None, None, "acme-bot")]);
        let got = resolve(&story_set(), &fresh, &batch("PRRT_1", 0, None), TRACKER).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].story.as_ref().map(|s| s.title.as_str()), Some("t"));
        // The token survives to the outward step, which is what substitutes it.
        assert!(got[0].reply.as_deref().unwrap().contains("{story}"));
        // A story writes no code and posts no reaction.
        assert!(got[0].patch.is_none());
        assert!(!got[0].does.gives_thumbs_up());
    }

    #[test]
    fn a_story_with_no_tracker_configured_is_refused_by_name() {
        // The overlay hides the option when `tracker` is off, but the payload is
        // not the overlay — so the daemon says so rather than filing into nowhere.
        let fresh = fetched(vec![thread("PRRT_1", None, None, "acme-bot")]);
        let err = resolve(
            &story_set(),
            &fresh,
            &batch("PRRT_1", 0, None),
            crate::config::Tracker::None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no tracker is configured"), "{err}");
    }

    #[test]
    fn deleting_the_token_while_editing_refuses_before_anything_is_filed() {
        // The draft is validated on arrival, so only a human edit can lose the
        // token. Filing a story that nothing links to is worse than not filing it,
        // and this refusal lands before the local half — nothing written, nothing
        // filed, decisions kept.
        let fresh = fetched(vec![thread("PRRT_1", None, None, "acme-bot")]);
        let err = resolve(
            &story_set(),
            &fresh,
            &batch("PRRT_1", 0, Some("Goed punt, komt later.")),
            TRACKER,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("{story}"), "{err}");
        assert!(err.contains("nothing linking to it"), "{err}");
    }

    #[test]
    fn a_token_in_a_position_that_files_nothing_is_refused() {
        // The reverse mistake: there would be no id to substitute, so the literal
        // braces would be posted to GitHub.
        let set = proposed("PRRT_1", vec![position(Does::Reply)]);
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(1), "john")]);
        let err = resolve(
            &set,
            &fresh,
            &batch("PRRT_1", 0, Some("Tracked as {story}.")),
            TRACKER,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("files no story"), "{err}");
    }

    #[test]
    fn an_oversized_reply_override_is_refused() {
        // The one agent-or-human string the validator never sized, and it is about
        // to carry a public URL.
        let set = proposed("PRRT_1", vec![position(Does::Reply)]);
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(1), "john")]);
        let huge = "x".repeat(crate::proposal::MAX_FIELD + 1);
        let err = resolve(&set, &fresh, &batch("PRRT_1", 0, Some(&huge)), TRACKER)
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn a_decision_the_human_never_saw_is_refused() {
        let set = proposed("PRRT_1", vec![position(Does::Reply)]);
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(1), "john")]);

        // An index past the list: the payload carries indices, so this is how a
        // bad client would try to reach content nobody reviewed.
        assert!(resolve(&set, &fresh, &batch("PRRT_1", 7, None), TRACKER).is_err());
        // A thread with no proposal behind it.
        assert!(resolve(&set, &fresh, &batch("PRRT_9", 0, None), TRACKER).is_err());
        // A thread that has since gone from the PR.
        let gone = fetched(vec![thread("PRRT_2", Some("a.ts"), Some(1), "john")]);
        assert!(resolve(&set, &gone, &batch("PRRT_1", 0, None), TRACKER).is_err());
    }

    #[test]
    fn a_patch_needs_a_line_to_blame() {
        // A review summary has no file, so there is nowhere for a diff to go — that
        // case goes through the manual phase, not through a guessed anchor.
        let set = proposed("PRRT_1", vec![position(Does::ChangeReply)]);
        let fresh = fetched(vec![thread("PRRT_1", None, None, "acme-bot")]);
        let err = resolve(&set, &fresh, &batch("PRRT_1", 0, None), TRACKER)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no file"), "{err}");
    }

    #[test]
    fn a_reply_already_in_the_thread_is_not_posted_twice() {
        // This is the whole of retry: derive what is missing from a fresh fetch
        // rather than remember what was sent.
        let mut t = thread("PRRT_1", Some("a.ts"), Some(1), "john");
        t.comments.push(comment(
            101,
            "kars",
            &github_write::with_footer("Resolved."),
        ));
        let fresh = fetched(vec![t]);

        assert!(already_replied(&fresh, "PRRT_1", "Resolved."));
        // Different words are a different reply, and should go.
        assert!(!already_replied(&fresh, "PRRT_1", "Fixed in the mapper."));
    }

    #[test]
    fn someone_elses_identical_comment_does_not_count_as_our_reply() {
        let mut t = thread("PRRT_1", Some("a.ts"), Some(1), "john");
        t.comments
            .push(comment(101, "john", &github_write::with_footer("Resolved.")));
        assert!(!already_replied(&fetched(vec![t]), "PRRT_1", "Resolved."));
    }

    #[test]
    fn a_reviewer_is_held_back_by_their_own_unsettled_thread() {
        let fresh = fetched(vec![
            thread("PRRT_1", Some("a.ts"), Some(10), "carol"),
            thread("PRRT_2", Some("a.ts"), Some(20), "carol"),
            thread("PRRT_3", Some("b.ts"), Some(1), "dave"),
        ]);

        // Dave's one thread is settled; one of carol's is not.
        let split = split_reviewers(&fresh, &["PRRT_1", "PRRT_3"]);
        assert_eq!(
            github_write::ready_to_rerequest(&split.all, &split.open),
            vec!["dave"]
        );
        assert_eq!(split.holding.len(), 1);
        assert_eq!(split.holding[0].0, "carol");
        // The row names the thread, not a count — it is the one to go and answer.
        assert_eq!(split.holding[0].1, "a.ts:20 · carol");

        // With everything settled, both go.
        let split = split_reviewers(&fresh, &["PRRT_1", "PRRT_2", "PRRT_3"]);
        assert_eq!(
            github_write::ready_to_rerequest(&split.all, &split.open),
            vec!["carol", "dave"]
        );
        assert!(split.holding.is_empty());
    }

    #[test]
    fn a_thread_that_arrived_mid_session_holds_its_author_back_on_its_own() {
        // No proposal, no decision, so it cannot be in `done` — and that is enough.
        // The re-request assertion "everything of yours is addressed" stays true
        // without any mechanism for noticing the arrival.
        let fresh = fetched(vec![
            thread("PRRT_1", Some("a.ts"), Some(10), "carol"),
            thread("PRRT_new", Some("c.ts"), Some(3), "carol"),
        ]);
        let split = split_reviewers(&fresh, &["PRRT_1"]);
        assert!(github_write::ready_to_rerequest(&split.all, &split.open).is_empty());
    }

    #[test]
    fn your_own_thread_never_asks_you_to_review_yourself() {
        let fresh = fetched(vec![thread("PRRT_1", Some("a.ts"), Some(1), "kars")]);
        let split = split_reviewers(&fresh, &[]);
        assert!(split.all.is_empty());
        assert!(split.holding.is_empty());
    }

    #[test]
    fn a_summary_thread_is_labelled_by_what_it_is() {
        assert_eq!(
            label_for(&thread("PRRT_1", None, None, "acme-bot")),
            "review summary · acme-bot"
        );
        let mut outdated = thread("PRRT_2", Some("a.ts"), None, "john");
        outdated.original_line = Some(9);
        outdated.is_outdated = true;
        // Outdated: no current line, so it is named by where the reviewer looked.
        assert_eq!(label_for(&outdated), "a.ts:9 · alice");
    }
}
