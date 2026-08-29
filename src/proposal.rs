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
//! `forge::model::ThreadRoot`). Nothing in this module trusts a field because it
//! parsed.
//!
//! `Proposal`, deliberately not `Finding`: [`crate::findings::Finding`] already
//! owns that word for the conditions the daemon writes to its `daemon.log`.

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

/// What you are saying back to the reviewer.
///
/// One of the three things decided per thread, and deliberately only that. It
/// used to be `Does`, which bundled the stance together with whether code gets
/// written and who writes it — so "agree, and I will fix it by hand" had no
/// spelling. Code is now simply whether the position carries a patch, and who
/// writes it is [`Mode`], chosen by the human rather than proposed by the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stance {
    /// The reviewer is right: make the change they asked for, and answer with a
    /// thumbs up on the thread's opening comment rather than words.
    ///
    /// "No words" is the whole of what this stance withholds — it does *not* mean
    /// no code. Agreeing and then changing nothing tells a reviewer they were
    /// right while leaving the code as it was, which is the one outcome the
    /// session prompt names as never acceptable.
    Agree,
    /// Answer in words.
    Reply,
    /// A fair point, out of scope here: file a tracker story and reply with its
    /// id.
    Story,
}

impl Stance {
    /// Does this stance put words on the thread?
    pub fn writes_reply(self) -> bool {
        matches!(self, Stance::Reply | Stance::Story)
    }
    /// A reaction on the thread's opening comment.
    pub fn gives_thumbs_up(self) -> bool {
        matches!(self, Stance::Agree)
    }
    pub fn files_story(self) -> bool {
        matches!(self, Stance::Story)
    }
}

/// Who writes the code a decision implies.
///
/// The human's call at triage time, not the agent's, which is why it lives on
/// the decision rather than on a position. It is meaningful on any stance: a
/// thread you agree with can still be one you want to fix by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The session applies the staged fix, adapting it if the branch moved.
    #[default]
    Agent,
    /// You write it, live, while the session waits.
    Manual,
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
    pub stance: Stance,
    /// A unified diff, exactly as `git diff` printed it in the agent's scratch
    /// worktree. Its presence is what "this position changes code" means; there
    /// is no second field that could disagree with it.
    #[serde(default)]
    pub patch: Option<String>,
    /// Present iff `stance.writes_reply()`. For a story position it must contain
    /// `{story}`, substituted once the story exists.
    #[serde(default)]
    pub reply: Option<String>,
    #[serde(default)]
    pub story: Option<StoryDraft>,
}

