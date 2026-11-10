//! The daemon's own restacks: the upper branch of a stack follows the one under
//! it, and leaves it once that one is merged.
//!
//! **Restacking used to be the agents' job alone** (`TODO.md`), and most of it
//! still is: a restack that stops on a conflict is theirs to settle. What moved
//! here is the case with no decision in it, which is a `git rebase` somebody had
//! to remember to ask for. Three paths, one rule each:
//!
//! - **Unpushed**: done, when the upper session is idle and its tree clean. Nothing
//!   on a remote changes, so nobody is asked.
//! - **Pushed**: asked first, on the upper session, because a yes force-pushes
//!   (with a lease) under a PR somebody may be reviewing. Only what the lower
//!   branch *pushed* is followed: its unpushed commits are in no PR, and would show
//!   up in the upper one.
//! - **The lower PR merged**: the upper branch moves onto the base with `--onto`,
//!   cut where its own commits start, because a squash merge has a change no lower
//!   commit has and a plain rebase replays them all.
//!
//! Either way the agent hears what happened on its next prompt
//! (`Session::restacked`): its commit ids changed under it, and a `--fixup` or a
//! remembered sha would otherwise fail with nothing to explain why.

use std::path::PathBuf;
use std::sync::Arc;

use crate::model::{Interaction, InteractionOption, MergedUnder, RestackAsk, State, TurnReason};
use crate::state::{AppState, Inner};

/// The answer that restacks and force-pushes. Compared together with the ask id,
/// so an agent writing the same value into its own `orch ask` grants nothing.
pub const RESTACK_YES: &str = "restack-push";
const RESTACK_NO: &str = "restack-leave";

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

/// Say `note` to every live session in `workspace`, on its next prompt.
fn tell(inner: &mut Inner, workspace: &str, note: &str) {
    for s in inner
        .sessions
        .values_mut()
        .filter(|s| s.workspace == workspace && s.state.is_live())
    {
        s.restacked = Some(note.to_string());
    }
}

