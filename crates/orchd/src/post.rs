//! Saying one thread's words out loud — the one irreversible act of a review.
//!
//! **What is left of the post batch.** A batch used to resolve every decision,
//! write the patches, push and then post in one call; a review session answers one
//! thread at a time, with the card in front of you, so the whole of that machinery
//! went and these rules stayed. They are the rules that made it safe:
//!
//! - **The payload cannot carry code.** The words come from `Inner.proposals` or
//!   from the box you typed in, never from something a client echoed back.
//! - **Retry is not a second code path.** Nothing is remembered about what landed;
//!   a fresh fetch says whether the reply is already on the thread, so a repeat
//!   posts nothing rather than posting twice.
//! - **A story is filed before the reply that links to it**, and [`STORY_TOKEN`]
//!   never reaches GitHub.
//! - **The daemon is the only thing that files one**, so the only tracker link a
//!   reply carries is the one it substituted. The agent's words are refused if
//!   they hold `{story}` with no story to file or a link to the tracker of their
//!   own: either is a story the skill filed itself — or invented — and nothing
//!   here checked that the link is the story it claims to be.
//!
//! [`STORY_TOKEN`]: crate::proposal::STORY_TOKEN

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

use crate::forge::Threads;
use crate::forge::{self, Forge, ForgeImpl};
use crate::state::AppState;

/// What became of one thread's outward words.
#[derive(Debug, PartialEq, Eq)]
pub enum Posted {
    Sent,
    /// The identical body is already on the thread, so this was a retry rather
    /// than a second post.
    AlreadyThere,
    /// The story the reply links to could not be filed, so nothing was said. A
    /// literal `{story}` on a reviewer's thread is worse than silence.
    HeldNoStory(String),
}

/// File the story a reply links to, substitute the token, and post it — once.
///
/// The three rules are the module's: a story is filed *before* the reply that
/// links to it, `STORY_TOKEN` never reaches GitHub, and a repeated call does not
/// post twice.
///
/// `fresh` must be a fetch from *now*: it supplies the comment ids the write
/// needs, the permalink a story is keyed on, and the answer to "did I already say
/// this".
// Eight arguments, and a struct for them would be worse: every one is a distinct
// thing the caller already holds, none is optional, and bundling them would add a
// type whose only job is to be unpacked again on the next line.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn post_one(
    app: &Arc<AppState>,
    forge: &ForgeImpl,
    at: &Path,
    pr: u64,
    thread_id: &str,
    reply: &str,
    story: Option<&crate::proposal::StoryDraft>,
    fresh: &Threads,
) -> Result<Posted> {
    let host = app.cfg.tracker.as_ref().map(|t| t.host.as_str());
    words_are_the_agents_own(reply, story.is_some(), host)?;
    let text = match story {
        None => reply.to_string(),
        Some(draft) => {
            // Keyed on the thread's own URL, which is what makes a retry find the
            // story it already filed rather than open a second one.
            let permalink = fresh
                .items
                .iter()
                .find(|t| t.id == thread_id)
                .and_then(|t| t.comments.first())
                .map(|c| c.url.clone())
                .unwrap_or_default();
            let wanted = [crate::story::Wanted {
                thread_id: thread_id.to_string(),
                draft: draft.clone(),
                permalink,
            }];
            let filed = crate::story::file_all(app, pr, &wanted).await;
            match filed.get(thread_id) {
                Some(Ok(f)) => with_story_id(reply, &f.story),
                other => return Ok(Posted::HeldNoStory(no_story_reason(other))),
            }
        }
    };
    send_reply_once(forge, at, thread_id, &text, fresh).await
}

