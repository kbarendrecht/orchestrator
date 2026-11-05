//! Where two workspaces changed the same file, and whether the same lines.
//!
//! **Two agents in two worktrees find out they collided at merge time**, after
//! both have built on their half. Each tree's changed-file list is already in
//! memory from the reconcile, so the first question costs nothing; the second
//! costs two `git diff -U0` runs per shared file, which is why it is bounded and
//! only asked about files the first question found.
//!
//! "Same lines" is a near-miss test, not a merge. Both sides are compared on the
//! *old* side of their diff, which means the same thing only when the two trees
//! branched from the same commit, so a pair with different merge-bases is told
//! about the file and nothing finer. Ranges that touch count as well as ranges
//! that overlap, because git refuses to merge two hunks with no unchanged line
//! between them.

use std::collections::HashSet;
use std::path::Path;

use orchd_base::git::git;
use orchd_base::model::DiffFile;

/// One file both trees changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    /// Whether the changed lines overlap or touch, as far as could be measured.
    pub lines: bool,
    /// What `lines` was measured against: see [`Stamp`].
    pub stamp: Stamp,
}

/// Both sides' added and deleted counts for one file, and whether the trees
/// shared a base when it was measured.
///
/// **What lets a refresh skip git.** A reconcile runs after tool calls and every
/// 15 seconds from the watcher, for every workspace, and a pair whose counts have
/// not moved is a pair whose answer has almost certainly not moved either. The
/// "almost": a rewrite that keeps both counts can move a hunk without changing
/// the stamp, and the answer then waits for the next edit that does.
pub type Stamp = [u32; 5];

/// A file two workspaces both changed, as the daemon holds it: ordered so `a` is
/// the smaller id, which keeps one entry per pair whichever side refreshed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub a: String,
    pub b: String,
    pub found: Found,
}

impl Pair {
    pub fn involves(&self, workspace: &str) -> bool {
        self.a == workspace || self.b == workspace
    }

    /// The workspace on the other side from `workspace`.
    pub fn other(&self, workspace: &str) -> &str {
        if self.a == workspace {
            &self.b
        } else {
            &self.a
        }
    }
}

/// Line comparisons one call may run. Each is two `git diff` processes, and a
/// refresh compares a tree with every other: a branch that touched two hundred
/// shared files is told "same file" about the rest rather than holding a
/// blocking thread for the lot.
pub const LINE_BUDGET: usize = 16;

/// The files `a` and `b` both changed, generated ones left out, sorted by path.
///
/// `base` is the commit both trees branched from, when it is the same one; the
/// line test runs only then. `known` is what this pair answered last time, reused
/// for a file whose [`Stamp`] has not moved. Blocking: it may run git.
pub fn compare(
    a: &Path,
    a_files: &[DiffFile],
    b: &Path,
    b_files: &[DiffFile],
    base: Option<&str>,
    known: &[Found],
    budget: &mut usize,
) -> Vec<Found> {
    let theirs: HashSet<&str> = b_files.iter().map(|f| f.path.as_str()).collect();
    let mut found = Vec::new();
    for mine in a_files.iter().filter(|f| theirs.contains(f.path.as_str())) {
        // A lockfile changes on every branch that adds a dependency and is
        // regenerated rather than merged: flagging it would put a mark on nearly
        // every pair and teach people to ignore the mark.
        if crate::diff::is_generated(a, &mine.path) {
            continue;
        }
        let Some(other) = b_files.iter().find(|f| f.path == mine.path) else {
            continue;
        };
        let stamp = [
            mine.added,
            mine.deleted,
            other.added,
            other.deleted,
            u32::from(base.is_some()),
        ];
        let before = known
            .iter()
            .find(|k| k.path == mine.path && k.stamp == stamp);
        let lines = match (base, Some(other)) {
            _ if before.is_some() => before.is_some_and(|k| k.lines),
            // Both created it: there is no old side to compare, and two new files
            // at one path are an add/add conflict whatever they hold.
            (_, Some(o)) if is_new(mine) && is_new(o) => true,
            (Some(base), Some(_)) if *budget > 0 => {
                *budget -= 1;
                touches(
                    &changed_ranges(a, base, &mine.path),
                    &changed_ranges(b, base, &mine.path),
                )
            }
            _ => false,
        };
        found.push(Found {
            path: mine.path.clone(),
            lines,
            stamp,
        });
    }
    found.sort_by(|x, y| x.path.cmp(&y.path));
    found
}

fn is_new(f: &DiffFile) -> bool {
    f.status == "?" || f.status.starts_with('A')
}

/// The old-side line ranges `path` changed in the tree at `cwd` since `base`,
/// working tree included. Empty when git cannot say.
fn changed_ranges(cwd: &Path, base: &str, path: &str) -> Vec<(u32, u32)> {
    git(
        cwd,
        &[
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            base,
            "--",
            path,
        ],
    )
    .map(|out| ranges(&out))
    .unwrap_or_default()
}

