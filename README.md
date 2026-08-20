# orchd

Rust daemon + browser SPA that hosts several Claude Code sessions over one
monorepo. The spec and the design mockup it was built from are retired now the
work has landed; `TODO.md` is the living record.

All ten steps of the spec's build order are implemented, with one deliberate
change: `fix-pr` is hand-triggered, never automatic.

## Run

Two front ends over one daemon. The desktop app is the intended one; the
headless binary is the faster way to debug the daemon itself.

```
mise install
cargo run -p orchestrator-desktop          # the app
cargo run -p orchd -- --main /path/to/acme   # headless, browser at the printed URL
```

The headless binary prints a URL with the token in it. Config lands at
`~/.config/orchd/config.json` on first run, hooks at `~/.config/orchd/hooks.json`.
The app reads the same config and asks for the checkout in a folder dialog when
there is none — or when the one on record has moved.

The SPA is compiled into the binary with `include_str!`, so editing anything
under `web/` needs a `cargo build` before it takes effect.

## Desktop app

`desktop/` is a Tauri v2 shell around `orchd` the library. The daemon runs
**in the same process**: `orchd::start` binds a loopback port and the webview is
pointed at it. No sidecar, no agreed-upon port, nothing to leave running.

The window is frameless and the SPA draws its own titlebar; on macOS the real
traffic lights float over a transparent one instead. Window controls do *not*
use Tauri's IPC — the page's origin is `http://127.0.0.1:<port>` with a port
chosen at bind time, and opening IPC to that origin means whitelisting
`http://127.0.0.1:*`, which would hand window control to anything else served
there. `POST /api/window/*` carries the same token as the rest of the UI and the
Rust side calls Tauri's window API directly.

Closing the window kills the Claude sessions and `ng-watch`. It does not touch
containers: `docker compose up -d` has already detached by then. Sessions that
were live are recorded first, so `auto_resume` rebuilds the rail next launch.

### Building it

Tauri v2 needs **WebKitGTK 4.1**, which means a baseline of Ubuntu 22.04 or
Debian 12. Ubuntu 20.04 ships only 4.0 (libsoup2) and cannot build this — there
is no backport.

```
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
```

`cargo run -p orchestrator-desktop` is enough for development. Bundling a `.deb`
or AppImage needs the CLI: `cargo install tauri-cli --version "^2"`, then
`cargo tauri build`. No Node anywhere — there is no frontend build step, because
the daemon serves the page.

On a virtualised GPU — WSLg especially — WebKitGTK's accelerated compositing can
render as stray white tiles (a white box, hover leaving smears). Under WSL the
app forces the software paint path automatically (`WEBKIT_DISABLE_DMABUF_RENDERER`
and `WEBKIT_DISABLE_COMPOSITING_MODE`, set in `main`), skipped on a real desktop
and overridable if you set either variable yourself.

Terminals in the desktop window use xterm's DOM renderer, not the WebGL one:
WebKitGTK garbles the glyph canvas, and the text only comes back when a scroll or
a selection forces a redraw. A browser tab keeps the WebGL fast path.

## Install and update

Releases are cut by tagging: `git tag v0.2.0 && git push origin v0.2.0` runs the
`release` workflow, which builds on Ubuntu 22.04 and attaches a Linux binary
tarball to the GitHub release.

Install and upgrade through `mise` with the `ubi` backend, which pulls that asset
straight from the releases page:

```
mise use -g "ubi:kbarendrecht/orchestrator[exe=orchestrator-desktop]"
mise up          # upgrade to the newest release
```

A running app also checks the releases page on launch and every six hours; when a
newer version is out it shows a dismissible nudge in the window. The nudge only
*notices* — `mise up` is what installs it. The check is release-builds only, so
`cargo run` from a checkout never nags.

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
- **Restart recovery** — session records persist. With `auto_resume` on (the
  default) the sessions that were live when the daemon went down are relaunched
  with `--resume` on the next start, in main and in worktrees alike. A crash
  costs the scrollback, not the conversation. Orphaned pids are reaped.
- **Diff viewer** — read-only, two-dot against the merge-base commit. File list
  from `--numstat` first, hunks per file, word-level highlighting from a token
  LCS computed server-side, split/unified, folds that expand by widening `-U`.
- **PR poller** — one GraphQL search per 5 minutes for your own open PRs on
  upstream. Rollup read off the head commit, outdated threads excluded, capped
  thread pages rendered `50+`, stacks detected by `baseRefName`.
- **Review queue** — built in: the daemon asks the forge directly on an offset
  timer and ranks the result with a config-driven rule engine (`review_ranking`;
  `requested` or `all_open` coverage). Degrades to `unavailable` rather than to
  an empty queue, and reads `not configured` when no forge repo resolves.
- **`/resolve`** — worktree pinned to the PR's head branch, skill invocation
  typed into the pty once `SessionStart` lands.
- **Test capabilities** — per-suite trust and isolation from config, lockfile
  drift by content hash, an autoload probe, host↔container path mapping.
- **Editable diff pane** — the right side is a live buffer with a disk write
  path. Saving refuses if the file moved underneath you, and a `PreToolUse`
  deny tells the agent, once, that you rewrote a file so it re-reads instead of
  clobbering you.
- **`fix-pr`** — hand-triggered, with the whole §8 guard table: authorship,
  one run per PR, capability trust, dep freshness, active-session suppression,
  concurrency and process caps, shared-resource locks. Push guards deny
  `push -u`, bare `--force`, and pushes to protected refs or upstream.

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

