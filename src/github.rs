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
            let raw =
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
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
    /// the run gave up", not a provenance record (§8).
    pub head_sha: Option<String>,
    /// GitHub's sense of resolved — a conversation the comment author closed.
    /// Not to be confused with `/api/pr/:n/resolve`, which is *our* flow for
    /// answering threads and deliberately never closes one.
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

/// The open PRs you authored, plus your own login.
///
/// The query has always asked for `viewer { login }` and thrown it away. It is
/// wanted: the vendored prompts take it as `{{LOGIN}}` to spot threads you
/// already answered and to refuse a branch that is not yours, and getting it
/// from the poll costs nothing over a second `gh api user` call.
pub fn poll(token: &str, owner: &str, name: &str) -> Result<(String, Vec<Pr>)> {
    let v = graphql(token, &query_for(owner, name))?;
    let viewer = v
        .pointer("/data/viewer/login")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    let nodes = v
        .pointer("/data/search/nodes")
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();

    let mut prs: Vec<Pr> = nodes.iter().filter_map(parse_pr).collect();
    link_stacks(&mut prs);
    Ok((viewer, prs))
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

// ---------------------------------------------------------------------------
// Review threads
// ---------------------------------------------------------------------------

/// One comment in a review thread.
#[derive(Debug, Clone, Serialize)]
pub struct Comment {
    /// REST id. The reply endpoint is keyed on this, not on the GraphQL node id.
    pub database_id: u64,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub url: String,
    /// The anchored patch text. GitHub hangs it off every comment; only the
    /// first one's is worth rendering, so `Thread::diff_hunk` reads that.
    pub diff_hunk: Option<String>,
}

/// An unresolved conversation on a PR.
#[derive(Debug, Clone, Serialize)]
pub struct Thread {
    /// `PRRT_…`. **Not** a resolve target — closing a thread is the comment
    /// author's button, never ours — but the join key between a thread and the
    /// finding the triage agent returns for it.
    pub id: String,
    pub path: Option<String>,
    /// `null` on an outdated thread; the finding can still stand.
    pub line: Option<u32>,
    pub start_line: Option<u32>,
    pub original_line: Option<u32>,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub comments: Vec<Comment>,
    /// [`Thread::is_answerable`] against the fetch's own viewer, resolved by
    /// [`threads`] so the SPA does not have to reimplement the rule. Always
    /// false straight out of [`parse_thread`], which has no viewer to judge by.
    pub answerable: bool,
}

impl Thread {
    /// The patch the thread is anchored to, off the opening comment.
    pub fn diff_hunk(&self) -> Option<&str> {
        self.comments.first()?.diff_hunk.as_deref()
    }

    /// Who opened it.
    pub fn author(&self) -> Option<&str> {
        self.comments.first().map(|c| c.author.as_str())
    }

    /// Whether this thread still wants something from you.
    ///
    /// Resolved threads are done. **Outdated ones are not skipped** — the code
    /// moved, but the point may still stand. A thread whose last comment is
    /// already yours has been answered; re-answering it is noise.
    pub fn is_answerable(&self, viewer: &str) -> bool {
        if self.is_resolved {
            return false;
        }
        match self.comments.last() {
            Some(last) => last.author != viewer,
            // A thread with no comments cannot be answered.
            None => false,
        }
    }
}

/// Everything one on-demand thread fetch yields.
#[derive(Debug, Clone, Serialize)]
pub struct Threads {
    /// Your own login, for `Thread::is_answerable` and the triage prompt.
    pub viewer: String,
    /// Head at fetch time. A force-push between triage and posting invalidates
    /// every finding derived from the earlier diff, so this is recorded and
    /// re-checked rather than trusted.
    pub head_sha: Option<String>,
    pub items: Vec<Thread>,
}

/// Threads come back 100 at a time. The poll query stays at [`PAGE`]; this is
/// the on-demand fetch for one PR, and it pages to the end rather than
/// reporting `50+` (§6).
const THREAD_PAGE: u32 = 100;

/// A runaway guard, not a real ceiling: 100 pages is 10,000 threads.
const MAX_THREAD_PAGES: usize = 100;

fn threads_query(owner: &str, name: &str, pr: u64, after: Option<&str>) -> String {
    // The cursor is GitHub's own opaque string; encoding it as JSON rather than
    // pasting it in quotes keeps a stray quote from breaking the document.
    let after = match after {
        Some(c) => serde_json::Value::String(c.to_string()).to_string(),
        None => "null".to_string(),
    };
    format!(
        r#"{{
  viewer {{ login }}
  repository(owner: "{owner}", name: "{name}") {{
    pullRequest(number: {pr}) {{
      headRefOid
      reviewThreads(first: {THREAD_PAGE}, after: {after}) {{
        pageInfo {{ hasNextPage endCursor }}
        nodes {{
          id
          isResolved
          isOutdated
          path
          line
          startLine
          originalLine
          comments(first: 50) {{
            nodes {{ databaseId author {{ login }} body createdAt url diffHunk }}
          }}
        }}
      }}
    }}
  }}
}}"#
    )
}

