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
//! [`crate::model::Cache`] therefore is what its name says. It saves an agent run and drives the
//! report's "reused" wording; losing it costs latency, not correctness, which is
//! why it may degrade to empty like every other store here.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::model::StoryRef;
use std::collections::HashMap;
use std::path::Path;

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
    // actually missing. The host is passed when there is one, so a stored entry is
    // held to the same rule the agent's own answer is; a cache hit still works
    // without a tracker, and `Cache::get` then checks what it can.
    let known_host = app.cfg.tracker.as_ref().map(|t| t.host.as_str());
    let mut todo: Vec<&Wanted> = Vec::new();
    {
        let inner = app.inner.read().await;
        for w in wanted {
            match inner.stories.get(pr, &w.thread_id, known_host) {
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
        match app.cfg.tracker.as_ref().map(|t| t.host.as_str()) {
            Some(h) => h,
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
    // The one check that matters here: a fabricated pair would put a permanent
    // public link to somebody else's story into a comment on a colleague's review,
    // and nothing downstream would notice. It is the constructor rather than a
    // check beside one, so there is no way to build the value without it.
    let story = StoryRef::new(id, url, host).ok_or_else(|| {
        format!(
            "reported {} with url {}, which is not that story — refusing to link it",
            id.trim(),
            url.trim()
        )
    })?;
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
    use crate::model::{Pass, Session};

    let tracker = app
        .cfg
        .tracker
        .as_ref()
        .context("no tracker configured, so there is nothing to file into")?;
    let mcp_server = &tracker.mcp_server;
    let head_ref = {
        let inner = app.inner.read().await;
        inner
            .pr(pr)
            .map(|p| p.head_ref.clone())
            .with_context(|| format!("PR #{pr} is not in the current poll"))?
    };
    let workspace = app
        .workspace_for(&head_ref)
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

    /* The entries go in a file rather than into the prompt, because the prompt is
    a skill now and a skill is static: `/orchd:story <pr>` is one line, so what
    a template used to substitute has to be somewhere the agent can read. The
    resolve run's plan is the same shape for the same reason.
    Dropped with the substitution: the `git remote` call that resolved
    `owner/repo` for a sentence. It was the only thing that could fail this
    spawn for a reason unrelated to filing a story. */
    let stories_file = scratch.join("stories.json");
    std::fs::write(&stories_file, serde_json::to_string_pretty(&drafts)?)
        .with_context(|| format!("writing {}", stories_file.display()))?;

    let id = uuid::Uuid::new_v4();
    let mut cmd = vec![
        "claude".to_string(),
        "-p".to_string(),
        // The skill, typed. Measured against 2.1.263 that `-p "/orchd:<name>"`
        // expands under a tight `--allowedTools`: the allowlist gates *tool calls*
        // and Claude Code expands a typed command before the model acts. The
        // comment below used to say the opposite and it was never measured.
        format!("/orchd:{} {pr}", Pass::STORY),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--session-id".to_string(),
        id.to_string(),
        // Scoped to the tracker server, reading the skill, and writing its report.
        //
        // `mcp__<server>` without parentheses, because MCP rules do not support
        // them — and the whole server rather than a list of tool names, because
        // the repo's tracker skill routes through search, labels, workflows and
        // whatever else that tracker needs, an enumerated allowlist would fight it
        // and fail as a silent mid-run denial, and the daemon deliberately knows
        // none of those names (`config::Tracker` says why).
        //
        // **Bare `Write`.** Measured, because all three plausible spellings
        // behave differently: `Write` permits creating the report, `Edit` does
        // not (creating a file is the Write tool, and an Edit rule does not cover
        // it), and `Write(<path>)` matches nothing at all — so it reads as a tight
        // rule and denies everything. Where it may write is scoped by `--add-dir`
        // instead, which is the only mechanism that actually constrains a path.
        "--allowedTools".to_string(),
        format!("mcp__{mcp_server} Read Write"),
        // The scratch dir is outside the worktree, so it has to be granted.
        "--add-dir".to_string(),
        scratch.to_string_lossy().into_owned(),
    ];
    // The plugin dir, which this run's own instructions come out of.
    //
    // It used to say the allowlist above meant this run could not invoke a skill.
    // That was never measured and is false: `claude -p "/orchd:orch"` under
    // `--allowedTools "Read Write"` runs the skill, because the allowlist gates
    // tool calls and a typed command is expanded before the model acts. Model-
    // *chosen* skills are a different question — those go through a tool.
    cmd.extend(crate::launch::session_flags()?);
    if tracker.stub {
        // Only the stub, and nothing else: `--strict-mcp-config` ignores every
        // configured server, which is what keeps a verification run from reaching
        // the real tracker by accident.
        cmd.push("--mcp-config".to_string());
        cmd.push(stub_config(&scratch)?.to_string_lossy().into_owned());
        cmd.push("--strict-mcp-config".to_string());
    }

    // The tracker variable this run needs is pushed by `session_env` now, for every
    // session rather than only this one.
    //
    // Off the runtime, like the three copies of this in `spawn.rs`: `session_env`
    // runs a bounded child (`mise env`, `direnv export`) that `run_bounded` polls
    // with `thread::sleep` for up to five seconds, and a tokio worker parked on
    // that is the whole board freezing while this run starts.
    /* What the skill reads instead of what a template substituted. The host is in
    here too, because the skill has to tell the agent which host a URL it hands
    back must be on — and that is now config rather than a constant the daemon
    could write into a prompt.

    Handed to `run_env` as its `extra` rather than pushed afterwards, so this run
    builds its environment through the one seam every other spawn uses. It has no
    ask token and no `Pass`, which is why both are `None` here. */
    let extra = vec![
        (
            crate::skills::VAR_STORIES.to_string(),
            stories_file.to_string_lossy().into_owned(),
        ),
        (
            crate::skills::VAR_DROP.to_string(),
            drop_file.to_string_lossy().into_owned(),
        ),
        (
            crate::skills::VAR_TRACKER_HOST.to_string(),
            tracker.host.to_string(),
        ),
    ];
    let (env, unset) = crate::proc::run_blocking("reading the session environment", {
        let (cfg, at) = (app.cfg.clone(), path.clone());
        move || crate::spawn::run_env(&cfg, &at, id, None, None, &extra)
    })
    .await?;
    // Still refused before the agent runs: `session_env` shrugs when there is no
    // token, which is right for every other session and not for this one. Asked of
    // the environment it just built rather than of the daemon's, because the
    // checkout's own copy is the other place a token comes from.
    /* Only when a variable is named. A tracker may authenticate itself out of an
    OAuth login the user did earlier — both official Linear and Atlassian servers
    are OAuth-first — and there is then nothing here to resolve and nothing to
    refuse the run for. */
    if let Some(var) = tracker.token_env.as_deref() {
        crate::config::resolve_token(&env, var)?;
    }

    // A real session, so its pty is there to read when a story goes wrong. It is a
    // run like `fix-pr` and triage, and archives the same way — and it goes
    // through the same two seams as every other spawn: `insert_and_spawn`, so the
    // record is in the map before the agent can fire a hook, and
    // `watch_session_exit`, the one observer that settles a pty ending. This used to
    // insert the record by hand after the spawn and settle nothing, so a filer's
    // row stayed live in the rail after the process had gone.
    let session = Session::new(
        id,
        workspace,
        path.clone(),
        Some(Pass {
            pr,
            command: Pass::STORY.to_string(),
        }),
    );
    let spawned =
        crate::spawn::insert_and_spawn(app, id, session, &cmd, &path, &env, &unset).await?;
    let worktree = path;
    let handle = spawned.handle.clone();
    crate::spawn::watch_session_exit(app.clone(), id, spawned.handle);
    app.notify().await;

    // The one timeout in this daemon. Every other agent runs under a rail entry
    // somebody is watching; this one runs inside an HTTP request the SPA is
    // blocking on, so a hang has to end by itself. Waiting here is only deciding
    // when to stop waiting; the record is settled by the watcher above.
    let budget = std::time::Duration::from_secs(app.cfg.story_timeout_seconds);
    let timed_out = tokio::time::timeout(budget, handle.wait()).await.is_err();
    if timed_out {
        // Escalating: a filer that trapped `SIGHUP` outlived its timeout and kept
        // the worktree, which is what the check below is about to inspect.
        handle.kill_gracefully().await;
        tracing::warn!(pr, session = %id, "story run timed out after {budget:?}");
    }
    app.notify().await;

    // The permission model can scope *where* it may write but not stop it writing
    // into the worktree it runs in, and the prompt is the only thing telling it not
    // to. So the tree is checked rather than assumed. Nothing here is recoverable —
    // the commit is already pushed — but leftover junk would otherwise surface as a
    // confusing `Gate::Dirty` on the next review, with no clue where it came from.
    // One hop for both: the `git status` is the expensive half, and the report is a
    // small file the daemon just caused to be written, at the same moment. Reading
    // it in a hop of its own bought a task dispatch and a thread handoff for a
    // `read_to_string`.
    let (clean, read) = crate::proc::run_blocking("checking the worktree after the story run", {
        let (w, f) = (worktree.clone(), drop_file.clone());
        move || (crate::git::is_clean(&w), std::fs::read_to_string(f))
    })
    .await?;
    match clean {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            pr, session = %id,
            "the story run left the worktree dirty; it was told not to write there"
        ),
        Err(e) => tracing::warn!("could not check the worktree after the story run: {e:#}"),
    }

    let raw = read.map_err(|_| {
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
        use crate::config::{Config, Tracker};

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
            tracker: Some(Tracker {
                mcp_server: "shortcut".into(),
                host: "app.shortcut.com".into(),
                token_env: Some("SHORTCUT_API_TOKEN".into()),
                stub: true,
            }),
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
        eprintln!("filed {} at {}", filed.story.id(), filed.story.url());
        assert!(!filed.reused, "the first run created it");
        assert!(
            StoryRef::new(filed.story.id(), filed.story.url(), "app.shortcut.com").is_some(),
            "id and url must agree"
        );
        assert!(
            filed.story.link().contains(filed.story.id()),
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
        app.inner.write().await.stories = crate::state::Durable::default();
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
        use crate::config::{Config, Tracker};

        let cfg = Config::load_or_init(None).expect("config");
        let main = cfg.main_checkout.clone();
        std::env::set_var("ORCHD_TRACKER_TOKEN", "stub-token");
        let cfg = Config {
            tracker: Some(Tracker {
                mcp_server: "shortcut".into(),
                host: "app.shortcut.com".into(),
                token_env: Some("SHORTCUT_API_TOKEN".into()),
                stub: true,
            }),
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
            title: "story test".into(),
            head_ref: head_ref.into(),
            ..crate::testutil::pr(number)
        }
    }
}
