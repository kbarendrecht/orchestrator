use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::transcript_dir_for;
use crate::git::{self, Unpushed};
use crate::model::*;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preflight {
    pub workspace: String,
    pub checks: Vec<Check>,
    pub can_remove: bool,
}

impl Preflight {
    fn failed(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }
}

/// The two preflight checks teardown can satisfy on its own by archiving. Named
/// rather than spelled inline at both the check and the gate that keys off them,
/// so relabelling one for the UI cannot silently turn the auto-archive off.
const CHECK_TRANSCRIPT: &str = "transcript copied";
const CHECK_RECOVERY: &str = "recovery record written";

/// Teardown preflight — **all** must pass (§2).
///
/// Nothing here is advisory. The unpushed check fails closed on anything that
/// carries commits existing nowhere else, which is precisely the case where
/// losing work is most likely.
pub async fn preflight(app: &Arc<AppState>, workspace: &str) -> Result<Preflight> {
    if workspace == MAIN {
        bail!("main is not a worktree and is never torn down");
    }
    let (path, branches) = {
        let inner = app.inner.read().await;
        let w = inner
            .workspaces
            .get(workspace)
            .with_context(|| format!("unknown workspace {workspace}"))?;
        (w.path.clone(), w.branches.clone())
    };

    let mut checks = Vec::new();

    // 1. No live session. Consults /proc rather than in-memory state, because a
    //    crashed daemon is exactly when a stale answer would let you delete a
    //    worktree with a live agent in it (§8b).
    let live = app.live_sessions_in(workspace).await;
    checks.push(Check {
        name: "no live session",
        passed: live.is_empty(),
        detail: if live.is_empty() {
            "no session outside Exited/Archived".into()
        } else {
            format!("{} live session(s): {:?}", live.len(), live)
        },
    });

    // Checks 2 and 3 are three git execs, and two of them walk the whole worktree —
    // `configure_repo` sets fsmonitor on main only, so in a worktree that is a full
    // scan. Off the runtime, and batched into one hop rather than three, because
    // they are asked together and only differ in what they report.
    let (clean, branch, unpushed) = {
        let at = path.clone();
        let fallback = branches.iter().next().cloned();
        let upstream = app.cfg.upstream_ref.clone();
        crate::proc::run_blocking("the teardown preflight's git checks", move || {
            let clean = git::is_clean(&at).unwrap_or(false);
            let branch = git::current_branch(&at)
                .ok()
                .or(fallback)
                .unwrap_or_default();
            let unpushed = if branch.is_empty() {
                Unpushed::NeverPushed {
                    commits: vec!["(no branch resolved)".to_string()],
                }
            } else {
                git::unpushed(&at, &branch, &upstream).unwrap_or(Unpushed::NeverPushed {
                    commits: vec!["(could not check origin)".to_string()],
                })
            };
            (clean, branch, unpushed)
        })
        .await?
    };

    // 2. Clean tree.
    checks.push(Check {
        name: "clean tree",
        passed: clean,
        detail: if clean {
            "git status --porcelain is empty".into()
        } else {
            "uncommitted changes present".into()
        },
    });

    // 3. Nothing unpushed.
    checks.push(Check {
        name: "nothing unpushed",
        passed: !unpushed.blocks_teardown(),
        detail: match &unpushed {
            Unpushed::NeverPushed { commits } if commits.is_empty() => {
                format!("{branch} was never pushed, but carries nothing beyond the base")
            }
            Unpushed::NeverPushed { commits } => format!(
                "{branch} has no counterpart on origin and carries {} commit(s) that exist nowhere else",
                commits.len()
            ),
            Unpushed::Ahead { commits } => {
                format!("{} commit(s) not on origin/{branch}", commits.len())
            }
            Unpushed::UpToDate => format!("origin/{branch} is up to date"),
        },
    });

    // 4. Transcript copied. The original in ~ survives teardown, but the copy
    //    protects against Claude Code pruning and against a later name
    //    collision (§2).
    let copied = transcripts_archived(app, workspace).await;
    checks.push(Check {
        name: CHECK_TRANSCRIPT,
        passed: copied.0,
        detail: copied.1,
    });

    // 5. Recovery record written.
    let recovery = recovery_recorded(app, workspace).await;
    checks.push(Check {
        name: CHECK_RECOVERY,
        passed: recovery.0,
        detail: recovery.1,
    });

    // 6. Processes stopped.
    let attached = {
        let inner = app.inner.read().await;
        inner
            .workspaces
            .get(workspace)
            .map(|w| {
                w.processes
                    .iter()
                    .filter(|p| p.pty.as_ref().map(|h| h.is_alive()).unwrap_or(false))
                    .count()
            })
            .unwrap_or(0)
    };
    checks.push(Check {
        name: "processes stopped",
        passed: attached == 0,
        detail: if attached == 0 {
            "no Process still attached".into()
        } else {
            format!("{attached} process(es) still running")
        },
    });

    let can_remove = checks.iter().all(|c| c.passed);
    Ok(Preflight {
        workspace: workspace.to_string(),
        checks,
        can_remove,
    })
}

