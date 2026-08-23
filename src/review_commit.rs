//! Which commit a review batch's work may be folded into.
//!
//! A decision, not a git operation, which is why it is not in `git.rs`: "whose
//! commit may be rewritten" and "when does a fixup have to degrade to a plain
//! commit on top" are the *batch's* rules about not rewriting somebody else's
//! history and not quietly changing what you already approved. They lived beside
//! the plumbing only because shelling git was already there.
//!
//! The execution stays in [`crate::git`] — `fold_in` and `pre_commit` are
//! mechanism, and reach into that module's rebase and hook plumbing. The line is
//! decide here, do there. `patch.rs` is the only caller of either.

use anyhow::Result;
use std::path::Path;

use crate::git;

/// Where the accepted changes should be folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Amend {
    /// Fold into this commit of the PR's own history, keeping the subject of
    /// each commit true to what it contains.
    Fixup(String),
    /// Amend `HEAD` instead, with the reason — shown so the fallback is visible
    /// rather than silent.
    Head(String),
    /// Make a **new** commit, because rewriting anything here would rewrite work
    /// that is not ours. The reason is shown for the same purpose.
    OnTop(String),
}


/// The **only** way to build an [`Amend::Head`].
///
/// Amending `HEAD` rewrites it, so the "someone else authored it" guard above is
/// worthless if the fallback then rewrites `HEAD` regardless — which is exactly
/// what happened: `Amend::Head("<sha> is <email>'s commit")` was a value whose own
/// text said it had refused, handed to a `git commit --amend`.
///
/// Note the discriminator is **authorship, not publication**. It has to be: a batch
/// only starts when the branch is level with origin, so `HEAD` is always already
/// pushed. The question is never "has anyone seen this commit", it is "is it mine
/// to rewrite".
pub fn head_or_on_top(cwd: &Path, my_email: Option<&str>, why: String) -> Amend {
    let Some(mine) = my_email else {
        return Amend::OnTop(format!(
            "{why}, and git has no identity here so no commit can be shown to be yours"
        ));
    };
    if git::is_merge(cwd, "HEAD") {
        return Amend::OnTop(format!("{why}, and HEAD is a merge"));
    }
    match git::authors_in(cwd, &["-n", "1", "HEAD"]).as_deref() {
        Some([author, ..]) if author != mine => {
            Amend::OnTop(format!("{why}, and HEAD is {author}'s commit"))
        }
        Some(_) => Amend::Head(why),
        None => Amend::OnTop(format!("{why}, and who wrote HEAD could not be read")),
    }
}


/// Decide the fixup target for a set of touched lines, degrading to `HEAD`.
///
/// The original command asked a human to fold each change "into the commit that
/// owns it"; `git blame` answers that mechanically. Three cases degrade, each
/// deliberately:
///
/// - **the lines disagree** — one commit cannot be two, and splitting the batch
///   per target is more history surgery than this is worth;
/// - **the line came from the base branch** — it is not the PR's commit to
///   rewrite;
/// - **someone else authored it** — rewriting a colleague's commit under your own
///   force-push is not ours to do, and it changes a sha they may have checked out.
pub fn amend_target(
    cwd: &Path,
    rev: Option<&str>,
    merge_base: &str,
    touched: &[(String, u32)],
    my_email: &str,
) -> Result<Amend> {
    // The caller's identity wins; `effective_email` is the fallback for when it is
    // empty, which is what `git config user.email` returns in a container that never
    // set one — git still commits there, as `you@hostname`, so "no identity" must not
    // be read as "every commit belongs to somebody else".
    let mine = Some(my_email.trim().to_lowercase())
        .filter(|e| !e.is_empty())
        .or_else(|| git::effective_email(cwd));
    let degrade = |why: String| head_or_on_top(cwd, mine.as_deref(), why);

    if touched.is_empty() {
        return Ok(degrade("nothing to attribute".into()));
    }

    let mut target: Option<git::Blame> = None;
    for (path, line) in touched {
        let Some(b) = git::blame_line(cwd, rev, path, *line)? else {
            return Ok(degrade(format!("{path}:{line} is not in any commit yet")));
        };
        match &target {
            None => target = Some(b),
            Some(t) if t.sha == b.sha => {}
            Some(t) => {
                return Ok(degrade(format!(
                    "the changes span {} and {}",
                    git::short(&t.sha),
                    git::short(&b.sha)
                )))
            }
        }
    }
    let hit = target.expect("non-empty touched set");

    // An ancestor of the merge base came from the base branch, not this PR.
    if git::is_ancestor(cwd, &hit.sha, merge_base) {
        return Ok(degrade(format!("{} predates this branch", git::short(&hit.sha))));
    }

    // **The whole range, not just the target.** `fold_in` autosquashes with
    // `rebase -i <sha>~1`, which replays every commit from `<sha>` to `HEAD` and
    // gives each a new sha — so a colleague's commit sitting *on top* of the one
    // that owns this line is rewritten and force-pushed, without this path ever
    // being taken. Checking only `hit.author_email` guarded the target and left
    // its descendants wide open.
    let range = format!("{}^..HEAD", hit.sha);
    let in_range = git::authors_in(cwd, &[&range]).or_else(|| {
        // No `^` on a root commit, so ask for the commit itself.
        git::authors_in(cwd, &["-n", "1", &hit.sha])
    });
    match in_range {
        None => return Ok(degrade("who wrote this branch could not be read".into())),
        Some(authors) => {
            if let Some(other) = authors.iter().find(|a| Some(a.as_str()) != mine.as_deref()) {
                return Ok(degrade(format!(
                    "folding into {} would rewrite {}'s commit above it",
                    git::short(&hit.sha),
                    other
                )));
            }
        }
    }
    // A root commit has no `~1` for the rebase to start from.
    if !git::rev_exists(cwd, &format!("{}~1", hit.sha)) {
        return Ok(degrade(format!(
            "{} is the first commit, so there is nothing to rebase onto",
            git::short(&hit.sha)
        )));
    }
    Ok(Amend::Fixup(hit.sha))
}

