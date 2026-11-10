//! The daemon's own restacks: the upper branch of a stack follows the one under
//! it, a stack behind its base follows it bottom first, and a branch leaves the
//! one under it once that one is merged.
//!
//! **Quiet by default.** It used to ask the user before every pushed restack, in
//! an ask box that folds but never closes, and it told every agent about every
//! stack on its next prompt. Both read as noise to people doing something else in
//! that session, so only what needs nobody is done here, and the rest waits for
//! the moment it matters: the agent's next push (`crate::push`).
//!
//! - **Unpushed**: done, when the trees are idle and clean. Nothing on a remote
//!   changes, so nobody is asked, and the agent hears afterwards on its next
//!   prompt (`Session::restacked`), because its commit ids changed under it.
//! - **Pushed**: never done unasked. The agent restacks when it next pushes, with
//!   the exact command in front of it, or you press the Stacked card's button
//!   (`restack_now`, `rebase_stack_of`), which force-pushes with a lease.
//! - **The lower PR merged**: an unpushed branch moves onto the base by itself,
//!   cut with `--onto` where its own commits start, because a squash merge has a
//!   change no lower commit has; a pushed one is told how at its next push.
//!
//! A restack that stops is aborted, so the tree is always as it was, and the
//! command that finishes it by hand waits for the next push.

use std::path::PathBuf;
use std::sync::Arc;

use crate::model::MergedUnder;
use crate::state::{AppState, Inner};

/// A tree orchd may rebase without asking anybody whether it is in the way: a
/// session in it, none of them busy, measured clean, nothing banked, no rebase
/// stopped part-way. The path and branch, or `None`.
fn quiet_tree(inner: &Inner, workspace: &str) -> Option<(PathBuf, String)> {
    let w = inner.workspaces.get(workspace)?;
    let mut here = inner
        .sessions
        .values()
        .filter(|s| s.workspace == workspace && s.state.is_live());
    let idle = here.clone().next().is_some() && !here.any(|s| s.state.is_busy());
    let clean = w.tree.measured
        && w.tree
            .changed
            .iter()
            .all(|f| !f.staged && !f.unstaged && f.status != "?");
    (idle && clean && w.banked.is_none() && !w.tree.rebasing)
        .then(|| Some((w.path.clone(), w.tree.branch.clone()?)))
        .flatten()
}

/// A tree nobody is changing: no session in it busy (none at all is fine, which
/// is a stack's base in review), clean, nothing banked, no rebase stopped. The
/// path and branch.
fn still_tree(inner: &Inner, workspace: &str) -> Option<(PathBuf, String)> {
    let w = inner.workspaces.get(workspace)?;
    let busy = inner
        .sessions
        .values()
        .any(|s| s.workspace == workspace && s.state.is_busy());
    let clean = w.tree.measured
        && w.tree
            .changed
            .iter()
            .all(|f| !f.staged && !f.unstaged && f.status != "?");
    (!busy && clean && w.banked.is_none() && !w.tree.rebasing)
        .then(|| Some((w.path.clone(), w.tree.branch.clone()?)))
        .flatten()
}

/// Tell every live session in `workspace`, on its next prompt, what orchd did to
/// its branch. Only for something done: its commit ids changed under it.
///
/// Added to what is already waiting rather than over it.
fn tell(inner: &mut Inner, workspace: &str, note: &str) {
    for s in inner
        .sessions
        .values_mut()
        .filter(|s| s.workspace == workspace && s.state.is_live())
    {
        s.restacked = Some(match s.restacked.take() {
            Some(before) if !before.contains(note) => format!("{before}\n\n{note}"),
            _ => note.to_string(),
        });
    }
}

/// Keep `note` for the next push of every live session in `workspace`: what it
/// has to do before that push lands, and nothing it has to do sooner.
pub(crate) fn tell_at_push(inner: &mut Inner, workspace: &str, note: &str) {
    for s in inner
        .sessions
        .values_mut()
        .filter(|s| s.workspace == workspace && s.state.is_live())
    {
        if !s.push_notes.iter().any(|n| n == note) {
            s.push_notes.push(note.to_string());
        }
    }
}

/// The command that finishes a restack by hand: what orchd ran, so the agent
/// does exactly that and settles the files.
pub(crate) fn by_hand(onto: &str, from: Option<&str>) -> String {
    let short = |c: &str| c.get(..10).unwrap_or(c).to_string();
    match from {
        Some(f) => format!("git rebase --onto {} {}", short(onto), short(f)),
        None => format!("git rebase {}", short(onto)),
    }
}