/// Refuse words that carry a story the daemon did not file.
///
/// Two shapes, both from the time the review skill filed its own stories and
/// substituted the link itself while saying "the daemon checks the URL" — which
/// nothing on this path did. `{story}` with no story to file would reach GitHub
/// as the literal token, and a link to the tracker in the agent's own words is a
/// story nobody here filed or checked: [`StoryRef::new`] is what holds an id and
/// its URL together, and only a link the daemon substitutes has been through it.
///
/// [`StoryRef::new`]: crate::model::StoryRef::new
fn words_are_the_agents_own(reply: &str, has_story: bool, host: Option<&str>) -> Result<()> {
    if !has_story && reply.contains(crate::proposal::STORY_TOKEN) {
        anyhow::bail!(
            "the reply names {} but no story came with it; send the story and the daemon \
             files it and puts its link in",
            crate::proposal::STORY_TOKEN
        );
    }
    if let Some(host) = host.filter(|h| !h.is_empty()) {
        if links_to(reply, host) {
            anyhow::bail!(
                "the reply links to {host} itself; a story link is the daemon's to put in — \
                 send the story with {} in the reply instead",
                crate::proposal::STORY_TOKEN
            );
        }
    }
    Ok(())
}

/// Whether `text` holds a URL on exactly `host`.
///
/// Matched on `://host` followed by the end of the authority, so a host that merely
/// starts with this one (`linear.app.example`) is not taken for it.
fn links_to(text: &str, host: &str) -> bool {
    let (text, needle) = (
        text.to_ascii_lowercase(),
        format!("://{}", host.to_ascii_lowercase()),
    );
    text.match_indices(&needle).any(|(at, m)| {
        text[at + m.len()..].chars().next().is_none_or(|c| {
            matches!(c, '/' | ':' | '?' | '#' | ')' | ']' | '>') || c.is_whitespace()
        })
    })
}

/// Put the filed story's id into a reply.
///
/// One of the two rules the whole design rests on — [`STORY_TOKEN`] must never
/// reach GitHub — so it has one implementation, called by the single-thread path
/// and by the batch's loop. `replace`, not `replacen`: a reply that names the
/// story twice stays consistent.
///
/// [`STORY_TOKEN`]: crate::proposal::STORY_TOKEN
fn with_story_id(reply: &str, story: &crate::model::StoryRef) -> String {
    reply.replace(crate::proposal::STORY_TOKEN, &story.link())
}

/// Why a reply that needs a story id cannot go out.
///
/// Shared so both paths hold a reply back for the same stated reason. `None` is
/// unreachable rather than expected: `file_all` answers for every thread it is
/// given.
fn no_story_reason(filed: Option<&std::result::Result<crate::story::Filed, String>>) -> String {
    match filed {
        Some(Err(e)) => e.clone(),
        _ => "the story run answered nothing for this thread".to_string(),
    }
}

/// Post a reply unless that exact text is already on the thread.
///
/// The other shared rule: a repeated call must not answer twice. Both callers want
/// the distinction between "sent" and "was already there" — the batch to report it
/// and a run to say so on the card — so it is a [`Posted`], not a bool.
async fn send_reply_once(
    forge: &ForgeImpl,
    at: &Path,
    thread_id: &str,
    text: &str,
    fresh: &Threads,
) -> Result<Posted> {
    if already_replied(fresh, thread_id, text) {
        return Ok(Posted::AlreadyThere);
    }
    let root = fresh
        .root_for(thread_id)
        .with_context(|| format!("thread {thread_id} has no comment to answer"))?;
    let (f, at, body) = (forge.clone(), at.to_path_buf(), text.to_string());
    tokio::task::spawn_blocking(move || f.reply(&at, &root, &body))
        .await
        .context("the write panicked")??;
    Ok(Posted::Sent)
}

