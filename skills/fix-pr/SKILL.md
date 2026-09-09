---
name: fix-pr
description: Get a pull request green. Take its base branch in, fix the red checks, amend, push, and watch until the checks settle. Mechanical only: never comments, never resolves a thread, never merges the PR itself. Use when somebody asks you to fix, rebase or unbreak a PR's CI, or when the orchestrator hands you a fix run.
---

# Get a PR green

`/orchd:fix-pr <pr>`. Mechanical only. **Never** post a comment, re-request a
review, open or merge a PR, or resolve a thread. Review threads are `/orchd:triage`
and `/resolve`'s job, and a run that starts answering people is doing something
nobody asked for. Merging the PR is what you never do; taking its base *into* the
branch is step 3, and on some repos that is a merge.

## What you are working on

Four values, and the daemon puts them in your environment when it starts a fix
run. There is no context call and no token here on purpose: a fix run force-pushes
unattended, so it is given the narrowest surface that does the job — it asks the
daemon for nothing and can therefore reach nothing.

| variable | what it is | when it is not set |
| --- | --- | --- |
| `$ORCH_PR` | the PR number | the argument you were invoked with |
| `$ORCH_UPSTREAM` | the PR's base, as a ref (`upstream/develop`) | `gh pr view <pr> --json baseRefName`, then that branch on the remote below |
| `$ORCH_UPSTREAM_REMOTE` | the remote to fetch (`upstream`) | `git remote` — one remote means it is that one; several means ask |
| `$ORCH_LOGIN` | you, on the forge | `gh api user --jq .login` |

The fallbacks are what make this work when a person types it in a checkout the
daemon never started. Use them; do not stop for a missing variable.

## Where you are

In a fix run, the daemon has already cut a worktree pinned to this PR's head
branch and started you in it. **Do not switch branches and do not create one** —
the run has to stay on the PR's head ref, and a fresh branch off the base is not
what is being fixed.

Confirm rather than assume: `git rev-parse --abbrev-ref HEAD` against `headRefName`
from step 1. A mismatch is a stop, not something to correct by switching.

## Steps

1. `gh pr view $ORCH_PR --json headRefName,baseRefName,headRefOid,headRepositoryOwner,url,title,mergeable,statusCheckRollup`.
   Head owner is not `$ORCH_LOGIN` → stop, it is someone else's branch to
   force-push.
2. Dirty tree → stop and show it. You are already on the right branch (above).
3. **Take the base in before you read a single log.** A check the base has already
   fixed is the commonest thing a run burns itself on, and its log looks exactly
   like a real failure. `git fetch $ORCH_UPSTREAM_REMOTE`, then take
   `$ORCH_UPSTREAM` into the branch the way this repo takes it. See
   [How this repo takes the base in](#how-this-repo-takes-the-base-in) for which of
   the two, and read that section before choosing. Conflicts: resolve them either
   way. A conflict whose resolution is a judgement call about behaviour → stop and
   ask, with both sides shown.
   **The PR's own base wins.** `$ORCH_UPSTREAM` is it whenever the daemon knew the
   PR, and the repo's default base only where it did not, so where the two disagree
   take `$ORCH_UPSTREAM_REMOTE/<baseRefName>` from step 1. A stacked PR sits on
   another PR's branch rather than on the default base, and taking the default base
   in would bury its parent.
4. **Push the sync and let the checks answer it.** Nothing came in (already up to
   date) → straight to step 5. Otherwise push, per
   [Pushing](#pushing), and re-read the checks with the Monitor tool over
   `gh pr checks $ORCH_PR --watch --interval 60`.
   - Green → the base was the whole of it. Report that and stop, having fixed
     nothing, which is the cheapest way this run can end.
   - Still red → carry on with *this* rollup. Step 1's described a head that no
     longer exists, and half its failures may be gone.
5. Fix what is red:
   - Failed checks from step 4 → fetch each log. If the repo has its own skills or
     docs for reading CI, follow them; watch for a CI that tests your branch merged
     with the base rather than as-is.
   - A failure naming a test absent from the working tree came from the base.
     Still yours to fix; say so in the report.
   - Run the repo's pre-commit (`mise run pre-commit:run` where it exists) before
     pushing.
6. Amend into the commit that owns the change; never a "fix review" or "fix CI"
   commit. The subject still describes the change after amending; if it no longer
   does, rewrite it. Splitting or reordering commits: only when asked.
   **Unless step 3 merged.** Amending under a merge commit is the same rewrite the
   merge shape exists to avoid, so there add one commit that names what it fixes
   and leave the history below it alone.
7. Push, per [Pushing](#pushing).
8. Watch with the Monitor tool over `gh pr checks $ORCH_PR --watch --interval 60`,
   event on each failure and on completion. A failure lands → back to step 5,
   amend, push, keep watching.
9. Report: which base you took in and how, what was fixed, final check state.

## How this repo takes the base in

Both shapes are ordinary, and which one is right is the repo's convention rather
than yours. Read its `CLAUDE.md` or `CONTRIBUTING` first; where neither says,
`git log --merges $ORCH_UPSTREAM..HEAD` answers it, because a branch that already
carries merges from the base is a branch somebody merges.

- **Rebase**, the default: `git rebase $ORCH_UPSTREAM`. The branch is yours (step 1
  proved it) and this run force-pushes anyway.
- **Merge**, where the PR branch may not be rewritten: a repo that asks for merge
  commits, a branch already carrying them, or a forge that refuses the force-push.
  `git merge $ORCH_UPSTREAM`, and keep that commit out of step 6.

Rebasing a branch a repo merges throws away every reviewer's place in it; merging
into a repo that rebases puts a commit in the history its policy refuses. Neither
is undone by pushing again, so spend the one command that tells you.

## Pushing

`--force-with-lease` follows the rewrite, not the step: a rebase or an amend moved
commits that are already pushed, so `git push --force-with-lease`. A merge and a
commit on top of it moved nothing, so plain `git push`.

**The push goes to the head branch's own remote, which is not always
`$ORCH_UPSTREAM_REMOTE`.** On a fork layout the head is on your fork and the base
is on upstream, so the two differ; on a single-repo layout they are one remote and
the distinction costs nothing. Follow the branch's own tracking, and where it has
none: `git push --force-with-lease <head remote> HEAD:<headRefName>`.

The daemon's push guard denies plain `--force` and any push to the base branch.
Those denials are correct, do not work around them.

## Stop instead of pushing again

- The same job failed twice on the same fix. Report the log, do not try a third.
- The fix would change behaviour beyond making the check pass (deleting an
  assertion, widening a type, dropping a rule to silence a linter). Say what would
  make it green and let the user call it.
- `--force-with-lease` is rejected, so somebody else pushed. Show `git log` of both
  sides, do not overwrite.

## Verify before reporting green

A check that went green because the test stopped testing is not fixed. For each
fix, state the mechanism: what was broken, and what now makes it pass. Silencing a
linter counts as a stop condition, not a fix.

The daemon does not read this report — it re-reads the PR's check state after you
exit and decides for itself whether the run ended red. The report is for the human
scrolling back through the pane, so make it worth reading there.
