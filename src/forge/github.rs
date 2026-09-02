use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use super::github_write::Target;
use super::model::{Checks, Comment, Pr, Thread, ThreadRoot, Threads};
use super::Forge;

/// Where the daemon's GitHub token came from.
///
/// §6 wants a fine-grained PAT with **read scopes only** — `pull_requests`,
/// `checks`, `contents`, `metadata`. The daemon never pushes; `fix-pr` pushes
/// through the agent's own git credentials, so any write scope here is
/// unnecessary blast radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(export, export_to = "../web/snapshot.d.ts"))]
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
    /* **Written for whoever reads it in the PR pane**, which is where this lands.
       It used to say "no ORCHD_GITHUB_TOKEN, no token file, and `gh auth token`
       could not be run: No such file or directory (os error 2)" — a walk through
       the daemon's own ladder, ending in an errno. That is the first thing a new
       install sees, and it names three things the reader has never heard of
       instead of the one command that fixes it. The ladder is still worth knowing,
       so it comes second, in the half a pane can show when it has room. */
    let out = match Command::new("gh").args(["auth", "token"]).output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "no GitHub credential: install `gh` and run `gh auth login`, or point \
             github_token_file at a token"
        ),
        Err(e) => bail!("no GitHub credential: `gh auth token` could not be run: {e}"),
    };
    if !out.status.success() {
        // gh is there and says why, usually "not logged in". Its own words, since
        // they are better than a guess about which of its states this is.
        bail!(
            "no GitHub credential, run `gh auth login`: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() {
        bail!("no GitHub credential: `gh auth token` returned nothing");
    }
    Ok(Token {
        value: v,
        source: TokenSource::GhCli,
    })
}

