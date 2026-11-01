//! Where a PR's branch is: the one answer every PR flow starts from.
//!
//! **One question, which twelve functions used to answer a part of each.** The
//! tree holding the branch, whether main is on it, whether the PR's head is even
//! this checkout's branch of that name — asked by `workspace_for`,
//! `worktree_holding`, `main_is_on`, `pr_head`, `pr_tree`, `recorded_worktree_for`
//! and their callers, from two sources, in an order every caller had to know. The
//! sources disagree for up to two seconds: `Tree::branch` is the head poller's
//! cache, git is the truth. That gap cost two races in one day, one each way —
//! a press refused because the cache had not seen main switch onto the branch,
//! and a fix-pr typed into main after main had switched off it.
//!
//! The order is the rule, and it lives here: someone else's branch first, because
//! no lookup by name means anything for it; then a worktree, from the record,
//! because nothing is cut from a worktree and the record is what names its tree
//! even when the directory is gone; then main, **from git**, because main is the
//! checkout every worktree is cut from and the cache is the one thing that may not
//! answer for it.

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

use crate::state::AppState;

/// Where a PR's branch is.
#[derive(Debug, Clone)]
pub enum PrPlace {
    /// The PR's head is another repository's branch of that name, or a local
    /// namesake that tracks another remote, with the sentence that says so.
    Foreign(String),
    /// A worktree holds the branch, by the daemon's record. `path` may be gone:
    /// a torn-down tree keeps its name and place, and is cut again where it stood.
    Worktree { ws: String, path: PathBuf },
    /// The main checkout has the branch checked out, by git.
    Main,
    /// Nobody holds it.
    Nowhere,
}

/// [`PrPlace`], and what cutting a tree for it would need.
#[derive(Debug, Clone)]
pub struct Located {
    pub place: PrPlace,
    /// The remote to fetch the PR's head from when the checkout has no local copy
    /// of it. See `git::HeadCheck::fetch_from`.
    pub from: Option<String>,
}

/// Where PR `pr`, whose head is `head_ref`, has its branch. A PR not in the poll
/// is taken as this checkout's own branch, which is what a press on a row the
/// poll has just dropped means.
pub async fn locate_pr(app: &Arc<AppState>, pr: u64, head_ref: &str) -> Result<Located> {
    let head = app.pr_head(pr).await;
    let place = if let Some(why) = head.refusal {
        PrPlace::Foreign(why)
    } else if let Some((ws, path)) = recorded_tree(app, head_ref).await {
        PrPlace::Worktree { ws, path }
    } else if main_is_on(app, head_ref).await? {
        PrPlace::Main
    } else {
        PrPlace::Nowhere
    };
    Ok(Located {
        place,
        from: head.from,
    })
}

/// The worktree the record says holds `head_ref`, with its path.
///
/// **The record, not the directory.** A torn-down tree keeps its name and path
/// here, and a PR flow cuts it again where it stood rather than a second one
/// somewhere else: transcripts are keyed by working directory, so a conversation
/// resumed later looks for its own path, and two trees on one branch is a choice
/// nothing should have to make. Only ever a worktree: main is git's to answer.
async fn recorded_tree(app: &Arc<AppState>, head_ref: &str) -> Option<(String, PathBuf)> {
    let ws = app.worktree_holding(head_ref).await?;
    let path = app.workspace_path(&ws).await?;
    Some((ws, path))
}

/// Whether the main checkout has this branch checked out right now, by git.
///
async fn main_is_on(app: &Arc<AppState>, head_ref: &str) -> Result<bool> {
    let main = app.cfg.main_checkout.clone();
    let on = crate::proc::run_blocking("reading main's branch", move || {
        crate::git::current_branch(&main)
    })
    .await?;
    Ok(on.as_deref().ok() == Some(head_ref))
}
