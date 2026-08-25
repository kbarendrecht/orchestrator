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

The **resolve run end to end** is **done**, and it found the bug the whole
fixture existed to find. A run on PR #9 answered three threads — two replies, one
👍, nothing resolved, both commits held local until the push button — and its
first attempt could not talk to the daemon at all: `…/thread/:id/committed`
answered `403 bad origin`, because the guard's ask-token exemption listed `/ask`,
`/wait` and `/spawn` and never gained `/committed`. Every line of that path was
unit-tested. The route was unreachable by its only caller.

Two notes for whoever drives the next one:

- **The fixture config carries no `port`,** so a fixture daemon wants 7777 and the
  headless binary refuses rather than starting beside your real app. Add
  `"port": 7799` to `config.json`, or close the app first.
- **A run is not unattended.** Its first act is reading `plan.json` in
  `config_dir`, outside the worktree, so Claude Code asks permission before the
  agent has read the plan — and again per commit and for the `committed` curl.
  Driving it headlessly means answering those over the pty websocket
  (`/ws/pty?target=session:<id>`); the daemon's own `needs_permission` state is
  the signal to look for, not the screen.

`triage::gate`'s dirty-worktree refusal is **verified** — the second thing driven
against the fixture. A polled PR with a `pr-4` worktree, dirtied on purpose, made
`POST …/triage` refuse with the real file list and `GET …/review` report the
`dirty` gate; cleaning the tree cleared it. No code change — the gate was already
right; it had just never met a worktree safe to dirty.

`open_file`'s `head_sha` arm is **verified** too. With a `pr-4` worktree on the
PR's head branch, `POST /api/open/file` minted a blob URL against the PR's pushed
sha rather than local HEAD — shown distinct by a local-only commit that moved the
worktree's HEAD while the URL kept the pushed sha. The local-HEAD fallback still
serves a workspace no PR names. No code change; the arm just needed a workspace
whose branch matched a polled PR, which no monorepo checkout offered.

The **thumbs-up idempotency** guess is **settled**. The ignored `posts_for_real`
test (`forge/github_write.rs`), pointed at this PR, 👍'd a comment twice and got
the same reaction id both times — GitHub is idempotent per (user, content), so
`post.rs`'s retry needs no ledger:

```
ORCHD_LIVE_REPO=kbarendrecht/orchd-fixture ORCHD_LIVE_PR=<n> \
  cargo test --lib -- --ignored --nocapture posts_for_real
```

It posts a real reply and reaction to the PR's first thread, so **rebuild the
fixture after running it** to restore clean awaiting-you threads.

Teardown and its archive are **done** — the first thing driven against the
fixture. Creating a worktree, killing its session and tearing it down proved the
archive auto-run correct and, in the same pass, turned up a bug the compile
could not: `claude --worktree` locks every worktree it cuts, the lock outlives
the killed session, and `git worktree remove` refused it forever. `worktree_remove`
now clears a lock whose pid is dead and retries, still without `--force`.

Rebuild before anything destructive. Every build is meant to be disposable.