impl AppState {
    /// The base branch's name, which no restack may push to.
    fn base_branch(&self) -> Option<String> {
        self.cfg
            .upstream_ref
            .split_once('/')
            .map(|(_, b)| b.to_string())
    }

    /// Follow the branch under it, for the upper half of every stack
    /// `workspace` is in, when the upper branch is unpushed. One attempt per
    /// lower head, so a rebase that stops is not retried on every refresh.
    ///
    /// **Two reasons to follow**: new commits under it, or a lower branch that
    /// was rewritten. The second has no new commits to count (its subjects are
    /// all carried) and shows only as files the two disagree on, so the cut is
    /// `restack_point`, the upper branch's copy of the last lower commit, and
    /// the old copies are dropped rather than replayed onto their new versions.
    pub(crate) async fn restack_when_safe(&self, workspace: &str) {
        struct Due {
            upper: String,
            path: PathBuf,
            branch: String,
            lower_branch: String,
            stack: crate::overlap::Stack,
        }
        let due: Vec<Due> = {
            let inner = self.inner.read().await;
            let mut due = Vec::new();
            for ((a, b), st) in &inner.stacks {
                let (lower, upper) = if st.lower_is_a { (a, b) } else { (b, a) };
                if lower != workspace && upper != workspace {
                    continue;
                }
                if st.behind == 0 && inner.stack_conflicts(upper, lower) == 0 {
                    continue;
                }
                if inner.stacked_on(upper).map(|(l, _)| l).as_deref() != Some(lower.as_str()) {
                    continue;
                }
                // A stack rebase is moving it already, bottom first.
                if inner.stack_rebasing.contains(upper) {
                    continue;
                }
                let Some((path, branch)) = quiet_tree(&inner, upper) else {
                    continue;
                };
                let lower_branch = inner
                    .workspaces
                    .get(lower)
                    .and_then(|l| l.tree.branch.clone())
                    .unwrap_or_else(|| lower.clone());
                due.push(Due {
                    upper: upper.clone(),
                    path,
                    branch,
                    lower_branch,
                    stack: st.clone(),
                });
            }
            due
        };
        let upstream = self.cfg.upstream_ref.clone();
        for d in due {
            // Pushed is the agent's, at its next push, or a press on the card.
            let (p, b, st, up) = (
                d.path.clone(),
                d.branch.clone(),
                d.stack.clone(),
                upstream.clone(),
            );
            let probe = orchd_base::proc::run_blocking("reading the stack", move || {
                if crate::git::pushed_ref(&p, &b).is_some() {
                    return None;
                }
                Some(crate::overlap::restack_point(&p, &st, &up))
            })
            .await
            .ok()
            .flatten();
            let Some(point) = probe else {
                continue;
            };
            let Due {
                upper,
                path,
                lower_branch,
                stack,
                ..
            } = d;
            let target = stack.lower_tip.clone();
            let what = match stack.behind {
                0 => "was rewritten".to_string(),
                1 => "got 1 new commit".to_string(),
                n => format!("got {n} new commits"),
            };
            {
                let mut inner = self.inner.write().await;
                if inner.restack_tried.get(&upper) == Some(&target) {
                    continue;
                }
                inner.restack_tried.insert(upper.clone(), target.clone());
            }
            let (p, onto, cut) = (path.clone(), target.clone(), point.clone());
            let done = orchd_base::proc::run_blocking("restacking", move || {
                // Checked again on disk, off the runtime: the snapshot is seconds old.
                if crate::git::rebase_in_progress(&p) || !crate::git::is_clean(&p).unwrap_or(false)
                {
                    anyhow::bail!("the tree is not clean any more");
                }
                crate::git::rebase_or_abort(&p, &onto, cut.as_deref())
            })
            .await;
            match done {
                Ok(Ok(())) => {
                    tracing::info!(workspace = %upper, onto = %target, "restacked onto the branch under it");
                    tell(
                        &mut *self.inner.write().await,
                        &upper,
                        &format!(
                            "orchd: while you were idle, `{lower_branch}` (the branch yours is \
                             stacked on) {what}, and orchd rebased your branch onto it. Your \
                             commits are the same; their ids changed."
                        ),
                    );
                    // The snapshot follows with whoever reconciles next; the
                    // reconcile's own callers push one.
                    let _ = Box::pin(self.reconcile(&upper)).await;
                }
                Ok(Err(e)) | Err(e) => {
                    tracing::info!(workspace = %upper, "not restacked, left to the agent: {e:#}");
                    let cmd = by_hand(&target, point.as_deref());
                    tell_at_push(
                        &mut *self.inner.write().await,
                        &upper,
                        &format!(
                            "`{lower_branch}` (the branch yours is stacked on) {what}, and orchd \
                             could not restack your branch onto it: {e:#}. Run `{cmd}`, settle \
                             those files, and continue the rebase."
                        ),
                    );
                }
            }
        }
    }

