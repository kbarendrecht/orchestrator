//! Filing a tracker story for a review comment that is fair but out of scope.
//!
//! "I'll pick it up in a follow-up PR" is a promise with nothing behind it; a
//! story link is a promise with a tracking number, and the reply then says
//! something checkable. So a `story+reply` position files one and replies with
//! its id.
//!
//! **The tracker is MCP-only**, so this is the one place the daemon borrows an
//! agent for a *value* rather than for a session. What that buys is not credential
//! avoidance — the Shortcut MCP entry is `Bearer ${SHORTCUT_API_TOKEN}`, so the
//! same token is needed either way — but the repo's own
//! `.claude/skills/shortcut/SKILL.md`: the language to write in, the Backlog state,
//! the team id, epic routing by category, priority only settable by a follow-up
//! update. A daemon-side template would hardcode those and write a worse story.
//!
//! **Stories are re-derived, not remembered.** An earlier draft made the story id
//! "the single exception to derive, do not remember" and kept a ledger. That was
//! wrong for the same reason a reply ledger was: it can be killed between the tool
//! call succeeding and the write, and then lies in exactly the case it exists for.
//! Instead the story body always carries the thread's permalink — appended by the
//! daemon, not trusted to the agent — and the filer searches for a story
//! containing it before creating one. A duplicate is then impossible at the
//! source, and a retry heals rather than stranding the thread.
//!
//! [`Cache`] therefore is what its name says. It saves an agent run and drives the
//! report's "reused" wording; losing it costs latency, not correctness, which is
//! why it may degrade to empty like every other store here.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// The tracker's variable and its value, for a session that may or may not need one.
///
/// `None` means "nothing to hand over" — no tracker configured, or no token in the
/// daemon's environment — and says so in silence, because boot already logs the
/// missing token once (`lib.rs`) and a warning per spawn would only bury it.
///
/// The story pass keeps the hard [`resolve_token`] instead: a run that files into a
/// tracker with no credential can only fail mid-flight, so it is refused up front.
///
/// `checkout` is the environment built so far, which is where the token comes from
/// when the daemon's own has none.
pub fn token_env_pair(
    kind: crate::config::TrackerKind,
    checkout: &[(String, String)],
) -> Option<(String, String)> {
    use crate::tracker::Tracker as _;
    let tracker = crate::tracker::TrackerImpl::for_kind(kind)?;
    let var = tracker.token_env();
    Some((var.to_string(), resolve_token(checkout, var).ok()?))
}

/// The tracker's API token, for its MCP server's `Authorization` header.
///
/// **Environment only.** The forge keeps a file ladder because its token is read
/// on every poll from the daemon's own process; this one is only ever handed to a
/// child, so a file bought nothing but a second place for a credential to sit at
/// the wrong mode. One source, and it is the one already in your shell.
///
/// Deliberately **not** a reader for the repo's `.env`, where a team's copy
/// actually lives. That file is shell-ish, and the line as it stands is
/// `SHORTCUT_API_TOKEN='' # can be generated in …`; a naive split yields
/// `'' # can be` and injects a garbage Bearer, which surfaces later as "the
/// tracker is down" rather than "the token is not set".
///
/// `checkout` is that same team copy read *correctly* — by the tool that owns the
/// file ([`crate::env_source`]), under the name the tracker's MCP entry expands.
/// It is a fallback, not the first answer: `ORCHD_TRACKER_TOKEN` is what an
/// operator set for this daemon, and a checkout must not be able to redirect
/// filing by exporting a token of its own.
pub fn resolve_token(checkout: &[(String, String)], var: &str) -> Result<String> {
    if let Ok(v) = std::env::var("ORCHD_TRACKER_TOKEN") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Ok(v);
        }
    }
    // Last wins, the same rule the pty applies to these pairs.
    if let Some((_, v)) = checkout.iter().rev().find(|(k, _)| k == var) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Ok(v);
        }
    }
    bail!("no tracker token: set ORCHD_TRACKER_TOKEN in the daemon's environment, or {var} in the checkout")
}

/// A story that exists in the tracker.
///
/// Both halves come from the tool response and neither is ever constructed by
/// `format!`: the org slug in the URL belongs to your tracker workspace and the daemon has no
/// business knowing it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoryRef {
    /// Short form, `sc-12345`. What the report shows.
    pub id: String,
    /// The clickable one, `https://app.shortcut.com/<org>/story/12345`.
    pub url: String,
}

