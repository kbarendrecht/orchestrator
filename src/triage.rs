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

    let (mut env, unset) = crate::config::transcript_env();
    env.push(("ORCH_SESSION_ID".to_string(), id.to_string()));
    // The run POSTs its proposals back, so it needs the API token — in the
    // environment, never in the prompt text, which lands in transcripts.
    env.push(("ORCHD_TOKEN".to_string(), app.token.clone()));

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
