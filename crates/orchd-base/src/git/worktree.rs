use anyhow::{bail, Context, Result};
use std::path::Path;

use super::*;

// ---------------------------------------------------------------------------
// Worktrees
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct WorktreeEntry {
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
}

pub fn worktree_list(main: &Path) -> Result<Vec<WorktreeEntry>> {
    let out = git(main, &["worktree", "list", "--porcelain"])?;
    let mut entries = Vec::new();
    let mut cur: Option<WorktreeEntry> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(e) = cur.take() {
                entries.push(e);
            }
            cur = Some(WorktreeEntry {
                path: path.to_string(),
                branch: None,
                head: None,
            });
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            if let Some(e) = cur.as_mut() {
                e.head = Some(head.to_string());
            }
        } else if let Some(branch) = line.strip_prefix("branch ") {
            if let Some(e) = cur.as_mut() {
                e.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
            }
        }
    }
    if let Some(e) = cur {
        entries.push(e);
    }
    Ok(entries)
}

/// Removal is `git worktree remove` followed by `git worktree prune` (§2), with
/// one narrow retry: a worktree stale-locked by a dead `claude --worktree` is
/// unlocked and removed, still without `--force`. See the body for why that is
/// not an escalation.
///
/// **Never `rm -rf` a worktree.** It can contain directories symlinked back to
/// main — a shared plan dir, a `vendor/` full of per-package symlinks. A recursive
/// delete that follows symlinks destroys the main checkout. If this refuses for
/// any reason other than a stale lock, the refusal is surfaced — it is never
/// escalated to `--force` and never falls back to a filesystem delete.
pub fn worktree_remove(main: &Path, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    // Already gone is a success, not an error.
    //
    // This runs after the repo's `WorktreeRemove` hook, which may have done the
    // whole job — so the ordinary outcome in a repo that declares one is that
    // there is nothing left here to remove. `git worktree remove` on a path that
    // is not there fails with "is not a working tree", which would turn a
    // completed teardown into a reported failure. Pruning is still owed: the hook
    // deleted a directory, and git's own registration of it is what is left.
    if !path.exists() {
        let _ = git(main, &["worktree", "prune"]);
        return Ok(());
    }
    if let Err(e) = git(main, &["worktree", "remove", &path_str]) {
        // A plain `git worktree remove` refuses for exactly two reasons: a dirty
        // tree, or a lock. Teardown's preflight already guaranteed the tree is
        // clean and no session is live, so the only thing left to trip on is a
        // *stale* lock — and `claude --worktree` leaves one on every worktree it
        // cuts, orphaned the moment the daemon kills the session (which is how
        // sessions end). Clearing a lock whose owner is dead is not the same as
        // `--force`: the retry is still a plain remove, so a dirty tree still
        // refuses, and nothing does a filesystem delete that could follow the
        // symlinks into main. A lock whose pid is still alive, or one with no pid
        // to check, is left to refuse — surfacing it beats unlocking something
        // that might mean what it says.
        if stale_lock_pid(main, path).is_some_and(|pid| !crate::pty::pid_alive(pid)) {
            let _ = git(main, &["worktree", "unlock", &path_str]);
            git(main, &["worktree", "remove", &path_str]).with_context(|| {
                format!(
                    "git worktree remove refused for {} even after clearing a stale claude lock",
                    path.display()
                )
            })?;
        } else {
            return Err(e).with_context(|| {
                format!(
                    "git worktree remove refused for {}; not escalating to --force",
                    path.display()
                )
            });
        }
    }
    let _ = git(main, &["worktree", "prune"]);
    Ok(())
}

/// Delete a branch, and **refuse one that carries commits**.
///
/// `git branch -d`, never `-D`. The lowercase flag is the whole point: git itself
/// refuses to delete a branch holding commits that are not reachable from where it
/// would be merged, so the guarantee is git's rather than a check this daemon
/// performs and could get wrong.
///
/// Written for the spare pool, which cuts a `worktree-<name>` branch per spare and
/// would otherwise leave one behind on every discard. That is also why the refusal
/// matters more than the deletion: a spare that somehow acquired a commit is not
/// silt, and the pool promotes it to an ordinary workspace instead. The error is
/// returned rather than logged, so the caller can tell the two outcomes apart.
pub fn branch_delete(main: &Path, branch: &str) -> Result<()> {
    git(main, &["branch", "-d", branch])
        .with_context(|| format!("git refused to delete {branch}"))?;
    Ok(())
}

/// The pid holding a worktree's lock, if it is locked and the lock reason names
/// one. `claude --worktree` writes `claude session <name> (pid <PID> start <N>)`,
/// which is the only lock this daemon ever expects to see — a plain checkout
/// never locks a worktree itself. Returns `None` when the worktree is not
/// locked or the reason carries no `pid`, both of which mean "do not touch it".
pub(super) fn stale_lock_pid(main: &Path, path: &Path) -> Option<u32> {
    let list = git(main, &["worktree", "list", "--porcelain"]).ok()?;
    // Both sides resolved before comparing, because **git reports the real path**
    // and the caller's may be reached through a symlink. On macOS that is the
    // normal case rather than the exotic one — `/tmp`, `/var` and `$TMPDIR` all
    // live under `/private` — and a string compare simply missed, so the lock was
    // never recognised as stale and teardown refused forever: the very bug this
    // function exists to fix, back again on one platform. Caught by CI on the
    // macos runner, not by reading.
    let resolve = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let want = resolve(path);
    // Records are blank-line separated; find the one for this path and read its
    // `locked` line without letting a later record's lock leak into the answer.
    let mut in_record = false;
    for line in list.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            in_record = resolve(Path::new(p)) == want;
        } else if in_record {
            if let Some(reason) = line.strip_prefix("locked") {
                return reason
                    .split_once("pid ")
                    .and_then(|(_, rest)| rest.split(|c: char| !c.is_ascii_digit()).next())
                    .and_then(|d| d.parse().ok());
            }
        }
    }
    None
}

