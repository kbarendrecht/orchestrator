# Working in this repo

`orchd` is a Rust daemon plus a vanilla-JS SPA that hosts several Claude Code
sessions over one monorepo. **README.md** has the architecture and the module
map; **TODO.md** has what is open, the decisions worth revisiting, and the things
deliberately not built. Read TODO.md before proposing work: several obvious ideas
are already in there with the reason they were not done.

## Build and run

```
cargo check                         # the daemon
cargo test                          # 398 tests, all in-tree
mise run check-web                  # type-check the SPA + enforce its module graph
mise run e2e                        # 13 flows against a real daemon, ~50s
cargo run -p orchestrator-desktop   # the app, daemon embedded in-process
mise run shot                       # screenshot the running SPA (drives Chrome)
```

The agent binary is `claude`, installed by the `claude-code` mise tool so one
`mise up` in the monorepo covers both.

**One daemon at a time.** The lock is `~/.config/orchd/instance.pid`, not the
port, so a second instance refuses to start rather than fighting over
`sessions.json` and the hook settings file. Close the running app before
`cargo run`. The file is deliberately **left behind** at shutdown —
`instance::holder` decides by asking whether the pid is alive — so waiting for it
to disappear is waiting for something that never happens.

## One repo is the test, not the specification

`orchd` is developed against a single monorepo, and almost every fact in this file
was learned there. That repo is the only live test there is, so **keep it working**.
But its arrangement is one arrangement, and a rule derived from it is a guess about
every other repo until something else confirms it.

The distinction to hold: what **Claude Code** guarantees is a contract, what **a
repo** happens to do is a convention. They are easy to confuse because only one repo
is ever in front of you. Plenty of what looks structural here is that repo's choice:
worktrees under `.claude/worktrees`, a base of `upstream/develop`, a `WorktreeCreate`
hook that cuts from a fixed ref and prints the path, worktree *setup* hung off
`SessionStart` because Claude Code has no post-create event, `mise` as the task
runner, `origin` as a fork with `upstream` as the real remote. Another repo may do
none of that, and a daemon that assumes it will be wrong quietly.

The failure this is written for is not a crash. It is a feature that works
everywhere it was tried and means nothing elsewhere: a code path reached only by a
layout nobody else has, or a guard reasoning about a hook the next repo puts on a
different event. Where a behaviour depends on the repo rather than on the daemon,
say so where it is written, and make the daemon degrade rather than insist.
`worktree_setup` and `reviews`'s ejected script are both that shape, configured per
repo with a sensible default and no opinion at all when they are unset.

Two practical rules. Read the repo's own configuration rather than a remembered copy
of this one's, which is why `upstream_ref`, `worktrees_subdir` and `tracker` are
settings. And before writing "the repo's X does Y" into a comment, check whether you
mean *this* repo; if you do, name it.

## Things that will bite you

- **The SPA is compiled in.** Everything under `web/` is `include_str!`d, so a
  CSS or JS change is invisible until the daemon is rebuilt *and* restarted. No
  amount of reloading the page helps — and a stale process holding the port makes
  this worse, because you are then debugging a build from ten minutes ago.
  `pkill -x orchd` will not always do it: Linux truncates the process name to 15
  characters, so `orchestrator-desktop` needs killing by pid.
  **And never `pkill -f orchd`.** It matches the *agents* too: the daemon passes
  the vendored prompts on the command line, they contain the word, and `-f` reads
  the whole command line — so it killed two live Claude sessions along with the
  daemon. Kill by pid, or `pgrep -x orchestrator-de` for the app.
- **There is a pre-commit hook, and it needs enabling once per clone.**
  `git config core.hooksPath .githooks` — git will not let a repo point at its own
  hooks, so a fresh clone has none until you say this. It runs the SPA checks only
  when something they could fail on is staged: ~2s on a `web/` change, nothing at
  all on a docs commit. A Rust change also re-checks `web/snapshot.d.ts` and
  refuses if the committed copy no longer matches the structs.
  `--no-verify` is a fine thing to reach for mid-refactor; the real gate is
  `mise run check-web`.
  **Every fifth Rust-or-`tools/e2e/` commit it also runs the e2e flows**, ~45s
  instead of ~2s — a middle ground, since running them always teaches everybody
  `--no-verify` and never running them leaves those faults to CI. The counter is
  in `.git/`, only qualifying commits spend it, and a *failure does not reset it*
  so the next commit tries again rather than burying a break for four more.
  `E2E_EVERY=1` forces a run, `E2E_EVERY=0` turns it off.
