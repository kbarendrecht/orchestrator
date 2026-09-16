//! Worktrees cut before anybody asks for one.
//!
//! **What this buys.** Cutting a worktree is the slowest thing between pressing
//! the button and an agent appearing, and none of it depends on the person
//! pressing it. Measured by the daemon on the monorepo, 18,925 files, release
//! build: the `worktree ready` line reports **4,742ms** for a cut and **84ms** for
//! a claim, and the whole HTTP create drops from about 5s to about 260ms.
//! [`worktree::create_worktree`](crate::worktree::create_worktree) already said
//! the first half of that — "the repo's own `WorktreeCreate` is usually the whole
//! of the wait" — and this is the other half.
//!
//! **What a spare is.** An ordinary workspace with no session. It is cut by the
//! same [`create_worktree`](crate::worktree::create_worktree), put through the
//! same [`run_worktree_hooks`](crate::worktree::run_worktree_hooks) and
//! registered by the same `register_worktree`. Nothing here describes a worktree
//! a second time, and nothing downstream has to learn a new kind of one.
//!
//! **Why it can keep its own name.** The daemon generates workspace names itself
//! ([`crate::names`]) and derives the branch as `worktree-<name>`, so a claim
//! hands over the name the spare was cut under. That is what makes the claim free:
//! no rename, no `git worktree move`, and none of the relative symlinks a repo's
//! setup hook wrote have to be revalidated.
//!
//! **What it refuses to do.** No `git reset --hard`, anywhere. A stale spare is
//! discarded and cut again; a spare that somehow acquired work is *promoted* to an
//! ordinary workspace and left alone. `docs/traps/daemon.md` records what an
//! unbanked reset costs, and a spare is never worth finding out again.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::model::Board;
use crate::state::AppState;

/// Cut a spare again once it is this old, whatever git says about it.
///
/// **The catch-all for drift the daemon cannot see.** Three of the four ways a
/// spare goes stale are measurable — the base moved, the tree was touched, a
/// symlink dangles — and the fourth is not: a dependency that appeared in main
/// after the cut leaves a tree that is clean, current, and missing a link the
/// repo's setup hook would have made. [`refresh`] re-runs that hook on every tick
/// to answer it, but a repo's setup can depend on anything at all, so a spare also
/// simply expires.
///
/// Twelve hours: longer than a working session, shorter than a working week, and
/// the re-cut costs 1.4s with nobody waiting.
const MAX_AGE: Duration = Duration::from_secs(12 * 60 * 60);

/// How many entries a broken-symlink scan will look at before giving up.
///
/// The scan does not follow symlinks, so on the monorepo it never enters the
/// linked `node_modules` and finishes in 30ms over 29 links. This bound is for the
/// repo that is not shaped like that: a tree deep enough to make the scan
/// expensive gets no answer rather than a slow one, and the age limit above still
/// covers it.
const SCAN_BUDGET: usize = 20_000;

/// Why a spare cannot be handed to the next session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stale {
    /// The tree is gone from disk.
    Missing,
    /// Uncommitted changes, or commits of its own. **Somebody may be in there** —
    /// a `cd` and an editor, or a `claude` started by hand — so this one is never
    /// removed. See [`discard`].
    Touched,
    /// The base moved on.
    Behind,
    /// A symlink no longer resolves: main's target went, or main itself moved.
    Dangling,
    /// The tree is no longer on the branch it was cut with. A swap moved that
    /// branch into main, or somebody checked something else out in it.
    Moved,
    /// Cut too long ago to vouch for.
    Old,
}

impl Stale {
    /// Whether the tree may be removed, or must only be let go of.
    ///
    /// The one distinction that matters in this module: `Touched` is the state
    /// where removing would destroy something, and it is exactly the state a spare
    /// should never have reached.
    fn removable(self) -> bool {
        !matches!(self, Stale::Touched | Stale::Moved)
    }

    fn why(self) -> &'static str {
        match self {
            Stale::Missing => "its tree is gone",
            Stale::Touched => "it has been worked in",
            Stale::Behind => "the base moved on",
            Stale::Dangling => "a symlink no longer resolves",
            Stale::Moved => "it is no longer on the branch it was cut with",
            Stale::Old => "it is older than the pool vouches for",
        }
    }
}

