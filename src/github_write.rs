//! Posting back to a PR: thread replies, 👍, and re-requesting a review.
//!
//! Everything here shells `gh` rather than going through [`crate::github`]'s
//! curl transport. §6 built that path around a read-only PAT; these are writes,
//! and `gh` already holds a credential that can make them. The unified auth
//! story is a later pass — until then the read path keeps its token and the
//! write path borrows gh's.
//!
//! What this module deliberately cannot do:
//!
//! - **Resolve a thread.** Closing a conversation is the comment author's
//!   button, not ours. There is no `resolveReviewThread` call here and there
//!   should not be one.
//! - **Approve, merge, or open a PR.** Nothing that carries the user's judgement
//!   beyond the reply text they approved.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Appended to every written reply the daemon posts, on its own line.
///
/// A reaction never carries it — a 👍 is not authored text, and a footer on one
/// would be noise. The triage agent is told not to add this itself, so if a
/// draft already ends with it the agent has misbehaved and [`with_footer`]
/// swallows the duplicate rather than posting it twice.
pub const FOOTER: &str = "(via orchestrator)";

/// A reply body with the disclosure footer on the end.
pub fn with_footer(body: &str) -> String {
    let body = body.trim_end();
    if body.lines().last().map(str::trim) == Some(FOOTER) {
        return body.to_string();
    }
    // A blank line, so GitHub renders it as its own paragraph rather than
    // running it onto the last sentence.
    format!("{body}\n\n{FOOTER}")
}

/// Where writes are aimed.
pub struct Target {
    /// The main checkout. `gh` is run here so it picks up the same auth and
    /// config the rest of the daemon's shelling does.
    pub cwd: PathBuf,
    pub owner: String,
    pub name: String,
}

impl Target {
    fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Reply in an existing review thread.
    ///
    /// Keyed on the **first comment's** REST `databaseId`, not the GraphQL
    /// thread id: the replies sub-resource is what threads a comment, whereas a
    /// bare `POST .../comments` would open a new conversation alongside the one
    /// being answered.
    pub fn reply(&self, pr: u64, comment_id: u64, body: &str) -> Result<Value> {
        let body = with_footer(body);
        // An empty reply is always a bug upstream — a blank comment is worse
        // than no comment, and it cannot be deleted from here.
        if body.trim() == FOOTER {
            bail!("refusing to post an empty reply to comment {comment_id}");
        }
        self.api(
            &reply_path(&self.owner, &self.name, pr, comment_id),
            Some(&json!({ "body": body })),
        )
    }

    /// 👍 a comment. The plain-adoption case: applied as asked, nothing to add.
    pub fn thumbs_up(&self, comment_id: u64) -> Result<Value> {
        self.api(
            &reaction_path(&self.owner, &self.name, comment_id),
            Some(&json!({ "content": "+1" })),
        )
    }

    /// Ask a reviewer to look again.
    ///
    /// Per reviewer, not per PR: one whose every thread is addressed is
    /// re-requested even while another's are still open.
    pub fn rerequest(&self, pr: u64, login: &str) -> Result<()> {
        let out = self
            .gh(&[
                "pr",
                "edit",
                &pr.to_string(),
                "--repo",
                &self.slug(),
                "--add-reviewer",
                login,
            ])
            .output()
            .context("spawning gh pr edit")?;
        if !out.status.success() {
            bail!(
                "re-requesting {login} on #{pr} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// `gh api <path>`, with a JSON body on stdin when there is one.
    ///
    /// The body goes over `--input -` rather than as `-f` pairs: reply text is
    /// multi-line Dutch prose, and an argument would put it in the process
    /// table and mangle its newlines besides.
    fn api(&self, path: &str, body: Option<&Value>) -> Result<Value> {
        let mut args = vec!["api", path];
        if body.is_some() {
            args.extend_from_slice(&["--input", "-"]);
        }

        let mut child = self
            .gh(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning gh api")?;
        if let Some(b) = body {
            child
                .stdin
                .as_mut()
                .context("gh stdin")?
                .write_all(b.to_string().as_bytes())?;
        }
        // Dropping stdin closes it; without this `gh` waits on EOF forever.
        drop(child.stdin.take());

        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "gh api {path} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        serde_json::from_slice(&out.stdout).context("parsing the gh api response")
    }

    fn gh(&self, args: &[&str]) -> Command {
        let mut c = Command::new("gh");
        c.args(args).current_dir(&self.cwd);
        c
    }
}

/// Which reviewers have nothing of theirs left open.
///
/// `open` is the logins still holding an unaddressed thread; `all` is everyone
/// who reviewed. A reviewer is re-requested only when none of their threads
/// remain — a draft reply does not count as addressed, so the caller must pass
/// what was actually posted.
pub fn ready_to_rerequest<'a>(all: &[&'a str], open: &[&str]) -> Vec<&'a str> {
    let mut out: Vec<&str> = all
        .iter()
        .copied()
        .filter(|login| !open.contains(login))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The path `gh api` is given for a thread reply. Split out to be testable.
pub fn reply_path(owner: &str, name: &str, pr: u64, comment_id: u64) -> String {
    format!("repos/{owner}/{name}/pulls/{pr}/comments/{comment_id}/replies")
}

/// The path `gh api` is given for a reaction. Note it is **not** nested under
/// the PR: reactions hang off the review comment directly.
pub fn reaction_path(owner: &str, name: &str, comment_id: u64) -> String {
    format!("repos/{owner}/{name}/pulls/comments/{comment_id}/reactions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_footer_goes_on_its_own_paragraph() {
        assert_eq!(
            with_footer("Fixed in the mapper."),
            "Fixed in the mapper.\n\n(via orchestrator)"
        );
    }

    #[test]
    fn trailing_whitespace_does_not_push_the_footer_down_the_page() {
        assert_eq!(
            with_footer("Klopt.\n\n\n   "),
            "Klopt.\n\n(via orchestrator)"
        );
    }

    #[test]
    fn a_draft_that_already_has_the_footer_does_not_get_a_second() {
        // The triage prompt tells the agent not to add one; this is the
        // backstop for when it does anyway.
        let once = with_footer("Klopt.");
        assert_eq!(with_footer(&once), once);
        assert_eq!(once.matches(FOOTER).count(), 1);
    }

    #[test]
    fn a_reply_path_threads_under_the_comment_it_answers() {
        // The bare `pulls/:n/comments` endpoint would open a new conversation
        // instead of replying in the existing one.
        assert_eq!(
            reply_path("acme-org", "acme", 10001, 9000000001),
            "repos/acme/monorepo/pulls/10001/comments/9000000001/replies"
        );
    }

    #[test]
    fn a_reaction_path_is_not_nested_under_the_pr() {
        assert_eq!(
            reaction_path("acme-org", "acme", 9000000001),
            "repos/acme/monorepo/pulls/comments/9000000001/reactions"
        );
    }

    #[test]
    fn only_reviewers_with_nothing_open_are_re_requested() {
        // Alice's five handled while Bob's two are open re-requests Alice
        // alone.
        assert_eq!(
            ready_to_rerequest(&["alice", "bob"], &["bob"]),
            vec!["alice"]
        );
        assert!(ready_to_rerequest(&["bob"], &["bob"]).is_empty());
        assert_eq!(
            ready_to_rerequest(&["alice", "alice"], &[]),
            vec!["alice"]
        );
    }
}