- **Splitting one working tree into several commits has two traps, and neither
  fails loudly.** `git diff -U0` splits finely, but `git apply --cached
  --unidiff-zero` has no context to check against and *trusts the line numbers*:
  four `state.rs` insertions landed inside unrelated expressions, and five commits
  in a row did not compile while `HEAD` did, because a later whole-file commit
  quietly repaired them. Use context hunks (a mismatch then fails instead of
  landing somewhere else) or write the whole file per stage, and **check out each
  commit and `cargo check` it** — a throwaway `git worktree add --detach` is the
  cheap way. The other trap is the hook: it regenerates `web/snapshot.d.ts` from
  the *working tree*, which holds every change, so any Rust commit in a split
  demands the final file and can only pass on the last one. `--no-verify` for the
  split, then `mise run check-web` on the end state. Worth knowing generally: the
  hook reads the working tree, so it never validates an intermediate commit at all.
- **Inserting a test can unregister the one next to it.** An anchor on
  `fn other_test() {` puts your test *between* that test's `#[test]` and its `fn`,
  which leaves yours with two attributes and its neighbour with none — so it stops
  running, and the count barely moves because yours now registers twice.
  `swapping_exchanges_two_branches_and_is_its_own_inverse` sat unregistered in a
  pushed commit that way. Anchor after the previous test's closing brace, and read
  the test count.
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
- **A transcript is keyed by session uuid, so two sessions in one directory do not
  interleave.** Said here because the opposite was written into two code comments
  and a domain finding, and it justified a guard that refused reviewing any PR
  whose worktree you had torn down. `transcript_file`, `find_transcript` and
  `archive`'s copy all key on the uuid; sharing a directory slug gets you two
  files. The real hazard of reusing a worktree name is elsewhere — a resume landing
  in a tree cut again for something else — and `worktree::branch_drift` says so
  rather than refusing.
- **Session names come from an undocumented field.** `store::ai_title` tails the
  transcript for `{"type":"ai-title","aiTitle":…}`. It degrades to the workspace
  name rather than failing, so a rail that suddenly reads `dfafdf` everywhere
  means Claude Code changed the format. Identical titles across unrelated sessions
  are Claude Code's doing, not a bug here: one `ai-title` string turned up in 8
  transcripts across 3 repositories, each correctly attributed to its own
  sessionId. The reader is right; the file says that.
- **Live findings go to a gitignored `daemon.log`, only when dogfooding.**
  `findings::write_log` overwrites `daemon.log` with what the daemon can see
  right now, each poll — but `start_findings_log` starts at all only when the
  repo being managed *is* this source tree (`findings::dogfood_log` compares
  `main_checkout` to the build-time `CARGO_MANIFEST_DIR`), or when `log_path` is
  set. So a daemon pointed at any other repo — or a throwaway build — writes
  nothing, and never dirties a tracked file. This replaced an older block spliced
  into `TODO.md` at the build-time path, which churned this repo from every build.
- **A session's environment is not the shell's, and the gap is invisible.** The
  daemon's environment is whatever started it; from a desktop launcher that is the
  systemd user manager's, which holds no checkout's variables. So a `.mcp.json`
  header spelled `Bearer ${SHORTCUT_API_TOKEN}` went out as literal text and the
  server answered 401 — while the same session started by typing `claude` in that
  checkout worked, because `mise activate` exports at a shell prompt and an app has
  no prompt. That is why the terminal is the worst place to reproduce this.
  `config::session_env` now asks the tool itself (`src/env_source/`, `mise` by
  default, `direnv` beside it, `none` to turn it off), per spawn, in the session's
  own cwd. Two things it will not do: it never fails a spawn (a missing variable is
  degraded, a refused spawn is lost), and it cannot trust a config for you — mise
  refuses an untrusted `mise.toml`, a fresh worktree is a fresh path, and the only
  sign is one warning in the log. Put `mise trust` in `worktree_setup` if that
  bites.
