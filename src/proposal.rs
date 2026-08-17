//! What a triage run hands back, and why it cannot be trusted.
//!
//! The agent reads every review thread, works out what it would do, and POSTs a
//! [`ProposalSet`] — one [`Proposal`] per thread, each offering two to four
//! [`Position`]s. A position is a *complete* answer: the stance, the code, and
//! the words together, so picking one cannot produce "change A but say B".
//!
//! **This is hostile input.** Not because the agent is adversarial, but because
//! its own input is: review comments are written by other people, and a model
//! reading them can be confused by them. So the shape is validated here, and the
//! thread ids are validated against a fresh fetch before anything is posted (see
//! `github_write::ThreadRoot`). Nothing in this module trusts a field because it
//! parsed.
//!
//! `Proposal`, deliberately not `Finding`: [`crate::todo::Finding`] already owns
//! that word for the conditions the daemon writes into `TODO.md`.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

/// Longest agent-supplied string accepted in any one field.
///
/// `read`, `verified` and a reply are prose, and a patch is a diff for one
/// thread's fix — none has a legitimate reason to be large. An unbounded field
/// from a subprocess is a memory footgun, and the diff viewer already sets the
/// precedent of capping rather than trusting (§5's eager 2000-line cap).
pub const MAX_FIELD: usize = 64 * 1024;

/// A tracker title is one line. Long enough for a real sentence in Dutch,
/// short enough that a runaway generation is refused rather than filed.
const MAX_TITLE: usize = 256;

/// How many positions one thread may offer, before the daemon appends its own.
///
/// The card shows them as a list you scan; past a handful the choice stops being
/// a choice. Two is the floor because a single option is not a decision.
const MAX_POSITIONS: usize = 4;
const MIN_POSITIONS: usize = 1;

/// What accepting a position actually does. The card renders this as the
/// right-hand label, so the option that rewrites a file cannot look as cheap as
/// the one that posts a reaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Does {
    /// Agree, say nothing: a thumbs up on the thread's opening comment.
    #[serde(rename = "thumbsup")]
    ThumbsUp,
    /// Words only, no code.
    #[serde(rename = "reply")]
    Reply,
    /// Write the patch, and thumbs up rather than write a reply.
    #[serde(rename = "change+thumbsup")]
    ChangeThumbsUp,
    /// Write the patch and reply.
    #[serde(rename = "change+reply")]
    ChangeReply,
    /// File a tracker story and reply with its id — for a fair point that is out
    /// of scope here.
    #[serde(rename = "story+reply")]
    StoryReply,
    /// You will write the code by hand. Appended by the daemon, never by the
    /// agent; the comment is written in the manual phase, after the fact.
    #[serde(rename = "manual")]
    Manual,
}

impl Does {
    pub fn writes_code(self) -> bool {
        matches!(self, Does::ChangeThumbsUp | Does::ChangeReply)
    }
    pub fn writes_reply(self) -> bool {
        matches!(
            self,
            Does::Reply | Does::ChangeReply | Does::StoryReply | Does::Manual
        )
    }
    /// A reaction on the thread's opening comment. The two `thumbsup` variants
    /// differ in whether they also write code, never in what they post.
    pub fn gives_thumbs_up(self) -> bool {
        matches!(self, Does::ThumbsUp | Does::ChangeThumbsUp)
    }
    pub fn files_story(self) -> bool {
        matches!(self, Does::StoryReply)
    }
    /// Appended by the daemon rather than proposed by the agent.
    pub fn is_daemon_supplied(self) -> bool {
        matches!(self, Does::Manual)
    }
}

/// A story to file, when a position defers the point rather than answering it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoryDraft {
    pub title: String,
    pub body: String,
}

/// One complete way of answering a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub label: String,
    /// The line under the label — why you would pick this one.
    #[serde(default)]
    pub sub: String,
    pub does: Does,
    /// A unified diff, exactly as `git diff` printed it in the agent's scratch
    /// worktree. Present iff `does.writes_code()`.
    #[serde(default)]
    pub patch: Option<String>,
    /// Present iff `does.writes_reply()`. For a story position it must contain
    /// `{story}`, substituted once the story exists.
    #[serde(default)]
    pub reply: Option<String>,
    #[serde(default)]
    pub story: Option<StoryDraft>,
}