/// Take a spare for the session about to start, or answer `None`.
///
/// **The id is taken under the write lock and the tree is measured afterwards.**
/// Two concurrent unnamed creates must not receive the same directory, and the
/// thing that used to make that impossible — `git worktree add` refusing an
/// existing path — is precisely what a claim skips. So the take is atomic and the
/// measuring runs on a tree no one else can now be handed.
///
/// **The measurement is not optional and is not the poller's.** `Tree`'s defaults
/// are indistinguishable from a fresh tree (see [`crate::model::Tree`]), the poll
/// interval is at least 30 seconds, and a spare is a real directory anybody can
/// `cd` into. So a claim asks git, every time, and hands back a tree it has just
/// looked at.
pub async fn claim(app: &Arc<AppState>) -> Option<(String, PathBuf)> {
    if app.cfg.spare_worktrees == 0 {
        return None;
    }
    let began = std::time::Instant::now();
    let taken = {
        let mut inner = app.inner.write().await;
        let id = inner.spare.ids.first().cloned()?;
        inner.with_spare("claimed", |s| {
            s.ids.retain(|x| x != &id);
            true
        });
        let path = inner.workspaces.get(&id).map(|w| w.path.clone());
        (id, path)
    };
    let (id, path) = taken;
    // A pool entry with no workspace record is what `reap_old` or a hand-removed
    // directory leaves behind. Not an error: the create falls back to cutting its
    // own, which is what it did before this module existed.
    let Some(path) = path else {
        tracing::info!(spare = %id, "the pool named a workspace the daemon no longer has; cutting fresh");
        return None;
    };

    match verdict(app, &id, &path).await {
        None => {
            // `verdict`'s four probes, and nothing else: this is what replaces the
            // cut above.
            tracing::info!(
                spare = %id,
                took_ms = began.elapsed().as_millis(),
                "claimed the pre-cut worktree"
            );
            Some((id, path))
        }
        Some(why) => {
            tracing::info!(spare = %id, "not handing over the spare: {}", why.why());
            discard(app, &id, why).await;
            None
        }
    }
}

/// Bring the pool up to `spare_worktrees`, quietly.
///
/// Spawned, never awaited by a request: the point of the pool is that nobody waits
/// for a cut, and a refill that blocked the create it was refilling *after* would
/// hand back the 4.4 seconds this exists to remove.
pub fn refill_soon(app: &Arc<AppState>) {
    let app = app.clone();
    tokio::spawn(async move {
        refill(&app).await;
    });
}

/// The body of [`refill_soon`], awaited by the tick and by tests.
pub async fn refill(app: &Arc<AppState>) {
    let want = app.cfg.spare_worktrees as usize;
    loop {
        let held = app.inner.read().await.spare.ids.len();
        if held >= want {
            return;
        }
        if !cut_one(app).await {
            // A cut that failed is not retried in a loop: the next tick will try
            // again, and a repo whose `WorktreeCreate` is broken must not have this
            // spinning on it.
            return;
        }
    }
}

/// Cut one spare and record it. `false` if anything refused.
async fn cut_one(app: &Arc<AppState>) -> bool {
    // What a create would have paid had the pool been empty, which is the other
    // half of the comparison `spawn`'s `worktree ready` line makes.
    let began = std::time::Instant::now();
    let name = {
        let inner = app.inner.read().await;
        let held: std::collections::HashSet<_> = inner.workspaces.keys().cloned().collect();
        crate::names::candidates().find(|c| !held.contains(c) && !app.cfg.worktree_path(c).exists())
    };
    let Some(name) = name else {
        return false;
    };
    let branch = format!("worktree-{name}");
    let path = app.cfg.worktree_path(&name);
    let base = app.cfg.upstream_ref.clone();

    // `Board::Quiet` throughout: `CreateRun` is one slot, and this cut is running
    // beside whatever a person is watching. See `crate::model::Board`.
    let made = match crate::worktree::create_worktree(
        app,
        &name,
        &path,
        crate::worktree::Want::New {
            branch: &branch,
            base: &base,
        },
        Board::Quiet,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(spare = %name, "could not cut a spare worktree: {e:#}");
            return false;
        }
    };
    crate::worktree::run_worktree_hooks(app, &made, Board::Quiet).await;

    app.register_worktree(&name, made.clone(), Some(branch))
        .await;
    {
        let mut inner = app.inner.write().await;
        inner.with_spare("cut", |s| {
            s.ids.push(name.clone());
            true
        });
    }
    // Recorded only now, after the hooks: a daemon killed mid-cut leaves a tree the
    // next boot adopts as an ordinary workspace — which is what an interrupted
    // create has always left — rather than a pool entry pointing at a half-made one.
    tracing::info!(
        spare = %name,
        at = %made.display(),
        took_ms = began.elapsed().as_millis(),
        "cut a spare worktree"
    );
    app.notify().await;
    true
}

