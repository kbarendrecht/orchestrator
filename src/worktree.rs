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
        name: "transcript copied",
        passed: copied.0,
        detail: copied.1,
    });

    // 5. Recovery record written.
    let recovery = recovery_recorded(app, workspace).await;
    checks.push(Check {
        name: "recovery record written",
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
    let pf = preflight(app, workspace).await?;
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
    git::worktree_remove(&app.cfg.main_checkout, &path)?;

    {
        let mut inner = app.inner.write().await;
        inner.workspaces.remove(workspace);
        inner.files.remove(workspace);
        for s in inner.sessions.values_mut() {
            if s.workspace == workspace {
                let resumable = !matches!(s.recovery, Some(ArchiveState::TranscriptOnly));
                s.set_state(State::Archived { resumable });
            }
        }
    }
    app.notify().await;
    Ok(pf)
}