- **Shelling out to coreutils is the other portability trap.** The review queue
  ran its command under `timeout`, which is GNU and not on a Mac, so it failed at
  the spawn and the pane blamed the review command for a missing binary it never
  named. `proc::run_bounded` enforces the deadline in Rust instead — one
  bounded-exec primitive (own process group, SIGKILL the group, pipes drained on
  threads), used by both the review queue and `worktree_setup`. Every other
  command the daemon spawns is POSIX (`git`, `curl`, `gh`, `ps`, `which`, `kill`) —
  keep it that way, and check `command -v` before reaching for a GNU flag.
- **`WorktreeCreate` is not a setup hook. It *is* the creation, and a daemon-cut
  worktree therefore never fires one.** Claude Code's own error text says what the
  event is for: worktree isolation "with other VCS systems". The hook reads the
  request on stdin, creates the tree by whatever means it likes, and prints the
  path, which Claude Code then validates (absolute, no dot segments, a real
  directory, not a symlink). So it cannot be re-run over a tree that already exists,
  and it cannot be asked to honour a base the daemon chose: the monorepo's copy
  hardcodes `upstream/develop`, which is exactly wrong for a PR worktree pinned to a
  head ref or a fork cut from its parent. That is why `ensure_pr_worktree` and the
  fork path cut their own.
  **The post-create seam is `SessionStart`**, because Claude Code has no post-create
  worktree event, and that one *does* fire for a daemon-cut tree: the daemon's
  `--settings` merges with the repo's rather than replacing it. Measured, after a
  session spent believing the opposite: three daemon-cut PR worktrees all had
  `remote.pushDefault`, the shared `.plan` and every symlink, because the monorepo
  hangs its `worktree-link` there. Do not "fix" a gap here without checking which
  event the repo in front of you actually uses.
  `worktree_setup` remains for a repo that puts real setup inside `WorktreeCreate`,
  where a daemon-cut tree would genuinely miss it: `spawn::run_worktree_setup` runs
  the configured command in each daemon-cut worktree, before the session, non-fatal.
  Do **not** add it to the `claude --worktree` arm, where the repo's own hook already
  ran. A relative script path resolves against `main_checkout`; cwd is the worktree.
  The matching teardown event is `WorktreeRemove`, which the daemon also does not
  fire; `git::worktree_remove` does its own thing.
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
- **A missing `cwd` is not an error to `portable-pty` — it is `$HOME`.**
  `CommandBuilder::as_command` filters the cwd on `is_dir()` and falls back to the
  home directory, so a session aimed at a worktree that no longer exists does not
  fail: it starts in `~` and runs there. A fix-pr run did exactly that, and the
  only thing that stopped it was Claude Code's workspace-trust prompt for a
  directory nobody had chosen. `PtyHandle::spawn` now refuses a `cwd` that is not a
  directory, which is the one place every session, process and shell goes through.
  The record that pointed there is the other half. A workspace record outlives its
  directory — `claude --worktree` removes its own tree when that session ends, and
  only `worktree::teardown` ever drops a record — so the PR flows were handed a
  name whose tree was gone. **The repair is to rebuild it where it stood**, not to
  cut a second tree or to prune the record: the session owns that directory,
  because transcripts are keyed by it. A resume already did that
  (`api::revive` → `worktree::revive`); `ensure_pr_worktree` now does it too, via
  `recorded_worktree_for`, which hands back the recorded path precisely so the tree
  can be cut again at it. `worktree_holding` deliberately ignores whether the
  directory exists — a live session whose tree was deleted still holds its branch,
  and `branch_busy` must keep saying so.
- **Hooks are observers, not gatekeepers.** They answer immediately and finish
  their work detached, because Claude gives a hook one second and a dropped
  future silently loses the state change. Do not make a hook wait on anything.
- **A hook for a session the daemon has not recorded yet is dropped in silence.**
  `spawn_session` inserts the record *after* `PtyHandle::spawn`, so an agent quick
  enough to fire `UserPromptSubmit` in that window loses it — and `Stop` arriving
  after the insert then leaves a session at `your_turn` with `had_a_turn` false, a
  conversation the rail will not offer to fork or resume. Real Claude Code takes
  human-scale seconds to a first prompt and never lands there; anything scripted
  does, which is why the e2e agent waits to see itself in `sessions.json` before
  speaking.