/// Every review thread on a PR, paged to the end.
///
/// Separate from [`poll`] on purpose: the 5-minute poll only needs a count, and
/// pulling every comment body for every open PR to get one would be a large
/// query on a short timer. This runs when the review overlay opens, for one PR.
pub fn threads(token: &str, owner: &str, name: &str, pr: u64) -> Result<Threads> {
    let mut out = Threads {
        viewer: String::new(),
        head_sha: None,
        items: Vec::new(),
    };
    let mut cursor: Option<String> = None;

    for _ in 0..MAX_THREAD_PAGES {
        let v = graphql(token, &threads_query(owner, name, pr, cursor.as_deref()))?;
        let (page, next) = parse_thread_page(&v)
            .with_context(|| format!("no pull request {pr} in the response"))?;

        if out.viewer.is_empty() {
            out.viewer = page.viewer;
        }
        if out.head_sha.is_none() {
            out.head_sha = page.head_sha;
        }
        out.items.extend(page.items);

        match next {
            Some(c) => cursor = Some(c),
            None => {
                out.mark_answerable();
                out.sort_for_review();
                return Ok(out);
            }
        }
    }
    // Fell off the guard. Returning what we have beats erroring: a partial list
    // is still worth triaging, and the caller cannot fix a 10,000-thread PR.
    tracing::warn!("pr {pr}: stopped paging review threads at {MAX_THREAD_PAGES} pages");
    out.mark_answerable();
    out.sort_for_review();
    Ok(out)
}

impl Threads {
    /// Resolve each thread's [`Thread::answerable`] against this fetch's viewer.
    fn mark_answerable(&mut self) {
        // Destructured so the viewer read and the thread writes are disjoint
        // borrows of self rather than an overlapping one.
        let Threads { viewer, items, .. } = self;
        for t in items {
            t.answerable = t.is_answerable(viewer);
        }
    }

    /// Put the threads in GitHub's Files-changed order.
    ///
    /// The API returns them **chronologically** — the Conversation tab's order,
    /// jumping between files — but people review in the Files tab, whose order is
    /// the diff's, and a diff's file order is a plain path sort. So sorting by
    /// `(path, line)` reproduces the view they already read in, and puts two
    /// comments a few lines apart in one file back to back, where the second
    /// usually changes what the first deserves as an answer.
    ///
    /// Two traps: an outdated thread has no `line` and must sort on the line it
    /// was originally left at; and the comparison is plain byte order on the full
    /// path, matching git (`-` is 0x2D and `/` is 0x2F, so `foo-baz.ts` sorts
    /// before `foo/bar.ts` — which looks wrong and is what git does). A thread
    /// with no path at all is a review summary: it is about the PR rather than a
    /// file, so it sorts last.
    fn sort_for_review(&mut self) {
        self.items.sort_by(|a, b| {
            let key = |t: &Thread| {
                (
                    t.path.is_none(),
                    t.path.clone().unwrap_or_default(),
                    t.line.or(t.original_line).unwrap_or(0),
                )
            };
            key(a).cmp(&key(b))
        });
    }

    /// How many threads still want something from you.
    pub fn answerable_count(&self) -> usize {
        self.items.iter().filter(|t| t.answerable).count()
    }
}

