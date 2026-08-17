//! Applying the patches a triage run proposed, safely.
//!
//! A proposed change is a **unified diff** the agent produced by making the edit
//! for real in a throwaway worktree, not a diff it wrote by hand — so it is
//! well-formed by construction, and `git apply` does the rest of the safety
//! work. Measured, not assumed (see the module tests):
//!
//! - a context line that moved under the patch makes it **refuse**, not fuzz
//!   (that is GNU `patch`); so a patch generated at triage time cannot silently
//!   land in the wrong place if the file changed before you accepted it;
//! - non-overlapping edits to one file both apply, offsets sliding;
//! - a combined stream is **atomic** — one bad hunk and nothing lands;
//! - `--check` and `--numstat` are dry runs that write nothing;
//! - `../escape`, a symlink write, and `.git/**` are all refused.
//!
//! The ladder below runs three `git apply` passes before a byte is written, so
//! the reason a batch will not apply can be named — git's own "patch does not
//! apply" cannot tell a stale patch from two that collide with each other.

use anyhow::{Context, Result};
use serde::Serialize;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// One thread's proposed change: the diff, tagged with the thread it answers so
/// a failure can point at the card.
pub struct Patch {
    pub thread_id: String,
    pub diff: String,
}

/// A path the batch will touch, with its line counts — the data behind the
/// card's `will write renovate.json5 +2 −1` label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileStat {
    pub path: String,
    pub added: u32,
    pub deleted: u32,
}

/// What the check ladder concluded. Only `Clean` may be applied.
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    /// Every patch applies, alone and together. Carries the combined file list.
    Clean(Vec<FileStat>),
    /// These threads' patches no longer apply on their own — the file moved
    /// under them since triage. Re-triage is the only fix.
    Stale(Vec<String>),
    /// Each applies alone, but these collide with each other: two threads
    /// patching the same lines. Named so the card can say which.
    Overlap(Vec<String>),
}