async fn transcripts_archived(app: &Arc<AppState>, workspace: &str) -> (bool, String) {
    let inner = app.inner.read().await;
    let sessions: Vec<_> = inner
        .sessions
        .values()
        .filter(|s| s.workspace == workspace)
        .collect();
    if sessions.is_empty() {
        return (true, "no sessions to archive".into());
    }
    let pending = sessions.iter().filter(|s| !s.transcript_archived).count();
    let copied = sessions
        .iter()
        .filter(|s| s.archived_transcript.is_some())
        .count();
    if pending == 0 {
        (
            true,
            format!("{copied} of {} transcript(s) copied, rest had none", sessions.len()),
        )
    } else {
        (false, format!("{pending} transcript(s) not yet archived"))
    }
}

async fn recovery_recorded(app: &Arc<AppState>, workspace: &str) -> (bool, String) {
    let inner = app.inner.read().await;
    let sessions: Vec<_> = inner
        .sessions
        .values()
        .filter(|s| s.workspace == workspace)
        .collect();
    if sessions.is_empty() {
        return (true, "no sessions to record".into());
    }
    let missing = sessions.iter().filter(|s| s.recovery.is_none()).count();
    if missing == 0 {
        (true, "(name, branch, head_sha) persisted".into())
    } else {
        (false, format!("{missing} session(s) without a recovery record"))
    }
}

/// Rebuild a torn-down worktree so an archived session can be resumed.
///
/// The counterpart to [`archive`], and it lives here for that reason: every other
/// worktree verb — preflight, archive, teardown — is this module's, and this one
/// was inlined in the HTTP handler that resumes a session.
///
/// Rebuilt at the **same absolute path**, because transcripts are keyed by working
/// directory and a different path resumes nothing (§2).
///
/// The `Some` return is a warning, not a failure: the branch having moved on since
/// the conversation is the normal case for work that landed, so it is said rather
/// than refused.
pub async fn revive(
    app: &Arc<AppState>,
    cwd: &std::path::Path,
    recovery: Option<ArchiveState>,
) -> Result<Option<String>> {
    // No record at all is not the same as "unrecoverable". A record is only written
    // by `archive`, so a worktree removed any other way — by hand, or lost with the
    // checkout it lived in — leaves the conversation intact and the instructions
    // missing. That refused a session that was perfectly rebuildable, and said
    // "no recovery record", which describes the daemon's bookkeeping rather than
    // anything the reader can act on.
    //
    // The tree is still describable without a record: its name is the directory it
    // ran in, and the branch follows by the same `worktree-<name>` convention
    // `register_worktree` uses. So derive one and let `worktree_rebuild` decide —
    // it already looks for the branch in the checkout, then on origin, then at the
    // recorded commit, and names whichever is missing.
    let derived = recovery.is_none();
    let recovery = recovery.or_else(|| derive_recovery(cwd));

    let Some(ArchiveState::Recoverable {
        name,
        branch,
        head_sha,
    }) = recovery
    else {
        bail!("no recovery record, and the worktree's name cannot be read off its path");
    };

    let main = app.cfg.main_checkout.clone();
    let (path, b, sha) = (cwd.to_path_buf(), branch.clone(), head_sha.clone());
    let moved =
        tokio::task::spawn_blocking(move || git::worktree_rebuild(&main, &path, &b, &sha))
            .await
            .map_err(|e| anyhow::anyhow!("rebuild task failed: {e}"))??;

    // A rebuilt worktree is a fresh checkout at the old path — its symlinks and
    // creation-time files are gone with the tree that was torn down, so the setup
    // seam has to run again, the same as on first creation.
    crate::spawn::run_worktree_hooks(app, cwd).await;

    app.register_worktree(&name, cwd.to_path_buf(), Some(branch.clone()))
        .await;

    // A derived record never knew where the conversation left off, so every rebuild
    // would look like the branch had moved. Saying "recorded  , branch now abc1234"
    // is worse than saying nothing: it invents a comparison that was never made.
    if derived {
        return Ok(Some(format!(
            "{name} was rebuilt from branch {branch} — no recovery record was written \
             when it went, so this is the branch as it stands, not necessarily the code \
             the conversation ran on"
        )));
    }

    Ok(moved.map(|tip| {
        format!(
            "{name} moved since this conversation: recorded {}, branch now {}",
            git::short(&head_sha),
            git::short(&tip)
        )
    }))
}

