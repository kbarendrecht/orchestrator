//! Where two workspaces changed the same file, and whether git can merge them.
//!
//! **Two agents in two worktrees find out they collided at merge time**, after
//! both have built on their half. Each tree's changed-file list is already in
//! memory from the reconcile, so the first question costs nothing; the second
//! costs two `git diff -U0` runs per shared file, which is why it is bounded and
//! only asked about files the first question found.
//!
//! **A conflict is asked of git, and only for a pair that shares a file.** Each
//! side's tree becomes a commit no branch points to (`git stash create`), and
//! `git merge-tree` merges the two in memory: three git runs per pair, against
//! the two per *file* the old guess cost, and an answer rather than a guess.
//! Pairs that share nothing never reach git, which is what keeps this cheap.
//!
//! The guess is still here, as the fallback for a git too old for `merge-tree
//! --write-tree` (2.38): both sides compared on the *old* side of their diff,
//! which means the same thing only when the two trees branched from the same
//! commit. Ranges that touch count as well as ranges that overlap, because git
//! refuses to merge two hunks with no unchanged line between them.

use std::collections::HashSet;
use std::path::Path;

use orchd_base::git::git;
use orchd_base::model::DiffFile;

/// One file both trees changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    /// Whether git cannot merge the two sides of this file. Measured by a merge
    /// where git allows it, and guessed from the changed lines where it does not.
    pub conflict: bool,
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
    let mut shared = Vec::new();
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
        shared.push((mine, other, stamp));
    }
    let before =
        |path: &str, stamp: &Stamp| known.iter().find(|k| k.path == path && &k.stamp == stamp);
    // One merge for the pair, and none when every shared file still measures what
    // it did last time.
    let fresh = shared.iter().any(|(m, _, s)| before(&m.path, s).is_none());
    let merged: Option<HashSet<String>> = (fresh && *budget > 0)
        .then(|| {
            *budget -= 1;
            real_conflicts(a, b)
        })
        .flatten();

    let mut found: Vec<Found> = shared
        .into_iter()
        .map(|(mine, other, stamp)| {
            let untracked = mine.status == "?" || other.status == "?";
            let conflict = if is_new(mine) && is_new(other) && untracked {
                // Both created it and one has not committed it, so the merge's
                // snapshot cannot see it: the two files themselves say. Equal is
                // no conflict, which is every file a stacked branch carries from
                // the branch under it.
                differ(a, b, &mine.path)
            } else if let Some(set) = &merged {
                set.contains(&mine.path)
            } else if let Some(k) = before(&mine.path, &stamp) {
                k.conflict
            } else if let (Some(base), true) = (base, *budget > 0) {
                *budget -= 1;
                touches(
                    &changed_ranges(a, base, &mine.path),
                    &changed_ranges(b, base, &mine.path),
                )
            } else {
                false
            };
            Found {
                path: mine.path.clone(),
                conflict,
                stamp,
            }
        })
        .collect();
    found.sort_by(|x, y| x.path.cmp(&y.path));
    found
}

/// Whether the file at `path` differs between the two trees, reading the files.
/// A folder git reported whole (`src/new/`) cannot be compared, and counts.
fn differ(a: &Path, b: &Path, path: &str) -> bool {
    if path.ends_with('/') {
        return true;
    }
    match (std::fs::read(a.join(path)), std::fs::read(b.join(path))) {
        (Ok(x), Ok(y)) => x != y,
        _ => true,
    }
}

/// Two branches where one is built on the other: the lower one's commits, or
/// some of them, are in the upper one's history past the base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    /// Whether the first tree of the pair is the lower one.
    pub lower_is_a: bool,
    /// Commits on the lower branch that the upper one does not have yet.
    pub behind: u32,
    /// The lower branch's head, so "it moved" is said once per move.
    pub lower_tip: String,
    /// The newest lower commit the upper branch has, by id or by a copy of it.
    /// What a copied stack is recognised from again once the lower branch has
    /// moved past it ([`stack_of`]'s `before`).
    pub anchor: String,
    /// How many commits the lower branch has past the base, so a branch built on
    /// two others is shown on the nearer one.
    pub lower_commits: u32,
}