/// 👍 one thread, the plain-adoption case. `false` when it was already answered.
///
/// The reaction half of [`post_one`]'s job, split off because the two are chosen
/// by stance and never both: `Agree` reacts and says nothing, `Reply`/`Story`
/// write words.
///
/// **On the last comment**, because that is the one [`forge::model::answered`]
/// reads: it went on the opening comment, so a 👍 for a reviewer's follow-up left
/// the thread waiting. And not at all on a thread that is already answered — its
/// last word is yours, or already carries your 👍 — because reacting to your own
/// comment says nothing, and a retry is then a no-op rather than a second write.
///
/// [`forge::model::answered`]: crate::forge::model::answered
pub(crate) async fn react_one(
    forge: &ForgeImpl,
    at: &Path,
    thread_id: &str,
    fresh: &Threads,
) -> Result<bool> {
    let thread = fresh
        .items
        .iter()
        .find(|t| t.id == thread_id)
        .with_context(|| format!("thread {thread_id} is not on the PR"))?;
    if !thread.answerable {
        return Ok(false);
    }
    let last = fresh
        .last_for(thread_id)
        .with_context(|| format!("thread {thread_id} has no comment to react to"))?;
    let (f, at) = (forge.clone(), at.to_path_buf());
    tokio::task::spawn_blocking(move || f.thumbs_up(&at, &last))
        .await
        .context("the write panicked")??;
    Ok(true)
}

