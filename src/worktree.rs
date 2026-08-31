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

    // 2. Clean tree.
    let clean = git::is_clean(&path).unwrap_or(false);
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
    let branch = git::current_branch(&path)
        .ok()
        .or_else(|| branches.iter().next().cloned())
        .unwrap_or_default();
    let unpushed = if branch.is_empty() {
        Unpushed::NeverPushed {
            commits: vec!["(no branch resolved)".to_string()],
        }
    } else {
        git::unpushed(&path, &branch, &app.cfg.upstream_ref).unwrap_or(Unpushed::NeverPushed {
            commits: vec!["(could not check origin)".to_string()],
        })
    };
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
            &head_sha[..head_sha.len().min(7)],
            &tip[..tip.len().min(7)]
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

    let branch = git::current_branch(&path).unwrap_or_else(|_| format!("worktree-{name}"));
    let head_sha = git::head_sha(&path).ok();

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
        if src.exists() {
            std::fs::copy(&src, &dest)
                .with_context(|| format!("copying {} to {}", src.display(), dest.display()))?;
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
    run_worktree_remove_hooks(app, workspace, &path).await;
    git::worktree_remove(&app.cfg.main_checkout, &path)?;

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

#[cfg(test)]
mod tests {

    /// Only `command` hooks, from both repo settings files, in Claude Code's order.
    ///
    /// An `http` hook is Claude Code's to deliver and means nothing outside a
    /// session, so it is skipped rather than guessed at: the daemon's own removal
    /// runs either way, which is what makes skipping safe.
    #[test]
    fn the_repos_worktree_hooks_are_read_from_both_settings_files() {
        let dir = std::env::temp_dir().join(format!("orch-wrhooks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        assert!(worktree_remove_hooks(&dir).is_empty(), "no settings, no hooks");

        std::fs::write(
            dir.join(".claude/settings.json"),
            r#"{"hooks":{"WorktreeRemove":[{"hooks":[
                 {"type":"command","command":"tidy-up"},
                 {"type":"http","url":"http://x/y"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(worktree_remove_hooks(&dir), vec!["tidy-up".to_string()]);
        // Keyed by event, so the create side reads its own and never the other's.
        assert!(repo_worktree_hooks(&dir, "WorktreeCreate").is_empty());

        // Local layers on top of project, the way Claude Code stacks them.
        std::fs::write(
            dir.join(".claude/settings.local.json"),
            r#"{"hooks":{"WorktreeRemove":[{"hooks":[{"type":"command","command":"mine"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(
            worktree_remove_hooks(&dir),
            vec!["tidy-up".to_string(), "mine".to_string()]
        );

        // A repo that declares other hooks but not this one, and a settings file
        // that is not valid JSON, both answer "none" rather than failing teardown.
        std::fs::write(dir.join(".claude/settings.json"), r#"{"hooks":{"SessionStart":[]}}"#).unwrap();
        std::fs::write(dir.join(".claude/settings.local.json"), "{ not json").unwrap();
        assert!(worktree_remove_hooks(&dir).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
    use super::*;

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

/// How long the repo's teardown hook gets. The same budget the setup hooks have:
/// removal is filesystem work, and anything slower than this is stuck rather than
/// busy.
const WORKTREE_REMOVE_TIMEOUT_SECS: u64 = 120;

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

/// The repo's `WorktreeRemove` hooks, in the order Claude Code would run them.
///
/// Read from the repo's own settings rather than the daemon's: this is the
/// *repo's* teardown policy, and the daemon's `--settings` file is where the
/// daemon's own hooks live. Both of Claude Code's repo-level files are consulted,
/// project first then local, which is the layering it uses everywhere else.
///
/// Only `type: "command"` is run. An `http` hook is Claude Code's to deliver and
/// has no meaning outside a session; skipping it silently is right, because the
/// daemon's own removal follows either way.
fn worktree_remove_hooks(main: &std::path::Path) -> Vec<String> {
    repo_worktree_hooks(main, "WorktreeRemove")
}

/// Run the repo's `WorktreeRemove` hooks, then let the daemon's own removal finish
/// the job.
///
/// **The hook first, the daemon's removal always.** A repo that declares one is
/// stating how worktrees go away here, and Claude Code treats that as the whole
/// mechanism. The daemon cannot: it created this tree with `git worktree add`, so
/// it is answerable for the git side whether or not a hook exists and whether or
/// not the hook did anything. `git::worktree_remove` is idempotent for exactly this
/// reason, so the two orders of events (hook removed it, hook did nothing) both end
/// with the tree gone and git's registration pruned.
///
/// **Non-fatal, and does not gate the removal.** A hook that fails must not strand a
/// worktree the preflight already proved safe to remove: the failure is logged with
/// its own stderr and teardown continues. The alternative is a tree nobody can get
/// rid of because a script the daemon does not own returned 1.
///
/// The payload's `worktreePath` and `worktreeName` are the spellings Claude Code's
/// own binary carries. That is the strongest evidence available: the monorepo
/// declares no `WorktreeRemove`, so this has never made a real round trip, and a
/// repo whose hook reads some other key gets a no-op rather than a wrong action.
async fn run_worktree_remove_hooks(app: &Arc<AppState>, workspace: &str, path: &std::path::Path) {
    let hooks = worktree_remove_hooks(&app.cfg.main_checkout);
    if hooks.is_empty() {
        return;
    }
    let payload = serde_json::json!({
        "hook_event_name": "WorktreeRemove",
        "cwd": app.cfg.main_checkout.to_string_lossy(),
        "worktreePath": path.to_string_lossy(),
        "worktreeName": workspace,
    })
    .to_string()
    .into_bytes();

    for cmd in hooks {
        // A shell string, the way Claude Code defines a `command` hook, so `$VAR`
        // and a pipe mean what the author wrote. `CLAUDE_PROJECT_DIR` is exported
        // because repo hooks address themselves through it, this one included.
        let argv = vec!["sh".to_string(), "-c".to_string(), cmd.clone()];
        let at = app.cfg.main_checkout.clone();
        let envs = vec![(
            "CLAUDE_PROJECT_DIR".to_string(),
            app.cfg.main_checkout.to_string_lossy().into_owned(),
        )];
        let body = payload.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::proc::run_bounded_with_input(
                &at,
                WORKTREE_REMOVE_TIMEOUT_SECS,
                &argv,
                "worktree remove hook",
                Some(body),
                &envs,
            )
        })
        .await;
        match result {
            Ok(Ok(out)) if out.status.success() => {
                tracing::info!(workspace, "the repo's WorktreeRemove hook ran");
            }
            Ok(Ok(out)) => tracing::warn!(
                workspace,
                "the repo's WorktreeRemove hook exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
            Ok(Err(e)) => tracing::warn!(workspace, "the repo's WorktreeRemove hook failed: {e:#}"),
            Err(e) => tracing::warn!(workspace, "the WorktreeRemove hook task panicked: {e}"),
        }
    }
}
