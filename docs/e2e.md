# The end-to-end flows

`mise run e2e` drives the flows a person actually presses buttons for, against a
real daemon, offline and deterministically. Thirteen of them, one file each under
`tools/e2e/flows/`, about 45 seconds for the lot.

It exists because of what unit tests kept missing. Two bugs found by hand-driving
a daemon in one afternoon — a resume that skipped its branch check because the
path happened to exist, and a swap that reported total failure after the branches
had already moved — were both fully covered by green unit tests. The state machine
is driven by hooks arriving over HTTP against a real git repository; a test that
does not involve either cannot see that class of fault.

## What is real, and what is not

Real: the daemon binary, its HTTP API, the hook wiring, `sessions.json`, git —
actual worktrees, actual branch moves, actual `stash create` carries, actual
locks.

Substituted, both by PATH shim:

- **`claude`** → `tools/e2e/fake-claude.mjs`. The daemon spawns its agent as
  `CommandBuilder::new("claude")`, a PATH lookup, so this needs no product change.
  It honours only what the daemon reads: the argv contract (`--session-id`,
  `--resume`, `--fork-session`, `--worktree`), the hooks **as written in the
  settings file the daemon produced**, a transcript at Claude Code's own path, and
  staying alive on the pty until killed.
- **`curl`** → `tools/e2e/fake-curl.mjs`, and *only* for a flow that asks for a
  repo. `forge::github::graphql` is the daemon's one route out, and it goes through
  `curl`. The shim answers `api.github.com` from a JSON file in the sandbox and
  execs the real `curl` for everything else — which it must, because the
  `SessionStart` hook is also a `curl`.

Reading the hooks out of the settings file rather than hardcoding them is the part
worth keeping: change `hooks::write_settings` and these flows change with it,
instead of quietly testing a contract that no longer exists.

## The sandbox

Three relocations, each load-bearing (`tools/e2e/harness.mjs`):

- `ORCHD_CONFIG_DIR` moves every piece of durable state, so a flow's daemon does
  not fight the real one and the one-daemon lock is per-sandbox.
- `HOME` moves too. Normally that is the wrong thing to do — the real `claude`
  reads its credentials from there — but with a fake agent there is nothing to
  lose, and it keeps transcripts out of your `~/.claude/projects`.
- `PATH` is prefixed with the shim directory.

Each flow gets its own sandbox, its own port (asked of the kernel, not guessed)
and its own daemon. They run one at a time on purpose: the point is a readable
failure, and several daemons racing for CPU makes the timeouts the flaky part. A
failing flow keeps its sandbox and prints the path — the daemon log and the git
state in it are the whole account of what happened.

## Writing one

A flow is a module exporting `run(sandbox)`, optionally `name` and `options`:

```js
export const name = 'open a worktree (daemon-cut)'
export const options = { delegated: true, turns: 0 }

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(session)
  // …
}
```

`options` picks the sandbox: `delegated` puts worktrees under
`.claude/worktrees`, which is what makes the daemon hand the cut to
`claude --worktree` instead of doing it itself; `turns` is how many turns the agent
takes unprompted, and `0` is the session that was never typed into — the one fork
and resume refuse. `repo` turns GitHub on and installs the curl shim. `autoResume`
brings live sessions back across `t.restart()`.

`t.restart()` stops the daemon and brings it back on the same state. It is the only
way to reach the durable half — `restore`, `prune_ghosts`, `auto_resume`,
`first_per_workspace` — and the only honest test of `Session::resumable()`, which is
computed during `restore` from fields that exist only on the record.

**Wait, never sleep.** `t.settled(id)` and `until(...)` exist because a POST
returning is not the same as the state having changed: hooks arrive over HTTP
afterwards. Two of the three flakes found while writing these were a read taken
one step too early — including `occupant` being read before `claim_main` had run
at the far end of a relocation.

`t.settled` waits for `your_turn`, not `working`, deliberately: a session mid-turn
is the one thing a swap refuses, so a looser wait makes a flow race its own setup.
It also refuses `your_turn`/`ready` while the agent still has turns to take:
`SessionStart` parks a fresh session there before its first prompt, and taking
that for settled once failed five flows at once — a swap refused an agent
"mid-turn", a kill landed "before its first turn" and the daemon forgot the
session, and `has_transcript` read false. Only a turnless agent (`turns: 0`)
settles at `ready`.

## What it does not cover

- **The SPA.** These flows open no page. `mise run check-web` and `mise run shot`
  are that side — and `mise run term-e2e` is the one exception: it reuses this
  sandbox but opens a browser on a session's pane to drive the terminal (attach,
  the pty echo round-trip, a dropped socket reconnecting, and input banked while it
  is down). It needs Chrome, which is why it is a separate task rather than a flow
  here. See `tools/e2e/term.mjs`.
- **What a fix run does.** The rebase, the force-push, `fix_pr::settle` — the fake
  agent takes turns and never touches git or the forge. Only the *start* of fix-pr
  is covered: the guards, the worktree, the automation record.
- **A real round trip to GitHub.** `mise run fixture` is that, and it needs
  credentials. These flows are the offline half.
- **`claude --resume` semantics.** The fake agent replays a transcript; it does not
  prove Claude Code resolves a conversation by id wherever the file sits. That was
  measured separately against 2.1.240.
- **A real agent's timing.** The fake agent awaits its hooks, so `Stop` never
  overtakes `UserPromptSubmit`. Production tolerates that overtake; these flows do
  not exercise it.