/// The placeholder a story reply must carry, since the id does not exist until
/// the story is filed.
pub const STORY_TOKEN: &str = "{story}";

/// One thread's triage: what the agent made of it, and the ways out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub thread_id: String,
    /// You already replied here and the reviewer came back. The card flags it,
    /// because a new reply has to stay consistent with what you said before.
    #[serde(default)]
    pub continued: bool,
    /// The agent's assessment: is the reviewer right, what breaks either way.
    pub read: String,
    /// The command it ran in its scratch worktree and what that showed. Absent
    /// only when no position changes code — there is nothing to re-prove.
    #[serde(default)]
    pub verified: Option<String>,
    /// Index into `positions` of the one to pre-select.
    pub recommend: usize,
    /// Named `positions` on the wire too; the agent's schema calls them options,
    /// which collides with `Option`.
    #[serde(alias = "options")]
    pub positions: Vec<Position>,
}

/// A whole triage run's output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalSet {
    /// The PR head the patches were generated against. Re-checked before
    /// writing: a force-push in between invalidates every diff.
    pub base_sha: String,
    #[serde(alias = "threads")]
    pub proposals: Vec<Proposal>,
}

impl Position {
    /// Does this position's `does` agree with the fields it carries?
    ///
    /// This is the check that keeps "do A but say B" impossible. The UI merges
    /// stance, code and words into one control precisely so they cannot diverge;
    /// if the agent hands back a `change+reply` with no patch, that guarantee is
    /// already broken before anything renders.
    fn check(&self, thread: &str, index: usize) -> Result<()> {
        let where_ = format!("thread {thread} position {index}");
        if self.label.trim().is_empty() {
            bail!("{where_}: empty label");
        }
        if self.does.is_daemon_supplied() {
            bail!(
                "{where_}: `{:?}` is appended by the daemon and must not be proposed",
                self.does
            );
        }
        let has_patch = self.patch.as_deref().is_some_and(|p| !p.trim().is_empty());
        if self.does.writes_code() != has_patch {
            bail!(
                "{where_}: `does` is {:?} but patch is {}",
                self.does,
                if has_patch { "present" } else { "missing" }
            );
        }
        let has_reply = self.reply.as_deref().is_some_and(|r| !r.trim().is_empty());
        if self.does.writes_reply() != has_reply {
            bail!(
                "{where_}: `does` is {:?} but reply is {}",
                self.does,
                if has_reply { "present" } else { "missing" }
            );
        }
        if self.does.files_story() {
            match &self.story {
                None => bail!("{where_}: `story+reply` without a story"),
                Some(s) if s.title.trim().is_empty() => {
                    bail!("{where_}: story has no title")
                }
                // A tracker title is a one-line summary and Shortcut has its own
                // limit; 64KB of it is nonsense, so it is bounded separately from
                // the prose fields.
                Some(s) if s.title.len() > MAX_TITLE => {
                    bail!("{where_}: story title exceeds {MAX_TITLE} bytes")
                }
                Some(s) if s.body.len() > MAX_FIELD => {
                    bail!("{where_}: story body exceeds {MAX_FIELD} bytes")
                }
                Some(_) => {}
            }
            // The id cannot be known yet, so the reply has to leave room for it.
            if !self.reply.as_deref().unwrap_or("").contains(STORY_TOKEN) {
                bail!("{where_}: story reply must contain {STORY_TOKEN}");
            }
        } else if self.story.is_some() {
            bail!("{where_}: story on a position that does not file one");
        }
        for (name, field) in [
            ("patch", self.patch.as_deref()),
            ("reply", self.reply.as_deref()),
        ] {
            if field.is_some_and(|f| f.len() > MAX_FIELD) {
                bail!("{where_}: {name} exceeds {MAX_FIELD} bytes");
            }
        }
        Ok(())
    }
}

