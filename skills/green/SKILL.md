---
name: green
description: Get a PR green. Take its base branch in the way the repo does, fix the easy red, ask about the rest, amend into the commit that owns the change, force-push, and watch CI until it passes. Use when a PR is red or has fallen behind, and never for review threads, which are somebody's words rather than a failing check.
---

# Get a PR green

`/orchd:green <pr-url|number>`, or no argument for the current branch's PR.

Mechanical only. Never posts a comment, never re-requests a review, never opens or
merges a PR. Review threads are a different job.

**Three things here are a repo's convention rather than a rule**, and they are
marked where they appear: whether the base is rebased onto or merged in, which task
runner runs the checks, and which remote the base branch is fetched from. Read the
repo's own `CLAUDE.md` before assuming any of them.

## Steps

1. `gh pr view <n> --json headRefName,baseRefName,headRepository,headRepositoryOwner,url,title,mergeable,statusCheckRollup`.
   Then whether you may force-push it: `$ORCH_MAY_PUSH` when the daemon set it,
   otherwise `gh api repos/<headRepositoryOwner>/<headRepository> --jq
   .permissions.push`. Not `yes` / `true` → stop, it is a branch you cannot push to.
   Push access, **not** whether the head repo's owner is you: an org's repo is owned
   by the org and is still yours to push to. It is the question the daemon asks.
2. On the branch already? Stay. Otherwise `git fetch origin <headRefName> && git
   switch <headRefName>`. Dirty tree → stop and show it.
3. Take the PR's own base in: `git fetch <remote> <baseRefName>`, then rebase onto
   `<remote>/<baseRefName>` or merge it, **whichever this repo does** — see
   [Rebase or merge](#rebase-or-merge) before choosing. Conflicts: resolve them
   either way. A conflict whose resolution picks a behaviour is a judgement call,
   see [Easy or a judgement call](#easy-or-a-judgement-call).
   *This repo's convention:* the base lives on `upstream`, and the daemon's
   `upstream_ref` setting is the same answer where a session cannot ask GitHub.
4. Fix what is red, easy ones only:
   - Failed checks from step 1 → fetch each log. A repo with a `github` skill of
     its own says how; otherwise `gh run view <id> --log-failed`.
   - A failure naming a test absent from the working tree came from the base
     branch. Still yours to fix; say so in the report.
   - Run this repo's checks before pushing.
5. Amend into the commit that owns the change; never a "fix review" or "fix CI"
   commit. The subject still describes the change after amending; if it no longer
   does, rewrite it. Splitting or reordering commits: only when asked.
   **Unless step 3 merged**: amending under a merge commit is the rewrite the merge
   shape exists to avoid, so add one commit that names what it fixes instead.
6. Push: `git push --force-with-lease` after a rebase or an amend, plain `git push`
   after a merge and a commit on top of it, which moved nothing already pushed.
7. Watch with the Monitor tool over `gh pr checks <n> --watch --interval 60`, event
   on each failure and on completion. A failure lands → back to step 4, amend, push,
   keep watching.
8. Report: which base you took in and how, what was fixed, final check state.

## Rebase or merge

The same rule as `/orchd:fix-pr`, because the two skills do one job and must not
answer it two ways. Which shape is right is the repo's convention rather than yours.
Read its `CLAUDE.md` or `CONTRIBUTING` first; where neither says,
`git log --merges <remote>/<baseRefName>..HEAD` answers it, because a branch that
already carries merges from the base is a branch somebody merges.

- **Rebase**, the default: you may push the branch (step 1) and this pass
  force-pushes anyway.
- **Merge**, where the PR branch may not be rewritten: a repo that asks for merge
  commits, a branch already carrying them, or a forge that refuses the force-push.

Rebasing a branch a repo merges throws away every reviewer's place in it; merging
into a repo that rebases puts a commit in the history its policy refuses. Neither is
undone by pushing again.

## Easy or a judgement call

Easy is a forced fix, yours without asking: one way to make the check pass, and it
does not change what the code does.

A judgement call is a fix that picks a behaviour, where more than one answer is
defensible. Ask with `AskUserQuestion`, four at a time, each carrying the failing
output and the code as it stands. Options are real positions, never `fix it / skip it`.

Unsure which it is → a judgement call.

## Stop instead of pushing again

- The same job failed twice on the same fix. Report the log, do not try a third.
- `--force-with-lease` is rejected, someone else pushed. Show `git log` of both
  sides, do not overwrite.

## Verify before reporting green

A check that went green because the test stopped testing is not fixed. For each
fix, state the mechanism: what was broken, and what now makes it pass. Silencing a
linter or deleting an assertion is a judgement call, not a fix.