/// What a resuming session is actually coming back to, when its worktree is
/// still standing.
///
/// A path that exists is not the same as *this conversation's* tree. A torn-down
/// worktree's name can be cut again at the same path — `ensure_pr_worktree` does
/// exactly that for a PR you come back to — and [`revive`] is then skipped
/// entirely, so nothing else ever looks at the branch.
///
/// Driven against a fixture daemon, which is how this was found rather than
/// reasoned: an archived `pr-4` recorded on `feat` resumed into a `pr-4` checked
/// out on `other`, and the API answered `200` with `warning: null`.
///
/// Said, not refused. The ordinary case is the same branch, where resuming is
/// exactly right, and the record is the conversation's own history — it cannot
/// arbitrate what the tree is for now.
pub fn branch_drift(cwd: &std::path::Path, recovery: Option<&ArchiveState>) -> Option<String> {
    let ArchiveState::Recoverable { name, branch, .. } = recovery? else {
        return None;
    };
    drift_message(name, branch, &git::current_branch(cwd).ok()?)
}

/// The wording, apart from the git call so it can be tested without one.
fn drift_message(name: &str, recorded: &str, now: &str) -> Option<String> {
    (recorded != now).then(|| {
        format!(
            "{name} is on {now} now, not the {recorded} this conversation ran on — \
             the worktree at that path was cut again after this one was archived"
        )
    })
}

/// A recovery record for a worktree that never got one.
///
/// Only the *shape* is recovered, not the history: `head_sha` is empty because
/// nothing recorded it. `worktree_rebuild` reaches for that value only when the
/// branch is gone from both the checkout and origin, and in that case the branch
/// is the honest thing to name as missing — which is what it does.
fn derive_recovery(cwd: &std::path::Path) -> Option<ArchiveState> {
    let name = cwd.file_name()?.to_string_lossy().into_owned();
    Some(ArchiveState::Recoverable {
        branch: format!("worktree-{name}"),
        name,
        head_sha: String::new(),
    })
}

/// Copy transcripts into daemon storage and persist a recovery record, so the
/// worktree can be rebuilt for `--resume` later (§2).
pub async fn archive(app: &Arc<AppState>, workspace: &str) -> Result<()> {
    let (path, name) = {
        let inner = app.inner.read().await;
        let w = inner
            .workspaces
            .get(workspace)
            .with_context(|| format!("unknown workspace {workspace}"))?;
        let name = match &w.kind {
            WorkspaceKind::Worktree { name } => name.clone(),
            WorkspaceKind::Main => bail!("main is never archived"),
        };
        (w.path.clone(), name)
    };

    // Off the runtime, like the transcript copy below: two git reads on a
    // teardown that is already waiting on a preflight and a remove.
    let (branch, head_sha) = {
        let (at, name) = (path.clone(), name.clone());
        crate::proc::run_blocking("reading the worktree's branch", move || {
            (
                git::current_branch(&at).unwrap_or_else(|_| format!("worktree-{name}")),
                git::head_sha(&at).ok(),
            )
        })
        .await?
    };

    let store = crate::config::Config::config_dir()?.join("transcripts");
    std::fs::create_dir_all(&store)?;

    let ids: Vec<SessionId> = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .values()
            .filter(|s| s.workspace == workspace)
            .map(|s| s.id)
            .collect()
    };

    for id in ids {
        // Claude Code keys the transcript directory by working directory, so the
        // path is derivable even if SessionStart never reported one.
        let src = {
            let inner = app.inner.read().await;
            inner
                .sessions
                .get(&id)
                .and_then(|s| s.transcript_path.clone())
                .or_else(|| transcript_dir_for(&path).ok().map(|d| d.join(format!("{id}.jsonl"))))
        };
        let src = src.unwrap_or_else(|| PathBuf::from("/nonexistent"));
        let dest = store.join(format!("{id}.jsonl"));
        // Off the runtime: a transcript is megabytes on a long conversation, and
        // this copies one per session in the workspace.
        {
            let (from, to) = (src.clone(), dest.clone());
            crate::proc::run_blocking("archiving the transcript", move || {
                if from.exists() {
                    std::fs::copy(&from, &to)
                        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
                }
                Ok::<(), anyhow::Error>(())
            })
            .await??;
        }

        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            if dest.exists() {
                s.archived_transcript = Some(dest.clone());
            }
            // Settled either way: a session killed before its first turn has no
            // `.jsonl`, and waiting for one that will never be written would
            // strand the worktree.
            s.transcript_archived = true;
            s.recovery = Some(match &head_sha {
                Some(sha) => ArchiveState::Recoverable {
                    name: name.clone(),
                    branch: branch.clone(),
                    head_sha: sha.clone(),
                },
                // Branch gone and sha unreachable: the transcript is still
                // readable, the session simply cannot be continued.
                None => ArchiveState::TranscriptOnly,
            });
        }
    }

    app.notify().await;
    Ok(())
}

