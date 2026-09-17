# Working in this repo

`orchd` is a Rust daemon plus a vanilla-JS SPA that hosts several Claude Code
sessions over one monorepo. **README.md** has the architecture and the module
map; **TODO.md** has what is open. Read it before proposing work: several obvious
ideas are already in there, with what has been tried and why the shape is what it
is.

This file is the rules. **`docs/traps/` is what each one cost** — the same entries
in the same order, with the incident, the measurement and the code behind them.
Read the group before working in it.

## Tools, not rules

**A rule nothing runs is a rule somebody will break, and the person who breaks it
will be whoever read this file longest ago.** So the standing move here is to turn
a rule into something that fails a build. Nearly every entry in *Things that will
bite you* was a rule first and cost a session before it became a check: the SPA's
module graph is a DAG because `dependency-cruiser` says so, `snapshot.d.ts` cannot
drift because `check-web` regenerates and diffs it, a fixture identity cannot be
committed because the pre-commit hook reads `git var`, `confirm()` cannot come
back because ESLint refuses the name, and a doc link cannot rot because
`check-docs` denies it — and, since the traps became an index over `docs/traps/`,
that the index and the files still carry the same entries.

Two moves do most of the work: **deny rather than warn** (a warning is a rule
again — CI's `-D warnings` is what makes clippy a gate), and **regenerate, then
diff against the committed copy** (`snapshot.d.ts`, `THIRD-PARTY-RUST.md`).

**The bar is on refusing a tool, not on adopting one.** Two refusals do not count,
because both were used here and both were wrong:

- *"Nothing has gone wrong yet."* A gate's whole job is the failure that has not
  happened. An advisory against a crate in the bundle arrives without a commit; a
  `println!` in the library is invisible until somebody launches from a desktop
  entry and has no terminal. If the failure would be expensive and silent, that is
  the argument for the tool, not against it.
- *"Not evaluated."* That is a sentence to delete by running the thing. Every lint
  below took one command to measure. `await_holding_lock` was written off as
  unevaluated and turned out to be zero sites — free, and it stays zero.

A refusal has to name what was **measured**. Two that qualify, so nobody
re-litigates them from the doctrine alone:

- `clippy::indexing_slicing` — 59 sites, and about a dozen are
  `body["key"] = json!(…)` on a `serde_json::Value`, where `IndexMut` inserts and
  cannot panic. A lint that fires on a safe, idiomatic API is a lint people learn
  to `#[allow]`, and the habit then covers the real ones.
  **The pass over the rest is done, and it produced tests rather than `.get()`.**
  Twenty-nine of those sites — half the workspace — are in `diff.rs`, and every
  one is an LCS table walk or a tokenizer byte offset: the shape where `[i]` *is*
  the algorithm, and `.get(i).unwrap_or(&0)` would turn an indexing bug into a
  silently wrong highlight rather than a panic. Each was read and is provably in
  range, and reading is not evidence — so
  `the_word_diff_survives_adversarial_lines` and the two beside it drive a
  thousand generated pairs of the strings that break byte-wise handling
  (four-byte encodings, a combining mark, a zero-width joiner) and assert the
  contract `Row::words` promises. **Checked against deliberate breakage**, which
  is what says the tests have any power: dropping the tokenizer's char-boundary
  advance and indexing one past the LCS walk each fail them.
  One thing the pass corrected: this entry used to send the reader to "the open
  crash report (#14, in `diff.rs`)". **There is no known panic in `diff.rs`, and
  never was one** — which is the half of that claim this lint turns on.
- `cargo nextest` — one process per test is the right shape for a suite whose
  fixtures are process-global, and it is **3x slower here** (50s against 16s,
  because most of these tests spawn git) *and* the generated-bindings tests race
  each other once they are separate processes, which needs a `test-group` to
  serialise. Measured, both numbers. Revisit if the suite grows a test that hangs.
- The Rust module graph is **not** on this list: it is a ratchet now
  (`mise run check-modules`), and the entry below says what it holds and why it
  is not a gate.
- **ast-grep** — its useful rules here are expressible in tools already running.
  "A host route must go through `callHost`" is an ESLint `no-restricted-syntax`
  selector and catches both spellings; the dialog and blocking-call rules were
  already covered. A second query language earns its place when something needs
  checking that neither ESLint nor clippy can see.
- **Pixel screenshots** — see `mise run page-check`: the churn lands on the
  commits that are *supposed* to change the page, and the baselines would be
  Chrome's while the app ships WebKitGTK and WKWebView.
- **`localStorage` only in `core.js`** — 28 uses across five modules, and each is
  that pane's own remembered preference. Centralising them buys one file to read
  and costs a layer of indirection on every setting. The hazard CLAUDE.md
  actually names — a key that does not carry its checkout — is not something a
  lint can see.
- **Anything only a Mac can check** — and this is smaller than it was. *Running* the
  app is a tool now: `mise run app-check` opens a checkout in the built binary, makes
  a session and restarts it, on macos-14 in `check` and against the bundle in
  `release`. That was written after #16 and #17 shipped a week apart, both invisible
  to every gate here, because none of them ran the thing. What is left to a written
  rule is what the *pixels* look like: the renderer, the window chrome and the
  native dialogs. `mise run renderer-check` and a real machine before a tag are that
  gate. Clicking the window is possible on a runner and deliberately not used — the
  entry in `docs/traps/macos.md` has the measurements.

What is left is cost, and cost is negotiable rather than disqualifying. The hook
runs only what the staged files could break, because a hook that is slow on a docs
commit teaches everybody `--no-verify`. A weekly job lives in `health.yml` rather
than `check.yml`, because "go and read an advisory" and "your commit is broken"
are different messages and mixing them teaches people to ignore the one that
matters.

When a rule truly cannot become a tool, say so where the rule is written, and say
what it costs.

## Build and run

```
cargo check --workspace             # the daemon and orchd-base
cargo test --workspace              # the whole suite, all in-tree
cargo fmt --all                     # the formatter, gated in CI and the hook
cargo clippy --workspace --all-targets   # what CI lints with, and it denies warnings
mise run check-web                  # type-check and lint the SPA + enforce its module graph
mise run check-docs                 # the doc comments' links, denied as warnings
mise run check-deps                 # advisories, licences, unused crates, spelling
mise run check-modules              # the daemon's module graph, held no worse
mise run check-ship                 # every binary the release must pack
mise run app-check                  # drive the real app: a session survives a restart
mise run page-check                 # what the rendered page must never show
mise run notices                    # regenerate THIRD-PARTY-RUST.md
mise run e2e                        # 27 flows against a real daemon
mise run deflake                    # each flow 8x, to name the flaky ones
cargo run -p orchestrator-desktop   # the app, daemon embedded in-process
mise run shot                       # screenshot the running SPA (drives Chrome)
mise run release                    # bump, wait for CI, tag and push
```

**mise carries the toolchain and every task, and nothing in CI uses it.** `[tools]`
has `rust`, `node`, `claude-code` and `gh` — the agent binary is `claude`, under
the `claude-code` name so one `mise up` in the monorepo covers both, and `gh` is
there because `mise run release` and `mise run fixture` refuse without it and it
had been working off a *global* config. `mise run deps` installs
`tools/node_modules` and every task that needs it depends on that, with
`sources`/`outputs` so it is skipped when nothing changed — so a fresh clone runs
`mise run e2e` rather than failing on a missing module. The one dependency
`[tools]` cannot carry is **Chrome**: `tools/` pins `playwright-core` to the
version whose WebKit build is on disk, and playwright drives a browser it does not
install.

A task is inline when it can be and a stanza pointing at `tools/` when it cannot.
`release` and `check-web` are inline shell; the rest point at scripts, and have to
— `shot.mjs` resolves `playwright-core` out of `tools/node_modules`, `e2e/run.mjs`
imports `./harness.mjs` and reads `flows/`, and `fixture-pr.mjs` is 547 lines of
GitHub setup. Two things bite when writing an inline one. **Arguments are appended
to the last line**, not bound to `$1`, so a script that wants them ends in `main
"$@"` and mise's echo of the command then looks wrong while being right. And it is
POSIX `sh` on whatever machine cuts the release, so no `sed -i` (that flag takes an
argument on a Mac and not on Linux) and no other GNU-only flag — write to a temp
file and `mv`, which is what `release` does.

**`cargo clippy --workspace --all-targets` is a gate and was not in this list.** CI
runs it with warnings denied, so a lint that is a warning here is a red build there.
Both flags matter. `--all-targets` matters because the eight that caught this were
all in test code that a plain `cargo clippy` never compiles. `--workspace` matters
because without it cargo lints the package in the current directory and nothing
else — so `desktop/` goes unlinted, the crate with the window, the launcher and
the restart handoff in it. The root manifest used to be the `orchd` package as
well, which made that default quietly wrong for every tool that has one; it is
the workspace and nothing else now. Green tests, `check-web` and e2e are
not enough on their own.

**One daemon per checkout.** The lock is an `flock` on
`<config dir>/instance.pid`, not the port, so a second instance refuses to
start rather than fighting over `sessions.json` and the hook settings file. Close
the running app before `cargo run`.
**`<config dir>` is per checkout for a hosted child** — `checkouts/<leaf>-<hash>`,
derived from the checkout — so the lock guards the noun it always claimed to. Two
daemons for one checkout are refused; two for two checkouts never meet, which is
what makes several checkouts possible. A refused start still rotates that
checkout's `orchd.log` before it is refused, because logging is installed before the
lock is taken: a failed launch costs you one log generation. The file is deliberately **left behind** — it
is what the lock is taken *on*, and removing it would let a second daemon lock a
fresh file while this one holds the old inode — so waiting for it to disappear is
waiting for something that never happens.

The kernel owns the release: it happens when the process ends however it ends, so
a crash leaves nothing to clean up and there is no stale-file path. It used to be
`create_new` plus a `ps`-based `holder` check, and both halves were wrong. The
stale path was the *common* one (`process::exit` runs no destructors), and two
launches inside it could both unlink and both create — two daemons, the exact
thing the lock exists to stop. And guessing from a command line meant a recycled
pid belonging to `vim ~/orchestrator/x` read as a live holder and wedged the app
out of starting.

**But the release is not instant, and the reason is `fork`.** A descendant holds a
copy of the lock fd from the `fork` until its own `exec` closes it, so a daemon
killed while `reconcile_all` is spawning git four wide leaves the lock held for a
few milliseconds *after it has been reaped*. The host restarts a dead checkout's
daemon exactly once, so those milliseconds spent that checkout's only recovery and
left it down — measured on a 2-core CI runner, refused 250ms in, naming the pid it
had just reaped. `instance::PATIENCE` is two seconds of retry, and it costs a real
refusal nothing: a second instance holds the lock for its whole life.

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
`worktree_setup` and `reviews_command` are both that shape, configured per repo
with a sensible default and no opinion at all when they are unset. **The review
queue's default is the daemon's own now, and that is the lesson rather than a
detail**: it was a `node` script the daemon ejected, which could not be a default,
because a launcher-started daemon's PATH is not a shell's and the node it found
there was too old to run it. A default may only depend on what the daemon already
needs — here the GitHub token and `curl` the PR pane runs on.

Two practical rules. Read the repo's own configuration rather than a remembered copy
of this one's, which is why `upstream_ref`, `worktrees_subdir` and `tracker` are
settings. And before writing "the repo's X does Y" into a comment, check whether you
mean *this* repo; if you do, name it.

## Things that will bite you

Every one of these cost something. The rule is here; what it cost is in
`docs/traps/`, one file per group, in the same order.

**This was 1,537 lines of it, inline.** That is the whole design record and it
has plainly stopped regressions — but it is also read in full at the start of
every session, before a single source file is opened, and most of what it holds
is the *history* of a rule rather than the rule. So the histories moved and the
rules stayed. Nothing was deleted: every word is under `docs/traps/`, and the
heading above each one is the line below.

**A list written twice is a list that drifts**, and this one drifts silently — an
entry added to one side is simply missing from the other, and the only symptom is
a reader who never learns the trap. So `mise run check-docs` holds the two
together: same entries, same order, same groups.

### The host, its children, and their state

[docs/traps/host.md](docs/traps/host.md)

- A checkout's daemon is a child process, and `crates/orchd-base/src/child.rs` is the protocol.
- A checkout's durable state lives in its own directory, and `ORCHD_CONFIG_DIR` is how it gets there.
- A migration that copies a list of files is a migration that loses the file nobody listed.
- The app is the host, and every checkout is a child `orchd`.
- First run is a screen, not a second application.
- The page is served by `host.rs`, not by the daemon.
- A child that will not start leaves one sentence, and it used to name nothing.
- A deadline checked after a blocking read is not a deadline.

### The gates, and what each one caught

[docs/traps/gates.md](docs/traps/gates.md)

- The SPA is compiled in.
- There is a pre-commit hook, and it needs enabling once per clone.
- Splitting one working tree into several commits has two traps, and neither fails loudly.
- Inserting a test can unregister the one next to it.
- `mise run check-web` is the SPA's gate, and it bites.
- ESLint answers what `tsc` structurally cannot: the promise nobody awaited.
- The SPA type-checks under `strict`, and getting there found two bugs.
- `catch (e)` gives you `unknown`, and `core.reason(e)` is the one answer.
- `mise run page-check` asserts what the page must never *show*.
- `mise run check-ship` asserts what a release would *pack*, which no test can see.
- Type-checking found bugs clicking around did not.
- A panic is denied where it can take the daemon down, and `clippy.toml` is why that became affordable.
- `health.yml` runs `cargo deny`, `cargo about`, `cargo machete`, `typos` and `zizmor` — on every push and weekly, in its own workflow.
- The doc comments are checked now, and they were not.
- `ctl(id)` is the one deliberate `any` in the SPA.

### The SPA, the webview, and the two module graphs

[docs/traps/spa.md](docs/traps/spa.md)

- ES modules work in the real webview — measured, not assumed.
- The page holds one snapshot and one socket per checkout, and `snap` is the active one.
- A page served by a host is cross-origin to every child daemon.
- `orchd --host <checkout>…` is how the multi-checkout page gets driven without a screen.
- The SPA is a module graph, not a file.
- The daemon's module graph is a DAG now, and `mise run check-modules` refuses a cycle.
- A run's end is published, not dispatched by name.
- The module graph is a DAG, and it was made one on purpose.
- Each module needs a line in `module()` in `host.rs` and a rebuild.
- A pane's paint signature is the one guard with no check over half of it.
- `snap` is a live binding, and only `receive()` may replace it.
- Never rewrite an identifier across an SPA file with a regex.
- The app is WebKitGTK, not Chrome.
- The DOM renderer is a WebKitGTK workaround, and only WebKitGTK's.
- Slow trackpad scroll in an agent pane is xterm's wheel maths, not the renderer.
- A window drag is the one call in this app that can abort the process, and it is guarded in two places.
- `window.confirm`, `window.prompt` and `window.alert` do nothing in this app on macOS.
- A socket may freeze which checkout it serves, never where that checkout is.
- HTML5 drag-and-drop and the native drag destination cannot both be live on one webview.
- The splash is replaced, never pushed, and `Backspace` is refused outside a text field.

### Claude Code, and what it guarantees

[docs/traps/claude-code.md](docs/traps/claude-code.md)

- A skill reaches a session through `--plugin-dir`, and that flag is per *invocation*.
- The daemon's session id is Claude's session id.
- Transcript paths slug both `/` and `.`.
- A transcript is keyed by session uuid, so two sessions in one directory do not interleave.
- A tracker is three config fields, and there is one way to write it.
- Session names come from an undocumented field.
- There is no findings log any more.
- Stopping a session is `kill_gracefully`, on every path.
- The open screen merges into `config.json`, never replaces it.
- A session's environment is not the shell's, and the gap is invisible.

### Portability, and the processes the daemon spawns

[docs/traps/portability.md](docs/traps/portability.md)

- No `std::process::Command` and no `std::fs` on a tokio worker.
- Shelling out to coreutils is the other portability trap.
- `WorktreeCreate` is not a setup hook. It *is* the creation, and a daemon-cut worktree therefore never fires one.
- Paths are resolved at one boundary, and comparing across it silently fails.
- A `/proc` read is a portability bug that compiles.
- The daemon cross-checks for macOS; the app cannot.
- A mise install path is version-pinned, so never write one into a file that outlives the process.
- A missing `cwd` is not an error to `portable-pty` — it is `$HOME`.

### The daemon's machinery: hooks, worktrees, main, the stores

[docs/traps/daemon.md](docs/traps/daemon.md)

- Hooks are observers, not gatekeepers.
- Every hook finds its session, and the window where one did not is closed.
- `forge/github_write.rs` will not resolve a thread, approve, merge or open a PR.
- The daemon no longer asks Claude Code to cut a worktree, and the isolation pin is why.
- Claude Code pins worktree isolation in the transcript, and the daemon clears it by writing to that same file.
- Opening a PR in a worktree can move main's branch out from under you
- Main goes back to base when the last session leaves it, and it takes the base back to do so.
- The drawer can hand a pane's output to the session, and the daemon owns *when*.
- A config this build cannot read is repaired on disk, not tolerated in memory.
- The changed-files pane's git verbs are drawn from `git status`, not from its own list.
- The rebase button banks a dirty tree, and never on `refs/stash`.
- One pty exit, one observer.
- Main's claim belongs to the session record, and a relocation reuses the id.
- Mutating a durable store carries its own write, and the compiler now says so.
- You cannot self-review your way to a testable review thread — use the fixture.
- The host's own file is `host.json`, and a hosted child must not write the host's files.
- A resume rebuilds a session's environment, so anything the daemon put there has to be re-handed.
- A spare worktree is a workspace with no session, and that is the shape the reaper hunts.
- Two `git worktree add`s at once fail on the config lock, and the spare pool made that reachable.

### The e2e flows

[docs/traps/e2e.md](docs/traps/e2e.md)

- An e2e flow must make idleness a condition, not an assumption.
- `mise run e2e` needs no product change, because the agent is a PATH lookup.
- GitHub is two programs, and the write half is `gh`.
- A flow's own git races the daemon's, and the full suite hides it.

### The UI's contracts

[docs/traps/ui.md](docs/traps/ui.md)

- The keyboard map has a contract, and it is the reason the next binding is obvious.
- The rail's `handle` button starts a pane, not the overlay.
- An ask the review overlay does not own must still be answerable in the box.
- Two panels dock at the bottom of the terminal, and they must not sit on each other.

### Performance, measured

[docs/traps/performance.md](docs/traps/performance.md)

- A start is a pile of child processes, and that is why it is slow somewhere else.
- A keystroke is one small frame, so the served sockets set `TCP_NODELAY`.
- `mise env` per spawn is a decision, not an oversight.
- A resume rebuilds the record from `spawn::Carried`, and anything not named there is thrown away.
- Every spawner records the session's branch, and the swap depends on it.
- Nothing may read `Tree` without asking whether it has been measured.
- A launcher-started app has no stdout, so it used to leave no log at all.
- The page's own boot timing is not visible from Rust.
- Measure a release build, or do not quote the number.
- Creating a worktree is 4.7 seconds; claiming a pre-cut one is 84 milliseconds.

### macOS, and the tooling around the build

[docs/traps/macos.md](docs/traps/macos.md)

- The app's modifier is ⌘ on macOS and Ctrl elsewhere
- The config dir has a space in it on macOS
- A fresh checkout the daemon points at needs Claude Code's workspace trust accepted once.
- `claude --worktree` leaves a lock the daemon must clear at teardown.
- `POST /api/pr/:n/fix-pr` starts a run immediately.
- Pushes are guarded, by two halves that must agree.
- A macOS runner can open the window, and it still cannot be clicked by name.
- A `rust-toolchain.toml` is a no-op here, and silently.
- A spawned `sysctl` answers for the child, not for this process.
- A shell-script `CFBundleExecutable` has no architecture, so the plist must declare one.

### The crates, and what a move breaks

[docs/traps/crates.md](docs/traps/crates.md)

- Four crates, all under `crates/`, and the root is the workspace and nothing else.
- `orchd` is a library only. `crates/orchd-serve` is the daemon
- The words are `host`, `checkout` and `session`, and `repository` is reserved.
- `crates/orchd-repo` is what a checkout is
- This is a workspace, and `crates/orchd-base` is the first crate out.
- `git` is a directory, and the split was the banners.
- `cargo fmt` is the formatter now, gated in CI and the hook.

### git, and driving the API by hand

[docs/traps/git.md](docs/traps/git.md)

- Git exports its own state into hooks and `--exec`, and one of the variables is a *relative* path.
- `git rebase --exec 'cargo test'` did something unexplained.
- A test that asserts on git's own error wording fails on an older git.
- A route an agent calls needs a line in `is_ask_route`, and forgetting it fails as `bad origin`.
- Driving the API by hand has four traps.
- A markdown heading in a tag message is a comment to git.

## Releases

`mise run release` — it bumps the version, **waits for `check` to go green on the
commit you are on**, then commits, tags and pushes. `mise run release -- 2027.1.1`
names a version instead of bumping the last component; `--dry-run` stops before
anything is written.

**The waiting is the whole point, and it is why the task exists.** `check` is the
only thing that runs the test suite on **macOS**, and the release workflow runs it
again *after* the tag exists — so a tag pushed before `check` answers is a tag
that may publish nothing. That has happened twice, both times a test that passed
on Linux and failed on macos-14, and both times it cost the same: a version number
spent, a tag deleted by hand, the next release starting over. Nothing in git stops
you tagging a red commit, so the guard has to be in front of the tag.

By hand it is: bump the version in `Cargo.toml` (`[workspace.package]`),
`desktop/tauri.conf.json` and `Cargo.lock` (five lines there, one per member),
commit as `Release <version>`, then `git tag v<version> && git push origin
v<version>`. The workflow refuses a tag that does not match the crate version,
because a released build that disagrees with its own tag nags about an update it
already is. Versions are CalVer: `<year>.<month>.<n>`.

**The release body is the tag's own message now**, written by
`tools/release-notes.mjs` and read back by the workflow. A tag cut by hand is
lightweight and carries none, and the workflow falls back to a compare link —
which is what every release published before this. To write them by hand:
`node tools/release-notes.mjs v<previous>..HEAD > notes.md`, then
`git tag -a --cleanup=verbatim v<version> -F notes.md`.

**The crate manifests are not on that list, and that is the fix for a release
that could not be cut.** The version used to be a literal in each of them, and
the crate split then took it out of the root — where *both* readers of "the
version" look. `mise run release` died on "no version line in Cargo.toml", and
the workflow's tag-matches-version step had been comparing the empty string
against every tag since the split, silently, because nothing tags on an ordinary
push. `[workspace.package]` holds it now and every member says
`version.workspace = true`, so cargo refuses a disagreement rather than a script
promising to prevent one. **This is the fourth thing a crate move broke by
reading a path** — after the module ratchet, `check-module-routes.mjs` and
`typos.toml` — and the first that failed silently; the entry on the crate layout
says to look for the next one.

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