/// Freshen a base ref that can go stale, then prove it resolves.
///
/// The repo's own `WorktreeCreate` hook opens with `git fetch upstream develop`, and
/// this is the path taken when there was no usable hook — `create_worktree` falls
/// through to the daemon's own creation — so without the fetch here those trees
/// started from whatever the last fetch happened to leave behind.
/// Non-fatal exactly as the hook has it: offline is a reason to cut from the
/// last-known base, not a reason to refuse.
///
/// Only fetches a remote-tracking base. A sha has no `/` and a local branch has no
/// remote to ask, so both fall through to the resolve check alone — which is what
/// lets a caller skip the fetch entirely by resolving `base` to a sha first.
pub(super) fn freshen_base(main: &Path, base: &str) -> Result<()> {
    if let Some((remote, branch)) = base.split_once('/') {
        if has_remote(main, remote)
            && git_net_ok(
                main,
                &["fetch", "--quiet", remote, branch],
                "the base fetch",
            )
            .is_err()
        {
            tracing::warn!("could not fetch {base}; using the last-known copy");
        }
    }
    // A base that does not resolve would otherwise cut from HEAD silently, which
    // is a different branch than the caller asked for.
    if !git_ok(main, &["rev-parse", "--verify", "--quiet", base]) {
        bail!("base ref {base} does not resolve");
    }
    Ok(())
}

/// The remote-tracking ref for a branch this checkout has no local copy of, or
/// `None` when no remote carries it.
///
/// `origin` first: PRs are opened from the fork, which is `origin` by convention
/// (§6), so that one fetch answers the ordinary case. Only when it comes up empty
/// does this fall back to every remote — a fork whose push remote is named
/// something other than `origin` (a personal remote, `fork`, `mine`) still
/// resolves rather than failing with "exists neither locally nor on origin". The
/// fallback's `fetch --all` runs only on that miss, never on the common path.
pub(super) fn remote_branch(main: &Path, branch: &str) -> Option<String> {
    let _ = git_net_ok(
        main,
        &["fetch", "origin", branch, "--no-tags"],
        "a branch fetch",
    );
    let origin = format!("origin/{branch}");
    if git_ok(main, &["rev-parse", "--verify", "--quiet", &origin]) {
        return Some(origin);
    }
    let _ = git_net_ok(main, &["fetch", "--all", "--no-tags"], "the fallback fetch");
    let listed = git(
        main,
        &["for-each-ref", "--format=%(refname:short)", "refs/remotes/"],
    )
    .ok()?;
    // The short ref is `<remote>/<branch>`; match the branch part exactly so a
    // slash in the branch name (`feature/x`) does not misfire against a shorter
    // one, and so branch `x` never matches `origin/feature/x`.
    listed
        .lines()
        .map(str::trim)
        .find(|s| s.split_once('/').is_some_and(|(_, b)| b == branch))
        .map(String::from)
}

/// Where a branch lives: `None` for local, `Some("<remote>/x")` when only a remote
/// has it, and an error when none does.
///
/// One function because the rule is one rule (§6): PRs are opened from the fork, so
/// a head ref this checkout has never seen is the ordinary case rather than a
/// failure. Both the worktree-add and the checkout path ask it.
pub(super) fn locate_branch(main: &Path, branch: &str) -> Result<Option<String>> {
    if branch_exists(main, branch) {
        return Ok(None);
    }
    match remote_branch(main, branch) {
        Some(remote) => Ok(Some(remote)),
        None => bail!("branch {branch} exists neither locally nor on any remote"),
    }
}

/// Refuse to move a tree that is carrying work.
///
/// A freshly created worktree is clean, so anything here is a hook having left
/// something behind, and moving it silently onto another branch is how that stops
/// being noticed. Tracked changes only: untracked files survive a checkout.
pub(super) fn refuse_if_dirty(tree: &Path, branch: &str) -> Result<()> {
    let set = status(tree, None, Untracked::Collapsed)?;
    if !set.staged.is_empty() || !set.unstaged.is_empty() {
        bail!(
            "{} has uncommitted work; not moving it onto {branch}",
            tree.display()
        );
    }
    Ok(())
}

/// Cut a worktree on a **new** branch, based on `base`.
///
/// The daemon's own version of what `claude --worktree` does, for a repo whose
/// worktrees do not live where that command puts them. Mirrors its naming
/// (`worktree-<name>`) so a worktree is recognisable whichever path created it.
pub fn worktree_add_new(main: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    if branch_exists(main, branch) {
        bail!("branch {branch} already exists");
    }
    let path_str = path.to_string_lossy().into_owned();
    freshen_base(main, base)?;
    git(main, &["worktree", "add", &path_str, "-b", branch, base])?;
    Ok(())
}