/// Put a restack question on a live session in `workspace`. `false` when one
/// of them is already asking something, which is the agent's own question and
/// wins; the caller asks again on a later refresh.
fn ask(
    inner: &mut Inner,
    workspace: &str,
    question: String,
    detail: String,
    what: RestackAsk,
) -> bool {
    let Some(s) = inner
        .sessions
        .values_mut()
        .find(|s| s.workspace == workspace && s.state.is_live())
    else {
        return false;
    };
    if s.interaction.as_ref().is_some_and(|i| i.answer.is_none()) {
        return false;
    }
    s.interaction = Some(Interaction {
        id: what.id,
        thread_id: None,
        question,
        detail: Some(detail),
        options: vec![
            InteractionOption {
                value: RESTACK_YES.to_string(),
                label: "Restack and push".to_string(),
                sub: "rebase, then force-push with a lease".to_string(),
                free: false,
            },
            InteractionOption {
                value: RESTACK_NO.to_string(),
                label: "Leave it to the agent".to_string(),
                sub: "it is told on its next prompt".to_string(),
                free: false,
            },
        ],
        asked_at: std::time::SystemTime::now(),
        answer: None,
        answer_text: None,
    });
    s.restack_ask = Some(RestackAsk {
        before: s.state.clone(),
        ..what
    });
    s.set_state(State::YourTurn {
        since: std::time::SystemTime::now(),
        reason: TurnReason::AskedAQuestion,
    });
    true
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
    /// `workspace` is in: done when unpushed, asked when pushed. One attempt per
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
                let Some((path, branch)) = quiet_tree(&inner, upper) else {
                    continue;
                };
                // A PR orchd may not push to is the agent's, whatever else holds.
                if inner
                    .prs
                    .iter()
                    .any(|p| p.head_ref == branch && p.head_pushable == Some(false))
                {
                    continue;
                }
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
            // Pushed or not decides what is followed: the lower head, or what the
            // lower branch pushed. The cut is read in the same trip.
            let (p, b, lb, st, up) = (
                d.path.clone(),
                d.branch.clone(),
                d.lower_branch.clone(),
                d.stack.clone(),
                upstream.clone(),
            );
            let probe = orchd_base::proc::run_blocking("reading the stack", move || {
                let point = crate::overlap::restack_point(&p, &st, &up);
                let target = match crate::git::pushed_ref(&p, &b) {
                    None => Some((false, st.lower_tip.clone())),
                    Some(_) => crate::overlap::pushed_lower_ahead(&p, &lb).map(|t| (true, t)),
                };
                target.map(|t| (t, point))
            })
            .await
            .ok()
            .flatten();
            let Some(((pushed, target), point)) = probe else {
                continue;
            };
            let Due {
                upper,
                path,
                branch,
                lower_branch,
                stack,
            } = d;
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
                if pushed {
                    let pr = inner
                        .prs
                        .iter()
                        .find(|p| p.head_ref == branch)
                        .map(|p| format!(" PR #{} is on it, and CI runs again.", p.number))
                        .unwrap_or_default();
                    let asked = ask(
                        &mut inner,
                        &upper,
                        format!("`{lower_branch}`, the branch this one is stacked on, {what}. Restack `{branch}` onto it and force-push it?"),
                        format!(
                            "The push uses --force-with-lease, so a push somebody else made in the \
                             meantime is never overwritten, and if the rebase stops nothing changes.{pr}"
                        ),
                        RestackAsk {
                            id: uuid::Uuid::new_v4(),
                            onto: target.clone(),
                            from: point,
                            branch: branch.clone(),
                            onto_name: format!("`{lower_branch}`"),
                            before: State::Exited,
                        },
                    );
                    if !asked {
                        // Somebody else's question is open: ask on a later refresh.
                        inner.restack_tried.remove(&upper);
                    }
                    continue;
                }
            }
            let (p, onto) = (path.clone(), target.clone());
            let done = orchd_base::proc::run_blocking("restacking", move || {
                // Checked again on disk, off the runtime: the snapshot is seconds old.
                if crate::git::rebase_in_progress(&p) || !crate::git::is_clean(&p).unwrap_or(false)
                {
                    anyhow::bail!("the tree is not clean any more");
                }
                crate::git::rebase_or_abort(&p, &onto, point.as_deref())
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
                }
            }
        }
    }

    /// Move every branch that was stacked on a PR that just merged onto the base.
    ///
    /// Queued by the PR poll (`Inner::tell_overlaps_merged`) and run here, off
    /// its lock, because it fetches. Unpushed and quiet is done; pushed and
    /// quiet is asked; anything else, or a rebase that stops, is the agent's,
    /// and its next prompt carries the exact command.
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
                tell(
                    &mut *self.inner.write().await,
                    &m.upper,
                    &format!(
                        "orchd: PR #{n}, the one your branch is stacked on, merged. orchd could not \
                         tell where your own commits start, so move your branch onto {upstream} \
                         yourself, leaving #{n}'s commits out (they are in {upstream} now)."
                    ),
                );
                continue;
            };
            let short = point.get(..10).unwrap_or(&point).to_string();
            let by_hand = format!(
                "orchd: PR #{n}, the one your branch is stacked on, merged. Move your branch onto \
                 {upstream} without its commits: `git rebase --onto {upstream} {short}`."
            );
            if !quiet {
                tell(&mut *self.inner.write().await, &m.upper, &by_hand);
                continue;
            }
            if pushed {
                let mut inner = self.inner.write().await;
                let asked = ask(
                    &mut inner,
                    &m.upper,
                    format!("PR #{n}, the one `{branch}` is stacked on, merged. Move `{branch}` onto {upstream} and force-push it?"),
                    format!(
                        "It drops #{n}'s commits, which are in the base now, and keeps this \
                         branch's own. The push uses --force-with-lease, and if the rebase stops \
                         nothing changes."
                    ),
                    RestackAsk {
                        id: uuid::Uuid::new_v4(),
                        onto,
                        from: Some(point),
                        branch: branch.clone(),
                        onto_name: upstream.clone(),
                        before: State::Exited,
                    },
                );
                if !asked {
                    tell(&mut inner, &m.upper, &by_hand);
                }
                continue;
            }
            let (p, o, f) = (path.clone(), onto.clone(), point.clone());
            let done = orchd_base::proc::run_blocking("restacking after a merge", move || {
                crate::git::rebase_or_abort(&p, &o, Some(&f))
            })
            .await
            .and_then(|r| r);
            let note = match done {
                Ok(()) => format!(
                    "orchd: PR #{n}, the one your branch was stacked on, merged, and orchd moved \
                     your branch onto {upstream} without its commits. Your own commits are the \
                     same; their ids changed."
                ),
                Err(e) => {
                    tracing::info!(workspace = %m.upper, "not restacked after #{n}: {e:#}");
                    format!("{by_hand} orchd tried and it stopped on a conflict, so it is yours.")
                }
            };
            tell(&mut *self.inner.write().await, &m.upper, &note);
            let _ = self.reconcile(&m.upper).await;
        }
    }

    /// The user said yes to a restack question: do it, pushed, and say how it went.
    pub async fn answer_restack(
        self: &Arc<Self>,
        session: crate::model::SessionId,
        what: RestackAsk,
    ) {
        let Some((workspace, path)) = ({
            let inner = self.inner.read().await;
            inner.sessions.get(&session).and_then(|s| {
                let w = inner.workspaces.get(&s.workspace)?;
                Some((w.id.clone(), w.path.clone()))
            })
        }) else {
            return;
        };
        let base = self.base_branch();
        let (p, w) = (path.clone(), what.clone());
        let done = orchd_base::proc::run_blocking("restacking a pushed branch", move || {
            crate::git::restack_pushed(&p, &w.branch, &w.onto, w.from.as_deref(), base.as_deref())
        })
        .await
        .and_then(|r| r);
        let onto = &what.onto_name;
        let note = match done {
            Ok(_) => format!(
                "orchd: the user agreed, and orchd rebased your branch onto {onto} and \
                 force-pushed it. Your commits are the same; their ids changed, here and on origin."
            ),
            Err(e) => {
                tracing::info!(%workspace, "restack the user agreed to did not land: {e:#}");
                format!(
                    "orchd: the user agreed to restack your branch onto {onto} and force-push it, \
                     and it did not work: {e:#}. Your branch is as it was; restack it yourself \
                     when you can."
                )
            }
        };
        tell(&mut *self.inner.write().await, &workspace, &note);
        let _ = self.reconcile(&workspace).await;
        self.notify().await;
    }
}
