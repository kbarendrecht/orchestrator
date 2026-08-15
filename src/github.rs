use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// Where the daemon's GitHub token came from.
///
/// §6 wants a fine-grained PAT with **read scopes only** — `pull_requests`,
/// `checks`, `contents`, `metadata`. The daemon never pushes; `/green` pushes
/// through the agent's own git credentials, so any write scope here is
/// unnecessary blast radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource {
    /// `ORCHD_GITHUB_TOKEN`, injected at daemon start.
    Env,
    /// A `0600` file outside the repo, path from config.
    File,
    /// `gh auth token`. Works out of the box but carries gh's own scopes,
    /// which include write. Surfaced as a warning rather than accepted quietly.
    GhCli,
}

pub struct Token {
    pub value: String,
    pub source: TokenSource,
}

pub fn resolve_token(token_file: Option<&Path>) -> Result<Token> {
    if let Ok(v) = std::env::var("ORCHD_GITHUB_TOKEN") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Ok(Token {
                value: v,
                source: TokenSource::Env,
            });
        }
    }
    if let Some(p) = token_file {
        if p.exists() {
            let raw = std::fs::read_to_string(p)
                .with_context(|| format!("reading {}", p.display()))?;
            let v = raw.trim().to_string();
            if !v.is_empty() {
                warn_if_world_readable(p);
                return Ok(Token {
                    value: v,
                    source: TokenSource::File,
                });
            }
        }
    }
    let out = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .context("no ORCHD_GITHUB_TOKEN, no token file, and `gh auth token` could not be run")?;
    if !out.status.success() {
        bail!(
            "no GitHub token available: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() {
        bail!("`gh auth token` returned nothing");
    }
    Ok(Token {
        value: v,
        source: TokenSource::GhCli,
    })
}

