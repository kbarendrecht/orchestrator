//! What an agent hears when it pushes or opens a PR, and nothing sooner.
//!
//! **The moment a conflict matters is the push.** Conflicts, stacks and the
//! upstream ref used to be appended to every next prompt, and a session that was
//! only reading code was told to negotiate with another one about work that had
//! nothing to do with it. A conflict between two branches is a fact about their
//! merge; until somebody pushes, nothing has to happen about it, and two sessions
//! should be able to work without hearing about each other at all.
//!
//! So `git push` and `gh pr create` are held once, by the `pre-edit` hook, with
//! everything that push should know in one note: the restack the branch needs, a
//! stack base that has fallen behind, the upstream ref it will not merge with, the
//! sessions it conflicts with and whose PR goes first, the `--base` of a stacked
//! PR, and what orchd kept for it (a merge under its stack, a restack that
//! stopped). The retry goes through.
//!
//! **Once per point, not once per push.** The note is built without side effects,
//! so the retry builds the same one and is let through; only then are its points
//! marked said (`Session::push_said`), and a later push is held again only for a
//! point the agent has not heard.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

use crate::model::SessionId;
use crate::state::{AppState, Inner, Progress};

/// Whether `command` pushes a branch or opens a PR: the two moments a conflict
/// starts to matter.
fn is_push(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    words.windows(2).any(|w| w == ["git", "push"]) || command.contains("gh pr create")
}

