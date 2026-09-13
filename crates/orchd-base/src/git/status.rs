use anyhow::Result;
use std::path::Path;

use crate::model::{ChangedFile, FileSet, FileStatus};

use super::*;

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

/// The same answer for **one path**, for a caller holding a path rather than
/// drawing a tree.
///
/// A pathspec because the unscoped call walks the whole worktree, and a worktree
/// is the one place that walk is never cheap: `configure_repo` sets fsmonitor on
/// main only, so every other tree pays a full scan (§2). `file_verb` asks about a
/// single file per click and used to pay that.
pub fn status_of(cwd: &Path, rel: &str) -> Result<FileSet> {
    let raw = git_raw(
        cwd,
        &[
            "status",
            "--porcelain=v2",
            "--untracked-files=all",
            "-z",
            "--",
            rel,
        ],
    )?;
    Ok(parse_status(&raw, None))
}

pub(super) fn parse_status(raw: &[u8], exclude: Option<&str>) -> FileSet {
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
pub(super) fn push_xy(set: &mut FileSet, xy: &str, path: &str, exclude: Option<&str>) {
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

pub(super) fn skip(path: &str, exclude: Option<&str>) -> bool {
    exclude.is_some_and(|prefix| path.starts_with(prefix))
}