/// Each hunk's old-side range from `-U0` output, as half-open `[start, end)`.
///
/// A pure insertion (`-12,0`) changes no old line; it sits *after* line 12, so it
/// is taken as `[12, 13)`: an edit to line 12 or 13 on the other side is a
/// change git would call adjacent.
pub fn ranges(u0: &str) -> Vec<(u32, u32)> {
    u0.lines()
        .filter_map(|l| l.strip_prefix("@@ -"))
        .filter_map(|rest| {
            let old = rest.split_whitespace().next()?;
            let (start, count) = match old.split_once(',') {
                Some((s, c)) => (s.parse::<u32>().ok()?, c.parse::<u32>().ok()?),
                None => (old.parse::<u32>().ok()?, 1),
            };
            Some(if count == 0 {
                (start, start + 1)
            } else {
                (start, start + count)
            })
        })
        .collect()
}

/// Whether any range in `a` overlaps or touches any in `b`.
pub fn touches(a: &[(u32, u32)], b: &[(u32, u32)]) -> bool {
    a.iter()
        .any(|&(s1, e1)| b.iter().any(|&(s2, e2)| s1 <= e2 && s2 <= e1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hunk_header_is_its_old_side_range() {
        let out = "diff --git a/x b/x\n@@ -3,2 +3,2 @@ fn a\n-a\n+b\n@@ -10 +10 @@\n-c\n+d\n@@ -20,0 +21,4 @@\n+e\n";
        assert_eq!(ranges(out), [(3, 5), (10, 11), (20, 21)]);
    }

    #[test]
    fn ranges_that_touch_collide_and_ranges_a_line_apart_do_not() {
        // Lines 3-4 against line 5: adjacent, which git will not merge cleanly.
        assert!(touches(&[(3, 5)], &[(5, 6)]));
        assert!(touches(&[(3, 5)], &[(4, 5)]));
        // Lines 3-4 against line 6: one unchanged line between them.
        assert!(!touches(&[(3, 5)], &[(6, 7)]));
        assert!(!touches(&[], &[(1, 2)]));
    }

    /// Two real trees off one base: a shared file changed on the same line, one
    /// changed far apart, a lockfile both touched, and a file only one changed.
    #[test]
    fn two_trees_are_compared_by_file_and_then_by_line() {
        let root = std::env::temp_dir().join(format!("orchd-overlap-{}", uuid::Uuid::new_v4()));
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();
        let g = |cwd: &Path, args: &[&str]| {
            git(cwd, args).unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        };
        g(&main, &["init", "-q", "-b", "main"]);
        g(&main, &["config", "user.email", "t@t"]);
        g(&main, &["config", "user.name", "t"]);
        let body: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        for f in ["same.txt", "apart.txt", "mine.txt"] {
            std::fs::write(main.join(f), &body).unwrap();
        }
        std::fs::write(main.join("Cargo.lock"), "a\n").unwrap();
        g(&main, &["add", "-A"]);
        g(&main, &["commit", "-qm", "base"]);
        let base = git(&main, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        let other = root.join("other");
        g(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "other",
                other.to_str().unwrap(),
            ],
        );

        let edit = |tree: &Path, file: &str, line: usize| {
            let mut lines: Vec<String> = body.lines().map(String::from).collect();
            lines[line - 1] = format!("changed {line}");
            std::fs::write(tree.join(file), lines.join("\n") + "\n").unwrap();
        };
        edit(&main, "same.txt", 10);
        edit(&other, "same.txt", 11);
        edit(&main, "apart.txt", 5);
        edit(&other, "apart.txt", 30);
        edit(&main, "mine.txt", 1);
        std::fs::write(main.join("Cargo.lock"), "b\n").unwrap();
        std::fs::write(other.join("Cargo.lock"), "c\n").unwrap();
        std::fs::write(main.join("new.txt"), "x\n").unwrap();
        std::fs::write(other.join("new.txt"), "y\n").unwrap();

        let files = |names: &[(&str, &str)]| -> Vec<DiffFile> {
            names
                .iter()
                .map(|(p, st)| DiffFile {
                    path: (*p).into(),
                    status: (*st).into(),
                    added: 0,
                    deleted: 0,
                    binary: false,
                    eager: false,
                    old_path: None,
                    staged: false,
                    unstaged: true,
                })
                .collect()
        };
        let mine = files(&[
            ("Cargo.lock", "M"),
            ("apart.txt", "M"),
            ("mine.txt", "M"),
            ("new.txt", "?"),
            ("same.txt", "M"),
        ]);
        let theirs = files(&[
            ("Cargo.lock", "M"),
            ("apart.txt", "M"),
            ("new.txt", "?"),
            ("same.txt", "M"),
        ]);
        let mut budget = LINE_BUDGET;
        let found = compare(&main, &mine, &other, &theirs, Some(&base), &[], &mut budget);
        let want = |path: &str, lines| Found {
            path: path.into(),
            lines,
            stamp: [0, 0, 0, 0, 1],
        };
        assert_eq!(
            found,
            [
                want("apart.txt", false),
                want("new.txt", true),
                want("same.txt", true)
            ]
        );
        assert_eq!(
            budget,
            LINE_BUDGET - 2,
            "the two files with an old side were measured"
        );

        // Different bases: the file is still shared, and the lines are not claimed.
        let mut budget = LINE_BUDGET;
        let found = compare(&main, &mine, &other, &theirs, None, &[], &mut budget);
        assert_eq!(
            found.iter().filter(|f| f.lines).count(),
            1,
            "only the add/add"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
