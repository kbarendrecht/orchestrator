//! The triage run: read the threads, propose, exit.
//!
//! Modelled on [`crate::spawn::spawn_fix_pr_session`], and headed the same way —
//! a `claude` session you can watch and take over, not a `-p` run that happens
//! out of sight. The prompt is rendered from `commands/triage.md`, written to a
//! file, and the session is told to *read* that file. Not a `/triage <pr>` slash
//! command (that resolves from the agent's own command path, which depends on a
//! repo usually not installed) and not typed inline (it is multi-line, so it
//! would submit at the first newline).
//!
//! Following `fix-pr`'s lesson ([`crate::fix_pr::settle`]): **the agent's stdout
//! is not parsed.** The pty stream stays raw for xterm.js, and the run reports by
//! POSTing a [`crate::proposal::ProposalSet`] to the daemon. So "did it work" is
//! answered by looking for proposals, not by reading an exit code — an agent can
//! exit 0 having said nothing useful, and parsing its output would be a second,
//! worse source of truth.

use anyhow::{Context, Result};
use std::sync::Arc;
use uuid::Uuid;

use crate::config::Config;
use crate::model::*;
use crate::prompt;
use crate::pty::PtyHandle;
use crate::spawn::{ensure_pr_worktree, DEFAULT_SIZE};
use crate::state::AppState;

/// Why a triage run cannot start.
///
/// These are the worktree-readiness gates: the review flow writes into this
/// worktree, and every guarantee downstream — the check ladder, the complete file
/// list, "only what you approved" — assumes the tree starts clean. CI colour and
/// a merge conflict deliberately do **not** appear here; they are signals about a
/// future merge and never touch the branch-local machinery.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "gate", rename_all = "snake_case")]
pub enum Gate {
    /// Uncommitted work of your own would be swept into the batch's commit.
    Dirty { files: Vec<String> },
    /// A stopped rebase: the tree cannot take a patch at all.
    Rebasing,
    /// `fix-pr` is rewriting this same worktree. The two are exclusive.
    FixPrRunning,
}

impl Gate {
    /// One line, for the gate screen's heading.
    pub fn say(&self) -> String {
        match self {
            Gate::Dirty { files } => format!(
                "{} uncommitted file(s) in this worktree — commit or stash first",
                files.len()
            ),
            Gate::Rebasing => "a rebase is stopped part-way in this worktree".into(),
            Gate::FixPrRunning => "fix-pr is rewriting this branch".into(),
        }
    }
}

/// Whether the worktree is ready to be written into.
///
/// Checked before a triage starts *and* again immediately before the batch
/// writes: a review can sit open for hours, and the tree can go dirty, a rebase
/// can stop, or `fix-pr` can start in between.
pub async fn gate(app: &Arc<AppState>, pr: u64, workspace: &str) -> Result<Option<Gate>> {
    gate_inner(app, pr, workspace, true).await
}

/// The same gates minus the clean-tree one.
///
/// For the manual phase's second half only, where the tree is dirty **because you
/// were asked to edit it**. `Rebasing` and `FixPrRunning` still hold: one cannot
/// take a commit at all, and the other is rewriting the same history.
pub async fn gate_allowing_your_edits(
    app: &Arc<AppState>,
    pr: u64,
    workspace: &str,
) -> Result<Option<Gate>> {
    gate_inner(app, pr, workspace, false).await
}

async fn gate_inner(
    app: &Arc<AppState>,
    pr: u64,
    workspace: &str,
    require_clean: bool,
) -> Result<Option<Gate>> {
    // Only a *running* fix-pr holds the worktree. An exhausted or finished one
    // has a record but has let go, so it must not gate.
    let fix_pr_running = matches!(
        app.inner.read().await.automation.get(pr),
        Some(crate::fix_pr::PrAutomation::Running { .. })
    );
    if fix_pr_running {
        return Ok(Some(Gate::FixPrRunning));
    }
    let Some(path) = app.workspace_path(workspace).await else {
        return Ok(None);
    };
    if crate::git::rebase_in_progress(&path) {
        return Ok(Some(Gate::Rebasing));
    }
    if require_clean && !crate::git::is_clean(&path)? {
        let set = crate::git::status(&path, None, crate::git::Untracked::Each)?;
        let mut files: Vec<String> = set
            .staged
            .iter()
            .chain(set.unstaged.iter())
            .chain(set.untracked.iter())
            .map(|f| f.path.clone())
            .collect();
        files.sort();
        files.dedup();
        return Ok(Some(Gate::Dirty { files }));
    }
    Ok(None)
}