/// Put an existing tree onto a new branch cut from `base`.
///
/// For a worktree the repo's `WorktreeCreate` hook made on a base of its own
/// choosing. `-B` rather than `-b`: the daemon owns this branch name, and a retry
/// after a half-finished attempt must not fail because the name is taken.
pub fn checkout_new_branch(main: &Path, tree: &Path, branch: &str, base: &str) -> Result<()> {
    freshen_base(main, base)?;
    refuse_if_dirty(tree, branch)?;
    git(tree, &["checkout", "-q", "-B", branch, base])?;
    Ok(())
}

/// Put an existing tree onto a branch that already exists, locally or on origin.
///
/// The PR half of the pair. If another worktree already holds the branch git
/// refuses, and the caller falls back on that error rather than a check here.
pub fn checkout_existing_branch(main: &Path, tree: &Path, branch: &str) -> Result<()> {
    refuse_if_dirty(tree, branch)?;
    match locate_branch(main, branch)? {
        None => git(tree, &["checkout", "-q", branch])?,
        Some(remote) => git(tree, &["checkout", "-q", "-B", branch, &remote])?,
    };
    Ok(())
}

/// Add a worktree checked out on an **existing** branch.
///
/// `claude --worktree` always cuts a fresh `worktree-<name>` from
/// `upstream/develop`, which is wrong for `/resolve`: that has to land on the
/// PR's own head branch (§8). The repo's `worktree-create` hook is therefore
/// not involved here, but `worktree-link` still runs at `SessionStart` and does
/// the symlinks, which is the same path §2 describes for rebuilding a worktree
/// on resume.
pub fn worktree_add_existing(main: &Path, path: &Path, branch: &str) -> Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    match locate_branch(main, branch)? {
        None => git(main, &["worktree", "add", &path_str, branch])?,
        Some(remote) => git(main, &["worktree", "add", &path_str, "-b", branch, &remote])?,
    };
    Ok(())
}

/// Rebuild an archived session's worktree at the path it was recorded under.
///
/// Transcripts are keyed by working directory, so `--resume` only finds the
/// conversation when the path is identical (§2) — the caller passes the recorded
/// `cwd`, not a freshly chosen one.
///
/// §2 step 1: the branch may be gone, merged and deleted. Recreate it, from
/// `origin` if the head ref is still there and from the recorded commit if it is
/// not. Step 3: the branch may have moved on instead, in which case the working
/// tree the conversation describes is not the one being rebuilt — that comes back
/// as the tip so the caller can say so.
pub fn worktree_rebuild(
    main: &Path,
    path: &Path,
    branch: &str,
    recorded: &str,
) -> Result<Option<String>> {
    let path_str = path.to_string_lossy().into_owned();
    if branch_exists(main, branch) {
        git(main, &["worktree", "add", &path_str, branch])?;
    } else {
        // The head ref lives on the fork; `remote_branch` tries origin first and
        // then any remote, so a fork not named origin still resolves (§6).
        if let Some(remote) = remote_branch(main, branch) {
            git(main, &["worktree", "add", &path_str, "-b", branch, &remote])?;
        } else if git_ok(main, &["cat-file", "-e", &format!("{recorded}^{{commit}}")]) {
            // Nothing to track, but the commit the conversation ran on is still
            // reachable, so the branch is recreated where it left off.
            git(
                main,
                &["worktree", "add", &path_str, "-b", branch, recorded],
            )?;
        } else {
            bail!(
                "branch {branch} is gone and {} is unreachable, so there is nothing \
                 to rebuild this conversation on",
                short(recorded)
            );
        }
    }

    let tip = head_sha(path)?;
    Ok((tip != recorded).then_some(tip))
}

/// Put a checkout on `branch`, fetching it from origin when it is not local yet.
///
/// Git refuses a branch that another worktree already has checked out, and that
/// refusal is the right answer: two trees on one branch is how you get a rebase
/// in one of them rewriting the other's HEAD underneath it.
pub fn switch_branch(cwd: &Path, branch: &str) -> Result<()> {
    if branch_exists(cwd, branch) {
        git(cwd, &["switch", branch])?;
        return Ok(());
    }
    // The head ref lives on the fork; `remote_branch` tries origin first and then
    // any remote, so a fork not named origin still resolves (§6).
    let remote = remote_branch(cwd, branch).ok_or_else(|| {
        anyhow::anyhow!("branch {branch} exists neither locally nor on any remote")
    })?;
    git(cwd, &["switch", "-c", branch, "--track", &remote])?;
    Ok(())
}