impl StoryRef {
    /// What `{story}` becomes in the posted reply.
    ///
    /// A markdown link rather than either half alone: the skill's rule is "never
    /// a bare number, always the full URL" because a colleague has to be able to
    /// click it, and a naked URL mid-sentence reads badly in prose. The
    /// substitution is deterministic given `(id, url)`, so `already_replied`'s
    /// exact match still recognises a reply it posted before.
    pub fn link(&self) -> String {
        format!("[{}]({})", self.id, self.url)
    }

    /// Does the URL actually point at this id, **on the tracker's own host**?
    ///
    /// The agent hands back both, and an id it invented for a story it never
    /// created would put a permanent public link to *somebody else's* story into
    /// a reply. The id's number appearing in the URL is what ties the two together.
    ///
    /// Matched as a whole path segment rather than as a substring, because
    /// Shortcut hands out URLs both bare and with a title slug on the end, and a
    /// slug can carry digits of its own.
    ///
    /// **The host and the scheme are checked too, and that is not paranoia.** This
    /// used to accept any URL carrying the number as a segment, so
    /// `http://attacker.example/12345` passed — and both halves of the pair come
    /// out of agent output whose *input* is third-party review comments, with the
    /// result posted publicly as a link somebody is meant to click. So: `https`
    /// only, an exact host match (which also rules out `app.shortcut.com.evil.com`
    /// and a userinfo prefix, since the authority is compared whole), and the
    /// number as a path segment.
    pub fn consistent(&self, host: &str) -> bool {
        let number = self.id.trim_start_matches(|c: char| !c.is_ascii_digit());
        if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        // Scheme, then authority, then path — no URL crate, because the shapes
        // being refused are exactly the ones a hand-rolled split gets right when
        // it compares the whole authority rather than searching inside it.
        let Some(rest) = self.url.strip_prefix("https://") else {
            return false;
        };
        let Some((authority, path)) = rest.split_once('/') else {
            return false;
        };
        // Hosts are case-insensitive; everything else here is not.
        if !authority.eq_ignore_ascii_case(host) {
            return false;
        }
        // The query and fragment are not path, and a number in either proves
        // nothing about which story this is.
        let path = path
            .split_once(['?', '#'])
            .map_or(path, |(before, _)| before);
        path.split('/').any(|seg| seg == number)
    }
}

/// One story asked for: which thread it answers, and the text approved on the card.
pub struct Wanted {
    pub thread_id: String,
    pub draft: crate::proposal::StoryDraft,
    /// The thread's own GitHub URL. Appended to the body, and the key the filer
    /// searches on — which is what makes a second run find this story rather than
    /// create another.
    pub permalink: String,
}

/// A story that now exists, and whether this batch is what created it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filed {
    pub story: StoryRef,
    /// It was already there — from the cache, or found by the search. The report
    /// says so, because "filed" and "already filed" read the same to a reviewer
    /// but not to someone deciding whether a retry worked.
    pub reused: bool,
}

/// Per thread: the story, or why there is not one.
///
/// A failure is a value rather than an error, because the batch carries on — the
/// thread simply stays open and its author is held back, which the report already
/// knows how to say.
pub type Results = HashMap<String, std::result::Result<Filed, String>>;

/// Stories filed per PR, keyed by the thread they answer.
///
/// Nested rather than keyed on a tuple because JSON object keys are strings, and
/// a `(u64, String)` key would have to be encoded and parsed back — a format to
/// get wrong for no gain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cache {
    #[serde(default)]
    pub by_pr: HashMap<u64, HashMap<String, StoryRef>>,
}

impl Cache {
    pub fn get(&self, pr: u64, thread_id: &str) -> Option<&StoryRef> {
        self.by_pr.get(&pr)?.get(thread_id)
    }

    pub fn put(&mut self, pr: u64, thread_id: &str, story: StoryRef) {
        self.by_pr
            .entry(pr)
            .or_default()
            .insert(thread_id.to_string(), story);
    }

