use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

/// PRs where your review is requested — other people's work (§6b).
///
/// The shape came from an existing `mise run reviews --json` task, which applies
/// a richer ranking than §6b describes: prio labels, personal versus team
/// request, re-review detection, reviewer-count tiebreak. The daemon consumes
/// that shape rather than imposing the one §6b invented.
///
/// **[`builtin`] fills the same struct and fills less of it**, which is the point
/// of keeping one shape for two sources: the pane reads one row type, and a repo
/// that wants the richer ranking keeps its command. What the built-in never sets
/// is said on each field.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub struct Review {
    #[cfg_attr(any(test, feature = "test-util"), ts(type = "number"))]
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    /// Age in hours, however the source expressed it.
    pub age_hours: f64,
    /// Source rank: 0 stopper, 1 prio, 2 requested of you, 3 of your team,
    /// 4 re-review, 5 other, 6 sidequest.
    ///
    /// **[`builtin`] emits only 2 and 3.** 0 and 1 are label ranks — `stopper` and
    /// `prio` — and a label is a convention one team agreed to, so a default that
    /// ranked on them ranked wrongly in every repository that had never heard of
    /// them. A configured command may still emit the whole scale.
    pub prio: u32,
    pub needs_re_review: bool,
    pub is_draft: bool,
    /// `conflicts`, `failing checks`, … — non-empty means it is waiting on
    /// someone else and sinks to the bottom.
    pub blockers: Vec<String>,
    pub reviewers: u32,
    /// Review cost. Absent until the source grows `changedFiles`; the column is
    /// omitted rather than faked (see docs/reviews-json.md). [`builtin`] leaves it
    /// unset: it is another page of the search per poll, for a column that hides
    /// itself.
    pub changed_files: Option<u32>,
    pub checks: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub struct ReviewQueue {
    pub login: String,
    pub actionable: Vec<Review>,
    pub blocked: Vec<Review>,
    pub total: u32,
    pub skipped: u32,
}

/// A pane that cannot be trusted must say so.
///
/// Silently showing zero reviews when the command is broken is the one failure
/// that would actually cost a colleague a day (§6b), so a non-zero exit,
/// unparseable output or an unknown `version` all land here instead.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub enum ReviewState {
    Ok(ReviewQueue),
    Degraded {
        reason: String,
    },
    /// Before the first poll lands. Distinct from `Degraded` so startup does
    /// not read as a broken command — and so it never becomes a TODO entry.
    #[default]
    Pending,
    /// Nothing to ask about: no `reviews_command`, and no GitHub repository for
    /// the built-in queue to search either. Not a fault — this checkout simply has
    /// no source — so, like `Pending`, it never becomes a TODO finding and the pane
    /// says so rather than reading "unavailable".
    ///
    /// It used to mean "no command configured", which is now the ordinary case and
    /// answers with a queue.
    Off,
}

/// Version the daemon understands. The source does not emit one yet, so its
/// absence is accepted; a *different* one is not.
const KNOWN_VERSION: u64 = 1;

/// One page of requests. Fifty was what the shipped script asked for and it was
/// never reached: a review queue that long is not one anybody works from.
const PAGE: usize = 50;

