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
//! - `../escape`, a write through a symlink, and `.git/**` are all refused
//!   (`git_apply_refuses_an_escape_a_symlink_and_the_git_dir`).
//!
//! The ladder below runs three `git apply` passes before a byte is written, so
//! the reason a batch will not apply can be named — git's own "patch does not
//! apply" cannot tell a stale patch from two that collide with each other.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    pub path: String,
    pub added: u32,
    pub deleted: u32,
}

/// What the check ladder concluded. Only `Clean` may be applied.
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    /// Every patch applies, alone and together.
    ///
    /// Carries both answers the one numstat pass can give, because they were being
    /// asked twice: `files` is the combined list the card renders, and `touched` is
    /// every path the batch writes, rename sources included, for the two checks the
    /// caller makes against the tree after the apply.
    Clean {
        files: Vec<FileStat>,
        touched: Vec<String>,
    },
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

/// The paths a diff touches, with line counts, and every path it writes.
///
/// One `git apply --numstat` for both, because they used to be two: `check` asked
/// for the file list and `write_batch` then asked the same question again over a
/// stream it rebuilt from scratch, which is a second child process and a second
/// serialization of the whole batch through a pipe.
///
/// The two answers differ over renames. A numstat row names one by its new path,
/// which is right for the card and wrong for the two questions asked after the
/// apply — is anything dirty that the patch did not touch, and what has to be put
/// back if a later step refuses — since a refused rename would otherwise leave the
/// old file deleted and the new one removed.
///
/// `--numstat` writes nothing, so this is safe before the user has confirmed
/// anything.
fn numstat(cwd: &Path, diff: &str) -> Result<(Vec<FileStat>, Vec<String>)> {
    let rows = numstat_rows(cwd, diff)?;
    let mut touched = Vec::new();
    for r in &rows {
        if let Some(from) = &r.from {
            touched.push(from.clone());
        }
        touched.push(r.path.clone());
    }
    touched.sort();
    touched.dedup();
    let files = aggregate(
        rows.into_iter()
            .map(|r| FileStat {
                path: r.path,
                added: r.added,
                deleted: r.deleted,
            })
            .collect(),
    );
    Ok((files, touched))
}

fn numstat_rows(cwd: &Path, diff: &str) -> Result<Vec<NumstatRow>> {
    let (ok, stdout, err) = git_apply(cwd, &["--numstat", "-z"], diff)?;
    // The caller only reaches here once the diff has passed --check, so a
    // failure now is a real fault, not a stale patch.
    anyhow::ensure!(ok, "git apply --numstat failed: {err}");
    Ok(parse_numstat_z(&stdout))
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

/// One `--numstat -z` record. `from` is set for a rename: the path the change
/// deleted to make `path`.
#[derive(Debug, PartialEq, Eq)]
pub struct NumstatRow {
    pub path: String,
    pub from: Option<String>,
    pub added: u32,
    pub deleted: u32,
    /// git printed `-` for both counts. Kept as a flag because the counts are
    /// surfaced as zero, and a binary file is otherwise indistinguishable from a
    /// change that added and removed nothing — which the diff pane has to tell
    /// apart, since it collapses one and renders the other.
    pub binary: bool,
}

/// The one numstat parser, for `git apply --numstat -z` and `git diff --numstat -z`
/// alike — it used to be written twice, once per flag shape.
///
/// `-z` so paths come through raw, matching `git::status`, and so a rename is
/// visible at all: the plain form prints only the new path. The record shapes
/// differ: an ordinary entry is `added\tdeleted\tpath\0`, a rename is
/// `added\tdeleted\t\0old\0new\0` — the path field is empty and the two paths
/// follow as their own records. A binary file prints `-\t-`, surfaced as zero
/// counts rather than dropped: it is still being written.
pub fn parse_numstat_z(raw: &str) -> Vec<NumstatRow> {
    let mut fields = raw.split('\0');
    let mut rows = Vec::new();
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let mut cols = field.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (cols.next(), cols.next(), cols.next())
        else {
            continue;
        };
        let n = |s: &str| s.parse().unwrap_or(0);
        let (path, from) = if path.is_empty() {
            let old = fields.next().map(str::to_string);
            match fields.next() {
                Some(new) => (new.to_string(), old),
                None => continue,
            }
        } else {
            (path.to_string(), None)
        };
        rows.push(NumstatRow {
            path,
            from,
            added: n(added),
            deleted: n(deleted),
            binary: added == "-" || deleted == "-",
        });
    }
    rows
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
        return Ok(Check::Clean {
            files: Vec::new(),
            touched: Vec::new(),
        });
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

    // Pass 3 — clean. The file list and the touched paths are the combined numstat.
    let (files, touched) = numstat(cwd, &all)?;
    Ok(Check::Clean { files, touched })
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
    let amend = crate::review_commit::amend_target(cwd, None, merge_base, touched, my_email)?;

    // What the tree already held. The gate upstream requires a clean tree, so this
    // is normally empty — but `write_batch` is `pub` and states no such
    // precondition of its own, and the two checks below both mean "beyond what was
    // here when we started". Taking the snapshot rather than trusting the gate is
    // what keeps the revert from reaching somebody's uncommitted work if a caller
    // ever arrives without one.
    let before = dirty_paths(cwd)?;

    // 2. The ladder.
    let (files, touched_paths) = match check(cwd, patches)? {
        Check::Clean { files, touched } => (files, touched),
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

    // 4/5/6, and every way out of them but a commit puts the tree back.
    let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    let why = match land(cwd, &paths, &before, &touched_paths, &amend) {
        // The one way out that keeps what it wrote.
        Ok(None) => {
            return Ok(Written::Committed {
                files,
                amend: amend.describe(),
            })
        }
        other => other,
    };

    /* **Put the tree back.** The patch is on disk by now, and returning `Refused`
       used to leave it there — so the report said nothing was committed while the
       worktree held changes nobody had asked for, and the next attempt was refused
       by `triage::gate` as `Dirty`, naming files the user never touched. It used to
       run only on the hooks' refusal, so an `Err` from the hooks or the fold left
       the same mess by the other door.

       Everything that has gone dirty *since we started*, not only the patch's own
       paths: whatever is dirty beyond `before` is the patch or what the hooks did
       to the tree on its account. Scoping to the patch's paths is what left a
       rename half-reverted, since numstat names it by the new path alone; scoping
       to the whole tree, which this did at first, would revert a caller's own
       uncommitted work on the strength of a gate taken in another module.

       This is the arm that applied its own patch. `write_manual` reaches
       `hooks_refusal` with `own_edits: true` and must **not** do this: those
       changes are the user's, typed by hand, and reverting them would destroy
       exactly what the manual phase exists to collect. */
    const UNREVERTED: &str =
        "the applied patch could not be reverted, so it is still in the working tree";
    let restored = added_since(cwd, &before).and_then(|ours| crate::git::restore_paths(cwd, &ours));
    match (why, restored) {
        (Ok(Some(why)), Ok(())) => Ok(Written::Refused(why)),
        // Better a refusal that admits the mess than one that hides it.
        (Ok(Some(why)), Err(e)) => Ok(Written::Refused(format!("{why}\n\n({UNREVERTED}: {e:#})"))),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(r)) => Err(e.context(format!("and {UNREVERTED}: {r:#}"))),
        // `Ok(None)` returned above; the compiler cannot see that through the match.
        (Ok(None), _) => Ok(Written::NothingToWrite),
    }
}

