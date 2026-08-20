//! The review queue — PRs where your review is requested (§6b).
//!
//! The candidates come from the forge ([`crate::forge::Forge::review_candidates`]);
//! the *ranking* is here and is config-driven ([`ReviewRanking`]), so it is the
//! same whatever forge produced them and so a repo that ranks review work
//! differently can say so without a patch. There is no external command any more:
//! the daemon asks the forge directly, and a checkout with no resolvable repo
//! reads as `Off`, not as a broken tool.

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

use crate::forge::{Checks, Forge, ReviewCandidate};

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
    /// Not a fault — like `Pending` it never becomes a TODO finding, and the
    /// pane says "not configured" rather than "unavailable".
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

/// How review candidates are classified and ordered. Lives in config so a repo
/// with different labels or priorities does not need a patched daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRanking {
    /// **First match wins, so order is precedence.** Each candidate takes the
    /// rank of the first rule whose `when` it satisfies; a trailing `always`
    /// rule is the catch-all. Because it is first-match, a demotion (a
    /// `sidequest` label) is placed *after* any label that should override it —
    /// the defaults list `stopper` before `sidequest` so a PR carrying both
    /// still ranks as a stopper.
    #[serde(default = "default_rules")]
    pub rules: Vec<Rule>,
    /// A candidate matching any of these is waiting on its author, not on you:
    /// it goes to the `blocked` list carrying the named blocker, and no ranking
    /// rule applies.
    #[serde(default = "default_blocked")]
    pub blocked_when: Vec<BlockedRule>,
    /// Ties within a rank, applied in order.
    #[serde(default = "default_tiebreak")]
    pub tiebreak: Vec<TieKey>,
}