/// The refresh ladder, run on the poll tick against every spare.
///
/// Cheapest first, and never on the request path. The tick has already fetched the
/// base by the time this runs, so "behind" here means behind what upstream said a
/// moment ago.
pub async fn refresh(app: &Arc<AppState>) {
    if app.cfg.spare_worktrees == 0 {
        // Turned off after having been on: let the pool go, and leave the trees as
        // ordinary workspaces for the reaper or the teardown button.
        let held: Vec<String> = app.inner.read().await.spare.ids.clone();
        if !held.is_empty() {
            let mut inner = app.inner.write().await;
            inner.with_spare("pool turned off", |s| {
                s.ids.clear();
                true
            });
            tracing::info!("spare_worktrees is 0: released {} spare(s)", held.len());
        }
        return;
    }

    let held: Vec<(String, Option<PathBuf>)> = {
        let inner = app.inner.read().await;
        inner
            .spare
            .ids
            .iter()
            .map(|id| (id.clone(), inner.workspaces.get(id).map(|w| w.path.clone())))
            .collect()
    };

    for (id, path) in held {
        let Some(path) = path else {
            discard(app, &id, Stale::Missing).await;
            continue;
        };
        match verdict(app, &id, &path).await {
            Some(why) => discard(app, &id, why).await,
            // Current by every measure the daemon has. Re-run the repo's own setup
            // hook anyway: it is the only thing that answers a dependency added to
            // main since the cut, it is idempotent by contract, and nothing is
            // waiting on it.
            None => crate::worktree::relink(app, &path).await,
        }
    }

    refill(app).await;
}

/// Reconcile the pool against what is actually on disk, once, at boot.
///
/// Workspace records are rediscovered every boot, so a spare comes back looking
/// like any other workspace nobody has opened. This is what re-attaches the pool to
/// it, and what recovers from a daemon killed mid-refill: an id whose tree or
/// record is gone is dropped, and the refill that follows tops the pool back up.
///
/// Runs after `adopt_existing_worktrees`, because until that has run there are no
/// workspace records to reconcile against.
pub async fn adopt_at_boot(app: &Arc<AppState>) {
    {
        let mut inner = app.inner.write().await;
        let stored = crate::store::load_spare();
        let kept: Vec<String> = stored
            .ids
            .iter()
            .filter(|id| inner.workspaces.contains_key(*id))
            .cloned()
            .collect();
        let dropped = stored.ids.len() - kept.len();
        inner.spare = crate::state::Durable::new(crate::store::SpareStore { ids: kept });
        if dropped > 0 {
            // Written back so the file stops naming trees that are not there.
            inner.with_spare("boot", |_| true);
            tracing::info!("dropped {dropped} spare(s) whose worktree is gone");
        }
    }
    refresh(app).await;
}

/// Let go of a spare, and remove its tree only when that destroys nothing.
///
/// **The two halves are the whole safety of this module.** `git worktree remove`
/// never deletes a branch and the teardown preflight refuses a dirty tree or an
/// unpushed commit, so a spare that has been worked in cannot be — and must not
/// be — cleaned up here. It is *promoted*: dropped from the pool and left standing
/// as an ordinary workspace, with its branch, for whoever put the work there.
async fn discard(app: &Arc<AppState>, id: &str, why: Stale) {
    {
        let mut inner = app.inner.write().await;
        inner.with_spare("discarded", |s| {
            let before = s.ids.len();
            s.ids.retain(|x| x != id);
            s.ids.len() != before
        });
    }
    if !why.removable() {
        tracing::info!(
            spare = %id,
            "promoting the spare to an ordinary workspace: {}",
            why.why()
        );
        app.notify().await;
        return;
    }
    if why == Stale::Missing {
        return;
    }

    match crate::worktree::teardown(app, id).await {
        Ok(_) => {
            // The branch goes too, and only through `branch_delete`, which is
            // `git branch -d`: git itself refuses a branch carrying commits, so
            // the pool cannot delete work by being wrong about what it holds.
            let (main, branch) = (app.cfg.main_checkout.clone(), format!("worktree-{id}"));
            let _ = crate::proc::run_blocking("deleting the spare's branch", move || {
                crate::git::branch_delete(&main, &branch)
            })
            .await;
            tracing::info!(spare = %id, "removed the spare: {}", why.why());
        }
        // The preflight said no. It is a better judge of this than the ladder is —
        // it looks at live sessions, banked work and attached processes, none of
        // which a spare should have — so the tree stays and stops being a spare.
        Err(e) => tracing::info!(
            spare = %id,
            "left the spare standing, the teardown preflight refused: {e:#}"
        ),
    }
}

