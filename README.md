# orchd

Rust daemon + browser SPA that hosts several Claude Code sessions over one
monorepo. Built from `orchestrator-spec.md`; the design comes from
`orchestrator-ui.html`.

Steps 1-4 of the spec's build order are implemented. Diff viewer, PR poller,
review queue and `/green` are not.

## Run

```
mise install
cargo run -- --main /path/to/acme
```

It prints a URL with the token in it. Config lands at
`~/.config/orchd/config.json` on first run, hooks at `~/.config/orchd/hooks.json`.

There is no system C toolchain on the machine this was built on, so
`.cargo/config.toml` points the linker at `tools/zigcc`. Delete both once
`build-essential` is installed.

## What works

- **pty host** — every session and process runs in a daemon-owned pty with a
  512KB ring buffer. Closing the browser kills nothing; reopening replays.
- **Sessions** — spawned with `--session-id` so the daemon's id and Claude's are
  the same value, and `$ORCH_SESSION_ID` for hook correlation.
- **State machine** — `SessionStart` → Working, `Stop` → `YourTurn` with a wait
  clock, `BuildFailing` when a managed process is red, `SessionEnd` → Exited.
  `SubagentStop` is an explicit no-op.
- **Main exclusivity** — one session at a time, no queue.
- **Process drawer** — managed processes with health parsed from output, plus
  `$SHELL` on demand in any workspace.
- **Worktrees** — created by launching `claude --worktree` from the main
  checkout, so the repo's own `worktree-create` / `worktree-link` hooks do the
  work. Six-check teardown preflight; `git worktree remove` only, never `rm -rf`.
- **Changed files** — `PostToolUse` for the exact path, `git status
  --porcelain=v2` reconcile on `Stop` and at most once per 30s while working.
- **Restart recovery** — session records persist; previously live sessions come
  back `Archived`, orphaned pids are reaped.

## What the spec got wrong

Three things were checked against a real Claude session rather than assumed.

**`--settings` merges, it does not replace.** §3 flagged this as needing
verification and §11 gave contradictory advice. Both project and daemon hooks
fire, so the repo's `worktree-edit-boundary` and `pre-bash` keep working and
§3's fallback of inlining them is unnecessary.

**`SessionStart` is never delivered over HTTP.** Every other event is
(`UserPromptSubmit`, `PostToolUse`, `SubagentStop`, `Stop`, `SessionEnd`), but
the http form of `SessionStart` silently does nothing while the command form
fires. Without the workaround in `hooks.rs` every session sits in `Starting`
forever. This is the one place the spec's hook table cannot be implemented as
written.

**`SubagentStop` really is separate from `Stop`.** A subagent emitted only
`SubagentStop`, and it arrived before the main agent's `Stop`. §3's worry was
well founded.

Two spec rules also had to be loosened, both for the same reason — taken
literally they create a state nothing can escape:

- **Unpushed check.** "No remote counterpart means every commit is unpushed,
  block" also blocks a fresh worktree, which is branched off `upstream/develop`
  and carries nothing. It now counts commits beyond the base, so it still fails
  closed on real work.
- **Transcript check.** A session killed before its first turn never gets a
  `.jsonl`, so requiring one stranded the worktree. "Nothing to copy" is now
  distinguished from "not copied yet".

## Layout

```
src/
  main.rs       wiring, routes, startup recovery, SPA serving
  config.rs     config file, managed process specs, transcript path slug
  model.rs      Workspace / Session / Process, State, ArchiveState
  state.rs      the daemon's owned state, snapshots, occupancy, reconcile
  pty.rs        portable-pty host
  ring.rs       scrollback
  hooks.rs      hook receiver and the generated settings file
  spawn.rs      session/worktree/process spawning, health parsing
  git.rs        status parsing, refs, unpushed, worktree ops
  worktree.rs   teardown preflight, archive, removal
  store.rs      session record persistence, orphan reaping
  api.rs        HTTP surface and the origin/token guards
  ws.rs         event stream and pty attach
web/            SPA (vanilla, xterm.js vendored)
```

## Notes

- The daemon sets `core.fsmonitor`, `core.untrackedCache` and
  `fetch.writeCommitGraph` on the main checkout at startup (§4). fsmonitor is
  main-only on purpose.
- Managed processes do not autostart. Flip `autostart` in config if you want
  `docker compose up` on daemon start.
- Bound to `127.0.0.1` only, with Origin/Host validation and a per-start token
  on every mutating route. Hook endpoints are exempt from the token but confined
  to their own prefix and can only ever update state.
