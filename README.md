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

The SPA is compiled into the binary with `include_str!`, so editing anything
under `web/` needs a `cargo build` before it takes effect.

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

One more, from using it rather than reading it:

- **Dead shells.** §2 says a dead shell keeps its buffer "until dismissed".
  Applied to every exit that makes Ctrl+D leave a corpse tab behind, which is
  the opposite of what pressing it means. A shell that exits cleanly is now
  removed outright; one that exits non-zero keeps its buffer and code, which is
  the case the rule was written for.

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
- **Transcripts.** `persist_transcripts` in config decides whether spawned
  sessions write a `.jsonl`, and the daemon sets the environment explicitly in
  both directions rather than inheriting it. A shell inside a Claude Code
  session carries `CLAUDE_CODE_CHILD_SESSION`, which silently turns transcript
  saving off in every child — so without this the daemon behaves differently
  depending on what launched it, and resume (§2) quietly stops working. Default
  is on. Turning it off is useful while developing the daemon itself, so its
  throwaway sessions do not litter your real session history; the daemon logs a
  warning at startup and the teardown preflight says so rather than passing
  silently.
- Managed processes do not autostart. Flip `autostart` in config if you want
  `docker compose up` on daemon start.
- Bound to `127.0.0.1` only, with Origin/Host validation and a per-start token
  on every mutating route. Hook endpoints are exempt from the token but confined
  to their own prefix and can only ever update state.
