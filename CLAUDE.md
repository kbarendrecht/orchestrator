# Working in this repo

`orchd` is a Rust daemon plus a vanilla-JS SPA that hosts several Claude Code
sessions over one monorepo. **README.md** has the architecture and the module
map; **TODO.md** has what is open, the decisions worth revisiting, and the things
deliberately not built. Read TODO.md before proposing work: several obvious ideas
are already in there with the reason they were not done.

## Build and run

```
cargo check                         # the daemon
cargo test                          # 283 tests, all in-tree
mise run check-web                  # type-check the SPA + enforce its module graph
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

- **The SPA is compiled in.** Everything under `web/` is `include_str!`d, so a
  CSS or JS change is invisible until the daemon is rebuilt *and* restarted. No
  amount of reloading the page helps — and a stale process holding the port makes
  this worse, because you are then debugging a build from ten minutes ago.
  `pkill -x orchd` will not always do it: Linux truncates the process name to 15
  characters, so `orchestrator-desktop` needs killing by pid.
- **There is a pre-commit hook, and it needs enabling once per clone.**
  `git config core.hooksPath .githooks` — git will not let a repo point at its own
  hooks, so a fresh clone has none until you say this. It runs the SPA checks only
  when something they could fail on is staged: ~2s on a `web/` change, nothing at
  all on a docs commit. A Rust change also re-checks `web/snapshot.d.ts` and
  refuses if the committed copy no longer matches the structs.
  `--no-verify` is a fine thing to reach for mid-refactor; the real gate is
  `mise run check-web`.
- **`mise run check-web` is the SPA's gate, and it bites.** Three things in one:
  it regenerates `web/snapshot.d.ts` and fails if the committed copy drifted, it
  runs `tsc --noEmit --checkJs` over every SPA file, and it runs
  `dependency-cruiser` over the module graph. All three were checked against
  deliberate breakage — a `#[serde(rename)]`, a typo'd `snap.` field, and an added
  cycle each fail it. There is still **no build step**: `tsc` only checks, and the
  files ship exactly as written.
- **Type-checking found bugs clicking around did not.** Turning `checkJs` on after
  the module split surfaced five modules referencing names that had stayed behind
  in `app.js` (`pendingSelect`, `TOKEN`, `WS_BASE`, `selected`, `prOf`) — every one
  a `ReferenceError` waiting for a code path the browser checks never hit. Treat a
  green page as weaker evidence than a green `check-web`.
- **`ctl(id)` is the one deliberate `any` in the SPA.** `getElementById` can only
  promise `HTMLElement`, so reading `.value` through `$` is a type error even when
  the id certainly names an `<input>`. `ctl` is the named escape hatch for form
  controls; `$` stays typed so everything else fetched through it keeps being
  checked. Do not widen `$`.
- **The SPA's view of the snapshot is generated, not hand-written.**
  `web/snapshot.d.ts` comes from the Rust structs via `ts-rs` (a dev-dependency,
  derived under `cfg(test)`, so nothing of it reaches the binary). `cargo test`
  rewrites it; `mise run check-web` regenerates it and **fails if the
  checked-in copy has drifted**. Rename a snapshot field in Rust and the diff
  shows up there — which is the point, since the old failure mode was a renamed
  field reading as `undefined` and rendering as nothing. Commit the regenerated
  file with the Rust change. It is a type file: never `include_str!`d, never
  served.