/// Start a triage run pinned to the PR's head branch.
///
/// `login` is the viewer whose PR this is; it comes from the thread fetch that
/// preceded this, rather than a second `gh api user` call.
pub async fn spawn(app: &Arc<AppState>, pr: u64, head_ref: &str, login: &str) -> Result<SessionId> {
    let workspace = ensure_pr_worktree(app, pr, head_ref).await?;

    if let Some(g) = gate(app, pr, &workspace).await? {
        anyhow::bail!("{}", g.say());
    }

    let path = app
        .workspace_path(&workspace)
        .await
        .context("worktree vanished")?;

    let (owner, repo) =
        crate::resolve_repo(app).context("no GitHub repo configured and none on the remote")?;
    let body = prompt::render(
        prompt::TRIAGE,
        &prompt::Vars {
            pr,
            owner,
            repo,
            login: login.to_string(),
            upstream: app.cfg.upstream_ref.clone(),
            upstream_remote: app.cfg.upstream_remote.clone(),
            proposals_url: format!("http://127.0.0.1:{}/api/pr/{pr}/proposals", app.cfg.port),
            ask_base: format!("http://127.0.0.1:{}/api/session", app.cfg.port),
            // Whether the agent may offer `story+reply` at all: an option the
            // daemon would refuse should never reach a card.
            tracker: if app.cfg.tracker.is_configured() {
                prompt::TRACKER_ON.to_string()
            } else {
                prompt::TRACKER_OFF.to_string()
            },
            language: app.cfg.default_language.clone(),
            ..Default::default()
        },
    )?;

    let id = Uuid::new_v4();
    let settings = Config::hooks_settings_path()?;

    // Written to a file the session is told to read, not typed in: the prompt is
    // multi-line and typing it would submit at the first newline. Under the
    // daemon's own config dir, never inside the checkout, so the tree the review
    // flow inspects stays clean — the same reasoning as `vendored_prompt_file`.
    let dir = Config::config_dir()?.join(format!("triage-{pr}"));
    std::fs::create_dir_all(&dir)?;
    let prompt_file = dir.join("prompt.md");
    std::fs::write(&prompt_file, body)
        .with_context(|| format!("writing {}", prompt_file.display()))?;

    let cmd = vec![
        "claude".to_string(),
        "--session-id".to_string(),
        id.to_string(),
        "--settings".to_string(),
        settings.to_string_lossy().into_owned(),
    ];

    // The run POSTs its proposals back, so it needs a credential — but only for
    // that one route on this one PR. It used to be handed `app.token`, the whole
    // API, and this is the run whose input is other people's review comments.
    let post_token = crate::state::random_token();
    app.inner
        .write()
        .await
        .proposal_tokens
        .insert(pr, post_token.clone());

    let (env, unset) = run_env(&app.cfg, id, &post_token, None);

    let spawned = PtyHandle::spawn(&cmd, &path, &env, &unset, DEFAULT_SIZE)?;
    let mut session = Session::new(
        id,
        workspace,
        path,
        Kind::Automation {
            pr,
            command: "triage".to_string(),
        },
    );
    session.pty = Some(spawned.handle.clone());
    session.pid = spawned.pid;
    session.pending_prompt = Some(format!(
        "Read {} and follow it. Those are your instructions for PR {pr}.",
        prompt_file.display()
    ));
    {
        let mut inner = app.inner.write().await;
        // A fresh run supersedes whatever the last one proposed; keeping stale
        // proposals visible while a new run works would be worse than a gap.
        inner.proposals.remove(&pr);
        // And with them any batch that stopped for the manual phase: its decisions
        // point at positions that no longer exist, so finishing it is impossible and
        // offering to would be a screen whose button always fails. The local commit it
        // left behind is not silently lost — the next batch's own gate names it.
        if inner.with_manual("re-triage abandoned a phase", |m| m.remove(&pr).is_some()) {
            tracing::warn!(pr, "a manual phase was open; re-triaging abandons it");
        }
        inner.sessions.insert(id, session);
    }

    watch(app.clone(), pr, id, spawned.handle);
    app.notify().await;
    Ok(id)
}

/// The `Kind::Automation` command a review session carries.
///
/// Named for the same reason `fix_pr::COMMAND` is: the spawn, the handoff route and
/// the exit watcher all have to agree on this string, and three literals is how
/// they stop agreeing without anything failing.
pub const COMMAND: &str = "review";