/// Whether the trees at `a` and `b` are a stack, and which way up.
///
/// **A stack shares history past the base.** Two peers branched from
/// `upstream` share nothing after it; a branch cut from another's commit shares
/// that commit, which is not in `upstream`. Their overlap is then not a dispute:
/// the upper one rebases onto the lower one, which is a restack, the agents' own
/// job (`TODO.md`).
///
/// **The lower one is the branch created first**, read from each branch's reflog.
/// Both histories hold the shared commits, so the graph alone cannot say which
/// branch they were made on. Without a reflog, a branch whose head *is* the
/// shared commit is the lower one; with neither, `None`, and the pair is left as
/// peers. Blocking: it runs git.
pub fn stack_of(a: &Path, b: &Path, upstream: &str, before: Option<&Stack>) -> Option<Stack> {
    let head = |p: &Path| {
        git(p, &["rev-parse", "HEAD"])
            .ok()
            .map(|s| s.trim().to_string())
    };
    let (ha, hb) = (head(a)?, head(b)?);
    let shared = git(a, &["merge-base", &ha, &hb]).ok()?.trim().to_string();
    // When the branch was made: the oldest reflog entry's own date. Not `%ct`,
    // which is the commit's, and a branch cut from another starts on the very
    // commit the other one is on, so both would read the same.
    let born = |p: &Path| -> Option<u64> {
        let branch = git(p, &["symbolic-ref", "--short", "HEAD"]).ok()?;
        let log = git(
            p,
            &[
                "reflog",
                "show",
                "--date=unix",
                "--format=%gd",
                &format!("refs/heads/{}", branch.trim()),
            ],
        )
        .ok()?;
        let last = log.lines().last()?;
        last.rsplit_once('{')?.1.trim_end_matches('}').parse().ok()
    };
    // Shared history that `upstream` already has is no stack by the graph; it
    // may still be one whose commits a rebase copied ([`copied_stack`]).
    if git(a, &["merge-base", "--is-ancestor", &shared, upstream]).is_ok() {
        return copied_stack(a, &ha, &hb, upstream, before);
    }
    let lower_is_a = match (born(a), born(b)) {
        (Some(x), Some(y)) if x != y => x < y,
        _ if ha == shared => true,
        _ if hb == shared => false,
        _ => return None,
    };
    let (lower, upper) = if lower_is_a { (&ha, &hb) } else { (&hb, &ha) };
    let count = |range: String| -> u32 {
        git(a, &["rev-list", "--count", &range])
            .ok()
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0)
    };
    Some(Stack {
        lower_is_a,
        behind: count(format!("{upper}..{lower}")),
        lower_tip: lower.clone(),
        anchor: shared,
        lower_commits: count(format!("{upstream}..{lower}")),
    })
}