/// Remove a worktree once every preflight check passes.
///
/// Removal is `git worktree remove` followed by `git worktree prune`. A refusal
/// is surfaced, never escalated to `--force` and never followed by a filesystem
/// delete — the worktree is full of symlinks into main, so a recursive delete
/// that follows them destroys the main checkout (§2).
pub async fn teardown(app: &Arc<AppState>, workspace: &str) -> Result<Preflight> {
    let mut pf = preflight(app, workspace).await?;

    // Copying transcripts and writing recovery records is a prerequisite of
    // removal (preflight checks 4 and 5), and this button is the only thing that
    // triggers it — nothing archives on its own. So when those are the *only*
    // blockers, do the archive here and re-check. Deliberately gated on nothing
    // else failing: a live session, a dirty tree, unpushed commits or an attached
    // process must still refuse, and must never see an archive run under them.
    if !pf.can_remove {
        let only_archive_blocks = pf
            .checks
            .iter()
            .all(|c| c.passed || matches!(c.name, CHECK_TRANSCRIPT | CHECK_RECOVERY));
        if only_archive_blocks {
            archive(app, workspace).await?;
            pf = preflight(app, workspace).await?;
        }
    }

    if !pf.can_remove {
        let failed: Vec<String> = pf
            .failed()
            .iter()
            .map(|c| format!("{}: {}", c.name, c.detail))
            .collect();
        bail!("teardown preflight failed — {}", failed.join("; "));
    }

    let path = app
        .workspace_path(workspace)
        .await
        .with_context(|| format!("unknown workspace {workspace}"))?;
    // The repo's own teardown first, then ours. Ours runs either way and no-ops
    // when the hook already did the job.
    let payload = remove_payload(&app.cfg.main_checkout, workspace, &path);
    run_repo_hooks(app, "WorktreeRemove", payload).await;
    // Off the runtime: this deletes the tree, which on a checkout carrying
    // `node_modules` is seconds of filesystem work, and it retries a stale lock.
    {
        let (main, at) = (app.cfg.main_checkout.clone(), path.clone());
        crate::proc::run_blocking("removing the worktree", move || {
            git::worktree_remove(&main, &at)
        })
        .await??;
    }

    {
        let mut inner = app.inner.write().await;
        inner.workspaces.remove(workspace);
        for s in inner.sessions.values_mut() {
            if s.workspace == workspace {
                let resumable = s.resumable();
                s.set_state(State::Archived { resumable });
            }
        }
    }
    app.notify().await;
    Ok(pf)
}

/// Remove the worktrees of conversations nobody has come back to.
///
/// The one thing on a timer, and it is on one because the silt is the daemon's own
/// doing: `claude --worktree` removes its own tree when its session ends, and the
/// daemon owns the pty and kills it, so that cleanup never runs. 61 trees and 14 GB
/// on the machine this was written for, of which 55 were the agent's to remove.
///
/// **The tree, never the conversation.** Every row stays in the rail, stays
/// archived and stays resumable, because `revive` rebuilds the tree at the same
/// absolute path. Deleting a record is a separate decision, taken by a person, and
/// is deliberately not automated: it drops the archived transcript with it.
///
/// It removes nothing the button would not. There is no second set of rules and no
/// bypass: this calls the same [`teardown`], so a live session, a dirty tree, an
/// unpushed commit or an attached process refuses here exactly as it refuses a
/// right-click, and the archive it needs runs the same way. What this adds is only
/// *when* to ask.
///
/// Deliberately quiet about a refusal. A tree that carries work is the normal
/// outcome and will be a refusal again on the next pass, so a warning per tree per
/// hour would be a log that teaches you to stop reading it. Removals are logged
/// individually, because that is the thing that happened.
pub async fn reap_old(app: &Arc<AppState>) -> usize {
    let days = app.cfg.worktree_retention_days;
    if days == 0 {
        return 0;
    }
    let Some(cutoff) = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(u64::from(days) * 86_400))
    else {
        return 0;
    };

    let candidates: Vec<String> = {
        let inner = app.inner.read().await;
        inner
            .workspaces
            .values()
            .filter(|w| !w.is_main())
            .filter(|w| {
                let mut newest = None;
                for s in inner.sessions.values().filter(|s| s.workspace == w.id) {
                    // Anything still running keeps its tree, whatever its age. The
                    // preflight says so too; this is just not asking.
                    if s.state.is_live() {
                        return false;
                    }
                    newest = newest.max(Some(crate::store::last_used(s)));
                }
                /* A tree with no conversation pointing at it at all, which is the
                   commonest shape of silt and the one nothing else can reach: 32 of
                   61 trees on the machine this was written for. They are made, not
                   found — `spawn::watch_session_exit` forgets a turnless session's
                   record and used to leave its tree standing, and `store` drops the
                   same class of record again at load.

                   The safest population rather than the riskiest: nothing can
                   resume it, and `git worktree remove` never deletes the branch, so
                   the commits stay reachable from main whatever happens here.

                   Dated by the directory, because the record that would have dated
                   it is precisely what is missing. Deliberately *not* the
                   per-worktree index, which looks like the better signal and is
                   worthless: the daemon's own reconcile runs `git status` in every
                   tree and refreshes it, measured at 0.0 days for all 32 while the
                   directories themselves read 8 to 19 days. A directory that is
                   gone answers `None` and is left alone — a record outliving its
                   tree is a real state here, and teardown would fail on it anyway. */
                newest
                    .or_else(|| std::fs::metadata(&w.path).ok()?.modified().ok())
                    .is_some_and(|at| at < cutoff)
            })
            .map(|w| w.id.clone())
            .collect()
    };

    let mut removed = 0;
    for ws in candidates {
        match teardown(app, &ws).await {
            Ok(_) => {
                removed += 1;
                tracing::info!(
                    workspace = %ws,
                    days,
                    "removed the worktree of a conversation nobody came back to; \
                     the row stays and a resume rebuilds the tree"
                );
            }
            // At debug: see the doc comment. A tree holding work refuses every pass.
            Err(e) => tracing::debug!(workspace = %ws, "left in place: {e:#}"),
        }
    }
    removed
}

