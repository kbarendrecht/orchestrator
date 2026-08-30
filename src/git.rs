use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::model::{ChangedFile, FileSet, FileStatus};

/// Shell out to `git` rather than a library binding — you need fsmonitor and
/// the real worktree/remote semantics (§1).
fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Like [`git`] but returns the raw bytes, for `-z` output that is not valid
/// UTF-8 in the general case.
fn git_raw(cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Whether a git command succeeded, for probes where failure is a valid answer.
fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// How much detail to ask for about untracked files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Untracked {
    /// One entry per untracked *directory*. What the rail wants: it runs on every
    /// hook event, for every workspace, including main — whose tree contains every
    /// worktree — and the result is broadcast to every client. One un-ignored build
    /// directory would otherwise become ten thousand rows in every snapshot.
    Collapsed,
    /// One entry per untracked *file*.
    ///
    /// What anything deciding what to *commit* needs. A collapsed `newdir/` cannot
    /// be counted, cannot be diffed, and cannot be refused one file at a time — so
    /// the manual phase listed a directory, showed none of it, and `git add -A`
    /// committed everything inside.
    Each,
}

/// Changed files for a workspace, grouped staged / unstaged / untracked (§4).
///
/// `exclude` is `Some(prefix)` for main: main's file tree contains every
/// worktree, so without dropping paths under the worktrees dir you see every
/// sibling session's work (§2). The prefix is the repo-relative worktrees
/// subdir (`Config::worktrees_subdir_str`), so it follows a relocated layout.
pub fn status(cwd: &Path, exclude: Option<&str>, untracked: Untracked) -> Result<FileSet> {
    let mode = match untracked {
        Untracked::Collapsed => "--untracked-files=normal",
        Untracked::Each => "--untracked-files=all",
    };
    let raw = git_raw(cwd, &["status", "--porcelain=v2", mode, "-z"])?;
    Ok(parse_status(&raw, exclude))
}

fn parse_status(raw: &[u8], exclude: Option<&str>) -> FileSet {
    let mut set = FileSet::default();
    let mut records = raw
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| String::from_utf8_lossy(r).into_owned())
        .peekable();

    while let Some(rec) = records.next() {
        let mut chars = rec.chars();
        let tag = chars.next().unwrap_or(' ');
        match tag {
            // Ordinary changed entry:
            //   1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
            '1' => {
                let fields: Vec<&str> = rec.splitn(9, ' ').collect();
                if fields.len() < 9 {
                    continue;
                }
                push_xy(&mut set, fields[1], fields[8], exclude);
            }
            // Renamed or copied:
            //   2 <XY> ... <X><score> <path>\0<origPath>
            // The original path is its own NUL-separated record and is consumed
            // here so it is not mistaken for an entry of its own.
            '2' => {
                let fields: Vec<&str> = rec.splitn(10, ' ').collect();
                records.next();
                if fields.len() < 10 {
                    continue;
                }
                push_xy(&mut set, fields[1], fields[9], exclude);
            }
            // Unmerged. Both sides count as unstaged work.
            'u' => {
                let fields: Vec<&str> = rec.splitn(11, ' ').collect();
                if let Some(path) = fields.last() {
                    if !skip(path, exclude) {
                        set.unstaged.push(ChangedFile {
                            path: (*path).to_string(),
                            status: FileStatus::Unstaged,
                            code: "UU".to_string(),
                        });
                    }
                }
            }
            '?' => {
                let path = rec.strip_prefix("? ").unwrap_or("");
                if !path.is_empty() && !skip(path, exclude) {
                    set.untracked.push(ChangedFile {
                        path: path.to_string(),
                        status: FileStatus::Untracked,
                        code: "??".to_string(),
                    });
                }
            }
            // '!' is ignored-file output, which is not requested here.
            _ => {}
        }
    }

    set.staged.sort_by(|a, b| a.path.cmp(&b.path));
    set.unstaged.sort_by(|a, b| a.path.cmp(&b.path));
    set.untracked.sort_by(|a, b| a.path.cmp(&b.path));
    set
}

/// `XY`: X is the staged status, Y the unstaged one. `.` means unmodified, and
/// a file can legitimately appear in both groups.
fn push_xy(set: &mut FileSet, xy: &str, path: &str, exclude: Option<&str>) {
    if skip(path, exclude) {
        return;
    }
    let mut it = xy.chars();
    let x = it.next().unwrap_or('.');
    let y = it.next().unwrap_or('.');
    if x != '.' {
        set.staged.push(ChangedFile {
            path: path.to_string(),
            status: FileStatus::Staged,
            code: xy.to_string(),
        });
    }
    if y != '.' {
        set.unstaged.push(ChangedFile {
            path: path.to_string(),
            status: FileStatus::Unstaged,
            code: xy.to_string(),
        });
    }
}

fn skip(path: &str, exclude: Option<&str>) -> bool {
    exclude.is_some_and(|prefix| path.starts_with(prefix))
}

// ---------------------------------------------------------------------------
// Refs
// ---------------------------------------------------------------------------

pub fn current_branch(cwd: &Path) -> Result<String> {
    Ok(git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string())
}

