use anyhow::{bail, Context, Result};
use std::path::Path;

use super::*;

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
    // Bounded and unpromptable like every other network git call: a push against
    // a remote wanting credentials would otherwise wait on a tty forever, and this
    // one runs inside an HTTP request somebody is watching.
    let out = git_net(
        cwd,
        &["push", "--force-with-lease", "origin", branch],
        "the push",
    )
    .context("running git push")?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // The lease failing is the one refusal worth naming: it means the remote
    // moved, and the fix is to look at both sides rather than push harder.
    if lease_refused(&err) {
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

/// Did the remote refuse the push because it had moved?
///
/// Git's own markers, and only those: `--force-with-lease` reports `[rejected] …
/// (stale info)`, and an unforced push behind the remote says `fetch first` or
/// `non-fast-forward`. A bare `rejected` used to count too, and it also matches a
/// hook's `pre-receive hook declined` and a protected branch's refusal, both of
/// which were then blamed on somebody else's push and answered with "re-triage".
pub(super) fn lease_refused(stderr: &str) -> bool {
    stderr.contains("stale info")
        || stderr.contains("fetch first")
        || stderr.contains("non-fast-forward")
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
    let out = run(cwd, &args).context("running git blame")?;
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
    let ident = run(cwd, &["var", "GIT_AUTHOR_IDENT"])
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
pub fn authors_in(cwd: &Path, args: &[&str]) -> Option<Vec<String>> {
    let mut argv = vec!["log", "--format=%ae"];
    argv.extend_from_slice(args);
    let out = run(cwd, &argv).ok().filter(|o| o.status.success())?;
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_lowercase())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

/// Does `rev` have more than one parent?
pub fn is_merge(cwd: &Path, rev: &str) -> bool {
    run(cwd, &["rev-list", "--parents", "-n", "1", rev])
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

pub fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Put `paths` back to what `HEAD` holds, deleting the ones `HEAD` does not have.
///
/// For undoing a patch **this daemon applied** when a later step refused it. Only
/// ever the paths named: a pre-commit hook that reformatted something else is not
/// this function's to guess at, and the caller says so in its refusal instead.
///
/// Never for a person's own edits. The distinction is the whole safety of it — see
/// `patch::write_batch`, which reverts, against `write_manual`, which must
/// not.
pub fn restore_paths(cwd: &Path, paths: &[String]) -> Result<()> {
    for p in paths {
        if git_ok(cwd, &["cat-file", "-e", &format!("HEAD:{p}")]) {
            git(cwd, &["checkout", "-q", "HEAD", "--", p])?;
        } else {
            // Not in HEAD, so the patch is what created it. `remove_file` rather
            // than `git clean`, which would take unrelated untracked files with it.
            let at = cwd.join(p);
            if at.exists() {
                std::fs::remove_file(&at).with_context(|| format!("removing {}", at.display()))?;
            }
        }
    }
    Ok(())
}

/// Does this revision resolve?
pub fn rev_exists(cwd: &Path, rev: &str) -> bool {
    git_ok(cwd, &["rev-parse", "--verify", "--quiet", rev])
}

/// Is `a` an ancestor of `b`? Exit status only, so a failure means "no".
pub fn is_ancestor(cwd: &Path, a: &str, b: &str) -> bool {
    git_ok(cwd, &["merge-base", "--is-ancestor", a, b])
}

/// The three shapes [`fold_in`] can be asked for.
///
/// The *decision* is `review_commit::Amend`, which also carries the reasons a
/// person reads. This is what is left once that decision is made, and it is a
/// separate type because the executor importing the decision is what made `git`
/// and `review_commit` import each other. `Amend::fold` is the one conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fold {
    /// Fold into this commit of the PR's own history.
    Fixup(String),
    /// Amend `HEAD`.
    AmendHead,
    /// A new commit on top, with the reason for the message.
    OnTop(String),
}

/// Commit everything staged-or-not into the shape a review batch asked for.
///
/// `Fixup` writes a `fixup!` commit and then autosquashes it away, so the PR's
/// history keeps one commit per change rather than growing a "fix review" commit.
/// **`--autosquash` is silently ignored without `-i`**, which is why the
/// sequence editor is stubbed out rather than the flag used alone.
///
/// A conflict during the rebase aborts and reports: there is no session attached
/// to a button press to resolve one, and leaving a stopped rebase behind would
/// strand the worktree.
pub fn fold_in(cwd: &Path, fold: &Fold) -> Result<()> {
    git(cwd, &["add", "-A"])?;
    match fold {
        Fold::OnTop(why) => {
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
        Fold::AmendHead => {
            git(cwd, &["commit", "--amend", "--no-edit"])?;
            Ok(())
        }
        Fold::Fixup(sha) => {
            git(cwd, &["commit", "--fixup", sha])?;
            // Through the runner like every other call, so this exec is in the
            // start figure and can produce a `slow git` line; the two editors are
            // what makes it need the environment-taking one.
            let out = run_with(
                cwd,
                &["rebase", "-i", "--autosquash", &format!("{sha}~1")],
                &[("GIT_SEQUENCE_EDITOR", "true"), ("GIT_EDITOR", "true")],
            )
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

/// How long the repo's pre-commit hooks may take.
///
/// Generous for [`NET_TIMEOUT_SECS`]'s reason and one more: a first run builds an
/// environment per hook, which clones and installs, and a hook is itself a whole
/// linter over the files it was given. This is a backstop against hanging.
pub(super) const PRE_COMMIT_TIMEOUT_SECS: u64 = 300;

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
///
/// **Bounded, like every other child process the daemon starts.** This one is
/// somebody else's program running somebody else's hooks: on a cold cache
/// `pre-commit` clones each hook's repository and builds its environment, so it
/// reaches the network and it can take minutes — and it sits on the request chain
/// that answers a review. It was a plain `Command::output()`, which has no deadline
/// at all, so a hook waiting on a prompt or a dead host hung the request for good.
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

    let mut argv: Vec<String> = vec!["pre-commit".into(), "run".into(), "--files".into()];
    argv.extend(files.iter().cloned());
    let out = match crate::proc::run_bounded(cwd, PRE_COMMIT_TIMEOUT_SECS, &argv, "pre-commit") {
        Ok(o) => o,
        // The one error that is not a failure. `run_bounded` reports a spawn error
        // through `anyhow::Context`, so the kernel's own answer is a source rather
        // than the top of the chain — and "not installed" has to keep reading as a
        // warning, not as a refused review.
        Err(e) if crate::proc::not_installed(&e) => return Ok(PreCommit::NotInstalled),
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

pub(super) fn hash_files(cwd: &Path, files: &[String]) -> std::collections::HashMap<String, u64> {
    files
        .iter()
        .map(|f| (f.clone(), hash_one(cwd, f)))
        .collect()
}

/// Content hash, not mtime: a hook can rewrite a file to the same bytes, and a
/// checkout can change mtime without changing content (`edit.rs` makes the same
/// choice for the same reason).
pub(super) fn hash_one(cwd: &Path, rel: &str) -> u64 {
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
