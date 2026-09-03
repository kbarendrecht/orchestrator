//! First-run logic: the recent-projects list and validating a chosen folder.
//!
//! The pure half of the first-boot flow — no window, no daemon, no Tauri — so it
//! runs and is tested like everything else. The desktop crate's bootstrap server
//! serves these over HTTP and adds the two things that need the app: the native
//! folder dialog and starting the daemon. Detection of a repo's settings (base
//! branch, GitHub repo, processes) is the review step and lands beside this later.
//!
//! Recents live in the config dir, so `ORCHD_CONFIG_DIR` relocates them with
//! everything else — which is what lets a test point the whole list at a temp dir.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;

/// A project opened before, newest first. The path is absolute; the name is its
/// last component, which is what a person recognises the checkout by.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentProject {
    pub path: String,
    pub name: String,
    /// Milliseconds since the epoch of the last open. The page renders "2 hours
    /// ago" from it; stored as a number so it needs no locale.
    pub last_opened_ms: u64,
}

/// What a valid checkout looks like to the open screen: enough to confirm the
/// choice before the daemon is asked to start on it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProjectInfo {
    pub path: String,
    pub name: String,
}

fn recent_file_in(dir: &Path) -> PathBuf {
    dir.join("recent.json")
}

/// The last component of a path, as a display name. `orchestrator` for
/// `~/development/orchestrator`. Falls back to the whole path if there is no
/// component (the filesystem root), which no real checkout is.
fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The recent-projects list, newest first. Missing or corrupt file reads as empty
/// rather than failing — a first run has no list, and a garbled one should not keep
/// the window shut.
pub fn recent_projects() -> Vec<RecentProject> {
    match Config::config_dir() {
        Ok(dir) => recent_projects_in(&dir),
        Err(_) => Vec::new(),
    }
}

fn recent_projects_in(dir: &Path) -> Vec<RecentProject> {
    let Ok(raw) = std::fs::read_to_string(recent_file_in(dir)) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// How many to keep. Long enough to cover the repos anyone juggles, short enough
/// that the list stays a glance rather than a history.
const MAX_RECENT: usize = 12;

/// Record that `path` was just opened: move it to the front with a fresh timestamp,
/// drop any older entry for the same path, and cap the list. Best effort — a failure
/// to write the list must never fail an open, so the caller logs and carries on.
pub fn record_recent(path: &Path) -> Result<()> {
    let dir = Config::config_dir()?;
    record_recent_in(&dir, path)
}

fn record_recent_in(dir: &Path, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    let mut list = recent_projects_in(dir);
    list.retain(|r| r.path != path_str);
    list.insert(
        0,
        RecentProject {
            name: name_of(path),
            path: path_str,
            last_opened_ms: now_ms(),
        },
    );
    list.truncate(MAX_RECENT);

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = recent_file_in(dir);
    std::fs::write(&file, serde_json::to_string_pretty(&list)? + "\n")
        .with_context(|| format!("writing {}", file.display()))?;
    Ok(())
}

/// Whether a chosen folder can host a session board, and its display name.
///
/// The two things that make an open fail later if they are wrong now: the folder
/// has to exist, and it has to be a git repository — the whole model is worktrees
/// cut from one checkout. `.git` as a file counts (a linked worktree), though
/// pointing the daemon at a worktree rather than its main checkout is a separate
/// mistake this does not police. Returns a human message, not an error type,
/// because it goes straight to the page.
pub fn validate(path: &Path) -> std::result::Result<ProjectInfo, String> {
    if !path.exists() {
        return Err("No such folder.".into());
    }
    if !path.is_dir() {
        return Err("That is a file, not a folder.".into());
    }
    if !path.join(".git").exists() {
        return Err("Not a git repository — orchd works on a git checkout.".into());
    }
    Ok(ProjectInfo {
        name: name_of(path),
        path: path.to_string_lossy().into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique dir per test, so the recents functions can be exercised through
    /// their dir-taking half with no global `ORCHD_CONFIG_DIR` — the tests run in
    /// parallel, and one process-wide env var would race between them.
    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orchd-firstrun-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_repo(at: &Path) {
        std::fs::create_dir_all(at.join(".git")).unwrap();
    }

    #[test]
    fn validate_wants_a_folder_that_is_a_git_repo() {
        let base = std::env::temp_dir().join(format!("orchd-val-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        let missing = base.join("nope");
        assert!(validate(&missing).is_err(), "a path that does not exist");

        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(validate(&plain).is_err(), "a folder with no .git");

        let repo = base.join("myrepo");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);
        let info = validate(&repo).expect("a git repo validates");
        assert_eq!(info.name, "myrepo", "the name is the last path component");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn recents_round_trip_newest_first_without_duplicates() {
        let dir = tmp("roundtrip");
        assert!(recent_projects_in(&dir).is_empty(), "a fresh dir has no recents");

        record_recent_in(&dir, Path::new("/a/alpha")).unwrap();
        record_recent_in(&dir, Path::new("/b/bravo")).unwrap();
        let list = recent_projects_in(&dir);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].path, "/b/bravo", "the most recent is first");
        assert_eq!(list[0].name, "bravo");

        // Re-opening alpha moves it to the front and does not duplicate it.
        record_recent_in(&dir, Path::new("/a/alpha")).unwrap();
        let list = recent_projects_in(&dir);
        assert_eq!(list.len(), 2, "the same path is not listed twice");
        assert_eq!(list[0].path, "/a/alpha");
    }

    #[test]
    fn recents_are_capped() {
        let dir = tmp("capped");
        for i in 0..(MAX_RECENT + 5) {
            record_recent_in(&dir, &PathBuf::from(format!("/p/repo{i}"))).unwrap();
        }
        assert_eq!(recent_projects_in(&dir).len(), MAX_RECENT, "the list is bounded");
    }

    #[test]
    fn a_corrupt_recent_file_reads_as_empty() {
        let dir = tmp("corrupt");
        std::fs::write(recent_file_in(&dir), "not json").unwrap();
        assert!(recent_projects_in(&dir).is_empty(), "garbage does not keep the window shut");
    }
}