/// What the working tree holds beyond `before` — the paths this pass is
/// answerable for.
fn added_since(cwd: &Path, before: &[String]) -> Result<Vec<String>> {
    Ok(dirty_paths(cwd)?
        .into_iter()
        .filter(|p| !before.contains(p))
        .collect())
}

/// Steps 4 to 6 of [`write_batch`], with the patch already in the tree: the hooks,
/// the check that they touched nothing else, and the fold. `Some` is a refusal to
/// report; `None` is a commit.
///
/// A function of its own so that the caller has one place to put the tree back
/// from, whichever of the three said no.
fn land(
    cwd: &Path,
    paths: &[String],
    before: &[String],
    touched: &[String],
    amend: &crate::review_commit::Amend,
) -> Result<Option<String>> {
    if let Some(why) = hooks_refusal(cwd, paths, false)? {
        return Ok(Some(why));
    }
    if let Some(why) = hooks_wrote_elsewhere(cwd, before, touched, "the accepted patch")? {
        return Ok(Some(why));
    }
    crate::git::fold_in(cwd, amend)?;
    Ok(None)
}

/// Did the hooks write outside `allowed`? The refusal to report, or `None`.
///
/// The hooks are judged on a path list, but `fold_in` commits with `git add -A`, so
/// a hook that regenerated a lockfile or an index passed as `Passed` and was swept
/// into the fixup and the push unseen — the "reformats beyond the approved diff"
/// this design refuses, arriving by a side door.
///
/// Called by both writers, and it has to be, which is the whole reason it is a
/// function. `write_manual`'s stray check looks like this one and is not: it runs
/// *before* the hooks, off the tree as it stood at entry, so a file a hook creates
/// during the manual phase is not a stray (that check is already over), is not
/// `Reformatted` (`hash_files` only hashes what it was handed), and on
/// `own_edits: true` a rewrite is allowed anyway. It went into the commit and the
/// force-push — on the path where the clean-tree gate is stood down and the work
/// being committed is a person's own.
///
/// `before` is the tree as this pass found it, so a file that was already dirty is
/// not blamed on the hooks.
fn hooks_wrote_elsewhere(
    cwd: &Path,
    before: &[String],
    allowed: &[String],
    what: &str,
) -> Result<Option<String>> {
    let extra: Vec<String> = dirty_paths(cwd)?
        .into_iter()
        .filter(|p| !allowed.contains(p) && !before.contains(p))
        .collect();
    if extra.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "the hooks wrote to {}, which {what} does not touch — what would land is no \
         longer what you approved. Nothing was committed.",
        extra.join(", ")
    )))
}