impl Proposal {
    fn check(&self) -> Result<()> {
        if self.thread_id.trim().is_empty() {
            bail!("a proposal has no thread_id");
        }
        let id = &self.thread_id;
        if !(MIN_POSITIONS..=MAX_POSITIONS).contains(&self.positions.len()) {
            bail!(
                "thread {id}: {} positions, expected {MIN_POSITIONS}–{MAX_POSITIONS}",
                self.positions.len()
            );
        }
        if self.recommend >= self.positions.len() {
            bail!(
                "thread {id}: recommends position {} of {}",
                self.recommend,
                self.positions.len()
            );
        }
        if self.read.trim().is_empty() {
            bail!("thread {id}: empty read — the card has nothing to show");
        }
        if self.read.len() > MAX_FIELD {
            bail!("thread {id}: read exceeds {MAX_FIELD} bytes");
        }
        for (i, p) in self.positions.iter().enumerate() {
            p.check(id, i)?;
        }
        // A position that changes code makes a claim about the code, and the
        // whole point of the scratch-worktree pass is that the claim was run.
        if self.positions.iter().any(|p| p.does.writes_code())
            && !self
                .verified
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
        {
            bail!("thread {id}: proposes a change with no evidence it was re-proved");
        }
        Ok(())
    }

    /// Whether any position would write to the worktree.
    pub fn changes_code(&self) -> bool {
        self.positions.iter().any(|p| p.does.writes_code())
    }
}

impl ProposalSet {
    /// Reject anything malformed, then append the two positions the daemon owns.
    ///
    /// `answerable` is the thread ids the fetch said still want an answer. A
    /// proposal for a thread not in that set is refused rather than ignored: the
    /// agent has either hallucinated an id or is working from a stale fetch, and
    /// posting to it would be a write against a thread nobody looked at.
    ///
    /// A thread in `answerable` with no proposal is the one failure the human
    /// cannot see, so it is named too.
    pub fn validate(mut self, answerable: &[String]) -> Result<Self> {
        if self.base_sha.trim().is_empty() {
            bail!("no base_sha: nothing to check the patches against");
        }
        for p in &self.proposals {
            p.check()?;
        }

        let mut seen: Vec<&str> = Vec::new();
        for p in &self.proposals {
            if seen.contains(&p.thread_id.as_str()) {
                bail!("thread {}: two proposals for one thread", p.thread_id);
            }
            seen.push(&p.thread_id);
            if !answerable.iter().any(|a| a == &p.thread_id) {
                bail!(
                    "thread {}: not among the threads awaiting an answer",
                    p.thread_id
                );
            }
        }
        let missing: Vec<&str> = answerable
            .iter()
            .map(String::as_str)
            .filter(|a| !seen.contains(a))
            .collect();
        if !missing.is_empty() {
            bail!("no proposal for {}", missing.join(", "));
        }

        for p in &mut self.proposals {
            p.positions.push(Position {
                label: "Manual".into(),
                sub: "you write the code — comment after you have".into(),
                does: Does::Manual,
                patch: None,
                // Written in the manual phase, once the work exists.
                reply: Some(String::new()),
                story: None,
            });
            p.positions.push(Position {
                label: "Say something else".into(),
                sub: "your words, your stance — no code".into(),
                does: Does::Reply,
                patch: None,
                reply: Some(String::new()),
                story: None,
            });
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(does: Does) -> Position {
        Position {
            label: "L".into(),
            sub: "s".into(),
            does,
            patch: does
                .writes_code()
                .then(|| "diff --git a/f b/f\n".to_string()),
            reply: does.writes_reply().then(|| {
                if does.files_story() {
                    format!("see {STORY_TOKEN}")
                } else {
                    "words".to_string()
                }
            }),
            story: does.files_story().then(|| StoryDraft {
                title: "t".into(),
                body: "b".into(),
            }),
        }
    }

    fn proposal(id: &str, does: Does) -> Proposal {
        Proposal {
            thread_id: id.into(),
            continued: false,
            read: "it is right".into(),
            verified: Some("grep showed one call".into()),
            recommend: 0,
            positions: vec![pos(does)],
        }
    }

    fn set(ps: Vec<Proposal>) -> ProposalSet {
        ProposalSet {
            base_sha: "abc123".into(),
            proposals: ps,
        }
    }

    #[test]
    fn the_daemon_appends_manual_and_say_something_else() {
        let out = set(vec![proposal("T1", Does::ChangeReply)])
            .validate(&["T1".into()])
            .unwrap();
        let labels: Vec<&str> = out.proposals[0]
            .positions
            .iter()
            .map(|p| p.label.as_str())
            .collect();
        assert_eq!(labels, vec!["L", "Manual", "Say something else"]);
    }

    #[test]
    fn an_agent_may_not_propose_the_daemons_own_positions() {
        // One source of truth for the fixed tail, or the two drift.
        let mut p = proposal("T1", Does::Reply);
        p.positions.push(pos(Does::Manual));
        let err = set(vec![p])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("appended by the daemon"), "{err}");
    }

    #[test]
    fn does_must_agree_with_the_fields() {
        // "change + reply" with no patch is the do-A-say-B bug arriving as data.
        let mut p = proposal("T1", Does::ChangeReply);
        p.positions[0].patch = None;
        let err = set(vec![p])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("patch is missing"), "{err}");

        // A thumbs-up-only position that also carries words.
        let mut p = proposal("T2", Does::ChangeThumbsUp);
        p.positions[0].reply = Some("words".into());
        let err = set(vec![p])
            .validate(&["T2".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("reply is present"), "{err}");
    }

    #[test]
    fn a_story_reply_must_leave_room_for_the_id() {
        let mut p = proposal("T1", Does::StoryReply);
        p.positions[0].reply = Some("Tracked as sc-12345.".into());
        let err = set(vec![p])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains(STORY_TOKEN), "{err}");
    }