/// The real `HEAD` file for this workspace, wherever git keeps it.
///
/// Not `<cwd>/.git/HEAD`: in a linked worktree `<cwd>/.git` is a pointer *file*,
/// and the HEAD that a checkout rewrites lives under the common dir at
/// `<main>/.git/worktrees/<id>/HEAD`. `--absolute-git-dir` resolves both — the
/// checkout's `.git` for main, the worktree's admin dir for a worktree — so the
/// HEAD poller watches the file that actually moves.
pub fn head_file(cwd: &Path) -> Result<PathBuf> {
    let dir = git(cwd, &["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(dir.trim()).join("HEAD"))
}

pub fn head_sha(cwd: &Path) -> Result<String> {
    Ok(git(cwd, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// Does this branch hold commits its remote does not?
///
/// For when GitHub could not say what the remote head is. `@{upstream}` is whatever
/// the branch tracks; with no tracking branch there is nothing to compare against, so
/// the honest answer is "assume yes" — an unnecessary push is refused by the lease,
/// while a skipped one posts a reply about a commit nobody can see.
pub fn has_unpushed(cwd: &Path, branch: &str) -> bool {
    for range in [
        format!("@{{upstream}}..{branch}"),
        format!("origin/{branch}..{branch}"),
    ] {
        if let Ok(out) = Command::new("git")
            .args(["rev-list", "--count", &range])
            .current_dir(cwd)
            .output()
        {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).trim() != "0";
            }
        }
    }
    true
}

/// Who git will author a commit as here.
///
/// Wanted by [`amend_target`], which refuses to fold into a commit somebody else
/// wrote. Empty rather than an error when git has no `user.email`: an unset
/// identity means "match nobody", which degrades the fold to a plain HEAD amend
/// instead of failing the batch.
pub fn user_email(cwd: &Path) -> String {
    Command::new("git")
        .args(["config", "user.email"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Two-dot against the merge-base *commit*, not the ref, or develop's own
/// commits appear as your deletions (§5).
pub fn merge_base(cwd: &Path, upstream: &str) -> Result<String> {
    Ok(git(cwd, &["merge-base", upstream, "HEAD"])?
        .trim()
        .to_string())
}

/// Repo config from §4. fsmonitor is set on main only: a daemon per worktree
/// over a large monorepo puts the `inotify.max_user_watches` question straight
/// back, and worktree reconciles are event-driven off hooks anyway.
pub fn configure_repo(main: &Path) -> Result<()> {
    let _ = git(main, &["config", "core.fsmonitor", "true"]);
    let _ = git(main, &["config", "core.untrackedCache", "true"]);
    let _ = git(main, &["config", "fetch.writeCommitGraph", "true"]);
    Ok(())
}

// ---------------------------------------------------------------------------
// Unpushed work
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Unpushed {
    /// No remote counterpart, so nothing on this branch has ever been pushed.
    /// `commits` is what it carries beyond the upstream base — the work that
    /// would actually be lost.
    NeverPushed {
        commits: Vec<String>,
    },
    Ahead {
        commits: Vec<String>,
    },
    UpToDate,
}

impl Unpushed {
    /// Blocks whenever there are commits that exist nowhere else.
    ///
    /// The spec's rule is "no remote counterpart means every commit is
    /// unpushed, block" (§2). Taken literally that also blocks a *fresh*
    /// worktree, which is branched straight off `upstream/develop` and carries
    /// nothing — it could never be removed again. Counting the commits beyond
    /// the base keeps the fail-closed behaviour for real work and drops only
    /// the false positive.
    pub fn blocks_teardown(&self) -> bool {
        match self {
            Unpushed::UpToDate => false,
            Unpushed::NeverPushed { commits } => !commits.is_empty(),
            Unpushed::Ahead { .. } => true,
        }
    }
}

/// Resolve the fork branch explicitly (§2).
///
/// `@{push}` does not resolve on a branch that was never pushed, and `@{u}`
/// resolves to `upstream/develop` — neither answers the question.
pub fn unpushed(cwd: &Path, branch: &str, upstream: &str) -> Result<Unpushed> {
    let remote_ref = format!("refs/remotes/origin/{branch}");
    if !git_ok(cwd, &["rev-parse", "--verify", "--quiet", &remote_ref]) {
        // Nothing on origin, so measure against the upstream base instead:
        // that is exactly the set of commits that exist nowhere but here.
        let range = format!("{upstream}..HEAD");
        let commits = git(cwd, &["log", &range, "--oneline"])
            .map(|out| out.lines().map(|l| l.trim().to_string()).collect())
            .unwrap_or_else(|_| vec!["(could not resolve the upstream base)".to_string()]);
        return Ok(Unpushed::NeverPushed { commits });
    }
    let range = format!("origin/{branch}..HEAD");
    let out = git(cwd, &["log", &range, "--oneline"])?;
    let commits: Vec<String> = out.lines().map(|l| l.trim().to_string()).collect();
    if commits.is_empty() {
        Ok(Unpushed::UpToDate)
    } else {
        Ok(Unpushed::Ahead { commits })
    }
}

pub fn is_clean(cwd: &Path) -> Result<bool> {
    let raw = git_raw(cwd, &["status", "--porcelain"])?;
    Ok(raw.iter().all(|b| b.is_ascii_whitespace()))
}

/// Is this tree clean, ignoring one path prefix?
///
/// [`is_clean`] counts untracked files, which is right for a worktree and wrong
/// for **main**: main *contains* the worktrees dir, so on any repo that has not
/// gitignored `.claude/worktrees/` main is permanently dirty and every gate that
/// asks "is main clean" refuses forever. That is invisible on a repo whose own
/// hooks add the exclude, which is exactly how it stayed unnoticed.
///
/// So: `is_clean` for a worktree, this for main, with
/// `Config::worktrees_subdir_str` as the prefix — the same exclude `reconcile`
/// already passes to [`status`] for the changed-file pane.
/// `Untracked::Each`, not `Collapsed`, and that is the whole trick. Git collapses
/// an untracked directory to its topmost entry, so a repo where nothing under
/// `.claude/` is tracked reports `.claude/` — *above* the exclude prefix, which
/// therefore never matches and main reads dirty anyway. Listing every untracked
/// file costs more, but this runs at a gate, not on every poll.
pub fn is_clean_excluding(cwd: &Path, exclude: Option<&str>) -> Result<bool> {
    let set = status(cwd, exclude, Untracked::Each)?;
    Ok(set.staged.is_empty() && set.unstaged.is_empty() && set.untracked.is_empty())
}

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

/// The pid holding a worktree's lock, if it is locked and the lock reason names
/// one. `claude --worktree` writes `claude session <name> (pid <PID> start <N>)`,
/// which is the only lock this daemon ever expects to see — a plain checkout
/// never locks a worktree itself. Returns `None` when the worktree is not
/// locked or the reason carries no `pid`, both of which mean "do not touch it".
fn stale_lock_pid(main: &Path, path: &Path) -> Option<u32> {
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

/// Add a worktree checked out on an **existing** branch.
///
/// `claude --worktree` always cuts a fresh `worktree-<name>` from
/// `upstream/develop`, which is wrong for `/resolve`: that has to land on the
/// PR's own head branch (§8). The repo's `worktree-create` hook is therefore
/// not involved here, but `worktree-link` still runs at `SessionStart` and does
/// the symlinks, which is the same path §2 describes for rebuilding a worktree
/// on resume.
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
    // A base that does not resolve would otherwise cut from HEAD silently, which
    // is a different branch than the caller asked for.
    if !git_ok(main, &["rev-parse", "--verify", "--quiet", base]) {
        bail!("base ref {base} does not resolve");
    }
    git(main, &["worktree", "add", &path_str, "-b", branch, base])?;
    Ok(())
}

pub fn worktree_add_existing(main: &Path, path: &Path, branch: &str) -> Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    if branch_exists(main, branch) {
        git(main, &["worktree", "add", &path_str, branch])?;
        return Ok(());
    }
    // The head ref lives on the fork, since PRs are opened from origin (§6).
    let _ = git(main, &["fetch", "origin", branch, "--no-tags"]);
    let remote = format!("origin/{branch}");
    if !git_ok(main, &["rev-parse", "--verify", "--quiet", &remote]) {
        bail!("branch {branch} exists neither locally nor on origin");
    }
    git(main, &["worktree", "add", &path_str, "-b", branch, &remote])?;
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
        // The head ref lives on the fork, since PRs are opened from origin (§6).
        let _ = git(main, &["fetch", "origin", branch, "--no-tags"]);
        let remote = format!("origin/{branch}");
        if git_ok(main, &["rev-parse", "--verify", "--quiet", &remote]) {
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
                &recorded[..recorded.len().min(7)]
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
    // The head ref lives on the fork, since PRs are opened from origin (§6).
    let _ = git(cwd, &["fetch", "origin", branch, "--no-tags"]);
    let remote = format!("origin/{branch}");
    if !git_ok(cwd, &["rev-parse", "--verify", "--quiet", &remote]) {
        bail!("branch {branch} exists neither locally nor on origin");
    }
    git(cwd, &["switch", "-c", branch, "--track", &remote])?;
    Ok(())
}

pub fn branch_exists(main: &Path, branch: &str) -> bool {
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
/// question — see [`unpushed`], which had to say the same thing.
///
/// With no remote counterpart the measure falls back to the commits beyond the
/// base, the same reading [`unpushed`]'s `NeverPushed` arm takes: on a branch that
/// was never pushed, everything it added is unpushed. Zero rather than an error
/// when git cannot answer, since this feeds a count in an overview and a
/// transient failure should not read as work to push.
pub fn unpushed_count(cwd: &Path, branch: &str, upstream: &str) -> u32 {
    let remote_ref = format!("refs/remotes/origin/{branch}");
    let range = if git_ok(cwd, &["rev-parse", "--verify", "--quiet", &remote_ref]) {
        format!("origin/{branch}..HEAD")
    } else {
        format!("{upstream}..HEAD")
    };
    git(cwd, &["rev-list", "--count", &range])
        .ok()
        .and_then(|out| out.trim().parse().ok())
        .unwrap_or(0)
}

/// Whether a rebase is stopped part-way in this worktree.
///
/// Checked on disk rather than inferred: a button that offers to rebase a tree
/// already mid-rebase would make a mess that is annoying to unpick.
pub fn rebase_in_progress(cwd: &Path) -> bool {
    let Ok(dir) = git(cwd, &["rev-parse", "--path-format=absolute", "--git-dir"]) else {
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
    let out = Command::new("git")
        .args(["rebase", upstream])
        .current_dir(cwd)
        .output()
        .context("running git rebase")?;
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
    bail!(
        "rebase failed: {}",
        stderr
            .lines()
            .chain(stdout.lines())
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output")
    );
}

pub fn rebase_abort(cwd: &Path) -> Result<()> {
    git(cwd, &["rebase", "--abort"])?;
    Ok(())
}

pub fn conflicted_files(cwd: &Path) -> Result<Vec<String>> {
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
        git(main, &upstream_fetch_argv(upstream_ref))?;
        return Ok(());
    }
    // Steady state: the symref is already recorded, so fetch just the branch it
    // names — no dearer than the named case. If that fetch fails the recorded
    // branch is gone (renamed or deleted upstream), so fall through and re-record.
    if let Some(b) = default_branch(main, remote) {
        if git(main, &["fetch", remote, &b, "--no-tags"]).is_ok() {
            return Ok(());
        }
        tracing::debug!("{remote}/HEAD named {b}, which no longer fetches; re-recording");
    }
    // Bootstrap: `git remote set-head -a` picks from the remote-tracking refs, so
    // they have to exist first — which is why this fetches the whole remote
    // before recording. Only the first poll on a checkout pays for it.
    git(main, &["fetch", remote, "--no-tags"])?;
    if let Err(e) = git(main, &["remote", "set-head", remote, "-a"]) {
        // Not fatal: the refs are fetched, only the symref is missing, and the
        // caller's merge-base will report the real problem.
        tracing::debug!("could not record {remote}/HEAD: {e:#}");
    }
    Ok(())
}

/// `remote/branch`, defaulting the remote to `origin` for a bare branch name.
/// `split_once` keeps a nested branch like `origin/release/2026` intact.
fn split_upstream(upstream_ref: &str) -> (&str, &str) {
    upstream_ref.split_once('/').unwrap_or(("origin", upstream_ref))
}

/// The branch `<remote>/HEAD` points at, as a plain name (`main`), or `None`
/// when the symref does not exist.
fn default_branch(main: &Path, remote: &str) -> Option<String> {
    let out = git(main, &["symbolic-ref", "--short", &format!("refs/remotes/{remote}/HEAD")]).ok()?;
    let full = out.trim();
    // `origin/main` -> `main`.
    Some(full.strip_prefix(&format!("{remote}/"))?.to_string())
}

fn has_remote(main: &Path, name: &str) -> bool {
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

/// The branch part of `upstream_ref`, e.g. `develop` for `upstream/develop`.
///
/// Pure string work, so for `origin/HEAD` it answers `HEAD` — which is a symref,
/// not a branch anyone can check out. Anything that *checks the base out* wants
/// [`base_checkout_branch`] instead; this is for naming it.
pub fn base_branch(upstream_ref: &str) -> &str {
    split_upstream(upstream_ref).1
}

/// The local branch to check out for `upstream_ref`, resolving a `HEAD` symref.
///
/// `git switch HEAD` fails outright — "a branch is expected" — so a base ref of
/// `origin/HEAD`, which is the default, needs the name behind the symref before
/// it can be checked out. That needs the repo, which is why this is separate from
/// [`base_branch`] rather than folded into it.
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
        carried = carried.and(apply_wip(main, wip));
    }
    if let Some(wip) = &main_wip {
        carried = carried.and(apply_wip(worktree, wip));
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
pub fn move_branch_out(
    main: &Path,
    dest: &Path,
    base: &str,
    new_branch: &str,
) -> Result<MovedOut> {
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
                if let Err(back) = apply_wip(main, sha) {
                    err = err.context(format!(
                        "and its uncommitted work is still in commit {sha}: {back:#}"
                    ));
                }
            }
            return Err(err);
        }
        let wip_error = match &wip {
            Some(sha) => apply_wip(dest, sha).err().map(|e| format!("{e:#}")),
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
            if let Err(back) = apply_wip(main, sha) {
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
            if let Err(back) = apply_wip(main, sha) {
                return Err(e.context(format!(
                    "and its uncommitted work is still in commit {sha}: {back:#}"
                )));
            }
        }
        return Err(e);
    }

    if let Err(e) = worktree_add_existing(main, dest, &branch) {
        return Err(undo(e.context(format!("no worktree could be cut for {branch}"))));
    }

    let wip_error = match &wip {
        Some(sha) => apply_wip(dest, sha).err().map(|e| format!("{e:#}")),
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
fn free_branch(main: &Path, stem: &str) -> String {
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
fn switch_detach(cwd: &Path) -> Result<()> {
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
fn capture_wip(cwd: &Path) -> Result<Option<String>> {
    let sha = git(cwd, &["stash", "create"])?.trim().to_string();
    if sha.is_empty() {
        return Ok(None);
    }
    // Only reset once the object is real. A `reset --hard` against a sha that does
    // not resolve is the one way this could destroy the work it exists to carry.
    if !git_ok(cwd, &["cat-file", "-e", &format!("{sha}^{{commit}}")]) {
        bail!("git stash create returned {sha}, which does not resolve — refusing to reset");
    }
    git(cwd, &["reset", "--hard", "-q"])?;
    Ok(Some(sha))
}

/// Re-apply banked work onto whatever this tree now has checked out.
fn apply_wip(cwd: &Path, sha: &str) -> Result<()> {
    git(cwd, &["stash", "apply", "--index", sha])
        .with_context(|| {
            format!(
                "the branches swapped but the uncommitted work did not re-apply in {}; \
                 it is still in commit {sha} — `git stash apply {sha}` there to recover it",
                cwd.display()
            )
        })
        .map(|_| ())
}

/// Put a checkout back on `base`, unless something says not to.
///
/// Returns the branch it left, or `None` when it did nothing: already there, or
/// carrying uncommitted work. Refusing on a dirty tree is the whole safety
/// argument — a checkout takes uncommitted changes with it, and finding your work
/// sitting on the base branch is not recoverable by pressing back.
///
/// Blocking and repo-only on purpose: the daemon-side caller owns the "is anyone
/// still working here" half, and this half is what a test can drive against a real
/// repo.
pub fn park_on_base(cwd: &Path, base: &str, exclude: Option<&str>) -> Result<Option<String>> {
    let on = current_branch(cwd)?;
    if on == base {
        return Ok(None);
    }
    // Excluding, not plain: this only ever runs on main, which contains the
    // worktrees dir. Plain `is_clean` reads that as dirty and parking never
    // happens — silently, since "not clean" is a legitimate reason to do nothing.
    if !is_clean_excluding(cwd, exclude)? {
        return Ok(None);
    }
    switch_branch(cwd, base)?;
    Ok(Some(on))
}

/// Split out so the named-branch rule is testable without a network fetch.
fn upstream_fetch_argv(upstream_ref: &str) -> Vec<&str> {
    let (remote, branch) = split_upstream(upstream_ref);
    vec!["fetch", remote, branch, "--no-tags"]
}

// ---------------------------------------------------------------------------
// The review flow's writes
// ---------------------------------------------------------------------------

/// Push the PR's own branch, refusing to clobber anyone else's work.
///
/// `--force-with-lease` rather than `--force`: it fails when the remote moved
/// since the last fetch, which is exactly the "someone else pushed" case that
/// must not be overwritten. Never `-u`: rebinding upstream to origin breaks pull
/// tracking in a triangular remote setup.
///
/// `base` is the branch this checkout is measured against, from `upstream_ref`.
/// The agent-side guard ([`crate::guard`]) is a `PreToolUse` hook on **Bash**, so
/// a daemon-side push never passes through it and the rule has to be re-stated
/// here or it is simply not enforced. Its other rule — plain `--force` — is
/// structurally impossible below, because the command is a fixed string.
///
/// This used to be a hardcoded `["develop", "main", "master", "release"]`, which
/// was wrong in both directions: it let a push to a base called `trunk` through,
/// and refused an ordinary feature branch that happened to be named `release`.
/// `None` is "no resolvable base", and refuses nothing.
pub fn push_with_lease(cwd: &Path, branch: &str, base: Option<&str>) -> Result<()> {
    if base == Some(branch) {
        bail!("refusing to push to {branch}: it is the base branch, open a PR instead");
    }
    let out = Command::new("git")
        .args(["push", "--force-with-lease", "origin", branch])
        .current_dir(cwd)
        .output()
        .context("running git push")?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // The lease failing is the one refusal worth naming: it means the remote
    // moved, and the fix is to look at both sides rather than push harder.
    if err.contains("stale info") || err.contains("fetch first") || err.contains("rejected") {
        bail!(
            "push refused: {branch} moved on origin since this review started. \
             Someone else pushed, or fix-pr ran. Re-triage rather than overwrite it."
        );
    }
    bail!(
        "push failed: {}",
        err.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no output")
    );
}

/// Who last touched a line, and who wrote that commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blame {
    pub sha: String,
    pub author_email: String,
}

/// `git blame` one line of the **committed** state.
///
/// `rev` is which committed state. `None` blames the working tree, which is only
/// correct *before* anything has been applied — a dirty tree attributes the line
/// to the uncommitted change (an all-zero sha), which is nobody's commit and
/// cannot be a fixup target, so this returns `None` for it.
///
/// `Some("HEAD")` reads through a dirty tree, which is what the manual phase
/// needs: the human has already edited the very line being blamed, so blaming the
/// working tree would degrade every manual fold to a plain HEAD amend and the pass
/// would never do its job. Measured: `git blame HEAD -L n,n` returns the owning
/// commit where the bare form returns all zeros.
pub fn blame_line(cwd: &Path, rev: Option<&str>, path: &str, line: u32) -> Result<Option<Blame>> {
    let range = format!("{line},{line}");
    let mut args = vec!["blame"];
    if let Some(r) = rev {
        args.push(r);
    }
    args.extend_from_slice(&["-L", &range, "--porcelain", "--", path]);
    let out = Command::new("git")
        .args(&args)
        .current_dir(cwd)
        .output()
        .context("running git blame")?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let sha = match lines.next().and_then(|l| l.split_whitespace().next()) {
        Some(s) if !s.chars().all(|c| c == '0') => s.to_string(),
        // All-zero means "not committed yet".
        _ => return Ok(None),
    };
    let email = text
        .lines()
        .find_map(|l| l.strip_prefix("author-mail "))
        .map(|m| m.trim_matches(['<', '>']).to_string())
        .unwrap_or_default();
    Ok(Some(Blame {
        sha,
        author_email: email,
    }))
}

/// Who git will actually author a commit as, asked of git rather than of config.
///
/// `git config user.email` is empty in a container that never set one — and git
/// commits there anyway, as `you@hostname`. Reading the config would report "no
/// identity", which the authorship checks below would take to mean *every* commit
/// belongs to somebody else.
pub fn effective_email(cwd: &Path) -> Option<String> {
    let ident = Command::new("git")
        .args(["var", "GIT_AUTHOR_IDENT"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())?;
    // `Name <email> 1699999999 +0200`
    let email = ident
        .split('<')
        .nth(1)?
        .split('>')
        .next()?
        .trim()
        .to_string();
    (!email.is_empty()).then(|| email.to_lowercase())
}

/// Every author `git log <args>` selects, lowercased.
///
/// `None` when git could not answer. The callers read that as "cannot tell", which
/// has to degrade rather than proceed: an empty list would mean "nobody else is
/// involved" and authorise a rewrite. Takes the arguments as a slice because
/// `["-1 HEAD"]` is one argument git cannot parse — which is how the first draft of
/// this check silently never fired.
pub(crate) fn authors_in(cwd: &Path, args: &[&str]) -> Option<Vec<String>> {
    let mut argv = vec!["log", "--format=%ae"];
    argv.extend_from_slice(args);
    let out = Command::new("git")
        .args(&argv)
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_lowercase())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

/// Does `rev` have more than one parent?
pub(crate) fn is_merge(cwd: &Path, rev: &str) -> bool {
    Command::new("git")
        .args(["rev-list", "--parents", "-n", "1", rev])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .count()
                > 2
        })
        .unwrap_or(false)
}

pub(crate) fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Does this revision resolve?
pub(crate) fn rev_exists(cwd: &Path, rev: &str) -> bool {
    git_ok(cwd, &["rev-parse", "--verify", "--quiet", rev])
}

/// Is `a` an ancestor of `b`? Exit status only, so a failure means "no".
pub(crate) fn is_ancestor(cwd: &Path, a: &str, b: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", a, b])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Commit everything staged-or-not into the shape [`Amend`] chose.
///
/// `Fixup` writes a `fixup!` commit and then autosquashes it away, so the PR's
/// history keeps one commit per change rather than growing a "fix review" commit.
/// **`--autosquash` is silently ignored without `-i`**, which is why the
/// sequence editor is stubbed out rather than the flag used alone.
///
/// A conflict during the rebase aborts and reports: there is no session attached
/// to a button press to resolve one, and leaving a stopped rebase behind would
/// strand the worktree.
pub fn fold_in(cwd: &Path, amend: &crate::review_commit::Amend) -> Result<()> {
    use crate::review_commit::Amend;
    git(cwd, &["add", "-A"])?;
    match amend {
        Amend::OnTop(why) => {
            // `--amend` succeeds on an empty staged diff; `commit -m` does not, and
            // a hook that reverted an edit back to HEAD's content would otherwise
            // turn a silent success into a hard error.
            if git_ok(cwd, &["diff", "--cached", "--quiet"]) {
                return Ok(());
            }
            // Never `fixup!`/`squash!`: a later batch's autosquash over this range
            // would silently absorb it.
            git(cwd, &["commit", "-m", &format!("review batch: {why}")])?;
            Ok(())
        }
        Amend::Head(_) => {
            git(cwd, &["commit", "--amend", "--no-edit"])?;
            Ok(())
        }
        Amend::Fixup(sha) => {
            git(cwd, &["commit", "--fixup", sha])?;
            let out = Command::new("git")
                .args(["rebase", "-i", "--autosquash", &format!("{sha}~1")])
                .env("GIT_SEQUENCE_EDITOR", "true")
                .env("GIT_EDITOR", "true")
                .current_dir(cwd)
                .output()
                .context("running autosquash rebase")?;
            if out.status.success() {
                return Ok(());
            }
            let files = conflicted_files(cwd).unwrap_or_default();
            if rebase_in_progress(cwd) {
                let _ = rebase_abort(cwd);
            }
            bail!(
                "could not fold into {}: the rebase conflicted{}. The change is still \
                 committed on top; fold it by hand or leave it.",
                short(sha),
                if files.is_empty() {
                    String::new()
                } else {
                    format!(" in {}", files.join(", "))
                }
            );
        }
    }
}

/// Commit the worktree as it stands — the gate's `commit…` button.
/// One commit's own diff, for showing what an agent actually wrote.
///
/// `--format=` so the header does not ride along: the card wants the change, not
/// the message it already knows. Capped, because a commit is agent output and the
/// card is not the place to render a generated file.
pub fn commit_diff(cwd: &Path, sha: &str, max: usize) -> Result<String> {
    let out = git(cwd, &["show", "--format=", "--no-color", sha])?;
    Ok(match out.char_indices().nth(max) {
        Some((cut, _)) => format!("{}\n… truncated", &out[..cut]),
        None => out,
    })
}

pub fn commit_all(cwd: &Path, message: &str) -> Result<()> {
    anyhow::ensure!(!message.trim().is_empty(), "a commit needs a message");
    git(cwd, &["add", "-A"])?;
    git(cwd, &["commit", "-m", message])?;
    Ok(())
}

/// Stash the worktree — the gate's `stash` button.
///
/// Never popped automatically: popping onto a branch the review just amended can
/// conflict, and silently juggling your uncommitted work is worse than leaving it
/// where you put it. Untracked files go too, or the tree is not actually clean.
pub fn stash(cwd: &Path) -> Result<()> {
    git(
        cwd,
        &[
            "stash",
            "push",
            "--include-untracked",
            "-m",
            "orchd: before a review batch",
        ],
    )?;
    Ok(())
}

/// What running the repo's pre-commit hooks concluded.
#[derive(Debug, PartialEq, Eq)]
pub enum PreCommit {
    /// No `.pre-commit-config.yaml`, so there is nothing configured to run.
    NotConfigured,
    /// Configured but `pre-commit` is not on PATH. A warning, not a stop: that
    /// is an environment problem, and blocking a whole review on it is worse
    /// than pushing code the local hooks did not see. (CI still runs them.)
    NotInstalled,
    /// Every hook passed and nothing was rewritten.
    Passed,
    /// A hook failed. A hard stop — the daemon cannot fix a lint error.
    Failed(String),
    /// The hooks passed but **rewrote files**. Also a stop: what would land is
    /// no longer what was approved on the cards, and absorbing the difference
    /// silently is exactly what the design refuses. The paths are handed back so
    /// the extra delta can be shown.
    Reformatted(Vec<String>),
}

/// Run the repo's pre-commit hooks over the files just written.
///
/// Detected rather than configured: if `.pre-commit-config.yaml` is absent there
/// is nothing to run. Scoped with `--files` rather than `--all-files`, because the
/// batch is answerable for what it wrote and not for the rest of the tree.
///
/// Called with the patches applied but **not yet committed**, so a rewrite can be
/// refused with nothing to undo.
pub fn pre_commit(cwd: &Path, files: &[String]) -> Result<PreCommit> {
    if !cwd.join(".pre-commit-config.yaml").exists() {
        return Ok(PreCommit::NotConfigured);
    }
    if files.is_empty() {
        return Ok(PreCommit::Passed);
    }

    // Hashes before and after: the only reliable way to tell "passed" from
    // "passed and rewrote your file" is to look.
    let before = hash_files(cwd, files);

    let mut args: Vec<&str> = vec!["run", "--files"];
    args.extend(files.iter().map(String::as_str));
    let out = match Command::new("pre-commit")
        .args(&args)
        .current_dir(cwd)
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PreCommit::NotInstalled),
        Err(e) => return Err(e).context("running pre-commit"),
    };

    let rewritten: Vec<String> = files
        .iter()
        .filter(|f| {
            let h = hash_one(cwd, f);
            before.get(*f).map(|b| b != &h).unwrap_or(false)
        })
        .cloned()
        .collect();
    // **Exit status first.** `fail_fast` is off by default, so a formatter
    // rewriting a file and a linter erroring in the same run is ordinary — and
    // checking `rewritten` first reported that as a mere reformat, made the
    // `Failed` arm below unreachable, and threw the hook's own output away.
    // `write_batch` refuses on either, so it never showed; `write_manual` only
    // logs a reformat, so it committed and pushed code that failed lint.
    if !out.status.success() {
        // Hooks report on stdout; stderr carries pre-commit's own troubles.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail: String = stdout
            .lines()
            .chain(stderr.lines())
            .filter(|l| !l.trim().is_empty())
            .take(20)
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(PreCommit::Failed(detail));
    }
    if !rewritten.is_empty() {
        return Ok(PreCommit::Reformatted(rewritten));
    }
    Ok(PreCommit::Passed)
}

fn hash_files(cwd: &Path, files: &[String]) -> std::collections::HashMap<String, u64> {
    files
        .iter()
        .map(|f| (f.clone(), hash_one(cwd, f)))
        .collect()
}

/// Content hash, not mtime: a hook can rewrite a file to the same bytes, and a
/// checkout can change mtime without changing content (`edit.rs` makes the same
/// choice for the same reason).
fn hash_one(cwd: &Path, rel: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match std::fs::read(cwd.join(rel)) {
        Ok(bytes) => bytes.hash(&mut h),
        // A hook may legitimately delete a file; absent hashes to a constant
        // distinct from any content.
        Err(_) => 0u8.hash(&mut h),
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    // The amend-target cases moved with nothing but their names: the fixture they
    // share with the blame tests stayed here, so they reach across for `Amend`.
    use crate::review_commit::*;

    /// Whether git refused because the branch is checked out in another tree.
    ///
    /// The *refusal* is the invariant two tests here are built on; its wording is
    /// not. Git says "already used by worktree at" from 2.35 and "already checked
    /// out at" before it, so matching one string made both tests fail on an older
    /// git for a reason that had nothing to do with the code — which is the whole
    /// failure mode a test is supposed to rule out.
    fn refused_as_already_checked_out(err: &str) -> bool {
        err.contains("already used by worktree") || err.contains("already checked out")
    }

    /// The assumption `spawn::refuse_if_main_is_on` exists for: git will not check
    /// one branch out into two trees. Pinned here because the guard's whole value
    /// is turning this refusal into a sentence that says what to do, and a git
    /// that stopped refusing would leave two agents on one branch instead.
    #[test]
    fn a_branch_checked_out_in_main_cannot_be_cut_into_a_worktree() {
        let dir = std::env::temp_dir().join(format!(
            "orchd-wt-twice-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("main");
        git(&dir, &["init", "-q", "main"]).unwrap();
        git(&main, &["config", "user.email", "t@t"]).unwrap();
        git(&main, &["config", "user.name", "t"]).unwrap();
        std::fs::write(main.join("a.txt"), "x").unwrap();
        git(&main, &["add", "-A"]).unwrap();
        git(&main, &["commit", "-qm", "init"]).unwrap();
        git(&main, &["checkout", "-q", "-b", "feature/x"]).unwrap();

        assert_eq!(current_branch(&main).unwrap(), "feature/x");
        let err = worktree_add_existing(&main, &dir.join("pr-1"), "feature/x")
            .expect_err("git must refuse a second checkout of one branch");
        assert!(
            refused_as_already_checked_out(&format!("{err:#}")),
            "unexpected refusal: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The guards that stand between "you closed the last pane in main" and
    /// someone's uncommitted work landing on develop.
    #[test]
    fn parking_leaves_a_dirty_checkout_exactly_where_it_is() {
        let dir = std::env::temp_dir().join(format!(
            "orchd-park-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = dir.join("repo");
        git(&dir, &["init", "-q", "-b", "develop", "repo"]).unwrap();
        git(&repo, &["config", "user.email", "t@t"]).unwrap();
        git(&repo, &["config", "user.name", "t"]).unwrap();
        std::fs::write(repo.join("a.txt"), "x").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-qm", "init"]).unwrap();

        // Already home: nothing to do, and no needless checkout.
        assert_eq!(park_on_base(&repo, "develop", None).unwrap(), None);

        git(&repo, &["checkout", "-q", "-b", "feature/x"]).unwrap();
        std::fs::write(repo.join("a.txt"), "edited").unwrap();
        // Dirty: the work would ride along to develop, so it stays put.
        assert_eq!(park_on_base(&repo, "develop", None).unwrap(), None);
        assert_eq!(current_branch(&repo).unwrap(), "feature/x");

        git(&repo, &["checkout", "-q", "--", "a.txt"]).unwrap();
        assert_eq!(
            park_on_base(&repo, "develop", None).unwrap().as_deref(),
            Some("feature/x")
        );
        assert_eq!(current_branch(&repo).unwrap(), "develop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fork layout is the one thing a first run can work out for itself, so it
    /// has to be right about both answers: present, and absent.
    #[test]
    fn a_fork_layout_is_detected_and_nothing_else_is_assumed() {
        let dir = std::env::temp_dir().join(format!(
            "orchd-detect-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = dir.join("repo");
        git(&dir, &["init", "-q", "repo"]).unwrap();
        git(&repo, &["remote", "add", "origin", "git@github.com:you/monorepo.git"]).unwrap();

        // origin alone is not a fork: no opinion, so the generic default stands.
        assert_eq!(detect_base(&repo), None);

        git(&repo, &["remote", "add", "upstream", "git@github.com:acme/monorepo.git"]).unwrap();
        // Never fetched, so `upstream/HEAD` does not resolve yet — the symbolic
        // form is the honest answer rather than a guessed branch name.
        assert_eq!(
            detect_base(&repo),
            Some(("upstream/HEAD".to_string(), "upstream".to_string()))
        );

        // Once the symref exists it is used, which is what makes a
        // develop-defaulting fork come out as `upstream/develop`.
        git(&repo, &["symbolic-ref", "refs/remotes/upstream/HEAD", "refs/remotes/upstream/develop"])
            .unwrap();
        assert_eq!(
            detect_base(&repo),
            Some(("upstream/develop".to_string(), "upstream".to_string()))
        );

        // Not a git repo at all is also no opinion, not a panic.
        assert_eq!(detect_base(&dir.join("nope")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A WIP that will not re-apply is a warning on a swap that happened, never an
    /// error instead of it.
    ///
    /// It used to return `Err`, and the caller bailed on the `?` before it forgot
    /// the traded branches, reconciled the panes or moved the conversations — so
    /// git held the swapped world and the daemon described the pre-swap one, with
    /// the SPA reporting a plain failure. Nothing could reconcile that afterwards.
    ///
    /// The apply is made to fail the one way it can: main's banked *new* file lands
    /// in a worktree that already has an untracked file of that name. Untracked
    /// files do not travel, so it is still sitting there.
    #[test]
    fn a_wip_that_cannot_reapply_still_leaves_the_branches_swapped() {
        let dir = std::env::temp_dir().join(format!(
            "orchd-swapwip-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("repo");
        git(&dir, &["init", "-q", "-b", "main", "repo"]).unwrap();
        git(&main, &["config", "user.email", "t@t"]).unwrap();
        git(&main, &["config", "user.name", "t"]).unwrap();
        std::fs::write(main.join("f.txt"), "base\n").unwrap();
        git(&main, &["add", "-A"]).unwrap();
        git(&main, &["commit", "-qm", "base"]).unwrap();
        git(&main, &["branch", "feature/b"]).unwrap();
        let tree = main.join(".claude/worktrees/w");
        git(&main, &["worktree", "add", "-q", tree.to_str().unwrap(), "feature/b"]).unwrap();

        // Banked: a staged addition in main, which travels to the worktree.
        std::fs::write(main.join("x.txt"), "main's new file\n").unwrap();
        git(&main, &["add", "x.txt"]).unwrap();
        // In the way: the same name, untracked in the worktree, so it stays put and
        // the apply has nowhere to put main's copy.
        std::fs::write(tree.join("x.txt"), "already here, untracked\n").unwrap();

        let s = swap_branches(&main, &tree).expect("a failed re-apply is not a failed swap");
        assert_eq!(current_branch(&main).unwrap(), "feature/b");
        assert_eq!(current_branch(&tree).unwrap(), "main");
        let why = s.wip_error.expect("the re-apply failure is reported");
        assert!(
            why.contains("did not re-apply") && why.contains("git stash apply"),
            "the message must name the commit and how to recover it: {why}"
        );
        // The work is still banked, not lost — that is what makes the warning a
        // warning. And the file in the way is untouched.
        assert_eq!(
            std::fs::read_to_string(tree.join("x.txt")).unwrap(),
            "already here, untracked\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Moving main's branch out: the tree is cut *after* main lets go, the work
    /// travels, and main is left on base rather than on a detached head.
    #[test]
    fn moving_a_branch_out_of_main_carries_its_work_and_leaves_main_on_base() {
        let dir = std::env::temp_dir().join(format!(
            "orchd-moveout-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("repo");
        git(&dir, &["init", "-q", "-b", "develop", "repo"]).unwrap();
        git(&main, &["config", "user.email", "t@t"]).unwrap();
        git(&main, &["config", "user.name", "t"]).unwrap();
        std::fs::write(main.join("f.txt"), "base\n").unwrap();
        git(&main, &["add", "-A"]).unwrap();
        git(&main, &["commit", "-qm", "base"]).unwrap();
        git(&main, &["switch", "-qc", "feature/b"]).unwrap();

        // Dirty, staged and untracked: the three cases the carry treats differently.
        std::fs::write(main.join("f.txt"), "edited in main\n").unwrap();
        std::fs::write(main.join("staged.txt"), "staged\n").unwrap();
        git(&main, &["add", "staged.txt"]).unwrap();
        std::fs::write(main.join("loose.txt"), "untracked\n").unwrap();

        let dest = main.join(".claude/worktrees/b");
        let moved = move_branch_out(&main, &dest, "develop", "worktree-b").expect("the move");
        assert_eq!((moved.branch.as_str(), moved.base.as_str()), ("feature/b", "develop"));
        assert!(!moved.created, "the branch was handed over, not cut");
        assert!(moved.wip_error.is_none(), "the carry: {:?}", moved.wip_error);

        assert_eq!(current_branch(&main).unwrap(), "develop");
        assert_eq!(current_branch(&dest).unwrap(), "feature/b");
        // The work is in the worktree, index distinction intact.
        assert_eq!(std::fs::read_to_string(dest.join("f.txt")).unwrap(), "edited in main\n");
        assert!(dest.join("staged.txt").exists(), "the staged file travelled");
        assert!(
            status(&dest, None, Untracked::Each)
                .unwrap()
                .staged
                .iter()
                .any(|f| f.path == "staged.txt"),
            "and it is still staged"
        );
        // Main kept none of the tracked work — that is the half that had to travel —
        // and the untracked file is still there, because `stash create` cannot take
        // one. Asserted rather than `is_clean`, which counts that file and would
        // read this correct state as dirty.
        let left = status(&main, Some(".claude/worktrees/"), Untracked::Each).unwrap();
        assert!(
            left.staged.is_empty() && left.unstaged.is_empty(),
            "main still holds tracked work: {left:?}"
        );
        assert_eq!(
            left.untracked.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["loose.txt"],
            "the untracked file stayed put, and is the only thing that did"
        );
        assert_eq!(std::fs::read_to_string(main.join("f.txt")).unwrap(), "base\n");

        // --- and the other half: main on base, with work but no branch of its own ---
        //
        // Nothing to hand over, so the work gets a branch cut for it and main does
        // not move at all. This is the case you land in by starting something in main
        // without branching first, which is the common one.
        std::fs::write(main.join("f.txt"), "started in main on develop\n").unwrap();
        let second = main.join(".claude/worktrees/work");
        let cut = move_branch_out(&main, &second, "develop", "worktree-work").expect("the cut");
        assert_eq!((cut.branch.as_str(), cut.base.as_str()), ("worktree-work", "develop"));
        assert!(cut.created, "the branch had to be created");
        assert!(cut.wip_error.is_none(), "the carry: {:?}", cut.wip_error);
        assert_eq!(current_branch(&second).unwrap(), "worktree-work");
        assert_eq!(
            std::fs::read_to_string(second.join("f.txt")).unwrap(),
            "started in main on develop\n",
            "the work did not travel to the branch cut for it"
        );
        // Main never left base and kept none of it.
        assert_eq!(current_branch(&main).unwrap(), "develop");
        assert_eq!(std::fs::read_to_string(main.join("f.txt")).unwrap(), "base\n");

        // Again, with the branch name already taken: suffixed rather than refused,
        // since a tree deleted long ago can leave its branch behind.
        std::fs::write(main.join("f.txt"), "and again\n").unwrap();
        let third = main.join(".claude/worktrees/work-2");
        let cut = move_branch_out(&main, &third, "develop", "worktree-work").expect("the cut");
        assert_eq!(cut.branch, "worktree-work-2");
    }

    /// The swap, and the refusal it is built around: git will not check one branch
    /// out twice, so the naive "switch each tree" fails on the first move. Both
    /// halves are pinned here because the three-step order *is* the feature.
    #[test]
    fn swapping_exchanges_two_branches_and_is_its_own_inverse() {
        let dir = std::env::temp_dir().join(format!(
            "orchd-swap-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("repo");
        git(&dir, &["init", "-q", "-b", "main", "repo"]).unwrap();
        git(&main, &["config", "user.email", "t@t"]).unwrap();
        git(&main, &["config", "user.name", "t"]).unwrap();
        std::fs::write(main.join("f.txt"), "base\n").unwrap();
        git(&main, &["add", "-A"]).unwrap();
        git(&main, &["commit", "-qm", "base"]).unwrap();
        git(&main, &["branch", "feature/b"]).unwrap();
        let tree = main.join(".claude/worktrees/w");
        git(&main, &["worktree", "add", "-q", tree.to_str().unwrap(), "feature/b"]).unwrap();

        assert_eq!(current_branch(&main).unwrap(), "main");
        assert_eq!(current_branch(&tree).unwrap(), "feature/b");

        // The refusal the three steps exist for: one branch, one tree.
        let naive = git(&main, &["switch", "feature/b"])
            .expect_err("git must refuse a branch already checked out");
        assert!(
            refused_as_already_checked_out(&format!("{naive:#}")),
            "unexpected refusal: {naive:#}"
        );

        let s = swap_branches(&main, &tree).expect("the swap");
        assert_eq!((s.main_now.as_str(), s.worktree_now.as_str()), ("feature/b", "main"));
        assert!(s.wip_error.is_none(), "nothing to carry, nothing to warn about");
        assert_eq!(current_branch(&main).unwrap(), "feature/b");
        assert_eq!(current_branch(&tree).unwrap(), "main");
        // Neither tree is left detached or dirty.
        assert!(
            is_clean_excluding(&main, Some(".claude/worktrees/")).unwrap()
                && is_clean(&tree).unwrap(),
            "excluding for main, which contains the worktrees dir"
        );

        // Swapping again is the undo, which is what makes the menu item safe to
        // press twice.
        swap_branches(&main, &tree).expect("swap back");
        assert_eq!(current_branch(&main).unwrap(), "main");
        assert_eq!(current_branch(&tree).unwrap(), "feature/b");

        // --- and the uncommitted work travels with its branch ---
        //
        // Both sides dirty, so the crosswise apply is exercised in both directions
        // rather than only the one anybody would test by hand.
        std::fs::write(main.join("f.txt"), "main was editing this\n").unwrap();
        std::fs::write(tree.join("f.txt"), "the worktree was editing this\n").unwrap();
        // One staged as well, since `stash create` banks the index too and
        // `stash apply --index` is what restores that distinction.
        std::fs::write(tree.join("staged.txt"), "staged in the worktree\n").unwrap();
        git(&tree, &["add", "staged.txt"]).unwrap();

        let s = swap_branches(&main, &tree).expect("swap with work in both trees");
        assert!(s.wip_error.is_none(), "both sides re-applied: {:?}", s.wip_error);

        assert_eq!(current_branch(&main).unwrap(), "feature/b");
        assert_eq!(current_branch(&tree).unwrap(), "main");
        assert_eq!(
            std::fs::read_to_string(main.join("f.txt")).unwrap(),
            "the worktree was editing this\n",
            "the worktree's edit followed its branch into main"
        );
        assert_eq!(
            std::fs::read_to_string(tree.join("f.txt")).unwrap(),
            "main was editing this\n",
            "and main's edit went the other way"
        );
        assert!(
            main.join("staged.txt").exists(),
            "a staged addition travels too"
        );
        // Still staged, not merely present: `--index` is the difference.
        let staged = git(&main, &["diff", "--cached", "--name-only"]).unwrap();
        assert!(staged.contains("staged.txt"), "index preserved, got {staged:?}");

        // Nothing was left banked behind either: a clean tree means no reset ran.
        swap_branches(&main, &tree).expect("swap back with the work");
        assert_eq!(
            std::fs::read_to_string(tree.join("f.txt")).unwrap(),
            "the worktree was editing this\n",
            "and it comes home again"
        );

        // Same branch both sides is refused rather than silently doing nothing.
        let same = main.join(".claude/worktrees/same");
        git(&main, &["worktree", "add", "-q", "--detach", same.to_str().unwrap()]).unwrap();
        git(&same, &["switch", "-q", "-c", "third"]).unwrap();
        git(&same, &["switch", "-q", "--detach"]).unwrap();
        assert!(swap_branches(&main, &main).is_err(), "main against itself");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `origin/HEAD` is the default base, and `git switch HEAD` fails with "a
    /// branch is expected" — so anything that checks the base out has to resolve
    /// the symref first. `park_main` did not, and silently never parked.
    #[test]
    fn a_head_base_resolves_to_a_real_branch_before_anyone_checks_it_out() {
        let dir = std::env::temp_dir().join(format!(
            "orchd-basebranch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = dir.join("repo");
        git(&dir, &["init", "-q", "repo"]).unwrap();
        git(&repo, &["remote", "add", "origin", "git@github.com:acme/monorepo.git"]).unwrap();

        // The string answer is the symref name, which is not checkout-able.
        assert_eq!(base_branch("origin/HEAD"), "HEAD");
        // And unresolvable until the symref exists, which is "cannot", not "HEAD".
        assert_eq!(base_checkout_branch(&repo, "origin/HEAD"), None);

        git(&repo, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"])
            .unwrap();
        assert_eq!(base_checkout_branch(&repo, "origin/HEAD").as_deref(), Some("main"));

        // A named base needs no repo lookup and passes straight through.
        assert_eq!(
            base_checkout_branch(&repo, "upstream/develop").as_deref(),
            Some("develop")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both supported layouts, through the two functions the panes actually run on
    /// the base ref: the merge-base the changed-file list is computed from, and the
    /// behind/ahead the divergence strip shows.
    ///
    /// Neither had a test against a *configured* base at all, so `origin/HEAD`
    /// becoming the default rested on it being "a valid rev". It is, but a symref
    /// is not the same shape as a branch name and that is exactly the assumption
    /// worth pinning.
    #[test]
    fn both_a_fork_and_a_plain_layout_answer_merge_base_and_divergence() {
        let dir = std::env::temp_dir().join(format!(
            "orchd-flows-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A remote to be the origin, and a clone of it.
        git(&dir, &["init", "-q", "--bare", "-b", "main", "origin.git"]).unwrap();
        let origin = dir.join("origin.git");
        let work = dir.join("work");
        git(&dir, &["init", "-q", "-b", "main", "work"]).unwrap();
        git(&work, &["config", "user.email", "t@t"]).unwrap();
        git(&work, &["config", "user.name", "t"]).unwrap();
        std::fs::write(work.join("a.txt"), "base\n").unwrap();
        git(&work, &["add", "-A"]).unwrap();
        git(&work, &["commit", "-qm", "base"]).unwrap();
        let base_sha = git(&work, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        git(&work, &["remote", "add", "origin", origin.to_str().unwrap()]).unwrap();
        git(&work, &["push", "-q", "origin", "main"]).unwrap();

        // --- plain layout: one remote, base is its own default branch ---
        // `fetch_upstream` is what records the symref; without it `origin/HEAD`
        // does not resolve, which is the trap it was written for.
        fetch_upstream(&work, "origin/HEAD").expect("fetch");
        git(&work, &["checkout", "-q", "-b", "feature/x"]).unwrap();
        std::fs::write(work.join("a.txt"), "mine\n").unwrap();
        git(&work, &["commit", "-qam", "mine"]).unwrap();

        assert_eq!(
            merge_base(&work, "origin/HEAD").expect("merge-base against a symref"),
            base_sha,
            "the changed-file list is computed from this"
        );
        assert_eq!(
            divergence(&work, "origin/HEAD").expect("divergence against a symref"),
            (0, 1),
            "one commit ahead of the remote's default branch, none behind"
        );

        // --- fork layout: a second remote, base is a named branch on it ---
        git(&dir, &["init", "-q", "--bare", "-b", "develop", "upstream.git"]).unwrap();
        let upstream = dir.join("upstream.git");
        git(&work, &["remote", "add", "upstream", upstream.to_str().unwrap()]).unwrap();
        git(&work, &["push", "-q", "upstream", "main:develop"]).unwrap();
        fetch_upstream(&work, "upstream/develop").expect("fetch the named base");

        assert_eq!(
            merge_base(&work, "upstream/develop").expect("merge-base against a branch"),
            base_sha
        );
        assert_eq!(divergence(&work, "upstream/develop").expect("divergence"), (0, 1));

        // Detection sees the fork, and answers with the symref rather than the
        // branch — because fetching a *named* base does not record
        // `upstream/HEAD`, and only a clone or the HEAD arm ever does. That is the
        // better answer anyway: `upstream/HEAD` is self-correcting when the remote
        // renames its default branch, where a recorded `develop` would rot.
        assert_eq!(
            detect_base(&work),
            Some(("upstream/HEAD".to_string(), "upstream".to_string()))
        );
        // And it resolves, once something records the symref.
        fetch_upstream(&work, "upstream/HEAD").expect("the HEAD arm records it");
        assert_eq!(
            base_checkout_branch(&work, "upstream/HEAD").as_deref(),
            Some("develop")
        );
        assert_eq!(merge_base(&work, "upstream/HEAD").expect("merge-base"), base_sha);

        // What the run overview needed and `divergence` cannot say. The branch is
        // one commit beyond the base and that commit is on nobody's remote, so both
        // read 1 here — the numbers only part once something is pushed.
        assert_eq!(divergence(&work, "upstream/develop").unwrap().1, 1);
        assert_eq!(
            unpushed_count(&work, "feature/x", "upstream/develop"),
            1,
            "never pushed, so everything beyond the base is unpushed"
        );
        git(&work, &["push", "-q", "origin", "feature/x"]).unwrap();
        assert_eq!(
            unpushed_count(&work, "feature/x", "upstream/develop"),
            0,
            "pushed, and the count follows the remote rather than the base"
        );
        std::fs::write(work.join("a.txt"), "more\n").unwrap();
        git(&work, &["commit", "-qam", "more"]).unwrap();
        assert_eq!(
            (
                divergence(&work, "upstream/develop").unwrap().1,
                unpushed_count(&work, "feature/x", "upstream/develop")
            ),
            (2, 1),
            "two commits past the base, one of them pushed — the case that made \
             `ahead` the wrong number to show"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_base_branch_is_the_configured_ref_without_its_remote() {
        assert_eq!(base_branch("upstream/develop"), "develop");
        // A nested branch name keeps every segment but the remote.
        assert_eq!(base_branch("origin/release/2026"), "release/2026");
        // A bare ref is already the branch.
        assert_eq!(base_branch("main"), "main");
    }

    #[test]
    fn upstream_fetch_argv_is_config_driven_not_hardcoded() {
        // A fork workflow fetches one named branch (as before).
        assert_eq!(
            upstream_fetch_argv("upstream/develop"),
            vec!["fetch", "upstream", "develop", "--no-tags"]
        );
        // A nested branch name stays intact.
        assert_eq!(
            upstream_fetch_argv("origin/release/2026"),
            vec!["fetch", "origin", "release/2026", "--no-tags"]
        );
        // A bare ref assumes origin.
        assert_eq!(
            upstream_fetch_argv("main"),
            vec!["fetch", "origin", "main", "--no-tags"]
        );
    }

    #[test]
    fn a_head_base_ref_is_recorded_and_then_resolves() {
        // The regression this guards: `git fetch <remote>` does not create
        // `refs/remotes/<remote>/HEAD`, so `origin/HEAD` did not resolve on a
        // checkout whose remote was added by hand — and every merge-base,
        // divergence and rebase against it failed silently.
        // Its own tree, not `scratch_repo`'s: these are three sibling repos and
        // nesting them inside another checkout confuses the remote plumbing.
        let dir = std::env::temp_dir().join(format!(
            "orchd-head-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let upstream = dir.join("up.git");
        let work = dir.join("work");
        git(&dir, &["init", "-q", "--bare", "-b", "main", "up.git"]).unwrap();
        git(&dir, &["init", "-q", "work"]).unwrap();
        git(&work, &["config", "user.email", "t@t"]).unwrap();
        git(&work, &["config", "user.name", "t"]).unwrap();
        std::fs::write(work.join("a.txt"), "x").unwrap();
        git(&work, &["add", "-A"]).unwrap();
        git(&work, &["commit", "-qm", "init"]).unwrap();
        git(&work, &["branch", "-M", "main"]).unwrap();
        git(&work, &["remote", "add", "origin", upstream.to_str().unwrap()]).unwrap();
        git(&work, &["push", "-q", "origin", "main"]).unwrap();

        // A *hand-added* remote: `git init` + `git remote add`, never cloned.
        let hand = dir.join("hand");
        git(&dir, &["init", "-q", "hand"]).unwrap();
        git(&hand, &["remote", "add", "origin", upstream.to_str().unwrap()]).unwrap();

        fetch_upstream(&hand, "origin/HEAD").expect("the fetch");
        assert_eq!(default_branch(&hand, "origin").as_deref(), Some("main"));
        // The whole point: the ref every consumer resolves against now exists.
        assert!(
            git(&hand, &["rev-parse", "--verify", "origin/HEAD"]).is_ok(),
            "origin/HEAD must resolve after fetch_upstream"
        );
    }

    /// A `feature` branch on the scratch repo, and the sha a conversation on it
    /// would have recorded.
    fn feature_at(repo: &std::path::Path) -> (std::path::PathBuf, String) {
        git(repo, &["branch", "feature"]).unwrap();
        let sha = git(repo, &["rev-parse", "feature"])
            .unwrap()
            .trim()
            .to_string();
        (repo.join(".claude/worktrees/wt"), sha)
    }

    #[test]
    fn head_file_resolves_and_tracks_the_branch_for_main_and_worktrees() {
        let repo = scratch_repo();

        // Main: HEAD is a real file directly under `.git`, and a checkout rewrites
        // its contents — which is exactly the change the poller reads.
        let main_head = head_file(&repo).unwrap();
        assert!(main_head.exists(), "no HEAD at {main_head:?}");
        assert!(main_head.starts_with(&repo) && main_head.ends_with("HEAD"));
        let before = std::fs::read_to_string(&main_head).unwrap();
        git(&repo, &["checkout", "-q", "-b", "other"]).unwrap();
        assert_ne!(before, std::fs::read_to_string(&main_head).unwrap());

        // Linked worktree: `<wt>/.git` is a pointer file, so the real HEAD lives
        // under the common dir — `<wt>/.git/HEAD` does not exist.
        let wt = repo.join(".claude/worktrees/wt");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        git(&repo, &["worktree", "add", "-q", "-b", "wt", wt.to_str().unwrap()]).unwrap();
        let wt_head = head_file(&wt).unwrap();
        assert!(wt_head.exists(), "no worktree HEAD at {wt_head:?}");
        assert!(!wt.join(".git/HEAD").exists());
        assert!(
            wt_head.to_string_lossy().contains(".git/worktrees/"),
            "worktree HEAD should live under the common dir, got {wt_head:?}"
        );
    }

    #[test]
    fn rebuilds_on_the_branch_that_is_still_there() {
        let repo = scratch_repo();
        let (wt, sha) = feature_at(&repo);
        let moved = worktree_rebuild(&repo, &wt, "feature", &sha).unwrap();
        assert!(
            moved.is_none(),
            "tip matches the record, so nothing to warn about"
        );
        assert!(
            wt.join(".git").exists(),
            "worktree was not created at {wt:?}"
        );
    }

    #[test]
    fn recreates_a_deleted_branch_at_the_recorded_commit() {
        // The merged-and-deleted case (§2 step 1): no ref left, but the commit the
        // conversation ran on is still in the object store.
        let repo = scratch_repo();
        let (wt, sha) = feature_at(&repo);
        git(&repo, &["branch", "-D", "feature"]).unwrap();
        assert!(!branch_exists(&repo, "feature"));

        let moved = worktree_rebuild(&repo, &wt, "feature", &sha).unwrap();
        assert!(
            moved.is_none(),
            "recreated at the recorded commit, so it matches"
        );
        assert!(branch_exists(&repo, "feature"), "branch was not recreated");
        assert_eq!(head_sha(&wt).unwrap(), sha);
    }

    #[test]
    fn reports_the_tip_when_the_branch_moved_on() {
        // §2 step 3: the transcript describes a tree that is no longer checked
        // out, and the caller has to be able to say so.
        let repo = scratch_repo();
        let (wt, recorded) = feature_at(&repo);
        git(&repo, &["switch", "-q", "feature"]).unwrap();
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "later"]).unwrap();
        let tip = git(&repo, &["rev-parse", "feature"])
            .unwrap()
            .trim()
            .to_string();
        git(&repo, &["switch", "-q", "main"]).unwrap();

        let moved = worktree_rebuild(&repo, &wt, "feature", &recorded).unwrap();
        assert_eq!(moved.as_deref(), Some(tip.as_str()));
    }

    #[test]
    fn refuses_when_neither_the_branch_nor_the_commit_survives() {
        let repo = scratch_repo();
        let wt = repo.join(".claude/worktrees/wt");
        let err = worktree_rebuild(&repo, &wt, "gone", &"0".repeat(40))
            .expect_err("nothing to rebuild on");
        assert!(format!("{err:#}").contains("unreachable"), "got: {err:#}");
    }

    fn rec(parts: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p.as_bytes());
            v.push(0);
        }
        v
    }

    #[test]
    fn splits_staged_and_unstaged_from_one_entry() {
        // XY = "MM": staged modification and a further unstaged one.
        let raw = rec(&["1 MM N... 100644 100644 100644 aaa bbb src/Foo.php"]);
        let set = parse_status(&raw, None);
        assert_eq!(set.staged.len(), 1);
        assert_eq!(set.unstaged.len(), 1);
        assert_eq!(set.staged[0].path, "src/Foo.php");
    }

    #[test]
    fn a_staged_only_entry_does_not_appear_as_unstaged() {
        let raw = rec(&["1 M. N... 100644 100644 100644 aaa bbb src/Foo.php"]);
        let set = parse_status(&raw, None);
        assert_eq!(set.staged.len(), 1);
        assert!(set.unstaged.is_empty());
    }

    #[test]
    fn consumes_the_original_path_of_a_rename() {
        let raw = rec(&[
            "2 R. N... 100644 100644 100644 aaa bbb R100 src/New.php",
            "src/Old.php",
            "? untracked.txt",
        ]);
        let set = parse_status(&raw, None);
        assert_eq!(set.staged.len(), 1);
        assert_eq!(set.staged[0].path, "src/New.php");
        // The old path must not be read back as an entry of its own.
        assert_eq!(set.untracked.len(), 1);
        assert_eq!(set.untracked[0].path, "untracked.txt");
    }

    #[test]
    fn excludes_sibling_worktrees_from_mains_view() {
        let raw = rec(&["? .claude/worktrees/other/file.php", "? src/Mine.php"]);
        let set = parse_status(&raw, Some(".claude/worktrees/"));
        assert_eq!(set.untracked.len(), 1);
        assert_eq!(set.untracked[0].path, "src/Mine.php");
    }

    #[test]
    fn keeps_worktree_paths_when_not_excluding() {
        let raw = rec(&["? .claude/worktrees/other/file.php"]);
        let set = parse_status(&raw, None);
        assert_eq!(set.untracked.len(), 1);
    }

    #[test]
    fn excludes_the_configured_prefix_not_a_hardcoded_one() {
        // A repo whose worktrees live under a different subdir excludes *that*,
        // and leaves the old default's path alone.
        let raw = rec(&["? .worktrees/other/file.php", "? .claude/worktrees/x.php"]);
        let set = parse_status(&raw, Some(".worktrees/"));
        assert_eq!(set.untracked.len(), 1);
        assert_eq!(set.untracked[0].path, ".claude/worktrees/x.php");
    }

    fn scratch_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orchd-git-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "one"]);
        // Resolved, because git reports resolved paths and a test comparing its
        // output against this one has to agree with it. `$TMPDIR` on macOS is
        // under `/var`, which is a symlink into `/private`, so unresolved it
        // matched nothing there — while on Linux `/tmp` is a real directory and
        // the difference never showed.
        std::fs::canonicalize(&dir).unwrap_or(dir)
    }

    /// What the resolve run's per-thread check rests on, including the way it
    /// fails.
    ///
    /// A run asks "is the tree I was triaged against still in my history", and the
    /// three answers all matter: yes while the agent commits on top, no once the
    /// branch is rewritten under it, and *no* for a sha this repo has never heard
    /// of — which is the same situation from the run's point of view, and must not
    /// read as yes just because git could not answer.
    #[test]
    fn ancestry_says_no_to_a_rewritten_branch_and_to_a_sha_it_cannot_resolve() {
        let repo = scratch_repo();
        let base = head_sha(&repo).expect("head");
        std::fs::write(repo.join("f.txt"), "two\n").unwrap();
        git(&repo, &["commit", "-qam", "two"]).unwrap();
        assert!(is_ancestor(&repo, &base, "HEAD"), "committing on top keeps it");

        // The rewrite: a history built from the same tree but with no parents,
        // which is what a branch reset below the base and re-committed leaves.
        let orphan = git(&repo, &["commit-tree", "-m", "orphan", &format!("{base}^{{tree}}")])
            .expect("an orphan commit with no parents");
        assert!(
            !is_ancestor(&repo, &base, orphan.trim()),
            "a history the base is not in must answer no"
        );
        assert!(
            !is_ancestor(&repo, "0000000000000000000000000000000000000000", "HEAD"),
            "a sha git cannot resolve is not an ancestor — the check fails closed"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn cuts_a_new_worktree_at_a_custom_subdir() {
        // The daemon's own creation path, used when worktrees do not live where
        // `claude --worktree` would put them — the case where delegating created
        // the worktree somewhere the daemon never looked.
        let repo = scratch_repo();
        let wt = repo.join(".worktrees/inv");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        worktree_add_new(&repo, &wt, "worktree-inv", "main").expect("worktree add");

        assert!(wt.join("f.txt").exists(), "the worktree is checked out");
        assert_eq!(current_branch(&wt).unwrap(), "worktree-inv");
        // Re-cutting the same branch must refuse rather than silently reuse it.
        let again = repo.join(".worktrees/inv2");
        assert!(worktree_add_new(&repo, &again, "worktree-inv", "main").is_err());
        // A base that does not resolve must not quietly cut from HEAD.
        let bad = repo.join(".worktrees/bad");
        assert!(worktree_add_new(&repo, &bad, "worktree-bad", "origin/nope").is_err());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn removes_a_worktree_stale_locked_by_a_dead_claude() {
        // The finding the review fixture surfaced: `claude --worktree` locks every
        // worktree it cuts, and the lock outlives the session the daemon kills, so
        // a plain `git worktree remove` refuses forever. Teardown's preflight has
        // already proven the tree clean and no session live, so a lock whose pid
        // is dead is stale — clearing it is not `--force`.
        let repo = scratch_repo();
        let wt = repo.join(".claude/worktrees/wt");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        worktree_add_new(&repo, &wt, "worktree-wt", "main").expect("worktree add");

        // pid 2^31-ish is not in the table — the closest a test can get to
        // "claude died" without racing a real one. Mirrors claude's own reason
        // string so the parser is exercised on the real shape.
        let reason = "claude session wt (pid 2147480000 start 1)";
        git(&repo, &["worktree", "lock", "--reason", reason, wt.to_str().unwrap()])
            .expect("lock");
        assert!(stale_lock_pid(&repo, &wt).is_some(), "the lock pid parses");

        worktree_remove(&repo, &wt).expect("remove clears the stale lock");
        assert!(!wt.exists(), "the worktree is gone from disk");
        assert!(
            !git(&repo, &["worktree", "list", "--porcelain"])
                .unwrap()
                .contains("worktrees/wt"),
            "and gone from git's record"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The same stale lock, but named through a symlink — because `git worktree
    /// list` answers with the *real* path and the caller's may not be one. This is
    /// what failed on the macOS runner while passing on Linux: `$TMPDIR` is under
    /// `/var`, a symlink into `/private`, so the string compare missed and the
    /// lock was never seen as stale. Now that `scratch_repo` hands back a resolved
    /// path, this is the only test that still exercises that comparison.
    #[test]
    fn a_stale_lock_is_found_even_when_the_worktree_is_named_through_a_symlink() {
        let repo = scratch_repo();
        let wt = repo.join(".claude/worktrees/linked");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        worktree_add_new(&repo, &wt, "worktree-linked", "main").expect("worktree add");
        let reason = "claude session linked (pid 2147480000 start 1)";
        git(&repo, &["worktree", "lock", "--reason", reason, wt.to_str().unwrap()])
            .expect("lock");

        // A second route to the very same worktree.
        let link = repo.parent().unwrap().join(format!(
            "orchd-wtlink-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&repo, &link).expect("symlink");
        let via_link = link.join(".claude/worktrees/linked");

        assert!(
            stale_lock_pid(&repo, &via_link).is_some(),
            "a symlinked path must still find the lock git reports under its real name"
        );
        worktree_remove(&repo, &via_link).expect("remove through the symlink");
        assert!(!wt.exists(), "the worktree is gone from disk");

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn keeps_refusing_a_worktree_locked_by_a_live_process() {
        // The safety half: a lock whose owner is alive must still refuse, or the
        // stale-lock path would become a rename for `--force`. Our own pid stands
        // in for a live claude.
        let repo = scratch_repo();
        let wt = repo.join(".claude/worktrees/live");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        worktree_add_new(&repo, &wt, "worktree-live", "main").expect("worktree add");

        let reason = format!("claude session live (pid {} start 1)", std::process::id());
        git(&repo, &["worktree", "lock", "--reason", &reason, wt.to_str().unwrap()])
            .expect("lock");

        let err = worktree_remove(&repo, &wt).unwrap_err();
        assert!(
            format!("{err:#}").contains("not escalating to --force"),
            "a live lock must surface, not be cleared: {err:#}"
        );
        assert!(wt.exists(), "the worktree is left in place");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn adds_a_worktree_on_an_existing_branch() {
        // /resolve has to land on the PR's own head branch, not a fresh one cut
        // from upstream/develop (§8).
        let repo = scratch_repo();
        std::process::Command::new("git")
            .args(["branch", "feature/x"])
            .current_dir(&repo)
            .output()
            .unwrap();

        let wt = repo.join(".claude/worktrees/pr-1");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        worktree_add_existing(&repo, &wt, "feature/x").expect("worktree add");

        assert!(wt.join("f.txt").exists());
        assert_eq!(current_branch(&wt).unwrap(), "feature/x");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn refuses_a_branch_that_exists_nowhere() {
        let repo = scratch_repo();
        let wt = repo.join(".claude/worktrees/pr-2");
        std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
        let err = worktree_add_existing(&repo, &wt, "feature/nope").unwrap_err();
        assert!(
            format!("{err:#}").contains("neither locally nor on origin"),
            "unexpected: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn unpushed_commits_block_teardown() {
        assert!(Unpushed::NeverPushed {
            commits: vec!["abc work".into()]
        }
        .blocks_teardown());
        assert!(Unpushed::Ahead {
            commits: vec!["abc x".into()]
        }
        .blocks_teardown());
        assert!(!Unpushed::UpToDate.blocks_teardown());
    }

    #[test]
    fn a_fresh_worktree_carrying_nothing_can_still_be_removed() {
        // Branched straight off upstream/develop and never pushed: there is no
        // work to lose, so teardown must not be blocked forever.
        assert!(!Unpushed::NeverPushed { commits: vec![] }.blocks_teardown());
    }
    /// A repo with two commits so blame has something to distinguish, plus a
    /// `base` ref standing in for the merge base.
    fn amend_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orchd-amend-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "me@here"]);
        run(&["config", "user.name", "me"]);
        // Commit 1 stands in for the base branch's history.
        std::fs::write(dir.join("f.txt"), "base1\nbase2\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "from develop"]);
        run(&["branch", "base"]);
        // Commit 2 is the PR's own.
        std::fs::write(dir.join("f.txt"), "base1\nbase2\nmine\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "the PR commit"]);
        dir
    }

    fn sha_of(dir: &Path, subject: &str) -> String {
        let out = std::process::Command::new("git")
            .args(["log", "--format=%H %s"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| l.contains(subject))
            .map(|l| l.split_whitespace().next().unwrap().to_string())
            .expect("commit")
    }

    #[test]
    fn a_line_from_this_prs_own_commit_is_a_fixup_target() {
        let d = amend_repo();
        let want = sha_of(&d, "the PR commit");
        let got = amend_target(&d, None, "base", &[("f.txt".into(), 3)], "me@here").unwrap();
        assert_eq!(got, Amend::Fixup(want));
    }

    #[test]
    fn a_line_that_predates_the_branch_degrades_to_head() {
        // Line 1 came from `develop`; it is not this PR's commit to rewrite.
        let d = amend_repo();
        match amend_target(&d, None, "base", &[("f.txt".into(), 1)], "me@here").unwrap() {
            Amend::Head(why) => assert!(why.contains("predates"), "{why}"),
            other => panic!("expected Head, got {other:?}"),
        }
    }

    #[test]
    fn someone_elses_commit_is_never_rewritten() {
        // Rewriting a colleague's commit under your own force-push changes a sha
        // they may have checked out.
        //
        // This used to assert `Head`, which was the bug wearing the test's clothes:
        // `fold_in`'s `Head` arm runs `git commit --amend`, so "degrading" to it
        // rewrote the very commit the guard had just refused to touch. When nothing
        // on the branch is yours, the only safe fold is a new commit.
        let d = amend_repo();
        match amend_target(&d, None, "base", &[("f.txt".into(), 3)], "someone@else").unwrap() {
            Amend::OnTop(why) => assert!(why.contains("me@here"), "must name them: {why}"),
            other => panic!("expected OnTop, got {other:?}"),
        }
    }

    #[test]
    fn a_colleagues_commit_stacked_on_the_target_is_not_rewritten_either() {
        // The half the first attempt missed. `fold_in` autosquashes with
        // `rebase -i <sha>~1`, which replays every commit from the target to HEAD and
        // gives each a new sha — so guarding only the target left its descendants
        // wide open, and this path was never taken.
        let d = amend_repo();
        let target = sha_of(&d, "the PR commit");
        // A colleague commits on top of the commit that owns the line.
        std::fs::write(d.join("theirs.txt"), "their work\n").unwrap();
        for args in [
            vec!["add", "-A"],
            vec![
                "-c",
                "user.email=alice@example.com",
                "-c",
                "user.name=alice",
                "commit",
                "-qm",
                "alice's commit",
            ],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&d)
                .output()
                .expect("git");
        }

        match amend_target(&d, None, "base", &[("f.txt".into(), 3)], "me@here").unwrap() {
            Amend::OnTop(why) => {
                assert!(why.contains("alice@example.com"), "must name them: {why}");
                assert!(why.contains(&target[..7]), "and the target: {why}");
            }
            other => panic!("a stacked commit must not be rewritten, got {other:?}"),
        }
    }

    #[test]
    fn committing_on_top_leaves_every_existing_sha_alone() {
        // The whole point: an amend rewrites, a new commit does not.
        let d = amend_repo();
        let before_head = head_sha(&d).unwrap();
        let before_count = std::process::Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(&d)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap();

        std::fs::write(d.join("f.txt"), "base1\nbase2\nmine\nby hand\n").unwrap();
        fold_in(&d, &Amend::OnTop("HEAD is alice's commit".into())).unwrap();

        let after_count = std::process::Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(&d)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap();
        assert_eq!(
            after_count.parse::<u32>().unwrap(),
            before_count.parse::<u32>().unwrap() + 1,
            "one commit added, not folded"
        );
        // Byte-identical: nothing anybody may have checked out has moved.
        let parent = std::process::Command::new("git")
            .args(["rev-parse", "HEAD^"])
            .current_dir(&d)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap();
        assert_eq!(parent, before_head, "the old HEAD must be untouched");
        assert!(is_clean(&d).unwrap());
    }

    #[test]
    fn committing_on_top_with_nothing_staged_is_not_an_error() {
        // `--amend` succeeds on an empty diff and `commit -m` does not, so a hook
        // that reverted an edit back to HEAD's content would turn a silent success
        // into a hard error — which, mid-batch, is a 500 with HEAD already moved.
        let d = amend_repo();
        let before = head_sha(&d).unwrap();
        fold_in(&d, &Amend::OnTop("nothing to attribute".into())).unwrap();
        assert_eq!(
            head_sha(&d).unwrap(),
            before,
            "nothing to commit, nothing done"
        );
    }

    #[test]
    fn changes_spanning_two_commits_degrade_to_head() {
        let d = amend_repo();
        let got = amend_target(
            &d,
            None,
            "base",
            &[("f.txt".into(), 1), ("f.txt".into(), 3)],
            "me@here",
        )
        .unwrap();
        match got {
            // Line 1 is from develop, so the span check or the predates check
            // fires first — either way it must not be a fixup.
            Amend::Head(_) => {}
            other => panic!("expected Head, got {other:?}"),
        }
    }

    #[test]
    fn blame_on_an_uncommitted_line_is_none() {
        // Blame must run against the committed state: a dirty tree attributes
        // the line to an all-zero sha, which is nobody's commit.
        let d = amend_repo();
        std::fs::write(d.join("f.txt"), "base1\nbase2\nmine\nfresh\n").unwrap();
        assert_eq!(blame_line(&d, None, "f.txt", 4).unwrap(), None);
    }

    #[test]
    fn a_fixup_folds_away_and_keeps_the_commit_count() {
        let d = amend_repo();
        let target = sha_of(&d, "the PR commit");
        let before = count_commits(&d);

        // Edit the line that commit owns, then fold into it.
        std::fs::write(d.join("f.txt"), "base1\nbase2\nMINE\n").unwrap();
        fold_in(&d, &Amend::Fixup(target)).unwrap();

        assert_eq!(count_commits(&d), before, "fixup should not add a commit");
        assert!(!subjects(&d).iter().any(|s| s.starts_with("fixup!")));
        let content = std::fs::read_to_string(d.join("f.txt")).unwrap();
        assert!(content.contains("MINE"));
    }

    #[test]
    fn a_head_amend_also_keeps_the_commit_count() {
        let d = amend_repo();
        let before = count_commits(&d);
        std::fs::write(d.join("g.txt"), "new\n").unwrap();
        fold_in(&d, &Amend::Head("spanned".into())).unwrap();
        assert_eq!(count_commits(&d), before);
        assert!(d.join("g.txt").exists());
    }

    fn count_commits(dir: &Path) -> usize {
        subjects(dir).len()
    }

    fn subjects(dir: &Path) -> Vec<String> {
        let out = std::process::Command::new("git")
            .args(["log", "--format=%s"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect()
    }

    #[test]
    fn a_push_to_the_base_branch_is_refused_before_it_runs() {
        // The agent-side guard only hooks Bash; a daemon push bypasses it.
        let d = amend_repo();
        let err = push_with_lease(&d, "trunk", Some("trunk")).unwrap_err().to_string();
        assert!(err.contains("refusing to push"), "{err}");
        // The list used to be four hardcoded names, so this pair was backwards:
        // `trunk` sailed through and `release` was refused for its name alone.
        // Only a real push attempt gets past the check, so the error is git's.
        let err = push_with_lease(&d, "release", Some("trunk")).unwrap_err().to_string();
        assert!(!err.contains("refusing to push"), "{err}");
        // No resolvable base refuses nothing here either.
        let err = push_with_lease(&d, "trunk", None).unwrap_err().to_string();
        assert!(!err.contains("refusing to push"), "{err}");
    }

    #[test]
    fn commit_all_needs_a_message_and_then_cleans_the_tree() {
        let d = amend_repo();
        std::fs::write(d.join("h.txt"), "x\n").unwrap();
        assert!(commit_all(&d, "   ").is_err());
        commit_all(&d, "wip").unwrap();
        assert!(is_clean(&d).unwrap());
    }

    #[test]
    fn stash_clears_untracked_files_too() {
        // Otherwise the tree is not actually clean and the gate would still fire.
        let d = amend_repo();
        std::fs::write(d.join("tracked-edit.txt"), "x\n").unwrap();
        std::fs::write(d.join("f.txt"), "base1\nbase2\nedited\n").unwrap();
        stash(&d).unwrap();
        assert!(is_clean(&d).unwrap());
        assert!(!d.join("tracked-edit.txt").exists());
    }
}
