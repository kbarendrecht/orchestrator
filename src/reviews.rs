use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::process::Command;

/// PRs where your review is requested — other people's work (§6b).
///
/// The source is scienta's own `mise run reviews --json`, which already applies
/// a richer ranking than §6b describes: prio labels, personal versus team
/// request, re-review detection, reviewer-count tiebreak. The daemon consumes
/// that shape rather than imposing the one §6b invented.
#[derive(Debug, Clone, Serialize)]
pub struct Review {
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
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewState {
    Ok(ReviewQueue),
    Degraded { reason: String },
}

impl Default for ReviewState {
    fn default() -> Self {
        ReviewState::Degraded {
            reason: "not polled yet".into(),
        }
    }
}

/// Version the daemon understands. The source does not emit one yet, so its
/// absence is accepted; a *different* one is not.
const KNOWN_VERSION: u64 = 1;

pub fn fetch(main: &Path, timeout_secs: u64) -> ReviewState {
    match run(main, timeout_secs) {
        Ok(q) => ReviewState::Ok(q),
        Err(e) => ReviewState::Degraded {
            reason: format!("{e:#}"),
        },
    }
}

fn run(main: &Path, timeout_secs: u64) -> Result<ReviewQueue> {
    // `timeout` rather than a thread: the child is a bun process that talks to
    // GitHub, and orphaning it would leave the next poll racing this one.
    let out = Command::new("timeout")
        .arg(timeout_secs.to_string())
        .args(["mise", "run", "reviews", "--json"])
        .current_dir(main)
        .output()
        .context("running `mise run reviews --json`")?;

    if !out.status.success() {
        let tail: String = String::from_utf8_lossy(&out.stderr)
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .join(" / ");
        bail!(
            "reviews exited {}: {}",
            out.status.code().unwrap_or(-1),
            if tail.is_empty() { "no stderr" } else { &tail }
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
    parse(&v)
}

fn parse(v: &Value) -> Result<ReviewQueue> {
    let entries = |key: &str| -> Vec<Review> {
        v.get(key)
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(parse_entry).collect())
            .unwrap_or_default()
    };
    let actionable = entries("actionable");
    let blocked = entries("blocked");
    if v.get("actionable").is_none() && v.get("blocked").is_none() {
        bail!("reviews output has neither `actionable` nor `blocked`");
    }
    Ok(ReviewQueue {
        login: v
            .get("forLogin")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        actionable,
        blocked,
        total: v.get("total").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
        skipped: v.get("skipped").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
    })
}

fn parse_entry(e: &Value) -> Option<Review> {
    let pr = e.get("pr")?;
    let number = pr.get("number")?.as_u64()?;

    // The source has expressed age as both `ageHours` and `ageDays`; take
    // whichever is there rather than depending on which day it is.
    let age_hours = e
        .get("ageHours")
        .and_then(|n| n.as_f64())
        .or_else(|| e.get("ageDays").and_then(|n| n.as_f64()).map(|d| d * 24.0))
        .unwrap_or(0.0);

    Some(Review {
        number,
        title: pr.get("title").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        // Deriving the url keeps a row clickable even if the field is dropped.
        url: pr
            .get("url")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("https://github.com/ScientaNL/Scienta/pull/{number}")),
        author: pr
            .get("author")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string(),
        age_hours,
        prio: e.get("prio").and_then(|n| n.as_u64()).unwrap_or(9) as u32,
        needs_re_review: e
            .get("needsReReview")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        is_draft: pr.get("isDraft").and_then(|b| b.as_bool()).unwrap_or(false),
        blockers: e
            .get("blockers")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        reviewers: e
            .get("reviewers")
            .and_then(|a| a.as_array())
            .map(|a| a.len() as u32)
            .unwrap_or(0),
        changed_files: pr
            .get("changedFiles")
            .and_then(|n| n.as_u64())
            .map(|n| n as u32),
        checks: pr
            .get("checks")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn reads_the_shape_the_source_actually_emits() {
        let q = parse(&v(r#"{
            "forLogin":"kbarendrecht","total":16,"skipped":4,
            "actionable":[{"pr":{"number":4771,"title":"Refactor billing exports",
                "url":"https://github.com/x/y/pull/4771","author":"joost","isDraft":false},
                "reviewers":["a","b"],"blockers":[],"needsReReview":false,
                "ageHours":52.5,"prio":2}],
            "blocked":[],"ownBlocked":[]}"#))
        .expect("parse");
        assert_eq!(q.login, "kbarendrecht");
        assert_eq!(q.actionable.len(), 1);
        let r = &q.actionable[0];
        assert_eq!(r.number, 4771);
        assert_eq!(r.author, "joost");
        assert_eq!(r.reviewers, 2);
        assert_eq!(r.prio, 2);
        assert!((r.age_hours - 52.5).abs() < 0.01);
        // Not emitted yet, so the column is omitted rather than invented.
        assert!(r.changed_files.is_none());
    }

    #[test]
    fn accepts_age_in_days_as_well_as_hours() {
        let q = parse(&v(r#"{"actionable":[{"pr":{"number":1,"title":"t","author":"a"},
            "ageDays":2}],"blocked":[]}"#))
        .unwrap();
        assert!((q.actionable[0].age_hours - 48.0).abs() < 0.01);
    }

    #[test]
    fn derives_a_url_when_the_field_is_missing() {
        let q = parse(&v(r#"{"actionable":[{"pr":{"number":99,"title":"t","author":"a"}}],
            "blocked":[]}"#))
        .unwrap();
        assert!(q.actionable[0].url.ends_with("/pull/99"));
    }

    #[test]
    fn output_of_the_wrong_shape_is_an_error_not_an_empty_queue() {
        // The failure that would cost a colleague a day.
        assert!(parse(&v(r#"{"something":"else"}"#)).is_err());
    }

    #[test]
    fn an_unknown_version_degrades() {
        let e = ReviewState::Degraded { reason: "x".into() };
        assert!(matches!(e, ReviewState::Degraded { .. }));
        // And the default before the first poll is degraded, not an empty Ok.
        assert!(matches!(ReviewState::default(), ReviewState::Degraded { .. }));
    }

    #[test]
    fn an_empty_but_valid_queue_is_ok_not_degraded() {
        let q = parse(&v(r#"{"forLogin":"me","actionable":[],"blocked":[]}"#)).unwrap();
        assert!(q.actionable.is_empty());
    }
}
