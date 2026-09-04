---
name: green
description: Get a PR green. Rebase it on its base branch, fix the easy red, ask about the rest, amend into the commit that owns the change, force-push, and watch CI until it passes. Use when a PR is red or has fallen behind, and never for review threads, which are somebody's words rather than a failing check.
---

# Get a PR green

`/orchd:green <pr-url|number>`, or no argument for the current branch's PR.

Mechanical only. Never posts a comment, never re-requests a review, never opens or
merges a PR. Review threads are a different job.

**Two lines here are a repo's convention rather than a rule**, and they are marked
where they appear: which task runner runs the checks, and which remote the base
branch is fetched from. Read the repo's own `CLAUDE.md` before assuming either.

## Steps

1. `gh pr view <n> --json headRefName,baseRefName,headRepositoryOwner,url,title,mergeable,statusCheckRollup`.
   Head owner is not `gh api user --jq .login` → stop, it is someone else's branch
   to force-push.
2. On the branch already? Stay. Otherwise `git fetch origin <headRefName> && git
   switch <headRefName>`. Dirty tree → stop and show it.
3. Rebase onto the PR's own base: `git fetch <remote> <baseRefName> && git rebase
   <remote>/<baseRefName>`. Conflicts: resolve them, never `git merge`. A conflict
   whose resolution picks a behaviour is a judgement call, see
   [Easy or a judgement call](#easy-or-a-judgement-call).
   *This repo's convention:* the base lives on `upstream`, and the daemon's
   `upstream_ref` setting is the same answer where a session cannot ask GitHub.
4. Fix what is red, easy ones only:
   - Failed checks from step 1 → fetch each log. A repo with a `github` skill of
     its own says how; otherwise `gh run view <id> --log-failed`.
   - A failure naming a test absent from the working tree came from the base
     branch. Still yours to fix; say so in the report.
   - Run the repo's pre-commit checks before pushing. *This repo's convention:*
     `mise run pre-commit:run`.
5. Amend into the commit that owns the change; never a "fix review" or "fix CI"
   commit. The subject still describes the change after amending; if it no longer
   does, rewrite it. Splitting or reordering commits: only when asked.
6. `git push --force-with-lease`.
7. Watch with the Monitor tool over `gh pr checks <n> --watch --interval 60`, event
   on each failure and on completion. A failure lands → back to step 4, amend, push,
   keep watching.
8. Report: what was rebased onto, what was fixed, final check state.

## Easy or a judgement call

Easy is a forced fix, yours without asking: one way to make the check pass, and it
does not change what the code does.

A judgement call is a fix that picks a behaviour, where more than one answer is
defensible. Ask, four at a time, each carrying the failing output and the code as
it stands. Options are real positions, never `fix it / skip it`.

**Ask through `orch ask`** when it is there, which is every session the
orchestrator started: the answer reaches a card the person clicks rather than a
prompt in a pane they are not looking at. `AskUserQuestion` is the fallback for a
session nobody is orchestrating. The `orch` skill covers the flags.

Unsure which it is → a judgement call.

## Stop instead of pushing again

- The same job failed twice on the same fix. Report the log, do not try a third.
- `--force-with-lease` is rejected, someone else pushed. Show `git log` of both
  sides, do not overwrite.

## Verify before reporting green

A check that went green because the test stopped testing is not fixed. For each
fix, state the mechanism: what was broken, and what now makes it pass. Silencing a
linter or deleting an assertion is a judgement call, not a fix.
