# git, and driving the API by hand

Every entry here cost something. `CLAUDE.md` indexes them by their first
line; this file is the rest — what happened, what was measured, and why the
code is the shape it is.

## Git exports its own state into hooks and `--exec`, and one of the variables is a *relative* path.
Measured, not assumed: a pre-commit hook here runs with
`GIT_INDEX_FILE=.git/index`, `GIT_PREFIX`, `GIT_AUTHOR_*` and `GIT_EXEC_PATH`
set. Because that index path is relative, any `git` a hook runs from a
*different* directory resolves it against that directory instead — which is how
the e2e suite, run from the hook, died on
`Unable to create '<newtree>/.git/index.lock': Not a directory`: a worktree's
`.git` is a file. Anything spawning git from a hook must strip `GIT_*` first;
`tools/e2e/harness.mjs` does, and that is the only reason the suite can run
there.

## `git rebase --exec 'cargo test'` did something unexplained.
It put test
fixture commits into the repo and moved its HEAD. Recovered fully, and the
mechanism was never confirmed — but the entry above is the strongest candidate
yet, and it re-opens a hypothesis once written off: `--exec` sets the same
variables, and the one that bites is `GIT_INDEX_FILE`, not the `GIT_DIR` that
was tested and cleared. Still worth avoiding until somebody proves it.

## A test that asserts on git's own error wording fails on an older git.
Git
says "already used by worktree at" from 2.35 and "already checked out at"
before it, and 2.34.1 is what some machines have — so the worktree guard and the
swap test both failed for a reason that had nothing to do with the code.
`git::refused_as_already_checked_out` matches either. The refusal is the
invariant; its phrasing is not.

## A route an agent calls needs a line in `is_ask_route`, and forgetting it fails as `bad origin`.
The vendored skills curl with no `Origin` and carry the
session's ask token, not the app token — so a session route missing from that
list is refused twice: the Origin check has no arm for it, and `needs_token`
then wants a token the agent is deliberately not given.
`…/thread/:id/committed` shipped like that, which made the resolve run's central
seam unreachable by its only caller while every unit test passed. Add the
suffix, and the test in `api::tests` that walks the paths the prompts really
call.

## Driving the API by hand has four traps.
The header is `x-orch-token`
(`Authorization: Bearer` is not read), the route is `/api/state` (`/api/snapshot`
does not exist, and an unknown route answers `{}`, which reads exactly like an
empty daemon), a POST needs an `Origin` matching the port or it is "bad origin",
and the config key is `worktrees_subdir`. An unknown config key is ignored in
silence, so `worktrees_dir` leaves the daemon managing `.claude/worktrees` and
logging that it is "ignoring worktree outside the managed dir".