/// Start the overlay review session pinned to the PR's head branch.
///
/// The single-session replacement for triage + the batch: it posts proposals like
/// triage does — filling the same overlay cards — but then stays alive, taking the
/// human's decisions over the ask channel and carrying out the change and the post
/// itself. So unlike [`spawn`] it needs `ORCH_ASK_TOKEN` in its environment, the
/// key the `/ask` and `/wait` routes check, and it is marked [`COMMAND`] so the
/// rail colours, the guards and the handoff tell it from a triage run.
pub async fn spawn_review(
    app: &Arc<AppState>,
    pr: u64,
    head_ref: &str,
    login: &str,
) -> Result<SessionId> {
    let workspace = ensure_pr_worktree(app, pr, head_ref).await?;

    if let Some(g) = gate(app, pr, &workspace).await? {
        anyhow::bail!("{}", g.say());
    }

    let path = app
        .workspace_path(&workspace)
        .await
        .context("worktree vanished")?;

    let (owner, repo) =
        crate::resolve_repo(app).context("no GitHub repo configured and none on the remote")?;
    let body = prompt::render(
        prompt::REVIEW_SESSION,
        &prompt::Vars {
            pr,
            owner,
            repo,
            login: login.to_string(),
            upstream: app.cfg.upstream_ref.clone(),
            upstream_remote: app.cfg.upstream_remote.clone(),
            proposals_url: format!("http://127.0.0.1:{}/api/pr/{pr}/proposals", app.cfg.port),
            ask_base: format!("http://127.0.0.1:{}/api/session", app.cfg.port),
            tracker: if app.cfg.tracker.is_configured() {
                prompt::TRACKER_ON.to_string()
            } else {
                prompt::TRACKER_OFF.to_string()
            },
            language: app.cfg.default_language.clone(),
            ..Default::default()
        },
    )?;

    let id = Uuid::new_v4();
    let settings = Config::hooks_settings_path()?;

    let dir = Config::config_dir()?.join(format!("review-{pr}"));
    std::fs::create_dir_all(&dir)?;
    let prompt_file = dir.join("prompt.md");
    std::fs::write(&prompt_file, body)
        .with_context(|| format!("writing {}", prompt_file.display()))?;

    let cmd = vec![
        "claude".to_string(),
        "--session-id".to_string(),
        id.to_string(),
        "--settings".to_string(),
        settings.to_string_lossy().into_owned(),
    ];

    // Minted here so the same value goes into the environment and onto the record:
    // the agent reads it from `ORCH_ASK_TOKEN`, and `/ask`/`/wait` check it against
    // `session.ask_token`. `Session::new` sets its own, overwritten below.
    let ask_token = crate::state::random_token();
    // Two narrow credentials, no broad one: asks are authenticated against this
    // session, proposals against this PR. Neither opens anything else, which is
    // what keeps "the daemon owns outward writes" an API rule rather than a
    // sentence in a prompt this run's own input could argue with.
    let post_token = crate::state::random_token();
    app.inner
        .write()
        .await
        .proposal_tokens
        .insert(pr, post_token.clone());

    let (env, unset) = run_env(&app.cfg, id, &post_token, Some(&ask_token));

    let spawned = PtyHandle::spawn(&cmd, &path, &env, &unset, DEFAULT_SIZE)?;
    let mut session = Session::new(
        id,
        workspace,
        path,
        Kind::Automation {
            pr,
            command: COMMAND.to_string(),
        },
    );
    session.pty = Some(spawned.handle.clone());
    session.pid = spawned.pid;
    session.ask_token = ask_token;
    session.pending_prompt = Some(format!(
        "Read {} and follow it. Those are your instructions for PR {pr}.",
        prompt_file.display()
    ));
    {
        let mut inner = app.inner.write().await;
        // A fresh session supersedes whatever the last one proposed, and any batch
        // that stopped for the manual phase — its decisions point at positions that
        // no longer exist. Same reasoning as `spawn`.
        inner.proposals.remove(&pr);
        if inner.with_manual("re-review abandoned a phase", |m| m.remove(&pr).is_some()) {
            tracing::warn!(pr, "a manual phase was open; re-reviewing abandons it");
        }
        inner.sessions.insert(id, session);
    }

    watch(app.clone(), pr, id, spawned.handle);
    app.notify().await;
    Ok(id)
}

