//! Restacks, done only when you ask, and the notes that let the agents do the
//! rest themselves.
//!
//! **Nothing here runs by itself.** It used to restack an idle unpushed branch,
//! rebase a stack behind its base, and move a branch off a merged PR, all
//! unasked; before that it asked about every pushed restack in a box that never
//! closed. Both meant somebody's branch moving or a question appearing while they
//! were doing something else. A stack is the agents' and yours now, like a
//! conflict: each agent hears what it needs before it pushes (`crate::push`),
//! where its branch sits in the stack, what to restack and what a rewrite does
//! to the branches above it, and the Stacked card has the two buttons
//! (`restack_now`, `rebase_stack_of`) for when you want it done now.
//!
//! **One exception, because it is mechanical** (`restack_when_safe`): the branch
//! under an unpushed, idle, clean one only grew, and which one is under which was
//! proved by containment. Then `git rebase --onto <new head> <old head>` replays
//! exactly the upper branch's own commits, with no guessing, after a backup ref.
//!
//! A restack that stops is aborted, so the tree is always as it was, and the
//! command that finishes it by hand waits for that branch's next push.

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

    /// The one restack orchd does unasked: the branch under an unpushed, idle,
    /// clean branch only grew, so its own commits are replayed onto the new head.
    ///
    /// **Every condition is a fact, none a guess.** Which branch is under which
    /// was proved by containment and remembered (`Inner::stack_dirs`), never read
    /// from the branches' ages; the old lower head is in both branches, so the
    /// lower one moved forward rather than being rewritten; the cut is that old
    /// head, not a commit matched by subject. The upper branch is unpushed, so no
    /// remote changes, and idle and clean by the daemon's view and again on disk.
    /// A backup ref (`refs/orchd/restack/<branch>`) is written first, so even a
    /// wrong idle guess is one `git reset` from undone. Anything else, a rewritten
    /// lower branch above all, is the agent's at its next push (`crate::push`).
    /// One attempt per lower head.
    pub(crate) async fn restack_when_safe(&self, workspace: &str) {
        let due: Vec<(String, PathBuf, String, String, String, String)> = {
            let inner = self.inner.read().await;
            let mut due = Vec::new();
            for ((a, b), st) in &inner.stacks {
                let (lower, upper) = if st.lower_is_a { (a, b) } else { (b, a) };
                if (lower != workspace && upper != workspace) || st.behind == 0 {
                    continue;
                }
                if inner.stacked_on(upper).map(|(l, _)| l).as_deref() != Some(lower.as_str())
                    || inner.stack_rebasing.contains(upper)
                    || inner.restack_tried.get(upper) == Some(&st.lower_tip)
                {
                    continue;
                }
                let Some((path, branch)) = quiet_tree(&inner, upper) else {
                    continue;
                };
                let Some(lower_branch) = inner
                    .workspaces
                    .get(lower)
                    .and_then(|l| l.tree.branch.clone())
                else {
                    continue;
                };
                let pair = if branch < lower_branch {
                    (branch.clone(), lower_branch.clone())
                } else {
                    (lower_branch.clone(), branch.clone())
                };
                let proved = inner
                    .stack_dirs
                    .as_ref()
                    .and_then(|d| d.get(&pair))
                    .is_some_and(|l| *l == lower_branch);
                if !proved || st.anchor.is_empty() {
                    continue;
                }
                due.push((
                    upper.clone(),
                    path,
                    branch,
                    lower_branch,
                    st.anchor.clone(),
                    st.lower_tip.clone(),
                ));
            }
            due
        };
        for (upper, path, branch, lower_branch, old, new) in due {
            self.inner
                .write()
                .await
                .restack_tried
                .insert(upper.clone(), new.clone());
            let (p, b, o, n) = (path.clone(), branch.clone(), old.clone(), new.clone());
            let done = orchd_base::proc::run_blocking("restacking", move || {
                let within = |x: &str, y: &str| {
                    crate::git::git(&p, &["merge-base", "--is-ancestor", x, y]).is_ok()
                };
                // The lower branch grew from `old`, and this one is built on it.
                if !within(&o, &n) || !within(&o, "HEAD") {
                    anyhow::bail!("the branch under it was rewritten, not only grown");
                }
                if crate::git::pushed_ref(&p, &b).is_some() {
                    anyhow::bail!("the branch is pushed");
                }
                if crate::git::rebase_in_progress(&p) || !crate::git::is_clean(&p).unwrap_or(false)
                {
                    anyhow::bail!("the tree is not clean any more");
                }
                crate::git::git(
                    &p,
                    &["update-ref", &format!("refs/orchd/restack/{b}"), "HEAD"],
                )?;
                crate::git::rebase_or_abort(&p, &n, Some(&o))
            })
            .await
            .and_then(|r| r);
            match done {
                Ok(()) => {
                    tracing::info!(workspace = %upper, onto = %new, "restacked onto the branch under it");
                    tell(
                        &mut *self.inner.write().await,
                        &upper,
                        &format!(
                            "orchd: while you were idle, `{lower_branch}` (the branch yours is \
                             stacked on) got new commits, and orchd replayed your own commits onto \
                             it. Their ids changed; the branch as it was is \
                             `refs/orchd/restack/{branch}`."
                        ),
                    );
                    let _ = Box::pin(self.reconcile(&upper)).await;
                }
                // Not done is not a failure: the push note says what to do.
                Err(e) => tracing::info!(workspace = %upper, "not restacked by itself: {e:#}"),
            }
        }
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

    /// Tell every branch that was stacked on a PR that just merged how to move
    /// onto the base, at its next push.
    ///
    /// Queued by the PR poll (`Inner::tell_overlaps_merged`) and read here, off
    /// its lock, because the cut needs git. The command is `--onto` the base,
    /// cut where the branch's own commits start: a squash merge has a change no
    /// lower commit has, so a plain rebase would replay them all.
    pub async fn restack_after_merge(&self) {
        let pending: Vec<MergedUnder> = std::mem::take(&mut self.inner.write().await.merged_under);
        let upstream = self.cfg.upstream_ref.clone();
        for m in pending {
            let Some(path) = self
                .inner
                .read()
                .await
                .workspaces
                .get(&m.upper)
                .map(|w| w.path.clone())
            else {
                continue;
            };
            let (st, up) = (m.stack.clone(), upstream.clone());
            let point = orchd_base::proc::run_blocking("reading the stack", move || {
                crate::overlap::restack_point(&path, &st, &up)
            })
            .await
            .ok()
            .flatten();
            let n = m.number;
            let note = match point {
                Some(point) => format!(
                    "PR #{n}, the one your branch is stacked on, merged. Move your branch onto \
                     {upstream} without its commits before you push: `git rebase --onto {upstream} {}`, \
                     then push with --force-with-lease. A PR of yours that targeted its branch now \
                     targets the base.",
                    point.get(..10).unwrap_or(&point)
                ),
                None => format!(
                    "PR #{n}, the one your branch is stacked on, merged, and orchd could not tell \
                     where your own commits start: move your branch onto {upstream} yourself before \
                     you push, leaving #{n}'s commits out (they are in {upstream} now)."
                ),
            };
            tell_at_push(&mut *self.inner.write().await, &m.upper, &note);
        }
    }
}
