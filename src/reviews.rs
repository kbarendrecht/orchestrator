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
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export, export_to = "../web/snapshot.d.ts"))]
pub struct Review {
    #[cfg_attr(test, ts(type = "number"))]
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    /// Age in hours, however the source expressed it.
    pub age_hours: f64,
    /// Source rank: 0 stopper, 1 prio, 2 requested of you, 3 of your team,
    /// 4 re-review, 5 other, 6 sidequest.
    pub prio: u32,
    pub needs_re_review: bool,
    pub is_draft: bool,
    /// `conflicts`, `failing checks`, … — non-empty means it is waiting on
    /// someone else and sinks to the bottom.
    pub blockers: Vec<String>,
    pub reviewers: u32,
    /// Review cost. Absent until the source grows `changedFiles`; the column is
    /// omitted rather than faked (see docs/reviews-json.md).
    pub changed_files: Option<u32>,
    pub checks: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export, export_to = "../web/snapshot.d.ts"))]
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
#[cfg_attr(test, derive(ts_rs::TS), ts(export, export_to = "../web/snapshot.d.ts"))]
pub enum ReviewState {
    Ok(ReviewQueue),
    Degraded { reason: String },
    /// Before the first poll lands. Distinct from `Degraded` so startup does
    /// not read as a broken command — and so it never becomes a TODO entry.
    #[default]
    Pending,
    /// No review-queue command configured. Not a fault — this repo simply has
    /// no such source — so, like `Pending`, it never becomes a TODO finding and
    /// the pane says "not configured" rather than "unavailable".
    Off,
}

/// Version the daemon understands. The source does not emit one yet, so its
/// absence is accepted; a *different* one is not.
const KNOWN_VERSION: u64 = 1;

/// The queue that ships, kept as a real script rather than a string literal so it
/// can be read, run and diffed. No dependencies, so it works from the config dir
/// where nothing has been installed.
const DEFAULT_SCRIPT: &str = include_str!("../reviews/default.js");

/// Where the ejected copy lives.
pub fn default_script_path() -> Result<std::path::PathBuf> {
    Ok(crate::config::Config::config_dir()?.join("reviews.js"))
}

/// Eject the built-in queue to the config dir, **without ever clobbering it.**
///
/// Written once, then yours: the whole point is that the ranking is an opinion you
/// can edit, and a daemon that rewrote it on every start would silently undo that.
/// Absent is the one case it writes, which also means deleting the file is how you
/// ask for the default back.
///
/// Returns the path either way, so a caller can point `reviews_command` at it
/// whether this run created it or a previous one did.
pub fn eject_default_script() -> Result<std::path::PathBuf> {
    let path = default_script_path()?;
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, DEFAULT_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    tracing::info!(path = %path.display(), "ejected the default review queue; edit it or point reviews_command elsewhere");
    Ok(path)
}

pub fn fetch(main: &Path, timeout_secs: u64, command: &[String], repo: Option<&str>) -> ReviewState {
    if command.is_empty() {
        return ReviewState::Off;
    }
    match run(main, timeout_secs, command, repo) {
        Ok(q) => ReviewState::Ok(q),
        Err(e) => ReviewState::Degraded {
            reason: format!("{e:#}"),
        },
    }
}

fn run(main: &Path, timeout_secs: u64, command: &[String], repo: Option<&str>) -> Result<ReviewQueue> {
    let out = crate::proc::run_bounded(main, timeout_secs, command, "reviews")?;

    if !out.status.success() {
        bail!(
            "reviews exited {}: {}",
            out.status.code().unwrap_or(-1),
            crate::proc::stderr_tail(&out.stderr)
        );
    }

    let v: Value =
        serde_json::from_slice(&out.stdout).context("reviews output was not JSON")?;
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
        let q = parse_t(v(r#"{"actionable":[{"pr":{"number":1,"title":"t","author":"a"},
            "ageDays":2}],"blocked":[]}"#))
        .unwrap();
        assert!((q.actionable[0].age_hours - 48.0).abs() < 0.01);
    }

    #[test]
    fn derives_a_url_from_the_configured_repo_when_the_field_is_missing() {
        let q = parse(
            v(r#"{"actionable":[{"pr":{"number":99,"title":"t","author":"a"}}],
            "blocked":[]}"#),
            Some("acme/monorepo"),
        )
        .unwrap();
        assert_eq!(q.actionable[0].url, "https://github.com/acme/monorepo/pull/99");
    }

    /// It used to name one hardcoded repo here, so everyone else's rows linked
    /// into a repo they could not open. No repo known is now no link.
    #[test]
    fn an_unknown_repo_yields_no_link_rather_than_a_wrong_one() {
        let q = parse(
            v(r#"{"actionable":[{"pr":{"number":99,"title":"t","author":"a"}}],
            "blocked":[]}"#),
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
        assert_eq!(q.actionable.iter().map(|r| r.number).collect::<Vec<_>>(), vec![7, 8]);
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
    fn the_ejected_script_prints_what_the_parser_reads() {
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
    fn an_empty_queue_from_the_script_is_ok() {
        let q = parse(
            v(r#"{"forLogin":"me","total":0,"skipped":0,"actionable":[],"blocked":[]}"#),
            None,
        )
        .expect("empty is valid");
        assert!(q.actionable.is_empty() && q.blocked.is_empty());
    }

    #[test]
    fn no_command_is_off_not_degraded() {
        // A repo with no review-queue source must not read as a broken command —
        // that would nag in the TODO block and colour the pane red for nothing.
        assert!(matches!(
            fetch(Path::new("/nonexistent"), 1, &[], None),
            ReviewState::Off
        ));
    }
}