- **ES modules work in the real webview — measured, not assumed.** WebKitGTK
  **2.50.4** ships here, and a spike drove the actual desktop window (not Chrome,
  not playwright's WebKit): a `type="module"` script imported a second module over
  a `/js/:file` route, the relative import resolved, strict mode was on, and the
  vendored classic globals (`window.__ORCH__`, `Terminal`, `Prism`) were all
  present by the time the module ran — module deferral happens *after* the classic
  scripts, so the ordering is safe. Two things the spike settled that matter for
  the migration: the content type must be a JavaScript one (`text/plain` loads and
  then refuses to execute), and modules come from `include_str!` like everything
  else, so **each new module needs an entry in the route's match and a rebuild** —
  adding a JS file stops being a JS-only change.
- **The SPA is a module graph, not a file.** `web/js/core.js` is the shared layer
  — the fetch wrappers, the DOM shorthands, the snapshot, the selection, the UI
  scale, and the vocabulary every pane needs to describe a session (`stateLabel`,
  `dotClass`, `isArchived`, `pending`, …). The features beside it are `term`,
  `rail`, `diff`, `review` (+ `review-diff`), `queue` and `settings`. `app.js` is
  what is left over: boot order, the websocket, the keyboard map, the window
  chrome — under a thousand lines, from 4798 before the split.
  `mise run check-web` prints the current module and dependency count.
- **The module graph is a DAG, and it was made one on purpose.** `app.js` → the
  six; `rail` → `term`, `review`; `review` → `diff`; everything → `core`. Three
  cycles had to be broken first, and each inversion is the reason a boundary is
  real rather than decorative:
  - zoom used to resize the terminals directly while they read the scale back.
    `core.setZoom` now announces through `onScaleChange`, and `term` registers.
  - the rail called `select()` which called `render()` which redrew the rail.
    `core` owns `selected`/`setSelected` and announces through `onSelection`;
    `app.js` registers what picking a session *means*.
  - the changed-files pane and the diff called each other, so the pane moved
    *inside* `diff` — two modules that call each other are one module with a line
    drawn through it.
  Adding a cycle back would work (ESM allows it) and would quietly undo this.
- **Each module needs a line in `module()` in `lib.rs` and a rebuild.**
  `include_str!` again: adding a JS file is a Rust change. That cost is why the
  modules track features rather than being cut finer.
- **`snap` is a live binding, and only `receive()` may replace it.** It is
  `export let` in `core.js`, so a hundred readers keep saying `snap.x` and see the
  new snapshot without re-importing. `receive` sets the snapshot and the clock it
  is measured against together — those drifting apart is what froze durations.
- **Never rewrite an identifier across an SPA file with a regex.** Three of the
  four apparent uses of `resize` were the *string* `'resize'` — an event name and a
  URL path — and a blind substitution would have broken window resizing with
  nothing failing. Rewrite by line number, asserting each line really is a call.
  `mise run check-web` catches a *renamed* identifier; it cannot catch a string
  that changed meaning.
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
  means Claude Code changed the format. Identical titles across unrelated sessions
  are Claude Code's doing, not a bug here: one `ai-title` string turned up in 8
  transcripts across 3 repositories, each correctly attributed to its own
  sessionId. The reader is right; the file says that.
- **TODO.md has a daemon-written block.** Everything between the `orchd live
  findings` markers is rewritten on every poll. Edit outside it, and never commit
  the block's churn.
- **Shelling out to coreutils is the other portability trap.** The review queue
  ran its command under `timeout`, which is GNU and not on a Mac, so it failed at
  the spawn and the pane blamed the review command for a missing binary it never
  named. `proc::run_bounded` enforces the deadline in Rust instead — one
  bounded-exec primitive (own process group, SIGKILL the group, pipes drained on
  threads), used by both the review queue and `worktree_setup`. Every other
  command the daemon spawns is POSIX (`git`, `curl`, `gh`, `ps`, `which`, `kill`) —
  keep it that way, and check `command -v` before reaching for a GNU flag.
- **A daemon-cut worktree fires no `WorktreeCreate` hook, so repo setup runs
  through `worktree_setup` instead.** Claude's `WorktreeCreate` fires only for
  `claude --worktree`; PR worktrees, resumes and relocated layouts are cut by the
  daemon's own `git worktree add`, which Claude knows nothing about — so anything
  the repo's hook did at creation (a rules-dedup file, in the case this was
  written for) was silently skipped. `spawn::run_worktree_setup` runs the configured command in each
  daemon-cut worktree, before the session, non-fatal. Do **not** add it to the
  `claude --worktree` arm — the repo's hook already ran there. A relative script
  path resolves against `main_checkout`; cwd is the worktree.