/// A stack whose commits a rebase onto a newer base copied: the upper branch
/// carries the lower one's commits under new ids, so the graph shares nothing
/// past the base.
///
/// **The upper branch must carry every lower commit**, by its change (`git
/// cherry`) or its subject; a rebase that resolved conflicts keeps the subject
/// and changes the change. The strictness is the point. Two branches that each
/// carry copies of a *third* branch's commits match each other on those, and a
/// looser rule ("half of one side") called such siblings stacked and told one
/// session it sat on four branches. Siblings each have commits the other lacks,
/// so neither carries all of the other.
///
/// **And it survives the lower branch moving on.** Once the lower one gets a
/// commit the upper lacks, "every commit" fails; so a pair that was a stack
/// (`before`) stays one while its `anchor`, the last lower commit the upper
/// carried, is still in the lower branch and everything up to it is still
/// carried. The commits after it are what the upper is behind.
fn copied_stack(
    cwd: &Path,
    ha: &str,
    hb: &str,
    upstream: &str,
    before: Option<&Stack>,
) -> Option<Stack> {
    let base = |h: &str| {
        git(cwd, &["merge-base", h, upstream])
            .ok()
            .map(|s| s.trim().to_string())
    };
    let (ba, bb) = (base(ha)?, base(hb)?);
    let commits = |range: String| -> Vec<(String, String)> {
        git(cwd, &["log", "--reverse", "--format=%H%x09%s", &range])
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(id, subject)| (id.to_string(), subject.to_string()))
            .collect()
    };
    // Which of `mine` the head `theirs` carries, by change or by subject, and
    // whether any matched by change.
    let carried = |mine: &[(String, String)],
                   my_base: &str,
                   my_head: &str,
                   their_base: &str,
                   their_head: &str| {
        let by_change: std::collections::HashSet<String> =
            git(cwd, &["cherry", their_head, my_head, my_base])
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.strip_prefix("- "))
                .map(|id| id.trim().to_string())
                .collect();
        let theirs = commits(format!("{their_base}..{their_head}"));
        let subjects: std::collections::HashSet<&str> =
            theirs.iter().map(|(_, s)| s.as_str()).collect();
        let kept: Vec<bool> = mine
            .iter()
            .map(|(id, s)| by_change.contains(id) || subjects.contains(s.as_str()))
            .collect();
        (kept, !by_change.is_empty())
    };
    let (ca, cb) = (
        commits(format!("{ba}..{ha}")),
        commits(format!("{bb}..{hb}")),
    );
    if ca.is_empty() || cb.is_empty() {
        return None;
    }
    let (ka, xa) = carried(&ca, &ba, ha, &bb, hb);
    let (kb, xb) = carried(&cb, &bb, hb, &ba, ha);
    // One commit matched on its subject alone is not enough to call a stack.
    let all =
        |kept: &[bool], by_change: bool| kept.iter().all(|k| *k) && (kept.len() >= 2 || by_change);
    let made = |lower_is_a: bool, behind: usize, anchor: String| {
        let (tip, list) = if lower_is_a { (ha, &ca) } else { (hb, &cb) };
        Some(Stack {
            lower_is_a,
            behind: u32::try_from(behind).unwrap_or(u32::MAX),
            lower_tip: tip.to_string(),
            anchor,
            lower_commits: u32::try_from(list.len()).unwrap_or(u32::MAX),
        })
    };
    match (all(&ka, xa), all(&kb, xb)) {
        (true, false) => return made(true, 0, ha.to_string()),
        (false, true) => return made(false, 0, hb.to_string()),
        // The same commits on both sides is the same work twice, not a stack.
        (true, true) => return None,
        (false, false) => {}
    }
    // Not carried whole: still the stack it was, if the lower branch only moved on.
    let before = before?;
    let (list, kept) = if before.lower_is_a {
        (&ca, &ka)
    } else {
        (&cb, &kb)
    };
    let at = list.iter().position(|(id, _)| *id == before.anchor)?;
    if !kept[..=at].iter().all(|k| *k) {
        return None;
    }
    made(
        before.lower_is_a,
        list.len() - at - 1,
        before.anchor.clone(),
    )
}

/// The commit in the upper tree's own history where its own commits start: the
/// last lower commit it carries. What `git rebase --onto <base> <this>` cuts at
/// once the lower branch is squash-merged.
///
/// The anchor itself on a stack the graph found. On a copied stack the anchor is
/// the lower branch's commit and the upper one carries a copy of it, found by
/// its subject among the upper's commits past `upstream`. `None` when neither is
/// there, and the caller says so rather than guessing a cut. Blocking.
pub fn restack_point(upper: &Path, stack: &Stack, upstream: &str) -> Option<String> {
    if git(
        upper,
        &["merge-base", "--is-ancestor", &stack.anchor, "HEAD"],
    )
    .is_ok()
    {
        return Some(stack.anchor.clone());
    }
    let subject = git(upper, &["log", "-1", "--format=%s", &stack.anchor]).ok()?;
    let subject = subject.trim();
    git(
        upper,
        &["log", "--format=%H%x09%s", &format!("{upstream}..HEAD")],
    )
    .ok()?
    .lines()
    .filter_map(|l| l.split_once('\t'))
    .find(|(_, s)| *s == subject)
    .map(|(id, _)| id.to_string())
}

/// The lower branch's pushed head, when the upper tree lacks something in it.
///
/// What a pushed upper branch follows: only what the lower one pushed, because
/// its unpushed commits are not in the lower PR and would show up in the upper
/// one. A commit the upper tree already has a copy of does not count. Blocking.
pub fn pushed_lower_ahead(upper: &Path, lower_branch: &str) -> Option<String> {
    let tip = git(
        upper,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/origin/{lower_branch}"),
        ],
    )
    .ok()?
    .trim()
    .to_string();
    let missing = git(
        upper,
        &[
            "rev-list",
            "--count",
            "--cherry-pick",
            "--right-only",
            &format!("HEAD...{tip}"),
        ],
    )
    .ok()?;
    (missing.trim() != "0").then_some(tip)
}