pub(super) fn branch_exists(main: &Path, branch: &str) -> bool {
    git_ok(
        main,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

/// How far this branch has drifted from the upstream base.
///
/// `behind` is what makes the rebase affordance appear: commits on
/// `upstream/develop` that this branch does not have, i.e. other people's work
/// that has landed since you branched.
pub fn divergence(cwd: &Path, upstream: &str) -> Result<(u32, u32)> {
    let range = format!("{upstream}...HEAD");
    let out = git(cwd, &["rev-list", "--left-right", "--count", &range])?;
    let mut parts = out.split_whitespace();
    let behind = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let ahead = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Ok((behind, ahead))
}

/// How many commits this branch holds that its own remote does not.
///
/// Counted against `origin/<branch>`, not `@{upstream}`: on this fork layout
/// `@{upstream}` is the *base* (`upstream/develop`), which answers a different
/// question — see [`unpushed()`], which had to say the same thing.
///
/// With no remote counterpart the measure falls back to the commits beyond the
/// base, the same reading [`unpushed()`]'s `NeverPushed` arm takes: on a branch that
/// was never pushed, everything it added is unpushed. Zero rather than an error
/// when git cannot answer, since this feeds a count in an overview and a
/// transient failure should not read as work to push.
pub fn unpushed_count(cwd: &Path, branch: &str, upstream: &str) -> u32 {
    let (range, _) = unpushed_range(cwd, branch, upstream);
    git(cwd, &["rev-list", "--count", &range])
        .ok()
        .and_then(|out| out.trim().parse().ok())
        .unwrap_or(0)
}

/// The range holding this branch's unpushed commits, and whether origin has the
/// branch at all. Beyond `origin/<branch>` when it exists; else beyond the
/// upstream base, which is exactly the set of commits that exist nowhere but
/// here. The one place that decides it, for [`unpushed()`] and [`unpushed_count`].
pub(super) fn unpushed_range(cwd: &Path, branch: &str, upstream: &str) -> (String, bool) {
    let remote_ref = format!("refs/remotes/origin/{branch}");
    if git_ok(cwd, &["rev-parse", "--verify", "--quiet", &remote_ref]) {
        (format!("origin/{branch}..HEAD"), true)
    } else {
        (format!("{upstream}..HEAD"), false)
    }
}

/// Whether a rebase is stopped part-way in this worktree.
///
/// Checked on disk rather than inferred: a button that offers to rebase a tree
/// already mid-rebase would make a mess that is annoying to unpick.
///
/// `--absolute-git-dir` rather than `--path-format=absolute --git-dir`, which
/// says the same thing and needs git 2.31. The error arm here returns `false`,
/// so on an older git every caller would read "not rebasing" and every guard
/// that gates on it would open — silently, on the machine that has the older
/// git and nowhere else. `head_file` already spells it this way.
pub fn rebase_in_progress(cwd: &Path) -> bool {
    let Ok(dir) = git(cwd, &["rev-parse", "--absolute-git-dir"]) else {
        return false;
    };
    let dir = Path::new(dir.trim());
    dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists()
}

/// Rebase onto the upstream base.
///
/// Never a merge: history stays linear, which is how this repo is worked (§5's
/// base choice depends on it too). A rebase that stops on conflicts is left
/// stopped — that is the state you resolve from — and reported rather than
/// silently aborted.
pub fn rebase_onto(cwd: &Path, upstream: &str) -> Result<()> {
    let out = run(cwd, &["rebase", upstream]).context("running git rebase")?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    if rebase_in_progress(cwd) {
        let files = conflicted_files(cwd).unwrap_or_default();
        bail!(
            "rebase stopped on conflicts in {} file(s): {}. Resolve them in a session, \
             or abort.",
            files.len(),
            files.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
        );
    }
    /* **An untracked file in the way is the one dirty-tree case a bank cannot
    clear**, since `stash create` carries tracked changes only, so it is worth
    its own sentence. Git's own is a header with the paths on the lines below
    it, and the generic arm underneath prints only that header — "would be
    overwritten by checkout" with nothing said about what. */
    let both = format!("{stderr}{stdout}");
    let blocked = untracked_in_the_way(&both);
    if !blocked.is_empty() {
        bail!(
            "not rebased: the base adds {}, and this tree has {} that git has never seen. \
             Move {} aside, or commit {}.",
            blocked.join(", "),
            if blocked.len() == 1 { "one" } else { "files" },
            if blocked.len() == 1 { "it" } else { "them" },
            if blocked.len() == 1 { "it" } else { "them" },
        );
    }
    bail!(
        "rebase failed: {}",
        stderr
            .lines()
            .chain(stdout.lines())
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output")
    );
}

/// The paths git listed under its untracked-would-be-overwritten header.
///
/// Matched on the header rather than on a whole-message shape, because the same
/// list follows it for `checkout`, `merge` and `rebase` alike, and the advice
/// paragraph after the paths always starts unindented.
pub(super) fn untracked_in_the_way(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut collecting = false;
    for line in text.lines() {
        if line.contains("untracked working tree files would be overwritten") {
            collecting = true;
            continue;
        }
        if !collecting {
            continue;
        }
        // Git indents each path and then leaves the margin for its advice.
        match line.strip_prefix('\t').or_else(|| line.strip_prefix("  ")) {
            Some(path) if !path.trim().is_empty() => out.push(path.trim().to_string()),
            _ => break,
        }
    }
    out
}

pub fn rebase_abort(cwd: &Path) -> Result<()> {
    git(cwd, &["rebase", "--abort"])?;
    Ok(())
}

/// Paths git has left unmerged, whatever put them there.
///
/// [`conflicted_files`]'s public twin. Kept as one call rather than folded into
/// `status`, because the callers want the *paths* and `FileSet` deliberately
/// files both sides of a conflict under `unstaged` — the pane's question, not
/// this one.
pub fn unmerged(cwd: &Path) -> Result<Vec<String>> {
    conflicted_files(cwd)
}

pub(super) fn conflicted_files(cwd: &Path) -> Result<Vec<String>> {
    let out = git(cwd, &["diff", "--name-only", "--diff-filter=U"])?;
    Ok(out.lines().map(|l| l.to_string()).collect())
}

/// Refresh the base ref the context bar measures against, from config rather
/// than a hardcoded `upstream develop`.
///
/// `upstream_ref` is `remote/branch` (e.g. `upstream/develop`, `origin/HEAD`).
/// A concrete branch is fetched by name — one branch, cheap, the common case.
///
/// **`HEAD` is not a branch you can fetch.** It is a symref under
/// `refs/remotes/<remote>/` that only `git clone` and `git remote set-head`
/// write — a plain `git fetch <remote>` does *not* create or refresh it, so on a
/// checkout whose remote was added by hand `origin/HEAD` never resolves and
/// every consumer (merge-base, divergence, rebase, the prompts' `{{UPSTREAM}}`)
/// silently fails. So the `HEAD` arm asks the remote what its default branch is,
/// records it in the symref, and then fetches that one branch by name — correct
/// on a fresh checkout and no more expensive than the named case.
pub fn fetch_upstream(main: &Path, upstream_ref: &str) -> Result<()> {
    let (remote, branch) = split_upstream(upstream_ref);
    if !branch.eq_ignore_ascii_case("HEAD") {
        // Through `git_net_ok` like the two `HEAD` arms below, not `git`: this is
        // the arm a configured `upstream_ref` takes, on the boot path, and an
        // https remote without a credential helper prompted on `/dev/tty` here
        // while the window never opened.
        git_net_ok(
            main,
            &upstream_fetch_argv(upstream_ref),
            "the upstream fetch",
        )?;
        return Ok(());
    }
    // Steady state: the symref is already recorded, so fetch just the branch it
    // names — no dearer than the named case. If that fetch fails the recorded
    // branch is gone (renamed or deleted upstream), so fall through and re-record.
    if let Some(b) = default_branch(main, remote) {
        if git_net_ok(
            main,
            &["fetch", remote, &b, "--no-tags"],
            "the upstream fetch",
        )
        .is_ok()
        {
            return Ok(());
        }
        tracing::debug!("{remote}/HEAD named {b}, which no longer fetches; re-recording");
    }
    // Bootstrap: `git remote set-head -a` picks from the remote-tracking refs, so
    // they have to exist first — which is why this fetches the whole remote
    // before recording. Only the first poll on a checkout pays for it.
    git_net_ok(main, &["fetch", remote, "--no-tags"], "the bootstrap fetch")?;
    if let Err(e) = git(main, &["remote", "set-head", remote, "-a"]) {
        // Not fatal: the refs are fetched, only the symref is missing, and the
        // caller's merge-base will report the real problem.
        tracing::debug!("could not record {remote}/HEAD: {e:#}");
    }
    Ok(())
}

/// `remote/branch`, defaulting the remote to `origin` for a bare branch name.
/// `split_once` keeps a nested branch like `origin/release/2026` intact.
pub(super) fn split_upstream(upstream_ref: &str) -> (&str, &str) {
    upstream_ref
        .split_once('/')
        .unwrap_or(("origin", upstream_ref))
}

/// The branch `<remote>/HEAD` points at, as a plain name (`main`), or `None`
/// when the symref does not exist.
pub(super) fn default_branch(main: &Path, remote: &str) -> Option<String> {
    let out = git(
        main,
        &[
            "symbolic-ref",
            "--short",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
    )
    .ok()?;
    let full = out.trim();
    // `origin/main` -> `main`.
    Some(full.strip_prefix(&format!("{remote}/"))?.to_string())
}

pub(super) fn has_remote(main: &Path, name: &str) -> bool {
    git(main, &["remote", "get-url", name]).is_ok()
}

/// The base ref a first run should adopt, when the checkout already answers.
///
/// A fork workflow is not the common case, but it is unmistakable when it is
/// there: an `upstream` remote beside `origin` means branches are pushed to one
/// and measured against the other. Detecting it means a fork user never has to
/// learn that there are two keys to set, while everyone else gets the generic
/// pair and never sees this.
///
/// `None` is "no opinion" — not a git repo, or no `upstream` — and the caller
/// keeps the defaults. Only ever consulted when writing a *first* config; an
/// existing `config.json` is never second-guessed.
pub fn detect_base(main: &Path) -> Option<(String, String)> {
    if !has_remote(main, "upstream") {
        return None;
    }
    // `upstream/HEAD` only resolves once that remote has been fetched, so fall
    // back to the symbolic form rather than guessing a branch name.
    // `fetch_upstream` refreshes the symref either way.
    let branch = default_branch(main, "upstream").unwrap_or_else(|| "HEAD".to_string());
    Some((format!("upstream/{branch}"), "upstream".to_string()))
}

/// The local branch to check out for `upstream_ref`, resolving a `HEAD` symref.
///
/// `git switch HEAD` fails outright — "a branch is expected" — so a base ref of
/// `origin/HEAD`, which is the default, needs the name behind the symref before
/// it can be checked out. That needs the repo, which is why it takes one.
///
/// There used to be a pure-string sibling (`base_branch`) that answered `HEAD` for
/// the default `origin/HEAD` — a name nobody can check out. It had four tests and
/// no callers, so what it really offered was a way to reach for the wrong one.
///
/// `None` when the symref has never been recorded, which happens on a checkout
/// whose remote was added by hand and not yet fetched. Callers treat that as
/// "cannot", not as an error: [`fetch_upstream`] records it on the next poll.
pub fn base_checkout_branch(main: &Path, upstream_ref: &str) -> Option<String> {
    let (remote, branch) = split_upstream(upstream_ref);
    if branch.eq_ignore_ascii_case("HEAD") {
        return default_branch(main, remote);
    }
    Some(branch.to_string())
}

/// The remote-tracking branches (`<remote>/<branch>`), for the first-run base-branch
/// picker. `*/HEAD` symrefs are dropped: they are pointers, not branches to build
/// from. Empty on any error, which the picker treats as "nothing to offer".
pub fn remote_branches(main: &Path) -> Vec<String> {
    git(
        main,
        &["for-each-ref", "--format=%(refname:short)", "refs/remotes/"],
    )
    .map(|out| {
        out.lines()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.ends_with("/HEAD"))
            .map(String::from)
            .collect()
    })
    .unwrap_or_default()
}

/// What a swap did. See [`Swap::wip_error`] for why re-applying the uncommitted
/// work failing is a field here rather than an `Err`.
pub struct Swap {
    /// The branch main has now.
    pub main_now: String,
    /// The branch the worktree has now.
    pub worktree_now: String,
    /// Set when the branches exchanged but the banked work would not re-apply.
    ///
    /// Deliberately not an `Err`. Once the exchange commits, "swapped" is what
    /// happened, and returning an error made the caller bail before it forgot the
    /// traded branches, reconciled the two panes and moved the conversations — so
    /// git described the swapped world while the daemon went on describing the
    /// pre-swap one, and the SPA said only "failed". The work itself is never at
    /// risk: it is in the WIP commit this message names.
    pub wip_error: Option<String>,
}

/// Exchange the branches checked out in two trees.
///
/// The tree a worktree *is* cannot be swapped with main — worktrees live inside
/// main, so one would have to contain its own parent, and git will not move the
/// main worktree anyway. What can be exchanged is what each has checked out, which
/// is what the swap is actually for: getting the work into main, where the managed
/// processes and the dev stack live.
///
/// # Why three steps
///
/// Git refuses a branch that is already checked out elsewhere, so the obvious
/// `switch` in each tree fails on the first one:
///
/// ```text
/// fatal: 'feature/b' is already used by worktree at '…/.claude/worktrees/w'
/// ```
///
/// So the worktree detaches first, which frees its branch; main takes it, which
/// frees main's; the worktree takes that. Measured, not assumed — the sequence and
/// its failure are both pinned by a test.
///
/// # On failure
///
/// The rollback matters more than the happy path. Step two is the one that can
/// fail on real repos (a stale index, a file that would be overwritten), and it
/// fails with the worktree *detached* — a state nobody asked for. So a failure
/// there puts the worktree back on its own branch before returning the error, and
/// the caller sees "nothing happened" rather than a repo it has to unpick.
pub fn swap_branches(main: &Path, worktree: &Path) -> Result<Swap> {
    let main_branch = current_branch(main)?;
    let tree_branch = current_branch(worktree)?;
    if main_branch == tree_branch {
        bail!("both are on {main_branch}, so there is nothing to exchange");
    }

    // Uncommitted work travels with its branch, or the swap moves the code and
    // leaves the edits behind — which is worse than refusing, because you would
    // find out by looking.
    //
    // Captured *before* anything moves, and crosswise afterwards: what was on top
    // of `tree_branch` is re-applied in main, which by then has `tree_branch`
    // checked out. Same base both sides, so an apply cannot conflict — that is what
    // makes this safe rather than hopeful.
    let main_wip = capture_wip(main)?;
    let tree_wip = capture_wip(worktree)?;

    // Detaching is the only free move: it takes a branch out of use without
    // needing another one to be available.
    switch_detach(worktree)?;

    if let Err(e) = switch_branch(main, &tree_branch) {
        // Undo the detach, or the worktree is left off its branch for a failure
        // that changed nothing else.
        if let Err(back) = switch_branch(worktree, &tree_branch) {
            bail!(
                "{e:#} — and {} could not be put back on {tree_branch}: {back:#}",
                worktree.display()
            );
        }
        return Err(e.context(format!("{} could not take {tree_branch}", main.display())));
    }

    if let Err(e) = switch_branch(worktree, &main_branch) {
        // Main already moved. Putting it back frees `tree_branch` again, so the
        // worktree can return to it and the pair is where it started.
        let _ = switch_branch(main, &main_branch);
        let _ = switch_branch(worktree, &tree_branch);
        return Err(e.context(format!(
            "{} could not take {main_branch}; both were put back",
            worktree.display()
        )));
    }

    // Crosswise, and after both branches are in place. A failure here is reported,
    // not rolled back: the WIP commit still holds the work and is named in the
    // error, so nothing is lost even in the case that should not happen.
    //
    // Reported *alongside the swap*, not instead of it — the branches are already
    // exchanged by the time this runs, so an `Err` here would deny something that
    // has happened. See `Swap::wip_error`.
    let mut carried = Ok(());
    if let Some(wip) = &tree_wip {
        carried = carried.and(apply_wip(main, wip, "the branches swapped"));
    }
    if let Some(wip) = &main_wip {
        carried = carried.and(apply_wip(worktree, wip, "the branches swapped"));
    }

    Ok(Swap {
        main_now: tree_branch,
        worktree_now: main_branch,
        wip_error: carried.err().map(|e| format!("{e:#}")),
    })
}

/// What a move out of main did.
#[derive(Debug)]
pub struct MovedOut {
    /// The branch now checked out in the new worktree: the one that left main, or
    /// the one cut for the work when main had nothing but base.
    pub branch: String,
    /// What main is on now.
    pub base: String,
    /// Whether that branch was created here rather than handed over.
    pub created: bool,
    /// See [`Swap::wip_error`]: the branch has already moved by the time the carry
    /// runs, so a failure is reported beside the move rather than undoing it.
    pub wip_error: Option<String>,
}

/// Move main's branch into a worktree of its own and put main back on `base`.
///
/// The one-directional half of [`swap_branches`]. There is no second branch to
/// exchange, so main returns to base and the branch gets a tree cut for it — which
/// is only possible in this order: git refuses a worktree for a branch that is
/// still checked out somewhere, so main has to let go of it first.
///
/// Same WIP contract as the swap: uncommitted work is carried rather than refused,
/// untracked files stay where they are (`stash create` cannot take them, and the
/// caller names them), and everything up to the worktree existing is undoable —
/// a refusal puts main back on its branch with its edits.
///
/// `new_branch` is only used when main is sitting on `base` — you were working in
/// main directly and it turned into something. Then there is no branch to hand
/// over, so the work gets one cut for it and main does not move at all. Uniquified
/// here rather than by the caller, because deciding it needs the repo.
pub fn move_branch_out(main: &Path, dest: &Path, base: &str, new_branch: &str) -> Result<MovedOut> {
    let branch = current_branch(main)?;
    if rebase_in_progress(main) {
        bail!("the main checkout has a rebase stopped part-way; finish or abort it first");
    }

    // Banked and the tree cleaned: a switch would otherwise carry the edits onto
    // base, which is the one outcome you cannot press back out of.
    let wip = capture_wip(main)?;

    // Main is on base: nothing to hand over and nothing to switch, so this is the
    // simpler half despite being the one that creates a branch. Main stays exactly
    // where it is, on base and clean.
    if branch == base {
        let branch = free_branch(main, new_branch);
        let path = dest.to_string_lossy().into_owned();
        if let Err(e) = git(main, &["worktree", "add", "-b", &branch, &path]) {
            let mut err = e.context(format!("no worktree could be cut for {branch}"));
            // Nothing moved but the work, so putting that back is the whole undo.
            if let Some(sha) = &wip {
                if let Err(back) = apply_wip(main, sha, "the branches swapped") {
                    err = err.context(format!(
                        "and its uncommitted work is still in commit {sha}: {back:#}"
                    ));
                }
            }
            return Err(err);
        }
        let wip_error = match &wip {
            Some(sha) => apply_wip(dest, sha, "the branches swapped")
                .err()
                .map(|e| format!("{e:#}")),
            None => None,
        };
        return Ok(MovedOut {
            branch,
            base: base.to_string(),
            created: true,
            wip_error,
        });
    }
    // Put main back the way it was, work included. Only correct while nothing else
    // has moved, which is why it is not called after the worktree exists.
    let undo = |mut err: anyhow::Error| -> anyhow::Error {
        if let Err(back) = switch_branch(main, &branch) {
            return err.context(format!("and main is left on {base}: {back:#}"));
        }
        if let Some(sha) = &wip {
            if let Err(back) = apply_wip(main, sha, "the branches swapped") {
                err = err.context(format!(
                    "and its uncommitted work is still in commit {sha}: {back:#}"
                ));
            }
        }
        err
    };

    if let Err(e) = switch_branch(main, base) {
        let e = e.context(format!("the main checkout could not go back to {base}"));
        // Nothing to switch back — it never left — so only the work needs restoring.
        if let Some(sha) = &wip {
            if let Err(back) = apply_wip(main, sha, "the branches swapped") {
                return Err(e.context(format!(
                    "and its uncommitted work is still in commit {sha}: {back:#}"
                )));
            }
        }
        return Err(e);
    }

    if let Err(e) = worktree_add_existing(main, dest, &branch) {
        return Err(undo(
            e.context(format!("no worktree could be cut for {branch}")),
        ));
    }

    let wip_error = match &wip {
        Some(sha) => apply_wip(dest, sha, "the branches swapped")
            .err()
            .map(|e| format!("{e:#}")),
        None => None,
    };
    Ok(MovedOut {
        branch,
        base: base.to_string(),
        created: false,
        wip_error,
    })
}

/// `stem`, or the first `stem-N` no branch has taken.
///
/// A worktree cut for work that had no branch is named after the tree, and a tree
/// deleted long ago can leave its branch behind — so the free directory the caller
/// found does not on its own mean the branch is free too.
pub(super) fn free_branch(main: &Path, stem: &str) -> String {
    if !branch_exists(main, stem) {
        return stem.to_string();
    }
    for n in 2..100 {
        let candidate = format!("{stem}-{n}");
        if !branch_exists(main, &candidate) {
            return candidate;
        }
    }
    stem.to_string()
}

/// Detach a tree at its current commit, releasing the branch it held.
pub(super) fn switch_detach(cwd: &Path) -> Result<()> {
    git(cwd, &["switch", "--detach", "-q"]).map(|_| ())
}

/// Untracked paths in a tree, which a swap cannot carry.
///
/// `stash create` banks tracked changes only, so these stay where they are. Named
/// so the caller can say which, rather than leaving you to notice that half your
/// work did not travel.
pub fn untracked_in(cwd: &Path, exclude: Option<&str>) -> Result<Vec<String>> {
    let set = status(cwd, exclude, Untracked::Each)?;
    Ok(set.untracked.iter().map(|f| f.path.clone()).collect())
}

/// Bank a tree's uncommitted work as a commit object, then clean the tree.
///
/// `stash create` rather than `stash push`, deliberately: it writes the WIP commit
/// and hands back its sha **without touching `refs/stash`**. That stack is shared
/// by every worktree of the repo and other sessions push and pop it concurrently,
/// so a swap that used it could hand someone else's work to the wrong tree.
///
/// `None` means the tree was clean and nothing was touched — including no reset,
/// which is what keeps a clean swap from ever running a destructive command.
///
/// Tracked changes only, staged and unstaged. `stash create` has no
/// `--include-untracked`, so untracked files stay where they are; the caller says
/// so rather than pretending they moved.
pub(super) fn capture_wip(cwd: &Path) -> Result<Option<String>> {
    let Some(sha) = create_wip(cwd)? else {
        return Ok(None);
    };
    git(cwd, &["reset", "--hard", "-q"])?;
    Ok(Some(sha))
}

/// The banking half on its own: write the WIP commit and prove it resolves.
///
/// Split out because [`bank_wip`] has to put a **ref** on that object before the
/// reset, and the verify is the line that must not be skipped either way: a
/// `reset --hard` against a sha that does not resolve is the one way any of this
/// destroys the work it exists to carry.
pub(super) fn create_wip(cwd: &Path) -> Result<Option<String>> {
    let sha = git(cwd, &["stash", "create"])?.trim().to_string();
    if sha.is_empty() {
        return Ok(None);
    }
    if !git_ok(cwd, &["cat-file", "-e", &format!("{sha}^{{commit}}")]) {
        bail!("git stash create returned {sha}, which does not resolve — refusing to reset");
    }
    Ok(Some(sha))
}

/// Copy a tree's uncommitted work into another tree, leaving the source as it was.
///
/// The fork's half of the pair. [`capture_wip`] banks and then **resets**, which is
/// right when a branch is leaving and wrong here: a fork is a second place to work,
/// not a move, and the parent must still have its edits when you look back at it.
/// `stash create` alone does exactly that — it writes the commit object and does not
/// touch the working tree — so this is the same banking without the reset.
///
/// Reaching the object from the other tree needs nothing special: worktrees of one
/// repository share `.git/objects`, so the dangling commit `stash create` returns is
/// visible in both.
///
/// `None` means the parent was clean and there was nothing to carry. Tracked changes
/// only, as ever: `stash create` has no `--include-untracked`, so the caller names
/// what stayed behind rather than pretending it travelled.
pub fn copy_wip(from: &Path, to: &Path) -> Result<Option<String>> {
    let sha = git(from, &["stash", "create"])?.trim().to_string();
    if sha.is_empty() {
        return Ok(None);
    }
    // Never apply a sha that does not resolve. Cheap, and the same guard
    // `capture_wip` needs for a much worse reason.
    if !git_ok(from, &["cat-file", "-e", &format!("{sha}^{{commit}}")]) {
        bail!("git stash create returned {sha}, which does not resolve");
    }
    git(to, &["stash", "apply", "--index", &sha]).with_context(|| {
        format!(
            "the fork was cut but the uncommitted work did not re-apply in {}; it is \
             banked in commit {sha} — `git stash apply {sha}` there to recover it",
            to.display()
        )
    })?;
    Ok(Some(sha))
}

/// Re-apply banked work onto whatever this tree now has checked out.
///
/// `what_happened` opens the failure message, because the recovery advice is the
/// same for every caller and the cause is not: a swap and a fork both leave the work
/// in the commit, and the sentence has to say which one you are looking at.
pub(super) fn apply_wip(cwd: &Path, sha: &str, what_happened: &str) -> Result<()> {
    git(cwd, &["stash", "apply", "--index", sha])
        .with_context(|| {
            format!(
                "{what_happened} but the uncommitted work did not re-apply in {}; \
                 it is still in commit {sha} — `git stash apply {sha}` there to recover it",
                cwd.display()
            )
        })
        .map(|_| ())
}