/// One page of the thread query, plus the cursor for the next one.
///
/// Split out from [`threads`] so the paging and parsing can be tested without a
/// network round trip.
fn parse_thread_page(v: &Value) -> Option<(Threads, Option<String>)> {
    let pr = v.pointer("/data/repository/pullRequest")?;
    let root = pr.pointer("/reviewThreads")?;

    let items = root
        .pointer("/nodes")
        .and_then(|n| n.as_array())
        .map(|ns| ns.iter().filter_map(parse_thread).collect())
        .unwrap_or_default();

    // A `hasNextPage` with no cursor would loop forever on the same page, so
    // the cursor is what actually decides whether to continue.
    let next = if root
        .pointer("/pageInfo/hasNextPage")
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
    {
        root.pointer("/pageInfo/endCursor")
            .and_then(|c| c.as_str())
            .map(|c| c.to_string())
    } else {
        None
    };

    Some((
        Threads {
            viewer: v
                .pointer("/data/viewer/login")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
            head_sha: pr
                .get("headRefOid")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
            items,
        },
        next,
    ))
}

fn parse_thread(n: &Value) -> Option<Thread> {
    let u32_at = |key: &str| n.get(key).and_then(|v| v.as_u64()).map(|v| v as u32);
    Some(Thread {
        id: n.get("id")?.as_str()?.to_string(),
        path: n
            .get("path")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        line: u32_at("line"),
        start_line: u32_at("startLine"),
        original_line: u32_at("originalLine"),
        is_resolved: n
            .get("isResolved")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        is_outdated: n
            .get("isOutdated")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        comments: n
            .pointer("/comments/nodes")
            .and_then(|c| c.as_array())
            .map(|cs| cs.iter().filter_map(parse_comment).collect())
            .unwrap_or_default(),
        // Filled in by `Threads::mark_answerable`, which knows the viewer.
        answerable: false,
    })
}