/// Is this tree still handable? `None` is yes.
///
/// One hop off the runtime for all three git-shaped questions, because they are
/// asked together and each is a child process.
async fn verdict(app: &Arc<AppState>, id: &str, path: &Path) -> Option<Stale> {
    if !path.exists() {
        return Some(Stale::Missing);
    }
    let at = path.to_path_buf();
    let upstream = app.cfg.upstream_ref.clone();
    let measured = crate::proc::run_blocking("measuring the spare", move || {
        // Clean covers both halves of "somebody is in here": an edit, and an
        // untracked file. A commit of its own is the `ahead` count.
        let clean = crate::git::is_clean(&at).unwrap_or(false);
        let (behind, ahead) = crate::git::divergence(&at, &upstream).unwrap_or((0, 0));
        let branch = crate::git::current_branch(&at).ok();
        let dangling = has_dangling_symlink(&at);
        let age = std::fs::metadata(&at)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok());
        (clean, behind, ahead, branch, dangling, age)
    })
    .await;

    let Ok((clean, behind, ahead, branch, dangling, age)) = measured else {
        // A measurement that could not be taken is not a fresh tree. Refusing costs
        // a cut; assuming costs the session.
        return Some(Stale::Touched);
    };

    if !clean || ahead > 0 {
        return Some(Stale::Touched);
    }
    /* The branch the spare was cut with, still checked out in it. A swap moves a
    worktree's branch into main and gives the tree main's in exchange, which leaves
    a tree that is clean and at the base and is nevertheless somebody else's. Not
    removable for the same reason `Touched` is not: whatever is on that branch was
    put there deliberately. */
    if branch.as_deref() != Some(&format!("worktree-{id}")) {
        return Some(Stale::Moved);
    }
    if behind > 0 {
        return Some(Stale::Behind);
    }
    if dangling {
        return Some(Stale::Dangling);
    }
    if age.is_some_and(|a| a > MAX_AGE) {
        return Some(Stale::Old);
    }
    None
}