/// The queue, built by the daemon with no external process at all.
///
/// **This is the default now, and the reason is that a script could not be one.**
/// The `reviews.js` this replaces ran `#!/usr/bin/env node` against the *daemon's*
/// PATH — which is the launcher's, not a shell's, exactly as `session_env`'s
/// docblock says of sessions. On the machine this was written for that resolved to
/// a system node old enough that `require('node:child_process')` does not exist,
/// so a fresh checkout's pane read `unavailable` and no amount of configuring it
/// could help. It needed `gh` as well. This needs neither: the token and `curl`
/// are what the PR pane already runs on, so a checkout that can list its PRs can
/// show its review queue.
///
/// **The rules are deliberately few, because the ones they replace were guesses
/// about somebody else's repository.** `stopper` and `prio` are label conventions,
/// not a standard, and a default that ranks on them ranks wrongly everywhere they
/// are not used — which is every repository but the one they were written for.
/// What is left holds anywhere on GitHub:
///
/// - the queue is what GitHub itself says is **requested of you**
///   (`review-requested:@me`, which includes a team you are in);
/// - **age orders it**, oldest first, because how long somebody has waited is true
///   regardless of how their team labels work;
/// - a row is **amber when you were named yourself** and grey when the request
///   went to a team — the difference between somebody asking you and somebody
///   asking a group you happen to be in;
/// - draft, conflicting and failing rows **sink below the fold**: those are
///   waiting on their author rather than on you.
///
/// A repo that wants its own opinion sets `reviews_command` and this never runs —
/// the contract for that is `docs/reviews-json.md`, unchanged.
pub fn builtin(token: &str, owner: &str, name: &str) -> Result<ReviewQueue> {
    // `review-requested:` is the filter that already includes team requests;
    // `user-review-requested:` is the narrower one. Letting GitHub answer "am I
    // asked" is what keeps this out of the business of knowing your teams.
    let search = format!("repo:{owner}/{name} is:open is:pr review-requested:@me");
    let query = format!(
        r#"{{
  viewer {{ login }}
  search(query: "{search}", type: ISSUE, first: {PAGE}) {{
    issueCount
    nodes {{
      ... on PullRequest {{
        number title url isDraft createdAt mergeable
        author {{ login }}
        reviewRequests(first: 20) {{ nodes {{ requestedReviewer {{ ... on User {{ login }} }} }} }}
        latestReviews(first: 20) {{ nodes {{ author {{ login }} }} }}
        commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state }} }} }} }}
      }}
    }}
  }}
}}"#
    );
    from_graphql(&crate::forge::github::graphql(token, &query)?)
}

/// Map one GraphQL answer onto the queue, applying the whole of the ranking.
///
/// Split from [`builtin`] so the rules are testable against a captured answer
/// rather than against GitHub — which is the only way the ordering and the amber
/// rule get checked at all.
fn from_graphql(v: &Value) -> Result<ReviewQueue> {
    let data = v
        .get("data")
        .context("the GraphQL answer carried no data")?;
    let viewer = data
        .pointer("/viewer/login")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let search = data
        .get("search")
        .context("the GraphQL answer carried no search")?;
    let total = search
        .get("issueCount")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;

    let mut actionable = Vec::new();
    let mut blocked = Vec::new();
    for n in search
        .get("nodes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        // A search over issues answers `{}` for anything that is not a pull
        // request, and `null` for a node it could not read at all.
        if n.get("number").is_none() {
            continue;
        }
        let r = row(n, &viewer);
        if r.blockers.is_empty() {
            actionable.push(r);
        } else {
            blocked.push(r);
        }
    }
    /* **Age, and nothing else.** Oldest first, which is the whole ordering: the
    thing it replaced sorted on label names first and used age only to break a
    tie, so on a repo with no such labels every row tied and the age was doing all
    the work anyway — with four ranks of machinery in front of it. */
    let oldest_first = |a: &Review, b: &Review| b.age_hours.total_cmp(&a.age_hours);
    actionable.sort_by(oldest_first);
    blocked.sort_by(oldest_first);

    Ok(ReviewQueue {
        login: viewer,
        // What the page could not carry, so a queue longer than one page says so
        // rather than quietly being the first fifty.
        skipped: total.saturating_sub((actionable.len() + blocked.len()) as u32),
        total,
        actionable,
        blocked,
    })
}

