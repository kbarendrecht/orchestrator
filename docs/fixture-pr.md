# The review fixture

A throwaway GitHub repo with a PR whose review threads are somebody else's, so
the resolve flow can be *driven* rather than reasoned about.

```
mise run fixture                 # build or rebuild it
mise run fixture -- --threads 5
mise run fixture -- --destroy
```

## Why it has to exist

Two rules in `src/forge/github.rs` pull in opposite directions, and one account
cannot satisfy both:

- `query_for` polls `author:@me`, so the PR has to be **yours**.
- `acknowledged()` reads a thread whose last comment is yours as answered, so a
  thread waiting on you has to have been left by **somebody else**.

That is the wall TODO.md recorded four failed attempts against. Everything
downstream — triage, `post_one`, `sweep_words_only`, the 👍 path, the whole
two-phase resolve run — was unit-tested and had never made a real round trip.

## The second identity is `github-actions[bot]`

A `workflow_dispatch` workflow on the fixture's default branch posts the threads
with its own `GITHUB_TOKEN`. Its author login is `github-actions`, which is not
yours, which is the only property `acknowledged()` cares about.

Chosen over a second GitHub account because it needs no account, no stored
credential and no manual step. What it does **not** cover: `rerequest()`, since a
bot cannot be a requested reviewer. Verifying that button still wants a second
human identity.

## What it builds

A private `orchd-fixture` under your account, plus a local clone and a daemon
config dir under `~/.cache/orchd-fixture/`. Three threads, one per arm of the
triage card — a defect that wants a patch, a question that wants only words, and
an out-of-scope suggestion that wants a story — so a run has all three to
exercise.

The script asserts the precondition rather than assuming it: every open thread's
last comment must be authored by somebody other than the viewer, and it fails
loudly if not. That check is the point of the whole thing.

## Pointing a daemon at it

```
ORCHD_CONFIG_DIR=~/.cache/orchd-fixture/config cargo run -p orchestrator-desktop
```

`ORCHD_CONFIG_DIR` moves *every* piece of durable state — `config.json`,
`sessions.json`, `automation.json`, `hooks.json`, the instance lock, the findings
block. Without it a fixture run writes throwaway sessions into your real
`sessions.json`, rewrites `main_checkout` to a scratch clone, and — because
`todo_path` defaults to this repo's own TODO.md — puts a fake repo's live-findings
block into a tracked file.

Overriding `HOME` would have relocated all of it for free and is the wrong lever:
`claude` reads its credentials from there, so every session the fixture daemon
spawned would come up unauthenticated.

The fixture config sets `todo_path` to a file in the config dir rather than
anywhere inside the clone, because the daemon rewrites that block on every poll
and a dirty fixture worktree silently changes what `triage::gate` sees.

## A fresh clone needs Claude Code's trust accepted once

A daemon on a just-cloned fixture cannot create worktrees: `claude --worktree`
refuses with "Workspace trust not yet accepted" and the spawned session exits
instantly, leaving a workspace record pointing at a path that was never created.
Accept trust once for the clone — run `claude` in it and accept the dialog, or
set `hasTrustDialogAccepted: true` for that directory in `~/.claude.json`. The
monorepo never shows this because it was trusted long ago; a fixture is the first
untrusted checkout the daemon points at.

## Two things that cost an afternoon

- **GitHub only indexes workflow files a push actually touches, and the first
  push to an empty repo is not indexed at all.** A workflow that arrives in that
  first push is never registered: `actions/workflows` reports `total_count: 0`,
  `gh workflow run` 404s with "not found on the default branch", and the file is
  plainly sitting there on the default branch. Nothing reports an error. The
  repo is therefore created with `--add-readme` so the workflow lands in an
  ordinary second push. Diagnosed by pushing an unrelated `ping.yml` in a later
  commit and watching it register instantly while the fixture's own workflow, in
  the first push, stayed invisible.
- **Rebuilding does not delete the repo.** `gh repo delete` needs the
  `delete_repo` scope, which a `repo`-scoped token does not have and which is a
  lot of authority to grant so a scratch repo can be recycled. The fixture is
  reset in place instead: `main` is only ever fast-forwarded, the churn is
  confined to the PR branch, and stale workflow files from an older build are
  pruned so a reused repo cannot keep dispatching one.

## What it unblocks

Each of these was listed in TODO.md as unverifiable, and each now has a target:

- The resolve flow end to end, against threads that really are awaiting you.
- `triage::gate`'s refusal on a dirty worktree — now safe to dirty on purpose.
- `open_file`'s `head_sha` arm, once a workspace sits on the PR branch.
- The thumbs-up idempotency assumption in `post.rs`, still a guess about whether
  GitHub returns the existing reaction or a second one.

Teardown and its archive are **done** — the first thing driven against the
fixture. Creating a worktree, killing its session and tearing it down proved the
archive auto-run correct and, in the same pass, turned up a bug the compile
could not: `claude --worktree` locks every worktree it cuts, the lock outlives
the killed session, and `git worktree remove` refused it forever. `worktree_remove`
now clears a lock whose pid is dead and retries, still without `--force`.

Rebuild before anything destructive. Every build is meant to be disposable.