- **Paths are resolved at one boundary, and comparing across it silently fails.**
  `main_checkout` is `canonicalize`d in `Config::parse`, so `worktrees_dir` and
  `worktree_path` are resolved too, and the agent-reported cwd is resolved where a
  delegated worktree is adopted. That is what lets `workspace_for_path` match the
  resolved paths `PostToolUse` hands it — an unresolved workspace root matches
  nothing, and the symptom is not an error but an edit that never appears in the
  changed-files pane. Do not introduce a workspace path that skipped that step.
  Barely visible on Linux; on macOS `/tmp`, `/var` and `$TMPDIR` are symlinks into
  `/private`, so it is the normal case.
- **A `/proc` read is a portability bug that compiles.** Two guards stat'd `/proc`
  and so answered *wrongly*, not loudly, off Linux: `pid_alive` read every session
  as dead (teardown would delete a worktree with a live agent — it fails open), and
  `instance::holder` read every lock as stale (a second daemon starts). Both are
  now portable — `kill(pid, 0)` and `ps -p <pid> -o command=`. `headroom` still
  reads `/proc/meminfo` on purpose, because it is documented to mean "no opinion"
  when it cannot read. Before adding a `/proc` read, ask which way it fails when
  the file is absent; CI cannot catch this, since it compiles everywhere.
- **The daemon cross-checks for macOS; the app cannot.** `cargo check --target
  aarch64-apple-darwin -p orchd` works and is worth running after touching
  anything platform-shaped. `-p orchestrator-desktop` does *not*:
  `objc2-exception-helper` compiles Objective-C and needs a real macOS SDK, so it
  fails in `cc-rs` on Linux for reasons that say nothing about your code. That
  half is only answered by `check.yml` on the macos-14 runner.
- **Hooks are observers, not gatekeepers.** They answer immediately and finish
  their work detached, because Claude gives a hook one second and a dropped
  future silently loses the state change. Do not make a hook wait on anything.
- **`github_write.rs` will not resolve a thread, approve, merge or open a PR.**
  That is a design boundary, not a gap. Resolving is the comment author's button.
- **One pty exit, one observer.** `spawn::watch_session_exit` is the only thing
  that waits on a session's handle; it dispatches onward (a fix run's verdict goes
  to `fix_pr::settle`). A second `pty.wait()` on the same handle would work and
  then rot, because "is this over" would have two answers maintained apart.
- **Mutating a durable store carries its own write.** `automation`, `manual` and
  `stories` are changed through `Inner::with_automation` / `with_manual` /
  `with_stories`, which persist and log with the caller's own context. Do not
  reach for `store::save_*` at a call site — that is the shape where one site gets
  the fix and the others quietly do not.
- **You cannot self-review your way to a testable review thread — use the
  fixture.** `acknowledged()` (`forge/github.rs`) treats a thread whose last
  comment is yours as answered, so a PR you comment on yourself has nothing
  awaiting an answer, and `query_for` polls `author:@me` so the PR must still be
  yours. `mise run fixture` builds a throwaway private repo whose threads are
  posted by `github-actions[bot]`, which satisfies both; `docs/fixture-pr.md` has
  the why and the two GitHub behaviours that cost an afternoon. It does not cover
  `rerequest()` — a bot cannot be a requested reviewer. The resolve run itself is
  still unit-tested only and has never made a real round trip, so do not read a
  green suite as more than that.
- **`ORCHD_CONFIG_DIR` relocates every piece of durable state**, which is what
  makes a fixture daemon safe: config, `sessions.json`, `automation.json`,
  `hooks.json`, the instance lock and the findings block all follow it. Overriding
  `HOME` would do the same for free and is wrong — `claude` reads its credentials
  from there, so every spawned session would come up unauthenticated.
- **The app's modifier is ⌘ on macOS and Ctrl elsewhere** (`core.appMod`, from the
  `__ORCH_PLATFORM__` the daemon substitutes into the page — told, not sniffed).
  Worth knowing why rather than just that: on a Mac ⌘ never reaches the pty, so the
  Ctrl-shadows-the-terminal trade-off the layer contract agonises over is
  Linux-only. Two exceptions, both deliberate: session switching is `Ctrl+Tab`
  everywhere because ⌘Tab is the macOS app switcher and never arrives, and the
  legend's rows carry `MOD` placeholders resolved at boot — including in the
  descriptions, not just the chords, which is a bug that shipped once.