Two more, from using it rather than reading it:

- **`main:instances` and main occupancy.** §7 rule 2 makes the e2e lock conflict
  with a session occupying main, because playwright teardown anchors its
  instances dir there. That turned "somebody has main open" into "no fix run may
  start anywhere" — and the run happens in the PR's own worktree, while a session
  in main is normally just editing code. The lock now conflicts with another
  *run* holding it, which is the collision that actually corrupts something. What
  you give up: running playwright by hand in main while a fix run also reaches
  e2e, where the two would fight over one instances dir.
- **Dead shells.** §2 says a dead shell keeps its buffer "until dismissed".
  Applied to every exit that makes Ctrl+D leave a corpse tab behind, which is
  the opposite of what pressing it means. A shell that exits cleanly is now
  removed outright; one that exits non-zero keeps its buffer and code, which is
  the case the rule was written for.

## `fix-pr` is triggered, not automatic

§8 fires `fix-pr` on a PR going red. It does not here — that was a deliberate
call, and it changes what the guard table is for: a gate you read before
starting, rather than one that trips while you are looking elsewhere. Everything
else in that table still applies, because those guards protect the machine and
the repo rather than the schedule.

What that removes: the trigger-on-transition rules, the stack ordering (nothing
fires on its own, so nothing needs serializing bottom-up), and the kill switch
(there is nothing to switch off). `Exhausted` is kept and shown as *gave up* on
the row, because a run that stopped without turning the PR green is worth
knowing about before you trigger another one.

`fix-pr` also creates the PR's worktree before it evaluates the guards. That is
not avoidable: lockfile drift and the autoload probe are questions about the
worktree, and there is no worktree to ask about until it exists. It is reusable
afterwards, including by `/resolve`.

## What the capability probe found

Pointed at the `dfafdf` worktree, the §7 rule 4 autoload probe reports every PHP
suite as `Untrusted`: `vendor` there is a plain symlink to main's, so composer's
`$baseDir` resolves to the main checkout and a suite run in the worktree would
load main's `src/`.

§7's "current state (post-WIP)" table says Unit and Integration are `Verified`
in a worktree, on the basis that worktrees get a real `vendor/` with copied
autoload files. That is not true of this checkout. Either the WIP is not in this
tree yet, or it has regressed — which is exactly what §7 says the probe is for.

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
  diff.rs       numstat, hunk parsing, word-level LCS
  git.rs        status parsing, refs, unpushed, worktree ops
  forge/        the Forge seam: trait + ForgeImpl dispatch (mod.rs), the
                agnostic model (model.rs), and the GitHub impl (github.rs
                token/GraphQL/PR model/stacks, github_write.rs the gh writes)
  reviews.rs    built-in review queue: candidates from the forge, config-driven
                ranking (coverage, rules, blockers, tiebreak), degraded states
  profiles/     baked-in config bundles a machine merges over (acme.json)
  capability.rs suites, trust, dep drift, autoload probe, path mapping
  fix_pr.rs     automation state and the fix-pr guard table
  edit.rs       file read/write with containment and conflict detection
  todo.rs       the generated block in TODO.md
guards/push.py  PreToolUse deny for dangerous pushes
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
- **Transcripts** are always written, and the daemon sets the environment
  explicitly rather than inheriting it. A shell inside a Claude Code session
  carries `CLAUDE_CODE_CHILD_SESSION`, which silently turns transcript saving off
  in every child — so without clearing it the daemon behaves differently
  depending on what launched it, and resume (§2) quietly stops working. There is
  no switch: it was a config flag while the daemon itself was being built and its
  throwaway sessions littered real session history, and every hour it was off
  cost a conversation instead.
- **Checking the UI from a terminal.** `mise run shot` drives Chrome, which is
  enough for layout and copy — but the app itself runs in **WebKitGTK**, and the
  two do not always agree. A splitter rule named `.split` collided with the diff
  body's own `.diff.split`, and the result was a 7px-wide diff pane: invisible in
  Chrome's numbers unless you measure the box, obvious the moment you look at
  pixels. `tools/` pins `playwright-core` to the exact version whose WebKit build
  is on disk (`npx playwright@1.49.1 install webkit`), so an engine-specific
  rendering fault can be reproduced rather than guessed at. Stub
  `/vendor/addon-webgl.js` in such a test: headless WebKit dies on xterm's WebGL
  renderer.
- `ng-watch` autostarts in main; nothing else does. Flip `autostart` in config
  if you want `docker compose up` on daemon start too. An autostarted process is
  not killed when the last session in its workspace ends, because no session
  started it.
- **GitHub auth** resolves in order: `ORCHD_GITHUB_TOKEN`, a `0600`
  `github_token_file`, then `gh auth token`. §6 wants read scopes only
  (`pull_requests`, `checks`, `contents`, `metadata`); gh's token carries write,
  so falling back to it logs a warning rather than passing quietly. GraphQL goes
  out through `curl` with the token on stdin, which keeps an HTTP+TLS stack (and
  its C toolchain) out of the build and the token out of the process table.
- **TODO.md** carries a generated block the daemon rewrites each poll with
  conditions that are true right now. Everything outside the markers is yours.
- Bound to `127.0.0.1` only, with Origin/Host validation and a per-start token
  on every mutating route. Hook endpoints are exempt from the token but confined
  to their own prefix and can only ever update state.