fn hash_of(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// What a stacked PR should target, when `command` opens one without saying.
///
/// **A stacked PR targets the branch under it**, so it shows only its own
/// commits, and GitHub moves it onto the base when the lower PR merges and its
/// branch is deleted. That needs both branches in the repository the PR is in:
/// from a fork, a base cannot name the fork's branch, and the next best thing is
/// a description that says what to review first.
fn pr_base_hint(inner: &Inner, workspace: &str, command: &str) -> Option<String> {
    if !command.contains("gh pr create") {
        return None;
    }
    let (lower, _) = inner.stacked_on(workspace)?;
    let branch = inner.workspaces.get(&lower)?.tree.branch.clone()?;
    let words: Vec<&str> = command.split_whitespace().collect();
    let names_it = words.windows(2).any(|w| {
        matches!(w, [flag, b] if (*flag == "--base" || *flag == "-B") && b.trim_matches(['"', '\'']) == branch)
    }) || words.contains(&format!("--base={branch}").as_str());
    if names_it {
        return None;
    }
    let pr = inner
        .prs
        .iter()
        .find(|p| p.head_ref == branch && !inner.foreign_heads.contains_key(&p.number))
        .map(|p| {
            // `https://github.com/<owner>/<name>/pull/<n>`: the repository the PR is in.
            let home = p
                .url
                .split('/')
                .skip(3)
                .take(2)
                .collect::<Vec<_>>()
                .join("/");
            (p.number, p.head_repo.as_deref() == Some(home.as_str()))
        });
    Some(match pr {
        Some((n, true)) => format!(
            "your branch is stacked on `{branch}` (PR #{n}): open this PR with `--base {branch}`, \
             so it shows only your own commits; GitHub moves it onto the base when #{n} merges and \
             its branch is deleted."
        ),
        Some((n, false)) => format!(
            "your branch is stacked on `{branch}` (PR #{n}), which is in a fork, so this PR cannot \
             target it and will show #{n}'s commits until #{n} merges: start the description with \
             \"Stacked on #{n}, review that one first.\""
        ),
        None => format!(
            "your branch is stacked on `{branch}`, which has no PR yet: open that one first, then \
             target it with `--base {branch}` if both branches are in the same repository, or name \
             it at the top of this description if not."
        ),
    })
}

/// The points of a note, before any are filtered as said: each one a sentence
/// that stands alone, and what saying it marks.
struct Points {
    items: Vec<String>,
    /// Conflicting files and their other workspace, marked told for the card.
    conflicts: Vec<(String, String)>,
    /// The upstream files, marked told for the card.
    upstream: Vec<String>,
}

impl AppState {
    /// Everything `session` should know before it pushes, as points, or `None`
    /// when it is not a session. `command` adds what only an opened PR needs.
    async fn push_points(&self, session: SessionId, command: &str) -> Option<Points> {
        let upstream = self.cfg.upstream_ref.clone();
        // Read under the lock; the one git question (the restack's cut) after it.
        let (mut items, conflicts, own_upstream, restack) = {
            let inner = self.inner.read().await;
            let s = inner.sessions.get(&session)?;
            let ws = s.workspace.clone();
            let w = inner.workspaces.get(&ws)?;
            let mut items: Vec<String> = Vec::new();

            // The stack: a restack it needs, and its base against upstream.
            let stacked = inner.stacked_on(&ws).map(|(lower, st)| (lower, st.clone()));
            let restack = stacked.as_ref().and_then(|(lower, st)| {
                let stale = inner.stack_conflicts(&ws, lower) > 0;
                (st.behind > 0 || stale).then(|| {
                    let lw = inner.workspaces.get(lower);
                    (
                        lw.and_then(|l| l.tree.branch.clone())
                            .unwrap_or_else(|| lower.clone()),
                        st.clone(),
                        w.path.clone(),
                        w.tree.branch.clone().unwrap_or_default(),
                    )
                })
            });
            if stacked.is_some() {
                let bottom = inner.stack_bottom(&ws);
                let chain: Vec<String> = std::iter::once(bottom.clone())
                    .chain(inner.uppers_of(&bottom))
                    .collect();
                let lowest_live = chain.iter().find(|x| inner.has_live_session(x));
                let base_files = inner
                    .upstream_conflicts
                    .get(&bottom)
                    .map(|(_, f)| f.clone())
                    .unwrap_or_default();
                if lowest_live == Some(&ws)
                    && !inner.has_live_session(&bottom)
                    && !base_files.is_empty()
                {
                    let base = inner.workspaces.get(&bottom);
                    let branch = base
                        .and_then(|b| b.tree.branch.clone())
                        .unwrap_or_else(|| bottom.clone());
                    let behind = base.map_or(0, |b| b.tree.divergence.0);
                    let pr = inner
                        .progress(&bottom)
                        .pr
                        .map(|n| format!(" (#{n})"))
                        .unwrap_or_default();
                    let few: Vec<&str> = base_files.iter().take(3).map(String::as_str).collect();
                    items.push(format!(
                        "the base of your stack, `{branch}`{pr}, is {behind} commits behind \
                         {upstream} and git cannot merge it in {} files ({}{}). It goes first, and \
                         nobody works in it: ask the user with `orch ask` whether to open a session on \
                         it. Push your own branch as it is; orchd restacks it once the base has moved.",
                        base_files.len(),
                        few.join(", "),
                        if base_files.len() > 3 { ", …" } else { "" }
                    ));
                }
            }

            // Upstream, for a branch that is not the upper half of a stack: the
            // stack's question is settled from its base, above.
            let own_upstream: Vec<String> = if stacked.is_none() {
                inner
                    .upstream_conflicts
                    .get(&ws)
                    .map(|(_, f)| f.clone())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            if !own_upstream.is_empty() {
                let few: Vec<&str> = own_upstream.iter().take(5).map(String::as_str).collect();
                items.push(format!(
                    "{upstream} has moved on, and your branch will not merge with it in {}{}. Rebase \
                     onto it first (`git rebase {upstream}`), settle those, then push.",
                    few.join(", "),
                    if own_upstream.len() > 5 { ", …" } else { "" }
                ));
            }

            // Other sessions, each with whose PR goes first.
            let mut by_other: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for p in inner.overlaps.iter().filter(|p| {
                p.involves(&ws) && inner.has_live_session(p.other(&ws)) && inner.real_conflict(p)
            }) {
                by_other
                    .entry(p.other(&ws).to_string())
                    .or_default()
                    .push(p.found.path.clone());
            }
            let mine = inner.progress(&ws);
            let mut conflicts = Vec::new();
            for (other, files) in &by_other {
                let Some(x) = inner
                    .sessions
                    .values()
                    .find(|x| &x.workspace == other && x.state.is_live())
                else {
                    continue;
                };
                let name = x.label().unwrap_or(&x.workspace);
                let theirs = inner.progress(other);
                let order = match Progress::ahead(&mine, &theirs) {
                    Some(true) => format!(
                        "Yours goes first ({}; theirs {}): push as you are, and they adapt after \
                         yours merges.",
                        mine.says, theirs.says
                    ),
                    Some(false) => format!(
                        "Theirs goes first ({}; yours {}): push as you are, and once theirs merges, \
                         rebase onto {upstream} and settle these files then.",
                        theirs.says, mine.says
                    ),
                    None => "Neither PR is further along: push as you are, and whichever merges \
                             second rebases then."
                        .to_string(),
                };
                items.push(format!(
                    "your changes to {} will conflict with session `{name}` (id {}). {order}",
                    files.join(", "),
                    x.id
                ));
                conflicts.extend(files.iter().map(|f| (f.clone(), other.clone())));
            }

            if let Some(hint) = pr_base_hint(&inner, &ws, command) {
                items.push(hint);
            }
            items.extend(s.push_notes.iter().cloned());
            (items, conflicts, own_upstream, restack)
        };

        // The restack command, which needs git for its cut.
        if let Some((lower_branch, st, path, branch)) = restack {
            let up = upstream.clone();
            let lb = lower_branch.clone();
            let cmd = orchd_base::proc::run_blocking("reading the stack for a push", move || {
                let point = crate::overlap::restack_point(&path, &st, &up);
                let onto = if crate::git::pushed_ref(&path, &branch).is_some() {
                    crate::overlap::pushed_lower_ahead(&path, &lb)
                        .unwrap_or_else(|| st.lower_tip.clone())
                } else {
                    st.lower_tip.clone()
                };
                (crate::restack::by_hand(&onto, point.as_deref()), st.behind)
            })
            .await
            .ok();
            if let Some((cmd, behind)) = cmd {
                let what = match behind {
                    0 => "was rewritten".to_string(),
                    1 => "has 1 new commit".to_string(),
                    n => format!("has {n} new commits"),
                };
                items.insert(
                    0,
                    format!(
                        "your branch is stacked on `{lower_branch}`, which {what}. Restack first: \
                         `{cmd}`, then push with --force-with-lease."
                    ),
                );
            }
        }
        Some(Points {
            items,
            conflicts,
            upstream: own_upstream,
        })
    }

    /// The note to hold this `git push` or `gh pr create` on, once, or `None` to
    /// let it through.
    ///
    /// Held when it says something the agent has not heard; the retry builds the
    /// same note and goes through, and only then are its points marked said.
    pub async fn push_note(&self, session: SessionId, command: &str) -> Option<String> {
        if !is_push(command) {
            return None;
        }
        let points = self.push_points(session, command).await?;
        let mut inner = self.inner.write().await;
        let s = inner.sessions.get_mut(&session)?;
        let fresh: Vec<&String> = points
            .items
            .iter()
            .filter(|i| !s.push_said.contains(&hash_of(i)))
            .collect();
        if fresh.is_empty() {
            return None;
        }
        let text =
            format!(
            "orchd: before this lands:\n{}\nThis command was held once so you see this; run it \
             again, changed or as it is.",
            fresh.iter().map(|i| format!("- {i}")).collect::<Vec<_>>().join("\n")
        );
        let h = hash_of(&text);
        if s.push_held != Some(h) {
            s.push_held = Some(h);
            return Some(text);
        }
        // The retry of a held push: let it through, and mark what it was told.
        s.push_held = None;
        let said: Vec<u64> = fresh.iter().map(|i| hash_of(i)).collect();
        s.push_said.extend(said);
        s.push_notes.clear();
        Self::mark_told(&mut inner, session, &points);
        None
    }

    /// The same points, typed in now: the Conflicts card's "Tell them now".
    /// Marks them said, so the next push does not hold over them again.
    pub async fn push_brief(&self, session: SessionId) -> Option<String> {
        let points = self.push_points(session, "").await?;
        let mut inner = self.inner.write().await;
        let s = inner.sessions.get_mut(&session)?;
        let fresh: Vec<String> = points
            .items
            .iter()
            .filter(|i| !s.push_said.contains(&hash_of(i)))
            .cloned()
            .collect();
        if fresh.is_empty() {
            return None;
        }
        s.push_said.extend(fresh.iter().map(|i| hash_of(i)));
        s.push_notes.clear();
        Self::mark_told(&mut inner, session, &points);
        Some(format!(
            "orchd: before you push:\n{}",
            fresh
                .iter()
                .map(|i| format!("- {i}"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }

    /// What the cards show as told, after a note said it.
    fn mark_told(inner: &mut Inner, session: SessionId, points: &Points) {
        if let Some(s) = inner.sessions.get_mut(&session) {
            s.told_overlaps.extend(
                points
                    .conflicts
                    .iter()
                    .map(|(path, other)| (path.clone(), other.clone(), true)),
            );
            if !points.upstream.is_empty() {
                s.told_upstream.clone_from(&points.upstream);
            }
        }
    }
}
