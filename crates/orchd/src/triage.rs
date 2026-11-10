//! The review run: `handle-review` in a pane, and the gates a PR's worktree must
//! pass before anything writes into it.
//!
//! Started by [`crate::spawn::spawn_run`], beside `fix-pr`, and shaped the same
//! way — a session you can watch and take over, not a `-p` run that happens out of
//! sight. Its first turn is the vendored skill, typed as
//! `/orchd:handle-review <pr>`. The daemon ships the skill itself and pushes
//! `--plugin-dir` on every spawn ([`crate::launch::session_flags`]), so the path
//! is one it owns; what a template used to interpolate arrives by environment
//! ([`crate::skills`]).
//!
//! **There used to be a second review run here**, the overlay's `review` session,
//! which posted proposals onto cards and carried out what you picked. It is gone:
//! one review verb, in a terminal, is what the product is.

use anyhow::{bail, Result};
use std::sync::Arc;

use crate::locate::{locate_pr, Located, PrPlace};
use crate::model::*;
use crate::spawn::ensure_pr_worktree_at;
use crate::state::AppState;

/// Why a review run cannot start.
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
    /// Uncommitted work of your own would be swept into the session's commit.
    Dirty { files: Vec<String> },
    /// A stopped rebase: the tree cannot take a patch at all.
    Rebasing,
    /// `fix-pr` is rewriting this same worktree. The two are exclusive.
    FixPrRunning,
}

impl Gate {
    /// The refusal for this gate: the sentence, and for a dirty tree the gate
    /// itself, so the page can offer the two ways out ([`settle_dirty`]).
    pub fn refusal(self) -> anyhow::Error {
        match &self {
            Gate::Dirty { .. } => anyhow::Error::new(crate::model::Settleable {
                message: self.say(),
                gate: serde_json::to_value(&self).unwrap_or_default(),
            }),
            _ => anyhow::anyhow!("{}", self.say()),
        }
    }

    /// One line, for the refusal the spawn toasts.
    pub fn say(&self) -> String {
        match self {
            Gate::Dirty { files } => format!(
                "{} uncommitted file(s) in the tree on this branch — commit or stash first",
                files.len()
            ),
            Gate::Rebasing => "a rebase is stopped part-way in the tree on this branch".into(),
            Gate::FixPrRunning => "fix-pr is rewriting this branch".into(),
        }
    }
}

/// Whether the PR's tree is ready to be written into.
///
/// Checked once, where the run is spawned, and **before anything changes the
/// tree** — on every path that writes into it: the review pass here and
/// `fix_pr::start`. It used to run after `ensure_pr_worktree`, and that call is not
/// read-only: when main holds the PR's branch it moves the branch *and main's
/// uncommitted work* into a new worktree, so the gate then inspected a tree the
/// flow had just made and found it clean, with somebody's work carried along
/// under a run that amends and force-pushes. `fix_pr::start` never gated at all,
/// and the pane flow skipped it whenever the tree had to be cut.
///
/// **It used to be checked twice and shown as a screen**, with `commit…` and
/// `stash` buttons in front of a review. That went with the overlay; the spawn
/// refuses with [`Gate::say`] in the toast.
pub async fn gate(app: &Arc<AppState>, pr: u64, located: &Located) -> Result<Option<Gate>> {
    // Only a *running* fix-pr holds the tree. A finished one has let go, so it
    // must not gate.
    let fix_pr_running = matches!(
        app.inner.read().await.automation.get(pr),
        Some(crate::model::PrAutomation::Running { .. })
    );
    if fix_pr_running {
        return Ok(Some(Gate::FixPrRunning));
    }
    tree_gate(located).await
}

