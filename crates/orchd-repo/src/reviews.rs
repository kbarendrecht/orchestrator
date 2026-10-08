use crate::config::{QueueRules, ReviewsSource};
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
    /// Whether your review was asked for, by name or through a team.
    ///
    /// **[`builtin`] lists every open PR now and says which ones asked**, so the
    /// pane can narrow to those without a second fetch. `None` from a configured
    /// command: its rows are its own ranking, and the pane offers no filter it
    /// cannot honour.
    pub requested: Option<bool>,
    /// Where a checkout's own rules put this row: 0 first, 4 last. Sort order
    /// only, and off the wire.
    ///
    /// **Separate from `prio` because `prio` says what the row *is*.** The first
    /// version of the configured ranks wrote them into `prio` — and the pane
    /// reads `prio` for the row's colour and its reason word, so a `First` label
    /// on a PR that asked your team turned it red and erased the word `team`. A
    /// rule about where a row *sits* must not rewrite what it says.
    #[serde(skip)]
    #[cfg_attr(any(test, feature = "test-util"), ts(skip))]
    pub rank: u8,
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

/// GitHub's spelling for "any of these labels": one `label:` term, comma
/// separated, each name quoted because labels carry spaces.
///
/// Quoting rather than escaping: a label with a `"` in it is not a label anybody
/// has, and dropping the quote is better than sending a query GitHub rejects —
/// a rejected search degrades the whole pane over one odd character.
///
/// **A label with a comma in it cannot be configured**, here or in the panel: the
/// list is comma separated at both ends, so such a name arrives as two. The local
/// rules then match neither half, so the cost is a PR that does not match rather
/// than one that wrongly does — which is the right way round for a queue.
fn quoted_csv(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("\"{}\"", n.replace('"', "")))
        .collect::<Vec<_>>()
        .join(",")
}

/// Version the daemon understands. The source does not emit one yet, so its
/// absence is accepted; a *different* one is not.
const KNOWN_VERSION: u64 = 1;

/// One page of each search. The queue lists every open PR somebody else wrote,
/// so on a busy repo this cap *is* reached: the `all` search asks oldest first so
/// the cut falls on the newest, and every PR that asked for you is merged in from
/// `asked` whatever its age, so the cap never costs a request.
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
/// - the queue is **every open PR somebody else wrote**, and each row says whether
///   GitHub has your review **requested** (`review-requested:@me`, which includes
///   a team you are in), so the pane can narrow to those. It used to be those
///   alone, and a repo where reviews are picked up rather than assigned showed two
///   rows out of thirty;
/// - **requested first, then age**, oldest first within each, because how long
///   somebody has waited is true regardless of how their team labels work, and a
///   PR that asked for you should not sit under thirty that did not;
/// - a row is **amber when you were named yourself** and grey when the request
///   went to a team — the difference between somebody asking you and somebody
///   asking a group you happen to be in;
/// - draft, conflicting and failing rows **sink below the fold**: those are
///   waiting on their author rather than on you.
///
/// A repo that wants its own opinion sets `reviews_command` and this never runs —
/// the contract for that is `docs/reviews-json.md`, unchanged.
pub fn builtin(token: &str, owner: &str, name: &str, rules: &QueueRules) -> Result<ReviewQueue> {
    /* Two searches in one round trip. `all` is the queue; `asked` is only its
    numbers, so a row can say whether it was requested. `review-requested:` is
    the filter that already includes team requests, and letting GitHub answer "am
    I asked" is what keeps this out of the business of knowing your teams. */
    /* **The filter's own terms, translated here because translating is a forge's
    job.** `config::QueueFilter` asks neutral questions — who wrote it, what is it
    labelled — and this is the one place that knows GitHub's grammar for them. A
    second forge writes its own `builtin`, against the same struct.

    **Into the search where GitHub can express it, and after the fetch where it
    cannot.** The page cap is why: one page of fifty comes back, so a filter
    applied only afterwards narrows what the cap already chose rather than what
    the repo holds. Author and the required labels go in; drafts, blocked and
    "asked for me" are read off rows this already parses. */
    let f = &rules.filter;
    /* **`-author:@me` is not configurable, and that is not an omission.** It was,
    for an afternoon, and the options were a lie: you cannot review your own PR,
    so "everyone" and "others" name the same list and "mine" names an empty one.
    Your own PRs are the pane above this one. */
    // `label:` repeated is AND on GitHub and the field means "any of", so one
    // term with commas, which is GitHub's spelling for OR.
    let wanted = if f.labels_any.is_empty() {
        String::new()
    } else {
        format!(" label:{}", quoted_csv(&f.labels_any))
    };
    // Oldest first, the order the queue ranks in, so a cut drops the newest.
    let all = format!("repo:{owner}/{name} is:open is:pr -author:@me{wanted} sort:created-asc");
    let asked = format!("repo:{owner}/{name} is:open is:pr review-requested:@me");
    let query = format!(
        r#"{{
  viewer {{ login }}
  asked: search(query: "{asked}", type: ISSUE, first: {PAGE}) {{
    nodes {{ ...Row }}
  }}
  all: search(query: "{all}", type: ISSUE, first: {PAGE}) {{
    issueCount
    nodes {{ ...Row }}
  }}
}}
fragment Row on PullRequest {{
  number title url isDraft createdAt mergeable
  labels(first: 20) {{ nodes {{ name }} }}
  author {{ login }}
  reviewRequests(first: 20) {{ nodes {{ requestedReviewer {{ ... on User {{ login }} }} }} }}
  latestReviews(first: 20) {{ nodes {{ author {{ login }} }} }}
  commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ state }} }} }} }}
}}"#
    );
    from_graphql(&crate::forge::graphql(token, &query)?, rules)
}