    /// A stack whose base is behind upstream and merges cleanly, with nothing in
    /// it pushed: rebase it, bottom first, by itself.
    ///
    /// **Only the case with no decision and no remote in it.** A pushed stack is
    /// the agents' at their next push, or the Stacked card's button
    /// (`rebase_stack_of`), and a base that conflicts with upstream is said at
    /// the push of the lowest session working in that stack (`crate::push`).
    /// Once per position of the base against upstream (`stack_rebase_tried`).
    pub(crate) async fn stack_upstream(&self, workspace: &str) {
        let upstream = self.cfg.upstream_ref.clone();
        let rungs = {
            let mut inner = self.inner.write().await;
            let bottom = inner.stack_bottom(workspace);
            let uppers = inner.uppers_of(&bottom);
            if uppers.is_empty() {
                return;
            }
            let Some(w) = inner.workspaces.get(&bottom) else {
                return;
            };
            let behind = w.tree.divergence.0;
            if behind == 0 {
                return;
            }
            let Some((_, files)) = inner.upstream_conflicts.get(&bottom).cloned() else {
                return;
            };
            let stamp = format!("{:?}|{behind}|{}", w.tree.base, files.len());
            if !files.is_empty() || inner.stack_rebase_tried.get(&bottom) == Some(&stamp) {
                return;
            }
            let chain: Vec<String> = std::iter::once(bottom.clone()).chain(uppers).collect();
            // Every branch in it has to hold still, or this waits.
            let mut rungs = Vec::new();
            for ws in &chain {
                match still_tree(&inner, ws) {
                    Some((path, branch)) => rungs.push((ws.clone(), path, branch)),
                    None => return,
                }
            }
            inner.stack_rebase_tried.insert(bottom, stamp);
            rungs
        };
        let (main, up) = (self.cfg.main_checkout.clone(), upstream.clone());
        let probe =
            orchd_base::proc::run_blocking("reading the stack against upstream", move || {
                let onto = crate::git::git(&main, &["rev-parse", &up])
                    .ok()?
                    .trim()
                    .to_string();
                let pushed = rungs
                    .iter()
                    .any(|(_, p, b)| crate::git::pushed_ref(p, b).is_some());
                Some((onto, pushed, rungs))
            })
            .await
            .ok()
            .flatten();
        let Some((onto, false, rungs)) = probe else {
            return;
        };
        let chain: Vec<String> = rungs.into_iter().map(|(ws, _, _)| ws).collect();
        Box::pin(self.rebase_stack(chain, onto)).await;
    }