fn parse_comment(n: &Value) -> Option<Comment> {
    let str_at = |key: &str| {
        n.get(key)
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Some(Comment {
        database_id: n.get("databaseId")?.as_u64()?,
        author: n
            .pointer("/author/login")
            // A deleted account leaves the comment with a null author.
            .and_then(|s| s.as_str())
            .unwrap_or("ghost")
            .to_string(),
        body: str_at("body"),
        created_at: str_at("createdAt"),
        url: str_at("url"),
        diff_hunk: n
            .get("diffHunk")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
    })
}

/// PR B is stacked on A when `B.baseRefName == A.headRefName` (§6).
fn link_stacks(prs: &mut [Pr]) {
    let by_head: HashMap<String, u64> =
        prs.iter().map(|p| (p.head_ref.clone(), p.number)).collect();
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

    // -- review threads ----------------------------------------------------

    /// One page of the thread query. `next` becomes `endCursor`/`hasNextPage`.
    fn thread_page(viewer: &str, nodes: &str, next: Option<&str>) -> Value {
        let page_info = match next {
            Some(c) => format!(r#"{{"hasNextPage":true,"endCursor":"{c}"}}"#),
            None => r#"{"hasNextPage":false,"endCursor":null}"#.to_string(),
        };
        node(&format!(
            r#"{{"data":{{"viewer":{{"login":"{viewer}"}},
                "repository":{{"pullRequest":{{"headRefOid":"abc123",
                  "reviewThreads":{{"pageInfo":{page_info},"nodes":[{nodes}]}}}}}}}}}}"#
        ))
    }

    fn thread_node(id: &str, resolved: bool, outdated: bool, authors: &[&str]) -> String {
        let comments: Vec<String> = authors
            .iter()
            .enumerate()
            .map(|(i, a)| {
                format!(
                    r#"{{"databaseId":{},"author":{{"login":"{a}"}},"body":"b{i}",
                        "createdAt":"2026-08-01T00:00:00Z","url":"u","diffHunk":"@@ -1 +1 @@\n+x"}}"#,
                    100 + i
                )
            })
            .collect();
        format!(
            r#"{{"id":"{id}","isResolved":{resolved},"isOutdated":{outdated},
                "path":"src/Foo.php","line":42,"startLine":null,"originalLine":40,
                "comments":{{"nodes":[{}]}}}}"#,
            comments.join(",")
        )
    }

    #[test]
    fn a_thread_page_yields_its_threads_and_the_next_cursor() {
        let v = thread_page(
            "kars",
            &thread_node("PRRT_1", false, false, &["john"]),
            Some("Y3Vy"),
        );
        let (page, next) = parse_thread_page(&v).unwrap();

        assert_eq!(page.viewer, "kars");
        assert_eq!(page.head_sha.as_deref(), Some("abc123"));
        assert_eq!(next.as_deref(), Some("Y3Vy"));

        let t = &page.items[0];
        assert_eq!(t.id, "PRRT_1");
        assert_eq!(t.path.as_deref(), Some("src/Foo.php"));
        assert_eq!(t.line, Some(42));
        assert_eq!(t.start_line, None);
        assert_eq!(t.original_line, Some(40));
        assert_eq!(t.author(), Some("john"));
        assert_eq!(t.diff_hunk(), Some("@@ -1 +1 @@\n+x"));
        assert_eq!(t.comments[0].database_id, 100);
    }

    #[test]
    fn the_last_page_reports_no_cursor() {
        let v = thread_page("kars", &thread_node("PRRT_1", false, false, &["john"]), None);
        assert_eq!(parse_thread_page(&v).unwrap().1, None);
    }

    #[test]
    fn a_next_page_without_a_cursor_stops_rather_than_looping() {
        // hasNextPage with a null endCursor would re-request the same page
        // forever; the cursor, not the flag, is what continues the loop.
        let v = node(
            r#"{"data":{"viewer":{"login":"kars"},"repository":{"pullRequest":{
                "headRefOid":"abc123","reviewThreads":{
                  "pageInfo":{"hasNextPage":true,"endCursor":null},"nodes":[]}}}}}"#,
        );
        assert_eq!(parse_thread_page(&v).unwrap().1, None);
    }

    #[test]
    fn a_missing_pull_request_is_an_error_not_an_empty_list() {
        // A deleted or mistyped PR must not read as "no threads to answer".
        let v = node(r#"{"data":{"viewer":{"login":"kars"},"repository":{"pullRequest":null}}}"#);
        assert!(parse_thread_page(&v).is_none());
    }

    #[test]
    fn a_resolved_thread_needs_no_answer() {
        let v = thread_page("kars", &thread_node("PRRT_1", true, false, &["john"]), None);
        let (page, _) = parse_thread_page(&v).unwrap();
        assert!(!page.items[0].is_answerable("kars"));
    }

    #[test]
    fn an_outdated_thread_still_needs_an_answer() {
        // The code moved, but the point may still stand — unlike the rail's
        // unresolved count, triage keeps these.
        let v = thread_page("kars", &thread_node("PRRT_1", false, true, &["john"]), None);
        let (page, _) = parse_thread_page(&v).unwrap();
        assert!(page.items[0].is_answerable("kars"));
    }

    #[test]
    fn a_thread_you_answered_last_needs_no_second_answer() {
        let v = thread_page(
            "kars",
            &thread_node("PRRT_1", false, false, &["john", "kars"]),
            None,
        );
        let (page, _) = parse_thread_page(&v).unwrap();
        assert!(!page.items[0].is_answerable("kars"));

        // ...but one where they got the last word still does.
        let v = thread_page(
            "kars",
            &thread_node("PRRT_2", false, false, &["john", "kars", "john"]),
            None,
        );
        let (page, _) = parse_thread_page(&v).unwrap();
        assert!(page.items[0].is_answerable("kars"));
    }

    #[test]
    fn a_deleted_author_reads_as_ghost_rather_than_dropping_the_comment() {
        let v = node(
            r#"{"data":{"viewer":{"login":"kars"},"repository":{"pullRequest":{
                "headRefOid":"abc","reviewThreads":{"pageInfo":{"hasNextPage":false},
                "nodes":[{"id":"PRRT_1","isResolved":false,"isOutdated":false,
                  "comments":{"nodes":[
                    {"databaseId":1,"author":null,"body":"b","createdAt":"t","url":"u"}]}}]}}}}}"#,
        );
        let (page, _) = parse_thread_page(&v).unwrap();
        assert_eq!(page.items[0].comments[0].author, "ghost");
        assert_eq!(page.items[0].diff_hunk(), None);
    }

    /// Smoke-test the live query. Ignored by default: it needs the network and
    /// a `gh` login, and it asserts against a PR whose threads can change.
    ///
    /// `cargo test --lib -- --ignored --nocapture fetches_real_threads`
    #[test]
    #[ignore = "hits the GitHub API"]
    fn fetches_real_threads() {
        let token = resolve_token(None).expect("a token");
        let got = threads(&token.value, "acme-org", "acme", 10001).expect("the fetch");

        assert!(!got.viewer.is_empty(), "viewer login came back empty");
        assert!(got.head_sha.is_some(), "no head sha");
        for t in &got.items {
            assert!(t.id.starts_with("PRRT_"), "odd thread id: {}", t.id);
            assert!(!t.comments.is_empty(), "{} has no comments", t.id);
            // An outdated thread reports a null `line` but keeps `originalLine`,
            // which is why the card falls back to it.
            if t.is_outdated {
                assert!(t.line.is_none() || t.original_line.is_some());
            }
        }
        eprintln!(
            "viewer={} head={:?} threads={}",
            got.viewer,
            got.head_sha,
            got.items.len()
        );
    }

    #[test]
    fn threads_sort_into_files_changed_order() {
        // The API hands these back chronologically; people review in the Files
        // tab, whose order is the diff's.
        let mut t = Threads {
            viewer: "kars".into(),
            head_sha: None,
            items: vec![
                thread_at(Some("b.ts"), Some(9)),
                thread_at(None, None), // review summary: no file
                thread_at(Some("a.ts"), Some(20)),
                thread_at(Some("a.ts"), Some(3)),
                thread_at(Some("a-z.ts"), Some(1)),
            ],
        };
        t.sort_for_review();
        let order: Vec<(Option<&str>, Option<u32>)> = t
            .items
            .iter()
            .map(|x| (x.path.as_deref(), x.line))
            .collect();
        assert_eq!(
            order,
            vec![
                // Plain byte order on the path, matching git: '-' (0x2D) sorts
                // before '/' and before 'a'..'z' continues.
                (Some("a-z.ts"), Some(1)),
                (Some("a.ts"), Some(3)),
                (Some("a.ts"), Some(20)),
                (Some("b.ts"), Some(9)),
                // A summary is about the PR, not a file, so it goes last.
                (None, None),
            ]
        );
    }

    #[test]
    fn an_outdated_thread_sorts_on_the_line_it_was_left_at() {
        // `line` is null once the code moved; without the fallback it would sort
        // to the top of its file instead of where the reviewer was looking.
        let mut t = Threads {
            viewer: "kars".into(),
            head_sha: None,
            items: vec![thread_at(Some("a.ts"), Some(5)), {
                let mut o = thread_at(Some("a.ts"), None);
                o.original_line = Some(2);
                o.is_outdated = true;
                o
            }],
        };
        t.sort_for_review();
        assert_eq!(t.items[0].original_line, Some(2), "outdated :2 sorts first");
        assert_eq!(t.items[1].line, Some(5));
    }

    fn thread_at(path: Option<&str>, line: Option<u32>) -> Thread {
        Thread {
            id: format!("PRRT_{}_{:?}", path.unwrap_or("none"), line),
            path: path.map(|p| p.to_string()),
            line,
            start_line: None,
            original_line: None,
            is_resolved: false,
            is_outdated: false,
            comments: Vec::new(),
            answerable: true,
        }
    }

    #[test]
    fn the_cursor_is_json_encoded_into_the_query() {
        let q = threads_query("o", "n", 7, Some(r#"cur"sor"#));
        assert!(q.contains(r#"after: "cur\"sor""#), "{q}");
        assert!(threads_query("o", "n", 7, None).contains("after: null"));
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
            ["develop".to_string(), "feature/x".to_string()]
                .into_iter()
                .collect();
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