/// The files git cannot merge between the tree at `tree` as it stands and
/// `upstream`, or `None` when it could not be asked.
///
/// **What a PR or CI would find out later**, asked while the agent can still act
/// on it. Two git runs, so the caller asks only for a tree that is behind and
/// whose changes moved since it last asked.
pub fn upstream_conflicts(tree: &Path, upstream: &str) -> Option<Vec<String>> {
    use orchd_base::git::{merge_conflicts, snapshot_commit};
    let mine = snapshot_commit(tree).ok()?;
    match merge_conflicts(tree, &mine, upstream) {
        Ok(paths) => Some(paths),
        Err(e) => {
            tracing::debug!("no merge-tree for {} and {upstream}: {e:#}", tree.display());
            None
        }
    }
}

/// The files git cannot merge between the two trees as they stand, or `None`
/// when it could not be asked (an older git, a tree mid-rebase).
fn real_conflicts(a: &Path, b: &Path) -> Option<HashSet<String>> {
    use orchd_base::git::{merge_conflicts, snapshot_commit};
    let (x, y) = (snapshot_commit(a).ok()?, snapshot_commit(b).ok()?);
    match merge_conflicts(a, &x, &y) {
        Ok(paths) => Some(paths.into_iter().collect()),
        Err(e) => {
            tracing::debug!(
                "no merge-tree for {} and {}: {e:#}",
                a.display(),
                b.display()
            );
            None
        }
    }
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
    /// A lower branch rewritten after the upper one copied it: the cut is the
    /// upper's copy of the last lower commit, found by its subject, and a rebase
    /// from there keeps only the upper's own commit on the new version.
    #[test]
    fn a_stale_copy_of_the_lower_branch_is_cut_at_its_copy() {
        let root = std::env::temp_dir().join(format!("orchd-stale-{}", uuid::Uuid::new_v4()));
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();
        let g = |args: &[&str]| git(&main, args).unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        let commit = |file: &str, body: &str, msg: &str| {
            std::fs::write(main.join(file), body).unwrap();
            g(&["add", "-A"]);
            g(&["commit", "-qm", msg]);
        };
        commit("base.txt", "base\n", "base");
        g(&["checkout", "-qb", "lower"]);
        commit("lower.txt", "one\n", "lower work");
        g(&["checkout", "-qb", "upper"]);
        commit("upper.txt", "mine\n", "upper work");
        // The lower branch is rewritten: same subject, new content.
        g(&["checkout", "-q", "lower"]);
        std::fs::write(main.join("lower.txt"), "two\n").unwrap();
        g(&["commit", "-qa", "--amend", "--no-edit"]);
        let tip = git(&main, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        g(&["checkout", "-q", "upper"]);

        let stack = Stack {
            lower_is_a: true,
            behind: 0,
            lower_tip: tip.clone(),
            anchor: tip.clone(),
            lower_commits: 1,
        };
        let point = restack_point(&main, &stack, "main").expect("the copy is found by its subject");
        assert_ne!(
            point, tip,
            "the cut is the upper's copy, not the new lower commit"
        );
        orchd_base::git::rebase_or_abort(&main, &tip, Some(&point)).unwrap();
        assert_eq!(
            std::fs::read_to_string(main.join("lower.txt")).unwrap(),
            "two\n",
            "the new version"
        );
        let own = git(&main, &["log", "--format=%s", &format!("{tip}..HEAD")]).unwrap();
        assert_eq!(own.trim(), "upper work", "and only its own commit on top");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A tree that changed a line upstream also changed conflicts with it; one
    /// that changed a different file does not.
    #[test]
    fn a_tree_is_merged_with_upstream_as_it_stands() {
        let root = std::env::temp_dir().join(format!("orchd-upstream-{}", uuid::Uuid::new_v4()));
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();
        let g = |cwd: &Path, args: &[&str]| {
            git(cwd, args).unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        };
        g(&main, &["init", "-q", "-b", "main"]);
        g(&main, &["config", "user.email", "t@t"]);
        g(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join("a.txt"), "one\n").unwrap();
        std::fs::write(main.join("b.txt"), "one\n").unwrap();
        g(&main, &["add", "-A"]);
        g(&main, &["commit", "-qm", "base"]);
        let tree = root.join("tree");
        g(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "work",
                tree.to_str().unwrap(),
            ],
        );
        std::fs::write(main.join("a.txt"), "upstream\n").unwrap();
        g(&main, &["commit", "-qam", "moved on"]);

        // Uncommitted, which is the state an agent is usually in.
        std::fs::write(tree.join("b.txt"), "mine\n").unwrap();
        assert_eq!(upstream_conflicts(&tree, "main"), Some(vec![]));
        std::fs::write(tree.join("a.txt"), "mine\n").unwrap();
        assert_eq!(
            upstream_conflicts(&tree, "main"),
            Some(vec!["a.txt".to_string()])
        );
        let _ = std::fs::remove_dir_all(&root);
    }

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
        let want = |path: &str, conflict| Found {
            path: path.into(),
            conflict,
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
            LINE_BUDGET - 1,
            "one merge for the pair, not one diff per file"
        );

        // Asked again with nothing moved: no git at all.
        let mut again = LINE_BUDGET;
        let same = compare(
            &main,
            &mine,
            &other,
            &theirs,
            Some(&base),
            &found,
            &mut again,
        );
        assert_eq!(same, found);
        assert_eq!(
            again, LINE_BUDGET,
            "an unchanged pair is answered from what it said last time"
        );

        // Different bases: git still merges them, from their own merge-base.
        let mut budget = LINE_BUDGET;
        let found = compare(&main, &mine, &other, &theirs, None, &[], &mut budget);
        assert_eq!(
            found
                .iter()
                .filter(|f| f.conflict)
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["new.txt", "same.txt"],
            "the merge does not need a shared base"
        );
        // And the tree it read is the agent's as it was: no stash, no index change.
        assert_eq!(git(&main, &["stash", "list"]).unwrap(), "");
        assert!(std::fs::read_to_string(main.join("same.txt"))
            .unwrap()
            .contains("changed 10"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One repo, three branches: `develop`, `lower` cut from it, and `upper` cut
    /// from `lower` a minute later. Then `lower` moves on by a commit.
    #[test]
    fn a_branch_built_on_another_is_a_stack_and_knows_which_way_up() {
        let root = std::env::temp_dir().join(format!("orchd-stack-{}", uuid::Uuid::new_v4()));
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();
        // Run with a fixed clock, so the reflog says which branch is older.
        let at = |cwd: &Path, when: &str, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .env("GIT_COMMITTER_DATE", when)
                .env("GIT_AUTHOR_DATE", when)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let early = "2026-01-01T10:00:00Z";
        let later = "2026-01-01T10:01:00Z";
        at(&main, early, &["init", "-q", "-b", "develop"]);
        at(&main, early, &["config", "user.email", "t@t"]);
        at(&main, early, &["config", "user.name", "t"]);
        std::fs::write(main.join("base.txt"), "base\n").unwrap();
        at(&main, early, &["add", "-A"]);
        at(&main, early, &["commit", "-qm", "base"]);
        let lower = root.join("lower");
        at(
            &main,
            early,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "lower",
                lower.to_str().unwrap(),
            ],
        );
        std::fs::write(lower.join("shared.txt"), "one\n").unwrap();
        at(&lower, early, &["add", "-A"]);
        at(&lower, early, &["commit", "-qm", "lower one"]);
        let upper = root.join("upper");
        at(
            &main,
            later,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "upper",
                upper.to_str().unwrap(),
                "lower",
            ],
        );
        std::fs::write(upper.join("mine.txt"), "mine\n").unwrap();
        at(&upper, later, &["add", "-A"]);
        at(&upper, later, &["commit", "-qm", "upper one"]);
        std::fs::write(lower.join("more.txt"), "more\n").unwrap();
        at(&lower, later, &["add", "-A"]);
        at(&lower, later, &["commit", "-qm", "lower two"]);

        let s = stack_of(&upper, &lower, "develop", None).expect("a stack");
        assert!(
            !s.lower_is_a,
            "the branch made first is the lower one, whichever side asks"
        );
        assert_eq!(s.behind, 1, "the commit the upper one does not have");
        assert!(
            stack_of(&lower, &upper, "develop", None)
                .expect("a stack")
                .lower_is_a
        );

        // **Restacked onto a newer `develop`**, the way an agent does it: the
        // upper branch now carries copies of the lower commits, its own one
        // changed on the way, and the graph shares nothing past `develop`.
        std::fs::write(main.join("later.txt"), "later\n").unwrap();
        at(&main, later, &["add", "-A"]);
        at(&main, later, &["commit", "-qm", "develop moves"]);
        at(&upper, later, &["rebase", "-q", "develop"]);
        // The lower branch was one commit ahead before the rebase, so only the
        // stack remembered from before says this is still one.
        assert_eq!(
            stack_of(&upper, &lower, "develop", None),
            None,
            "not carried whole"
        );
        let copied =
            stack_of(&upper, &lower, "develop", Some(&s)).expect("still a stack, by its commits");
        assert!(!copied.lower_is_a, "and still the right way up");
        assert_eq!(copied.behind, 1, "the commit it lacked before the rebase");

        // A sibling: cut from the lower branch as well, with work of its own. It
        // carries the lower commits, so it is stacked on the lower one; it is not
        // stacked on the upper one, nor the upper on it.
        let sibling = root.join("sibling");
        at(
            &main,
            later,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "sibling",
                sibling.to_str().unwrap(),
                "lower",
            ],
        );
        std::fs::write(sibling.join("own.txt"), "own\n").unwrap();
        at(&sibling, later, &["add", "-A"]);
        at(&sibling, later, &["commit", "-qm", "sibling's own"]);
        at(&sibling, later, &["rebase", "-q", "develop"]);
        assert_eq!(
            stack_of(&sibling, &upper, "develop", None),
            None,
            "siblings are not a stack"
        );

        // The lower branch moves on: the copy no longer carries all of it, and
        // the pair it was is still the stack, one commit behind.
        std::fs::write(lower.join("three.txt"), "three\n").unwrap();
        at(&lower, later, &["add", "-A"]);
        at(&lower, later, &["commit", "-qm", "lower three"]);
        assert_eq!(
            stack_of(&upper, &lower, "develop", None),
            None,
            "without the memory it is not"
        );
        let moved =
            stack_of(&upper, &lower, "develop", Some(&copied)).expect("with it, still a stack");
        assert_eq!(moved.behind, 2, "behind by both commits it lacks");

        // Two branches that share only `develop` are peers.
        let peer = root.join("peer");
        at(
            &main,
            later,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "peer",
                peer.to_str().unwrap(),
                "develop",
            ],
        );
        std::fs::write(peer.join("p.txt"), "p\n").unwrap();
        at(&peer, later, &["add", "-A"]);
        at(&peer, later, &["commit", "-qm", "peer"]);
        assert_eq!(stack_of(&peer, &lower, "develop", None), None);

        // And the file both carry from `lower` is not a conflict: committed on
        // both sides, the merge says so.
        let row = |p: &str, st: &str| DiffFile {
            path: p.into(),
            status: st.into(),
            added: 1,
            deleted: 0,
            binary: false,
            eager: false,
            old_path: None,
            staged: false,
            unstaged: false,
        };
        let mut budget = LINE_BUDGET;
        let found = compare(
            &lower,
            &[row("shared.txt", "A")],
            &upper,
            &[row("shared.txt", "A")],
            None,
            &[],
            &mut budget,
        );
        assert_eq!(found.len(), 1);
        assert!(
            !found[0].conflict,
            "a file both created the same way merges cleanly"
        );
        // Untracked on both sides: the contents decide.
        std::fs::write(lower.join("new.txt"), "same\n").unwrap();
        std::fs::write(upper.join("new.txt"), "same\n").unwrap();
        let mut budget = LINE_BUDGET;
        let same = compare(
            &lower,
            &[row("new.txt", "?")],
            &upper,
            &[row("new.txt", "?")],
            None,
            &[],
            &mut budget,
        );
        assert!(!same[0].conflict, "equal untracked files are no conflict");
        std::fs::write(upper.join("new.txt"), "different\n").unwrap();
        let mut budget = LINE_BUDGET;
        let differ = compare(
            &lower,
            &[row("new.txt", "?")],
            &upper,
            &[row("new.txt", "?")],
            None,
            &[],
            &mut budget,
        );
        assert!(
            differ[0].conflict,
            "and different ones are an add/add conflict"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