    /// Rebase a stack onto `onto`, bottom first: each branch onto the new head of
    /// the one under it, cut at its copy of that one (`restack_point`). Pushed
    /// branches go up with a lease. Stops at the first branch that does not land,
    /// and keeps the command for that branch's next push. Answers the workspace
    /// it stopped at, if it did.
    pub(crate) async fn rebase_stack(&self, chain: Vec<String>, onto: String) -> Option<String> {
        let upstream = self.cfg.upstream_ref.clone();
        let base = self.base_branch();
        self.inner
            .write()
            .await
            .stack_rebasing
            .extend(chain.iter().cloned());
        let mut stopped: Option<String> = None;
        for (i, ws) in chain.iter().enumerate() {
            let rung = {
                let inner = self.inner.read().await;
                let lower = (i > 0)
                    .then(|| inner.stacked_on(ws))
                    .flatten()
                    .and_then(|(l, st)| Some((inner.workspaces.get(&l)?.path.clone(), st.clone())));
                still_tree(&inner, ws).map(|(p, b)| (p, b, lower))
            };
            let Some((path, branch, lower)) = rung else {
                stopped = Some(ws.clone());
                break;
            };
            let (p, b, up, base, first) = (
                path.clone(),
                branch.clone(),
                upstream.clone(),
                base.clone(),
                onto.clone(),
            );
            let done = orchd_base::proc::run_blocking("rebasing a stack", move || {
                let (target, from) = match &lower {
                    None => (first, None),
                    Some((lower_path, st)) => (
                        crate::git::git(lower_path, &["rev-parse", "HEAD"])?
                            .trim()
                            .to_string(),
                        crate::overlap::restack_point(&p, st, &up),
                    ),
                };
                let cmd = by_hand(&target, from.as_deref());
                let landed = if crate::git::pushed_ref(&p, &b).is_some() {
                    crate::git::restack_pushed(&p, &b, &target, from.as_deref(), base.as_deref())
                        .map(|_| ())
                } else {
                    crate::git::rebase_or_abort(&p, &target, from.as_deref())
                };
                Ok::<_, anyhow::Error>(landed.map_err(|e| (cmd, e)))
            })
            .await
            .and_then(|r| r);
            match done {
                Ok(Ok(())) => {
                    let _ = Box::pin(self.reconcile(ws)).await;
                }
                Ok(Err((cmd, e))) => {
                    tracing::info!(workspace = %ws, "stack rebase stopped: {e:#}");
                    tell_at_push(
                        &mut *self.inner.write().await,
                        ws,
                        &format!(
                            "the stack was rebased onto {upstream}, bottom first, and stopped at \
                             your branch: {e:#}. Run `{cmd}`, settle those files, and continue the \
                             rebase; the branches above yours follow once it has moved."
                        ),
                    );
                    stopped = Some(ws.clone());
                    break;
                }
                Err(e) => {
                    tracing::info!(workspace = %ws, "stack rebase could not run: {e:#}");
                    stopped = Some(ws.clone());
                    break;
                }
            }
        }
        let mut inner = self.inner.write().await;
        for ws in &chain {
            inner.stack_rebasing.remove(ws);
        }
        // Said to the branches that moved: their ids changed.
        let note = format!(
            "orchd: your stack was rebased onto {upstream}, bottom first, yours included. Your \
             commits are the same; their ids changed."
        );
        for ws in &chain {
            if Some(ws) == stopped.as_ref() {
                break;
            }
            tell(&mut inner, ws, &note);
        }
        stopped
    }

    /// The Stacked card's "Restack now": move `workspace` onto the branch under
    /// it, pushed or not, because you asked for it. A pushed branch follows what
    /// the lower branch pushed, and goes up with a lease.
    pub async fn restack_now(self: &Arc<Self>, workspace: &str) -> anyhow::Result<()> {
        let (path, branch, lower_branch, stack) = {
            let inner = self.inner.read().await;
            let (lower, stack) = inner
                .stacked_on(workspace)
                .ok_or_else(|| anyhow::anyhow!("this branch is not stacked on another"))?;
            let (path, branch) = still_tree(&inner, workspace).ok_or_else(|| {
                anyhow::anyhow!("its tree is busy or not clean; let the session finish first")
            })?;
            let lower_branch = inner
                .workspaces
                .get(&lower)
                .and_then(|l| l.tree.branch.clone())
                .unwrap_or(lower);
            (path, branch, lower_branch, stack.clone())
        };
        let (base, up) = (self.base_branch(), self.cfg.upstream_ref.clone());
        let lb = lower_branch.clone();
        orchd_base::proc::run_blocking("restacking", move || {
            let point = crate::overlap::restack_point(&path, &stack, &up);
            if crate::git::pushed_ref(&path, &branch).is_some() {
                let onto = crate::overlap::pushed_lower_ahead(&path, &lb)
                    .unwrap_or_else(|| stack.lower_tip.clone());
                crate::git::restack_pushed(&path, &branch, &onto, point.as_deref(), base.as_deref())
                    .map(|_| ())
            } else {
                crate::git::rebase_or_abort(&path, &stack.lower_tip, point.as_deref())
            }
        })
        .await??;
        tell(
            &mut *self.inner.write().await,
            workspace,
            &format!(
                "orchd: the user restacked your branch onto `{lower_branch}`. Your commits are the \
                 same; their ids changed."
            ),
        );
        let _ = self.reconcile(workspace).await;
        self.notify().await;
        Ok(())
    }

    /// The Stacked card's "Rebase the stack": the whole stack `workspace` is in,
    /// onto the upstream ref, bottom first, because you asked for it.
    pub async fn rebase_stack_of(self: &Arc<Self>, workspace: &str) -> anyhow::Result<()> {
        let chain = {
            let inner = self.inner.read().await;
            let bottom = inner.stack_bottom(workspace);
            let chain: Vec<String> = std::iter::once(bottom.clone())
                .chain(inner.uppers_of(&bottom))
                .collect();
            if let Some(busy) = chain.iter().find(|ws| still_tree(&inner, ws).is_none()) {
                anyhow::bail!("`{busy}` is busy or not clean; let it finish first");
            }
            chain
        };
        let (main, up) = (
            self.cfg.main_checkout.clone(),
            self.cfg.upstream_ref.clone(),
        );
        let onto = orchd_base::proc::run_blocking("reading the upstream ref", move || {
            anyhow::Ok(
                crate::git::git(&main, &["rev-parse", &up])?
                    .trim()
                    .to_string(),
            )
        })
        .await??;
        let stopped = self.rebase_stack(chain, onto).await;
        self.notify().await;
        match stopped {
            None => Ok(()),
            Some(at) => anyhow::bail!(
                "stopped at `{at}`: git could not rebase it, and its agent gets the command at its next push"
            ),
        }
    }

