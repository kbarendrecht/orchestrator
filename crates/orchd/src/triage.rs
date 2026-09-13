//! The triage run: read the threads, propose, exit.
//!
//! One of the four runs [`crate::spawn::spawn_run`] starts, beside `fix-pr` and
//! the resolve run, and shaped the same way — a `claude` session you can watch
//! and take over, not a `-p` run that happens out of sight. Its first turn is the
//! vendored skill, typed as `/orchd:triage <pr>`. A slash
//! command used to be the wrong shape here because it resolved from the agent's own
//! command path, which depends on a repo usually not installed — the daemon now
//! ships the skill itself and pushes `--plugin-dir` on every spawn
//! ([`crate::launch::session_flags`]), so the path is one it owns. The runs that
//! still have a multi-line prompt carry it another way: every vendored prompt is
//! a skill now, and what a template used to interpolate arrives by route or by
//! environment ([`crate::skills`]).
//!
//! Following `fix-pr`'s lesson ([`crate::fix_pr::settle`]): **the agent's stdout
//! is not parsed.** The pty stream stays raw for xterm.js, and the run reports by
//! POSTing a [`crate::proposal::ProposalSet`] to the daemon. So "did it work" is
//! answered by looking for proposals, not by reading an exit code — an agent can
//! exit 0 having said nothing useful, and parsing its output would be a second,
//! worse source of truth.

use anyhow::{bail, Result};
use std::sync::Arc;

use crate::model::*;
use crate::spawn::ensure_pr_worktree;
use crate::state::AppState;

/// Why a triage run cannot start.
///
/// These are the worktree-readiness gates: the review flow writes into this
/// worktree, and every guarantee downstream — the check ladder, the complete file
/// list, "only what you approved" — assumes the tree starts clean. CI colour and
/// a merge conflict deliberately do **not** appear here; they are signals about a
/// future merge and never touch the branch-local machinery.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "snapshot.d.ts")
)]
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
        Some(crate::model::PrAutomation::Running { .. })
    );
    if fix_pr_running {
        return Ok(Some(Gate::FixPrRunning));
    }
    let Some(path) = app.workspace_path(workspace).await else {
        return Ok(None);
    };
    // Both of these read the worktree from disk, and `dirty_paths` is a full
    // `git status` — on the monorepo that is a whole-tree scan per worktree, since
    // `configure_repo` sets fsmonitor on main only. One `spawn_blocking` for the
    // pair rather than two, because they are asked together.
    let at = path.clone();
    let gate = crate::proc::run_blocking("the triage gate's git checks", move || {
        if crate::git::rebase_in_progress(&at) {
            return Ok(Some(Gate::Rebasing));
        }
        if require_clean {
            // One `git status` answers both questions, and it is the same list the
            // manual phase's writer refuses on, so the gate never names a different
            // set than the write does.
            let files = crate::patch::dirty_paths(&at)?;
            if !files.is_empty() {
                return Ok(Some(Gate::Dirty { files }));
            }
        }
        Ok(None)
    })
    .await?;
    gate
}

/// Start the overlay review session pinned to the PR's head branch.
///
/// One session answers the whole PR: it posts proposals into the overlay's cards,
/// then stays alive, takes the human's decisions over the ask channel and carries
/// out the change and the post itself. So it needs `ORCH_ASK_TOKEN` in its
/// environment, the key the `/ask` and `/wait` routes check, and it is marked
/// [`Pass::REVIEW`] so the rail colours, the guards and the handoff know it.
///
/// **It used to have a sibling.** A headless `triage` pass posted the same
/// proposals and ended there, and a second run carried out what the cards decided.
/// `RunKind` existed to tell the two apart; with one left, `asks` is always true
/// and the command is always `REVIEW`.
pub async fn spawn_review(app: &Arc<AppState>, pr: u64, head_ref: &str) -> Result<SessionId> {
    let kind = RunKind {
        command: Pass::REVIEW,
        asks: true,
    };
    spawn_posting_run(app, pr, head_ref, kind).await
}

/// What tells a triage run from a review session. Everything else about the two
/// spawns is the same, and was written twice until the copies drifted.
struct RunKind {
    /// The `Pass` command the session carries, and the prefix of its
    /// scratch dir under the config dir (`<command>-<pr>`). One field, because it
    /// was two that were always the same string, in the struct whose whole purpose
    /// is to stop two copies drifting.
    command: &'static str,
    /// Whether the run takes decisions over the ask channel, and so needs
    /// `ORCH_ASK_TOKEN` in its environment.
    asks: bool,
}