/// Does any symlink under this tree fail to resolve?
///
/// **Never follows one.** Descending through a symlinked directory would walk into
/// main's `node_modules` — the very thing the link exists to avoid copying — and on
/// a repo that links a directory back to its own parent it would not terminate.
/// So directories are entered only when `symlink_metadata` says they are real
/// directories, which also makes the scan cheap: 30ms over the monorepo's 29 links,
/// because the heavy trees are the links themselves.
///
/// Answers `false` when the budget runs out. A "probably fine" here costs a spare
/// that is refreshed by age instead; a "probably broken" would re-cut a good tree
/// on every tick.
fn has_dangling_symlink(root: &Path) -> bool {
    let mut stack = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > SCAN_BUDGET {
                return false;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                // `exists` follows the link, which is the question being asked.
                if !path.exists() {
                    return true;
                }
            } else if meta.is_dir() {
                // `.git` in a worktree is a file, not a directory, so there is no
                // object store to walk here.
                stack.push(path);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil;

    /// A repo whose base branch is `main`, ready for the pool to cut into.
    ///
    /// `upstream_ref` is a **local** branch on purpose: `worktree_add_new` freshens
    /// the base before it cuts, and a fixture with no remote must not be made to
    /// fetch one.
    fn repo(tag: &str) -> (std::path::PathBuf, Arc<AppState>) {
        let main = testutil::scratch_repo(tag);
        let app = testutil::app_at(
            &main,
            r#""upstream_ref":"main","upstream_remote":"origin","spare_worktrees":1"#,
        );
        (main, app)
    }

    /// How many worktrees git itself believes there are, main included.
    fn worktrees(main: &std::path::Path) -> usize {
        crate::git::worktree_list(main)
            .expect("git lists worktrees")
            .len()
    }

    /// The whole point: a claim hands over the tree that was already there, and
    /// cuts nothing. The worktree count is the assertion because it is the thing a
    /// timing test would have been a proxy for.
    #[tokio::test]
    async fn a_claim_hands_over_the_pre_cut_tree_and_cuts_nothing() {
        let (main, app) = repo("spare-claim");
        refill(&app).await;
        let after_cut = worktrees(&main);
        assert_eq!(after_cut, 2, "main plus one spare");
        let held = app.inner.read().await.spare.ids.clone();
        assert_eq!(held.len(), 1);

        let (id, path) = claim(&app).await.expect("the spare is handed over");
        assert_eq!(id, held[0]);
        assert!(path.exists());
        assert_eq!(
            worktrees(&main),
            after_cut,
            "a claim must not add a worktree"
        );
        assert!(
            app.inner.read().await.spare.ids.is_empty(),
            "a claimed spare leaves the pool"
        );
    }

    /// The guard that cannot be left to the poller: a tree somebody has touched is
    /// refused, **and left standing**, because the toucher may be a person.
    #[tokio::test]
    async fn a_touched_spare_is_refused_and_promoted_rather_than_removed() {
        let (main, app) = repo("spare-dirty");
        refill(&app).await;
        let (id, path) = {
            let inner = app.inner.read().await;
            let id = inner.spare.ids[0].clone();
            let path = inner.workspaces[&id].path.clone();
            (id, path)
        };
        std::fs::write(path.join("someone-was-here.txt"), "work").unwrap();

        assert!(
            claim(&app).await.is_none(),
            "a touched spare is not handed over"
        );
        assert!(path.exists(), "and it is never removed");
        assert_eq!(worktrees(&main), 2, "the tree is still a worktree");
        assert!(
            !app.inner.read().await.spare.ids.contains(&id),
            "but it is no longer in the pool"
        );
        assert!(
            app.inner.read().await.workspaces.contains_key(&id),
            "it stays an ordinary workspace"
        );
    }

    /// A commit of its own is work too, and the branch has to survive.
    #[tokio::test]
    async fn a_spare_holding_a_commit_is_promoted_and_keeps_its_branch() {
        let (main, app) = repo("spare-committed");
        refill(&app).await;
        let (id, path) = {
            let inner = app.inner.read().await;
            let id = inner.spare.ids[0].clone();
            (id.clone(), inner.workspaces[&id].path.clone())
        };
        testutil::git(&path, &["commit", "-q", "--allow-empty", "-m", "mine"]);

        assert!(claim(&app).await.is_none());
        assert!(path.exists(), "a tree with a commit in it is never removed");
        let branches = testutil::git(&main, &["branch", "--list", &format!("worktree-{id}")]);
        assert!(!branches.trim().is_empty(), "and its branch survives");
    }

    /// The base moving is the ordinary case, and the only one where the tree goes.
    #[tokio::test]
    async fn a_spare_behind_the_base_is_removed_and_replaced() {
        let (main, app) = repo("spare-behind");
        refill(&app).await;
        let first = app.inner.read().await.spare.ids[0].clone();
        let at = app.inner.read().await.workspaces[&first].path.clone();

        testutil::git(&main, &["commit", "-q", "--allow-empty", "-m", "moved on"]);
        refresh(&app).await;

        assert!(!at.exists(), "the stale tree is gone");
        let now = app.inner.read().await.spare.ids.clone();
        assert_eq!(now.len(), 1, "and the pool is full again");
        assert_ne!(now[0], first, "with a different tree");
        let branches = testutil::git(&main, &["branch", "--list", &format!("worktree-{first}")]);
        assert!(
            branches.trim().is_empty(),
            "a cleanly removed spare takes its branch with it"
        );
    }

    /// A swap moves a worktree's branch into main and gives it another in return.
    /// The tree is then clean and at the base and is still not the pool's.
    #[tokio::test]
    async fn a_spare_moved_onto_another_branch_is_promoted() {
        let (_main, app) = repo("spare-moved");
        refill(&app).await;
        let path = {
            let inner = app.inner.read().await;
            let id = inner.spare.ids[0].clone();
            inner.workspaces[&id].path.clone()
        };
        testutil::git(&path, &["checkout", "-q", "-b", "somebody-elses"]);

        assert!(claim(&app).await.is_none());
        assert!(path.exists(), "somebody else's branch is not removed");
    }

    /// A background cut beside a named create, which is the shape `app-check`
    /// drives and the one that took `bundle` red.
    ///
    /// **`git worktree add -b` writes `.git/config` under a lock it does not
    /// retry**, so before `AppState::cutting` the second of these died on
    /// `could not lock config file .git/config: File exists`. A race is not a
    /// deterministic test — without the mutex this fails often rather than always
    /// — so what it really pins is that the two paths *can* be driven at once and
    /// both trees arrive.
    #[tokio::test]
    async fn a_spare_cut_and_a_named_create_do_not_fight_over_the_config_lock() {
        let (main, app) = repo("spare-lockrace");
        let named = app.cfg.worktree_path("ledger");
        let (_, create) = tokio::join!(refill(&app), async {
            crate::worktree::create_worktree(
                &app,
                "ledger",
                &named,
                crate::worktree::Want::New {
                    branch: "worktree-ledger",
                    base: "main",
                },
                Board::Quiet,
            )
            .await
        });
        create.expect("the named create survives a spare being cut beside it");
        assert!(named.exists(), "the named tree is there");
        assert_eq!(worktrees(&main), 3, "main, the spare, and the named tree");
    }

    /// Two unnamed creates racing. The write lock is the only thing between them —
    /// a claim does no git, so nothing downstream would refuse the second.
    #[tokio::test]
    async fn two_claims_cannot_take_the_same_spare() {
        let (_main, app) = repo("spare-race");
        refill(&app).await;

        let (a, b) = tokio::join!(claim(&app), claim(&app));
        let got: Vec<_> = [a, b].into_iter().flatten().collect();
        assert_eq!(got.len(), 1, "exactly one claimant gets the spare");
    }

    /// The setting is the disk answer, and it has to cut nothing at all.
    #[tokio::test]
    async fn a_zero_pool_cuts_nothing_and_claims_nothing() {
        let main = testutil::scratch_repo("spare-off");
        let app = testutil::app_at(
            &main,
            r#""upstream_ref":"main","upstream_remote":"origin","spare_worktrees":0"#,
        );
        refill(&app).await;
        assert_eq!(worktrees(&main), 1, "main and nothing else");
        assert!(claim(&app).await.is_none());
    }

    /// The restart case: `spare.json` names a tree that is no longer a workspace,
    /// and the pool must let go of it rather than hand it over.
    #[tokio::test]
    async fn the_pool_lets_go_of_an_id_with_no_workspace() {
        let (_main, app) = repo("spare-ghost");
        {
            let mut inner = app.inner.write().await;
            inner.with_spare("test", |s| {
                s.ids.push("never-registered".into());
                true
            });
        }
        assert!(claim(&app).await.is_none(), "a ghost id is not handed over");
        assert!(
            !app.inner
                .read()
                .await
                .spare
                .ids
                .contains(&"never-registered".to_string()),
            "and it is dropped"
        );
    }

    /// The staleness class no git command can see, in the one form that is
    /// detectable: a link whose target went away.
    #[tokio::test]
    async fn a_dangling_symlink_makes_a_spare_stale() {
        let (_main, app) = repo("spare-link");
        refill(&app).await;
        let path = {
            let inner = app.inner.read().await;
            let id = inner.spare.ids[0].clone();
            inner.workspaces[&id].path.clone()
        };
        let target = path.parent().unwrap().join("gone-away");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, path.join("node_modules")).unwrap();
        assert!(!has_dangling_symlink(&path), "a link that resolves is fine");

        std::fs::remove_dir_all(&target).unwrap();
        assert!(has_dangling_symlink(&path), "one that does not is not");
    }

    /// The scan must not walk *through* a link: following one would enter main's
    /// own dependency tree, which is the cost the link exists to avoid.
    #[tokio::test]
    async fn the_symlink_scan_never_follows_a_link() {
        let dir = testutil::scratch("spare-loop");
        let inner = dir.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        // A link back to the directory that contains it: a scan that followed
        // links would not terminate.
        std::os::unix::fs::symlink(&dir, inner.join("up")).unwrap();
        assert!(!has_dangling_symlink(&dir));
    }
}