- **The config dir has a space in it on macOS**, and anything from it that reaches
  a shell must be quoted. `config_dir` is `~/Library/Application Support/orchd`
  there and `~/.config/orchd` elsewhere. The push guard's hook is a shell string
  (`type: "command"` — that is how `SessionStart` gets a pipe and `|| true`), so an
  unquoted path splits at the space and the hook runs nothing: the guard fails
  open and silently stops existing. Use `hooks::sh_quote`. Prompt-file paths are
  fine — they go into prose the agent reads, not a shell.
- **A fresh checkout the daemon points at needs Claude Code's workspace trust
  accepted once.** Until then `claude --worktree` refuses ("Workspace trust not
  yet accepted") and the spawned session exits instantly, leaving a workspace
  record for a worktree that was never created. Accept it in the dialog or set
  `hasTrustDialogAccepted` for that dir in `~/.claude.json`. The monorepo hides
  this by having been trusted long ago; `docs/fixture-pr.md` has it.
- **`claude --worktree` leaves a lock the daemon must clear at teardown.** Every
  worktree it cuts is `git worktree lock`ed, and the lock outlives the session the
  daemon kills — so a plain `git worktree remove` refuses it forever.
  `git::worktree_remove` clears a lock whose owning pid is dead and retries, still
  never `--force` and never a filesystem delete (preflight already proved the tree
  clean, so a stale lock is the only thing left to trip on). Do not "simplify" the
  retry away.
- **`POST /api/pr/:n/fix-pr` starts a run immediately.** No confirmation: the
  guard table refuses on authorship, a run already going, a busy branch and the
  concurrency cap, and *nothing else*. "The PR looks fine" is not a refusal,
  because a run is also how a PR that has fallen behind gets rebased. Easy to fire
  by accident while poking at the API.
- **Pushes are guarded.** `--force-with-lease` only, no `push -u`, no protected
  refs; `guards/push.py` denies the rest at `PreToolUse`. Never `git merge` into a
  branch here, rebase.
- **`cargo fmt` is not this repo's formatter.** There is no `rustfmt.toml` and
  `main` is not stable-rustfmt-clean, so running it out of habit reformats around
  27 files you never touched. Revert everything outside your own change before
  committing.
- **Git exports its own state into hooks and `--exec`, and one of the variables is
  a *relative* path.** Measured, not assumed: a pre-commit hook here runs with
  `GIT_INDEX_FILE=.git/index`, `GIT_PREFIX`, `GIT_AUTHOR_*` and `GIT_EXEC_PATH`
  set. Because that index path is relative, any `git` a hook runs from a
  *different* directory resolves it against that directory instead — which is how
  the e2e suite, run from the hook, died on
  `Unable to create '<newtree>/.git/index.lock': Not a directory`: a worktree's
  `.git` is a file. Anything spawning git from a hook must strip `GIT_*` first;
  `tools/e2e/harness.mjs` does, and that is the only reason the suite can run
  there.
- **`git rebase --exec 'cargo test'` did something unexplained.** It put test
  fixture commits into the repo and moved its HEAD. Recovered fully, and the
  mechanism was never confirmed — but the entry above is the strongest candidate
  yet, and it re-opens a hypothesis once written off: `--exec` sets the same
  variables, and the one that bites is `GIT_INDEX_FILE`, not the `GIT_DIR` that
  was tested and cleared. Still worth avoiding until somebody proves it.
- **A test that asserts on git's own error wording fails on an older git.** Git
  says "already used by worktree at" from 2.35 and "already checked out at"
  before it, and 2.34.1 is what some machines have — so the worktree guard and the
  swap test both failed for a reason that had nothing to do with the code.
  `git::refused_as_already_checked_out` matches either. The refusal is the
  invariant; its phrasing is not.
- **Driving the API by hand has four traps.** The header is `x-orch-token`
  (`Authorization: Bearer` is not read), the route is `/api/state` (`/api/snapshot`
  does not exist, and an unknown route answers `{}`, which reads exactly like an
  empty daemon), a POST needs an `Origin` matching the port or it is "bad origin",
  and the config key is `worktrees_subdir`. An unknown config key is ignored in
  silence, so `worktrees_dir` leaves the daemon managing `.claude/worktrees` and
  logging that it is "ignoring worktree outside the managed dir".

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