#[cfg(unix)]
fn warn_if_world_readable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = std::fs::metadata(p) {
        let mode = md.permissions().mode() & 0o077;
        if mode != 0 {
            tracing::warn!("{} is readable by others; chmod 600 it", p.display());
        }
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// POST a GraphQL query.
///
/// Shelling out to `curl` rather than linking an HTTP+TLS stack keeps the
/// daemon's build free of a C toolchain, which this machine does not have. The
/// token is passed on stdin, never on the command line, so it cannot be read
/// out of the process table.
pub fn graphql(token: &str, query: &str) -> Result<Value> {
    use std::io::Write;
    use std::process::Stdio;

    let body = serde_json::json!({ "query": query }).to_string();
    let mut child = Command::new("curl")
        .args([
            "-sS",
            "--fail-with-body",
            "-X",
            "POST",
            "-H",
            "@-",
            "-H",
            "Content-Type: application/json",
            // mergeStateStatus is behind this Accept header.
            "-H",
            "Accept: application/vnd.github.merge-info-preview+json",
            "-H",
            "User-Agent: orchd",
            "--data-binary",
            &body,
            "https://api.github.com/graphql",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning curl")?;
    child
        .stdin
        .as_mut()
        .context("curl stdin")?
        .write_all(format!("Authorization: bearer {token}\n").as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "github request failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: Value = serde_json::from_slice(&out.stdout).context("parsing the GraphQL response")?;
    if let Some(errors) = v.get("errors") {
        bail!("github returned errors: {errors}");
    }
    Ok(v)
}

/// The latest published release of `owner/name`, as `(tag, html_url)`.
///
/// Unauthenticated: releases are public and the check must work before any token
/// is configured. Anything short of a clean answer — no network, no `curl`, a
/// repo with no releases (404), unparseable JSON — is `None`, never an error;
/// the update nudge is a nicety and must never be able to break startup.
pub fn latest_release(owner: &str, name: &str) -> Option<(String, String)> {
    let out = Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "10",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: orchd",
            &format!("https://api.github.com/repos/{owner}/{name}/releases/latest"),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let tag = v.get("tag_name")?.as_str()?.to_string();
    let url = v
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    Some((tag, url))
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Checks {
    Passing,
    Failing,
    Pending,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pr {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub head_ref: String,
    pub head_owner: Option<String>,
    pub base_ref: String,
    pub is_draft: bool,
    /// `MERGEABLE` / `CONFLICTING` / `UNKNOWN`.
    pub mergeable: String,
    pub merge_state: String,
    pub checks: Checks,
    /// Head commit. `/green` amends and rebases, so this moves on every
    /// internal attempt — it is an identity for "has the branch changed since
    /// the skill gave up", not a provenance record (§8).
    pub head_sha: Option<String>,
    pub unresolved: u32,
    /// True when `reviewThreads` had another page, so `unresolved` is a floor.
    /// Rendered as `50+` rather than `50`, so an under-count cannot silently
    /// hide work (§6).
    pub unresolved_capped: bool,
    pub changes_requested: bool,
    /// PRs stacked directly on this one.
    pub children: Vec<u64>,
}

impl Pr {
    /// Sort key for the rail PR group (§9):
    /// needs-resolving → failing → open and clean → draft.
    pub fn rank(&self) -> u8 {
        if self.is_draft {
            return 4;
        }
        if self.unresolved > 0 || self.changes_requested {
            return 0;
        }
        if self.checks == Checks::Failing || self.mergeable == "CONFLICTING" {
            return 1;
        }
        if self.checks == Checks::Pending {
            return 2;
        }
        3
    }
}

const PAGE: u32 = 50;

fn query_for(owner: &str, name: &str) -> String {
    // Search filters to your own open PRs server-side, which is one query
    // rather than paging the whole open set and discarding most of it.
    format!(
        r#"{{
  viewer {{ login }}
  search(query: "repo:{owner}/{name} is:pr is:open author:@me", type: ISSUE, first: {PAGE}) {{
    nodes {{
      ... on PullRequest {{
        number
        title
        url
        headRefName
        baseRefName
        isDraft
        mergeable
        mergeStateStatus
        headRepositoryOwner {{ login }}
        commits(last: 1) {{ nodes {{ commit {{ oid statusCheckRollup {{ state }} }} }} }}
        reviewThreads(first: {PAGE}) {{
          pageInfo {{ hasNextPage }}
          nodes {{ isResolved isOutdated }}
        }}
        reviews(states: CHANGES_REQUESTED, first: 20) {{ nodes {{ author {{ login }} }} }}
      }}
    }}
  }}
}}"#
    )
}

pub fn poll(token: &str, owner: &str, name: &str) -> Result<Vec<Pr>> {
    let v = graphql(token, &query_for(owner, name))?;
    let nodes = v
        .pointer("/data/search/nodes")
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();

    let mut prs: Vec<Pr> = nodes.iter().filter_map(parse_pr).collect();
    link_stacks(&mut prs);
    Ok(prs)
}

fn parse_pr(n: &Value) -> Option<Pr> {
    let number = n.get("number")?.as_u64()?;

    // The rollup hangs off the head commit, not off the PR (§6).
    let rollup = n
        .pointer("/commits/nodes/0/commit/statusCheckRollup/state")
        .and_then(|s| s.as_str());
    let checks = match rollup {
        Some("SUCCESS") => Checks::Passing,
        Some("FAILURE") | Some("ERROR") => Checks::Failing,
        Some("PENDING") | Some("EXPECTED") => Checks::Pending,
        _ => Checks::Unknown,
    };

    let threads = n.pointer("/reviewThreads/nodes").and_then(|t| t.as_array());
    // A thread that is outdated is about code that no longer exists, so it does
    // not gate `/resolve`.
    let unresolved = threads
        .map(|ts| {
            ts.iter()
                .filter(|t| {
                    t.get("isResolved").and_then(|b| b.as_bool()) == Some(false)
                        && t.get("isOutdated").and_then(|b| b.as_bool()) != Some(true)
                })
                .count() as u32
        })
        .unwrap_or(0);
    let capped = n
        .pointer("/reviewThreads/pageInfo/hasNextPage")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);

    let changes_requested = n
        .pointer("/reviews/nodes")
        .and_then(|r| r.as_array())
        .map(|r| !r.is_empty())
        .unwrap_or(false);

    Some(Pr {
        number,
        title: n.get("title")?.as_str()?.to_string(),
        url: n
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or_default()
            .to_string(),
        head_ref: n.get("headRefName")?.as_str()?.to_string(),
        head_owner: n
            .pointer("/headRepositoryOwner/login")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        base_ref: n
            .get("baseRefName")
            .and_then(|s| s.as_str())
            .unwrap_or("develop")
            .to_string(),
        is_draft: n.get("isDraft").and_then(|b| b.as_bool()).unwrap_or(false),
        mergeable: n
            .get("mergeable")
            .and_then(|s| s.as_str())
            .unwrap_or("UNKNOWN")
            .to_string(),
        merge_state: n
            .get("mergeStateStatus")
            .and_then(|s| s.as_str())
            .unwrap_or("UNKNOWN")
            .to_string(),
        checks,
        head_sha: n
            .pointer("/commits/nodes/0/commit/oid")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        unresolved,
        unresolved_capped: capped,
        changes_requested,
        children: Vec::new(),
    })
}

/// PR B is stacked on A when `B.baseRefName == A.headRefName` (§6).
fn link_stacks(prs: &mut [Pr]) {
    let by_head: HashMap<String, u64> = prs
        .iter()
        .map(|p| (p.head_ref.clone(), p.number))
        .collect();
    let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
    for p in prs.iter() {
        if let Some(parent) = by_head.get(&p.base_ref) {
            children.entry(*parent).or_default().push(p.number);
        }
    }
    for p in prs.iter_mut() {
        if let Some(c) = children.remove(&p.number) {
            p.children = c;
        }
    }
}

/// Read `owner/name` out of a git remote URL, ssh or https.
pub fn repo_from_remote(url: &str) -> Option<(String, String)> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some((owner.to_string(), name.to_string()))
}

pub fn remote_url(cwd: &Path, remote: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["remote", "get-url", remote])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_owner_and_name_from_either_remote_form() {
        assert_eq!(
            repo_from_remote("git@github.com:acme/monorepo.git"),
            Some(("acme-org".into(), "acme".into()))
        );
        assert_eq!(
            repo_from_remote("https://github.com/acme/monorepo"),
            Some(("acme-org".into(), "acme".into()))
        );
        assert_eq!(repo_from_remote("git@gitlab.com:x/y.git"), None);
    }

    fn node(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn outdated_threads_do_not_count_as_unresolved() {
        // An outdated thread is about code that no longer exists, so it must
        // not gate /resolve.
        let n = node(
            r#"{"number":1,"title":"t","headRefName":"a","baseRefName":"develop",
                "reviewThreads":{"pageInfo":{"hasNextPage":false},"nodes":[
                  {"isResolved":false,"isOutdated":true},
                  {"isResolved":false,"isOutdated":false},
                  {"isResolved":true,"isOutdated":false}]}}"#,
        );
        let pr = parse_pr(&n).unwrap();
        assert_eq!(pr.unresolved, 1);
        assert!(!pr.unresolved_capped);
    }

    #[test]
    fn a_capped_thread_page_is_flagged() {
        let n = node(
            r#"{"number":1,"title":"t","headRefName":"a","baseRefName":"develop",
                "reviewThreads":{"pageInfo":{"hasNextPage":true},"nodes":[]}}"#,
        );
        assert!(parse_pr(&n).unwrap().unresolved_capped);
    }

    #[test]
    fn the_rollup_is_read_off_the_head_commit() {
        let n = node(
            r#"{"number":1,"title":"t","headRefName":"a","baseRefName":"develop",
                "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"FAILURE"}}}]}}"#,
        );
        assert_eq!(parse_pr(&n).unwrap().checks, Checks::Failing);
    }

    #[test]
    fn a_missing_rollup_is_unknown_not_passing() {
        let n = node(r#"{"number":1,"title":"t","headRefName":"a","baseRefName":"develop"}"#);
        assert_eq!(parse_pr(&n).unwrap().checks, Checks::Unknown);
    }

    fn pr(number: u64, head: &str, base: &str) -> Pr {
        Pr {
            number,
            title: String::new(),
            url: String::new(),
            head_ref: head.into(),
            head_owner: None,
            base_ref: base.into(),
            is_draft: false,
            mergeable: "MERGEABLE".into(),
            merge_state: "CLEAN".into(),
            checks: Checks::Passing,
            head_sha: None,
            unresolved: 0,
            unresolved_capped: false,
            changes_requested: false,
            children: vec![],
        }
    }

    #[test]
    fn detects_a_stack_by_base_ref() {
        let mut prs = vec![
            pr(1, "feat/a", "develop"),
            pr(2, "feat/b", "feat/a"),
            pr(3, "feat/c", "feat/b"),
        ];
        link_stacks(&mut prs);
        assert_eq!(prs[0].children, vec![2]);
        assert_eq!(prs[1].children, vec![3]);
        assert!(prs[2].children.is_empty());
    }

    #[test]
    fn a_pr_matches_the_workspace_holding_its_head_ref() {
        // The mapping is by branch set, not by workspace name: a worktree keeps
        // a PR after you have moved the main checkout to another branch (§2).
        let branches: std::collections::HashSet<String> =
            ["develop".to_string(), "feature/x".to_string()].into_iter().collect();
        let p = pr(1, "feature/x", "develop");
        assert!(branches.contains(&p.head_ref));
        let other = pr(2, "feature/never-checked-out", "develop");
        assert!(!branches.contains(&other.head_ref));
    }

    #[test]
    fn ranks_needing_you_above_failing_and_drafts_last() {
        let mut needs = pr(1, "a", "develop");
        needs.unresolved = 2;
        let mut failing = pr(2, "b", "develop");
        failing.checks = Checks::Failing;
        let clean = pr(3, "c", "develop");
        let mut draft = pr(4, "d", "develop");
        draft.is_draft = true;
        draft.checks = Checks::Failing;

        assert!(needs.rank() < failing.rank());
        assert!(failing.rank() < clean.rank());
        // A draft stays at the bottom even when it is red.
        assert!(draft.rank() > clean.rank());
    }
}