/// [`gate`] without the fix-pr half, for the fix run itself: its own guard table
/// refuses a second run, and by the time it asks this its record is not written
/// yet, so the two halves would only ever disagree about nothing.
///
/// The tree is the worktree [`crate::locate::locate_pr`] names, when it is still
/// on disk: the one the flow would reuse. Main is not gated here, because a flow
/// that finds main on the branch carries its work out with it by design, and
/// someone else's branch is refused by name where its tree would be cut.
pub async fn tree_gate(located: &Located) -> Result<Option<Gate>> {
    let at = match &located.place {
        PrPlace::Worktree { path, .. } if path.is_dir() => path.clone(),
        _ => return Ok(None),
    };
    // Both of these read the tree from disk, and `dirty_paths` is a full
    // `git status` — on the monorepo that is a whole-tree scan per worktree, since
    // `configure_repo` sets fsmonitor on main only. One `spawn_blocking` for the
    // pair rather than two, because they are asked together.
    let gate = crate::proc::run_blocking("the triage gate's git checks", move || {
        if crate::git::rebase_in_progress(&at) {
            return Ok(Some(Gate::Rebasing));
        }
        let files = crate::patch::dirty_paths(&at)?;
        if !files.is_empty() {
            return Ok(Some(Gate::Dirty { files }));
        }
        Ok(None)
    })
    .await?;
    gate
}

/// What a run route takes: how to settle a dirty tree before it starts, when the
/// person picked one.
#[derive(Debug, Default, serde::Deserialize)]
pub struct RunBody {
    #[serde(default)]
    pub dirty: Option<Dirty>,
}

/// The two ways out of [`Gate::Dirty`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dirty {
    /// Set the files aside on the workspace's bank, where the changes pane puts
    /// them back or drops them. New files too: the bank keeps tracked changes
    /// only, so they are staged first, or they stay behind and the gate refuses
    /// again.
    Bank,
    /// Keep them in the branch, as a commit of their own.
    Commit,
}

/// The message of a [`Dirty::Commit`]: plain about what it is, since it lands
/// on a PR branch where a reviewer reads it.
const KEPT_MESSAGE: &str = "work in progress, kept before a run on this PR";

/// Settle a dirty PR tree the way the person picked, so the run's gate passes.
///
/// Refused while a session is working in that tree (the files may be its, mid
/// edit), and banking is refused over an earlier bank, which `bank_wip` would
/// overwrite and lose. Nothing to do when no worktree holds the branch.
pub async fn settle_dirty(app: &Arc<AppState>, pr: u64, head_ref: &str, how: Dirty) -> Result<()> {
    let located = locate_pr(app, pr, head_ref).await?;
    let (ws, path) = match &located.place {
        PrPlace::Worktree { ws, path } if path.is_dir() => (ws.clone(), path.clone()),
        _ => return Ok(()),
    };
    // Clean already (settled in another window, or by the agent): nothing to do,
    // and no refusal about a bank that this would not touch.
    let p = path.clone();
    let dirty =
        crate::proc::run_blocking("reading the tree", move || crate::patch::dirty_paths(&p))
            .await??;
    if dirty.is_empty() {
        return Ok(());
    }
    if let Some(who) = app.busy_session_in(&ws).await {
        bail!("{who} is working in that tree, so its files may be mid-edit; let it finish first");
    }
    match how {
        Dirty::Bank => {
            if app.workspace_banked(&ws).await.is_some() {
                bail!(
                    "that tree already has work set aside; put it back or drop it in its changes pane first"
                );
            }
            let (p, w) = (path.clone(), ws.clone());
            let bank = crate::proc::run_blocking("setting the tree's work aside", move || {
                crate::git::git(&p, &["add", "-A"])?;
                crate::git::bank_wip(&p, &w)
            })
            .await??;
            app.set_banked(&ws, bank).await;
        }
        Dirty::Commit => {
            let p = path.clone();
            crate::proc::run_blocking("committing the tree's work", move || {
                crate::git::git(&p, &["add", "-A"])?;
                crate::git::git(&p, &["commit", "-q", "-m", KEPT_MESSAGE])?;
                anyhow::Ok(())
            })
            .await??;
        }
    }
    app.notify().await;
    Ok(())
}

