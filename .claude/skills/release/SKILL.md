---
name: release
description: Use when the user types /release or asks to "cut a release", "ship a version": push main to origin and run `mise run release`, which bumps the CalVer version, waits for CI, tags, and waits for the release to publish assets.
---

# Cut a release

`mise run release` does the work, and CLAUDE.md § Releases says what it waits
for and why. This skill is the steps around it.

## Steps

1. **Check the tree.** On `main`, nothing uncommitted:

   ```
   git status -sb
   ```

   Uncommitted changes: stop and ask. A release is the commit, so a dirty tree
   would ship something other than what was tested.

2. **Line up with origin.**

   ```
   git fetch origin
   git log --oneline HEAD..origin/main
   ```

   If origin has commits this branch lacks, rebase onto `origin/main` and run
   the gates again before going on. Never merge: history stays linear.

3. **Push main.** `mise run release` waits for `check` on the commit you are
   on, and CI only runs on a pushed commit:

   ```
   git push origin main
   ```

4. **Run the release** in the background. It waits for `check` to go green,
   bumps the version from the clock, commits, tags, pushes, and waits for the
   release workflow and its assets. That takes ten to twenty minutes:

   ```
   mise run release
   ```

   Use `run_in_background` and wait for the notification. Do not poll.

5. **When it ends**, read the output:
   - green: report the version and the release URL it printed.
   - `check` red on the commit: report the failing job (`gh run view --log-failed`),
     fix it, and start again from step 1. Nothing was tagged.
   - the release workflow red, or green with no assets: fix the cause, commit,
     push, and run `mise run release --retry`. That re-cuts the same version
     rather than spending a new number.

## Report

One or two lines: the version, the release URL, and anything that failed on
the way.