/// Map one GraphQL answer onto the queue, applying the whole of the ranking.
///
/// Split from [`builtin`] so the rules are testable against a captured answer
/// rather than against GitHub — which is the only way the ordering and the amber
/// rule get checked at all.
fn from_graphql(v: &Value, rules: &QueueRules) -> Result<ReviewQueue> {
    let data = v
        .get("data")
        .context("the GraphQL answer carried no data")?;
    let viewer = data
        .pointer("/viewer/login")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let search = data
        .get("all")
        .context("the GraphQL answer carried no search")?;
    let asked: std::collections::HashSet<u64> = data
        .pointer("/asked/nodes")
        .and_then(Value::as_array)
        .map(|ns| {
            ns.iter()
                .filter_map(|n| n.get("number").and_then(Value::as_u64))
                .collect()
        })
        .unwrap_or_default();
    let total = search
        .get("issueCount")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;

    let nodes = |key: &str| {
        data.pointer(&format!("/{key}/nodes"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
    };
    let mut actionable = Vec::new();
    let mut blocked = Vec::new();
    let mut seen = std::collections::HashSet::new();
    /* `asked` first and merged in whole: `all` is capped at a page, and a PR that
    asked for you must not be the one the cap drops. */
    for n in nodes("asked").iter().chain(nodes("all")) {
        // A search over issues answers `{}` for anything that is not a pull
        // request, and `null` for a node it could not read at all.
        let Some(number) = n.get("number").and_then(Value::as_u64) else {
            continue;
        };
        if !seen.insert(number) {
            continue;
        }
        let labels = label_names(n);
        /* **Dropped here rather than in the search, and that is the trade.** A
        `-label:` term would spend the page cap better, but GitHub's search has no
        way to say "none of these" that survives a label name with a comma in it,
        and a row wrongly kept is visible while a row wrongly dropped is not. */
        if has_label(&labels, &rules.skip_labels) {
            continue;
        }
        /* **Applied here as well as in the search, and the `asked` rows are why.**
        Two searches come back and only `all` carries the query's terms: `asked` is
        merged in whole, deliberately, so the page cap can never drop a PR that
        asked for you. That means a required label put only in the query let every
        asked row through unlabelled — which reads as the setting not working. So
        the query term is an optimisation that spends the cap better, and this is
        the rule. */
        if !rules.filter.labels_any.is_empty() && !has_label(&labels, &rules.filter.labels_any) {
            continue;
        }
        /* The same hole, for the one term that is not configurable: `asked` does
        not carry `-author:@me` either, and GitHub will let a team request land on
        your own PR. You cannot review it, so it is not a queue row. */
        if n.pointer("/author/login").and_then(Value::as_str) == Some(viewer.as_str()) {
            continue;
        }
        let mut r = row(n, &viewer, &asked);
        if rules.filter.requested_only && !r.requested.unwrap_or(false) {
            continue;
        }
        if rules.filter.hide_drafts && r.is_draft {
            continue;
        }
        /* **Everything but the draft**, and that word matters. `draft` is one of
        the three blockers, so "drop the blocked ones" would have dropped drafts
        as well — making the checkbox beside this one a subset of it, with its own
        label promising something narrower. Two controls that overlap silently are
        worse than one, so each now means exactly what it says. */
        if rules.filter.hide_blocked && r.blockers.iter().any(|b| b != "draft") {
            continue;
        }
        /* **The ranks the built-in leaves empty, filled by the checkout's own
        convention.** 0 and 1 have meant `stopper` and `prio` since the queue was
        a script, and the built-in emits neither because a label means nothing
        outside the team that agreed it. Configured, it is that team speaking, so
        the rank is theirs to set. 6 is the contract's `sidequest`: last among the
        actionable, still above the fold. */
        // Where the checkout's rules put it. `prio` is left alone: see `rank`.
        r.rank = band(r.prio, &labels, rules);
        if r.blockers.is_empty() {
            actionable.push(r);
        } else {
            blocked.push(r);
        }
    }
    /* **Requested, then age.** Oldest first within each, which is the rest of the
    ordering: the thing it replaced sorted on label names first and used age only
    to break a tie, so on a repo with no such labels every row tied and the age
    was doing all the work anyway. Requested leads because the queue now holds
    PRs nobody asked you about, and one that did ask must not sit under them. */
    let oldest_first = |a: &Review, b: &Review| {
        /* **The band leads, and with nothing configured it changes nothing.**
        Sorting on `prio` itself would have been the obvious line and it is wrong:
        the queue emits 2 for "named you" and 3 for "asked your team", which tie
        under `requested` today and are broken by age — so leading with `prio`
        would quietly reorder every queue that configures nothing at all. `band`
        maps both of those to one number until a checkout says otherwise, and
        `requested` below it then keeps doing the work it always did. */
        a.rank
            .cmp(&b.rank)
            .then(b.requested.cmp(&a.requested))
            .then(b.age_hours.total_cmp(&a.age_hours))
    };
    actionable.sort_by(oldest_first);
    blocked.sort_by(oldest_first);

    Ok(ReviewQueue {
        login: viewer,
        // What the page could not carry, so a queue longer than one page says so
        // rather than quietly being the first fifty.
        skipped: total.saturating_sub(
            nodes("all")
                .iter()
                .filter(|n| n.get("number").is_some())
                .count() as u32,
        ),
        total,
        actionable,
        blocked,
    })
}

/// Where a row sits, as one number: lower is nearer the top.
///
/// **One scale for two things that used to be in different places** — the label
/// ranks `prio` carries, and the two request kinds, which were a hardcoded tie.
/// Both now land here, so "a team request outranks a label" is a thing a checkout
/// can say and the comparator does not have to know which of the two it is
/// reading.
///
/// 0 is a kind configured `Top`, 1 a `high_labels` row, 2 a request left at
/// `Above`, 3 everything else, 4 a `low_labels` row. With nothing configured only
/// 2 and 3 are reachable, which is exactly the order this queue has always had:
/// requested first, then age.
fn band(prio: u32, labels: &[String], rules: &QueueRules) -> u8 {
    let place = |p: crate::config::QueuePlace| match p {
        crate::config::QueuePlace::Top => 0,
        crate::config::QueuePlace::Above => 2,
        crate::config::QueuePlace::Normal => 3,
    };
    /* A configured command may still emit the label ranks 0 and 1 itself, and
    those are its ranking rather than this checkout's rules — kept at the top,
    where that scale has always put them. */
    let placed = match prio {
        0 | 1 => 1,
        2 => place(rules.asked_of_me),
        3 => place(rules.asked_of_team),
        _ => 3,
    };
    if has_label(labels, &rules.high_labels) {
        return placed.min(1);
    }
    /* **A lift wins over a sink**: a label only sinks a row that nothing placed
    above the rest. So a chore label does not bury a PR somebody asked you to
    look at, while the same label on a PR nobody asked about does sink it.
    Setting that kind of ask to `Normal` is how you say the label should win. */
    if placed >= 3 && has_label(labels, &rules.low_labels) {
        return 4;
    }
    placed
}

/// Whether any of `labels` is one of `names`, the way GitHub's own search reads
/// them: case folded and trimmed.
///
/// **The two halves had to agree and did not.** `label:Needs-Review` matches a
/// `needs-review` label at GitHub, so a checkout that typed it passed the search
/// and then had every row dropped by the rule meant to keep them — an empty queue
/// with nothing to say why. Trimmed too, because `config.json` is hand-edited and
/// a space after a comma is not a different label.
fn has_label(labels: &[String], names: &[String]) -> bool {
    names.iter().any(|n| {
        let want = n.trim().to_lowercase();
        !want.is_empty() && labels.iter().any(|l| l.trim().to_lowercase() == want)
    })
}

/// The label names on a PR node, as GitHub spells them.
fn label_names(n: &Value) -> Vec<String> {
    n.pointer("/labels/nodes")
        .and_then(Value::as_array)
        .map(|ls| {
            ls.iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// One PR, as the pane reads it.
fn row(n: &Value, viewer: &str, asked: &std::collections::HashSet<u64>) -> Review {
    let number = n.get("number").and_then(Value::as_u64).unwrap_or(0);
    let requested = asked.contains(&number);
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
        number,
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
        /* 5 is the contract's "other": nobody asked you. Named yourself is
        always a request, so only the request decides between 3 and 5. */
        prio: if requested_of_you {
            2
        } else if requested {
            3
        } else {
            5
        },
        /* Asked again *after* you reviewed: GitHub drops a reviewer from the
        requested set once they review, so being in it now with a review of
        yours behind it is a re-request. Every PR is listed now, and without the
        first half every one you ever reviewed said "re-requested". */
        needs_re_review: (requested || requested_of_you) && reviewers.contains(viewer),
        is_draft,
        blockers,
        reviewers: reviewers.len() as u32,
        // One more page of the search to fill, and the column hides itself until
        // something provides it. Not worth a second round trip per poll.
        changed_files: None,
        checks,
        requested: Some(requested || requested_of_you),
        // Set by the caller, which is the only place the rules are known.
        rank: 0,
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

/// The queue for this checkout, from whichever of the three sources it names.
///
/// **The source is asked rather than inferred now**, which is the whole of issue
/// #41's second half: "the built-in, filtered" has no spelling in "is there a
/// command". `Config::reviews_source` answers from the old shape when the field
/// is absent, so a checkout that never heard of this keeps the queue it had.
///
/// The command keeps its precedence deliberately. A team's real ranking lives in
/// its own tooling — this repo's own checkout points `reviews_command` at a
/// `mise` task — and a default that overrode it would be the daemon insisting
/// where CLAUDE.md says it should defer.
pub fn fetch(
    main: &Path,
    timeout_secs: u64,
    source: ReviewsSource,
    command: &[String],
    rules: &QueueRules,
    repo: Option<&str>,
    token: Option<&str>,
) -> ReviewState {
    /* A command selected but never written is nothing to run, and an empty argv
    reaching `run_bounded` is a spawn failure reported as a broken queue. Reads
    as "no review queue here", which is what it is. */
    if source == ReviewsSource::Command && command.is_empty() {
        return ReviewState::Off;
    }
    if source == ReviewsSource::Command {
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
    /* The built-in is the custom one with nothing configured — same code, and the
    default `QueueRules` is the behaviour this had before any of it existed. */
    let none = QueueRules::default();
    let rules = if source == ReviewsSource::Custom {
        rules
    } else {
        &none
    };
    match builtin(token, owner, name, rules) {
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
        // A command's rows are its own ranking; see `Review::requested`.
        requested: None,
        /* **Flat for every row, and that is what leaves a command's own order
        alone.** `rank` is this checkout's rules, which a command's rows are not
        subject to — so with every row equal here the sort falls through to
        `requested` and the age, which is the comparator this path has always
        had. (`prio` has never been in it: a command's scale reaches the pane's
        colour and its reason word, not its order.) */
        rank: 0,
    }
}

#[cfg(test)]
mod tests {
    // Only the tests build a filter by hand: the queue itself reads one off the
    // rules it is handed.
    use crate::config::QueueFilter;

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
            fetch(
                Path::new("/nonexistent"),
                1,
                ReviewsSource::Default,
                &[],
                &QueueRules::default(),
                None,
                None
            ),
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
            ReviewsSource::Default,
            &[],
            &QueueRules::default(),
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
    /// request, a draft, a conflict, a failing check, a re-review, and a PR
    /// nobody asked you about.
    /// The same answer with labels on three of the rows, so the configured lists
    /// have something to match. Built by patching [`answer`] rather than by a
    /// second copy of it: the ordering the other tests assert is the thing these
    /// are changing, and two fixtures would drift apart.
    fn labelled(by_number: &[(u64, &str)]) -> Value {
        let mut v = answer();
        for key in ["asked", "all"] {
            let Some(nodes) = v
                .pointer_mut(&format!("/data/{key}/nodes"))
                .and_then(Value::as_array_mut)
            else {
                continue;
            };
            for n in nodes.iter_mut() {
                let Some(number) = n.get("number").and_then(Value::as_u64) else {
                    continue;
                };
                let names: Vec<Value> = by_number
                    .iter()
                    .filter(|(k, _)| *k == number)
                    .map(|(_, l)| serde_json::json!({ "name": l }))
                    .collect();
                n["labels"] = serde_json::json!({ "nodes": names });
            }
        }
        v
    }

    fn numbers(q: &ReviewQueue) -> Vec<u64> {
        q.actionable.iter().map(|r| r.number).collect()
    }

    #[test]
    fn default_rules_leave_the_queue_exactly_as_it_was() {
        let plain = from_graphql(&answer(), &QueueRules::default()).expect("parses");
        // Every row carries labels now, and none of them is configured.
        let with_labels = from_graphql(
            &labelled(&[(14, "chore"), (10, "bug"), (11, "docs")]),
            &QueueRules::default(),
        )
        .expect("parses");
        assert_eq!(
            numbers(&plain),
            numbers(&with_labels),
            "a label nothing names must not move a row"
        );
    }

    #[test]
    fn a_high_label_outranks_a_review_asked_of_you_by_name() {
        let rules = QueueRules {
            high_labels: vec!["security".into()],
            ..QueueRules::default()
        };
        // 14 is the row nobody asked you about, and it sorts last without a label.
        let plain = from_graphql(&answer(), &QueueRules::default()).expect("parses");
        assert_ne!(
            numbers(&plain).first(),
            Some(&14),
            "14 does not lead by age"
        );

        let q = from_graphql(&labelled(&[(14, "security")]), &rules).expect("parses");
        assert_eq!(
            numbers(&q).first(),
            Some(&14),
            "a high label leads, over the PR that named you"
        );

        /* **And it moved the row without rewriting what the row is.** The pane
        colours on `prio` and writes its reason word from it — amber for "named
        you", the word `team` for a team ask — so a rule about where a row sits
        must leave those alone. The first version of this wrote `prio = 1` and
        turned a labelled team ask red with the word `prio` on it. */
        let named = q
            .actionable
            .iter()
            .find(|r| r.number == 10)
            .expect("the named ask is still here");
        assert_eq!(named.prio, 2, "named you, and the pane can still say so");
        let team = q
            .actionable
            .iter()
            .find(|r| r.number == 11)
            .expect("the team ask is still here");
        assert_eq!(team.prio, 3, "a team ask, and the pane can still say so");

        let lifted = q
            .actionable
            .iter()
            .find(|r| r.number == 14)
            .expect("the lifted row is here");
        assert_eq!(
            lifted.prio, 5,
            "lifted by a label, and still nobody's request"
        );
    }

    #[test]
    fn a_low_label_sinks_a_row_without_hiding_it() {
        let rules = QueueRules {
            low_labels: vec!["later".into()],
            ..QueueRules::default()
        };
        let plain = numbers(&from_graphql(&answer(), &QueueRules::default()).expect("parses"));
        // 14 is the row nobody asked you about, so nothing lifts it.
        assert!(plain.contains(&14));

        let q = from_graphql(&labelled(&[(14, "later")]), &rules).expect("parses");
        let got = numbers(&q);
        assert_eq!(got.len(), plain.len(), "sunk, not dropped");
        assert_eq!(
            got.last(),
            Some(&14),
            "and it sorts last among the actionable"
        );
    }

    #[test]
    fn a_lift_wins_over_a_sink_until_you_say_otherwise() {
        let rules = QueueRules {
            low_labels: vec!["later".into()],
            ..QueueRules::default()
        };
        let lead = *numbers(&from_graphql(&answer(), &QueueRules::default()).expect("parses"))
            .first()
            .expect("the queue has rows");

        // The leading row is one that asked for you, so a low label must not hide
        // the ask behind a word the author chose for the change.
        let q = from_graphql(&labelled(&[(lead, "later")]), &rules).expect("parses");
        assert_eq!(
            numbers(&q).first(),
            Some(&lead),
            "a request outranks a low label"
        );

        // Unless you say that request has no special place, and then it does sink.
        let normal = QueueRules {
            asked_of_me: crate::config::QueuePlace::Normal,
            asked_of_team: crate::config::QueuePlace::Normal,
            ..rules.clone()
        };
        let q = from_graphql(&labelled(&[(lead, "later")]), &normal).expect("parses");
        assert_eq!(
            numbers(&q).last(),
            Some(&lead),
            "and with no place, the label wins"
        );
    }

    #[test]
    fn a_required_label_reaches_the_rows_the_query_did_not_narrow() {
        let rules = QueueRules {
            filter: QueueFilter {
                labels_any: vec!["needs-review".into()],
                ..QueueFilter::default()
            },
            ..QueueRules::default()
        };
        /* Only 10 carries it. 15 comes from the `asked` search, which the GitHub
        query narrows for `all` alone — so this is the case that was wrong: every
        asked row appeared whatever its labels. */
        let q = from_graphql(&labelled(&[(10, "needs-review")]), &rules).expect("parses");
        let got = numbers(&q);
        assert!(
            !got.contains(&15),
            "a row without the label must not appear: {got:?}"
        );
    }

    /* **The same label in two lists, which nothing stops you typing.** Every pair
    has to have an answer, and the answer has to be the one the help text claims,
    or the first person to do it by accident learns the real rule the hard way. */
    #[test]
    fn a_label_in_two_lists_resolves_the_same_way_every_time() {
        let both_ends = QueueRules {
            high_labels: vec!["urgent".into()],
            low_labels: vec!["urgent".into()],
            ..QueueRules::default()
        };
        let q = numbers(&from_graphql(&labelled(&[(14, "urgent")]), &both_ends).expect("parses"));
        assert_eq!(
            q.first(),
            Some(&14),
            "lifted, not sunk: the lift is read first"
        );

        let lift_and_drop = QueueRules {
            high_labels: vec!["urgent".into()],
            skip_labels: vec!["urgent".into()],
            ..QueueRules::default()
        };
        let q =
            numbers(&from_graphql(&labelled(&[(14, "urgent")]), &lift_and_drop).expect("parses"));
        assert!(
            !q.contains(&14),
            "dropped: a row that needs no review has nowhere to be lifted to"
        );

        // Required and skipped at once is a queue that can hold nothing, and it
        // holds nothing rather than quietly ignoring one of the two.
        let contradiction = QueueRules {
            filter: QueueFilter {
                labels_any: vec!["x".into()],
                ..QueueFilter::default()
            },
            skip_labels: vec!["x".into()],
            ..QueueRules::default()
        };
        let q = from_graphql(&labelled(&[(14, "x"), (10, "x")]), &contradiction).expect("parses");
        assert!(
            q.actionable.is_empty() && q.blocked.is_empty(),
            "an empty queue, not a surprise"
        );
    }

    /* **`Only ones asking for me` keeps the team asks**, because a team ask is an
    ask. The place settings then say where they sit, which is the combination that
    reads as a contradiction until you try it. */
    #[test]
    fn asking_for_me_keeps_team_asks_and_the_place_still_moves_them() {
        let rules = QueueRules {
            filter: QueueFilter {
                requested_only: true,
                ..QueueFilter::default()
            },
            asked_of_team: crate::config::QueuePlace::Normal,
            ..QueueRules::default()
        };
        let q = from_graphql(&answer(), &rules).expect("parses");
        let got = numbers(&q);
        assert!(got.contains(&11), "the team ask is still an ask: {got:?}");
        assert!(
            got.iter().position(|n| *n == 10) < got.iter().position(|n| *n == 11),
            "and it sits below the one that named you: {got:?}"
        );
    }

    /* **The rules are ignored by the two sources that are not `custom`.** The panel
    saves them whichever source is selected, so this is the guard on that: picking
    `default` must give the default queue however much is typed into the fields. */
    #[test]
    fn only_the_custom_source_applies_the_rules() {
        let loud = QueueRules {
            skip_labels: vec!["chore".into()],
            ..QueueRules::default()
        };
        let plain = numbers(&from_graphql(&answer(), &QueueRules::default()).expect("parses"));
        let applied = numbers(&from_graphql(&labelled(&[(14, "chore")]), &loud).expect("parses"));
        assert!(!applied.contains(&14), "custom drops it");
        assert!(plain.contains(&14), "and the default does not");
    }

    /* **The two hide toggles do not overlap**, which they did: `draft` is one of
    the three blockers, so dropping "the blocked ones" dropped drafts too and the
    checkbox beside it did nothing anybody could see. */
    #[test]
    fn hiding_drafts_and_hiding_blocked_are_independent() {
        let draft_only = QueueRules {
            filter: QueueFilter {
                hide_drafts: true,
                ..QueueFilter::default()
            },
            ..QueueRules::default()
        };
        let q = from_graphql(&answer(), &draft_only).expect("parses");
        let below: Vec<u64> = q.blocked.iter().map(|r| r.number).collect();
        assert!(!below.contains(&12), "12 is the draft: {below:?}");
        assert!(
            below.contains(&13),
            "13 conflicts and fails, and stays: {below:?}"
        );

        let blocked_only = QueueRules {
            filter: QueueFilter {
                hide_blocked: true,
                ..QueueFilter::default()
            },
            ..QueueRules::default()
        };
        let q = from_graphql(&answer(), &blocked_only).expect("parses");
        let below: Vec<u64> = q.blocked.iter().map(|r| r.number).collect();
        assert!(below.contains(&12), "the draft stays: {below:?}");
        assert!(
            !below.contains(&13),
            "and the conflicting one goes: {below:?}"
        );
    }

    /* **GitHub matches a label without caring about case and this did**, so a
    checkout that typed `Needs-Review` passed the search and then had every row
    dropped by the rule meant to keep them. The two halves have to agree. */
    #[test]
    fn a_label_matches_however_it_is_typed() {
        let rules = QueueRules {
            filter: QueueFilter {
                labels_any: vec!["Needs-Review".into()],
                ..QueueFilter::default()
            },
            high_labels: vec![" SECURITY ".into()],
            ..QueueRules::default()
        };
        let q = from_graphql(
            &labelled(&[(14, "needs-review"), (14, "security"), (10, "needs-review")]),
            &rules,
        )
        .expect("parses");
        let got = numbers(&q);
        assert!(got.contains(&14), "the required label matched: {got:?}");
        assert_eq!(got.first(), Some(&14), "and so did the one that lifts it");
    }

    #[test]
    fn the_two_request_kinds_are_placed_apart() {
        // 10 asked you by name and 11 asked a team you are in; 11 is the older, so
        // with one band for both — which is the default — age puts 11 first.
        let tied = numbers(&from_graphql(&answer(), &QueueRules::default()).expect("parses"));
        assert!(
            tied.iter().position(|n| *n == 11) < tied.iter().position(|n| *n == 10),
            "tied, the older of the two leads: {tied:?}"
        );

        // Told the team ask has no special place, the named one leads instead.
        let team_normal = QueueRules {
            asked_of_team: crate::config::QueuePlace::Normal,
            ..QueueRules::default()
        };
        let q = numbers(&from_graphql(&answer(), &team_normal).expect("parses"));
        assert!(
            q.iter().position(|n| *n == 10) < q.iter().position(|n| *n == 11),
            "the named ask now leads the team one: {q:?}"
        );

        // And a named ask put on top outranks a high label, which nothing else does.
        let both = QueueRules {
            asked_of_me: crate::config::QueuePlace::Top,
            high_labels: vec!["security".into()],
            ..QueueRules::default()
        };
        let q = numbers(&from_graphql(&labelled(&[(14, "security")]), &both).expect("parses"));
        assert_eq!(
            q.first(),
            Some(&15),
            "the oldest named ask leads the high label: {q:?}"
        );
    }

    #[test]
    fn a_no_review_needed_label_drops_the_row_from_both_lists() {
        let rules = QueueRules {
            skip_labels: vec!["no-review".into()],
            ..QueueRules::default()
        };
        // 12 is the draft and 13 the conflicting one: both sit below the fold, so
        // this also proves the drop reaches the list the fold holds.
        let q = from_graphql(&labelled(&[(14, "no-review"), (13, "no-review")]), &rules)
            .expect("parses");
        assert!(
            !numbers(&q).contains(&14),
            "dropped from the actionable list"
        );
        assert!(
            !q.blocked.iter().any(|r| r.number == 13),
            "and from the blocked list"
        );
    }

    #[test]
    fn the_filter_toggles_drop_what_they_name() {
        let drafts = QueueRules {
            filter: QueueFilter {
                hide_drafts: true,
                ..QueueFilter::default()
            },
            ..QueueRules::default()
        };
        let q = from_graphql(&answer(), &drafts).expect("parses");
        assert!(
            !q.blocked.iter().any(|r| r.is_draft),
            "no draft survives hide_drafts"
        );

        let asked_only = QueueRules {
            filter: QueueFilter {
                requested_only: true,
                ..QueueFilter::default()
            },
            ..QueueRules::default()
        };
        let q = from_graphql(&answer(), &asked_only).expect("parses");
        assert!(
            q.actionable
                .iter()
                .chain(q.blocked.iter())
                .all(|r| r.requested == Some(true)),
            "requested_only leaves only the rows that asked"
        );
        assert!(
            !numbers(&q).contains(&14),
            "and 14, which nobody asked you about, is gone"
        );
    }

    fn answer() -> Value {
        // The rows `all` and `asked` share, written once: GitHub answers both
        // searches with the same fragment.
        let rows = r#"{"number":14,"title":"oldest of all, nobody asked you","url":"u14","isDraft":false,
             "createdAt":"2026-08-20T12:00:00Z","mergeable":"MERGEABLE",
             "author":{"login":"ola"},
             "reviewRequests":{"nodes":[]},
             "latestReviews":{"nodes":[]},
             "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}},
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
            {}"#;
        let asked: Vec<Value> = serde_json::from_str::<Vec<Value>>(&format!("[{rows}]"))
            .unwrap()
            .into_iter()
            .filter(|n| matches!(n.get("number").and_then(Value::as_u64), Some(10..=13)))
            .collect();
        // #15 asked for you and is older than the page `all` returned, so only
        // `asked` carries it.
        let mut asked = asked;
        asked.push(
            serde_json::json!({"number":15,"title":"asked, beyond the page","url":"u15",
            "isDraft":false,"createdAt":"2026-08-01T12:00:00Z","mergeable":"MERGEABLE",
            "author":{"login":"dana"},
            "reviewRequests":{"nodes":[{"requestedReviewer":{"login":"me"}}]},
            "latestReviews":{"nodes":[]},
            "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS"}}}]}}),
        );
        v(&format!(
            r#"{{"data":{{"viewer":{{"login":"me"}},"asked":{{"nodes":{}}},"all":{{"issueCount":8,"nodes":[{rows}]}}}}}}"#,
            serde_json::to_string(&asked).unwrap()
        ))
    }

    #[test]
    fn requested_then_age_orders_the_queue() {
        let q = from_graphql(&answer(), &QueueRules::default()).expect("a captured answer parses");
        assert_eq!(
            q.actionable.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![15, 11, 10, 14],
            "requested first, oldest first within it: #15 only `asked` carried and is \
             the oldest request; #14 is older than #10 and #11 and still last, nobody \
             asked you"
        );
    }

    /// "Re-requested" is a request with your review behind it, not any PR you
    /// once reviewed: GitHub drops you from the requested set when you review.
    #[test]
    fn a_pr_you_reviewed_and_nobody_re_requested_is_not_a_re_review() {
        let row = |requested: bool| {
            let mut reviewed = serde_json::json!({"number":30,"title":"t","url":"u",
                "isDraft":false,"createdAt":"2026-09-01T12:00:00Z","mergeable":"MERGEABLE",
                "author":{"login":"ola"},"reviewRequests":{"nodes":[]},
                "latestReviews":{"nodes":[{"author":{"login":"me"}}]},
                "commits":{"nodes":[]}});
            if requested {
                reviewed["reviewRequests"]["nodes"] =
                    serde_json::json!([{"requestedReviewer":{"login":"me"}}]);
            }
            let asked = if requested {
                vec![reviewed.clone()]
            } else {
                vec![]
            };
            let q = from_graphql(
                &serde_json::json!({"data":{"viewer":{"login":"me"},
                "asked":{"nodes":asked},"all":{"issueCount":1,"nodes":[reviewed]}}}),
                &QueueRules::default(),
            )
            .expect("parse");
            q.actionable[0].needs_re_review
        };
        assert!(!row(false), "reviewed once, not asked again");
        assert!(row(true), "asked again after your review");
    }

    /// Every open PR is listed, and the ones nobody asked you about say so: the
    /// pane's "requested only" filter reads that field and nothing else.
    #[test]
    fn a_pr_nobody_asked_you_about_is_listed_and_says_so() {
        let q = from_graphql(&answer(), &QueueRules::default()).expect("parse");
        let other = q
            .actionable
            .iter()
            .find(|r| r.number == 14)
            .expect("#14 is listed");
        assert_eq!(other.requested, Some(false));
        assert_eq!(other.prio, 5, "the contract's other");
        assert!(q
            .actionable
            .iter()
            .chain(q.blocked.iter())
            .filter(|r| r.number != 14)
            .all(|r| r.requested == Some(true)));
    }

    #[test]
    fn amber_is_being_named_yourself_and_a_team_request_is_not() {
        let q = from_graphql(&answer(), &QueueRules::default()).expect("parse");
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
        let q = from_graphql(&answer(), &QueueRules::default()).expect("parse");
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
        let q = from_graphql(&answer(), &QueueRules::default()).expect("parse");
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
        let q = from_graphql(&answer(), &QueueRules::default()).expect("parse");
        assert_eq!(
            q.actionable.len() + q.blocked.len(),
            6,
            "the node that is not a pull request is dropped, and #15 from `asked` is in"
        );
        assert_eq!(q.total, 8, "what GitHub said the search holds");
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
