use anyhow::{bail, Context, Result};
use std::path::Path;
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

/// Changed files for a workspace, grouped staged / unstaged / untracked (§4).
///
/// `exclude_worktrees` is set for main: main's file tree contains every
/// worktree, so without it you see every sibling session's work (§2).
pub fn status(cwd: &Path, exclude_worktrees: bool) -> Result<FileSet> {
    let raw = git_raw(
        cwd,
        &["status", "--porcelain=v2", "--untracked-files=normal", "-z"],
    )?;
    Ok(parse_status(&raw, exclude_worktrees))
}

fn parse_status(raw: &[u8], exclude_worktrees: bool) -> FileSet {
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
                push_xy(&mut set, fields[1], fields[8], exclude_worktrees);
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
                push_xy(&mut set, fields[1], fields[9], exclude_worktrees);
            }
            // Unmerged. Both sides count as unstaged work.
            'u' => {
                let fields: Vec<&str> = rec.splitn(11, ' ').collect();
                if let Some(path) = fields.last() {
                    if !skip(path, exclude_worktrees) {
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
                if !path.is_empty() && !skip(path, exclude_worktrees) {
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
fn push_xy(set: &mut FileSet, xy: &str, path: &str, exclude_worktrees: bool) {
    if skip(path, exclude_worktrees) {
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

fn skip(path: &str, exclude_worktrees: bool) -> bool {
    exclude_worktrees && path.starts_with(".claude/worktrees/")
}

// ---------------------------------------------------------------------------
// Refs
// ---------------------------------------------------------------------------

pub fn current_branch(cwd: &Path) -> Result<String> {
    Ok(git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?
        .trim()
        .to_string())
}

pub fn head_sha(cwd: &Path) -> Result<String> {
    Ok(git(cwd, &["rev-parse", "HEAD"])?.trim().to_string())
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
    NeverPushed { commits: Vec<String> },
    Ahead { commits: Vec<String> },
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

/// Removal is `git worktree remove` followed by `git worktree prune` (§2).
///
/// **Never `rm -rf` a worktree.** It contains `.plan/` symlinked to main's
/// `.plan/`, and a `vendor/` full of per-package symlinks into main. A recursive
/// delete that follows symlinks destroys the main checkout. If this refuses, the
/// refusal is surfaced — it is never escalated to `--force` and never falls back
/// to a filesystem delete.
pub fn worktree_remove(main: &Path, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    git(main, &["worktree", "remove", &path_str]).with_context(|| {
        format!(
            "git worktree remove refused for {}; not escalating to --force",
            path.display()
        )
    })?;
    let _ = git(main, &["worktree", "prune"]);
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

pub fn fetch_upstream(main: &Path) -> Result<()> {
    git(main, &["fetch", "upstream", "develop", "--no-tags"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let set = parse_status(&raw, false);
        assert_eq!(set.staged.len(), 1);
        assert_eq!(set.unstaged.len(), 1);
        assert_eq!(set.staged[0].path, "src/Foo.php");
    }

    #[test]
    fn a_staged_only_entry_does_not_appear_as_unstaged() {
        let raw = rec(&["1 M. N... 100644 100644 100644 aaa bbb src/Foo.php"]);
        let set = parse_status(&raw, false);
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
        let set = parse_status(&raw, false);
        assert_eq!(set.staged.len(), 1);
        assert_eq!(set.staged[0].path, "src/New.php");
        // The old path must not be read back as an entry of its own.
        assert_eq!(set.untracked.len(), 1);
        assert_eq!(set.untracked[0].path, "untracked.txt");
    }

    #[test]
    fn excludes_sibling_worktrees_from_mains_view() {
        let raw = rec(&[
            "? .claude/worktrees/other/file.php",
            "? src/Mine.php",
        ]);
        let set = parse_status(&raw, true);
        assert_eq!(set.untracked.len(), 1);
        assert_eq!(set.untracked[0].path, "src/Mine.php");
    }

    #[test]
    fn keeps_worktree_paths_when_not_excluding() {
        let raw = rec(&["? .claude/worktrees/other/file.php"]);
        let set = parse_status(&raw, false);
        assert_eq!(set.untracked.len(), 1);
    }

    fn scratch_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("orchd-git-{}-{:?}", std::process::id(), std::thread::current().id()));
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
        dir
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
}