/// Shared with `story::resolve_token`: both ladders read a token out of a file
/// that is meant to be `0600`.
#[cfg(unix)]
pub fn warn_if_world_readable(p: &Path) {
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
            // A hung request must not pin a poll thread forever. The review query
            // is one server-side search and the PR poll a handful of requests, so
            // a generous ceiling never bites a healthy call but bounds a stuck one
            // — this is where the old external `review_timeout_seconds` went.
            "--max-time",
            "120",
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
/// A `token`, when present, is sent as a bearer credential: the release repo is
/// private, so an unauthenticated call 404s and the nudge silently never fires.
/// The token is optional because a public release repo needs none, and the
/// caller passes what its ladder happens to resolve.
///
/// Anything short of a clean answer — no network, no `curl`, a repo with no
/// releases (404), unparseable JSON — is `None`, never an error; the update
/// nudge is a nicety and must never be able to break startup.
pub fn latest_release(owner: &str, name: &str, token: Option<&str>) -> Option<(String, String)> {
    use std::io::Write;
    use std::process::Stdio;

    let url = format!("https://api.github.com/repos/{owner}/{name}/releases/latest");
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "--max-time",
        "10",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: orchd",
    ]);
    // The token rides on stdin, never argv: the same ladder can resolve a
    // `gh auth token` with wider scopes than this read expects, so it must not
    // show in the process table for the life of the curl — the stdin dance
    // graphql() does, for the same reason.
    let token = token.map(str::trim).filter(|t| !t.is_empty());
    if token.is_some() {
        cmd.arg("-H").arg("@-").stdin(Stdio::piped());
    }
    let mut child = cmd
        .arg(&url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    if let Some(t) = token {
        child
            .stdin
            .as_mut()?
            .write_all(format!("Authorization: Bearer {t}\n").as_bytes())
            .ok()?;
    }
    let out = child.wait_with_output().ok()?;
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
// The GitHub forge
// ---------------------------------------------------------------------------

/// One repo on github.com. Reads go through the curl+PAT transport
/// ([`graphql`]); writes shell `gh`, which carries its own credential — so
/// `token` is the read half only, and a repo can have one without the other.
///
/// Cheap to clone (three `String`s) and holds no borrows, so it can be moved
/// into a `spawn_blocking`: every method here blocks on a subprocess and must
/// not run on the async runtime. Constructed per operation, not held, so a
/// rotated token file is picked up on the next poll.
#[derive(Debug, Clone)]
pub struct GitHubForge {
    owner: String,
    name: String,
    /// Read-path PAT. Empty is allowed to construct — the first read then fails
    /// with GitHub's own auth error, which is the same path a bad token takes.
    token: String,
}

impl GitHubForge {
    pub fn new(owner: impl Into<String>, name: impl Into<String>, token: impl Into<String>) -> Self {
        GitHubForge {
            owner: owner.into(),
            name: name.into(),
            token: token.into(),
        }
    }

    /// `owner/name` from a git remote. Off the trait because it must run before
    /// an instance exists — it is how the caller learns what repo to build one
    /// for. github.com-specific by nature (URL shapes), which is why it belongs
    /// to the GitHub forge rather than to a generic seam.
    pub fn detect(cwd: &Path, remote: &str) -> Option<(String, String)> {
        repo_from_remote(&remote_url(cwd, remote)?)
    }
}

impl Forge for GitHubForge {
    fn poll_prs(&self) -> Result<(String, Vec<Pr>)> {
        poll(&self.token, &self.owner, &self.name)
    }

    fn threads(&self, pr: u64) -> Result<Threads> {
        fetch_threads(&self.token, &self.owner, &self.name, pr)
    }

    fn reply(&self, at: &Path, root: &ThreadRoot, body: &str) -> Result<()> {
        self.target(at).reply(root, body).map(|_| ())
    }

    fn thumbs_up(&self, at: &Path, root: &ThreadRoot) -> Result<()> {
        self.target(at).thumbs_up(root).map(|_| ())
    }

    fn rerequest(&self, at: &Path, pr: u64, login: &str) -> Result<()> {
        self.target(at).rerequest(pr, login)
    }

    /// github.com's blob grammar. The host is a constant rather than carried on
    /// the forge because `repo_from_remote` only recognises github.com remotes
    /// in the first place — an Enterprise host never gets this far, so storing
    /// one here would be dead state pretending to be support.
    ///
    /// Path segments are escaped individually: a `/` in a path is structure and
    /// must stay a separator, while a `#` or `?` in a filename would otherwise
    /// truncate the URL at the fragment or query.
    fn blob_url(&self, r#ref: &str, path: &str) -> String {
        // Byte-wise, not char-wise: a percent escape encodes UTF-8 bytes, so an
        // accented filename needs one `%XX` per byte and `c as u32` would emit
        // one per codepoint and produce a URL nothing resolves.
        let esc = |s: &str| {
            s.split('/')
                .map(|seg| {
                    seg.bytes()
                        .map(|b| match b {
                            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.'
                            | b'~' => (b as char).to_string(),
                            _ => format!("%{b:02X}"),
                        })
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("/")
        };
        format!(
            "https://github.com/{}/{}/blob/{}/{}",
            self.owner,
            self.name,
            esc(r#ref),
            esc(path)
        )
    }
}

impl GitHubForge {
    /// A `gh`-shelling write handle rooted at `at`. The working dir varies per
    /// caller — the resolve run writes from the worktree, the API from main — so
    /// it is a parameter, never baked into the forge.
    fn target(&self, at: &Path) -> Target {
        Target {
            cwd: at.to_path_buf(),
            owner: self.owner.clone(),
            name: self.name.clone(),
        }
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
        headRepository {{ nameWithOwner viewerPermission }}
        commits(last: 1) {{ nodes {{ commit {{ oid committedDate statusCheckRollup {{ state }} }} }} }}
        reviewThreads(first: {PAGE}) {{
          pageInfo {{ hasNextPage endCursor }}
          nodes {{ isResolved isOutdated
            comments(last: 1) {{ nodes {{
              author {{ login }}
              reactionGroups {{ content viewerHasReacted }}
            }} }}
          }}
        }}
        reviews(states: CHANGES_REQUESTED, first: 20) {{ nodes {{ author {{ login }} submittedAt }} }}
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

    let mut prs: Vec<Pr> = Vec::with_capacity(nodes.len());
    for n in &nodes {
        let Some(mut pr) = parse_pr(n, &viewer) else { continue };
        // The poll asks for one 50-thread page per PR to keep the fan-out cheap;
        // a PR with more than that comes back a floor (`50+`). Page out the rest
        // for just those PRs — the common case never pays for it — and recount so
        // the number, and "waiting on you", are true rather than a floor.
        if pr.unresolved_capped {
            match all_summary_threads(token, owner, name, pr.number, n) {
                Ok((threads, guard_hit)) => {
                    let (unresolved, awaiting_you) = count_open(&threads, &viewer);
                    pr.unresolved = unresolved;
                    pr.awaiting_you = awaiting_you;
                    pr.unresolved_capped = guard_hit;
                    pr.needs_you =
                        needs_you(awaiting_you, pr.changes_requested, unresolved, n, guard_hit);
                }
                // A floor beats nothing: keep the first page's capped values and
                // try again next poll, same as the detailed fetch's runaway guard.
                Err(e) => tracing::warn!(pr = pr.number, "could not page review threads: {e:#}"),
            }
        }
        prs.push(pr);
    }
    link_stacks(&mut prs);
    Ok((viewer, prs))
}

/// One PR's remaining review-thread pages, with the poll's slim per-thread fields
/// rather than the detailed fetch's comment bodies. Only issued for a PR the first
/// page reported as capped, so the poll's fan-out over every PR stays one query.
fn summary_threads_query(owner: &str, name: &str, pr: u64, after: &str) -> String {
    // JSON-encode the cursor rather than pasting it in quotes, so a stray quote in
    // GitHub's opaque string cannot break the document (as in `threads_query`).
    let after = serde_json::Value::String(after.to_string()).to_string();
    format!(
        r#"{{
  repository(owner: "{owner}", name: "{name}") {{
    pullRequest(number: {pr}) {{
      reviewThreads(first: {THREAD_PAGE}, after: {after}) {{
        pageInfo {{ hasNextPage endCursor }}
        nodes {{ isResolved isOutdated
          comments(last: 1) {{ nodes {{
            author {{ login }}
            reactionGroups {{ content viewerHasReacted }}
          }} }}
        }}
      }}
    }}
  }}
}}"#
    )
}

/// The raw thread nodes of one page plus the cursor for the next, or `None` if the
/// PR is missing from the response. Split out so paging is testable without a round
/// trip, matching [`parse_thread_page`].
fn parse_summary_thread_page(v: &Value) -> Option<(Vec<Value>, Option<String>)> {
    let root = v.pointer("/data/repository/pullRequest/reviewThreads")?;
    let nodes = root
        .pointer("/nodes")
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();
    let next = next_cursor(root);
    Some((nodes, next))
}

/// Every review-thread node for one capped PR: the first page (already in `node`)
/// plus every following page. The bool is whether the [`MAX_THREAD_PAGES`] runaway
/// guard fired, in which case the count is still a floor.
fn all_summary_threads(
    token: &str,
    owner: &str,
    name: &str,
    pr: u64,
    node: &Value,
) -> Result<(Vec<Value>, bool)> {
    let mut all: Vec<Value> = node
        .pointer("/reviewThreads/nodes")
        .and_then(|n| n.as_array())
        .cloned()
        .unwrap_or_default();
    let mut cursor = node
        .pointer("/reviewThreads/pageInfo/endCursor")
        .and_then(|c| c.as_str())
        .map(|c| c.to_string());

    for _ in 0..MAX_THREAD_PAGES {
        let Some(c) = cursor else {
            return Ok((all, false));
        };
        let v = graphql(token, &summary_threads_query(owner, name, pr, &c))?;
        let (nodes, next) = parse_summary_thread_page(&v)
            .with_context(|| format!("no pull request {pr} in the response"))?;
        all.extend(nodes);
        cursor = next;
    }
    tracing::warn!("pr {pr}: stopped paging review threads at {MAX_THREAD_PAGES} pages");
    Ok((all, true))
}

/// Does this repository permission let you push?
///
/// GitHub's `RepositoryPermission`: `ADMIN`, `MAINTAIN`, `WRITE`, `TRIAGE`,
/// `READ`. Only the first three carry write access — `TRIAGE` manages issues and
/// `READ` is read — and an unrecognised value is *not* push access, because a new
/// tier arriving must not silently authorise a force-push.
fn may_push(permission: &str) -> bool {
    matches!(permission, "ADMIN" | "MAINTAIN" | "WRITE")
}

/// Whether a thread's last word is yours, one way or another.
///
/// The rule itself is [`crate::forge::model::answered`], shared with
/// `Thread::is_answerable`; this is only the part that knows where the two facts
/// live in the poll's own JSON. The summary query asks for one comment, newest
/// first, so `nodes/0` is the last word.
fn acknowledged(thread: &Value, viewer: &str) -> bool {
    let Some(last) = thread.pointer("/comments/nodes/0") else {
        // No comments at all is not a thread anybody is waiting on.
        return true;
    };
    let author = last
        .pointer("/author/login")
        .and_then(|l| l.as_str())
        .unwrap_or("ghost");
    crate::forge::model::answered(author, viewer_thumbed(last), viewer)
}

/// Did you 👍 this comment? Read out of a `reactionGroups` selection.
///
/// Its own function because two queries select it and both hand it to the same
/// rule — the poll's, straight off the JSON, and the detailed fetch's, on its way
/// into [`Comment::viewer_thumbed`].
fn viewer_thumbed(comment: &Value) -> bool {
    comment
        .get("reactionGroups")
        .and_then(|g| g.as_array())
        .map(|groups| {
            groups.iter().any(|g| {
                g.get("content").and_then(|c| c.as_str()) == Some("THUMBS_UP")
                    && g.get("viewerHasReacted").and_then(|b| b.as_bool()) == Some(true)
            })
        })
        .unwrap_or(false)
}

/// Count open threads and, of those, the ones still waiting on you.
///
/// An outdated thread is about code that no longer exists, so it does not *nag*:
/// it is dropped here and nowhere else. `Thread::is_answerable` keeps it, because
/// the point can still stand and the flow can still answer it — the two are
/// answering different questions, and this is the pair to the note there. What
/// they must never disagree on is the last word, which is why both go through
/// `model::answered`.
///
/// And closing a thread is the reviewer's button, so "unresolved" is
/// not the same question as "waiting on you" — answer every thread and the count
/// does not move. What moves is who spoke last, and whether you acknowledged them:
/// `/resolve` replies where there is something to say and leaves a 👍 where there
/// is not, so both count as handled.
///
/// Shared between the first page (`parse_pr`) and the paged recount of a capped
/// PR (`poll`), so the two can never disagree on what "unresolved" means.
fn count_open(threads: &[Value], viewer: &str) -> (u32, u32) {
    let open: Vec<&Value> = threads
        .iter()
        .filter(|t| {
            t.get("isResolved").and_then(|b| b.as_bool()) == Some(false)
                && t.get("isOutdated").and_then(|b| b.as_bool()) != Some(true)
        })
        .collect();
    let unresolved = open.len() as u32;
    let awaiting_you = open.iter().filter(|t| !acknowledged(t, viewer)).count() as u32;
    (unresolved, awaiting_you)
}

/// Whether you already answered a change request with code.
///
/// "Changes requested" is not the same question as "waiting on you". GitHub keeps
/// the review listed until the reviewer comes back and re-reads, so a PR you have
/// pushed a fix to still reports one — and the rail kept the ball amber for work
/// that was done, with the next move squarely on the reviewer.
///
/// Pushing is the answer available when the objection lives in the review body,
/// where there is no thread to reply to and nothing to 👍. It is also what
/// outdates the threads there *were*, which is how a PR whose comments you all
/// handled arrives here looking like a PR that never had any.
///
/// Timestamps are GitHub's ISO-8601 in UTC, so they order lexicographically and
/// nothing has to parse a date. Either one missing is no opinion, which leaves
/// the answer where it was.
/// Whether the PR waits on you. Decided once, for the first page and for the
/// recount a capped PR pays for, because the recount takes exactly the PRs a fix
/// applied to `parse_pr` alone would miss.
fn needs_you(
    awaiting_you: u32,
    changes_requested: bool,
    unresolved: u32,
    n: &Value,
    capped: bool,
) -> bool {
    awaiting_you > 0 || (changes_requested && unresolved == 0 && !answered_by_pushing(n)) || capped
}

fn answered_by_pushing(n: &Value) -> bool {
    let pushed = n
        .pointer("/commits/nodes/0/commit/committedDate")
        .and_then(|d| d.as_str());
    let asked = n
        .pointer("/reviews/nodes")
        .and_then(|r| r.as_array())
        .and_then(|r| {
            r.iter()
                .filter_map(|v| v.get("submittedAt").and_then(|d| d.as_str()))
                .max()
        });
    match (pushed, asked) {
        (Some(pushed), Some(asked)) => pushed > asked,
        _ => false,
    }
}

fn parse_pr(n: &Value, viewer: &str) -> Option<Pr> {
    let number = n.get("number")?.as_u64()?;

    // The rollup hangs off the head commit, not off the PR (§6).
    let checks = checks_from(
        n.pointer("/commits/nodes/0/commit/statusCheckRollup/state")
            .and_then(|s| s.as_str()),
    );

    let threads = n
        .pointer("/reviewThreads/nodes")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let (unresolved, awaiting_you) = count_open(&threads, viewer);
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
        head_repo: n
            .pointer("/headRepository/nameWithOwner")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        head_pushable: n
            .pointer("/headRepository/viewerPermission")
            .and_then(|s| s.as_str())
            .map(may_push),
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
        awaiting_you,
        changes_requested,
        needs_you: needs_you(awaiting_you, changes_requested, unresolved, n, capped),
        children: Vec::new(),
    })
}

/// GitHub's status-check rollup enum → our four states. Shared by the PR poll
/// and the review queue so the two never disagree on what "failing" means.
fn checks_from(state: Option<&str>) -> Checks {
    match state {
        Some("SUCCESS") => Checks::Passing,
        Some("FAILURE") | Some("ERROR") => Checks::Failing,
        Some("PENDING") | Some("EXPECTED") => Checks::Pending,
        _ => Checks::Unknown,
    }
}

/// The next page's cursor, or `None` when this was the last page. A
/// `hasNextPage` with a null cursor would re-request the same page forever, so
/// the cursor is what decides. Shared by both coverage walks.
fn next_cursor(root: &Value) -> Option<String> {
    if root.pointer("/pageInfo/hasNextPage").and_then(|b| b.as_bool()) != Some(true) {
        return None;
    }
    root.pointer("/pageInfo/endCursor")
        .and_then(|c| c.as_str())
        .map(String::from)
}

// ---------------------------------------------------------------------------
// Review threads
// ---------------------------------------------------------------------------

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
            nodes {{ databaseId author {{ login }} body createdAt url diffHunk
              reactionGroups {{ content viewerHasReacted }} }}
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
/// Reached through [`GitHubForge::threads`].
fn fetch_threads(token: &str, owner: &str, name: &str, pr: u64) -> Result<Threads> {
    let mut out = Threads {
        pr,
        viewer: String::new(),
        head_sha: None,
        items: Vec::new(),
    };
    let mut cursor: Option<String> = None;

    for _ in 0..MAX_THREAD_PAGES {
        let v = graphql(token, &threads_query(owner, name, pr, cursor.as_deref()))?;
        let (page, next) = parse_thread_page(&v, pr)
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

/// One page of the thread query, plus the cursor for the next one.
///
/// Split out from [`threads`] so the paging and parsing can be tested without a
/// network round trip.
fn parse_thread_page(v: &Value, pr: u64) -> Option<(Threads, Option<String>)> {
    let node = v.pointer("/data/repository/pullRequest")?;
    let root = node.pointer("/reviewThreads")?;

    let items = root
        .pointer("/nodes")
        .and_then(|n| n.as_array())
        .map(|ns| ns.iter().filter_map(parse_thread).collect())
        .unwrap_or_default();

    let next = next_cursor(root);

    Some((
        Threads {
            pr,
            viewer: v
                .pointer("/data/viewer/login")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
            head_sha: node
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
        viewer_thumbed: viewer_thumbed(n),
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
            Some(("acme".into(), "monorepo".into()))
        );
        assert_eq!(
            repo_from_remote("https://github.com/acme/monorepo"),
            Some(("acme".into(), "monorepo".into()))
        );
        assert_eq!(repo_from_remote("git@gitlab.com:x/y.git"), None);
    }

    /// The three tiers that carry write, and nothing else.
    ///
    /// An unrecognised value is deliberately *not* push access: this authorises a
    /// force-pushing run, and a permission tier GitHub adds later must fail the
    /// guard closed rather than be waved through by a catch-all.
    #[test]
    fn only_the_write_permissions_let_a_fix_run_push() {
        for yes in ["ADMIN", "MAINTAIN", "WRITE"] {
            assert!(may_push(yes), "{yes} can push");
        }
        for no in ["TRIAGE", "READ", "NONE", "", "write", "SOMETHING_NEW"] {
            assert!(!may_push(no), "{no} must not authorise a force-push");
        }
    }

    /// The head repo and its permission come off the PR, and both may be absent —
    /// a deleted fork answers `headRepository: null`, which must read as "unknown"
    /// rather than as anything the guard can act on.
    #[test]
    fn a_prs_head_repo_and_push_right_are_read_or_left_unknown() {
        let with = serde_json::json!({
            "number": 7, "title": "t", "url": "u", "headRefName": "feature/x",
            "headRepository": { "nameWithOwner": "acme/monorepo", "viewerPermission": "WRITE" },
            "baseRefName": "develop", "isDraft": false,
            "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "reviewThreads": { "pageInfo": { "hasNextPage": false }, "nodes": [] },
        });
        let pr = parse_pr(&with, "me").expect("parsed");
        assert_eq!(pr.head_repo.as_deref(), Some("acme/monorepo"));
        assert_eq!(pr.head_pushable, Some(true));

        let mut without = with.clone();
        without["headRepository"] = serde_json::Value::Null;
        let pr = parse_pr(&without, "me").expect("parsed");
        assert_eq!(pr.head_repo, None);
        assert_eq!(pr.head_pushable, None, "absent is unknown, not `false`");
    }

    #[test]
    fn blob_url_keeps_separators_and_escapes_the_rest() {
        let f = GitHubForge::new("acme", "monorepo", "t");
        assert_eq!(
            f.blob_url("abc123", "src/forge/github.rs"),
            "https://github.com/acme/monorepo/blob/abc123/src/forge/github.rs"
        );
        // A slash is structure and stays one; a space and a `#` would otherwise
        // break the URL or truncate it at the fragment.
        assert_eq!(
            f.blob_url("worktree-a", "a dir/b#c.txt"),
            "https://github.com/acme/monorepo/blob/worktree-a/a%20dir/b%23c.txt"
        );
        // Non-ASCII is one escape per UTF-8 byte, not per codepoint.
        assert_eq!(
            f.blob_url("main", "café.rs"),
            "https://github.com/acme/monorepo/blob/main/caf%C3%A9.rs"
        );
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
        let pr = parse_pr(&n, "me").unwrap();
        assert_eq!(pr.unresolved, 1);
        assert!(!pr.unresolved_capped);
    }

    #[test]
    fn a_thread_you_answered_is_not_waiting_on_you() {
        // The whole point: closing a thread is the reviewer's button, so a PR
        // whose every thread has your reply or your 👍 is their turn, not yours.
        let n = node(
            r#"{"number":1,"title":"t","headRefName":"a","baseRefName":"develop",
                "reviews":{"nodes":[{"author":{"login":"them"}}]},
                "reviewThreads":{"pageInfo":{"hasNextPage":false},"nodes":[
                  {"isResolved":false,"isOutdated":false,"comments":{"nodes":[
                    {"author":{"login":"me"},"reactionGroups":[]}]}},
                  {"isResolved":false,"isOutdated":false,"comments":{"nodes":[
                    {"author":{"login":"them"},
                     "reactionGroups":[{"content":"THUMBS_UP","viewerHasReacted":true}]}]}}]}}"#,
        );
        let pr = parse_pr(&n, "me").unwrap();
        assert_eq!(pr.unresolved, 2, "GitHub still calls both unresolved");
        assert_eq!(pr.awaiting_you, 0, "one you answered, one you thumbed");
        assert!(
            !pr.needs_you,
            "changes-requested must not outvote threads that are all handled"
        );
    }

    #[test]
    fn a_reviewer_who_spoke_last_is_waiting_on_you() {
        let n = node(
            r#"{"number":1,"title":"t","headRefName":"a","baseRefName":"develop",
                "reviewThreads":{"pageInfo":{"hasNextPage":false},"nodes":[
                  {"isResolved":false,"isOutdated":false,"comments":{"nodes":[
                    {"author":{"login":"them"},"reactionGroups":[]}]}}]}}"#,
        );
        let pr = parse_pr(&n, "me").unwrap();
        assert_eq!(pr.awaiting_you, 1);
        assert!(pr.needs_you);
    }

    #[test]
    fn changes_requested_with_nothing_to_answer_still_needs_you() {
        // The objection lives in the review body, so there is no thread to
        // answer and no 👍 to leave; it would otherwise vanish from the rail.
        let n = node(
            r#"{"number":1,"title":"t","headRefName":"a","baseRefName":"develop",
                "reviews":{"nodes":[{"author":{"login":"them"}}]},
                "reviewThreads":{"pageInfo":{"hasNextPage":false},"nodes":[]}}"#,
        );
        let pr = parse_pr(&n, "me").unwrap();
        assert!(pr.changes_requested);
        assert!(pr.needs_you);
    }

    /// PR 10002: two threads answered with a 👍 and a push, which outdated both.
    /// The change request is still listed, so the rail kept saying "you" for work
    /// that was waiting on the reviewer.
    #[test]
    fn a_change_request_you_pushed_a_fix_for_is_not_waiting_on_you() {
        let n = node(
            r#"{"number":1,"title":"t","headRefName":"a","baseRefName":"develop",
                "commits":{"nodes":[{"commit":{"oid":"abc","committedDate":"2026-08-21T09:00:00Z"}}]},
                "reviews":{"nodes":[{"author":{"login":"them"},"submittedAt":"2026-08-20T12:00:00Z"}]},
                "reviewThreads":{"pageInfo":{"hasNextPage":false},"nodes":[
                  {"isResolved":false,"isOutdated":true,"comments":{"nodes":[
                    {"author":{"login":"them"},"reactionGroups":[
                      {"content":"THUMBS_UP","viewerHasReacted":true}]}]}}]}}"#,
        );
        let pr = parse_pr(&n, "me").unwrap();
        assert!(pr.changes_requested, "the review is still on the PR");
        assert!(!pr.needs_you, "but the next move is the reviewer's");
    }

    /// The other order: they read the push and asked again.
    #[test]
    fn a_change_request_newer_than_your_push_is_still_yours() {
        let n = node(
            r#"{"number":1,"title":"t","headRefName":"a","baseRefName":"develop",
                "commits":{"nodes":[{"commit":{"oid":"abc","committedDate":"2026-08-20T12:00:00Z"}}]},
                "reviews":{"nodes":[{"author":{"login":"them"},"submittedAt":"2026-08-21T09:00:00Z"}]},
                "reviewThreads":{"pageInfo":{"hasNextPage":false},"nodes":[]}}"#,
        );
        assert!(parse_pr(&n, "me").unwrap().needs_you);
    }

    #[test]
    fn a_capped_thread_page_is_flagged() {
        let n = node(
            r#"{"number":1,"title":"t","headRefName":"a","baseRefName":"develop",
                "reviewThreads":{"pageInfo":{"hasNextPage":true},"nodes":[]}}"#,
        );
        assert!(parse_pr(&n, "me").unwrap().unresolved_capped);
    }

    // -- summary paging (poll past 50) -------------------------------------

    /// One page of the slim summary query, shaped like `summary_threads_query`.
    fn summary_page(nodes: &str, next: Option<&str>) -> Value {
        let page_info = match next {
            Some(c) => format!(r#"{{"hasNextPage":true,"endCursor":"{c}"}}"#),
            None => r#"{"hasNextPage":false,"endCursor":null}"#.to_string(),
        };
        node(&format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{
                "reviewThreads":{{"pageInfo":{page_info},"nodes":[{nodes}]}}}}}}}}}}"#
        ))
    }

    #[test]
    fn a_summary_page_yields_its_nodes_and_the_next_cursor() {
        let v = summary_page(
            r#"{"isResolved":false,"isOutdated":false,
                "comments":{"nodes":[{"author":{"login":"them"},"reactionGroups":[]}]}}"#,
            Some("Y3Vy"),
        );
        let (nodes, next) = parse_summary_thread_page(&v).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(next.as_deref(), Some("Y3Vy"));
        // The nodes carry exactly what `count_open` reads.
        let (unresolved, awaiting) = count_open(&nodes, "me");
        assert_eq!((unresolved, awaiting), (1, 1));
    }

    #[test]
    fn a_summary_next_page_without_a_cursor_stops_rather_than_looping() {
        // hasNextPage with a null endCursor would re-request the same page; the
        // cursor, not the flag, is what continues the loop.
        let v = node(
            r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{
                "pageInfo":{"hasNextPage":true,"endCursor":null},"nodes":[]}}}}}"#,
        );
        assert_eq!(parse_summary_thread_page(&v).unwrap().1, None);
    }

    #[test]
    fn a_missing_pull_request_in_a_summary_page_is_none() {
        let v = node(r#"{"data":{"repository":{"pullRequest":null}}}"#);
        assert!(parse_summary_thread_page(&v).is_none());
    }

    #[test]
    fn count_open_matches_parse_pr_over_the_same_threads() {
        // Two unresolved (one answered by you, one still theirs), one outdated,
        // one resolved: unresolved counts 2, only the reviewer's is awaiting you.
        let threads: Vec<Value> = serde_json::from_str(
            r#"[
              {"isResolved":false,"isOutdated":false,"comments":{"nodes":[
                {"author":{"login":"me"},"reactionGroups":[]}]}},
              {"isResolved":false,"isOutdated":false,"comments":{"nodes":[
                {"author":{"login":"them"},"reactionGroups":[]}]}},
              {"isResolved":false,"isOutdated":true,"comments":{"nodes":[
                {"author":{"login":"them"},"reactionGroups":[]}]}},
              {"isResolved":true,"isOutdated":false,"comments":{"nodes":[
                {"author":{"login":"them"},"reactionGroups":[]}]}}
            ]"#,
        )
        .unwrap();
        assert_eq!(count_open(&threads, "me"), (2, 1));
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
            "viewer",
            &thread_node("PRRT_1", false, false, &["alice"]),
            Some("Y3Vy"),
        );
        let (page, next) = parse_thread_page(&v, 10001).unwrap();

        assert_eq!(page.viewer, "viewer");
        assert_eq!(page.head_sha.as_deref(), Some("abc123"));
        assert_eq!(next.as_deref(), Some("Y3Vy"));

        let t = &page.items[0];
        assert_eq!(t.id, "PRRT_1");
        assert_eq!(t.path.as_deref(), Some("src/Foo.php"));
        assert_eq!(t.line, Some(42));
        assert_eq!(t.start_line, None);
        assert_eq!(t.original_line, Some(40));
        assert_eq!(t.author(), Some("alice"));
        assert_eq!(t.diff_hunk(), Some("@@ -1 +1 @@\n+x"));
        assert_eq!(t.comments[0].database_id, 100);
    }

    #[test]
    fn the_last_page_reports_no_cursor() {
        let v = thread_page("viewer", &thread_node("PRRT_1", false, false, &["alice"]), None);
        assert_eq!(parse_thread_page(&v, 10001).unwrap().1, None);
    }

    #[test]
    fn a_next_page_without_a_cursor_stops_rather_than_looping() {
        // hasNextPage with a null endCursor would re-request the same page
        // forever; the cursor, not the flag, is what continues the loop.
        let v = node(
            r#"{"data":{"viewer":{"login":"viewer"},"repository":{"pullRequest":{
                "headRefOid":"abc123","reviewThreads":{
                  "pageInfo":{"hasNextPage":true,"endCursor":null},"nodes":[]}}}}}"#,
        );
        assert_eq!(parse_thread_page(&v, 10001).unwrap().1, None);
    }

    #[test]
    fn a_missing_pull_request_is_an_error_not_an_empty_list() {
        // A deleted or mistyped PR must not read as "no threads to answer".
        let v = node(r#"{"data":{"viewer":{"login":"viewer"},"repository":{"pullRequest":null}}}"#);
        assert!(parse_thread_page(&v, 10001).is_none());
    }

    #[test]
    fn a_resolved_thread_needs_no_answer() {
        let v = thread_page("viewer", &thread_node("PRRT_1", true, false, &["alice"]), None);
        let (page, _) = parse_thread_page(&v, 10001).unwrap();
        assert!(!page.items[0].is_answerable("viewer"));
    }

    #[test]
    fn an_outdated_thread_still_needs_an_answer() {
        // The code moved, but the point may still stand — unlike the rail's
        // unresolved count, triage keeps these.
        let v = thread_page("viewer", &thread_node("PRRT_1", false, true, &["alice"]), None);
        let (page, _) = parse_thread_page(&v, 10001).unwrap();
        assert!(page.items[0].is_answerable("viewer"));
    }

    #[test]
    fn a_thread_you_answered_last_needs_no_second_answer() {
        let v = thread_page(
            "viewer",
            &thread_node("PRRT_1", false, false, &["alice", "viewer"]),
            None,
        );
        let (page, _) = parse_thread_page(&v, 10001).unwrap();
        assert!(!page.items[0].is_answerable("viewer"));

        // ...but one where they got the last word still does.
        let v = thread_page(
            "viewer",
            &thread_node("PRRT_2", false, false, &["alice", "viewer", "alice"]),
            None,
        );
        let (page, _) = parse_thread_page(&v, 10001).unwrap();
        assert!(page.items[0].is_answerable("viewer"));
    }

    #[test]
    fn a_deleted_author_reads_as_ghost_rather_than_dropping_the_comment() {
        let v = node(
            r#"{"data":{"viewer":{"login":"viewer"},"repository":{"pullRequest":{
                "headRefOid":"abc","reviewThreads":{"pageInfo":{"hasNextPage":false},
                "nodes":[{"id":"PRRT_1","isResolved":false,"isOutdated":false,
                  "comments":{"nodes":[
                    {"databaseId":1,"author":null,"body":"b","createdAt":"t","url":"u"}]}}]}}}}}"#,
        );
        let (page, _) = parse_thread_page(&v, 10001).unwrap();
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
        let got = GitHubForge::new("acme", "monorepo", token.value)
            .threads(10001)
            .expect("the fetch");

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
    fn a_thread_root_is_the_opening_comment_of_the_fetched_pr() {
        // The reply endpoint is nested under a PR *and* keyed on a comment id;
        // both come off the same fetch so they cannot disagree.
        let v = thread_page(
            "viewer",
            &thread_node("PRRT_1", false, false, &["alice", "viewer", "alice"]),
            None,
        );
        let (page, _) = parse_thread_page(&v, 10001).unwrap();
        let root = page.root_for("PRRT_1").expect("a root");
        assert_eq!(root.pr(), 10001);
        // The opening comment, not the latest one — replying to the last comment
        // would still thread, but the root is the only id we ever legitimately
        // post to, so it is the only one this hands out.
        assert_eq!(root.comment_id(), 100);
    }

    #[test]
    fn a_thread_id_that_is_not_in_the_fetch_yields_no_root() {
        // The triage agent's own input includes comments other people wrote, so an
        // id it hands back has to be looked up rather than trusted. There is no
        // other constructor: an unvalidated id cannot reach `gh`.
        let v = thread_page("viewer", &thread_node("PRRT_1", false, false, &["alice"]), None);
        let (page, _) = parse_thread_page(&v, 10001).unwrap();
        assert!(page.root_for("PRRT_somebody_elses_pr").is_none());
        assert!(page.root_for("").is_none());
    }

    #[test]
    fn a_thread_with_no_comments_yields_no_root() {
        let v = node(
            r#"{"data":{"viewer":{"login":"viewer"},"repository":{"pullRequest":{
                "headRefOid":"abc","reviewThreads":{"pageInfo":{"hasNextPage":false},
                "nodes":[{"id":"PRRT_1","isResolved":false,"isOutdated":false,
                  "comments":{"nodes":[]}}]}}}}}"#,
        );
        let (page, _) = parse_thread_page(&v, 1).unwrap();
        assert!(page.root_for("PRRT_1").is_none());
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
        assert_eq!(parse_pr(&n, "me").unwrap().checks, Checks::Failing);
    }

    #[test]
    fn a_missing_rollup_is_unknown_not_passing() {
        let n = node(r#"{"number":1,"title":"t","headRefName":"a","baseRefName":"develop"}"#);
        assert_eq!(parse_pr(&n, "me").unwrap().checks, Checks::Unknown);
    }

    fn pr(number: u64, head: &str, base: &str) -> Pr {
        Pr {
            number,
            title: String::new(),
            url: String::new(),
            head_ref: head.into(),
            head_repo: None,
            head_pushable: None,
            base_ref: base.into(),
            is_draft: false,
            mergeable: "MERGEABLE".into(),
            merge_state: "CLEAN".into(),
            checks: Checks::Passing,
            head_sha: None,
            unresolved: 0,
            unresolved_capped: false,
            awaiting_you: 0,
            changes_requested: false,
            needs_you: false,
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

}
