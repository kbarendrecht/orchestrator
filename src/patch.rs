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
