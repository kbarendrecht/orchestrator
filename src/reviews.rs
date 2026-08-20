//! The review queue — the PRs waiting on you to review (§6b).
//!
//! Candidates come from the forge ([`crate::forge::Forge::review_candidates`]);
//! everything opinionated — which PRs count, how they rank, what blocks them —
//! is here and is config-driven ([`ReviewRanking`]), so it is the same whatever
//! forge produced them and a repo that reviews differently can say so without a
//! patch. There is no external command: the daemon asks the forge directly.
//!
//! **Coverage** is the one choice that changes *what* is fetched. `requested`
//! (the default) asks only for PRs where your review is requested. `all_open`
//! walks every open PR and keeps the ones you have not already settled — the
//! shape a repo uses when review is a shared pool rather than a personal
//! assignment.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::SystemTime;

use crate::forge::{Checks, Forge, ReviewCandidate, ReviewRef};

/// One row in the queue, ranked and ready to render.
#[derive(Debug, Clone, Serialize)]
pub struct Review {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    /// Age in hours, derived from the PR's creation time.
    pub age_hours: f64,
    /// The rank the ranking rules assigned. Lower is more urgent. Kept for the
    /// sort and for anyone reading the API; the SPA colours off [`Review::tone`]
    /// rather than off magic numbers here.
    pub prio: u32,
    pub needs_re_review: bool,
    pub is_draft: bool,
    /// `conflicts`, `failing checks`, … — non-empty means it is waiting on
    /// someone else and sinks to the `blocked` list.
    pub blockers: Vec<String>,
    /// Distinct humans who have already reviewed — a review-cost tiebreak.
    pub reviewers: u32,
    /// Review cost. `None` if the forge did not report a file count.
    pub changed_files: Option<u32>,
    /// Why this row is where it is — the matched rule's reason, or the blockers,
    /// shown verbatim in the queue. Computed here so the SPA does not reimplement
    /// the rule.
    pub reason: String,
    /// The dot colour the SPA should draw. Decoupled from `prio` so a custom
    /// ranking cannot silently break the colouring.
    pub tone: Tone,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ReviewQueue {
    pub login: String,
    pub actionable: Vec<Review>,
    pub blocked: Vec<Review>,
    pub total: u32,
    pub skipped: u32,
}

/// A pane that cannot be trusted must say so.
///
/// Silently showing zero reviews when the source is broken is the one failure
/// that would actually cost a colleague a day (§6b), so a forge error lands in
/// `Degraded` rather than in an empty queue.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewState {
    Ok(ReviewQueue),
    Degraded { reason: String },
    /// Before the first poll lands. Distinct from `Degraded` so startup does
    /// not read as a broken source — and so it never becomes a TODO entry.
    Pending,
    /// No forge repo resolves for this checkout, so there is nothing to query.
    Off,
}

impl Default for ReviewState {
    fn default() -> Self {
        ReviewState::Pending
    }
}

// ---------------------------------------------------------------------------
// The ranking engine (forge-agnostic)
// ---------------------------------------------------------------------------

/// Which PRs the review queue fetches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    /// Only PRs where your review is requested (direct or via a team). Lean and
    /// the default.
    #[default]
    Requested,
    /// Every open PR, minus the ones you have already settled — a shared-pool
    /// queue. One query per open-PR page, so it is opt-in per repo.
    AllOpen,
}

/// How the review queue is filtered, classified and ordered. Config, not
/// hardcoded, so a repo with different labels, bots or priorities does not need
/// a patched daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRanking {
    #[serde(default)]
    pub coverage: Coverage,
    /// A PR carrying any of these labels is not review work and never appears.
    #[serde(default)]
    pub skip_labels: Vec<String>,
    /// Logins whose reviews do not count toward the reviewer tally or "someone
    /// has looked". Any login ending in `[bot]` is excluded regardless.
    #[serde(default)]
    pub bot_reviewers: Vec<String>,
    /// **First match wins, so order is precedence.** Each candidate takes the
    /// rank of the first rule whose `when` it satisfies; a trailing `always`
    /// rule is the catch-all. A demotion (a `sidequest` label) belongs *after*
    /// the request rules so a requested sidequest still ranks as a request.
    #[serde(default = "default_rules")]
    pub rules: Vec<Rule>,
    /// A candidate matching any of these is waiting on its author, not on you:
    /// it goes to the `blocked` list carrying the named blocker.
    #[serde(default = "default_blocked")]
    pub blocked_when: Vec<BlockedRule>,
    /// Ties within a rank, applied in order.
    #[serde(default = "default_tiebreak")]
    pub tiebreak: Vec<TieKey>,
}