    /// Move every branch that was stacked on a PR that just merged onto the base.
    ///
    /// Queued by the PR poll (`Inner::tell_overlaps_merged`) and run here, off
    /// its lock, because it fetches. Unpushed and quiet is done; anything else,
    /// or a rebase that stops, is the agent's at its next push, with the command.
    pub async fn restack_after_merge(&self) {
        let pending: Vec<MergedUnder> = std::mem::take(&mut self.inner.write().await.merged_under);
        if pending.is_empty() {
            return;
        }
        let (main, base) = (self.cfg.main_checkout.clone(), self.cfg.base());
        if let Err(e) =
            orchd_base::proc::run_blocking("the upstream fetch", move || base.fetch(&main))
                .await
                .and_then(|r| r)
        {
            tracing::warn!(
                "restack after a merge: upstream fetch failed, using the last-known base: {e:#}"
            );
        }
        let upstream = self.cfg.upstream_ref.clone();
        for m in pending {
            let (path, branch, quiet) = {
                let inner = self.inner.read().await;
                let Some(w) = inner.workspaces.get(&m.upper) else {
                    continue;
                };
                match quiet_tree(&inner, &m.upper) {
                    Some((p, b)) => (p, b, true),
                    None => (
                        w.path.clone(),
                        w.tree.branch.clone().unwrap_or_default(),
                        false,
                    ),
                }
            };
            let (p, st, up, b) = (
                path.clone(),
                m.stack.clone(),
                upstream.clone(),
                branch.clone(),
            );
            let probe = orchd_base::proc::run_blocking("reading the stack", move || {
                let point = crate::overlap::restack_point(&p, &st, &up);
                let pushed = crate::git::pushed_ref(&p, &b).is_some();
                let onto = crate::git::git(&p, &["rev-parse", &up])
                    .ok()
                    .map(|s| s.trim().to_string());
                (point, pushed, onto)
            })
            .await;
            let Ok((point, pushed, onto)) = probe else {
                continue;
            };
            let n = m.number;
            let (Some(point), Some(onto)) = (point, onto) else {
                tell_at_push(
                    &mut *self.inner.write().await,
                    &m.upper,
                    &format!(
                        "PR #{n}, the one your branch is stacked on, merged, and orchd could not \
                         tell where your own commits start: move your branch onto {upstream} \
                         yourself, leaving #{n}'s commits out (they are in {upstream} now)."
                    ),
                );
                continue;
            };
            let cmd = format!(
                "git rebase --onto {upstream} {}",
                point.get(..10).unwrap_or(&point)
            );
            let by_hand_note = format!(
                "PR #{n}, the one your branch is stacked on, merged. Move your branch onto \
                 {upstream} without its commits: `{cmd}`."
            );
            if !quiet || pushed {
                tell_at_push(&mut *self.inner.write().await, &m.upper, &by_hand_note);
                continue;
            }
            let (p, o, f) = (path.clone(), onto.clone(), point.clone());
            let done = orchd_base::proc::run_blocking("restacking after a merge", move || {
                crate::git::rebase_or_abort(&p, &o, Some(&f))
            })
            .await
            .and_then(|r| r);
            let mut inner = self.inner.write().await;
            match done {
                Ok(()) => tell(
                    &mut inner,
                    &m.upper,
                    &format!(
                        "orchd: PR #{n}, the one your branch was stacked on, merged, and orchd moved \
                         your branch onto {upstream} without its commits. Your own commits are the \
                         same; their ids changed."
                    ),
                ),
                Err(e) => {
                    tracing::info!(workspace = %m.upper, "not restacked after #{n}: {e:#}");
                    tell_at_push(
                        &mut inner,
                        &m.upper,
                        &format!("{by_hand_note} orchd tried, and it stopped on a conflict: {e:#}."),
                    );
                }
            }
            drop(inner);
            let _ = self.reconcile(&m.upper).await;
        }
    }
}