    #[test]
    fn story_fields_are_bounded_like_every_other_agent_string() {
        // These were the last two with no cap. A title is one line, so it gets a
        // tighter one than the prose fields.
        let mut p = proposal("T1", Does::StoryReply);
        p.positions[0].story = Some(StoryDraft {
            title: "t".repeat(MAX_TITLE + 1),
            body: "b".into(),
        });
        let err = set(vec![p])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("title exceeds"), "{err}");

        let mut p = proposal("T1", Does::StoryReply);
        p.positions[0].story = Some(StoryDraft {
            title: "t".into(),
            body: "b".repeat(MAX_FIELD + 1),
        });
        let err = set(vec![p])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("body exceeds"), "{err}");
    }

    #[test]
    fn a_change_without_evidence_is_refused() {
        // The scratch-worktree pass exists so the claim was run, not reasoned
        // about; a patch with no `verified` skipped it.
        let mut p = proposal("T1", Does::ChangeThumbsUp);
        p.verified = None;
        let err = set(vec![p])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("re-proved"), "{err}");

        // A words-only proposal needs none.
        let mut p = proposal("T2", Does::Reply);
        p.verified = None;
        assert!(set(vec![p]).validate(&["T2".into()]).is_ok());
    }

    #[test]
    fn a_thread_nobody_asked_about_is_refused() {
        let err = set(vec![proposal("GHOST", Does::Reply)])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not among the threads"), "{err}");
    }

    #[test]
    fn a_silently_dropped_thread_is_named() {
        // The one failure the human cannot see on the cards.
        let err = set(vec![proposal("T1", Does::Reply)])
            .validate(&["T1".into(), "T2".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no proposal for T2"), "{err}");
    }

    #[test]
    fn two_proposals_for_one_thread_are_refused() {
        let err = set(vec![
            proposal("T1", Does::Reply),
            proposal("T1", Does::Reply),
        ])
        .validate(&["T1".into()])
        .unwrap_err()
        .to_string();
        assert!(err.contains("two proposals"), "{err}");
    }

    #[test]
    fn recommend_must_point_at_a_real_position() {
        let mut p = proposal("T1", Does::Reply);
        p.recommend = 3;
        let err = set(vec![p])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("recommends position 3"), "{err}");
    }

    #[test]
    fn oversized_fields_are_refused() {
        let mut p = proposal("T1", Does::ChangeReply);
        p.positions[0].patch = Some("x".repeat(MAX_FIELD + 1));
        let err = set(vec![p])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn the_agents_own_field_names_deserialize() {
        // The prompt says `threads` and `options`; the Rust side says
        // `proposals` and `positions` to avoid colliding with `Option`.
        let json = r#"{
          "base_sha": "abc",
          "threads": [{
            "thread_id": "T1",
            "read": "r",
            "verified": "v",
            "recommend": 0,
            "options": [
              { "label": "Apply", "sub": "respond with thumbs up",
                "does": "change+thumbsup", "patch": "diff --git a/f b/f\n", "reply": null }
            ]
          }]
        }"#;
        let s: ProposalSet = serde_json::from_str(json).unwrap();
        assert_eq!(s.proposals[0].positions[0].does, Does::ChangeThumbsUp);
        assert!(s.proposals[0].changes_code());
        assert!(s.validate(&["T1".into()]).is_ok());
    }
}
