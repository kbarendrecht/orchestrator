# Working in this repo

`orchd` is a Rust daemon plus a vanilla-JS SPA that hosts several Claude Code
sessions over one monorepo. **README.md** has the architecture and the module
map; **TODO.md** has what is open, the decisions worth revisiting, and the things
deliberately not built. Read TODO.md before proposing work: several obvious ideas
are already in there with the reason they were not done.

## Build and run

```
cargo check                         # the daemon
cargo test                          # 214 tests, all in-tree
cargo run -p orchestrator-desktop   # the app, daemon embedded in-process
mise run shot                       # screenshot the running SPA (drives Chrome)
```

The agent binary is `claude`, installed by the `claude-code` mise tool so one
`mise up` in the monorepo covers both.

**One daemon at a time.** The lock is `~/.config/orchd/instance.pid`, not the
port, so a second instance refuses to start rather than fighting over
`sessions.json` and the hook settings file. Close the running app before
`cargo run`.

## Things that will bite you

- **The SPA is compiled in.** `web/*` is `include_str!`d into the binary, so a
  CSS or JS change is invisible until the daemon is rebuilt *and* restarted. No
  amount of reloading the page helps.
- **The app is WebKitGTK, not Chrome.** `mise run shot` drives Chrome and is fine
  for layout and copy, but the two engines disagree often enough to matter.
  `tools/` pins `playwright-core` to the version whose WebKit build is on disk so
  an engine-specific fault can be reproduced; stub `/vendor/addon-webgl.js` in
  such a test, because headless WebKit dies on xterm's WebGL renderer.
- **Terminals in the desktop window use xterm's DOM renderer on purpose.**
  WebGL garbles glyphs under WebKitGTK: text arrives as noise and only comes back
  when a scroll or a selection forces a redraw. Clearing the texture atlas after
  every refit and disposing the addon on context loss both failed to fix it, so
  the canvas is gone in the webview. A browser tab keeps the fast path. Do not
  "optimise" WebGL back in without reading the TODO entry first.
- **The daemon's session id is Claude's session id.** Every spawn passes
  `--session-id`, which is what makes `--resume`, transcript lookup and hook
  correlation need no mapping. A fork passes `--session-id <new> --resume <old>
  --fork-session`, which is honoured. Keep that invariant.
- **Transcript paths slug both `/` and `.`.** `.claude/worktrees/x` becomes
  `--claude-worktrees-x`, not `-.claude-...`. Getting this wrong makes every
  worktree session look like it has no transcript.
- **Session names come from an undocumented field.** `store::ai_title` tails the
  transcript for `{"type":"ai-title","aiTitle":…}`. It degrades to the workspace
  name rather than failing, so a rail that suddenly reads `dfafdf` everywhere
  means Claude Code changed the format.
- **TODO.md has a daemon-written block.** Everything between the `orchd live
  findings` markers is rewritten on every poll. Edit outside it, and never commit
  the block's churn.
- **Hooks are observers, not gatekeepers.** They answer immediately and finish
  their work detached, because Claude gives a hook one second and a dropped
  future silently loses the state change. Do not make a hook wait on anything.
- **`github_write.rs` will not resolve a thread, approve, merge or open a PR.**
  That is a design boundary, not a gap. Resolving is the comment author's button.
- **Pushes are guarded.** `--force-with-lease` only, no `push -u`, no protected
  refs; `guards/push.py` denies the rest at `PreToolUse`. Never `git merge` into a
  branch here, rebase.

## Releases

Bump the version in `Cargo.toml`, `desktop/Cargo.toml`, `desktop/tauri.conf.json`
and `Cargo.lock`, commit as `Release <version>`, then `git tag v<version> && git
push origin v<version>`. The workflow refuses a tag that does not match the crate
version, because a released build that disagrees with its own tag nags about an
update it already is. Versions are CalVer: `<year>.<month>.<n>`.

## Style

Comments explain **why**, not what. The bar in this codebase is high and worth
matching: nearly every non-obvious line carries the reason it is that way, often
including the failure that produced it. A comment restating the code is worse
than none.

Commits are a subject line, imperative, no ceremony. A body only when the why is
not in the diff, and then one or two lowercase sentences. No marketing words, no
section headers, no bullet lists restating the change.

## Is the DOM renderer too slow?

Short answer: no, and here is the reasoning rather than a shrug.

The cost of xterm's DOM renderer scales with visible cells times update rate.
Terminals here are 40x140, so 5,600 cells, and xterm coalesces writes internally
so paints are capped at roughly one per frame no matter how fast the pty streams.
It was VS Code's default for years at larger sizes than this and is still its
fallback.

The thing that would have made it expensive does not happen: hidden terminals do
not paint. `.termhost[hidden]` is `display:none`, so a parked xterm has no
dimensions and its renderer stops. However many sessions are open, at most the
centre pane and one drawer terminal are actually rendering.

Where you would notice it is a sustained multi-megabyte burst, a `cargo build`
or a test log dumping faster than 60fps of DOM updates can keep up, which shows
as scroll lag rather than lost output. If that ever bites, the fix is not to turn
WebGL back on globally: give a context to the *visible* terminal only and dispose
hidden ones, which is nearly free here because the daemon's ring buffer replays
the scrollback on reattach.