    /// Never pruned by PR. A merged PR's stories still matter to a late retry,
    /// and the whole file is a handful of ids.
    pub fn len(&self) -> usize {
        self.by_pr.values().map(HashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// What the agent writes into the drop file.
#[derive(Debug, Deserialize)]
struct Report {
    #[serde(default)]
    stories: Vec<Reported>,
}

#[derive(Debug, Deserialize)]
struct Reported {
    thread_id: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    url: Option<String>,
    /// False when the search found it already there.
    #[serde(default)]
    created: bool,
    #[serde(default)]
    error: Option<String>,
}

/// The line appended to every story body.
///
/// Load-bearing, not decoration: it is what the filer searches for, so a run that
/// created a story and died before reporting is found rather than duplicated. The
/// daemon writes it so the agent cannot forget to, and so its exact shape is one
/// thing rather than a prompt instruction that might drift.
///
/// Deliberately English regardless of `default_language`, which governs what the
/// agent *writes* — this is a key the daemon matches on, and a key that changes
/// wording with a config setting is a key that stops matching. It shipped in
/// Dutch, from the repo this was extracted out of.
fn source_line(pr: u64, permalink: &str) -> String {
    format!("Source: review of #{pr} — {permalink}")
}

/// File every story this batch needs, in one agent run.
///
/// One launch for all of them: two story positions must not mean two cold starts
/// each reading a 200-line skill, inside an HTTP request the SPA is blocking on.
///
/// Never returns `Err`. A failure belongs to a thread, and the batch continues —
/// the thread stays open and holds its author back from a re-request on its own.
pub async fn file_all(
    app: &std::sync::Arc<crate::state::AppState>,
    pr: u64,
    wanted: &[Wanted],
) -> Results {
    let mut out: Results = HashMap::new();
    if wanted.is_empty() {
        return out;
    }

    // The cache first, so a retry of a half-finished batch only asks about what is
    // actually missing.
    let mut todo: Vec<&Wanted> = Vec::new();
    {
        let inner = app.inner.read().await;
        for w in wanted {
            match inner.stories.get(pr, &w.thread_id) {
                Some(hit) => {
                    out.insert(
                        w.thread_id.clone(),
                        Ok(Filed {
                            story: hit.clone(),
                            reused: true,
                        }),
                    );
                }
                None => todo.push(w),
            }
        }
    }
    if todo.is_empty() {
        return out;
    }

    // The host a reported URL has to be on, resolved here rather than at the top:
    // a cache hit needs no tracker and must keep working without one, which is what
    // resolving it earlier broke.
    let tracker_host = {
        use crate::tracker::Tracker as _;
        match crate::tracker::TrackerImpl::for_kind(app.cfg.tracker) {
            Some(t) => t.host(),
            // Nothing configured to file into, so there is no URL to trust and no
            // run to make. Said per thread, because the caller reports per thread.
            None => {
                for w in &todo {
                    out.insert(
                        w.thread_id.clone(),
                        Err("no tracker is configured, so no story can be filed".to_string()),
                    );
                }
                return out;
            }
        }
    };

    match run_filer(app, pr, &todo).await {
        Ok(reported) => {
            for w in &todo {
                match reported.iter().find(|r| r.thread_id == w.thread_id) {
                    Some(r) => {
                        out.insert(w.thread_id.clone(), accept(r, tracker_host));
                    }
                    // Every thread given was required to come back. A missing one
                    // is not "nothing happened" — the story may exist — so it says
                    // what a retry will do rather than implying a clean slate.
                    None => {
                        out.insert(
                            w.thread_id.clone(),
                            Err(
                                "the story run reported nothing for this thread. If it got as \
                                 far as filing, the next attempt will find that story by its \
                                 link back to the thread rather than making a second one."
                                    .to_string(),
                            ),
                        );
                    }
                }
            }
            // Cache what landed, so a retry does not pay for the agent again.
            let filed: Vec<(String, StoryRef)> = out
                .iter()
                .filter_map(|(t, r)| r.as_ref().ok().map(|f| (t.clone(), f.story.clone())))
                .collect();
            if !filed.is_empty() {
                let mut inner = app.inner.write().await;
                inner.with_stories("stories filed", |c| {
                    for (thread, story) in filed {
                        c.put(pr, &thread, story);
                    }
                    true
                });
            }
        }
        Err(e) => {
            let why = format!("{e:#}");
            for w in &todo {
                out.insert(w.thread_id.clone(), Err(why.clone()));
            }
        }
    }
    out
}

/// Turn one reported entry into a result, refusing a pair that does not hang
/// together.
fn accept(r: &Reported, host: &str) -> std::result::Result<Filed, String> {
    if let Some(e) = &r.error {
        return Err(e.clone());
    }
    let (Some(id), Some(url)) = (&r.id, &r.url) else {
        return Err("reported neither a story nor an error".to_string());
    };
    let story = StoryRef {
        id: id.trim().to_string(),
        url: url.trim().to_string(),
    };
    if !story.consistent(host) {
        // The one check that matters here: a fabricated pair would put a permanent
        // public link to somebody else's story into a comment on a colleague's
        // review, and nothing downstream would notice.
        return Err(format!(
            "reported {} with url {}, which is not that story — refusing to link it",
            story.id, story.url
        ));
    }
    Ok(Filed {
        story,
        reused: !r.created,
    })
}

/// Spawn the filer, wait for it, read what it wrote.
async fn run_filer(
    app: &std::sync::Arc<crate::state::AppState>,
    pr: u64,
    todo: &[&Wanted],
) -> Result<Vec<Reported>> {
    use crate::config::Config;
    use crate::model::{Kind, Session};
    use crate::pty::PtyHandle;

    use crate::tracker::Tracker as _;
    let tracker = crate::tracker::TrackerImpl::for_kind(app.cfg.tracker)
        .context("no tracker configured, so there is nothing to file into")?;
    let head_ref = {
        let inner = app.inner.read().await;
        inner
            .pr(pr)
            .map(|p| p.head_ref.clone())
            .with_context(|| format!("PR #{pr} is not in the current poll"))?
    };
    let workspace = crate::api::workspace_for(app, &head_ref)
        .await
        .with_context(|| format!("no worktree holding {head_ref}"))?;
    let path = app
        .workspace_path(&workspace)
        .await
        .context("the worktree vanished")?;

    // Scratch under the daemon's own config dir, not the worktree and not
    // elsewhere in the checkout. Both alternatives are broken: the repo's
    // `worktree-edit-boundary` hook blocks a write under the main checkout that
    // lands outside the worktree, and a file *inside* the worktree would make it
    // dirty, which is the gate `post::run` re-checks.
    let scratch = Config::config_dir()?.join(format!("story-{pr}"));
    std::fs::create_dir_all(&scratch)?;
    let drop_file = scratch.join("stories.json");
    // Cleared per run, so a previous run's report can never be read as this one's.
    // Only the file: the directory may hold state that has to outlive a run.
    let _ = std::fs::remove_file(&drop_file);

    let drafts: Vec<serde_json::Value> = todo
        .iter()
        .map(|w| {
            let body = format!(
                "{}\n\n{}",
                w.draft.body.trim_end(),
                source_line(pr, &w.permalink)
            );
            serde_json::json!({
                "thread_id": w.thread_id,
                "title": w.draft.title,
                "body": body,
            })
        })
        .collect();

    let (owner, repo) =
        crate::resolve_repo(app).context("no GitHub repo configured and none on the remote")?;
    let body = crate::prompt::render(
        tracker.prompt(),
        &crate::prompt::Vars {
            pr,
            owner,
            repo,
            stories: serde_json::to_string_pretty(&drafts)?,
            drop_file: drop_file.to_string_lossy().into_owned(),
            ..Default::default()
        },
    )?;

    let id = uuid::Uuid::new_v4();
    let settings = Config::hooks_settings_path()?;
    let mut cmd = vec![
        "claude".to_string(),
        "-p".to_string(),
        body,
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--session-id".to_string(),
        id.to_string(),
        "--settings".to_string(),
        settings.to_string_lossy().into_owned(),
        // Scoped to the tracker server, reading the skill, and writing its report.
        //
        // `mcp__<server>` without parentheses, because MCP rules do not support
        // them — and the whole server rather than a list of tool names, because
        // the skill routes through `epics-search`, `labels-list`,
        // `workflows-list` and more, and an enumerated allowlist would fight it
        // and fail as a silent mid-run denial.
        //
        // **Bare `Write`.** Measured, because all three plausible spellings
        // behave differently: `Write` permits creating the report, `Edit` does
        // not (creating a file is the Write tool, and an Edit rule does not cover
        // it), and `Write(<path>)` matches nothing at all — so it reads as a tight
        // rule and denies everything. Where it may write is scoped by `--add-dir`
        // instead, which is the only mechanism that actually constrains a path.
        "--allowedTools".to_string(),
        format!("mcp__{} Read Write", tracker.mcp_server()),
        // The scratch dir is outside the worktree, so it has to be granted.
        "--add-dir".to_string(),
        scratch.to_string_lossy().into_owned(),
    ];
    if app.cfg.tracker == crate::config::TrackerKind::Stub {
        // Only the stub, and nothing else: `--strict-mcp-config` ignores every
        // configured server, which is what keeps a verification run from reaching
        // the real tracker by accident.
        cmd.push("--mcp-config".to_string());
        cmd.push(stub_config(&scratch)?.to_string_lossy().into_owned());
        cmd.push("--strict-mcp-config".to_string());
    }

    // The tracker variable this run needs is pushed by `session_env` now, for every
    // session rather than only this one.
    let (env, unset) = crate::config::session_env(&app.cfg, &path, id, None);
    // Still refused before the agent runs: `session_env` shrugs when there is no
    // token, which is right for every other session and not for this one. Asked of
    // the environment it just built rather than of the daemon's, because the
    // checkout's own copy is the other place a token comes from.
    resolve_token(&env, tracker.token_env())?;

    let spawned = PtyHandle::spawn(&cmd, &path, &env, &unset, crate::spawn::DEFAULT_SIZE)?;
    let worktree = path.clone();
    let handle = spawned.handle.clone();
    // A real session, so its pty is there to read when a story goes wrong. It is
    // automation like `fix-pr` and triage, and archives the same way.
    let mut session = Session::new(
        id,
        workspace,
        path,
        Kind::Automation {
            pr,
            command: "story".to_string(),
        },
    );
    session.pty = Some(spawned.handle.clone());
    session.pid = spawned.pid;
    app.inner.write().await.sessions.insert(id, session);
    app.notify().await;

    // The one timeout in this daemon. Every other agent runs under a rail entry
    // somebody is watching; this one runs inside an HTTP request the SPA is
    // blocking on, so a hang has to end by itself.
    let budget = std::time::Duration::from_secs(app.cfg.story_timeout_seconds);
    let timed_out = tokio::time::timeout(budget, handle.wait()).await.is_err();
    if timed_out {
        let _ = handle.kill();
        tracing::warn!(pr, session = %id, "story run timed out after {budget:?}");
    }
    app.notify().await;

    // The permission model can scope *where* it may write but not stop it writing
    // into the worktree it runs in, and the prompt is the only thing telling it not
    // to. So the tree is checked rather than assumed. Nothing here is recoverable —
    // the commit is already pushed — but leftover junk would otherwise surface as a
    // confusing `Gate::Dirty` on the next review, with no clue where it came from.
    match crate::git::is_clean(&worktree) {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            pr, session = %id,
            "the story run left the worktree dirty; it was told not to write there"
        ),
        Err(e) => tracing::warn!("could not check the worktree after the story run: {e:#}"),
    }

    let raw = std::fs::read_to_string(&drop_file).map_err(|_| {
        if timed_out {
            anyhow::anyhow!(
                "the story run was killed after {}s without reporting. If it got as far as \
                 filing, the next attempt finds that story by its link back to the thread.",
                app.cfg.story_timeout_seconds
            )
        } else {
            anyhow::anyhow!(
                "the story run exited without writing its report. Its session is in the rail; \
                 a retry searches the tracker first, so it will not file twice."
            )
        }
    })?;
    let report: Report = serde_json::from_str(&raw)
        .with_context(|| format!("the story run wrote something unparseable to {drop_file:?}"))?;
    Ok(report.stories)
}

/// Write the stub server's MCP config next to the drop file.
///
/// A real stdio MCP server *named* `shortcut`, so the tool names the prompt and
/// the skill use are byte-identical to the live ones and there is no difference in
/// the daemon between stub and live beyond which flags are passed.
fn stub_config(scratch: &Path) -> Result<std::path::PathBuf> {
    let script = std::env::current_dir()?.join("tools/stub-shortcut-mcp.py");
    anyhow::ensure!(
        script.exists(),
        "tracker is `stub` but {} is missing",
        script.display()
    );
    // The log lives outside the per-run scratch, because it is the stub's whole
    // database — the created stories are replayed from it so `stories-search` can
    // find them. Keeping it in the scratch dir made it disappear with every wipe,
    // which quietly turned the search into something that could never hit, and made
    // "the retry heals" look proven when it had merely re-created the story under
    // the same id.
    let log = crate::config::Config::config_dir()?.join("story-stub.jsonl");
    let cfg = serde_json::json!({
        "mcpServers": {
            "shortcut": {
                "type": "stdio",
                "command": "python3",
                "args": [script.to_string_lossy(), "--log", log.to_string_lossy()],
            }
        }
    });
    let path = scratch.join("stub-mcp.json");
    std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Drive the real filer against the stub MCP server.
    ///
    /// Ignored by default: it spawns a `claude` process, which is slow and needs a
    /// login. But it is the only thing that proves the parts a unit test cannot —
    /// that the flags let the agent reach `mcp__shortcut` and write its report at
    /// all, that the prompt is followed, and that the search finds a story a
    /// previous run created instead of making a second one.
    ///
    /// It deliberately does **not** post to GitHub. The reply path is already
    /// proven by `forge::github_write::posts_for_real`, and pointing this at a real PR
    /// would notify a colleague to verify a stub.
    ///
    /// ```text
    /// cargo test --lib -- --ignored --nocapture files_for_real_against_the_stub
    /// ```
    #[tokio::test]
    #[ignore = "spawns a claude process"]
    async fn files_for_real_against_the_stub() {
        use crate::config::{Config, TrackerKind};

        let cfg = Config::load_or_init(None).expect("the daemon's own config");
        // The agent has to run inside a real checkout of the repo, or `.mcp.json`
        // and `.claude/skills/shortcut` do not resolve.
        let main = cfg.main_checkout.clone();
        assert!(
            main.join(".mcp.json").exists(),
            "{} has no .mcp.json, so no tracker server to approve",
            main.display()
        );

        // The stub ignores the value, but `resolve_token` still has to find one.
        std::env::set_var("ORCHD_TRACKER_TOKEN", "stub-token-not-used-by-the-stub");
        let cfg = Config {
            tracker: TrackerKind::Stub,
            // Long enough for a cold start plus the skill read.
            story_timeout_seconds: 300,
            ..cfg
        };
        let app = crate::state::AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        // A PR and a worktree, faked into place: this test is about the filer, not
        // about the poller or `ensure_pr_worktree`.
        let pr = 999_001;
        let head_ref = "worktree-story-test";
        {
            let mut inner = app.inner.write().await;
            let mut fake = fake_pr(pr, head_ref);
            fake.head_sha = Some("deadbeef".into());
            inner.prs.push(fake);
            // Onto `main`, which `AppState::new` already created — `register_worktree`
            // is `or_insert`, so it would not touch it. The filer only needs a
            // directory that is a real checkout of the repo; it does not rebase or
            // commit, so the main checkout is a safe place to run it.
            if let Some(w) = inner.workspaces.get_mut("main") {
                w.branches.insert(head_ref.to_string());
            }
        }
        let _ = main;

        let permalink = "https://github.com/o/r/pull/999001#discussion_r777";
        let wanted = vec![Wanted {
            thread_id: "PRRT_test_1".into(),
            draft: crate::proposal::StoryDraft {
                title: "Split the guard out of the service".into(),
                body: "The guard belongs in its own file.".into(),
            },
            permalink: permalink.into(),
        }];

        // --- first run: it must create exactly one story ---------------------
        let out = file_all(&app, pr, &wanted).await;
        let filed = out
            .get("PRRT_test_1")
            .expect("an answer for the thread")
            .as_ref()
            .unwrap_or_else(|e| panic!("the filer failed: {e}"));
        eprintln!("filed {} at {}", filed.story.id, filed.story.url);
        assert!(!filed.reused, "the first run created it");
        assert!(filed.story.consistent(HOST), "id and url must agree");
        assert!(
            filed.story.link().contains(&filed.story.id),
            "the reply substitution carries the id"
        );

        let log = Config::config_dir().unwrap().join("story-stub.jsonl");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        let created = calls.lines().filter(|l| l.contains("\"created\"")).count();
        assert_eq!(created, 1, "exactly one story per batch:\n{calls}");
        // The permalink has to be in the description, or the search below cannot
        // find it and a retry would duplicate.
        assert!(
            calls.contains(permalink),
            "the body must carry the thread link:\n{calls}"
        );

        // --- second run, cache warm: no agent, same story --------------------
        let again = file_all(&app, pr, &wanted).await;
        let hit = again.get("PRRT_test_1").unwrap().as_ref().unwrap();
        assert!(hit.reused, "the cache answered");
        assert_eq!(hit.story, filed.story);

        // --- third run, cache cleared: the search must find it ---------------
        //
        // This is the path that makes stories re-derivable rather than remembered:
        // a run that filed and then died leaves no record, and the next attempt has
        // to find the story instead of making another.
        app.inner.write().await.stories = Cache::default();
        let healed = file_all(&app, pr, &wanted).await;
        let found = healed
            .get("PRRT_test_1")
            .unwrap()
            .as_ref()
            .unwrap_or_else(|e| panic!("the search did not heal: {e}"));
        assert_eq!(found.story, filed.story, "found the same story");
        assert!(found.reused, "found rather than created");

        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        let created = calls.lines().filter(|l| l.contains("\"created\"")).count();
        assert_eq!(created, 1, "still exactly one story:\n{calls}");
    }

    /// A run that does not finish in time is killed, and says what a retry will do.
    ///
    /// The budget is deliberately far too short — shorter than a cold start — so the
    /// kill path is exercised rather than waited for. What matters is not the
    /// timeout itself but the wording: the story may or may not exist, so the
    /// message must not imply a clean slate. This is the case that would file a
    /// duplicate if the filer trusted a ledger instead of searching.
    #[tokio::test]
    #[ignore = "spawns a claude process"]
    async fn a_run_that_overruns_is_killed_and_says_what_a_retry_does() {
        use crate::config::{Config, TrackerKind};

        let cfg = Config::load_or_init(None).expect("config");
        let main = cfg.main_checkout.clone();
        std::env::set_var("ORCHD_TRACKER_TOKEN", "stub-token");
        let cfg = Config {
            tracker: TrackerKind::Stub,
            story_timeout_seconds: 5,
            ..cfg
        };
        let app = crate::state::AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let pr = 999_002;
        let head_ref = "worktree-story-timeout";
        {
            let mut inner = app.inner.write().await;
            inner.prs.push(fake_pr(pr, head_ref));
            if let Some(w) = inner.workspaces.get_mut("main") {
                w.branches.insert(head_ref.to_string());
            }
        }
        let _ = main;

        let out = file_all(
            &app,
            pr,
            &[Wanted {
                thread_id: "PRRT_timeout".into(),
                draft: crate::proposal::StoryDraft {
                    title: "Never finished".into(),
                    body: "This run gets killed.".into(),
                },
                permalink: "https://github.com/o/r/pull/999002#discussion_r1".into(),
            }],
        )
        .await;

        let err = out
            .get("PRRT_timeout")
            .expect("an answer even for a killed run")
            .as_ref()
            .expect_err("a killed run cannot have filed anything it could report");
        eprintln!("reported: {err}");
        assert!(err.contains("killed"), "{err}");
        // The load-bearing half. "Nothing happened" would be a lie, and the retry
        // has to be described as safe or nobody will press it.
        assert!(
            err.contains("link back to the thread"),
            "the message must say the retry finds the story rather than duplicating: {err}"
        );
        // Nothing was cached, so a retry goes back to the agent — which searches.
        assert!(app.inner.read().await.stories.is_empty());
    }

    fn fake_pr(number: u64, head_ref: &str) -> crate::forge::Pr {
        crate::forge::Pr {
            number,
            title: "story test".into(),
            url: String::new(),
            head_ref: head_ref.into(),
            head_repo: None,
            head_pushable: None,
            base_ref: "develop".into(),
            is_draft: false,
            mergeable: "MERGEABLE".into(),
            merge_state: "CLEAN".into(),
            checks: crate::forge::Checks::Unknown,
            head_sha: None,
            unresolved: 0,
            unresolved_capped: false,
            awaiting_you: 0,
            changes_requested: false,
            needs_you: false,
            children: vec![],
        }
    }

    fn story() -> StoryRef {
        StoryRef {
            id: "sc-12345".into(),
            url: "https://app.shortcut.com/acme/story/12345".into(),
        }
    }

    #[test]
    fn the_substitution_is_clickable_and_short() {
        assert_eq!(
            story().link(),
            "[sc-12345](https://app.shortcut.com/acme/story/12345)"
        );
    }

    /// The tracker's host, as `Shortcut::host` reports it.
    const HOST: &str = "app.shortcut.com";

    #[test]
    fn an_id_that_does_not_match_its_url_is_refused() {
        // The agent hands back both. If they disagree, one of them is invented,
        // and posting the link would point a colleague at someone else's story.
        assert!(story().consistent(HOST));

        let mut swapped = story();
        swapped.url = "https://app.shortcut.com/acme/story/99999".into();
        assert!(!swapped.consistent(HOST));

        // Shortcut hands out both forms; a title slug on the end is still the
        // same story.
        let mut slugged = story();
        slugged.url =
            "https://app.shortcut.com/acme/story/12345/document-the-schedules".into();
        assert!(slugged.consistent(HOST));

        // ...and a slug carrying digits of its own must not stand in for the id.
        let mut decoy = story();
        decoy.id = "sc-777".into();
        decoy.url = "https://app.shortcut.com/acme/story/12345/fix-777-errors".into();
        assert!(
            !decoy.consistent(HOST),
            "matched a slug instead of the id segment"
        );

        let mut empty = story();
        empty.id = "sc-".into();
        assert!(!empty.consistent(HOST));
    }

    /// **The URL is agent output, and its input is third-party review text.** The
    /// pair ends up as a permanent public link in a reply, so a number appearing
    /// somewhere in the string was never enough: every URL below carries the right
    /// story number and every one of them must still be refused.
    #[test]
    fn a_url_off_the_trackers_host_is_refused() {
        let with = |url: &str| {
            let mut s = story();
            s.url = url.into();
            s.consistent(HOST)
        };

        assert!(!with("http://attacker.example/12345"), "another host entirely");
        assert!(!with("https://attacker.example/story/12345"), "https, still not ours");
        // The shapes a substring check on the host would have let through.
        assert!(!with("https://app.shortcut.com.evil.example/story/12345"), "suffixed host");
        assert!(!with("https://evil.example/app.shortcut.com/story/12345"), "host in the path");
        assert!(
            !with("https://app.shortcut.com@evil.example/story/12345"),
            "userinfo pointing elsewhere"
        );
        // Scheme matters: a link somebody clicks should not be downgradeable.
        assert!(!with("http://app.shortcut.com/acme/story/12345"), "plain http");
        assert!(!with("//app.shortcut.com/acme/story/12345"), "no scheme");
        // A number in the query or the fragment is not a path segment.
        assert!(!with("https://app.shortcut.com/acme/story/999?id=12345"), "query");
        assert!(!with("https://app.shortcut.com/acme/story/999#12345"), "fragment");
        // And the host on its own, with no path, names no story.
        assert!(!with("https://app.shortcut.com"), "no path at all");

        // The real thing still passes, including a differently-cased host.
        assert!(with("https://app.shortcut.com/acme/story/12345"));
        assert!(with("https://APP.Shortcut.COM/acme/story/12345"), "hosts are case-insensitive");
    }

    #[test]
    fn the_cache_is_keyed_by_pr_and_thread() {
        let mut c = Cache::default();
        assert!(c.is_empty());
        c.put(10001, "PRRT_1", story());
        assert_eq!(c.get(10001, "PRRT_1"), Some(&story()));
        // Same thread id under a different PR is a different story.
        assert_eq!(c.get(10004, "PRRT_1"), None);
        assert_eq!(c.get(10001, "PRRT_2"), None);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn the_cache_survives_a_round_trip() {
        let mut c = Cache::default();
        c.put(10001, "PRRT_1", story());
        let back: Cache = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.get(10001, "PRRT_1"), Some(&story()));
    }
}