- **`github_write.rs` will not resolve a thread, approve, merge or open a PR.**
  That is a design boundary, not a gap. Resolving is the comment author's button.
  Which means **`is_resolved` can never stand for "handled"**: the daemon never
  sets it, so every thread it has ever answered is still unresolved. A re-request
  guard derived from `!is_resolved` shipped and could never fire — read
  `post::rerequest_all`, and ask "did *we* settle it" instead. The neighbouring
  trap is `answerable`, which flips the moment you post: it answers "is anyone
  owed a reply", so it is only "who reviewed" on a fetch taken *before* the
  posting.
- **Claude Code pins worktree isolation in the transcript, and the daemon clears it
  by writing to that same file.** Every turn re-appends a `worktree-state` record
  (`worktreePath`, `worktreeName`, `hookBased: true`), and on resume its own hook
  refuses any git command aimed outside that original worktree — *including the
  tree the daemon just moved it into*. A swap that worked perfectly (branch, files,
  record, conversation all correct) left the agent unable to run `git status` on
  its own work: "This session is isolated in the worktree …, but this command
  redirects git to the shared checkout".

  It used to say here that the daemon cannot clear that from outside, since
  `ExitWorktree` is the agent's own tool, so `api::arrival_notice` asked the agent
  to call it. **Both halves of that were wrong, and a conversation paid for it for
  two days**: it went on editing a worktree that had since been cut again for a
  different branch while its own branch sat in main, taking the bare isolation
  refusal sixteen times, and it never called `ExitWorktree` once.

  - **The pin is a running value, not a header.** The *last* `worktree-state`
    record wins, and letting go is one line —
    `{"type":"worktree-state","worktreeSession":null,"sessionId":…}`, preceded by
    `{"type":"relocated","relocatedCwd":…}`. Measured across 395 transcripts: 128
    end exactly that way. So `store::clear_worktree_pin` appends what Claude Code
    would have written, and `spawn_session` calls it on resume whenever the pin
    disagrees with the cwd. **Only ever between processes** — the old pty dead, the
    new one not started — because a live agent is appending to that file too.
  - **Asking was delivered on the wrong tools.** The notice rides `PreToolUse`,
    which was registered `Edit|Write`, and the isolation bites on *git* — Bash. The
    explanation sat queued behind a write the session never made. `PostToolUse` had
    been widened off that same matcher for the same reason; `PreToolUse` now matches
    every tool.

  `arrival_notice` stays, because it says in words what the record only implies, but
  it is no longer the mechanism.
- **Opening a PR in a worktree can move main's branch out from under you** — by
  design, since `park_main` will not carry uncommitted work and a branch stuck in
  main makes every PR flow for it impossible. If main holds that PR's own branch,
  `ensure_pr_worktree` moves branch *and* work into the tree it was about to cut
  and puts main back on base, logging that it did. Only a live session in main is
  still refused. It is not a read-only flow with respect to main.
- **One pty exit, one observer.** `spawn::watch_session_exit` is the only thing
  that waits on a session's handle; it dispatches onward (a fix run's verdict goes
  to `fix_pr::settle`). A second `pty.wait()` on the same handle would work and
  then rot, because "is this over" would have two answers maintained apart.
- **Main's claim belongs to the session record, and a relocation reuses the id.**
  `claim_main` runs before anything is created so a refusal costs no worktree and
  no pty — but until the record is installed the map still describes the *outgoing*
  session, whose exit watcher is entitled to settle it, and `release_main` keys on
  the id. So the claim the incoming session just took gets handed back, and main
  holds a live agent with **no occupant recorded** — the value `switch_main_to_pr`
  reads before moving the checkout. `spawn::spawn_session` closes the window with
  `reclaim_main` after the insert. Reproduced one run in four by the two-way swap
  e2e flow, and invisible to every unit test.
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
  from there, so every spawned session would come up unauthenticated. The one
  exception is `mise run e2e`, where the agent is a fake with no credentials to
  lose, so relocating `HOME` is what keeps transcripts out of your
  `~/.claude/projects`.
