# fix-pr — get a PR green, an orchd run

Vendored here so the daemon carries its own copy: it is substituted and passed to
`claude -p` inline, not resolved from the agent's command path.

You are getting PR **{{PR}}** of `{{OWNER}}/{{REPO}}` green. Placeholders
`{{PR}}`, `{{OWNER}}`, `{{REPO}}`, `{{LOGIN}}`, `{{UPSTREAM}}` and
`{{UPSTREAM_REMOTE}}` are filled in by the daemon before you see this.

Mechanical only. Never posts a comment, never re-requests a review, never opens or
merges a PR, never resolves a thread. Review threads are `/resolve`'s job.

## Where you are

The daemon has already created a worktree pinned to this PR's head branch and
started you inside it. Do not switch branches and do not create one — §8 requires
the run stay on the PR's head ref, and `--worktree` would cut a fresh branch off
`{{UPSTREAM}}` instead.

Confirm rather than assume: `git rev-parse --abbrev-ref HEAD` against
`headRefName` from step 1. A mismatch is a stop, not something to correct by
switching.

## Steps

1. `gh pr view {{PR}} --json headRefName,headRefOid,headRepositoryOwner,url,title,mergeable,statusCheckRollup`.
   Head owner is not `{{LOGIN}}` → stop, it's someone else's branch to force-push.
2. Dirty tree → stop and show it. You are already on the right branch (above).
3. `git fetch {{UPSTREAM_REMOTE}} && git rebase {{UPSTREAM}}`. Conflicts: resolve
   them, never `git merge`. A conflict whose resolution is a judgement call about
   behaviour → stop and ask, with both sides shown.
4. Fix what's red:
   - Failed checks from step 1 → fetch each log; the repo's `github` skill has the
     commands and the CI-tests-merged-with-develop gotcha.
   - A failure naming a test absent from the working tree came from develop. Still
     yours to fix; say so in the report.
   - Run the repo's pre-commit (`mise run pre-commit:run` where it exists) before
     pushing.
5. Amend into the commit that owns the change; never a "fix review" or "fix CI"
   commit. The subject still describes the change after amending; if it no longer
   does, rewrite it. Splitting or reordering commits: only when asked.
6. `git push --force-with-lease`. The daemon's push guard denies plain `--force`
   and any push to the base branch — those denials are correct, do not work
   around them.
7. Watch with the Monitor tool over `gh pr checks {{PR}} --watch --interval 60`,
   event on each failure and on completion. A failure lands → back to step 4,
   amend, push, keep watching.
8. Report: what was rebased onto, what was fixed, final check state.

## Stop instead of pushing again

- The same job failed twice on the same fix. Report the log, don't try a third.
- The fix would change behaviour beyond making the check pass (deleting an
  assertion, widening a type, dropping a rule to silence a linter). Say what would
  make it green and let the user call it.
- `--force-with-lease` is rejected, someone else pushed. Show `git log` of both
  sides, don't overwrite.

## Verify before reporting green

A check that went green because the test stopped testing is not fixed. For each
fix, state the mechanism: what was broken, what now makes it pass. Silencing a
linter counts as a stop condition, not a fix.

The daemon does not read this report — it re-reads the PR's check state after you
exit and decides for itself whether the run ended red. The report is for the human
scrolling back through the pane, so make it worth reading there.
