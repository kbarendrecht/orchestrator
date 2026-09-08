---
name: fix-pr
description: Get a pull request green — rebase it on its base, fix the red checks, amend, force-push with a lease, and watch until the checks settle. Mechanical only: never comments, never resolves a thread, never merges. Use when somebody asks you to fix, rebase or unbreak a PR's CI, or when the orchestrator hands you a fix run.
---

# Get a PR green

`/orchd:fix-pr <pr>`. Mechanical only. **Never** post a comment, re-request a
review, open or merge a PR, or resolve a thread. Review threads are `/orchd:triage`
and `/resolve`'s job, and a run that starts answering people is doing something
nobody asked for.

## What you are working on

Four values, and the daemon puts them in your environment when it starts a fix
run. There is no context call and no token here on purpose: a fix run force-pushes
unattended, so it is given the narrowest surface that does the job — it asks the
daemon for nothing and can therefore reach nothing.

| variable | what it is | when it is not set |
| --- | --- | --- |
| `$ORCH_PR` | the PR number | the argument you were invoked with |
| `$ORCH_UPSTREAM` | the ref to rebase onto (`upstream/develop`) | `gh pr view <pr> --json baseRefName`, then that branch on the remote below |
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

1. `gh pr view $ORCH_PR --json headRefName,headRefOid,headRepositoryOwner,url,title,mergeable,statusCheckRollup`.
   Head owner is not `$ORCH_LOGIN` → stop, it is someone else's branch to
   force-push.
2. Dirty tree → stop and show it. You are already on the right branch (above).
3. `git fetch $ORCH_UPSTREAM_REMOTE && git rebase $ORCH_UPSTREAM`. Conflicts:
   resolve them, never `git merge`. A conflict whose resolution is a judgement call
   about behaviour → stop and ask, with both sides shown.
4. Fix what is red:
   - Failed checks from step 1 → fetch each log. If the repo has its own skills or
     docs for reading CI, follow them; watch for a CI that tests your branch merged
     with the base rather than as-is.
   - A failure naming a test absent from the working tree came from the base.
     Still yours to fix; say so in the report.
   - Run the repo's pre-commit (`mise run pre-commit:run` where it exists) before
     pushing.
5. Amend into the commit that owns the change; never a "fix review" or "fix CI"
   commit. The subject still describes the change after amending; if it no longer
   does, rewrite it. Splitting or reordering commits: only when asked.
6. `git push --force-with-lease`. The daemon's push guard denies plain `--force`
   and any push to the base branch — those denials are correct, do not work around
   them.
7. Watch with the Monitor tool over `gh pr checks $ORCH_PR --watch --interval 60`,
   event on each failure and on completion. A failure lands → back to step 4,
   amend, push, keep watching.
8. Report: what was rebased onto, what was fixed, final check state.

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