/// Refuse a PR whose branch you may not rewrite, before anything is spawned or cut.
///
/// The review pass amends into the commits that own each change and pushes
/// `--force-with-lease`, the same writes `fix-pr` makes, so it asks the same
/// question it does — `Pr::push_refusal` — rather than leaving each skill to
/// compare the head repo's owner with your login, which refused every org PR. The
/// skill is then handed the answer (`ORCH_MAY_PUSH`) instead of working it out
/// again.
async fn may_push(app: &Arc<AppState>, pr: u64) -> Result<()> {
    let inner = app.inner.read().await;
    let Some(p) = inner.pr(pr) else {
        bail!("PR #{pr} is not in the current poll");
    };
    match p.push_refusal(inner.viewer.as_deref().unwrap_or_default()) {
        Some(why) => bail!("{why}"),
        None => Ok(()),
    }
}

/// Hand `/orchd:<command> <pr>` to the live session on `head_ref`'s tree, if there
/// is one, and say which session that was.
///
/// **Main included**, unlike [`AppState::worktree_holding`]. That one places a run
/// the daemon *starts*, and a run started in main would rebase and force-push the
/// tree everything is cut from. This starts nothing: it types a turn into a session
/// you opened there, through "open in main" — and leaving main out made that PR's
/// `handle` and `fix` refuse with "close that session", the one PR you had open.
///
/// `None` is "nobody is there", and the caller starts its own run. Shared by the
/// review pass and by `fix-pr`, because a button pressed on a PR whose branch
/// already has a session open means the same thing for both: ask that session.
pub(crate) async fn hand_to_live_session(
    app: &Arc<AppState>,
    located: &Located,
    pr: u64,
    head_ref: &str,
    command: &str,
) -> Result<Option<SessionId>> {
    // Someone else's branch of this name is nobody working on this PR, and typing
    // a review of it at that agent would be worse than no answer: "nobody is
    // there" sends the caller to the cut, which says why. Main is answered by git,
    // not the cache — `locate` has the two races that cost.
    let ws = match &located.place {
        PrPlace::Worktree { ws, .. } => ws.clone(),
        PrPlace::Main => MAIN.to_string(),
        PrPlace::Foreign(_) | PrPlace::Nowhere => return Ok(None),
    };
    /* **The session the PR row names, when it is here.** The row's chip and its
    buttons point at `Inner::session_on` — the newest live session on the branch —
    and this took the oldest in the tree. With sharing on in main they differ, and
    the press typed into a session the row never offered, often one mid-turn. A
    session still being spawned has no pty to type into, so one that has a process
    is preferred over it. */
    let live = app.live_sessions_in(&ws).await;
    let named = app.inner.read().await.session_on(head_ref);
    let with_process = {
        let inner = app.inner.read().await;
        live.iter()
            .copied()
            .find(|id| inner.sessions.get(id).is_some_and(|s| s.pid.is_some()))
    };
    let Some(id) = named
        .filter(|n| live.contains(n))
        .or(with_process)
        .or_else(|| live.first().copied())
    else {
        return Ok(None);
    };
    /* **Landing on it is half the press, and it used to be all of it.** This
    returned the id alone: the rail selected the session you already had,
    the toast said "handling #<n>", and nothing whatever had been asked of
    that agent. A button that navigates and then claims to have worked is
    worse than one that refuses, because there is nothing to notice.

    So the command is typed at it, as the ordinary user turn it is — you
    pressed a button, and the transcript should read as though you asked.
    `type_user_turn` carries the three refusals with it (mid-turn, a
    permission prompt, an open question), which is exactly right here: each
    is a keystroke that would mean something other than a prompt. */
    let (run, agent) = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .ok_or_else(|| crate::state::no_such_session(id))?;
        (s.pass.as_ref().map(|p| p.command.clone()), s.agent)
    };
    match run.as_deref() {
        // Already doing this, so the press is "show me" and nothing else.
        // Typing it again would queue a second pass over the same threads.
        Some(c) if c == command => Ok(Some(id)),
        /* Another run owns the tree. `gate` refuses a live `fix-pr` for
        this reason and never sees it, because the branch above wins first —
        and typing a slash command into an unattended run that is rewriting
        this history is how you derail it. Refused by name, like the gate. */
        Some(c) => {
            bail!("{}", Pass::holds_tree(c, &ws))
        }
        // An ordinary conversation, which is the case the button is for.
        None => {
            app.type_user_turn(id, &agent.skill_line(command, &pr.to_string()))
                .await?;
            /* Working from the moment it is typed, not from when Claude Code's
            prompt hook says so. A double press otherwise typed the command twice:
            the button re-enables in milliseconds, and the session was still at
            its prompt as far as `type_user_turn` could tell. Now the second press
            is refused as mid-turn, and `UserPromptSubmit` agrees a moment later. */
            if let Some(s) = app.inner.write().await.sessions.get_mut(&id) {
                s.set_state(crate::model::State::Working);
            }
            app.notify().await;
            Ok(Some(id))
        }
    }
}