/// One PR, as the pane reads it.
fn row(n: &Value, viewer: &str) -> Review {
    let text = |key: &str| {
        n.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let is_draft = n.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
    let mergeable = n
        .get("mergeable")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let checks = n
        .pointer("/commits/nodes/0/commit/statusCheckRollup/state")
        .and_then(Value::as_str)
        .map(str::to_string);

    /* Named yourself, or reached through a team — the one distinction the pane
    colours, and the only one left. A team entry has no `login` at all (it is a
    `Team`, and the query asks for the `User` fields alone), so an absent one is
    the team case rather than a field that failed to read. */
    let requested_of_you = n
        .pointer("/reviewRequests/nodes")
        .and_then(Value::as_array)
        .is_some_and(|rs| {
            rs.iter().any(|r| {
                r.pointer("/requestedReviewer/login")
                    .and_then(Value::as_str)
                    == Some(viewer)
            })
        });

    // Distinct humans, so two reviews by one person are one pair of eyes.
    let reviewers: std::collections::BTreeSet<&str> = n
        .pointer("/latestReviews/nodes")
        .and_then(Value::as_array)
        .map(|rs| {
            rs.iter()
                .filter_map(|r| r.pointer("/author/login").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();

    /* Waiting on its author rather than on you. Kept from the script it replaces,
    and the reason it survived the cull where the labels did not: these three are
    facts GitHub reports about any repository, not conventions somebody's team
    agreed to. */
    let mut blockers = Vec::new();
    if is_draft {
        blockers.push("draft".to_string());
    }
    if mergeable == "CONFLICTING" {
        blockers.push("conflicts".to_string());
    }
    if checks.as_deref() == Some("FAILURE") {
        blockers.push("failing checks".to_string());
    }

    Review {
        number: n.get("number").and_then(Value::as_u64).unwrap_or(0),
        title: text("title"),
        url: text("url"),
        author: n
            .pointer("/author/login")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        age_hours: age_hours(n.get("createdAt").and_then(Value::as_str)),
        /* 2 and 3 are `docs/reviews-json.md`'s own "requested of you" and "of your
        team", kept rather than renamed because a configured command still emits
        that scale and the pane reads one field for both sources. The built-in
        never emits 0 or 1 — those are the label ranks, and they are gone. */
        prio: if requested_of_you { 2 } else { 3 },
        needs_re_review: reviewers.contains(viewer),
        is_draft,
        blockers,
        reviewers: reviewers.len() as u32,
        // One more page of the search to fill, and the column hides itself until
        // something provides it. Not worth a second round trip per poll.
        changed_files: None,
        checks,
    }
}

/// Hours since `ts`, a GitHub timestamp (`2026-09-12T18:00:00Z`).
///
/// Rounded to a tenth, as the script did: the pane prints `3d` and `33h`, so
/// anything finer is precision nobody reads.
fn age_hours(ts: Option<&str>) -> f64 {
    let Some(then) = ts.and_then(epoch_secs) else {
        return 0.0;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (((now - then).max(0) as f64 / 3600.0) * 10.0).round() / 10.0
}

/// Epoch seconds from an RFC 3339 UTC timestamp.
///
/// Hand-rolled because this workspace carries no date crate and one field does
/// not earn one — `github.rs` compares these strings lexically for the same
/// reason. The civil-date arithmetic is Hinnant's `days_from_civil`, which is
/// exact for every proleptic Gregorian date and has no table and no leap-year
/// special case beyond the three already in the expression.
fn epoch_secs(ts: &str) -> Option<i64> {
    if ts.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // March-based year: February's length stops being a special case.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let year_of_era = y - era * 400;
    let month_shifted = (month + 9) % 12;
    let day_of_year = (153 * month_shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    // 719468 is the days between 0000-03-01 and 1970-01-01.
    let days = era * 146097 + day_of_era - 719468;
    Some(days * 86400 + hour * 3600 + minute * 60 + second)
}

/// The queue for this checkout: a configured command when there is one, the
/// built-in otherwise.
///
/// The command keeps precedence deliberately. A team's real ranking lives in its
/// own tooling — this repo's own checkout points `reviews_command` at a `mise`
/// task — and a default that overrode it would be the daemon insisting where
/// CLAUDE.md says it should defer.
pub fn fetch(
    main: &Path,
    timeout_secs: u64,
    command: &[String],
    repo: Option<&str>,
    token: Option<&str>,
) -> ReviewState {
    if !command.is_empty() {
        return match run(main, timeout_secs, command, repo) {
            Ok(q) => ReviewState::Ok(q),
            Err(e) => ReviewState::Degraded {
                reason: format!("{e:#}"),
            },
        };
    }
    // Not a fault: a checkout with no GitHub repository behind it has no queue to
    // show, which is what `Off` has always meant — only the reason changed.
    let Some((owner, name)) = repo.and_then(|r| r.split_once('/')) else {
        return ReviewState::Off;
    };
    let Some(token) = token else {
        return ReviewState::Degraded {
            reason: "no GitHub token — the review queue reads the same one the PR pane does"
                .to_string(),
        };
    };
    match builtin(token, owner, name) {
        Ok(q) => ReviewState::Ok(q),
        Err(e) => ReviewState::Degraded {
            reason: format!("{e:#}"),
        },
    }
}

fn run(
    main: &Path,
    timeout_secs: u64,
    command: &[String],
    repo: Option<&str>,
) -> Result<ReviewQueue> {
    let out = orchd_base::proc::run_bounded(main, timeout_secs, command, "reviews")?;

    if !out.status.success() {
        bail!(
            "reviews exited {}: {}",
            out.status.code().unwrap_or(-1),
            orchd_base::proc::stderr_tail(&out.stderr)
        );
    }

    let v: Value = serde_json::from_slice(&out.stdout).context("reviews output was not JSON")?;
    // Several logins would come back as an array; the daemon only ever asks
    // about one.
    let v = match v {
        Value::Array(mut a) if !a.is_empty() => a.remove(0),
        other => other,
    };
    if let Some(ver) = v.get("version").and_then(|x| x.as_u64()) {
        if ver != KNOWN_VERSION {
            bail!("reviews reported version {ver}, which this daemon does not understand");
        }
    }
    parse(v, repo)
}

/// The `Queue` the source prints (`docs/reviews-json.md`), as far as the daemon
/// reads it. A mirror struct rather than pointer-poking so the shape is written
/// down once; `actionable`/`blocked` stay raw so one bad row is dropped and named
/// rather than sinking the whole queue.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct QueueDoc {
    for_login: String,
    total: u32,
    skipped: u32,
    actionable: Option<Vec<Value>>,
    blocked: Option<Vec<Value>>,
}

/// One `QueueEntry`. `pr` is required; everything else has a default, because the
/// source has grown fields over time and an older one must still parse.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntryDoc {
    pr: PrDoc,
    /// How many humans already reviewed.
    ///
    /// `Value` rather than a typed field, because the two sources disagree and
    /// both are real: `docs/reviews-json.md` documents `reviewers: string[]`, and
    /// the shipped script's own captured output (the test below) has
    /// `"reviewers":0`. Typing it either way would drop every row from the other.
    #[serde(default)]
    reviewers: Value,
    #[serde(default)]
    blockers: Vec<String>,
    #[serde(default)]
    needs_re_review: bool,
    #[serde(default)]
    age_hours: Option<f64>,
    #[serde(default)]
    age_days: Option<f64>,
    #[serde(default)]
    prio: Option<u32>,
}

/// The `Pr` inside an entry. `number` is the one field a row is useless without.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrDoc {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    changed_files: Option<u32>,
    #[serde(default)]
    checks: Option<String>,
}

/// Takes the document by value: it holds every actionable and blocked row, and
/// the caller has no use for it afterwards, so deserializing from a clone copied
/// the whole queue once per poll for nothing.
fn parse(v: Value, repo: Option<&str>) -> Result<ReviewQueue> {
    let doc: QueueDoc = serde_json::from_value(v).context("reviews output shape")?;
    if doc.actionable.is_none() && doc.blocked.is_none() {
        bail!("reviews output has neither `actionable` nor `blocked`");
    }
    let entries = |key: &str, rows: Option<Vec<Value>>| -> Vec<Review> {
        rows.unwrap_or_default()
            .into_iter()
            .filter_map(|e| match serde_json::from_value::<EntryDoc>(e) {
                Ok(d) => Some(review_of(d, repo)),
                // Dropped, but said: a silently vanishing row is the failure this
                // pane exists to avoid, and the message names what the shape was.
                Err(err) => {
                    tracing::warn!("reviews: skipping a `{key}` row the daemon cannot read: {err}");
                    None
                }
            })
            .collect()
    };
    Ok(ReviewQueue {
        login: doc.for_login,
        actionable: entries("actionable", doc.actionable),
        blocked: entries("blocked", doc.blocked),
        total: doc.total,
        skipped: doc.skipped,
    })
}

fn review_of(e: EntryDoc, repo: Option<&str>) -> Review {
    let number = e.pr.number;
    Review {
        number,
        title: e.pr.title,
        // Deriving the url keeps a row clickable even if the field is dropped.
        // From the *configured* repo: this used to name one hardcoded repo, so
        // every other user's rows linked somewhere they could not see. GitHub's
        // URL shape, which is the only forge there is an impl for; with no repo
        // known the row simply does not link.
        url: e
            .pr
            .url
            .or_else(|| repo.map(|r| format!("https://github.com/{r}/pull/{number}")))
            .unwrap_or_default(),
        author: e.pr.author.unwrap_or_else(|| "unknown".to_string()),
        // The source has expressed age as both `ageHours` and `ageDays`; take
        // whichever is there rather than depending on which day it is.
        age_hours: e
            .age_hours
            .or_else(|| e.age_days.map(|d| d * 24.0))
            .unwrap_or(0.0),
        prio: e.prio.unwrap_or(9),
        needs_re_review: e.needs_re_review,
        is_draft: e.pr.is_draft,
        blockers: e.blockers,
        reviewers: match &e.reviewers {
            Value::Array(a) => a.len() as u32,
            // Already a count. Reading it as one rather than as "not an array,
            // so zero", which is what the pointer-poking version did.
            Value::Number(n) => n.as_u64().unwrap_or(0) as u32,
            _ => 0,
        },
        changed_files: e.pr.changed_files,
        checks: e.pr.checks,
    }
}

#[cfg(test)]
mod tests {
    /// The tests that do not care about link derivation.
    fn parse_t(v: Value) -> Result<ReviewQueue> {
        parse(v, Some("acme/monorepo"))
    }

    use super::*;

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn reads_the_shape_the_source_actually_emits() {
        let q = parse_t(v(r#"{
            "forLogin":"kbarendrecht","total":16,"skipped":4,
            "actionable":[{"pr":{"number":2001,"title":"Refactor a config loader",
                "url":"https://github.com/x/y/pull/2001","author":"dana","isDraft":false},
                "reviewers":["a","b"],"blockers":[],"needsReReview":false,
                "ageHours":52.5,"prio":2}],
            "blocked":[],"ownBlocked":[]}"#))
        .expect("parse");
        assert_eq!(q.login, "kbarendrecht");
        assert_eq!(q.actionable.len(), 1);
        let r = &q.actionable[0];
        assert_eq!(r.number, 2001);
        assert_eq!(r.author, "dana");
        assert_eq!(r.reviewers, 2);
        assert_eq!(r.prio, 2);
        assert!((r.age_hours - 52.5).abs() < 0.01);
        // Not emitted yet, so the column is omitted rather than invented.
        assert!(r.changed_files.is_none());
    }

    #[test]
    fn accepts_age_in_days_as_well_as_hours() {
        let q = parse_t(v(
            r#"{"actionable":[{"pr":{"number":1,"title":"t","author":"a"},
            "ageDays":2}],"blocked":[]}"#,
        ))
        .unwrap();
        assert!((q.actionable[0].age_hours - 48.0).abs() < 0.01);
    }

    #[test]
    fn derives_a_url_from_the_configured_repo_when_the_field_is_missing() {
        let q = parse(
            v(
                r#"{"actionable":[{"pr":{"number":99,"title":"t","author":"a"}}],
            "blocked":[]}"#,
            ),
            Some("acme/monorepo"),
        )
        .unwrap();
        assert_eq!(
            q.actionable[0].url,
            "https://github.com/acme/monorepo/pull/99"
        );
    }

    /// It used to name one hardcoded repo here, so everyone else's rows linked
    /// into a repo they could not open. No repo known is now no link.
    #[test]
    fn an_unknown_repo_yields_no_link_rather_than_a_wrong_one() {
        let q = parse(
            v(
                r#"{"actionable":[{"pr":{"number":99,"title":"t","author":"a"}}],
            "blocked":[]}"#,
            ),
            None,
        )
        .unwrap();
        assert!(q.actionable[0].url.is_empty());
    }

    #[test]
    fn output_of_the_wrong_shape_is_an_error_not_an_empty_queue() {
        // The failure that would cost a colleague a day.
        assert!(parse_t(v(r#"{"something":"else"}"#)).is_err());
    }

    /// One row the daemon cannot read is dropped and named, not the whole queue
    /// and not silently: the other rows still show. A row with no PR number is
    /// the case, because a row that cannot be opened is not a row.
    #[test]
    fn a_bad_row_is_skipped_and_the_rest_still_parse() {
        let q = parse_t(v(r#"{"actionable":[
            {"pr":{"title":"no number"}},
            {"pr":{"number":7,"title":"fine","author":"a"}},
            {"nothing":"like an entry"},
            {"pr":{"number":8,"title":"also fine","author":"b"}}],
            "blocked":[]}"#))
        .unwrap();
        assert_eq!(
            q.actionable.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![7, 8]
        );
    }

    /// The count of reviewers arrives as an array in one place and as a number in
    /// the other, and both have to read. See `EntryDoc::reviewers`.
    #[test]
    fn a_reviewer_count_is_read_as_an_array_or_as_a_number() {
        let of = |rev: &str| {
            parse_t(v(&format!(
                r#"{{"actionable":[{{"pr":{{"number":1,"title":"t"}},"reviewers":{rev}}}],"blocked":[]}}"#
            )))
            .unwrap()
            .actionable[0]
                .reviewers
        };
        assert_eq!(of(r#"["a","b"]"#), 2);
        assert_eq!(of("0"), 0);
        assert_eq!(of("3"), 3);
        assert_eq!(of("null"), 0);
    }

    #[test]
    fn the_state_before_the_first_poll_is_pending_not_degraded() {
        // Degraded means the command is broken and someone should look; startup
        // is not that, and treating it as such cries wolf on every restart.
        assert!(matches!(ReviewState::default(), ReviewState::Pending));
    }

    #[test]
    fn an_empty_but_valid_queue_is_ok_not_degraded() {
        let q = parse_t(v(r#"{"forLogin":"me","actionable":[],"blocked":[]}"#)).unwrap();
        assert!(q.actionable.is_empty());
    }

    /// The shipped script and the parser have to agree, and nothing else checks
    /// that: `fetch` shells out, so a shape change in `reviews/default.py` would
    /// surface as a degraded pane at runtime rather than a red test.
    ///
    /// This is real output, captured from the script against a live repo — not a
    /// hand-written approximation of it, which is the version that stays passing
    /// while the script drifts.
    #[test]
    fn a_configured_command_prints_what_the_parser_reads() {
        let real = r#"{"forLogin":"kbarendrecht","total":1,"skipped":0,"actionable":[
            {"pr":{"number":10003,
                   "title":"Rename a widget helper",
                   "url":"https://github.com/acme/monorepo/pull/10003",
                   "author":"bob","isDraft":false,
                   "mergeable":"MERGEABLE","checks":"SUCCESS"},
             "prio":3,"ageHours":70.4,"reviewers":0,"needsReReview":false,
             "blockers":[]}],"blocked":[]}"#;
        let q = parse(v(real), Some("acme/monorepo")).expect("the shipped shape parses");
        assert_eq!(q.login, "kbarendrecht");
        assert_eq!(q.total, 1);
        let e = &q.actionable[0];
        assert_eq!(e.number, 10003);
        assert_eq!(e.author, "bob");
        assert_eq!(e.prio, 3);
        assert_eq!(e.age_hours, 70.4);
        assert_eq!(e.checks.as_deref(), Some("SUCCESS"));
        assert!(e.blockers.is_empty());
        // The url came from the script, so the repo-derived fallback stayed out.
        assert!(e.url.ends_with("/pull/10003"));
    }

    /// A repo with nothing waiting is not a broken command.
    #[test]
    fn an_empty_queue_from_a_command_is_ok() {
        let q = parse(
            v(r#"{"forLogin":"me","total":0,"skipped":0,"actionable":[],"blocked":[]}"#),
            None,
        )
        .expect("empty is valid");
        assert!(q.actionable.is_empty() && q.blocked.is_empty());
    }

    #[test]
    fn no_repo_is_off_not_degraded() {
        // A checkout with no GitHub repository behind it has no queue to show, and
        // must not read as a broken command — that would colour the pane red for
        // nothing. No command *and* no repo is the only way to reach `Off` now.
        assert!(matches!(
            fetch(Path::new("/nonexistent"), 1, &[], None, None),
            ReviewState::Off
        ));
    }

    #[test]
    fn a_repo_with_no_token_is_degraded_and_says_so() {
        // The opposite case, and it is a fault: there *is* a repo to ask about and
        // the daemon cannot. Silence here would read as "nobody wants anything
        // from you", which is the one wrong answer a review queue can give.
        let state = fetch(
            Path::new("/nonexistent"),
            1,
            &[],
            Some("acme/monorepo"),
            None,
        );
        match state {
            ReviewState::Degraded { reason } => assert!(
                reason.contains("token"),
                "the reason has to name what is missing: {reason}"
            ),
            other => panic!("a repo with no token must be degraded, got {other:?}"),
        }
    }

    /// One captured answer, carrying every rule at once: two people, a team
    /// request, a draft, a conflict, a failing check and a re-review.
    fn answer() -> Value {
        v(r#"{"data":{
          "viewer":{"login":"me"},
          "search":{"issueCount":7,"nodes":[
            {"number":10,"title":"newest, you by name","url":"u10","isDraft":false,
             "createdAt":"2026-09-12T12:00:00Z","mergeable":"MERGEABLE",
             "author":{"login":"dana"},
             "reviewRequests":{"nodes":[{"requestedReviewer":{"login":"me"}}]},
             "latestReviews":{"nodes":[]},
             "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}},
            {"number":11,"title":"oldest, your team","url":"u11","isDraft":false,
             "createdAt":"2026-09-01T12:00:00Z","mergeable":"MERGEABLE",
             "author":{"login":"ola"},
             "reviewRequests":{"nodes":[{"requestedReviewer":{}}]},
             "latestReviews":{"nodes":[{"author":{"login":"me"}},{"author":{"login":"ola"}}]},
             "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}},
            {"number":12,"title":"a draft","url":"u12","isDraft":true,
             "createdAt":"2026-09-05T12:00:00Z","mergeable":"MERGEABLE",
             "author":{"login":"dana"},
             "reviewRequests":{"nodes":[{"requestedReviewer":{"login":"me"}}]},
             "latestReviews":{"nodes":[]},"commits":{"nodes":[]}},
            {"number":13,"title":"conflicting and failing","url":"u13","isDraft":false,
             "createdAt":"2026-09-06T12:00:00Z","mergeable":"CONFLICTING",
             "author":{"login":"ola"},
             "reviewRequests":{"nodes":[{"requestedReviewer":{"login":"me"}}]},
             "latestReviews":{"nodes":[]},
             "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE"}}}]}},
            {}
          ]}}}"#)
    }

    #[test]
    fn age_orders_the_queue_and_nothing_else_does() {
        let q = from_graphql(&answer()).expect("a captured answer parses");
        assert_eq!(
            q.actionable.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![11, 10],
            "oldest first, whoever it was asked of — #11 is the team request and \
             still leads, because age is the whole ordering"
        );
    }

    #[test]
    fn amber_is_being_named_yourself_and_a_team_request_is_not() {
        let q = from_graphql(&answer()).expect("parse");
        let by = |n: u64| {
            q.actionable
                .iter()
                .chain(q.blocked.iter())
                .find(|r| r.number == n)
                .unwrap_or_else(|| panic!("#{n}"))
        };
        // 2 and 3 are the contract's "requested of you" and "of your team"; the
        // pane paints 2 amber. A team entry carries no `login` at all.
        assert_eq!(by(10).prio, 2, "you were named");
        assert_eq!(by(11).prio, 3, "a team you are in was named");
        // And the labels that used to outrank both are gone: nothing emits 0 or 1.
        assert!(
            q.actionable
                .iter()
                .chain(q.blocked.iter())
                .all(|r| r.prio >= 2),
            "the built-in must never emit a label rank"
        );
    }

    #[test]
    fn what_waits_on_its_author_sinks_below_the_fold() {
        let q = from_graphql(&answer()).expect("parse");
        let mut sunk: Vec<_> = q.blocked.iter().map(|r| r.number).collect();
        sunk.sort_unstable();
        assert_eq!(sunk, vec![12, 13], "the draft and the broken one");
        let broken = q.blocked.iter().find(|r| r.number == 13).expect("#13");
        assert_eq!(
            broken.blockers,
            vec!["conflicts".to_string(), "failing checks".to_string()],
            "both reasons travel, because the pane prints them"
        );
        assert!(q.blocked.iter().any(|r| r.blockers == ["draft"]));
    }

    #[test]
    fn a_row_carries_who_already_looked_and_whether_you_did() {
        let q = from_graphql(&answer()).expect("parse");
        let team = q.actionable.iter().find(|r| r.number == 11).expect("#11");
        assert_eq!(team.reviewers, 2, "two distinct humans");
        assert!(team.needs_re_review, "you are one of them");
        let fresh = q.actionable.iter().find(|r| r.number == 10).expect("#10");
        assert_eq!(fresh.reviewers, 0);
        assert!(!fresh.needs_re_review);
        assert_eq!(fresh.author, "dana");
        assert_eq!(fresh.checks.as_deref(), Some("SUCCESS"));
    }

    #[test]
    fn a_node_that_is_not_a_pull_request_is_skipped_not_counted() {
        let q = from_graphql(&answer()).expect("parse");
        assert_eq!(
            q.actionable.len() + q.blocked.len(),
            4,
            "the node that is not a pull request is dropped"
        );
        assert_eq!(q.total, 7, "what GitHub said the search holds");
        assert_eq!(q.skipped, 3, "what this page could not carry");
        assert_eq!(q.login, "me");
    }

    #[test]
    fn a_github_timestamp_becomes_an_age() {
        // Exact anchors, so a wrong civil-date expression cannot pass: these are
        // the epoch itself, a leap day, and the turn of a century that is not one.
        assert_eq!(epoch_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_secs("1970-01-02T00:00:01Z"), Some(86401));
        assert_eq!(epoch_secs("2000-02-29T00:00:00Z"), Some(951782400));
        assert_eq!(epoch_secs("1900-03-01T00:00:00Z"), Some(-2203891200));
        assert_eq!(epoch_secs("2026-09-12T18:00:00Z"), Some(1789236000));
        // Anything that is not one answers `None` rather than a wrong number.
        assert_eq!(epoch_secs("nope"), None);
        assert_eq!(epoch_secs("2026-13-01T00:00:00Z"), None);
        assert_eq!(
            age_hours(None),
            0.0,
            "no timestamp is no age, never a negative"
        );
    }
}