impl Position {
    /// Whether accepting this position writes to the worktree.
    pub fn writes_code(&self) -> bool {
        self.patch.as_deref().is_some_and(|p| !p.trim().is_empty())
    }
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
    /// Does this position's stance agree with the fields it carries?
    ///
    /// This is the check that keeps "do A but say B" impossible. The words and
    /// the code travel together in one position precisely so they cannot
    /// diverge; if the agent hands back a `story` stance with no story, that
    /// guarantee is already broken before anything renders.
    ///
    /// The patch is deliberately not checked against the stance any more: any
    /// stance may carry a fix, including `agree`, and its presence is the only
    /// statement that code changes. There is no second field left to disagree.
    fn check(&self, thread: &str, index: usize) -> Result<()> {
        let where_ = format!("thread {thread} position {index}");
        if self.label.trim().is_empty() {
            bail!("{where_}: empty label");
        }
        let has_reply = self.reply.as_deref().is_some_and(|r| !r.trim().is_empty());
        if self.stance.writes_reply() != has_reply {
            bail!(
                "{where_}: stance is {:?} but reply is {}",
                self.stance,
                if has_reply { "present" } else { "missing" }
            );
        }
        if self.stance.files_story() {
            match &self.story {
                None => bail!("{where_}: `story` stance without a story"),
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
        if self.positions.iter().any(|p| p.writes_code())
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
        self.positions.iter().any(|p| p.writes_code())
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
            // `Manual` used to be appended here as a position of its own. It is a
            // `Mode` now: writing the code by hand is a fact about who does the
            // work, not about what you are saying back, and as a position it made
            // "agree, and I will fix it myself" unspellable.
            p.positions.push(Position {
                label: "Something else".into(),
                // Not "no code": the box is an instruction to the session, which may
                // well write code from it. What is yours here is the direction.
                sub: "Custom instructions".into(),
                stance: Stance::Reply,
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

    fn pos(stance: Stance) -> Position {
        fixing(stance, false)
    }

    /// A position with, or without, a fix attached. Two axes now, so the helper
    /// takes two.
    fn fixing(stance: Stance, patch: bool) -> Position {
        Position {
            label: "L".into(),
            sub: "s".into(),
            stance,
            patch: patch.then(|| "diff --git a/f b/f\n".to_string()),
            reply: stance.writes_reply().then(|| {
                if stance.files_story() {
                    format!("see {STORY_TOKEN}")
                } else {
                    "words".to_string()
                }
            }),
            story: stance.files_story().then(|| StoryDraft {
                title: "t".into(),
                body: "b".into(),
            }),
        }
    }

    fn proposal(id: &str, stance: Stance) -> Proposal {
        Proposal {
            thread_id: id.into(),
            continued: false,
            read: "it is right".into(),
            verified: Some("grep showed one call".into()),
            recommend: 0,
            positions: vec![pos(stance)],
        }
    }

    fn set(ps: Vec<Proposal>) -> ProposalSet {
        ProposalSet {
            base_sha: "abc123".into(),
            proposals: ps,
        }
    }

    #[test]
    fn the_daemon_appends_say_something_else_and_nothing_more() {
        // `Manual` used to be appended here too. It is a `Mode` now, so the tail
        // is one position, not two.
        let out = set(vec![proposal("T1", Stance::Reply)])
            .validate(&["T1".into()])
            .unwrap();
        let labels: Vec<&str> = out.proposals[0]
            .positions
            .iter()
            .map(|p| p.label.as_str())
            .collect();
        assert_eq!(labels, vec!["L", "Something else"]);
    }

    #[test]
    fn a_fix_may_ride_any_stance_including_agree() {
        // The combination `Does` could not spell: agree with the reviewer and
        // still carry the change. Nothing about the stance forbids a patch.
        let mut p = proposal("T1", Stance::Agree);
        p.positions[0] = fixing(Stance::Agree, true);
        let out = set(vec![p]).validate(&["T1".into()]).expect("valid");
        assert!(out.proposals[0].positions[0].writes_code());
        assert!(out.proposals[0].changes_code());
    }

    #[test]
    fn a_stance_must_agree_with_the_words() {
        // An `agree` position that also carries words is the do-A-say-B bug
        // arriving as data: a thumbs up posts nothing to read.
        let mut p = proposal("T2", Stance::Agree);
        p.positions[0].reply = Some("words".into());
        let err = set(vec![p])
            .validate(&["T2".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("reply is present"), "{err}");

        // And the other way: words promised, none written.
        let mut p = proposal("T1", Stance::Reply);
        p.positions[0].reply = None;
        let err = set(vec![p])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("reply is missing"), "{err}");
    }

    #[test]
    fn a_story_reply_must_leave_room_for_the_id() {
        let mut p = proposal("T1", Stance::Story);
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
        let mut p = proposal("T1", Stance::Story);
        p.positions[0].story = Some(StoryDraft {
            title: "t".repeat(MAX_TITLE + 1),
            body: "b".into(),
        });
        let err = set(vec![p])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("title exceeds"), "{err}");

        let mut p = proposal("T1", Stance::Story);
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
        let mut p = proposal("T1", Stance::Agree);
        p.positions[0] = fixing(Stance::Agree, true);
        p.verified = None;
        let err = set(vec![p])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("re-proved"), "{err}");

        // A words-only proposal needs none.
        let mut p = proposal("T2", Stance::Reply);
        p.verified = None;
        assert!(set(vec![p]).validate(&["T2".into()]).is_ok());
    }

    #[test]
    fn a_thread_nobody_asked_about_is_refused() {
        let err = set(vec![proposal("GHOST", Stance::Reply)])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not among the threads"), "{err}");
    }

    #[test]
    fn a_silently_dropped_thread_is_named() {
        // The one failure the human cannot see on the cards.
        let err = set(vec![proposal("T1", Stance::Reply)])
            .validate(&["T1".into(), "T2".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no proposal for T2"), "{err}");
    }

    #[test]
    fn two_proposals_for_one_thread_are_refused() {
        let err = set(vec![
            proposal("T1", Stance::Reply),
            proposal("T1", Stance::Reply),
        ])
        .validate(&["T1".into()])
        .unwrap_err()
        .to_string();
        assert!(err.contains("two proposals"), "{err}");
    }

    #[test]
    fn recommend_must_point_at_a_real_position() {
        let mut p = proposal("T1", Stance::Reply);
        p.recommend = 3;
        let err = set(vec![p])
            .validate(&["T1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("recommends position 3"), "{err}");
    }

    #[test]
    fn oversized_fields_are_refused() {
        let mut p = proposal("T1", Stance::Reply);
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
                "stance": "agree", "patch": "diff --git a/f b/f\n", "reply": null }
            ]
          }]
        }"#;
        let s: ProposalSet = serde_json::from_str(json).unwrap();
        assert_eq!(s.proposals[0].positions[0].stance, Stance::Agree);
        assert!(s.proposals[0].changes_code());
        assert!(s.validate(&["T1".into()]).is_ok());
    }
}