/// Spawn an interactive session pinned to a PR's head branch, and type a slash
/// command into it once it is ready.
///
/// The answer to the rail's review button: a session in the PR worktree running
/// `/orchd:handle-review <pr>` in the pane, the agent doing the reading, fixing,
/// pushing and posting itself while you supervise. The daemon does no
/// irreversible writes here — the agent does, in a shell you can take over.
pub async fn spawn_command_session(
    app: &Arc<AppState>,
    pr: u64,
    head_ref: &str,
    command: &str,
) -> Result<SessionId> {
    // First, before a session is asked or a tree is cut: the pass amends and
    // force-pushes, so a branch you may not push to is refused by name here rather
    // than by the agent half-way through.
    may_push(app, pr).await?;
    // If the branch already has a tree with a live session, take you there rather
    // than spawning a second one (§8). Someone else's branch of the same name is
    // neither handed to nor gated on — both start from `locate_pr` — and
    // `ensure_pr_worktree` below refuses it and says why.
    // Located once for the whole press, so the hand-off, the gate and the cut all
    // act on the same answer rather than three reads of a branch that may move.
    let located = locate_pr(app, pr, head_ref).await?;
    if let Some(id) = hand_to_live_session(app, &located, pr, head_ref, command).await? {
        return Ok(id);
    }
    /* **The same gates as `fix-pr`**, because the pass writes into that tree: a rebase stopped part-way cannot take a commit, a running `fix-pr`
    is rewriting the same history, and a dirty tree means the first thing this
    agent amends is work somebody else left there.

    After the live-session branch above for the reason that branch exists: landing
    on the pane already doing this is not a refusal case. And before
    `ensure_pr_worktree`, so the tree it reuses is asked about before anything
    is written into it rather than after. */
    if let Some(g) = gate(app, pr, &located).await? {
        return Err(g.refusal());
    }

    // Reuse the worktree holding the branch, or pin one to it. `git worktree add`
    // directly, because the WorktreeCreate hook always cuts a new branch from
    // upstream/develop; `worktree-link` still runs at SessionStart.
    //
    // No name-reuse refusal here, deliberately, and it was removed rather than
    // never written. It refused whenever an archived session's recovery record
    // named `pr-<n>` — which teardown writes — so reviewing a PR whose worktree you
    // had torn down was refused for good, with advice ("rename") that cannot be
    // followed for a name the daemon derives from the PR number. `fix-pr` never had
    // the check and was unaffected, so one PR answered two ways.
    //
    // What it claimed to prevent does not happen (transcripts are keyed by session
    // uuid; `spawn_worktree_session` has the whole account), and the real hazard,
    // a resume into a tree cut again at the same path, is `worktree::branch_drift`'s.
    let name = ensure_pr_worktree_at(app, pr, head_ref, &located).await?;
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
        skill: command.to_string(),
        args: pr.to_string(),
        agent: app.agent_for(None).await?,
        asks: true,
        extra_env: vec![
            (crate::skills::VAR_PR.to_string(), pr.to_string()),
            // Asked by `may_push` before anything was spawned, so it is the answer
            // and not a guess.
            (crate::skills::VAR_MAY_PUSH.to_string(), "yes".to_string()),
            // The language a reply is written in when the thread does not settle
            // it. Config, so the skill cannot carry it.
            (
                crate::skills::VAR_LANGUAGE.to_string(),
                app.settings().default_language,
            ),
        ],
    };
    // Its own id: this is the `handle-review` pane, the one run with no record of the
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
        let (env, _) = crate::spawn::run_env(&cfg, &dir, id, Some("ask-tok"), &[]);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

        assert_eq!(get("ORCH_ASK_TOKEN").as_deref(), Some("ask-tok"));
        assert_eq!(
            get("ORCH_SESSION_ID").as_deref(),
            Some(id.to_string().as_str())
        );

        // The regression this exists for. `ORCHD_TOKEN` here means the run holds
        // the whole API again — teardown, spawn, file writes — while reading text a
        // stranger wrote on a pull request.
        assert!(
            get("ORCHD_TOKEN").is_none(),
            "the app token must never reach a run that reads third-party comments"
        );
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
    /// `fix-pr` never had the check, so one PR answered two ways.
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
        pushable(&app, 4, true).await;
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

    /// Put PR `n` on `feature/x` into the poll, and you on it, with push access or
    /// without. Every review spawn asks `may_push` first.
    async fn pushable(app: &Arc<crate::state::AppState>, n: u64, may: bool) {
        let mut inner = app.inner.write().await;
        inner.viewer = Some("me".into());
        inner.prs = vec![crate::forge::Pr {
            head_ref: "feature/x".into(),
            head_repo: Some("acme/repo".into()),
            head_pushable: Some(may),
            ..crate::testutil::pr(n)
        }];
    }

    /// A branch you may not push to is refused before any session is asked or any
    /// tree is cut, and by name — the question `fix-pr` asks, not the owner-is-you
    /// rule the skills each kept.
    #[tokio::test]
    async fn a_review_of_a_branch_you_cannot_push_to_is_refused_first() {
        let (app, dir, _) = with_live_session("handle-nopush", None).await;
        pushable(&app, 4, false).await;
        let err = spawn_command_session(&app, 4, "feature/x", Pass::HANDLE_REVIEW)
            .await
            .expect_err("not yours to push to");
        let err = format!("{err:#}");
        assert!(err.contains("acme/repo, which you cannot push to"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dirty worktree already holding the PR's branch is refused by every flow
    /// that writes into it, `fix_pr::start` included — which never gated at all.
    ///
    /// Main on the branch is deliberately not this case: a flow that finds main
    /// there carries the branch and its work out into the PR's tree (e2e flows 19
    /// and 41). Asserted on what matters — the work still in the tree and no run
    /// recorded — not only on the refusal.
    #[tokio::test]
    async fn a_dirty_pr_worktree_is_refused_before_a_flow_writes_into_it() {
        let dir = crate::testutil::scratch_repo("gate-tree");
        let app = crate::testutil::app_at(&dir, "");
        let tree = app.cfg.worktree_path("pr-4");
        crate::testutil::git(
            &dir,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/x",
                &tree.to_string_lossy(),
            ],
        );
        std::fs::write(tree.join("wip.txt"), "somebody's work\n").unwrap();
        crate::testutil::git(&tree, &["add", "wip.txt"]);
        app.register_worktree("pr-4", tree.clone(), Some("feature/x".into()))
            .await;
        pushable(&app, 4, true).await;

        let pane = spawn_command_session(&app, 4, "feature/x", Pass::HANDLE_REVIEW).await;
        let fix = crate::fix_pr::start(&app, 4).await;
        for (what, out) in [("pane", pane), ("fix-pr", fix)] {
            let err = format!("{:#}", out.expect_err(what));
            assert!(err.contains("uncommitted"), "{what}: {err}");
        }

        assert!(
            tree.join("wip.txt").exists(),
            "the tree's work went somewhere"
        );
        assert!(app.inner.read().await.automation.get(4).is_none());

        // The refusal carries the gate, so the page can ask rather than only say.
        let err = spawn_command_session(&app, 4, "feature/x", Pass::HANDLE_REVIEW)
            .await
            .expect_err("still dirty");
        let gate = err
            .chain()
            .find_map(|e| e.downcast_ref::<crate::model::Settleable>())
            .map(|s| s.gate.clone())
            .expect("a settleable refusal");
        assert_eq!(gate["gate"], "dirty");
        assert_eq!(gate["files"].as_array().map(Vec::len), Some(1));

        // Set aside: a clean tree, new files included, and the work on the bank.
        std::fs::write(tree.join("new.txt"), "untracked\n").unwrap();
        settle_dirty(&app, 4, "feature/x", Dirty::Bank)
            .await
            .expect("banked");
        assert!(!tree.join("wip.txt").exists() && !tree.join("new.txt").exists());
        assert!(
            app.workspace_banked("pr-4").await.is_some(),
            "the bank holds it"
        );
        assert_eq!(
            tree_gate(&locate_pr(&app, 4, "feature/x").await.unwrap())
                .await
                .unwrap(),
            None
        );
        assert!(
            settle_dirty(&app, 4, "feature/x", Dirty::Bank)
                .await
                .is_ok(),
            "a clean tree has nothing to set aside, and banks nothing over the bank"
        );

        // Commit: the files stay, as a commit on the branch.
        std::fs::write(tree.join("kept.txt"), "kept\n").unwrap();
        app.set_banked("pr-4", None).await;
        settle_dirty(&app, 4, "feature/x", Dirty::Commit)
            .await
            .expect("committed");
        assert!(tree.join("kept.txt").exists());
        let subject = crate::testutil::git(&tree, &["log", "-1", "--format=%s"]);
        assert_eq!(subject.trim(), KEPT_MESSAGE);
        assert_eq!(
            tree_gate(&locate_pr(&app, 4, "feature/x").await.unwrap())
                .await
                .unwrap(),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A live session in the PR's worktree, with whatever run it is doing.
    ///
    /// Idle rather than mid-turn, and with this test process's pid, because
    /// `live_sessions_in` wants both before it will report the session at all.
    async fn with_live_session(
        tag: &str,
        pass: Option<Pass>,
    ) -> (Arc<crate::state::AppState>, std::path::PathBuf, uuid::Uuid) {
        let dir = crate::testutil::scratch(tag);
        let cfg = crate::config::Config::parse(&format!(
            r#"{{"main_checkout":{:?}}}"#,
            dir.to_string_lossy()
        ))
        .expect("parse");
        let app = crate::state::AppState::new(cfg, "t".into(), crate::window::Chrome::None);
        let id = uuid::Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            inner.workspaces.insert(
                "pr-4".into(),
                crate::model::Workspace {
                    id: "pr-4".into(),
                    path: dir.join("pr-4"),
                    kind: crate::model::WorkspaceKind::Worktree {
                        name: "pr-4".into(),
                    },
                    branches: ["feature/x".to_string()].into_iter().collect(),
                    processes: Vec::new(),
                    occupant: None,
                    tree: crate::model::Tree {
                        branch: Some("feature/x".into()),
                        ..Default::default()
                    },
                    banked: None,
                },
            );
            let mut s = Session::new(id, "pr-4".into(), dir.join("pr-4"), pass);
            s.set_state(State::YourTurn {
                since: std::time::SystemTime::now(),
                reason: crate::model::TurnReason::TurnComplete,
            });
            s.pid = Some(std::process::id());
            inner.sessions.insert(id, s);
        }
        pushable(&app, 4, true).await;
        (app, dir, id)
    }

    /// The press has to reach the agent, not just the rail.
    ///
    /// **This returned `Ok(id)` and typed nothing.** The rail selected the session
    /// you already had and toasted "handling #4", so a button that did half its job
    /// looked exactly like one that did all of it — which is how it was reported:
    /// "handle review didn't work when I had an open session for a PR".
    ///
    /// Asserted through `type_user_turn`'s own refusal rather than a delivered
    /// keystroke, because a real one needs a pty and a `claude`. The session here
    /// has none, so reaching that refusal is proof the typing was attempted; the
    /// old code could not produce it, because it never got there.
    #[tokio::test]
    async fn handling_a_pr_that_already_has_a_session_asks_that_session() {
        let (app, dir, _) = with_live_session("handle-live", None).await;
        let err = format!(
            "{:#}",
            spawn_command_session(&app, 4, "feature/x", Pass::HANDLE_REVIEW)
                .await
                .expect_err("no pty here, so the typing cannot land")
        );
        assert!(
            err.contains("not running"),
            "the command never reached the session: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Main as a real checkout on `feature/x`, holding one live session per `passes`
    /// entry, oldest first; nothing else holds the branch.
    ///
    /// Real, because the hand-off asks git which branch main is on: the cached
    /// `Tree::branch` is up to two seconds stale in either direction.
    async fn with_main_on_branch(
        tag: &str,
        passes: &[Option<Pass>],
    ) -> (
        Arc<crate::state::AppState>,
        std::path::PathBuf,
        Vec<uuid::Uuid>,
    ) {
        let (app, dir, worktree_session) = with_live_session(tag, None).await;
        let git = |args: &[&str]| crate::testutil::git(&dir, args);
        git(&["init", "-q", "-b", "trunk"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "a",
        ]);
        git(&["switch", "-q", "-c", "feature/x"]);
        let mut ids = Vec::new();
        {
            let mut inner = app.inner.write().await;
            inner.workspaces.remove("pr-4");
            inner.sessions.remove(&worktree_session);
            let now = std::time::SystemTime::now();
            for (i, pass) in passes.iter().enumerate() {
                let id = uuid::Uuid::new_v4();
                let mut s = Session::new(id, MAIN.into(), dir.clone(), pass.clone());
                s.set_state(State::YourTurn {
                    since: now,
                    reason: crate::model::TurnReason::TurnComplete,
                });
                s.pid = Some(std::process::id());
                s.branch = Some("feature/x".into());
                s.created_at = now + std::time::Duration::from_secs(i as u64);
                inner.sessions.insert(id, s);
                ids.push(id);
            }
            inner.workspaces.insert(
                MAIN.into(),
                crate::model::Workspace {
                    id: MAIN.into(),
                    path: dir.clone(),
                    kind: crate::model::WorkspaceKind::Main,
                    branches: ["feature/x".to_string()].into_iter().collect(),
                    processes: Vec::new(),
                    occupant: ids.first().copied(),
                    tree: crate::model::Tree {
                        branch: Some("feature/x".into()),
                        ..Default::default()
                    },
                    banked: None,
                },
            );
        }
        (app, dir, ids)
    }

    /// A session in main on the PR's branch is asked too, by both verbs.
    ///
    /// "Open in main" makes exactly this, and main was left out of the hand-off: the
    /// row hid `handle` and `fix`, and the daemon refused both with "close that
    /// session" — on the one PR you had open. Reaching `type_user_turn`'s refusal
    /// is proof the typing was attempted, as in the test above.
    #[tokio::test]
    async fn a_session_in_main_on_the_branch_is_asked_too() {
        let (app, dir, _) = with_main_on_branch("handle-main", &[None]).await;
        let pane = spawn_command_session(&app, 4, "feature/x", Pass::HANDLE_REVIEW).await;
        let fix = crate::fix_pr::start(&app, 4).await;
        for (what, out) in [("handle", pane), ("fix", fix)] {
            let err = format!(
                "{:#}",
                out.expect_err("no pty here, so the typing cannot land")
            );
            assert!(
                err.contains("not running"),
                "{what} never reached the session in main: {err}"
            );
        }
        assert!(
            app.inner.read().await.automation.get(4).is_none(),
            "a hand-off starts no run, so it records none"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cache still says main is on the branch, git says it has left: nobody is
    /// asked. Trusting the cache typed a fix-pr — a rebase and a force-push — into
    /// main on an unrelated branch.
    #[tokio::test]
    async fn a_stale_cache_does_not_hand_a_pr_to_main() {
        let (app, dir, _) = with_main_on_branch("handle-stale", &[None]).await;
        crate::testutil::git(&dir, &["switch", "-q", "trunk"]);
        let located = locate_pr(&app, 4, "feature/x").await.unwrap();
        let handed = hand_to_live_session(&app, &located, 4, "feature/x", Pass::FIX_PR)
            .await
            .expect("not an error, just nobody");
        assert_eq!(handed, None, "main left the branch, so it is not asked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With several sessions in main, the press goes to the one the PR row names —
    /// the newest on the branch — not the oldest in the tree. Shown by giving only
    /// the newest the pass being pressed: landing on it needs no typing.
    #[tokio::test]
    async fn the_hand_off_asks_the_session_the_row_names() {
        let pass = Pass {
            pr: 4,
            command: Pass::FIX_PR.to_string(),
        };
        let (app, dir, ids) = with_main_on_branch("handle-named", &[None, Some(pass)]).await;
        let located = locate_pr(&app, 4, "feature/x").await.unwrap();
        let handed = hand_to_live_session(&app, &located, 4, "feature/x", Pass::FIX_PR)
            .await
            .expect("landing on the run already going is not a refusal");
        assert_eq!(
            handed,
            Some(ids[1]),
            "went to the oldest instead of the named one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Already doing this pass: the press means "show me", and typing it again
    /// would queue a second walk over the same threads.
    #[tokio::test]
    async fn a_session_already_on_this_pass_is_only_landed_on() {
        let pass = Pass {
            pr: 4,
            command: Pass::HANDLE_REVIEW.to_string(),
        };
        let (app, dir, id) = with_live_session("handle-same", Some(pass)).await;
        let got = spawn_command_session(&app, 4, "feature/x", Pass::HANDLE_REVIEW)
            .await
            .expect("landing on the pass already running is not a refusal");
        assert_eq!(got, id, "landed somewhere other than the running pass");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Another run owns the tree, and typing a slash command into it derails it.
    ///
    /// `gate` refuses a live `fix-pr` for exactly this reason and never sees one,
    /// because a running fix-pr *is* a live session and the branch above it wins.
    /// So the refusal has to be here.
    #[tokio::test]
    async fn handling_a_pr_a_fix_run_is_working_on_is_refused_by_name() {
        let pass = Pass {
            pr: 4,
            command: Pass::FIX_PR.to_string(),
        };
        let (app, dir, _) = with_live_session("handle-fixpr", Some(pass)).await;
        let err = format!(
            "{:#}",
            spawn_command_session(&app, 4, "feature/x", Pass::HANDLE_REVIEW)
                .await
                .expect_err("a running fix-pr owns that worktree")
        );
        assert!(
            err.contains("fix-pr") && err.contains("pr-4"),
            "the refusal must name the run and the tree: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