impl Default for ReviewRanking {
    fn default() -> Self {
        ReviewRanking {
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
    /// The dot colour. Derived from the rank/candidate when absent.
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
/// use `always` for a catch-all, which is explicit on purpose so a typo cannot
/// accidentally match every PR.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Predicate {
    #[serde(default)]
    pub always: bool,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub requested: Option<Requested>,
    #[serde(default)]
    pub re_review: Option<bool>,
    #[serde(default)]
    pub draft: Option<bool>,
    #[serde(default)]
    pub changes_requested: Option<bool>,
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
    fn matches(&self, c: &ReviewCandidate) -> bool {
        if self.always {
            return true;
        }
        // An otherwise-empty predicate matches nothing.
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
            if !check(c.re_review == b) {
                return false;
            }
        }
        if let Some(b) = self.draft {
            if !check(c.is_draft == b) {
                return false;
            }
        }
        if let Some(b) = self.changes_requested {
            if !check(c.changes_requested == b) {
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

fn default_rules() -> Vec<Rule> {
    let rule = |when: Predicate, rank: u32, reason: Option<&str>, tone: Tone| Rule {
        when,
        rank,
        reason: reason.map(String::from),
        tone: Some(tone),
    };
    let label = |l: &str| Predicate {
        label: Some(l.to_string()),
        ..Default::default()
    };
    let requested = |r: Requested| Predicate {
        requested: Some(r),
        ..Default::default()
    };
    vec![
        rule(label("stopper"), 0, Some("prio stopper"), Tone::Prio),
        rule(label("prio"), 1, Some("prio"), Tone::Prio),
        // A demotion: after the labels that outrank it, before request type, so a
        // sidequest sinks even when it is requested of you.
        rule(label("sidequest"), 6, Some("sidequest"), Tone::Normal),
        rule(requested(Requested::Personal), 2, None, Tone::Normal),
        rule(
            Predicate {
                re_review: Some(true),
                ..Default::default()
            },
            4,
            Some("re-requested"),
            Tone::Rereview,
        ),
        rule(requested(Requested::Team), 3, Some("team"), Tone::Normal),
        rule(
            Predicate {
                always: true,
                ..Default::default()
            },
            5,
            None,
            Tone::Normal,
        ),
    ]
}

fn default_blocked() -> Vec<BlockedRule> {
    let blocked = |when: Predicate, blocker: &str| BlockedRule {
        when,
        blocker: blocker.to_string(),
    };
    vec![
        blocked(
            Predicate {
                draft: Some(true),
                ..Default::default()
            },
            "draft",
        ),
        blocked(
            Predicate {
                mergeable: Some("CONFLICTING".to_string()),
                ..Default::default()
            },
            "conflicts",
        ),
        blocked(
            Predicate {
                checks: Some("failing".to_string()),
                ..Default::default()
            },
            "failing checks",
        ),
        blocked(
            Predicate {
                changes_requested: Some(true),
                ..Default::default()
            },
            "changes requested",
        ),
    ]
}

fn default_tiebreak() -> Vec<TieKey> {
    vec![TieKey::FewestReviewers, TieKey::Oldest]
}

// ---------------------------------------------------------------------------
// Fetch + classify
// ---------------------------------------------------------------------------

/// Ask the forge for review candidates and rank them. `Degraded` on a forge
/// error — never an empty queue. The `Off`/`Pending` states are the caller's:
/// they depend on whether a repo resolves, which `fetch` is not given.
pub fn fetch(forge: &impl Forge, ranking: &ReviewRanking) -> ReviewState {
    match forge.review_candidates() {
        Ok((login, cands)) => ReviewState::Ok(rank(login, &cands, ranking, SystemTime::now())),
        Err(e) => ReviewState::Degraded {
            reason: format!("{e:#}"),
        },
    }
}

/// The pure core: candidates + ranking + a clock → a ranked queue. Split from
/// [`fetch`] so it can be tested without a forge or the wall clock.
fn rank(login: String, cands: &[ReviewCandidate], ranking: &ReviewRanking, now: SystemTime) -> ReviewQueue {
    let total = cands.len() as u32;
    let mut actionable = Vec::new();
    let mut blocked = Vec::new();

    for c in cands {
        let blockers: Vec<String> = ranking
            .blocked_when
            .iter()
            .filter(|b| b.when.matches(c))
            .map(|b| b.blocker.clone())
            .collect();

        let matched = ranking.rules.iter().find(|r| r.when.matches(c));
        let prio = matched.map(|r| r.rank).unwrap_or(u32::MAX);
        let tone = matched
            .and_then(|r| r.tone)
            .unwrap_or_else(|| derived_tone(prio, c));
        let reason = if !blockers.is_empty() {
            blockers.join(", ")
        } else {
            matched
                .and_then(|r| r.reason.clone())
                .unwrap_or_else(|| derived_reason(c))
        };

        let review = Review {
            number: c.number,
            title: c.title.clone(),
            url: c.url.clone(),
            author: c.author.clone(),
            age_hours: age_hours_from(&c.created_at, now),
            prio,
            needs_re_review: c.re_review,
            is_draft: c.is_draft,
            reviewers: c.reviewers,
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
        skipped: 0,
    }
}

/// When a rule sets no tone: red for the top ranks, amber for a re-request,
/// neutral otherwise. Mirrors what the SPA used to derive from `prio` itself.
fn derived_tone(prio: u32, c: &ReviewCandidate) -> Tone {
    if prio <= 1 {
        Tone::Prio
    } else if c.re_review {
        Tone::Rereview
    } else {
        Tone::Normal
    }
}

fn derived_reason(c: &ReviewCandidate) -> String {
    if c.re_review {
        "re-requested".to_string()
    } else if c.is_draft {
        "draft".to_string()
    } else {
        String::new()
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
/// fixed. An unparseable stamp is `0.0` (reads as brand new), which is the safe
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

    // Trim the zone/fraction: GitHub emits `Z`, but tolerate a fraction too.
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
            re_review: false,
            changes_requested: false,
            changed_files: None,
            reviewers: 0,
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
        let q = ranked(&[team.clone(), personal.clone()]);
        assert_eq!(q.actionable[0].number, 1, "personal first");
        assert_eq!(q.actionable[1].number, 2);
    }

    #[test]
    fn a_stopper_beats_a_sidequest_even_on_the_same_pr() {
        // First-match with `stopper` listed before `sidequest`: a PR carrying
        // both is a stopper, not a sidequest sunk to the bottom.
        let mut both = cand(1);
        both.labels = vec!["sidequest".into(), "stopper".into()];
        both.requested_personally = true;
        let plain = {
            let mut c = cand(2);
            c.requested_personally = true;
            c
        };
        let q = ranked(&[plain, both]);
        assert_eq!(q.actionable[0].number, 1, "the stopper leads");
        assert_eq!(q.actionable[0].prio, 0);
        assert_eq!(q.actionable[0].tone, Tone::Prio);
    }

    #[test]
    fn a_sidequest_sinks_below_an_ordinary_request() {
        let mut side = cand(1);
        side.labels = vec!["sidequest".into()];
        side.requested_personally = true;
        let mut ordinary = cand(2);
        ordinary.requested_team = true;
        let q = ranked(&[side, ordinary]);
        assert_eq!(q.actionable[0].number, 2, "the team request leads");
        assert_eq!(q.actionable[1].number, 1, "the sidequest sinks");
    }

    #[test]
    fn conflicts_and_failing_checks_go_to_the_blocked_list() {
        let mut conflicting = cand(1);
        conflicting.mergeable = "CONFLICTING".into();
        conflicting.requested_personally = true;
        let mut failing = cand(2);
        failing.checks = Checks::Failing;
        failing.requested_personally = true;
        let ok = {
            let mut c = cand(3);
            c.requested_personally = true;
            c
        };
        let q = ranked(&[conflicting, failing, ok]);
        assert_eq!(q.actionable.len(), 1);
        assert_eq!(q.actionable[0].number, 3);
        assert_eq!(q.blocked.len(), 2);
        assert!(q.blocked.iter().any(|r| r.blockers == vec!["conflicts"]));
        assert!(q
            .blocked
            .iter()
            .any(|r| r.blockers == vec!["failing checks"]));
    }

    #[test]
    fn fewest_reviewers_breaks_a_rank_tie_then_oldest() {
        let mut a = cand(1);
        a.requested_personally = true;
        a.reviewers = 2;
        a.created_at = "2026-01-01T00:00:00Z".into(); // older
        let mut b = cand(2);
        b.requested_personally = true;
        b.reviewers = 0;
        b.created_at = "2026-07-01T00:00:00Z".into(); // newer
        let q = ranked(&[a, b]);
        // Same rank (both personal); the one with fewer reviewers wins the tie.
        assert_eq!(q.actionable[0].number, 2);
        assert_eq!(q.actionable[1].number, 1);
    }

    #[test]
    fn oldest_breaks_a_tie_when_reviewer_counts_match() {
        let mut older = cand(1);
        older.requested_personally = true;
        older.created_at = "2026-01-01T00:00:00Z".into();
        let mut newer = cand(2);
        newer.requested_personally = true;
        newer.created_at = "2026-07-01T00:00:00Z".into();
        let q = ranked(&[newer, older]);
        assert_eq!(q.actionable[0].number, 1, "older first");
    }

    #[test]
    fn a_re_review_is_amber_and_ranks_below_a_team_first_request() {
        let mut re = cand(1);
        re.requested_team = true;
        re.re_review = true;
        let mut team = cand(2);
        team.requested_team = true;
        let q = ranked(&[re, team]);
        assert_eq!(q.actionable[0].number, 2, "team first-request leads");
        let re_row = q.actionable.iter().find(|r| r.number == 1).unwrap();
        assert_eq!(re_row.tone, Tone::Rereview);
        assert!(re_row.needs_re_review);
    }

    #[test]
    fn age_is_computed_from_created_at() {
        // 2026-08-01T00:00:00Z + 48h == 2026-08-03.
        let now = SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(epoch_secs("2026-08-03T00:00:00Z").unwrap() as u64);
        let h = age_hours_from("2026-08-01T00:00:00Z", now);
        assert!((h - 48.0).abs() < 0.01, "got {h}");
    }

    #[test]
    fn an_unparseable_timestamp_reads_as_new_rather_than_panicking() {
        assert_eq!(age_hours_from("not a date", SystemTime::now()), 0.0);
    }

    #[test]
    fn epoch_matches_a_known_instant() {
        // 2001-09-09T01:46:40Z is exactly 1_000_000_000.
        assert_eq!(epoch_secs("2001-09-09T01:46:40Z"), Some(1_000_000_000));
        assert_eq!(epoch_secs("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn an_empty_predicate_matches_nothing_but_always_matches_all() {
        let c = cand(1);
        assert!(!Predicate::default().matches(&c));
        assert!(Predicate {
            always: true,
            ..Default::default()
        }
        .matches(&c));
    }

    #[test]
    fn a_multi_field_predicate_is_an_and() {
        let mut c = cand(1);
        c.labels = vec!["prio".into()];
        c.requested_personally = true;
        let both = Predicate {
            label: Some("prio".into()),
            requested: Some(Requested::Personal),
            ..Default::default()
        };
        assert!(both.matches(&c));
        c.requested_personally = false;
        c.requested_team = true;
        assert!(!both.matches(&c), "personal no longer holds");
    }

    #[test]
    fn the_default_state_is_pending_not_degraded() {
        assert!(matches!(ReviewState::default(), ReviewState::Pending));
    }

    #[test]
    fn a_custom_ranking_reorders_the_queue() {
        // A repo that wants team requests on top can say so without a patch.
        let ranking = ReviewRanking {
            rules: vec![
                Rule {
                    when: Predicate {
                        requested: Some(Requested::Team),
                        ..Default::default()
                    },
                    rank: 0,
                    reason: Some("team".into()),
                    tone: Some(Tone::Prio),
                },
                Rule {
                    when: Predicate {
                        always: true,
                        ..Default::default()
                    },
                    rank: 1,
                    reason: None,
                    tone: None,
                },
            ],
            blocked_when: vec![],
            tiebreak: vec![],
        };
        let mut personal = cand(1);
        personal.requested_personally = true;
        let mut team = cand(2);
        team.requested_team = true;
        let q = rank("me".into(), &[personal, team], &ranking, SystemTime::now());
        assert_eq!(q.actionable[0].number, 2, "team was promoted to the top");
    }
}