/// Run `git apply <flags>` with `diff` on stdin. Returns the exit success,
/// stdout, and trimmed stderr — a failed `--check` is an expected answer, not an
/// error to bail on, so the status is handed back rather than turned into one.
fn git_apply(cwd: &Path, flags: &[&str], diff: &str) -> Result<(bool, String, String)> {
    let mut args = vec!["apply"];
    args.extend_from_slice(flags);
    let mut child = Command::new("git")
        .args(&args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning git apply {}", flags.join(" ")))?;
    child
        .stdin
        .as_mut()
        .context("git apply stdin")?
        .write_all(diff.as_bytes())?;
    drop(child.stdin.take());
    let out = child.wait_with_output()?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

/// Does this diff apply cleanly right now? A dry run; writes nothing.
fn applies(cwd: &Path, diff: &str) -> Result<bool> {
    Ok(git_apply(cwd, &["--check"], diff)?.0)
}

/// The paths a diff touches, with line counts. `--numstat` writes nothing, so
/// this is safe to call before the user has confirmed anything.
fn numstat(cwd: &Path, diff: &str) -> Result<Vec<FileStat>> {
    let (ok, stdout, err) = git_apply(cwd, &["--numstat"], diff)?;
    // The caller only reaches here once the diff has passed --check, so a
    // failure now is a real fault, not a stale patch.
    anyhow::ensure!(ok, "git apply --numstat failed: {err}");
    Ok(aggregate(parse_numstat(&stdout)))
}

/// One row per path. A combined stream carries a separate `diff --git` block per
/// patch, so two threads touching one file yield two numstat rows for it; the
/// card wants one line with the totals.
fn aggregate(rows: Vec<FileStat>) -> Vec<FileStat> {
    let mut out: Vec<FileStat> = Vec::new();
    for r in rows {
        match out.iter_mut().find(|f| f.path == r.path) {
            Some(f) => {
                f.added += r.added;
                f.deleted += r.deleted;
            }
            None => out.push(r),
        }
    }
    out
}

/// `git apply --numstat` prints `<added>\t<deleted>\t<path>` a line each; a
/// binary file prints `-\t-\t<path>`, which we surface as zero counts rather
/// than dropping the path — it is still being written.
fn parse_numstat(raw: &str) -> Vec<FileStat> {
    raw.lines()
        .filter_map(|line| {
            let mut cols = line.splitn(3, '\t');
            let added = cols.next()?;
            let deleted = cols.next()?;
            let path = cols.next()?;
            Some(FileStat {
                path: path.to_string(),
                added: added.parse().unwrap_or(0),
                deleted: deleted.parse().unwrap_or(0),
            })
        })
        .collect()
}

/// The concatenation of every patch's diff — one stream git applies atomically.
fn combined(patches: &[Patch]) -> String {
    patches
        .iter()
        .map(|p| p.diff.as_str())
        .collect::<Vec<_>>()
        .join("")
}

/// The three-pass ladder. A **dry run** — decides whether the batch may be
/// applied and, when it may, reports the file list; writes nothing.
///
/// 1. each patch alone → any that fail are stale (the file moved under them);
/// 2. the combined stream → if each passed alone but the whole fails, it is an
///    overlap *between* threads, and the pair is hunted down to name it;
/// 3. otherwise clean.
pub fn check(cwd: &Path, patches: &[Patch]) -> Result<Check> {
    if patches.is_empty() {
        return Ok(Check::Clean(Vec::new()));
    }

    // Pass 1 — stale detection. A patch that will not apply by itself has gone
    // stale; report every one, not just the first, so re-triage is a single
    // round trip.
    let mut stale = Vec::new();
    for p in patches {
        if !applies(cwd, &p.diff)? {
            stale.push(p.thread_id.clone());
        }
    }
    if !stale.is_empty() {
        return Ok(Check::Stale(stale));
    }

    // Pass 2 — the combined stream. Every patch applied alone, so a failure
    // here is threads colliding with each other.
    let all = combined(patches);
    if !applies(cwd, &all)? {
        return Ok(Check::Overlap(overlapping(cwd, patches)?));
    }

    // Pass 3 — clean. The file list is the combined numstat.
    Ok(Check::Clean(numstat(cwd, &all)?))
}

/// Which threads collide. Every patch applies alone (pass 1 established that),
/// so a colliding set is at least a pair; check pairs to name them. A collision
/// only the full set triggers — no pair explains it — falls back to reporting
/// all of them, which is honest if less precise.
fn overlapping(cwd: &Path, patches: &[Patch]) -> Result<Vec<String>> {
    use std::collections::BTreeSet;
    let mut hit: BTreeSet<String> = BTreeSet::new();
    for i in 0..patches.len() {
        for j in (i + 1)..patches.len() {
            let pair = format!("{}{}", patches[i].diff, patches[j].diff);
            if !applies(cwd, &pair)? {
                hit.insert(patches[i].thread_id.clone());
                hit.insert(patches[j].thread_id.clone());
            }
        }
    }
    if hit.is_empty() {
        return Ok(patches.iter().map(|p| p.thread_id.clone()).collect());
    }
    Ok(hit.into_iter().collect())
}

/// Write the batch, atomically. Call only after [`check`] returned `Clean` — a
/// re-check here would race the very staleness it guards against, so the caller
/// is trusted to have checked immediately before. `git apply` is still atomic,
/// so a patch gone stale in the gap fails the whole write rather than half of it.
pub fn apply(cwd: &Path, patches: &[Patch]) -> Result<()> {
    if patches.is_empty() {
        return Ok(());
    }
    let (ok, _, err) = git_apply(cwd, &[], &combined(patches))?;
    anyhow::ensure!(ok, "applying the accepted patches failed: {err}");
    Ok(())
}

/// What the local half of the batch did, or refused to do.
///
/// Everything here happens before the push, so every failure leaves the branch as
/// it was and the decisions still staged — the design's "nothing left the machine"
/// promise is structural rather than asserted.
#[derive(Debug, PartialEq, Eq)]
pub enum Written {
    /// Patches applied, hooks happy, folded into a commit. Ready to push.
    Committed {
        files: Vec<FileStat>,
        /// How the fold was resolved, so the UI can say when it fell back.
        amend: String,
    },
    /// Nothing to write — every decision was reply-only. Still a success: the
    /// posting half has work even when the code does not.
    NothingToWrite,
    /// The ladder refused. Carries the reason already phrased for a human.
    Refused(String),
}

/// Apply the accepted patches and fold them into a commit — the local half.
///
/// Order matters and differs from an earlier draft of the plan, which ran
/// pre-commit *after* the fixup. That commits unlinted code and then needs the
/// hook's own edits amended in silently, which is precisely the "reformats beyond
/// the approved diff" case the design refuses to absorb. So nothing is committed
/// until the hooks have had their say:
///
/// 1. blame first, while the tree is still the committed state — blame on a dirty
///    tree returns an all-zero sha, which is nobody's commit;
/// 2. check the ladder, 3. apply, 4. pre-commit,
/// 5. refuse if the hooks rewrote anything — with nothing committed, that is free
///    to undo;
/// 6. fold.
pub fn write_batch(
    cwd: &Path,
    merge_base: &str,
    my_email: &str,
    patches: &[Patch],
    touched: &[(String, u32)],
) -> Result<Written> {
    if patches.is_empty() {
        return Ok(Written::NothingToWrite);
    }

    // 1. Blame before anything touches the tree.
    // No rev: nothing has been applied yet, so the working tree *is* the
    // committed state and blaming it is correct.
    let amend = crate::git::amend_target(cwd, None, merge_base, touched, my_email)?;

    // 2. The ladder.
    let files = match check(cwd, patches)? {
        Check::Clean(files) => files,
        Check::Stale(threads) => {
            return Ok(Written::Refused(format!(
                "the file moved under {} since triage — re-triage before accepting",
                threads.join(", ")
            )))
        }
        Check::Overlap(threads) => {
            return Ok(Written::Refused(format!(
                "{} patch the same lines; accept one, or re-triage to get patches that \
                 account for each other",
                threads.join(" and ")
            )))
        }
    };

    // 3. Apply, atomically.
    apply(cwd, patches)?;

    // 4/5. The hooks, on what we wrote.
    let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    match crate::git::pre_commit(cwd, &paths)? {
        crate::git::PreCommit::Passed | crate::git::PreCommit::NotConfigured => {}
        crate::git::PreCommit::NotInstalled => {
            tracing::warn!(
                "`.pre-commit-config.yaml` is present but `pre-commit` is not installed; \
                 pushing code the local hooks did not see"
            );
        }
        crate::git::PreCommit::Failed(detail) => {
            return Ok(Written::Refused(format!(
                "pre-commit failed, so nothing was committed:\n{detail}"
            )))
        }
        crate::git::PreCommit::Reformatted(paths) => {
            return Ok(Written::Refused(format!(
                "the hooks rewrote {} — what would land is no longer what you approved. \
                 Nothing was committed.",
                paths.join(", ")
            )))
        }
    }

    // 6. Fold.
    crate::git::fold_in(cwd, &amend)?;
    Ok(Written::Committed {
        files,
        amend: match amend {
            crate::git::Amend::Fixup(sha) => {
                format!("folded into {}", sha.chars().take(7).collect::<String>())
            }
            crate::git::Amend::Head(why) => format!("amended HEAD — {why}"),
        },
    })
}

/// Fold hand-written edits into the branch — the manual phase's local half.
///
/// The same pass as [`write_batch`] minus the parts that only make sense for a
/// proposed patch: there is nothing to check and nothing to apply, because the
/// edits are already on disk. What is left is what matters — blame to find the
/// commit that owns the line, the hooks, and the fold.
///
/// Two differences from `write_batch`, both forced:
///
/// - **Blame reads `HEAD`, not the working tree.** The human has just edited the
///   very line being blamed, so the bare form would report "not committed yet" for
///   every manual thread and degrade the whole pass to a plain HEAD amend.
/// - **Pre-commit runs on what `git status` reports**, not on a known file list.
///   Nobody declared what was touched; that is the point of the phase, and it is
///   also what makes the file list complete rather than trusted.
pub fn write_manual(
    cwd: &Path,
    merge_base: &str,
    my_email: &str,
    touched: &[(String, u32)],
    approved: &[String],
    // The sha the reviewers' anchor lines were validated against.
    anchored_at: &str,
) -> Result<Written> {
    let changed = crate::git::status(cwd, false)?;
    let paths: Vec<String> = changed
        .staged
        .iter()
        .chain(changed.unstaged.iter())
        .chain(changed.untracked.iter())
        .map(|f| f.path.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if paths.is_empty() {
        // A Manual thread does not have to change code — "I did this in another
        // PR" is a legitimate answer, and the comment is what was required.
        return Ok(Written::NothingToWrite);
    }

    // Everything dirty must be something the phase showed you, because
    // `git::fold_in` commits with `git add -A` and the clean-tree gate is stood
    // down on this path — so a scratch file a session left behind would be folded
    // into the commit and force-pushed into somebody's PR. That is exactly the
    // sweep this design rejected when it rejected letting your edits ride along
    // with the batch.
    //
    // Refused rather than scoped: staging only the approved paths leaves the rest
    // dirty, and the `--autosquash` rebase inside `fold_in` will not run on an
    // unclean tree — so scoping trades a wrong commit for a half-finished fold. The
    // daemon cannot tell your fix from a stray `.log`, and naming it is the same
    // answer the worktree gate already gives.
    let strays: Vec<String> = paths
        .iter()
        .filter(|p| !approved.iter().any(|a| a == *p))
        .cloned()
        .collect();
    if !strays.is_empty() {
        return Ok(Written::Refused(format!(
            "the worktree holds {} that the phase did not show you — commit, remove or \
             stash {} and press continue again. Everything in this commit has to be work \
             you looked at.",
            strays.join(", "),
            if strays.len() == 1 { "it" } else { "them" }
        )));
    }

    // Blame only while the reviewers' anchors still point where they did.
    //
    // They are line numbers from the PR head at triage time. If half one applied a
    // patch above one of them and folded it in, HEAD's content has shifted and
    // blaming HEAD at the old number reads a *different* line — which does not
    // degrade, it silently picks the wrong commit and reports "folded into" it.
    // Measured: an anchor owned by one commit before half one blames another after.
    // So when HEAD has moved off the sha the anchors were validated against, the
    // anchors are stale by construction and the fold says so instead of guessing.
    let amend = if crate::git::head_sha(cwd)? == anchored_at {
        crate::git::amend_target(cwd, Some("HEAD"), merge_base, touched, my_email)?
    } else {
        crate::git::Amend::Head(
            "the accepted patches moved these lines, so the reviewers' anchors no longer \
             say which commit owns them"
                .to_string(),
        )
    };

    match crate::git::pre_commit(cwd, &paths)? {
        crate::git::PreCommit::Passed | crate::git::PreCommit::NotConfigured => {}
        crate::git::PreCommit::NotInstalled => {
            tracing::warn!(
                "`.pre-commit-config.yaml` is present but `pre-commit` is not installed; \
                 pushing code the local hooks did not see"
            );
        }
        crate::git::PreCommit::Failed(detail) => {
            return Ok(Written::Refused(format!(
                "pre-commit failed on your edits, so nothing was committed:\n{detail}"
            )))
        }
        // Unlike an accepted patch, a hook rewriting your own edit is not a
        // surprise — you wrote it, and formatting is what hooks are for. So it is
        // reported rather than refused, and the file list below includes it.
        crate::git::PreCommit::Reformatted(rewritten) => {
            tracing::info!("the hooks reformatted {}", rewritten.join(", "));
        }
    }

    // Recounted after the hooks, so the file list is what will actually land.
    let files = numstat_worktree(cwd)?;
    crate::git::fold_in(cwd, &amend)?;
    Ok(Written::Committed {
        files,
        amend: match amend {
            crate::git::Amend::Fixup(sha) => {
                format!("folded into {}", sha.chars().take(7).collect::<String>())
            }
            crate::git::Amend::Head(why) => format!("amended HEAD — {why}"),
        },
    })
}

/// What the working tree holds that `HEAD` does not: the file list and the patch.
///
/// The manual phase's window onto its own work. Untracked files are staged
/// `--intent-to-add` first, or `git diff` cannot see them at all and a whole new
/// file would be missing from both halves.
pub fn worktree_change(cwd: &Path) -> Result<(Vec<FileStat>, String)> {
    let files = numstat_worktree(cwd)?;
    let out = Command::new("git")
        .args(["diff", "HEAD"])
        .current_dir(cwd)
        .output()
        .context("running git diff")?;
    anyhow::ensure!(
        out.status.success(),
        "git diff failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok((files, String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// Every path the working tree changes against `HEAD`, with line counts.
///
/// The manual phase's equivalent of `git apply --numstat`: the file list has to be
/// derived from the tree rather than from a patch nobody wrote.
///
/// **Reads only.** The obvious implementation stages untracked files
/// `--intent-to-add` so one `git diff` can see them, and that was the implementation
/// — but an intent-to-add entry makes `git stash push` fail outright (measured on git
/// 2.34.1: `error: Entry 'x' not uptodate. Cannot merge.`, with or without `-u`), so
/// merely *looking* at the diff broke the review overlay's own stash button until the
/// batch committed. A read that breaks a write somewhere else is not a read. Untracked
/// files are counted directly instead, and nothing touches the index.
fn numstat_worktree(cwd: &Path) -> Result<Vec<FileStat>> {
    let out = Command::new("git")
        .args(["diff", "--numstat", "HEAD"])
        .current_dir(cwd)
        .output()
        .context("running git diff --numstat")?;
    anyhow::ensure!(
        out.status.success(),
        "git diff --numstat failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let mut files = aggregate(parse_numstat(&String::from_utf8_lossy(&out.stdout)));

    // A brand-new file is entirely added, so its line count is its line count.
    for path in untracked(cwd)? {
        let added = std::fs::read_to_string(cwd.join(&path))
            .map(|s| s.lines().count() as u32)
            // Binary or unreadable: still name the path, like `parse_numstat` does.
            .unwrap_or(0);
        files.push(FileStat {
            path,
            added,
            deleted: 0,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Untracked, not ignored. `git status` would answer this too, but this asks the one
/// question and cannot be confused by a rename record.
fn untracked(cwd: &Path) -> Result<Vec<String>> {
    let out = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(cwd)
        .output()
        .context("running git ls-files --others")?;
    anyhow::ensure!(
        out.status.success(),
        "git ls-files --others failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo with a 40-line file, committed, plus a helper to build a patch by
    /// editing that file in a scratch checkout and diffing — the exact path the
    /// real agent uses, so the tests exercise real git-generated diffs.
    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orchd-patch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run(&dir, &["init", "-q", "-b", "main"]);
        run(&dir, &["config", "user.email", "t@t"]);
        run(&dir, &["config", "user.name", "t"]);
        let body: String = (1..=40).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.join("f.txt"), body).unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-qm", "base"]);
        dir
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Produce a patch that replaces `line<n>` with `text`, generated by git.
    fn patch_line(dir: &Path, thread: &str, n: u32, text: &str) -> Patch {
        let f = dir.join("f.txt");
        let orig = std::fs::read_to_string(&f).unwrap();
        let edited: String = orig
            .lines()
            .map(|l| if l == format!("line{n}") { text } else { l })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&f, format!("{edited}\n")).unwrap();
        let diff = run(dir, &["diff"]);
        run(dir, &["checkout", "-q", "--", "f.txt"]);
        Patch {
            thread_id: thread.to_string(),
            diff,
        }
    }

    #[test]
    fn non_overlapping_edits_to_one_file_both_apply() {
        let d = scratch();
        let a = patch_line(&d, "A", 5, "FIVE");
        let b = patch_line(&d, "B", 30, "THIRTY");
        let got = check(&d, &[a, b]).unwrap();
        match got {
            Check::Clean(files) => {
                // One row despite two patches touching the same file.
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].path, "f.txt");
                assert_eq!((files[0].added, files[0].deleted), (2, 2));
            }
            other => panic!("expected clean, got {other:?}"),
        }
    }

    #[test]
    fn a_patch_stale_against_the_current_file_is_named() {
        let d = scratch();
        let a = patch_line(&d, "A", 5, "FIVE");
        // Change the very line A depends on, so its context no longer matches.
        let f = d.join("f.txt");
        let now = std::fs::read_to_string(&f)
            .unwrap()
            .replace("line5", "moved");
        std::fs::write(&f, now).unwrap();
        assert_eq!(check(&d, &[a]).unwrap(), Check::Stale(vec!["A".into()]));
    }

    #[test]
    fn two_threads_patching_adjacent_lines_report_overlap() {
        let d = scratch();
        // Both edit within each other's 3-line context window.
        let a = patch_line(&d, "A", 20, "TWENTY");
        let b = patch_line(&d, "B", 21, "TWENTYONE");
        // Each applies alone...
        assert!(applies(&d, &a.diff).unwrap());
        assert!(applies(&d, &b.diff).unwrap());
        // ...but together they collide, and both are named.
        assert_eq!(
            check(&d, &[a, b]).unwrap(),
            Check::Overlap(vec!["A".into(), "B".into()])
        );
    }

    #[test]
    fn apply_writes_the_batch_and_check_wrote_nothing() {
        let d = scratch();
        let a = patch_line(&d, "A", 5, "FIVE");
        let b = patch_line(&d, "B", 30, "THIRTY");
        let patches = [a, b];

        // check() is a dry run: the file is untouched afterwards.
        check(&d, &patches).unwrap();
        assert!(std::fs::read_to_string(d.join("f.txt"))
            .unwrap()
            .contains("line5"));

        apply(&d, &patches).unwrap();
        let after = std::fs::read_to_string(d.join("f.txt")).unwrap();
        assert!(after.contains("FIVE") && after.contains("THIRTY"));
        assert!(!after.contains("line5\n"));
    }

    #[test]
    fn an_empty_batch_is_vacuously_clean() {
        let d = scratch();
        assert_eq!(check(&d, &[]).unwrap(), Check::Clean(vec![]));
        apply(&d, &[]).unwrap();
    }

    // -- the local half of the batch ---------------------------------------

    /// The scratch repo plus a second commit, so blame has a PR commit to find
    /// and `base` stands in for the merge base.
    fn batch_repo() -> std::path::PathBuf {
        let d = scratch();
        run(&d, &["config", "user.email", "me@here"]);
        run(&d, &["config", "user.name", "me"]);
        run(&d, &["branch", "base"]);
        // A second commit owning the lines the patches will touch.
        let body: String = (1..=40)
            .map(|n| {
                if n == 5 || n == 30 {
                    format!("mine{n}\n")
                } else {
                    format!("line{n}\n")
                }
            })
            .collect();
        std::fs::write(d.join("f.txt"), body).unwrap();
        run(&d, &["add", "-A"]);
        run(&d, &["commit", "-qm", "the PR commit"]);
        d
    }

    fn patch_mine(dir: &Path, thread: &str, n: u32, text: &str) -> Patch {
        let f = dir.join("f.txt");
        let orig = std::fs::read_to_string(&f).unwrap();
        let edited: String = orig
            .lines()
            .map(|l| if l == format!("mine{n}") { text } else { l })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&f, format!("{edited}\n")).unwrap();
        let diff = run(dir, &["diff"]);
        run(dir, &["checkout", "-q", "--", "f.txt"]);
        Patch {
            thread_id: thread.to_string(),
            diff,
        }
    }

    fn commit_count(dir: &Path) -> usize {
        run(dir, &["log", "--format=%s"]).lines().count()
    }

    #[test]
    fn two_accepted_patches_fold_into_the_commit_that_owns_them() {
        let d = batch_repo();
        let before = commit_count(&d);
        let patches = [
            patch_mine(&d, "A", 5, "FIVE"),
            patch_mine(&d, "B", 30, "THIRTY"),
        ];
        let touched = vec![("f.txt".to_string(), 5), ("f.txt".to_string(), 30)];

        let got = write_batch(&d, "base", "me@here", &patches, &touched).unwrap();
        match got {
            Written::Committed { files, amend } => {
                assert_eq!(files.len(), 1);
                assert!(amend.starts_with("folded into"), "{amend}");
            }
            other => panic!("expected Committed, got {other:?}"),
        }
        // Folded, not stacked: no extra commit, and the content is in.
        assert_eq!(commit_count(&d), before);
        let content = std::fs::read_to_string(d.join("f.txt")).unwrap();
        assert!(content.contains("FIVE") && content.contains("THIRTY"));
        assert!(crate::git::is_clean(&d).unwrap(), "tree left dirty");
    }

    #[test]
    fn hand_written_edits_fold_into_the_commit_that_owns_the_line() {
        // The manual phase. Nothing is applied — the edits are already on disk,
        // which is exactly the state `write_batch` refuses to work in.
        let d = batch_repo();
        let before = commit_count(&d);
        let f = d.join("f.txt");
        let edited: String = std::fs::read_to_string(&f)
            .unwrap()
            .lines()
            .map(|l| {
                if l == "mine5" {
                    "by hand\n".to_string()
                } else {
                    format!("{l}\n")
                }
            })
            .collect();
        std::fs::write(&f, edited).unwrap();
        // A whole new file too, which `git diff` only sees once it is intent-to-add
        // — the case that would silently drop a file from the list.
        std::fs::write(d.join("new.txt"), "fresh\n").unwrap();

        let touched = vec![("f.txt".to_string(), 5)];
        let approved = vec!["f.txt".to_string(), "new.txt".to_string()];
        let head = crate::git::head_sha(&d).unwrap();
        match write_manual(&d, "base", "me@here", &touched, &approved, &head).unwrap() {
            Written::Committed { files, amend } => {
                // Blamed against HEAD, so it found the owning commit rather than
                // reporting "not committed yet" for the line just edited.
                assert!(amend.starts_with("folded into"), "{amend}");
                let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
                assert!(paths.contains(&"f.txt"), "{paths:?}");
                assert!(
                    paths.contains(&"new.txt"),
                    "an untracked file must count: {paths:?}"
                );
            }
            other => panic!("expected Committed, got {other:?}"),
        }
        assert_eq!(commit_count(&d), before, "folded, not stacked");
        assert!(crate::git::is_clean(&d).unwrap(), "tree left dirty");
        assert!(std::fs::read_to_string(&f).unwrap().contains("by hand"));
    }

    #[test]
    fn a_file_the_phase_never_showed_you_is_refused_not_swept_in() {
        // `fold_in` commits with `git add -A` and the clean-tree gate is stood down
        // here, so without this an unrelated scratch file a session left behind gets
        // folded into the commit and force-pushed into somebody else's PR — the very
        // sweep this design rejected when it rejected letting your edits ride along.
        let d = batch_repo();
        let before = commit_count(&d);
        std::fs::write(d.join("f.txt"), "by hand\n").unwrap();
        std::fs::write(d.join("debug.log"), "scratch from a session\n").unwrap();

        let head = crate::git::head_sha(&d).unwrap();
        let got = write_manual(
            &d,
            "base",
            "me@here",
            &[("f.txt".into(), 5)],
            // Only f.txt was on screen.
            &["f.txt".to_string()],
            &head,
        )
        .unwrap();
        match got {
            Written::Refused(why) => {
                assert!(why.contains("debug.log"), "must name the stray: {why}");
                assert!(!why.contains("f.txt"), "must not name your own edit: {why}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(commit_count(&d), before, "nothing may be committed");
        assert!(
            !crate::git::is_clean(&d).unwrap(),
            "your edits stay on disk"
        );
    }

    #[test]
    fn anchors_that_the_accepted_patches_moved_are_not_blamed() {
        // The anchor line numbers come from the PR head at triage. Once half one has
        // applied a patch above one of them and folded it in, blaming HEAD at the old
        // number reads a different line and picks the wrong commit — silently, and
        // reported as "folded into" it. So a moved HEAD means the anchors are stale
        // by construction and the fold says so instead of guessing.
        let d = batch_repo();
        std::fs::write(d.join("f.txt"), "by hand\n").unwrap();

        let stale = "0000000000000000000000000000000000000000";
        match write_manual(
            &d,
            "base",
            "me@here",
            &[("f.txt".into(), 5)],
            &["f.txt".to_string()],
            stale,
        )
        .unwrap()
        {
            Written::Committed { amend, .. } => {
                assert!(amend.starts_with("amended HEAD"), "{amend}");
                assert!(amend.contains("anchors no longer"), "{amend}");
            }
            other => panic!("expected Committed, got {other:?}"),
        }
    }

    #[test]
    fn reading_the_worktree_leaves_the_index_alone() {
        // It used to stage untracked files `--intent-to-add` so one `git diff` could
        // see them, which broke `git stash push` outright — so merely looking at the
        // phase's diff killed the review overlay's own stash button.
        let d = batch_repo();
        std::fs::write(d.join("f.txt"), "by hand\n").unwrap();
        std::fs::write(d.join("brand-new.txt"), "one\ntwo\n").unwrap();
        let before = run(&d, &["status", "--porcelain"]);

        let (files, diff) = worktree_change(&d).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"f.txt"), "{paths:?}");
        // A brand-new file still has to appear, counted, even though `git diff`
        // cannot see it.
        let new = files.iter().find(|f| f.path == "brand-new.txt").unwrap();
        assert_eq!((new.added, new.deleted), (2, 0));
        assert!(diff.contains("f.txt"), "the hunk text is still git's");

        assert_eq!(
            run(&d, &["status", "--porcelain"]),
            before,
            "reading the diff must not stage anything"
        );
        // The thing that actually broke: stash still works afterwards.
        crate::git::stash(&d).expect("stash after a phase read");
        assert!(crate::git::is_clean(&d).unwrap());
    }

    #[test]
    fn a_manual_thread_that_changed_nothing_is_not_a_failure() {
        // "I did this in another PR" is a legitimate answer; the comment was the
        // thing required, not the code.
        let d = batch_repo();
        let before = commit_count(&d);
        let got =
            write_manual(&d, "base", "me@here", &[("f.txt".to_string(), 5)], &[], "x").unwrap();
        assert_eq!(got, Written::NothingToWrite);
        assert_eq!(commit_count(&d), before);
    }

    #[test]
    fn a_stale_patch_refuses_by_name_and_writes_nothing() {
        let d = batch_repo();
        let p = patch_mine(&d, "A", 5, "FIVE");
        let before = commit_count(&d);
        // Move the line the patch depends on.
        let f = d.join("f.txt");
        let now = std::fs::read_to_string(&f)
            .unwrap()
            .replace("mine5", "moved");
        std::fs::write(&f, now).unwrap();
        run(&d, &["commit", "-qam", "moved it"]);

        match write_batch(&d, "base", "me@here", &[p], &[("f.txt".into(), 5)]).unwrap() {
            Written::Refused(why) => {
                assert!(why.contains('A'), "{why}");
                assert!(why.contains("re-triage"), "{why}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        assert_eq!(commit_count(&d), before + 1, "only the test's own commit");
        assert!(crate::git::is_clean(&d).unwrap());
    }

    #[test]
    fn an_overlapping_pair_is_named_and_nothing_is_committed() {
        let d = batch_repo();
        let before = commit_count(&d);
        // Adjacent enough to sit in each other's context window.
        let a = patch_mine(&d, "A", 5, "FIVE");
        let b = {
            let f = d.join("f.txt");
            let orig = std::fs::read_to_string(&f).unwrap();
            std::fs::write(&f, orig.replace("line6", "SIX")).unwrap();
            let diff = run(&d, &["diff"]);
            run(&d, &["checkout", "-q", "--", "f.txt"]);
            Patch {
                thread_id: "B".into(),
                diff,
            }
        };
        match write_batch(&d, "base", "me@here", &[a, b], &[("f.txt".into(), 5)]).unwrap() {
            Written::Refused(why) => {
                assert!(why.contains('A') && why.contains('B'), "{why}");
                assert!(why.contains("same lines"), "{why}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        assert_eq!(commit_count(&d), before);
        assert!(crate::git::is_clean(&d).unwrap());
    }

    #[test]
    fn a_hook_that_both_fails_and_rewrites_is_a_failure() {
        // `fail_fast` is off by default, so a formatter rewriting a file while a
        // linter errors in the same run is the ordinary shape of a bad commit. It
        // used to report as a mere reformat, because the rewrite was checked before
        // the exit status — harmless in `write_batch`, which refuses either way, and
        // not harmless in `write_manual`, which only logs a reformat and would have
        // committed and pushed code that failed lint.
        let d = batch_repo();
        std::fs::write(
            d.join(".pre-commit-config.yaml"),
            "repos:\n  - repo: local\n    hooks: []\n",
        )
        .unwrap();
        let bin = d.join("fake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_exec(
            &bin.join("pre-commit"),
            "#!/bin/sh\nprintf 'reformatted\\n' >> f.txt\nprintf 'ruff...FAILED\\nf.txt:5 unused import\\n'\nexit 1\n",
        );
        // A real repo tracks its hook config, and `write_manual` now refuses a tree
        // holding anything the phase did not show you — the fixture has to be
        // committed rather than left lying around dirty.
        run(&d, &["add", "-A"]);
        run(&d, &["commit", "-qm", "hook fixture"]);
        let before = commit_count(&d);

        let got = with_path(&bin, || crate::git::pre_commit(&d, &["f.txt".to_string()])).unwrap();
        match got {
            crate::git::PreCommit::Failed(detail) => {
                // And the hook's own words survive, which the old ordering dropped.
                assert!(detail.contains("unused import"), "{detail}");
            }
            other => panic!("a failing hook must be Failed, got {other:?}"),
        }

        // ...and the manual half refuses it rather than logging and committing.
        // The hook rewrote f.txt when it ran above, so the tree is dirty there too.
        std::fs::write(d.join("by-hand.txt"), "edited\n").unwrap();
        let head = crate::git::head_sha(&d).unwrap();
        let approved = vec!["f.txt".to_string(), "by-hand.txt".to_string()];
        let got = with_path(&bin, || {
            write_manual(
                &d,
                "base",
                "me@here",
                &[("f.txt".into(), 5)],
                &approved,
                &head,
            )
        })
        .unwrap();
        assert!(
            matches!(got, Written::Refused(ref why) if why.contains("pre-commit failed")),
            "expected a refusal, got {got:?}"
        );
        assert_eq!(commit_count(&d), before, "nothing may be committed");
    }

    #[test]
    fn a_hook_that_rewrites_files_stops_the_batch_with_nothing_committed() {
        let d = batch_repo();
        let before = commit_count(&d);
        // A hook that "formats" by rewriting the file it was handed.
        std::fs::write(
            d.join(".pre-commit-config.yaml"),
            "repos:\n  - repo: local\n    hooks: []\n",
        )
        .unwrap();
        let bin = d.join("fake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_exec(
            &bin.join("pre-commit"),
            "#!/bin/sh\nprintf 'reformatted\\n' >> f.txt\nexit 0\n",
        );
        let p = patch_mine(&d, "A", 5, "FIVE");

        let got = with_path(&bin, || {
            write_batch(&d, "base", "me@here", &[p], &[("f.txt".into(), 5)])
        })
        .unwrap();
        match got {
            Written::Refused(why) => {
                assert!(why.contains("rewrote"), "{why}");
                assert!(why.contains("f.txt"), "{why}");
                assert!(why.contains("Nothing was committed"), "{why}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        assert_eq!(commit_count(&d), before, "must not have committed");
    }

    #[test]
    fn a_failing_hook_stops_the_batch_but_a_missing_binary_only_warns() {
        let d = batch_repo();
        std::fs::write(
            d.join(".pre-commit-config.yaml"),
            "repos:\n  - repo: local\n    hooks: []\n",
        )
        .unwrap();
        let bin = d.join("fake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_exec(
            &bin.join("pre-commit"),
            "#!/bin/sh\necho 'phpstan.....Failed' \nexit 1\n",
        );

        let p = patch_mine(&d, "A", 5, "FIVE");
        let got = with_path(&bin, || {
            write_batch(&d, "base", "me@here", &[p], &[("f.txt".into(), 5)])
        })
        .unwrap();
        match got {
            Written::Refused(why) => assert!(why.contains("pre-commit failed"), "{why}"),
            other => panic!("expected Refused, got {other:?}"),
        }

        // Same repo, same config, no `pre-commit` on PATH: an environment problem
        // must not block the review.
        let empty = d.join("no-bin");
        std::fs::create_dir_all(&empty).unwrap();
        let p = patch_mine(&d, "A", 5, "FIVE");
        let got = with_path(&empty, || {
            write_batch(&d, "base", "me@here", &[p], &[("f.txt".into(), 5)])
        })
        .unwrap();
        assert!(matches!(got, Written::Committed { .. }), "{got:?}");
    }

    #[test]
    fn a_reply_only_batch_writes_nothing_and_still_succeeds() {
        let d = batch_repo();
        let before = commit_count(&d);
        assert_eq!(
            write_batch(&d, "base", "me@here", &[], &[]).unwrap(),
            Written::NothingToWrite
        );
        assert_eq!(commit_count(&d), before);
    }

    fn write_exec(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// Run `f` with `dir` as the only entry on PATH, so a fake `pre-commit` is
    /// found (or deliberately is not). Serialised by the mutex: PATH is process
    /// state and cargo runs tests in threads.
    fn with_path<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PATH");
        // git is still needed, so keep the real PATH behind the fake dir.
        let combined = match &old {
            Some(p) => format!("{}:{}", dir.display(), p.to_string_lossy()),
            None => dir.display().to_string(),
        };
        std::env::set_var("PATH", &combined);
        let out = f();
        match old {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        out
    }

    #[test]
    fn numstat_parses_counts_and_keeps_binary_paths() {
        let rows = parse_numstat("2\t1\tsrc/f.rs\n-\t-\tlogo.png\n");
        assert_eq!(
            rows,
            vec![
                FileStat {
                    path: "src/f.rs".into(),
                    added: 2,
                    deleted: 1
                },
                FileStat {
                    path: "logo.png".into(),
                    added: 0,
                    deleted: 0
                },
            ]
        );
    }
}