async fn spawn_posting_run(
    app: &Arc<AppState>,
    pr: u64,
    head_ref: &str,
    kind: RunKind,
) -> Result<SessionId> {
    let workspace = ensure_pr_worktree(app, pr, head_ref).await?;

    if let Some(g) = gate(app, pr, &workspace).await? {
        anyhow::bail!("{}", g.say());
    }

    /* Both posting runs are skills now, so the first turn is one typed line and
    `FirstTurn` went with the last rendered prompt: what a template substituted,
    `/api/pr/:n/triage-context` answers. The command string is the skill's
    directory name — `skills::a_skill_is_named_after_the_command_that_types_it`
    is what keeps those two spellings together. */
    let pending = format!("/orchd:{} {pr}", kind.command);

    // The post token is minted by `spawn_run`, because both posting runs spawn
    // there and `posts_proposals` names them; the ask token only when the run has
    // somebody to ask.
    let spec = crate::spawn::RunSpec {
        command: kind.command.to_string(),
        pending,
        asks: kind.asks,
        extra_env: Vec::new(),
    };

    // Cleared *before* the spawn, not after. This is the run's own bookkeeping in
    // the sense `spawn::spawn_run` describes: a `claude` that dies at once is
    // reaped first, and the exit watcher's "exited without posting proposals"
    // warning then reads the *previous* run's proposals, finds them, and stays
    // silent about exactly the failure it was written for.
    //
    // Nothing to undo if the spawn fails. A fresh run supersedes whatever the last
    // one proposed, and a run that could not start has superseded it just as much:
    // keeping stale proposals on screen because the new run failed would be the
    // worse of the two gaps.
    {
        let mut inner = app.inner.write().await;
        inner.proposals.remove(&pr);
    }

    let id = crate::spawn::spawn_run(app, &workspace, pr, uuid::Uuid::new_v4(), spec).await?;
    app.notify().await;
    Ok(id)
}

/// Spawn an interactive session pinned to a PR's head branch, and type a slash
/// command into it once it is ready.
///
/// The default answer to the rail's review button, again: a `claude` session in the
/// PR worktree running `/orchd:handle-review <pr>` in the pane, the agent doing the
/// reading, fixing, pushing and posting itself while you supervise. The daemon does
/// no irreversible writes here — the agent does, in a shell you can take over.
///
/// **"Again" because this had no caller for a while.** The button went to the
/// triage-into-cards flow, and the docblock went on claiming the pane was the
/// default while the only path in was a test — with a prompt lookup that could not
/// have answered anyway. The overlay is the opt-in alternative once more, for the
/// reason it was written down as the robust path in the first place: the cards are
/// not good enough to be the only way through a review yet, and a review you can
/// only finish by learning a new screen is a worse default than one that hands you
/// a terminal.
pub async fn spawn_command_session(
    app: &Arc<AppState>,
    pr: u64,
    head_ref: &str,
    command: &str,
) -> Result<SessionId> {
    // If the branch already has a worktree with a live session, take you there
    // rather than spawning a second one (§8).
    if let Some(ws) = app.worktree_holding(head_ref).await {
        let live = app.live_sessions_in(&ws).await;
        if let Some(id) = live.first() {
            return Ok(*id);
        }
        /* **The same worktree gates as the other review verb**, because the pass
        writes into that tree: a rebase stopped part-way cannot take a commit, a
        running `fix-pr` is rewriting the same history, and a dirty tree means
        the first thing this agent amends is work somebody else left there.

        Here rather than at the route, and after the live-session branch above
        for the reason that branch exists: landing on the pane already doing this
        is not a refusal case. The route used to re-derive both reads to decide
        the same thing, which is two spellings of "is anyone on this branch" —
        the pair `branch_busy` was written to be the only definition of. */
        if let Some(g) = gate(app, pr, &ws).await? {
            bail!("{}", g.say());
        }
        return start_with_prompt(app, &ws, pr, command).await;
    }

    // Otherwise pin a worktree to that branch. `git worktree add` directly,
    // because the WorktreeCreate hook always cuts a new branch from
    // upstream/develop; `worktree-link` still runs at SessionStart.
    //
    // No name-reuse refusal here, deliberately, and it was removed rather than
    // never written. It refused whenever an archived session's recovery record
    // named `pr-<n>` — which teardown writes — so reviewing a PR whose worktree you
    // had torn down was refused for good, with advice ("rename") that cannot be
    // followed for a name the daemon derives from the PR number. `fix-pr` and
    // `triage` never had the check and were unaffected, so one PR answered two ways.
    //
    // What it claimed to prevent does not happen (transcripts are keyed by session
    // uuid; `spawn_worktree_session` has the whole account), and the real hazard,
    // a resume into a tree cut again at the same path, is `worktree::branch_drift`'s.
    let name = ensure_pr_worktree(app, pr, head_ref).await?;
    start_with_prompt(app, &name, pr, command).await
}