/// How long a repo's worktree hook gets. Creation and removal are both filesystem
/// work with at most a fetch in front of them, and anything slower than this is
/// stuck rather than busy.
const WORKTREE_HOOK_TIMEOUT_SECS: u64 = 300;

/// The repo's `command` hooks for one worktree event, in the order Claude Code
/// would run them.
///
/// Read from the repo's own settings rather than the daemon's: these are the
/// *repo's* worktree policy, and the daemon's `--settings` file is where the
/// daemon's own hooks live. Both of Claude Code's repo-level files are consulted,
/// project first then local, which is the layering it uses everywhere else.
///
/// Only `type: "command"` is returned. An `http` hook is Claude Code's to deliver
/// and has no meaning outside a session; skipping it silently is right, because the
/// daemon's own path follows either way.
pub(crate) fn repo_worktree_hooks(main: &std::path::Path, event: &str) -> Vec<String> {
    let mut out = Vec::new();
    for name in [".claude/settings.json", ".claude/settings.local.json"] {
        let Ok(raw) = std::fs::read_to_string(main.join(name)) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(entries) = v.pointer(&format!("/hooks/{event}")).and_then(|h| h.as_array()) else {
            continue;
        };
        for entry in entries {
            let Some(hooks) = entry.get("hooks").and_then(|h| h.as_array()) else {
                continue;
            };
            for h in hooks {
                if h.get("type").and_then(|t| t.as_str()) != Some("command") {
                    continue;
                }
                if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                    out.push(cmd.to_string());
                }
            }
        }
    }
    out
}