- **`mise run e2e` needs no product change, because the agent is a PATH lookup.**
  The daemon spawns `CommandBuilder::new("claude")` and reaches GitHub only through
  `Command::new("curl")`, so a shim earlier on PATH substitutes either without the
  daemon knowing. Everything else in those flows is real — real worktrees, real
  branch moves, real `stash create` carries, real locks, the real API, and the hooks
  read out of the settings file the daemon itself wrote, so a change to
  `hooks::write_settings` changes what they exercise instead of passing them by.
  Read `docs/e2e.md` before adding one: it has the sandbox options, why every wait
  is a condition rather than a sleep, and the limits (no SPA, no real round trip to
  GitHub, nothing about what a fix run *does*). What they buy is the class of fault
  unit tests structurally cannot see: the first full run turned up a `claim_main`
  race, and driving them from the hook turned up what git hands a hook.
- **The keyboard map has a contract, and it is the reason the next binding is
  obvious.** Above the keydown handler in `web/app.js`: **bare keys belong to the
  open overlay, `Ctrl` is the whole app, `Esc` dismisses the topmost thing.** The
  whole `Alt` layer was deleted to get here — every action it held already had a
  `Ctrl` spelling, and two vocabularies for one set of verbs is what made the map
  unpredictable. Do not reintroduce `Alt` to dodge a collision; `Ctrl+Shift` is the
  escape hatch. Plain `Ctrl+<letter>` shadows the pty, so `Ctrl+Shift+…` is the
  default and a plain letter is taken only where the idiom earns it. The legend
  (`Ctrl+Shift+?`) is hand-written HTML and is the one thing here that can silently
  drift from the code.
- **Measure a release build, or do not quote the number.** "orchd uses 76 MB" was a
  `cargo run` debug build — 113 MB of binary against release's 11 MB, nearly all
  paged-in debug text. Release, idle, polling: 7.6 MB RSS and **1.1 MB** of heap.
  Two performance suspects were chased on the strength of the wrong figure. For the
  same reason `web/js/term.js` pins `scrollback: 2000`: xterm holds each line as a
  `Uint32Array` of `cols * 3`, so depth costs process memory whether or not a
  terminal paints (10000 lines cost +36.7 MB against 2000's +13.3 MB) — and the
  daemon's ring buffer only replays ~3600 lines anyway, so a deeper buffer was
  never durable. JS-heap metrics are useless here: CDP reported 0.9 MB for 9000
  lines that cost ~23 MB, because typed-array stores are external memory.
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
  guard table refuses on authorship — *can you push to the head repo*, read from
  `headRepository.viewerPermission`, not whose name is on it — a run already going,
  a busy branch and the concurrency cap, and *nothing else*. "The PR looks fine" is not a refusal,
  because a run is also how a PR that has fallen behind gets rebased. Easy to fire
  by accident while poking at the API.
- **Pushes are guarded, by two halves that must agree.** `src/guard.rs` holds the
  rules; `orch guard push` runs them as a `PreToolUse` hook on the agent's Bash,
  and `git::push_with_lease` re-states the base-branch rule because a *daemon*
  push never passes through a hook. Two rules only: no lease-less `--force`, and
  no push to the base branch, which comes from `upstream_ref` rather than a list
  of likely names. Never `git merge` into a branch here, rebase.
  It is a **mistake-catcher, not a control** — Bash only, so `gh` or a script the
  agent writes goes around it. Do not write docs that claim otherwise; the README
  did, and that is the kind of sentence that earns misplaced trust.
  The Python script this replaced failed open when `python3` was missing, and
  matched refspecs by spelling — `git push origin main` refused while
  `git push origin HEAD:refs/heads/main` passed.
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
- **A route an agent calls needs a line in `is_ask_route`, and forgetting it fails
  as `bad origin`.** The vendored prompts curl with no `Origin` and carry the
  session's ask token, not the app token — so a session route missing from that
  list is refused twice: the Origin check has no arm for it, and `needs_token`
  then wants a token the agent is deliberately not given.
  `…/thread/:id/committed` shipped like that, which made the resolve run's central
  seam unreachable by its only caller while every unit test passed. Add the
  suffix, and the test in `api::tests` that walks the paths the prompts really
  call.
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