async fn start_with_prompt(
    app: &Arc<AppState>,
    workspace: &str,
    pr: u64,
    command: &str,
) -> Result<SessionId> {
    /* A vendored skill, typed. This rendered a *prompt* until the conversion, and
    the lookup had no arm for the command it was called with — so the only path
    into here could only ever bail, which is why it had no caller but a test.
    Namespaced (`/orchd:<command>`), because what it types now is one of this
    daemon's own skills rather than whatever the repo happens to define. */
    let spec = crate::spawn::RunSpec {
        command: command.to_string(),
        pending: format!("/orchd:{command} {pr}"),
        asks: true,
        extra_env: vec![
            (crate::skills::VAR_PR.to_string(), pr.to_string()),
            // The language a reply is written in when the thread does not settle
            // it. Config, so the skill cannot carry it.
            (
                crate::skills::VAR_LANGUAGE.to_string(),
                app.cfg.default_language.clone(),
            ),
        ],
    };
    // Its own id: this is the `/resolve` pane, the one run with no record of the
    // daemon's beside it, so there is nothing for a caller to write first.
    crate::spawn::spawn_run(app, workspace, pr, uuid::Uuid::new_v4(), spec).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The credential a run is handed, pinned. This is the one thing about seam 2
    /// a unit test can see: the route guard and the token check are driven over
    /// real HTTP in `api::tests`, but *which* value reaches the agent is decided
    /// by `spawn::run_env`, and the prompts read it by name. Kept here rather than
    /// beside that function because the prompts it checks are this module's.
    #[test]
    fn a_run_is_given_the_narrow_token_and_never_the_app_token() {
        let id = uuid::Uuid::new_v4();
        // `env_source: None`: this asserts what the *daemon* hands a run, and a
        // real source would make the answer depend on the machine it runs on.
        let cfg = crate::config::Config {
            env_source: crate::config::EnvSourceKind::None,
            ..crate::testutil::test_config()
        };
        let dir = std::env::temp_dir();
        let run_env = |post: Option<&str>, ask: Option<&str>| {
            crate::spawn::run_env(&cfg, &dir, id, ask, post, &[])
        };
        let (env, _) = run_env(Some("post-tok"), Some("ask-tok"));
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

        assert_eq!(get("ORCH_POST_TOKEN").as_deref(), Some("post-tok"));
        assert_eq!(get("ORCH_ASK_TOKEN").as_deref(), Some("ask-tok"));
        assert_eq!(
            get("ORCH_SESSION_ID").as_deref(),
            Some(id.to_string().as_str())
        );

        // The regression this exists for. `ORCHD_TOKEN` here means the run holds
        // the whole API again — teardown, spawn, file writes — to do a job that is
        // one POST, while reading text a stranger wrote on a pull request.
        assert!(
            get("ORCHD_TOKEN").is_none(),
            "the app token must never reach a run that reads third-party comments"
        );
        // And the name the run actually curls with; a rename here would fail only
        // at the POST, in a run that had already done its work.
        assert!(crate::skills::REVIEW.contains("$ORCH_POST_TOKEN"));

        // And a run that posts nothing is handed no post token at all.
        let (fix, _) = run_env(None, None);
        assert!(!fix.iter().any(|(n, _)| n == "ORCH_POST_TOKEN"));
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

    /// Reviewing a PR whose worktree you tore down must not be refused on the name.
    ///
    /// Teardown writes a recovery record naming `pr-<n>`, and the old check refused
    /// on exactly that — for good, since "rename" is impossible for a name derived
    /// from the PR number, leaving deleting the conversation as the only way out.
    /// `fix-pr` and `triage` never had the check, so one PR answered two ways.
    ///
    /// Asserted as "not *this* refusal" rather than success: reaching a real spawn
    /// would need a repo, a branch and `claude`. The failure here is the missing
    /// branch, which is the next thing the path legitimately trips on.
    #[tokio::test]
    async fn reviewing_a_pr_is_not_refused_because_its_worktree_was_torn_down() {
        let dir = crate::testutil::scratch("reuse");
        let cfg = crate::config::Config::parse(&format!(
            r#"{{"main_checkout":{:?}}}"#,
            dir.to_string_lossy()
        ))
        .expect("parse");
        let app = crate::state::AppState::new(cfg, "t".into(), crate::window::Chrome::None);
        {
            let mut inner = app.inner.write().await;
            let id = uuid::Uuid::new_v4();
            let mut s = Session::new(id, "pr-4".into(), dir.join("pr-4"), None);
            s.had_a_turn = true;
            s.set_state(State::Archived { resumable: true });
            // Exactly what `worktree::archive` writes when a pr-4 tree is torn down.
            s.recovery = Some(ArchiveState::Recoverable {
                name: "pr-4".into(),
                branch: "feature/x".into(),
                head_sha: "abc1234".into(),
            });
            inner.sessions.insert(id, s);
        }
        let err = format!(
            "{:#}",
            spawn_command_session(&app, 4, "feature/x", "resolve")
                .await
                .expect_err("no repo here, so it cannot get as far as a session")
        );
        assert!(
            !err.contains("already used") && !err.contains("interleave"),
            "refused on the reused name again: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