/// Run the repo's `command` hooks for one worktree event, in order.
///
/// One function because there are two events and they differ only in their payload:
/// same `sh -c`, same `CLAUDE_PROJECT_DIR`, same bounded exec, same reporting. Two
/// copies of "how the daemon runs a repo hook" is how the next change to it — an env
/// key, an exit-code rule, a timeout — reaches one and not the other.
///
/// **Never fatal, and never gates the daemon's own work.** A hook that fails must
/// not strand a worktree, so failures are logged with the hook's own stderr and the
/// caller carries on. Returns the successful runs' output, in order, for the one
/// caller that reads stdout.
///
/// A shell string, the way Claude Code defines a `command` hook, so `$VAR` and a
/// pipe mean what the author wrote. `CLAUDE_PROJECT_DIR` is exported on the child
/// because repo hooks address themselves through it.
pub(crate) async fn run_repo_hooks(
    app: &Arc<AppState>,
    event: &str,
    payload: serde_json::Value,
) -> Vec<std::process::Output> {
    let hooks = repo_worktree_hooks(&app.cfg.main_checkout, event);
    let mut out = Vec::new();
    if hooks.is_empty() {
        return out;
    }
    let body = payload.to_string().into_bytes();
    for cmd in hooks {
        let argv = vec!["sh".to_string(), "-c".to_string(), cmd];
        let at = app.cfg.main_checkout.clone();
        let envs = vec![(
            "CLAUDE_PROJECT_DIR".to_string(),
            app.cfg.main_checkout.to_string_lossy().into_owned(),
        )];
        let body = body.clone();
        let label = event.to_string();
        let run = tokio::task::spawn_blocking(move || {
            crate::proc::run_bounded_with_input(
                &at,
                WORKTREE_HOOK_TIMEOUT_SECS,
                &argv,
                &label,
                Some(body),
                &envs,
            )
        })
        .await;
        match run {
            Ok(Ok(o)) if o.status.success() => out.push(o),
            Ok(Ok(o)) => tracing::warn!(
                event,
                "the repo's {event} hook exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Ok(Err(e)) => tracing::warn!(event, "the repo's {event} hook failed: {e:#}"),
            Err(e) => tracing::warn!(event, "the {event} hook task panicked: {e}"),
        }
    }
    out
}

/// The payload for a `WorktreeRemove`.
///
/// `worktreePath` and `worktreeName` are the spellings Claude Code's own binary
/// carries. That is the strongest evidence available: the monorepo declares no
/// `WorktreeRemove`, so this has never made a real round trip, and a repo whose hook
/// reads some other key gets a no-op rather than a wrong action.
fn remove_payload(main: &std::path::Path, workspace: &str, path: &std::path::Path) -> serde_json::Value {
    serde_json::json!({
        "hook_event_name": "WorktreeRemove",
        "cwd": main.to_string_lossy(),
        "worktreePath": path.to_string_lossy(),
        "worktreeName": workspace,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real repo, a real worktree, an old archived conversation in it.
    ///
    /// Written because this is the one thing in the daemon that removes something
    /// without being asked, and the selection is not the part that can be reasoned
    /// about safely: `is_main`, the age comparison and the preflight all have to
    /// agree, and only a real tree proves they do.
    fn repo_with_a_worktree(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, String) {
        let g = |at: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(at)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let dir = std::env::temp_dir().join(format!("orchd-reap-{tag}-{}", uuid::Uuid::new_v4()));
        let main = dir.join("main");
        std::fs::create_dir_all(&main).unwrap();
        g(&dir, &["init", "-q", "-b", "develop", "main"]);
        g(&main, &["config", "user.email", "t@t"]);
        g(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join("f"), "x").unwrap();
        g(&main, &["add", "-A"]);
        g(&main, &["commit", "-qm", "init"]);
        let sha = g(&main, &["rev-parse", "HEAD"]);
        let wt = dir.join("wt");
        g(&main, &["worktree", "add", "-q", "-b", "worktree-old", wt.to_str().unwrap()]);
        (main, wt, sha)
    }

    /// The whole point, driven end to end: the tree goes, the conversation stays.
    #[tokio::test]
    async fn reaping_removes_an_old_tree_and_keeps_the_conversation() {
        use crate::model::{ArchiveState, Kind, Session};

        let (main, wt, sha) = repo_with_a_worktree("old");
        let app = crate::testutil::app_at(
            &main,
            r#""worktree_retention_days":60,"upstream_ref":"develop","upstream_remote":"origin""#,
        );
        app.register_worktree("old", wt.clone(), Some("worktree-old".into())).await;

        let id = uuid::Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, "old".to_string(), wt.clone(), Kind::Interactive);
            // Ninety days back, and everything the preflight wants already settled:
            // the interesting question is the timer, not the archive.
            s.created_at = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 86_400);
            s.transcript_archived = true;
            s.recovery = Some(ArchiveState::Recoverable {
                name: "old".into(),
                branch: "worktree-old".into(),
                head_sha: sha,
            });
            s.set_state(State::Archived { resumable: true });
            inner.sessions.insert(id, s);
        }

        assert_eq!(reap_old(&app).await, 1, "an old clean tree is reaped");
        assert!(!wt.exists(), "the tree is gone");
        let inner = app.inner.read().await;
        assert!(
            inner.sessions.contains_key(&id),
            "the conversation stays: this removes trees, never rows"
        );
        assert!(
            inner.sessions[&id].recovery.is_some(),
            "and keeps what a resume rebuilds the tree from"
        );
        drop(inner);
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    /// A conversation you worked in for a long time is not old the day you stop.
    ///
    /// The reason retention counts from the transcript's last write rather than from
    /// `created_at`: a session started months ago and worked in yesterday reads as
    /// ancient by its start date, and its tree would go while you still remembered
    /// it. Measured on real records, the gap reaches 66 hours today; the shape is
    /// what matters, not the size.
    #[tokio::test]
    async fn a_long_running_conversation_is_dated_by_its_last_turn() {
        use crate::model::{ArchiveState, Kind, Session};

        let (main, wt, sha) = repo_with_a_worktree("lastused");
        let app = crate::testutil::app_at(
            &main,
            r#""worktree_retention_days":60,"upstream_ref":"develop","upstream_remote":"origin""#,
        );
        app.register_worktree("lastused", wt.clone(), Some("worktree-old".into())).await;

        // Started 90 days ago, and its transcript was written to a moment ago.
        let transcript = main.parent().unwrap().join("live.jsonl");
        std::fs::write(&transcript, b"{}\n").unwrap();
        let id = uuid::Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, "lastused".to_string(), wt.clone(), Kind::Interactive);
            s.created_at = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 86_400);
            s.transcript_path = Some(transcript.clone());
            s.transcript_archived = true;
            s.recovery = Some(ArchiveState::Recoverable {
                name: "lastused".into(),
                branch: "worktree-old".into(),
                head_sha: sha,
            });
            s.set_state(State::Archived { resumable: true });
            inner.sessions.insert(id, s);
        }

        assert_eq!(
            reap_old(&app).await,
            0,
            "worked in yesterday is not old, whatever the start date says"
        );
        assert!(wt.exists());

        // Now let the conversation itself go cold. Same record, same start date.
        let out = std::process::Command::new("touch")
            .args(["-t", "202001010000", transcript.to_str().unwrap()])
            .output()
            .expect("touch");
        assert!(out.status.success());

        assert_eq!(reap_old(&app).await, 1, "cold for long enough, and the tree goes");
        assert!(!wt.exists());
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    /// A tree no conversation points at is reaped, dated by the directory.
    ///
    /// The population the timer exists for and the one it originally could not
    /// reach: `watch_session_exit` forgets a turnless session's record, and the
    /// record is what dates a tree. 32 of 61 trees were in this state.
    #[tokio::test]
    async fn a_worktree_no_conversation_points_at_is_dated_by_its_directory() {

        let (main, wt, _) = repo_with_a_worktree("orphan");
        let app = crate::testutil::app_at(
            &main,
            r#""worktree_retention_days":60,"upstream_ref":"develop","upstream_remote":"origin""#,
        );
        // Registered the way `adopt_existing_worktrees` registers every tree on
        // disk at boot, and with no session record at all.
        app.register_worktree("orphan", wt.clone(), Some("worktree-old".into())).await;

        // Young by its own mtime: nothing yet, however orphaned. This is what keeps
        // a tree the agent has just cut, and not yet reported, out of reach.
        assert_eq!(reap_old(&app).await, 0, "a fresh orphan is left alone");
        assert!(wt.exists());

        // Backdated. `touch -t` rather than a crate: POSIX, and the only thing that
        // can make a directory old without waiting sixty days.
        let out = std::process::Command::new("touch")
            .args(["-t", "202001010000", wt.to_str().unwrap()])
            .output()
            .expect("touch");
        assert!(out.status.success(), "touch: {}", String::from_utf8_lossy(&out.stderr));

        assert_eq!(reap_old(&app).await, 1, "an old orphan is reaped");
        assert!(!wt.exists(), "the tree is gone");
        // The branch is not, which is what makes an orphan safe to remove: whatever
        // was committed in there is still reachable from main.
        let branches = std::process::Command::new("git")
            .args(["branch", "--list", "worktree-old"])
            .current_dir(&main)
            .output()
            .expect("git branch");
        assert!(
            String::from_utf8_lossy(&branches.stdout).contains("worktree-old"),
            "removing a worktree must never take its branch"
        );
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    /// Three ways it must decline, none of which reach the preflight.
    #[tokio::test]
    async fn reaping_declines_young_trees_live_sessions_and_a_zero_setting() {
        use crate::model::{Kind, Session};

        let (main, wt, _) = repo_with_a_worktree("keep");
        let fresh = |days: u32| {
            let (wt, main) = (wt.clone(), main.clone());
            async move {
                let app = crate::testutil::app_at(
                    &main,
                    &format!(
                        r#""worktree_retention_days":{days},"upstream_ref":"develop","upstream_remote":"origin""#
                    ),
                );
                app.register_worktree("keep", wt, Some("worktree-old".into())).await;
                app
            }
        };

        /// Archived, transcript settled, so only the age and the state are in play.
        async fn add(
            app: &Arc<AppState>,
            wt: &std::path::Path,
            state: State,
            at: std::time::SystemTime,
        ) {
            let mut inner = app.inner.write().await;
            let id = uuid::Uuid::new_v4();
            let mut s = Session::new(id, "keep".to_string(), wt.to_path_buf(), Kind::Interactive);
            s.created_at = at;
            s.transcript_archived = true;
            s.set_state(state);
            inner.sessions.insert(id, s);
        }

        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 86_400);

        // 1. Old, but the setting is off.
        let app = fresh(0).await;
        add(&app, &wt, State::Archived { resumable: true }, old).await;
        assert_eq!(reap_old(&app).await, 0, "`0` never reaps");
        assert!(wt.exists());

        // 2. Old, but something is still running in it. Age never outvotes that.
        let app = fresh(60).await;
        add(&app, &wt, State::Archived { resumable: true }, old).await;
        add(&app, &wt, State::Working, old).await;
        assert_eq!(reap_old(&app).await, 0, "a live session keeps its tree");
        assert!(wt.exists());

        // 3. Archived, clean, and simply not old enough.
        let app = fresh(60).await;
        add(&app, &wt, State::Archived { resumable: true }, std::time::SystemTime::now()).await;
        assert_eq!(reap_old(&app).await, 0, "a young tree is left alone");
        assert!(wt.exists());

        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    /// Only `command` hooks, from both repo settings files, in Claude Code's order.
    ///
    /// An `http` hook is Claude Code's to deliver and means nothing outside a
    /// session, so it is skipped rather than guessed at: the daemon's own removal
    /// runs either way, which is what makes skipping safe.
    #[test]
    fn the_repos_worktree_hooks_are_read_from_both_settings_files() {
        let dir = std::env::temp_dir().join(format!("orch-wrhooks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        assert!(repo_worktree_hooks(&dir, "WorktreeRemove").is_empty(), "no settings, no hooks");

        std::fs::write(
            dir.join(".claude/settings.json"),
            r#"{"hooks":{"WorktreeRemove":[{"hooks":[
                 {"type":"command","command":"tidy-up"},
                 {"type":"http","url":"http://x/y"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(repo_worktree_hooks(&dir, "WorktreeRemove"), vec!["tidy-up".to_string()]);
        // Keyed by event, so the create side reads its own and never the other's.
        assert!(repo_worktree_hooks(&dir, "WorktreeCreate").is_empty());

        // Local layers on top of project, the way Claude Code stacks them.
        std::fs::write(
            dir.join(".claude/settings.local.json"),
            r#"{"hooks":{"WorktreeRemove":[{"hooks":[{"type":"command","command":"mine"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(
            repo_worktree_hooks(&dir, "WorktreeRemove"),
            vec!["tidy-up".to_string(), "mine".to_string()]
        );

        // A repo that declares other hooks but not this one, and a settings file
        // that is not valid JSON, both answer "none" rather than failing teardown.
        std::fs::write(dir.join(".claude/settings.json"), r#"{"hooks":{"SessionStart":[]}}"#).unwrap();
        std::fs::write(dir.join(".claude/settings.local.json"), "{ not json").unwrap();
        assert!(repo_worktree_hooks(&dir, "WorktreeRemove").is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
    

    /// Resuming into a standing worktree skips the rebuild, so the branch was
    /// never looked at. Driven against a fixture: an archived `pr-4` recorded on
    /// `feat` came back in a `pr-4` on `other`, answered 200, `warning: null`.
    #[test]
    fn a_worktree_cut_again_on_another_branch_is_said_not_swallowed() {
        let why = drift_message("pr-4", "feat", "other").expect("drift is reported");
        assert!(
            why.contains("pr-4") && why.contains("feat") && why.contains("other"),
            "the message must name the tree and both branches: {why}"
        );
        // The ordinary case, and the reason this warns rather than refuses.
        assert_eq!(drift_message("pr-4", "feat", "feat"), None);
    }

    /// A transcript-only record has no branch to compare, and a live tree is not a
    /// reason to invent one.
    #[test]
    fn no_recovery_record_means_no_drift_warning() {
        assert_eq!(
            branch_drift(std::path::Path::new("/nonexistent"), None),
            None
        );
        assert_eq!(
            branch_drift(
                std::path::Path::new("/nonexistent"),
                Some(&ArchiveState::TranscriptOnly)
            ),
            None
        );
    }

    /// The gap this closes: a worktree removed by hand leaves no recovery record,
    /// and resuming its session refused with "no recovery record" — a fact about
    /// the daemon's bookkeeping, not something the reader can act on. The tree is
    /// still describable from the path it ran in.
    #[test]
    fn a_missing_recovery_record_is_derived_from_the_path() {
        let got = derive_recovery(std::path::Path::new(
            "/home/me/repo/.claude/worktrees/compressed-singing-lerdorf",
        ));
        match got {
            Some(ArchiveState::Recoverable {
                name,
                branch,
                head_sha,
            }) => {
                assert_eq!(name, "compressed-singing-lerdorf");
                // The convention `register_worktree` uses, so the branch a
                // daemon-cut or `claude --worktree` tree is on.
                assert_eq!(branch, "worktree-compressed-singing-lerdorf");
                // Never recorded, and `worktree_rebuild` only consults it once the
                // branch is gone from the checkout *and* origin — where naming the
                // branch is the honest answer anyway.
                assert!(head_sha.is_empty());
            }
            other => panic!("expected a derived record, got {other:?}"),
        }

        // A path with no final component gives nothing to derive from, and the
        // caller says so rather than inventing a name.
        assert!(derive_recovery(std::path::Path::new("/")).is_none());
    }
}