/// The environment a proposals-posting run is given, and nothing more.
///
/// One function for both runs so the two cannot drift — and extracted at all
/// because this is the seam that decides *which* credential a run holds, and it
/// used to be `app.token`: the whole API, handed to the pass whose input is other
/// people's review comments.
///
/// Both credentials go in the environment rather than the prompt, because prompt
/// text lands in a transcript and a pty buffer. `ask` is `None` for the headless
/// triage pass, which has nobody to ask.
///
/// Testable on purpose. Verifying it live needs a real triage run against a PR
/// with unanswered threads, and the fixture that provides one depends on GitHub
/// Actions — so when Actions is unavailable, this is the only thing standing
/// between a two-line plumbing slip and a review flow that cannot post.
fn run_env(
    cfg: &crate::config::Config,
    id: SessionId,
    post_token: &str,
    ask: Option<&str>,
) -> (Vec<(String, String)>, Vec<&'static str>) {
    let (mut env, unset) = crate::config::session_env(cfg, id, ask);
    env.push(("ORCH_POST_TOKEN".to_string(), post_token.to_string()));
    (env, unset)
}

/// Notice when a run ends without having proposed anything.
///
/// Success is "proposals arrived", not "exited zero" — an agent can finish
/// cleanly having posted nothing, and that is the failure the user would
/// otherwise stare at an empty overlay wondering about.
fn watch(app: Arc<AppState>, pr: u64, id: SessionId, handle: Arc<PtyHandle>) {
    tokio::spawn(async move {
        handle.wait().await;
        let posted = app.inner.read().await.proposals.contains_key(&pr);
        if !posted {
            tracing::warn!(pr, session = %id, "triage exited without posting proposals");
        }
        app.notify().await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The credential a run is handed, pinned. This is the one thing about seam 2
    /// a unit test can see: the route guard and the token check are driven over
    /// real HTTP in `api::tests`, but *which* value reaches the agent is decided
    /// here, and the prompts read it by name.
    #[test]
    fn a_run_is_given_the_narrow_token_and_never_the_app_token() {
        let id = uuid::Uuid::new_v4();
        let (env, _) = run_env(&crate::config::test_config(), id, "post-tok", Some("ask-tok"));
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };

        assert_eq!(get("ORCH_POST_TOKEN").as_deref(), Some("post-tok"));
        assert_eq!(get("ORCH_ASK_TOKEN").as_deref(), Some("ask-tok"));
        assert_eq!(get("ORCH_SESSION_ID").as_deref(), Some(id.to_string().as_str()));

        // The regression this exists for. `ORCHD_TOKEN` here means the run holds
        // the whole API again — teardown, spawn, file writes — to do a job that is
        // one POST, while reading text a stranger wrote on a pull request.
        assert!(
            get("ORCHD_TOKEN").is_none(),
            "the app token must never reach a run that reads third-party comments"
        );
        // And the name the prompts actually curl with. `commands/triage.md` and
        // `review-session.md` send `$ORCH_POST_TOKEN`; a rename here that missed
        // them would fail only at the POST, in a run that had already done its work.
        assert!(crate::prompt::TRIAGE.contains("$ORCH_POST_TOKEN"));
        assert!(crate::prompt::REVIEW_SESSION.contains("$ORCH_POST_TOKEN"));

        // The headless triage pass has nobody to ask, so it gets no ask token.
        let (solo, _) = run_env(&crate::config::test_config(), id, "post-tok", None);
        assert!(!solo.iter().any(|(n, _)| n == "ORCH_ASK_TOKEN"));
        assert!(solo.iter().any(|(n, _)| n == "ORCH_POST_TOKEN"));
    }

    #[test]
    fn a_gate_says_what_is_wrong_in_one_line() {
        assert!(Gate::Rebasing.say().contains("rebase"));
        assert!(Gate::FixPrRunning.say().contains("fix-pr"));
        let d = Gate::Dirty {
            files: vec!["a.rs".into(), "b.rs".into()],
        };
        // The count is what you act on; the list is shown separately.
        assert!(d.say().contains('2'), "{}", d.say());
        assert!(d.say().contains("commit or stash"));
    }

    #[test]
    fn gates_serialize_with_a_tag_the_spa_can_switch_on() {
        let j = serde_json::to_string(&Gate::Dirty {
            files: vec!["a.rs".into()],
        })
        .unwrap();
        assert!(j.contains(r#""gate":"dirty""#), "{j}");
        let j = serde_json::to_string(&Gate::FixPrRunning).unwrap();
        assert!(j.contains(r#""gate":"fix_pr_running""#), "{j}");
    }
}