impl Default for ReviewRanking {
    fn default() -> Self {
        ReviewRanking {
            coverage: Coverage::default(),
            skip_labels: Vec::new(),
            bot_reviewers: Vec::new(),
            rules: default_rules(),
            blocked_when: default_blocked(),
            tiebreak: default_tiebreak(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub when: Predicate,
    /// Lower is more urgent.
    pub rank: u32,
    /// Shown in the queue's "why" column. Falls back to a derived reason when
    /// absent.
    #[serde(default)]
    pub reason: Option<String>,
    /// The dot colour. Derived from the candidate when absent.
    #[serde(default)]
    pub tone: Option<Tone>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockedRule {
    pub when: Predicate,
    pub blocker: String,
}

/// A condition over one candidate. Every field that is set must hold (implicit
/// AND); an unset field does not constrain. An empty predicate matches nothing —
/// use `always` for a catch-all, which is explicit so a typo cannot match every
/// PR.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Predicate {
    #[serde(default)]
    pub always: bool,
    #[serde(default)]
    pub label: Option<String>,
    /// The PR must **not** carry this label. Lets a blocked rule carve out an
    /// escape (a "known-unrelated red check" label, say).
    #[serde(default)]
    pub without_label: Option<String>,
    #[serde(default)]
    pub requested: Option<Requested>,
    #[serde(default)]
    pub re_review: Option<bool>,
    #[serde(default)]
    pub draft: Option<bool>,
    /// `MERGEABLE` / `CONFLICTING` / `UNKNOWN`.
    #[serde(default)]
    pub mergeable: Option<String>,
    /// `passing` / `failing` / `pending` / `unknown`.
    #[serde(default)]
    pub checks: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
}

impl Predicate {
    fn matches(&self, c: &ReviewCandidate, re_review: bool) -> bool {
        if self.always {
            return true;
        }
        let mut constrained = false;
        let mut check = |cond: bool| -> bool {
            constrained = true;
            cond
        };
        if let Some(l) = &self.label {
            if !check(c.labels.iter().any(|x| x == l)) {
                return false;
            }
        }
        if let Some(l) = &self.without_label {
            if !check(!c.labels.iter().any(|x| x == l)) {
                return false;
            }
        }
        if let Some(r) = self.requested {
            let hit = match r {
                Requested::Personal => c.requested_personally,
                Requested::Team => c.requested_team,
                Requested::Any => c.requested_personally || c.requested_team,
            };
            if !check(hit) {
                return false;
            }
        }
        if let Some(b) = self.re_review {
            if !check(re_review == b) {
                return false;
            }
        }
        if let Some(b) = self.draft {
            if !check(c.is_draft == b) {
                return false;
            }
        }
        if let Some(m) = &self.mergeable {
            if !check(c.mergeable.eq_ignore_ascii_case(m)) {
                return false;
            }
        }
        if let Some(s) = &self.checks {
            if !check(checks_name(&c.checks).eq_ignore_ascii_case(s)) {
                return false;
            }
        }
        if let Some(a) = &self.author {
            if !check(&c.author == a) {
                return false;
            }
        }
        constrained
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requested {
    Personal,
    Team,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TieKey {
    /// Fewer prior reviewers first — cheaper to be the one who unblocks it.
    FewestReviewers,
    /// Oldest first — it has waited longest.
    Oldest,
    /// Newest first.
    Newest,
}

/// The dot colour a row gets. `snake_case` so the SPA reads `"prio"` etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    /// Red: somebody's release is waiting on you.
    Prio,
    /// Amber: a re-request, waiting on you rather than on a colleague.
    Rereview,
    /// Neutral.
    Normal,
}

fn checks_name(c: &Checks) -> &'static str {
    match c {
        Checks::Passing => "passing",
        Checks::Failing => "failing",
        Checks::Pending => "pending",
        Checks::Unknown => "unknown",
    }
}

/// Generic, label-free defaults — no repo's label names are assumed. A repo with
/// a `prio`/`stopper` convention (or a shared-pool `all_open` queue) puts its own
/// rules in `config.json`; these just rank requests above re-reviews above the
/// rest.
fn default_rules() -> Vec<Rule> {
    let requested = |r: Requested, rank: u32, reason: Option<&str>| Rule {
        when: Predicate {
            requested: Some(r),
            ..Default::default()
        },
        rank,
        reason: reason.map(String::from),
        tone: None,
    };
    vec![
        requested(Requested::Personal, 0, None),
        requested(Requested::Team, 1, Some("team")),
        Rule {
            when: Predicate {
                re_review: Some(true),
                ..Default::default()
            },
            rank: 2,
            reason: None, // the re-review overlay names it
            tone: None,
        },
        Rule {
            when: Predicate {
                always: true,
                ..Default::default()
            },
            rank: 3,
            reason: None,
            tone: None,
        },
    ]
}

/// For other people's PRs, only conflicts and a red rollup are blockers; a draft
/// is filtered out of the queue entirely, and a changes-requested review is the
/// author's work, not yours.
fn default_blocked() -> Vec<BlockedRule> {
    vec![
        BlockedRule {
            when: Predicate {
                mergeable: Some("CONFLICTING".to_string()),
                ..Default::default()
            },
            blocker: "conflicts".to_string(),
        },
        BlockedRule {
            when: Predicate {
                checks: Some("failing".to_string()),
                ..Default::default()
            },
            blocker: "failing checks".to_string(),
        },
    ]
}

fn default_tiebreak() -> Vec<TieKey> {
    vec![TieKey::Oldest, TieKey::FewestReviewers]
}

// ---------------------------------------------------------------------------
// Fetch + classify
// ---------------------------------------------------------------------------

/// Ask the forge for candidates (in the configured coverage) and rank them.
/// `Degraded` on a forge error — never an empty queue. The `Off`/`Pending`
/// states are the caller's: they depend on whether a repo resolves.
pub fn fetch(forge: &impl Forge, ranking: &ReviewRanking) -> ReviewState {
    let all_open = ranking.coverage == Coverage::AllOpen;
    match forge.review_candidates(all_open) {
        Ok((login, cands)) => ReviewState::Ok(rank(login, &cands, ranking, SystemTime::now())),
        Err(e) => ReviewState::Degraded {
            reason: format!("{e:#}"),
        },
    }
}

/// What ranking derives about one candidate from its raw reviews, given the
/// viewer and the bot list. Kept together so include/rank/tiebreak all read the
/// same numbers.
struct Derived {
    /// PR author ≠ you and you have reviewed it before.
    re_review: bool,
    /// Your standing review is an approval — it settles the PR for you.
    approved: bool,
    /// You reviewed the exact commit that is now head — nothing new to look at.
    reviewed_current_head: bool,
    /// Distinct humans who have reviewed, bots and dismissals excluded.
    reviewers: u32,
}

fn is_bot(login: &str, bots: &[String]) -> bool {
    login.ends_with("[bot]") || bots.iter().any(|b| b == login)
}

fn derive(c: &ReviewCandidate, viewer: &str, bots: &[String]) -> Derived {
    let mine = |r: &ReviewRef| r.author == viewer;
    let re_review = c.author != viewer && c.reviews.iter().any(mine);
    let approved = c.reviews.iter().any(|r| mine(r) && r.state == "APPROVED");
    let reviewed_current_head = c.head_oid.as_deref().is_some_and(|head| {
        c.reviews
            .iter()
            .any(|r| mine(r) && r.commit_oid.as_deref() == Some(head))
    });
    // Distinct authors whose review currently counts.
    let mut seen = HashSet::new();
    for r in &c.reviews {
        if r.state == "DISMISSED" || r.state == "PENDING" {
            continue;
        }
        if is_bot(&r.author, bots) || r.author == c.author {
            continue;
        }
        seen.insert(r.author.as_str());
    }
    Derived {
        re_review,
        approved,
        reviewed_current_head,
        reviewers: seen.len() as u32,
    }
}

/// Whether this candidate is review work for `viewer`.
///
/// Never your own PR, never a draft, never one wearing a skip label. Beyond
/// that, `requested` coverage trusts the forge's filter and keeps everything;
/// `all_open` keeps a PR only while it still wants your eyes — a standing
/// personal request always does, otherwise not once you have approved it or
/// reviewed its current head.
fn is_review_work(c: &ReviewCandidate, viewer: &str, d: &Derived, r: &ReviewRanking) -> bool {
    if c.author == viewer || c.is_draft {
        return false;
    }
    if c.labels.iter().any(|l| r.skip_labels.iter().any(|s| s == l)) {
        return false;
    }
    match r.coverage {
        Coverage::Requested => true,
        Coverage::AllOpen => {
            c.requested_personally || (!d.approved && !d.reviewed_current_head)
        }
    }
}

/// The pure core: candidates + ranking + a clock → a ranked queue. Split from
/// [`fetch`] so it can be tested without a forge or the wall clock.
fn rank(login: String, cands: &[ReviewCandidate], ranking: &ReviewRanking, now: SystemTime) -> ReviewQueue {
    let total = cands.len() as u32;
    let mut actionable = Vec::new();
    let mut blocked = Vec::new();
    let mut kept = 0u32;

    for c in cands {
        let d = derive(c, &login, &ranking.bot_reviewers);
        if !is_review_work(c, &login, &d, ranking) {
            continue;
        }
        kept += 1;

        let blockers: Vec<String> = ranking
            .blocked_when
            .iter()
            .filter(|b| b.when.matches(c, d.re_review))
            .map(|b| b.blocker.clone())
            .collect();

        let matched = ranking.rules.iter().find(|rule| rule.when.matches(c, d.re_review));
        let prio = matched.map(|rule| rule.rank).unwrap_or(u32::MAX);
        // The pane's precedence, kept exactly: red for a prio rule, then amber
        // for a re-review, then the rule's own tone or neutral.
        let tone = if matched.and_then(|r| r.tone) == Some(Tone::Prio) {
            Tone::Prio
        } else if d.re_review {
            Tone::Rereview
        } else {
            matched.and_then(|r| r.tone).unwrap_or(Tone::Normal)
        };
        // And the pane's "why": blockers, then the re-review note, then the
        // matched rule's reason.
        let reason = if !blockers.is_empty() {
            blockers.join(", ")
        } else if d.re_review {
            "re-requested".to_string()
        } else {
            matched.and_then(|r| r.reason.clone()).unwrap_or_default()
        };

        let review = Review {
            number: c.number,
            title: c.title.clone(),
            url: c.url.clone(),
            author: c.author.clone(),
            age_hours: age_hours_from(&c.created_at, now),
            prio,
            needs_re_review: d.re_review,
            is_draft: c.is_draft,
            reviewers: d.reviewers,
            changed_files: c.changed_files,
            reason,
            tone,
            blockers: blockers.clone(),
        };

        if blockers.is_empty() {
            actionable.push(review);
        } else {
            blocked.push(review);
        }
    }

    sort(&mut actionable, &ranking.tiebreak);
    sort(&mut blocked, &ranking.tiebreak);

    ReviewQueue {
        login,
        actionable,
        blocked,
        total,
        skipped: total - kept,
    }
}

fn sort(rows: &mut [Review], tiebreak: &[TieKey]) {
    rows.sort_by(|a, b| {
        a.prio.cmp(&b.prio).then_with(|| {
            for key in tiebreak {
                let ord = match key {
                    TieKey::FewestReviewers => a.reviewers.cmp(&b.reviewers),
                    // Older first: a larger age sorts earlier.
                    TieKey::Oldest => b.age_hours.total_cmp(&a.age_hours),
                    TieKey::Newest => a.age_hours.total_cmp(&b.age_hours),
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        })
    });
}

/// Hours between an ISO-8601 UTC timestamp (`2026-08-20T12:34:56Z`) and `now`.
///
/// Parsed by hand rather than pulling in a date crate — the daemon shells out
/// instead of linking heavy dependencies elsewhere, and GitHub's format is
/// fixed. An unparseable stamp is `0.0` (reads as brand new), the safe
/// direction: it sorts to the bottom of an oldest-first tiebreak rather than
/// jumping the queue.
fn age_hours_from(created_at: &str, now: SystemTime) -> f64 {
    let Some(created) = epoch_secs(created_at) else {
        return 0.0;
    };
    let now = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    ((now - created).max(0) as f64) / 3600.0
}

/// Seconds since the Unix epoch for `YYYY-MM-DDTHH:MM:SS[.frac][Z]`, or `None`.
fn epoch_secs(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;

    let time = time.trim_end_matches('Z');
    let time = time.split('.').next().unwrap_or(time);
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let sec: i64 = t.next().unwrap_or("0").parse().ok()?;

    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm). Valid
/// for any Gregorian date and needs no leap-year special-casing at the call site.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(number: u64) -> ReviewCandidate {
        ReviewCandidate {
            number,
            title: format!("PR {number}"),
            url: format!("https://example.com/pull/{number}"),
            author: "someone".into(),
            created_at: "2026-08-01T00:00:00Z".into(),
            is_draft: false,
            mergeable: "MERGEABLE".into(),
            checks: Checks::Passing,
            labels: vec![],
            requested_personally: false,
            requested_team: false,
            changed_files: None,
            head_oid: Some("HEAD".into()),
            reviews: vec![],
        }
    }

    fn review(author: &str, state: &str, oid: &str) -> ReviewRef {
        ReviewRef {
            author: author.into(),
            state: state.into(),
            commit_oid: Some(oid.into()),
        }
    }

    fn ranked(cands: &[ReviewCandidate]) -> ReviewQueue {
        rank(
            "me".into(),
            cands,
            &ReviewRanking::default(),
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000),
        )
    }

    #[test]
    fn a_personal_request_outranks_a_team_request() {
        let mut personal = cand(1);
        personal.requested_personally = true;
        let mut team = cand(2);
        team.requested_team = true;
        let q = ranked(&[team, personal]);
        assert_eq!(q.actionable[0].number, 1, "personal first");
        assert_eq!(q.actionable[1].number, 2);
    }

    #[test]
    fn conflicts_and_failing_checks_block_but_a_draft_is_dropped_entirely() {
        let mut conflicting = cand(1);
        conflicting.mergeable = "CONFLICTING".into();
        conflicting.requested_personally = true;
        let mut failing = cand(2);
        failing.checks = Checks::Failing;
        failing.requested_personally = true;
        let mut draft = cand(3);
        draft.is_draft = true;
        draft.requested_personally = true;
        let mut ok = cand(4);
        ok.requested_personally = true;
        let q = ranked(&[conflicting, failing, draft, ok]);
        assert_eq!(q.actionable.len(), 1);
        assert_eq!(q.actionable[0].number, 4);
        assert_eq!(q.blocked.len(), 2, "conflicts + failing, the draft is not here");
        assert!(q.blocked.iter().all(|r| r.number != 3), "draft filtered out, not blocked");
    }

    #[test]
    fn a_re_review_is_amber_and_derived_from_your_prior_review() {
        let mut c = cand(1);
        c.requested_personally = true;
        c.reviews = vec![review("me", "CHANGES_REQUESTED", "old")]; // you looked at an older head
        let q = ranked(&[c]);
        let row = &q.actionable[0];
        assert!(row.needs_re_review);
        assert_eq!(row.tone, Tone::Rereview);
        assert_eq!(row.reason, "re-requested");
    }

    #[test]
    fn reviewers_excludes_bots_dismissed_and_the_author() {
        let mut c = cand(1);
        c.author = "erin".into();
        c.requested_personally = true;
        c.reviews = vec![
            review("ada", "APPROVED", "x"),
            review("ada", "COMMENTED", "x"),   // same human, still one
            review("acme-bot[bot]", "APPROVED", "x"), // bot suffix
            review("erin", "COMMENTED", "x"), // the author
            review("bob", "DISMISSED", "x"),  // no longer counts
        ];
        let ranking = ReviewRanking::default();
        let d = derive(&c, "me", &ranking.bot_reviewers);
        assert_eq!(d.reviewers, 1, "only ada counts");
    }

    #[test]
    fn all_open_keeps_a_standing_request_but_drops_approved_and_seen_head() {
        let ranking = ReviewRanking {
            coverage: Coverage::AllOpen,
            ..Default::default()
        };
        // Approved by you → settled, dropped.
        let mut approved = cand(1);
        approved.reviews = vec![review("me", "APPROVED", "old")];
        // You reviewed the current head → nothing new, dropped.
        let mut seen = cand(2);
        seen.reviews = vec![review("me", "COMMENTED", "HEAD")];
        // You reviewed an older head → still wants you, kept (re-review).
        let mut moved = cand(3);
        moved.reviews = vec![review("me", "COMMENTED", "old")];
        // Never touched, not requested → still review work in a shared pool.
        let fresh = cand(4);
        // Approved, but a standing personal request means they asked again → kept.
        let mut re_asked = cand(5);
        re_asked.requested_personally = true;
        re_asked.reviews = vec![review("me", "APPROVED", "old")];

        let q = rank("me".into(), &[approved, seen, moved, fresh, re_asked], &ranking, SystemTime::now());
        let nums: Vec<u64> = q.actionable.iter().map(|r| r.number).collect();
        assert!(!nums.contains(&1), "approved is settled");
        assert!(!nums.contains(&2), "current head already seen");
        assert!(nums.contains(&3), "older head still wants you");
        assert!(nums.contains(&4), "shared-pool PR is review work");
        assert!(nums.contains(&5), "a standing request outranks your stale approval");
    }

    #[test]
    fn requested_coverage_keeps_even_a_seen_head() {
        // In requested mode the forge already scoped it; don't second-guess.
        let mut c = cand(1);
        c.requested_personally = true;
        c.reviews = vec![review("me", "APPROVED", "HEAD")];
        let q = ranked(&[c]);
        assert_eq!(q.actionable.len(), 1);
    }

    #[test]
    fn a_skip_label_removes_a_pr_from_the_queue() {
        let ranking = ReviewRanking {
            skip_labels: vec!["on-hold".into()],
            ..Default::default()
        };
        let mut c = cand(1);
        c.requested_personally = true;
        c.labels = vec!["on-hold".into()];
        let q = rank("me".into(), &[c], &ranking, SystemTime::now());
        assert!(q.actionable.is_empty());
        assert_eq!(q.skipped, 1);
    }

    #[test]
    fn oldest_then_fewest_reviewers_breaks_a_rank_tie() {
        // Same rank (both personal). Default tiebreak is oldest first, then fewer
        // reviewers — the order the real queue used, not reviewers-first.
        let mut old_busy = cand(1);
        old_busy.requested_personally = true;
        old_busy.created_at = "2026-01-01T00:00:00Z".into(); // older
        old_busy.reviews = vec![review("ada", "COMMENTED", "x"), review("bob", "COMMENTED", "x")];
        let mut newer_quiet = cand(2);
        newer_quiet.requested_personally = true;
        newer_quiet.created_at = "2026-07-01T00:00:00Z".into(); // newer, no reviewers
        let q = ranked(&[newer_quiet, old_busy]);
        assert_eq!(q.actionable[0].number, 1, "older wins the tie even with more reviewers");
    }

    // A acme-shaped rule set, to prove the config can reproduce it.
    fn acme_ranking() -> ReviewRanking {
        let label = |l: &str, rank: u32, reason: Option<&str>, tone: Option<Tone>| Rule {
            when: Predicate { label: Some(l.into()), ..Default::default() },
            rank,
            reason: reason.map(String::from),
            tone,
        };
        let requested = |r: Requested, rank: u32, reason: Option<&str>| Rule {
            when: Predicate { requested: Some(r), ..Default::default() },
            rank,
            reason: reason.map(String::from),
            tone: None,
        };
        ReviewRanking {
            coverage: Coverage::AllOpen,
            skip_labels: vec!["on-hold".into()],
            bot_reviewers: vec!["acme-bot".into()],
            rules: vec![
                label("Prio Stopper", 0, Some("prio stopper"), Some(Tone::Prio)),
                label("Prio", 1, Some("prio"), Some(Tone::Prio)),
                requested(Requested::Personal, 2, None),
                requested(Requested::Team, 3, Some("team requested")),
                Rule { when: Predicate { re_review: Some(true), ..Default::default() }, rank: 4, reason: None, tone: None },
                label("sidequest :compass:", 6, Some("sidequest"), None),
                Rule { when: Predicate { always: true, ..Default::default() }, rank: 5, reason: None, tone: None },
            ],
            blocked_when: vec![
                BlockedRule { when: Predicate { mergeable: Some("CONFLICTING".into()), ..Default::default() }, blocker: "conflicts".into() },
                BlockedRule {
                    when: Predicate {
                        checks: Some("failing".into()),
                        without_label: Some("Failed check can be ignored".into()),
                        ..Default::default()
                    },
                    blocker: "failing checks".into(),
                },
            ],
            tiebreak: vec![TieKey::Oldest, TieKey::FewestReviewers],
        }
    }

    #[test]
    fn a_requested_sidequest_ranks_as_a_request_not_sunk() {
        // acme's whole point: being asked personally beats the label that
        // would otherwise sink it. Sidequest is last in the ladder.
        let r = acme_ranking();
        let mut side_requested = cand(1);
        side_requested.labels = vec!["sidequest :compass:".into()];
        side_requested.requested_personally = true;
        let mut side_alone = cand(2);
        side_alone.labels = vec!["sidequest :compass:".into()];
        // Reaches the queue in all_open as a shared-pool PR (nothing settled).
        let q = rank("me".into(), &[side_alone, side_requested], &r, SystemTime::now());
        let by = |n: u64| q.actionable.iter().find(|x| x.number == n).unwrap();
        assert_eq!(by(1).prio, 2, "a requested sidequest ranks as the request");
        assert_eq!(by(2).prio, 6, "an unrequested sidequest sinks to the bottom");
        assert!(by(1).prio < by(2).prio);
    }

    #[test]
    fn a_team_request_ranks_above_a_re_review() {
        let r = acme_ranking();
        let mut team = cand(1);
        team.requested_team = true;
        let mut re = cand(2);
        re.reviews = vec![review("me", "COMMENTED", "old")]; // re-review, not requested
        let q = rank("me".into(), &[re, team], &r, SystemTime::now());
        assert_eq!(q.actionable[0].number, 1, "team-request (3) above re-review (4)");
    }

    #[test]
    fn the_ignore_label_lets_a_red_check_through_as_actionable() {
        let r = acme_ranking();
        let mut red = cand(1);
        red.requested_personally = true;
        red.checks = Checks::Failing;
        red.labels = vec!["Failed check can be ignored".into()];
        let q = rank("me".into(), &[red], &r, SystemTime::now());
        assert_eq!(q.actionable.len(), 1, "the escape label keeps it actionable");
        assert!(q.blocked.is_empty());
    }

    #[test]
    fn age_is_computed_from_created_at() {
        let now = SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(epoch_secs("2026-08-03T00:00:00Z").unwrap() as u64);
        let h = age_hours_from("2026-08-01T00:00:00Z", now);
        assert!((h - 48.0).abs() < 0.01, "got {h}");
    }

    #[test]
    fn epoch_matches_a_known_instant() {
        assert_eq!(epoch_secs("2001-09-09T01:46:40Z"), Some(1_000_000_000));
        assert_eq!(epoch_secs("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn an_empty_predicate_matches_nothing_but_always_matches_all() {
        let c = cand(1);
        assert!(!Predicate::default().matches(&c, false));
        assert!(Predicate { always: true, ..Default::default() }.matches(&c, false));
    }

    #[test]
    fn the_default_state_is_pending_not_degraded() {
        assert!(matches!(ReviewState::default(), ReviewState::Pending));
    }

    #[test]
    fn an_old_config_ranking_still_parses_with_only_some_fields() {
        // Forward-compat: a config written before coverage/skip_labels existed.
        let r: ReviewRanking = serde_json::from_str(r#"{"tiebreak":["newest"]}"#).expect("parse");
        assert_eq!(r.coverage, Coverage::Requested);
        assert!(!r.rules.is_empty(), "rules defaulted");
        assert_eq!(r.tiebreak, vec![TieKey::Newest]);
    }
}