fn already_replied(fresh: &Threads, thread_id: &str, body: &str) -> bool {
    let want = forge::with_footer(body);
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::forge::Thread;
    use crate::proposal::StoryDraft;
    use crate::testutil::{comment, thread};

    fn fetched(items: Vec<Thread>) -> Threads {
        Threads {
            pr: 10001,
            viewer: "viewer".into(),
            head_sha: Some("abc123".into()),
            items,
        }
    }

    #[test]
    fn a_reply_already_in_the_thread_is_not_posted_twice() {
        // This is the whole of retry: derive what is missing from a fresh fetch
        // rather than remember what was sent.
        let mut t = thread("PRRT_1", Some("a.ts"), Some(1), "alice");
        t.comments
            .push(comment(101, "viewer", &forge::with_footer("Fixed.")));
        let fresh = fetched(vec![t]);

        assert!(already_replied(&fresh, "PRRT_1", "Fixed."));
        // Different words are a different reply, and should go.
        assert!(!already_replied(&fresh, "PRRT_1", "Fixed in the mapper."));
    }

    #[test]
    fn someone_elses_identical_comment_does_not_count_as_our_reply() {
        let mut t = thread("PRRT_1", Some("a.ts"), Some(1), "alice");
        t.comments
            .push(comment(101, "alice", &forge::with_footer("Fixed.")));
        assert!(!already_replied(&fetched(vec![t]), "PRRT_1", "Fixed."));
    }

    async fn app() -> Arc<AppState> {
        crate::testutil::app("post-one").0
    }

    /// A forge is needed to call `post_one`, but these cases all return before any
    /// write, so it is never used — an empty repo is enough and nothing goes out.
    fn no_write_forge() -> ForgeImpl {
        ForgeImpl::for_kind(crate::config::ForgeKind::GitHub, "o", "n", String::new())
    }

    /// The resolve run reaches GitHub one thread at a time, so idempotency cannot
    /// be a batch-level property: a retried `…/committed` must not say it twice.
    #[tokio::test]
    async fn a_reply_already_on_the_thread_is_not_posted_again() {
        let app = app().await;
        let mut t = thread("PRRT_1", Some("a.ts"), Some(1), "alice");
        // The viewer's own answer, footed the way the forge writes it.
        t.comments
            .push(comment(101, "viewer", &crate::forge::with_footer("Fixed.")));
        let fresh = fetched(vec![t]);

        let out = post_one(
            &app,
            &no_write_forge(),
            &app.cfg.main_checkout.clone(),
            10001,
            "PRRT_1",
            "Fixed.",
            None,
            &fresh,
        )
        .await
        .unwrap();
        assert_eq!(out, Posted::AlreadyThere);
    }

    /// The token is why the story is filed first. A cached story answers without
    /// spawning the filer, which is what makes this testable at all — and the
    /// substituted text is what the idempotency check then compares, so seeing
    /// `AlreadyThere` proves the link went in.
    #[tokio::test]
    async fn the_story_token_is_replaced_by_the_link_before_anything_is_posted() {
        let app = app().await;
        // Through the constructor, which is the only way in now: the pair has to
        // hang together before anything can render it as a link.
        let story = crate::model::StoryRef::new("sc-1", "https://tracker/story/1", "tracker")
            .expect("a consistent pair");
        app.inner.write().await.with_stories("a test fixture", |c| {
            c.put(10001, "PRRT_1", story.clone());
            true
        });

        let mut t = thread("PRRT_1", Some("a.ts"), Some(1), "alice");
        t.comments.push(comment(
            101,
            "viewer",
            &crate::forge::with_footer(&format!("Tracked as: {}", story.link())),
        ));
        let fresh = fetched(vec![t]);

        let draft = StoryDraft {
            title: "t".into(),
            body: "b".into(),
        };
        let out = post_one(
            &app,
            &no_write_forge(),
            &app.cfg.main_checkout.clone(),
            10001,
            "PRRT_1",
            &format!("Tracked as: {}", crate::proposal::STORY_TOKEN),
            Some(&draft),
            &fresh,
        )
        .await
        .unwrap();
        // Matched the substituted body, so `{story}` never reached the write.
        assert_eq!(out, Posted::AlreadyThere);
    }

    /// A thread already answered gets no 👍: its last word is yours, so a reaction
    /// would be on your own comment, and a retry has nothing left to do. Returns
    /// before any write, which is what lets a forge with no repo stand in.
    #[tokio::test]
    async fn an_answered_thread_is_not_reacted_to_again() {
        let mut t = thread("PRRT_1", Some("a.ts"), Some(1), "alice");
        t.comments.push(comment(101, "viewer", "Done."));
        t.answerable = false;
        let fresh = fetched(vec![t]);
        let reacted = react_one(&no_write_forge(), Path::new("/"), "PRRT_1", &fresh)
            .await
            .expect("an answered thread is not an error");
        assert!(!reacted);
    }

    /// The two shapes of a story the daemon did not file, refused before anything
    /// is fetched or written. Both used to pass: the skill filed and linked its own
    /// stories, and the route posted whatever words it was given.
    #[test]
    fn a_reply_may_not_carry_a_story_the_daemon_did_not_file() {
        let host = Some("app.shortcut.com");
        let token = format!("Tracked as {}.", crate::proposal::STORY_TOKEN);
        assert!(words_are_the_agents_own(&token, false, host).is_err());
        assert!(words_are_the_agents_own(&token, true, host).is_ok());

        let linked = "Tracked as [sc-1](https://app.shortcut.com/acme/story/1).";
        let err = words_are_the_agents_own(linked, false, host).unwrap_err();
        assert!(err.to_string().contains("app.shortcut.com"), "{err}");
        assert!(words_are_the_agents_own(linked, true, host).is_err());

        // A host that only begins with the tracker's is not the tracker.
        let other = "See https://app.shortcut.com.example/x for the spec.";
        assert!(words_are_the_agents_own(other, false, host).is_ok());
        // And no tracker means there is no host to recognise.
        assert!(words_are_the_agents_own(linked, false, None).is_ok());
        assert!(words_are_the_agents_own("Fixed.", false, host).is_ok());
    }

    /// A thread the fetch no longer carries has no comment to answer, and that is
    /// an error rather than a silent skip.
    #[tokio::test]
    async fn a_vanished_thread_refuses_rather_than_posting_nowhere() {
        let app = app().await;
        let fresh = fetched(vec![thread("PRRT_9", Some("a.ts"), Some(1), "alice")]);
        let err = post_one(
            &app,
            &no_write_forge(),
            &app.cfg.main_checkout.clone(),
            10001,
            "PRRT_1",
            "Fixed.",
            None,
            &fresh,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("PRRT_1"), "{err}");
    }
}