/// Run the repo's pre-commit hooks over `paths`: the refusal to report, or `None`
/// when what is in the tree may land.
///
/// `own_edits` says whose work the hooks ran on. Unlike an accepted patch, a hook
/// rewriting your own edit is not a surprise — you wrote it, and formatting is what
/// hooks are for — so a rewrite is reported rather than refused, and the caller's
/// file list includes it.
fn hooks_refusal(cwd: &Path, paths: &[String], own_edits: bool) -> Result<Option<String>> {
    Ok(match crate::git::pre_commit(cwd, paths)? {
        crate::git::PreCommit::Passed | crate::git::PreCommit::NotConfigured => None,
        crate::git::PreCommit::NotInstalled => {
            tracing::warn!(
                "`.pre-commit-config.yaml` is present but `pre-commit` is not installed; \
                 pushing code the local hooks did not see"
            );
            None
        }
        crate::git::PreCommit::Failed(detail) if own_edits => Some(format!(
            "pre-commit failed on your edits, so nothing was committed:\n{detail}"
        )),
        crate::git::PreCommit::Failed(detail) => {
            Some(format!("pre-commit failed, so nothing was committed:\n{detail}"))
        }
        crate::git::PreCommit::Reformatted(rewritten) if own_edits => {
            tracing::info!("the hooks reformatted {}", rewritten.join(", "));
            None
        }
        crate::git::PreCommit::Reformatted(paths) => Some(format!(
            "the hooks rewrote {} — what would land is no longer what you approved. \
             Nothing was committed.",
            paths.join(", ")
        )),
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
    // The same source the phase's own file list came from, so the two lists are in
    // one encoding and a `café.md` cannot read as a stray.
    let paths = dirty_paths(cwd)?;
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
        // Capped: an untracked directory is now one stray per file, and a hundred of
        // them in one sentence is not a message anybody reads.
        let shown = if strays.len() > 10 {
            format!(
                "{}, and {} more",
                strays[..10].join(", "),
                strays.len() - 10
            )
        } else {
            strays.join(", ")
        };
        return Ok(Written::Refused(format!(
            "the worktree holds {} that the phase did not show you. Press `re-read the \
             tree` and look at {}, then continue — everything in this commit has to be \
             work you looked at. If {} not yours, commit, remove or stash {}.",
            shown,
            if strays.len() == 1 { "it" } else { "them" },
            if strays.len() == 1 {
                "it is"
            } else {
                "they are"
            },
            if strays.len() == 1 { "it" } else { "them" }
        )));
    }

    // Blame only the anchors the accepted patches did not disturb.
    //
    // An anchor is a line number from the PR head at triage. If half one patched that
    // file and folded it in, HEAD's content has shifted and blaming HEAD at the old
    // number reads a *different* line — which does not degrade, it silently picks the
    // wrong commit and reports "folded into" it. Measured: an anchor owned by one
    // commit before half one blames another after.
    //
    // The first attempt compared HEAD to the sha the anchors were validated against,
    // and that was dead in both directions — the two gates in `post::run_inner` mean
    // the comparison is decided entirely by whether half one committed *anything*, so
    // it said "stale" exactly when it could not tell and "fresh" exactly when
    // staleness was impossible. Staleness is per file: only the paths half one
    // rewrote are suspect, and in the common mixed batch the patches and the manual
    // edits are in different files.
    let disturbed = paths_changed_between(cwd, anchored_at)?;
    let (usable, moved): (Vec<_>, Vec<_>) = touched.iter().cloned().partition(|(path, _)| {
        disturbed
            .as_ref()
            .map(|d| !d.contains(path))
            // Could not tell, so nothing is trusted.
            .unwrap_or(false)
    });
    let amend = if usable.is_empty() && !moved.is_empty() {
        let mut names: Vec<String> = moved.into_iter().map(|(p, _)| p).collect();
        names.sort();
        names.dedup();
        // Through the shared constructor, never `Amend::Head` directly: it is the
        // only thing that checks whether HEAD is ours to rewrite, and building one
        // here walked straight past it.
        crate::review_commit::head_or_on_top(
            cwd,
            crate::git::effective_email(cwd).as_deref(),
            format!(
                "the accepted patches moved the lines in {}, so the reviewers' anchors no \
                 longer say which commit owns them",
                names.join(", ")
            ),
        )
    } else {
        crate::review_commit::amend_target(cwd, Some("HEAD"), merge_base, &usable, my_email)?
    };

    if let Some(why) = hooks_refusal(cwd, &paths, true)? {
        return Ok(Written::Refused(why));
    }
    // The strays check above ran before the hooks did, against the tree as it stood
    // at entry, so it cannot see a file a hook *creates* — and nothing else here
    // could either: a created file is not a rewrite of an approved one, so
    // `hooks_refusal` passes it, and `own_edits` allows a rewrite in any case. It
    // went into the fixup and the force-push unseen. `before` is empty because this
    // path's whole input is the dirty tree, and `paths` is that same list.
    if let Some(why) = hooks_wrote_elsewhere(cwd, &[], &paths, "the work the phase showed you")? {
        return Ok(Written::Refused(why));
    }

    // Recounted after the hooks, so the file list is what will actually land.
    let files = numstat_worktree(cwd)?;
    crate::git::fold_in(cwd, &amend)?;
    Ok(Written::Committed {
        files,
        amend: amend.describe(),
    })
}

/// What the working tree holds that `HEAD` does not: the file list and the patch.
///
/// The manual phase's window onto its own work. Untracked files are staged
/// `--intent-to-add` first, or `git diff` cannot see them at all and a whole new
/// file would be missing from both halves.
pub fn worktree_change(cwd: &Path) -> Result<(Vec<FileStat>, String)> {
    let files = numstat_worktree(cwd)?;
    let mut diff = crate::git::git(cwd, &["diff", "HEAD"])?;

    // `git diff HEAD` cannot see an untracked file at all, and the only way to make
    // it — `--intent-to-add` — is the index write that broke `git stash`. So each new
    // file is diffed against nothing, separately. Without this the phase listed a
    // whole new file and showed none of it, under a screen that says everything in
    // the commit is work you looked at.
    let tracked: std::collections::HashSet<String> = numstat_counts(cwd)?.into_keys().collect();
    for f in files.iter().filter(|f| !tracked.contains(&f.path)) {
        diff.push_str(&new_file_diff(cwd, &f.path)?);
    }
    Ok((files, diff))
}

/// A whole-file hunk for something git is not tracking yet.
fn new_file_diff(cwd: &Path, path: &str) -> Result<String> {
    let out = Command::new("git")
        // The rest of the display is raw, so the header paths must be too.
        .args([
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-index",
            "--",
            "/dev/null",
            path,
        ])
        .current_dir(cwd)
        .output()
        .context("running git diff --no-index")?;
    // **1 means "they differ"**, which for a new file is always — so the usual
    // `ensure!(success)` would treat every success as a failure. Only 2 and above is
    // a real error.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // **1 means "they differ"**, which for a new file is always — so the usual
    // `ensure!(success)` would treat every success as a failure. But 1 is also what a
    // real error exits with, and accepting it unconditionally is how an untracked
    // *directory* came back as an empty diff and got committed unseen. Empty output
    // with something on stderr is a failure whatever the code says.
    if stdout.is_empty() && !stderr.trim().is_empty() {
        anyhow::bail!("git diff --no-index failed for {path}: {}", stderr.trim());
    }
    match out.status.code() {
        Some(0) | Some(1) => Ok(stdout.into_owned()),
        _ => anyhow::bail!("git diff --no-index failed for {path}: {}", stderr.trim()),
    }
}

/// Every path the working tree changes against `HEAD`, with line counts.
///
/// The manual phase's equivalent of `git apply --numstat`: the file list has to be
/// derived from the tree rather than from a patch nobody wrote.
///
/// **`git status` decides the paths**, and nothing else does. That is the whole
/// point of the shape. `git diff --numstat` and `git ls-files` apply `core.quotePath`
/// and collapse a rename into `old => new`, while `git::status` uses `-z` and emits
/// raw bytes — so a list built from the former and checked against the latter
/// disagrees about any non-ASCII filename and about every rename. Measured:
/// `status -z` says `café.md` where `ls-files` says `"caf\303\251.md"`. That
/// mismatch made the manual phase refuse a person's own work as a stray and left the
/// batch permanently unfinishable. So `status` names the files and the other commands
/// only supply numbers for names it already chose.
///
/// **Reads only.** The obvious implementation stages untracked files
/// `--intent-to-add` so one `git diff` can see them, and that was the implementation
/// — but an intent-to-add entry makes `git stash push` fail outright (measured on git
/// 2.34.1: `error: Entry 'x' not uptodate. Cannot merge.`), so merely *looking* at the
/// diff broke the review overlay's own stash button. A read that breaks a write
/// somewhere else is not a read.
fn numstat_worktree(cwd: &Path) -> Result<Vec<FileStat>> {
    let counts = numstat_counts(cwd)?;
    let mut files: Vec<FileStat> = dirty_paths(cwd)?
        .into_iter()
        .map(|path| match counts.get(&path) {
            Some((added, deleted)) => FileStat {
                path,
                added: *added,
                deleted: *deleted,
            },
            // Untracked, so `git diff` never mentioned it: a brand-new file is
            // entirely added, and its line count is its line count.
            None => {
                let added = std::fs::read_to_string(cwd.join(&path))
                    .map(|s| s.lines().count() as u32)
                    // Binary or unreadable: still name it, as `parse_numstat_z` does.
                    .unwrap_or(0);
                FileStat {
                    path,
                    added,
                    deleted: 0,
                }
            }
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// The paths rewritten between `since` and `HEAD`.
///
/// Empty when they are the same commit, which is the case where nothing could have
/// moved. `-z` for the same raw encoding everything else here uses.
/// `None` when git could not answer — an unknown sha, say. Treating that as "nothing
/// moved" would blame lines that may well have shifted, so the caller reads `None` as
/// "assume all of them did" and degrades, which is the safe direction.
fn paths_changed_between(
    cwd: &Path,
    since: &str,
) -> Result<Option<std::collections::HashSet<String>>> {
    let out = match crate::git::git(cwd, &["diff", "--name-only", "-z", since, "HEAD"]) {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!("could not diff {since}..HEAD, so no anchor is trusted: {e:#}");
            return Ok(None);
        }
    };
    Ok(Some(
        out.split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
    ))
}

/// Everything `git status` calls changed, staged or not, tracked or not.
///
/// The one source of path strings for the manual phase. A rename appears once, as its
/// new path, because that is what `--porcelain=v2` reports.
pub fn dirty_paths(cwd: &Path) -> Result<Vec<String>> {
    let set = crate::git::status(cwd, None, crate::git::Untracked::Each)?;
    Ok(set
        .staged
        .iter()
        .chain(set.unstaged.iter())
        .chain(set.untracked.iter())
        .map(|f| f.path.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect())
}

/// `path -> (added, deleted)` for tracked changes, from `--numstat -z`.
///
/// `-z` so the keys are raw bytes matching `git::status`. Its record shape differs
/// from the plain form: an ordinary entry is `added\tdeleted\tpath\0`, but a rename
/// is `added\tdeleted\t\0old\0new\0` — the path field is empty and the two paths
/// follow as their own records.
fn numstat_counts(cwd: &Path) -> Result<std::collections::HashMap<String, (u32, u32)>> {
    let raw = crate::git::git(cwd, &["diff", "--numstat", "-z", "HEAD"])?;
    // Keyed on a rename's new path, so it matches what `git status` reported.
    Ok(parse_numstat_z(&raw)
        .into_iter()
        .map(|r| (r.path, (r.added, r.deleted)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo with a 40-line file, committed, plus a helper to build a patch by
    /// editing that file in a scratch checkout and diffing — the exact path the
    /// real agent uses, so the tests exercise real git-generated diffs.
    fn scratch() -> std::path::PathBuf {
        let dir = crate::testutil::scratch("patch");
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
            Check::Clean { files, touched } => {
                // One row despite two patches touching the same file.
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].path, "f.txt");
                assert_eq!((files[0].added, files[0].deleted), (2, 2));
                // And one path, from the same numstat pass.
                assert_eq!(touched, vec!["f.txt".to_string()]);
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

    /// A diff creating `path` from nothing, as `git diff` would print it.
    fn new_file_diff(path: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+x\n"
        )
    }

    /// The three paths the module doc says git refuses, checked against git
    /// rather than believed: a patch is agent output, and these are the shapes
    /// that would let one write outside the worktree or into its metadata.
    #[test]
    fn git_apply_refuses_an_escape_a_symlink_and_the_git_dir() {
        let dir = scratch();
        // A directory that is a symlink out of the tree, committed as the repo
        // would carry it.
        std::os::unix::fs::symlink(std::env::temp_dir(), dir.join("out")).unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-qm", "link"]);

        for path in ["../escape.txt", ".git/hooks/pre-commit", "out/through-the-link.txt"] {
            let (ok, _, err) = git_apply(&dir, &["--check"], &new_file_diff(path)).unwrap();
            assert!(!ok, "{path} was accepted");
            assert!(!err.is_empty(), "{path}: refused without a reason");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_batch_is_vacuously_clean() {
        let d = scratch();
        assert_eq!(
            check(&d, &[]).unwrap(),
            Check::Clean {
                files: vec![],
                touched: vec![]
            }
        );
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
    fn a_non_ascii_name_and_a_rename_are_not_strays() {
        // The file list and the stray check used to come from git commands with
        // different path encodings — `ls-files`/`--numstat` quote and octal-escape,
        // `status -z` does not — so a person's own `café.md` read as a stray and the
        // phase could never be finished. Both sides now come from `status`.
        let d = batch_repo();
        std::fs::write(d.join("café.md"), "één\ntwee\n").unwrap();
        run(&d, &["mv", "f.txt", "hernoemd.txt"]);

        let (files, _) = worktree_change(&d).unwrap();
        let listed: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(listed.contains(&"café.md"), "raw, not quoted: {listed:?}");
        assert!(
            listed.contains(&"hernoemd.txt"),
            "a rename is its new path, not an arrow: {listed:?}"
        );
        assert!(
            !listed
                .iter()
                .any(|p| p.contains("=>") || p.contains("\\303")),
            "no arrows and no octal escapes: {listed:?}"
        );

        // And what the phase showed is accepted by the gate that consumes it.
        let approved: Vec<String> = listed.iter().map(|s| s.to_string()).collect();
        let head = crate::git::head_sha(&d).unwrap();
        let got = write_manual(&d, "base", "me@here", &[], &approved, &head).unwrap();
        assert!(
            !matches!(got, Written::Refused(_)),
            "own work must not be a stray: {got:?}"
        );
    }

    #[test]
    fn a_wall_of_strays_is_capped() {
        // An untracked directory is one stray per file now, and a hundred of them in
        // one sentence is not a message anybody reads.
        let d = batch_repo();
        for n in 0..25 {
            std::fs::write(d.join(format!("stray{n:02}.txt")), "x\n").unwrap();
        }
        let head = crate::git::head_sha(&d).unwrap();
        match write_manual(&d, "base", "me@here", &[], &[], &head).unwrap() {
            Written::Refused(why) => {
                assert!(why.contains("and 15 more"), "{why}");
                assert!(why.contains("stray00.txt"), "{why}");
                assert!(!why.contains("stray20.txt"), "capped: {why}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_new_directory_is_listed_and_shown_file_by_file() {
        // `git status --untracked-files=normal` collapses an untracked directory to
        // one `newdir/` entry — which cannot be counted, cannot be diffed, and could
        // not be refused one file at a time. The phase listed the directory, showed
        // none of it, and `git add -A` committed everything inside, under a screen
        // that says everything in the commit is work you looked at.
        let d = batch_repo();
        std::fs::create_dir_all(d.join("newdir/sub")).unwrap();
        std::fs::write(d.join("newdir/a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(d.join("newdir/sub/b.txt"), "three\n").unwrap();

        let (files, diff) = worktree_change(&d).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&"newdir/a.txt"),
            "per file, not per dir: {paths:?}"
        );
        assert!(paths.contains(&"newdir/sub/b.txt"), "{paths:?}");
        assert!(
            !paths.contains(&"newdir/"),
            "the collapsed entry must be gone: {paths:?}"
        );
        assert_eq!(
            files
                .iter()
                .find(|f| f.path == "newdir/a.txt")
                .unwrap()
                .added,
            2
        );
        assert!(diff.contains("+one"), "and its contents are shown:\n{diff}");
        assert!(diff.contains("+three"), "including nested ones:\n{diff}");

        // ...and one file out of the directory can be refused on its own.
        let head = crate::git::head_sha(&d).unwrap();
        let got = write_manual(
            &d,
            "base",
            "me@here",
            &[],
            &["newdir/a.txt".to_string()],
            &head,
        )
        .unwrap();
        match got {
            Written::Refused(why) => assert!(why.contains("newdir/sub/b.txt"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_brand_new_file_is_shown_and_not_just_listed() {
        // The list named it and the diff never showed it, under a screen that says
        // everything in the commit is work you looked at. `git diff HEAD` cannot see
        // an untracked file, and the only way to make it — `--intent-to-add` — is the
        // index write that broke `git stash`.
        let d = batch_repo();
        std::fs::write(d.join("new_helper.rs"), "fn helper() {}\nfn other() {}\n").unwrap();
        std::fs::write(d.join("f.txt"), "edited\n").unwrap();

        let before = run(&d, &["status", "--porcelain"]);
        let (files, diff) = worktree_change(&d).unwrap();

        let new = files.iter().find(|f| f.path == "new_helper.rs").unwrap();
        assert_eq!((new.added, new.deleted), (2, 0));
        assert!(
            diff.contains("new_helper.rs"),
            "the new file must be in the diff"
        );
        assert!(diff.contains("+fn helper()"), "with its contents:\n{diff}");
        assert!(diff.contains("f.txt"), "and the tracked change too");
        assert_eq!(
            run(&d, &["status", "--porcelain"]),
            before,
            "and still nothing staged"
        );
    }

    #[test]
    fn only_the_files_the_patches_touched_lose_their_anchors() {
        // The first attempt compared HEAD to the anchors' sha, which the gates above
        // it had already decided — so it said "stale" exactly when it could not tell.
        // Staleness is per file: in the common mixed batch the accepted patch and the
        // manual edit are in different files, and that anchor is still good.
        let d = batch_repo();
        // A second file that exists at triage time, so its anchor is real.
        std::fs::write(d.join("other.txt"), "line1\nline2\n").unwrap();
        run(&d, &["add", "-A"]);
        run(&d, &["commit", "-qm", "another file"]);
        let at_triage = crate::git::head_sha(&d).unwrap();

        // Half one: a patch lands in f.txt and is committed.
        std::fs::write(d.join("f.txt"), "half one wrote this\n").unwrap();
        run(&d, &["add", "-A"]);
        run(&d, &["commit", "-qm", "half one"]);

        // Half two: the human edits the *other* file, whose anchor never moved.
        std::fs::write(d.join("other.txt"), "line1\nby hand\n").unwrap();
        match write_manual(
            &d,
            "base",
            "me@here",
            &[("other.txt".into(), 2)],
            &["other.txt".to_string()],
            &at_triage,
        )
        .unwrap()
        {
            Written::Committed { amend, .. } => assert!(
                amend.starts_with("folded into"),
                "an untouched file keeps its anchor: {amend}"
            ),
            other => panic!("expected Committed, got {other:?}"),
        }
    }

    #[test]
    fn anchors_in_a_file_the_patches_rewrote_are_not_blamed() {
        // The anchor line numbers come from the PR head at triage. Once half one has
        // applied a patch above one of them and folded it in, blaming HEAD at the old
        // number reads a different line and picks the wrong commit — silently, and
        // reported as "folded into" it. So the fold says so instead of guessing.
        let d = batch_repo();
        let at_triage = crate::git::head_sha(&d).unwrap();

        // Half one rewrites the very file the anchor is in.
        std::fs::write(d.join("f.txt"), "half one rewrote this whole file\n").unwrap();
        run(&d, &["add", "-A"]);
        run(&d, &["commit", "-qm", "half one"]);
        // Half two edits it by hand.
        std::fs::write(d.join("f.txt"), "half one rewrote this\nand then I did\n").unwrap();

        match write_manual(
            &d,
            "base",
            "me@here",
            &[("f.txt".into(), 5)],
            &["f.txt".to_string()],
            &at_triage,
        )
        .unwrap()
        {
            Written::Committed { amend, .. } => {
                assert!(amend.starts_with("amended HEAD"), "{amend}");
                assert!(amend.contains("f.txt"), "it must name the file: {amend}");
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
        // Committed, so the tree is clean the way the gate guarantees it, and the
        // restore below can be asserted on the whole tree.
        run(&d, &["add", "-A"]);
        run(&d, &["commit", "-qm", "hook fixture"]);
        let before = commit_count(&d);
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
        assert!(crate::git::is_clean(&d).unwrap(), "the patch and the rewrite must both be gone");
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
        // Committed, so the tree starts clean the way the gate guarantees.
        run(&d, &["add", "-A"]);
        run(&d, &["commit", "-qm", "hook fixture"]);

        let p = patch_mine(&d, "A", 5, "FIVE");
        let got = with_path(&bin, || {
            write_batch(&d, "base", "me@here", &[p], &[("f.txt".into(), 5)])
        })
        .unwrap();
        match got {
            Written::Refused(why) => assert!(why.contains("pre-commit failed"), "{why}"),
            other => panic!("expected Refused, got {other:?}"),
        }
        /* **And the patch is out of the tree again.** The refusal happens *after*
           it is on disk, and this used to leave it there: the report said nothing
           was committed while the worktree held changes nobody asked for, and the
           next attempt was refused as `Dirty` naming files the user never touched.
           The old assertion only counted commits, which stayed right throughout. */
        let after = std::fs::read_to_string(d.join("f.txt")).unwrap();
        assert!(
            after.contains("mine5") && !after.contains("FIVE"),
            "the refused patch was left in f.txt:\n{after}"
        );
        assert!(crate::git::is_clean(&d).unwrap(), "the tree must be back to HEAD");

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

    /// Run `f` with `dir` in front of a PATH that cannot supply a real
    /// `pre-commit`, so the fake one in `dir` is found — or, for the arm that wants
    /// it missing, nothing is. Serialised by the mutex: PATH is process state and
    /// cargo runs tests in threads.
    ///
    /// **Two things this has to hold at once**, and each one broke it in turn.
    /// Keeping the whole real PATH behind `dir` makes the "not installed" arm
    /// assert something about the *machine*: a mise shim for `pre-commit` with no
    /// version set was enough to turn a lookup that must find nothing into a hook
    /// that ran and failed. Replacing PATH outright breaks every *other* test in
    /// the run, because PATH is process-wide and this mutex only serialises its own
    /// callers: they lose git for as long as `f` takes. So the real entries stay,
    /// minus the ones that actually hold a `pre-commit`.
    fn with_path<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PATH");
        let real = old.clone().unwrap_or_default();
        let kept = std::env::split_paths(&real).filter(|p| !p.join("pre-commit").exists());
        let combined = std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(kept))
            .expect("rebuilding PATH");
        std::env::set_var("PATH", &combined);
        let out = f();
        match old {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        out
    }

    #[test]
    fn numstat_parses_counts_keeps_binary_paths_and_sees_both_halves_of_a_rename() {
        let rows = parse_numstat_z(concat!(
            "2\t1\tsrc/f.rs\0",
            "-\t-\tlogo.png\0",
            "0\t0\t\0old.rs\0new.rs\0"
        ));
        assert_eq!(
            rows,
            vec![
                NumstatRow {
                    path: "src/f.rs".into(),
                    from: None,
                    added: 2,
                    deleted: 1,
                    binary: false
                },
                // `-\t-` counts as zero lines, and says so: the flag is what tells
                // a binary file from a change that added and removed nothing.
                NumstatRow {
                    path: "logo.png".into(),
                    from: None,
                    added: 0,
                    deleted: 0,
                    binary: true
                },
                NumstatRow {
                    path: "new.rs".into(),
                    from: Some("old.rs".into()),
                    added: 0,
                    deleted: 0,
                    binary: false
                },
            ]
        );
    }

    /// A hook that writes *beside* the patch — a regenerated lockfile, an index —
    /// used to pass as `Passed`, because the rewrite check hashed the patch's own
    /// paths while the fold committed the whole tree. Refused now, by name, and the
    /// tree is put back.
    #[test]
    fn a_hook_that_writes_outside_the_patch_is_refused_and_the_tree_put_back() {
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
            "#!/bin/sh\nprintf 'regenerated\\n' > lock.txt\nexit 0\n",
        );
        // Committed, as a real repo's hook config is, so the tree is clean the way
        // the gate guarantees before a batch — which is what makes "everything
        // dirty afterwards is the patch's doing" true.
        run(&d, &["add", "-A"]);
        run(&d, &["commit", "-qm", "hook fixture"]);
        let before = commit_count(&d);

        let p = patch_mine(&d, "A", 5, "FIVE");
        let got = with_path(&bin, || {
            write_batch(&d, "base", "me@here", &[p], &[("f.txt".into(), 5)])
        })
        .unwrap();
        match got {
            Written::Refused(why) => {
                assert!(why.contains("lock.txt"), "{why}");
                assert!(why.contains("Nothing was committed"), "{why}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        assert_eq!(commit_count(&d), before);
        assert!(crate::git::is_clean(&d).unwrap(), "the hook's file and the patch must both be gone");
    }

    /// The manual phase's half of the same hole, and the one with the worse blast
    /// radius: the clean-tree gate is stood down here and the work being committed
    /// is a person's own.
    ///
    /// The strays check runs *before* the hooks, off the tree as it stood at entry,
    /// so a file a hook creates during the phase is invisible to it. It is not a
    /// rewrite either, so `hooks_refusal` passes it, and `own_edits` allows a
    /// rewrite regardless. It went into the fixup and the force-push unseen.
    #[test]
    fn a_hook_that_writes_during_the_manual_phase_is_refused() {
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
            "#!/bin/sh\nprintf 'regenerated\\n' > lock.txt\nexit 0\n",
        );
        run(&d, &["add", "-A"]);
        run(&d, &["commit", "-qm", "hook fixture"]);
        let before = commit_count(&d);

        // The human's own edit, which the phase showed them and approved.
        let f = d.join("f.txt");
        let edited = std::fs::read_to_string(&f).unwrap().replace("mine5", "by hand");
        std::fs::write(&f, edited).unwrap();

        let touched = vec![("f.txt".to_string(), 5)];
        let approved = vec!["f.txt".to_string()];
        let head = crate::git::head_sha(&d).unwrap();
        let got = with_path(&bin, || {
            write_manual(&d, "base", "me@here", &touched, &approved, &head)
        })
        .unwrap();
        match got {
            Written::Refused(why) => {
                assert!(why.contains("lock.txt"), "{why}");
                assert!(why.contains("Nothing was committed"), "{why}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        assert_eq!(commit_count(&d), before, "nothing may land");
    }

    /// The revert puts back what this pass dirtied, not whatever the tree happens to
    /// hold. `write_batch` is `pub` and states no precondition; reverting everything
    /// dirty meant trusting a clean-tree gate taken in another module, and a caller
    /// arriving without one would have had its uncommitted work reset to `HEAD`.
    #[test]
    fn a_refusal_reverts_only_what_this_pass_wrote() {
        let d = batch_repo();
        std::fs::write(
            d.join(".pre-commit-config.yaml"),
            "repos:\n  - repo: local\n    hooks: []\n",
        )
        .unwrap();
        let bin = d.join("fake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_exec(&bin.join("pre-commit"), "#!/bin/sh\necho 'lint.....Failed'\nexit 1\n");
        run(&d, &["add", "-A"]);
        run(&d, &["commit", "-qm", "hook fixture"]);

        // Somebody's own work, already in the tree when the batch arrives.
        std::fs::write(d.join("mine.txt"), "do not touch\n").unwrap();

        let p = patch_mine(&d, "A", 5, "FIVE");
        let got = with_path(&bin, || {
            write_batch(&d, "base", "me@here", &[p], &[("f.txt".into(), 5)])
        })
        .unwrap();
        assert!(matches!(got, Written::Refused(_)), "{got:?}");

        assert_eq!(
            std::fs::read_to_string(d.join("mine.txt")).unwrap(),
            "do not touch\n",
            "the caller's own uncommitted file must survive the revert"
        );
        // And the patch itself is gone.
        assert!(!std::fs::read_to_string(d.join("f.txt")).unwrap().contains("FIVE"));
    }

    /// A refused rename has two halves to put back. Numstat names it by the new path
    /// alone, so restoring only that removed the new file and left the old one
    /// deleted: a refusal that lost a file.
    #[test]
    fn a_refused_rename_restores_both_halves() {
        let d = batch_repo();
        std::fs::write(
            d.join(".pre-commit-config.yaml"),
            "repos:\n  - repo: local\n    hooks: []\n",
        )
        .unwrap();
        let bin = d.join("fake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_exec(&bin.join("pre-commit"), "#!/bin/sh\necho 'lint.....Failed'\nexit 1\n");
        run(&d, &["add", "-A"]);
        run(&d, &["commit", "-qm", "hook fixture"]);

        // A real rename patch, the way an agent's `git diff` would print one.
        run(&d, &["mv", "f.txt", "g.txt"]);
        let diff = run(&d, &["diff", "--cached", "-M"]);
        run(&d, &["reset", "-q", "--hard"]);
        assert!(diff.contains("rename from f.txt"), "{diff}");
        let p = Patch {
            thread_id: "A".into(),
            diff,
        };

        let got = with_path(&bin, || {
            write_batch(&d, "base", "me@here", &[p], &[("f.txt".into(), 5)])
        })
        .unwrap();
        assert!(matches!(got, Written::Refused(_)), "{got:?}");
        assert!(d.join("f.txt").exists(), "the old half of the rename was not restored");
        assert!(!d.join("g.txt").exists(), "the new half of the rename was left behind");
        assert!(crate::git::is_clean(&d).unwrap());
    }
}
