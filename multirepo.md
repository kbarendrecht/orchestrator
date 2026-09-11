# Multi-repository, as implemented on `vdkolk/multi-repo`

A review of `4f6ff72..826b930`, six commits, 2266 insertions and 114 deletions
across 21 files. (An earlier draft quoted the range as `4967619..826b930`, which
excludes the first of the six — the one that adds `sibling_origin` and
`__ORCH_CHECKOUTS__` — and measures 2020/164.) Written 2026-09-09, against a branch that is exactly
`main` plus these six commits.

## What it does

Several repositories show **at once**, in one rail, rather than being switched
between. Each repository is a whole `orchd` of its own — its own process, port,
`config.json`, `ORCHD_CONFIG_DIR` and instance lock. The SPA holds one websocket
and one snapshot per repository and composes them into one rail. Everything other
than the rail follows a single *active* repository, so the ~58 call sites that
name only a path did not change.

The extra repositories are `extra_checkouts`, a config key read by the **app**,
not by the daemon, and substituted into the page at load. Per-repository settings
therefore live in that repository's own file, under
`<config dir>/repos/<name>-<hash>/`.

A secondary daemon is a **child process of the app**: the bundle ships no `orchd`,
so the app re-executes itself with `--secondary <checkout> --board-origin <origin>`.
The child mints its own token and prints `ready <port> <token>` on stdout for the
parent to read, and dies on stdin EOF.

This reverses the in-process design recorded in `TODO.md`, and the reversal is
argued rather than assumed: that item ruled a child process out because it "has no
Tauri handle", which is true only of the daemon *serving the page*. A secondary
serves no page, so the titlebar keeps talking to the primary.

## Verified state

| Gate | Result |
| --- | --- |
| `cargo check` / `clippy --all-targets` / `test` | reported green by the author; the count is below |
| `mise run check-web` | green; 9 modules, 19 dependencies, no violation |
| `mise run e2e` (branch) | 24 passed, 0 failed |
| `mise run e2e` (`main`, 2026-09-10, this machine) | **24 passed, 0 failed** |

**The `main` e2e row was wrong in the first draft**, and it was the only number
distinguishing the two trees. It read "15 passed, 1 failed", which no unfiltered run
can produce: `tools/e2e/run.mjs` discovers flows with `readdirSync`, catches per
flow and prints `pass` and `failures.length` after the loop, so the two must sum to
the 24 files on disk. A re-run gives 24 and 0. The conclusion it was used for still
holds — the `--no-verify` trailer's claim that four flows fail on `main` does not
reproduce — but "the branch is greener than its base" rested on a bad figure and is
withdrawn: the two trees are equal here.

The test count does not reconcile either. `main` has 522 lib tests and the branch
adds 11 test attributes in library files (`api.rs` 21→22, `config.rs` 21→25,
`peers.rs` 0→6), which is **533**, not the 528 the first draft reported. Five more
are in `desktop/src/secondary.rs`. The branch's suite was not run here, so this is
an attribute count and not a pass count.

**No e2e flow exercises multi-repo**, which the author states.

## The best of it

- **The daemon stayed ignorant, and that is the whole design.** Nothing in `orchd`
  became repo-aware. `MAIN`, `claim_main`, `release_main`, the swap and both PR
  flows never learned a repo qualifier, because a daemon still owns exactly one
  checkout. The alternative — one daemon holding many repos — would have spent its
  entire budget on the session model.
- **A process global is *correct* when each daemon is its own process.** The
  `Config::config_dir()` sweep that `TODO.md` priced as cost item 1, 21 call sites
  across eight modules, is not needed at all. The design that looked more expensive
  turned out to be the one that removes work.
- **Failure isolation is kept rather than paid.** `TODO.md` listed "a crash, an OOM
  or a self-upgrade restart takes every repo's live sessions" as an accepted cost
  of the in-process shape. This shape does not pay it.
- **Durable state is genuinely separated.** Every `config_dir` consumer follows
  `ORCHD_CONFIG_DIR` per child: `instance.pid`, `sessions.json`, `automation.json`,
  `manual.json`, `stories.json`, `resolve-runs.json`, `window.json`, `hooks.json`,
  the skills plugin dir, transcripts and `orchd.log`. `hooks::write_settings` bakes
  each daemon's own port into its own settings file, and `session_flags` passes
  that path as `--settings`. There is no shared hook file and no shared lock.
- **The token comes back out rather than going in.** Handing a child its token
  through the environment would have put it in the environment of every session
  that child spawns. The child mints its own and prints one line. This respects an
  invariant the codebase already asserts in `triage.rs`.
- **Stdin EOF is a second, independent kill switch.** A SIGKILLed app still takes
  its secondaries down, because the pipes are `O_CLOEXEC` and so do not leak into
  `claude` grandchildren.
- **The Origin widening is one exact string.** `sibling_origin` is set only from
  `StartOptions`, never from the file (`#[serde(skip)]`), with the preflight
  answered before the token check. Proxying every call and both websockets through
  the primary was the alternative, and it would have given up the isolation that
  made separate processes worth choosing.
- **Single-repository UX is untouched.** The only added chrome is a `+ repository`
  button at the foot of the rail. `multiRepo()` gates everything else — no header,
  no colour band, no extra click on any existing action.
- **The colour palette is reasoned.** Assigned over the whole set rather than per
  repository, keyed on the checkout rather than on position, and hand-picked
  because an earlier list put two nine-degree-apart shades side by side on the
  first real run.
- **The docblocks are of this codebase's standard.** `peers.rs` and `secondary.rs`
  both open by arguing the decision against the alternative and naming what the
  first version got wrong.

## The worst of it

Ordered by how much they cost.

- **Two of the three new controls cannot work.** `close repository` posts
  `{ path: repo.path }`, but `core.js` maps peers into objects with no `path`
  field — so the body is `{}` and the handler rejects it 422. Dead on every
  install. And both `add` and `close` use `call()`, which targets the *active*
  repository, while only the primary has `checkout_control` — so touching a session
  in a second repository makes the `+` answer "adding a repository needs the desktop
  app, not a browser tab", inside the desktop app.
- **`setActiveRepo` is a second writer of `snap`.** `core.js` gives the base one
  writer — "only `receive()` may replace it", because the snapshot and the clock it
  is measured against have to move together or durations freeze — and the branch has
  three. Stage 4's "one writer path" is the fix; this is where the need for it is
  visible in the code rather than argued from the domain.
- **`announceWaiting` re-announces on every checkout switch.** It compares each
  checkout's snapshot against one module-level `waitingKnown`, so switching
  checkouts makes every waiting session look newly waiting — which breaks its own
  rule that it fires "only on the transition in, so it never nags". Peer-blindness
  is the smaller half of this one.
- **Workspace ids are not unique across daemons, and the SPA assumes they are.**
  `MAIN` is `"main"` in every daemon and a managed process id is
  `<workspace>:<name>`, so `main:ng-watch` names two different processes. `terms`,
  `selectedProc` and the persisted `procOrder` key on that id alone. The rail
  solved exactly this for `showArchived` by qualifying the key; the terminals were
  not. This is the deepest fault, because it is a *model* error rather than a bug.
- **A terminal binds to `activeRepo` at reconnect, not at open.** `connect()` reads
  the global, and `connect()` is also the reconnect path. A blip while you are in
  another repository points the socket at the wrong daemon, and it never heals.
- **The teardown loop tests every terminal against one snapshot**, several times a
  second, because it walks all of `terms` and asks whether the *active* snapshot
  still has it. An earlier draft read this as closing every peer's terminal. The
  worse direction is the opposite one: `proc:main:ng-watch` is present in **both**
  snapshots, so the loop *retains* the peer's terminal and `Term.show` then hands the
  other checkout's live terminal back under this checkout's tab.
- **A dead secondary looks alive, indefinitely.** `repo.live` is never cleared,
  nothing calls `try_wait`, the child becomes a permanent zombie, and `add_checkout`
  then refuses to re-add it as "already open". Rows keep drawing working dots and
  ticking clocks — the exact lie the waitbar exists to prevent.
- **Shutdown can orphan live agents.** The SIGKILL fallback signals the daemon pid
  only, and the child is not in its own process group — so there is no group to
  sweep, in precisely the case where the child's own graceful shutdown did not
  finish.
  **And no group sweep would reach the agents anyway**, which an earlier draft of
  this document got wrong twice. It said "`kill_gracefully` exists for this shape and
  is opted out of": `kill_gracefully` is a method on `PtyHandle`, for a pty child,
  and the branch's own comment says so — nothing was opted out of. The deeper point
  is that `portable-pty` calls `setsid()` before exec, so **every `claude` session is
  its own process group** and a daemon's group contains the daemon alone. The only
  thing that ever takes sessions down is the daemon's own `Server::shutdown`, which
  is already a `JoinSet` of `kill_gracefully` calls under one shared grace — and a
  SIGKILLed daemon runs none of it. So the fix is to give the daemon time to run its
  own shutdown, and to treat a daemon that would not is a daemon whose sessions
  outlive it. `proc::run_bounded_with_input`'s `process_group(0)` plus
  `killpg(TERM)`, a second, `killpg(KILL)` is the shape to copy for the child
  itself.
- **Boot and quit have long tails.** A slow secondary holds the window at the splash
  for up to 90 s. Quit stops secondaries sequentially, 20 s each.
- **The keyboard map no longer covers every checkout.** `MOD+Space` and `Ctrl+Tab` are
  active-repo only while the waitbar counts across all repositories, so the bar can
  read "2 need you" and the chord it advertises can answer "nothing waiting on
  you". `announceWaiting` never tells a screen reader about a peer. The repository
  header is a `div` with an `onclick` — activating a repository is mouse-only, and
  it is the only way to reach one whose sessions have all finished.
- **No repository identity exists outside the rail.** PR numbers, workspace names
  and file paths are shared vocabulary between two checkouts, and the context bar,
  the diff header and the drawer say nothing about which checkout they describe.
- **Two daemons on one checkout is still reachable**, through two primaries with
  different `ORCHD_CONFIG_DIR` values listing the same extra checkout. The lock is
  keyed on the config dir, which is derived from the primary *and* the checkout.
- **No e2e flow exercises it, and the desktop crate is outside the lint gate.**
  CI's `cargo clippy --all-targets` from the workspace root lints `orchd` only, so
  the ~700 new lines in `secondary.rs` and `desktop/src/main.rs` are unlinted —
  pre-existing, newly load-bearing. An earlier draft of this document said "nothing
  tests any of it", which was false and contradicted two of its own lines: 16 unit
  tests come with the branch and CI runs them on both platforms.
- **Two docblocks were severed by insertion.** `shell_of` was inserted inside
  `open_url`'s docblock, and `deadPid` between `until`'s docblock and `until`. Both
  functions lost their documentation to the new neighbour — the same anchoring
  mistake `CLAUDE.md` records for `#[test]` attributes, costing docs instead of
  test registration.
- **`TODO.md` is wrong about the button it discusses — on `main`, not only on the
  branch.** `TODO.md:98` states the header's repo-switch button's "only behaviour is
  a toast reading *not implemented yet*". That has been false since `b9d30f0`, which
  is an ancestor of `main`, and no `not implemented yet` string exists anywhere in
  `web/` or `src/`. So the repair belongs on `main` regardless of what happens to
  multi-repo.

## What five lenses agreed on

Domain, architecture, simplicity, failure-mode and UX were applied separately. They
converged on three statements, which is the strongest signal in this document.

1. **`activeRepo` must not be a second piece of state.** The domain says a session
   belongs to a workspace belongs to a checkout, so the checkout is *derived* from
   the selection. Architecture says `onActiveRepo` was built with zero subscribers,
   which is the DAG inversion this codebase made a rule. UX says activating a
   repository without carrying a selection blanks the centre pane. Three lenses,
   one root cause, and most of the SPA bug list hangs off it.
2. **The set of open checkouts has no owner and nothing watches it.** Four representations of "which
   repositories are open" — the `SECONDARIES` static, `AppState::checkouts`,
   `extra_checkouts`, and the page's `checkouts[]`. The one thread that learns a
   secondary died, the stdout drain, discards the fact and returns.
3. **Two features exist only to make each other correct.** Drag reorder forces
   colour to be identity-keyed, which is why `colours_for` is set-wide with a
   collision probe and four tests. Delete the drag and colour becomes
   `PALETTE[i % len]`. Together: 205 lines, no user-facing capability.
   **Decided the other way: both are kept.** The lenses are right that the two are
   one decision, and the decision taken is the pair, not neither — order is worth
   owning, and a colour that moves when you close an unrelated checkout is worse than
   the machinery that stops it. So `colours_for`, `slot_for`, the collision probe and
   the four tests come across from the branch, and the Stage 1 deletion list drops
   them.

## Safety, found by the domain and failure-mode lenses

These are not coding errors. They are consequences of letting one window hold N
checkouts, and they are the reason this cannot ship as it stands. All four are
verified against the code.

- **The instance lock now guards the wrong noun.** `instance.rs` states its
  invariant in terms of the *checkout* — "both spawn sessions into the same
  worktrees". It locks `<config dir>/instance.pid`, and a secondary's config dir is
  rooted at the *primary's*, so two primaries with different `ORCHD_CONFIG_DIR`
  values listing one extra checkout is two daemons on one checkout.
  **The mechanism is smaller than an earlier draft claimed**, and the correction
  matters because that draft made it a leg of the root-cause argument.
  `config_dir_for` hashes the **checkout alone** — the FNV loop runs over
  `checkout.to_string_lossy()` and nothing else — and the primary appears once, as
  the parent directory: `primary.join("repos").join(format!("{safe}-{hash:x}"))`. So
  the mapping from checkout to directory *name* is already a function of the
  checkout; only its root varies. The minimal repair is re-rooting that one line at
  a fixed directory. The branch's own test states the invariant and then asserts the
  violation.
  The supporting example was wrong too: **a fixture daemon is not this case.**
  `tools/fixture-pr.mjs` sets `main_checkout` to a throwaway clone, and the e2e
  harness mkdtemps a fresh repo per sandbox.
- **A checkout may be nested inside another open one.** `firstrun::validate`
  accepts any directory where `.git` *exists*, and a worktree's `.git` is a file.
  `add_checkout` refuses only the current `main_checkout` and exact duplicates. So
  daemon A's worktree can become daemon B's main checkout: one object store, one
  `git worktree` list, and A's retention reaper can remove B's checkout, because
  B's live sessions are not in A's records.
- **Two checkouts of one GitHub repository defeat the fix-pr guard.** The branch
  explicitly blesses "a fork beside its parent". Both derive the same `Repos`, both
  poll the same PRs, and "one live run per PR" reads *this* daemon's
  `automation.json` while `branch_busy` reads *this* daemon's workspaces. Two
  force-pushing runs against one head ref is reachable, and neither guard can see
  the other.
- **Closing and re-adding a repository resurrects its old sessions.** `remove` stops
  the child and rewrites the config key. It never touches that repository's
  `sessions.json`, and `config_dir_for` hashes the path, so a re-add reuses the same
  directory. `auto_resume` then spawns a `claude --resume` for every record with
  `was_live && cwd.exists() && had_a_turn` — a months-old conversation per
  workspace, on a repository you just reopened.

## The root cause, from first principles

One process does two unrelated jobs. It **serves the window** — the page, the
window, the list of checkouts, cross-repo attention — and it **manages one
checkout** — sessions, worktrees, git, PRs. `orchd` was built when those were one
thing, because there was one checkout. The branch adds a second checkout and
inherits the conflation, so one daemon becomes "the one that also serves the page".

Every structural fault in this document follows from that single fact:

- `close` refuses `main_checkout`: you cannot close the daemon serving you.
- Add and close reach for the *active* checkout's daemon, and only the primary can
  answer, so they break the moment a peer is active. **Not a structural fault**, and
  an earlier draft said it was: `callOn(repoId, path, body)` already exists in
  `core.js`, `call` is defined one line later as its active-checkout default, and
  `rail.js` already uses `callOn` under the docblock "the one place `callOn` earns
  its keep". `addRepo` and `closeRepo` simply call the default. With the companion
  bug — the body is `{}` because the peer mapping drops `path`, while `peers::id_for`
  already *is* the path — the whole defect is four edits in one file.
- The checkout list is substituted into the page rather than served, so add and
  close can only `location.reload()`. **Also weaker than an earlier draft claimed**:
  `AppState::checkouts` already holds the list, `Peer` is `Serialize`, three routes
  already mutate it and `lib.rs` already serialises that vector — so a
  `GET /api/checkouts` is a handler plus a route line, and death already has an owner
  to be reported to. What is missing is the observer, which is an ordinary bug in the
  list above, not a consequence of the conflation.
- A secondary's config dir is rooted at the primary's, so the instance lock stops
  keying on the checkout, so two daemons on one checkout is reachable. (One line,
  not a structural consequence — see the correction above.)
- The primary runs in-process while secondaries are child processes: two hosting
  models for one kind of thing.

None of that is multi-repo complexity. It is the cost of one daemon being special.

**Two of those five legs are weaker than the first draft made them**, and the
corrections are inline above: add/close is four edits in one file, and the checkout
list already lives in `AppState` and is already serialised. The case for the split
therefore rests on the three that survive — no privileged checkout to nest another
under, a lock that keys on the checkout, and one hosting model instead of two — plus
the safety findings. That is still the case, but it is a smaller one than "every
structural fault follows from this", and a reader deciding whether to spend five
stages deserves the smaller version.

## Target architecture

**The host is not a daemon. The host is the app.**

- The Tauri process serves the page and a small **host API**: which checkouts
  exist, add, close, liveness, window commands, aggregate attention. The app already
  serves HTTP in-process — `firstrun::serve` is exactly this shape, with the
  daemon's Host and Origin rules — and this codebase chose HTTP over Tauri IPC on
  purpose, so flows stay headlessly testable.
- **Every checkout gets an identical daemon**, and every one is a child process. No
  flag, no privilege, no promotion, no embedded instance. `orchd` becomes purely
  "one checkout", which is what `peers.rs` claims it is and is not.
- The page holds one connection to the host and one per checkout daemon. The host
  connection carries the checkout list and liveness; each daemon connection carries
  that checkout's snapshot and ptys, unchanged.
- **`Repos` changes role, from a description to a key.** Today the upstream/fork pair
  is descriptive: it says which GitHub repository this checkout is, and nothing reads
  it as an identity. Making one repository per host means the pair becomes a
  **uniqueness key over the checkout list**, which has one consequence worth stating
  where the architecture is decided rather than where the refusal is: the host derives
  it *itself*, from the candidate's git remotes, **before that checkout has a daemon**.
  `state::AppState::new` derives it today from `forge::remote_url` on `upstream_remote`
  and `origin`, which is a daemon reading its own checkout at start — too late to
  refuse an `add`, because the process it would refuse has already been spawned. So
  the derivation moves to a function both callers share.
- **Config splits along the same line.** A host file holds the checkout list and
  window geometry. Each checkout has its own `config.json` under
  `<config dir>/checkouts/<hash>/`, keyed on the checkout alone. `main_checkout`
  stays *inside* a daemon, meaning "the checkout I manage" — true and useful — and
  stops meaning "the special one".
- **A lone `orchd` in a terminal keeps working.** The host server is a library
  either host can run: the app runs it for N daemons; a solo daemon runs one for
  itself. `cargo run -p orchd` plus a browser tab stays intact.

What this **deletes** rather than fixes: `peers.rs` as a concept, `CheckoutControl`,
`AppState::checkouts`, `checkout_control`, the `__ORCH_CHECKOUTS__` substitution,
both `location.reload()`s, the `--secondary` flag, the primary/secondary asymmetry,
the header switcher and every question about it, and the four competing
representations of the checkout list. (An earlier draft also listed
"promotion-on-close", which the branch never built — `grep -i promot` finds nothing
in it.)

**`WindowControl` stays**, and an earlier draft of this document said it went, on the
belief that the host *is* the Tauri process. It is not. The host is a **library**, and
the app is one of two things that can run it — so the trait is load-bearing for three
reasons, each of them checked:

- **The crate graph forbids the alternative.** `desktop/Cargo.toml` depends on
  `orchd`, `orchd` has no `tauri` dependency, and that direction cannot reverse. A
  `host` module inside `orchd` can never call Tauri directly; the trait is what lets
  it drive a window it cannot name.
- **A solo `orchd` has no window to drive.** Its host holds no handle at all, so
  something has to express "no native window attached" — which Stage 0 item 3 already
  pins as behaviour to preserve.
- **It is not the same shape as `CheckoutControl`.** That existed only because one
  daemon was special, and it breaks the moment a peer is active. One
  `Arc<dyn WindowControl>` for one honest reason is an abstraction over a real
  boundary between two crates.

The alternatives were to put the host in the `desktop` crate — which kills the solo
daemon and flow 25's headless testability — or to split it across both, which gives
one API surface two owners. Both are worse than one indirection.

**The four safety findings, one line each**, because an earlier draft counted
nesting on both sides of a 2+2 split and the resurrection on neither:

1. **The instance lock** keys on the checkout again, so it stops existing by
   construction (Stage 2 item 7).
2. **A checkout inside another open one** becomes a refusal the host applies at
   `add`, in either direction, in one place, with the reason named.
3. **Two checkouts of one repository** likewise, keyed on the repository each daemon
   would poll and checked twice — see that decision.
4. **Resurrection on re-add** is answered by asking rather than by clearing state,
   and by the `stopping` flag that stops `close` from being read as a crash.

**The one genuine trade.** The embedded daemon goes. Today the app hosts one daemon
in-process; after this, all checkouts are children and the app runs none itself.
Cost, measured rather than estimated: **2.7 ms and 9.3 MB per child process** —
which is what the split adds, not what a checkout costs; the numbers and the method
are under the hosting decision. Gain: one hosting model, one
lifecycle, one log shape, one lock rule. Lean means one path. The decision is
all-children, and the child is the `orchd` binary rather than a re-exec of the app.

## Principles for the work

Nothing new. These are the rules this codebase already lives by, applied to the
host.

- **One owner per fact.** The host owns the checkout list. Nothing else holds a
  copy it could disagree with.
- **A seam has a subscriber or it does not exist.** `onActiveRepo` with zero
  listeners was the shape to avoid.
- **Every spawner arms its observer**, at the moment the handle exists, and there is
  one observer per child. `spawn::watch_session_exit` is the model.
- **Delete before you add.** Every deletion is its own green commit on `main`, so the
  refactor moves less code and every move has a test behind it.
- **Pin the seams, do not raise coverage.** A boundary refactor breaks behaviour at
  the boundary and nowhere else. Tests go where the boundary moves.
- **The acceptance flow is written first**, and is out of the runner's list until it
  can pass. It says when to stop. It does not sit red in a suite everybody runs.
- **Measure, then change.** No timing claim, no port claim, no "slow" without a
  number from `timing.rs`.

## The plan

Five stages. Each is independently shippable on `main` and leaves the tree green.
Stages 0 and 1 touch no multi-repo code at all.

### Stage 0 — pin what must survive

Tests for exactly the behaviour the split moves. Each must pass before and after,
unchanged.

1. **`GET /` and the token.** The page substitution, the same-origin token hand-off,
   and that `GET /` is deliberately not token-gated.
2. **The Host and Origin rules** in `api::guard`, including the CORS preflight arm
   the branch added, generalised: the origin a daemon accepts is *the host's*, for
   every daemon.
3. **`dispatch_window`**, and its "no native window attached" degradation.
4. **The first-run page**: no config, and a checkout that has moved. Both paths stay.
5. **The restart handoff**: `relaunch` and `await_handoff`. Pin that the successor
   waits for the pid it was given, and the "no native window" and refused-restart
   paths around it.
   **Do not pin the `--wait-for-pid` behaviour as correct — it is a bug**, and an
   earlier draft of this document called it a fix and told the reader to pin it.
   `relaunch` builds its argv from `args_os().skip(1)`, which **keeps** any
   `--wait-for-pid <pid>` pair already there, then pushes a second one; `await_handoff`
   takes the **first** occurrence. So the second restart in one app lifetime waits on
   the already-dead grandparent, returns at once, and then dies on the live `flock` —
   leaving no app at all. It is byte-identical on the branch, so it is nobody's
   regression. Fix it (drop the old pair, or read the last occurrence) as its own
   commit with its own test, and pin the fixed behaviour.
6. **`auto_resume` on restart**, per daemon.
7. **The instance lock, as it behaves today**: a second daemon under the *same*
   config dir is refused, and the lock survives a `process::exit`. That is what Stage
   0 can pin, because a pin must pass before and after.
   **The checkout-keyed assertion is a Stage 2 test, not a Stage 0 pin**, and an
   earlier draft put it here. Two config dirs are two lock files today —
   `acquire()` is `Config::config_dir()?.join("instance.pid")` and `config_dir()`
   returns `ORCHD_CONFIG_DIR` when set — which is this document's own argument for
   the finding. So the test would have failed from the moment it landed, in
   `cargo test`, which every commit runs, and without the "out of the runner's list"
   protection item 8 gets.
8. **The acceptance flow, landed disabled** (`tools/e2e/flows/25-host.mjs`): boot a host with
   two checkout daemons; assert two ports answer and the host lists both; kill one,
   assert the host reports it down; close one, assert nothing restarts and the
   other's sessions are untouched; add a checkout nested inside an open one, assert
   the refusal names containment. `ORCHD_CONFIG_DIR` and the fake-agent shim already
   make a second daemon cheap.
   **It is out of the runner's list until Stage 3, and that needs one harness change
   first.** A flow that stays red for three stages is not free here: the pre-commit
   hook runs the e2e suite every fifth Rust-or-`tools/e2e/` commit and a failure does
   not reset the counter, so a permanently red flow blocks every fifth commit and
   teaches `--no-verify`. But **`run.mjs` has no way to skip one today** — it
   `readdirSync`s `flows/`, imports every `.mjs` and has no `only`, `skip` or
   `disabled`, and a module without a `run` export throws and counts as failed. So
   Stage 0 adds one: a flow may export `pending: true`, which the runner prints as
   pending and counts in neither total. Stage 3 removes that line from flow 25, and
   that removal is the definition of done.
9. **Put the desktop crate under CI's clippy.** `cargo clippy --all-targets` from the
   workspace root lints `orchd` only, which is why ~700 branch lines are ungated.

### Stage 1 — delete

Each its own commit. Nothing here depends on the host existing.

1. **The header switcher is not deleted here.** It moved to Stage 3, and the reason
   is this plan's own two sentences: "stages 0 to 2 change nothing a single-checkout
   user can see", and "with symmetric `close`, switch is `add` then `close`" — where
   `close` arrives in Stage 3. Verified while doing the work: that button is the
   **only** way to change project, because `Config::existing` returns `None` only for
   a checkout that has moved, so nothing else raises the first-run page. Delete it
   here and there is no way to open another project for two stages.
2. From the branch, **do not take**: `peers.rs`,
   `CheckoutControl`, `AppState::checkouts`, `checkout_control`, the
   `__ORCH_CHECKOUTS__` substitution, the `--secondary` flag, the second FNV-1a, and
   both `location.reload()`s.
   **Two corrections to an earlier draft of this list.** It said "do not take the
   duplicate `name_for`" — there is no duplicate: `peers::name_for` is the only
   definition and `secondary.rs` calls it across the crate boundary, so following
   that instruction deletes the only copy and breaks three call sites. And it dropped
   `--board-origin` outright, which is the *only* thing that ever sets
   `sibling_origin`; see item 3.
3. **Take from the branch**: the e2e `deadPid` portability fix, the `sibling_origin`
   idea **with the flag that sets it** — renamed `--host-origin`, applied to every
   daemon, because `--board-origin` is the only thing on the branch that ever gives
   that field a value and item 2 above would otherwise leave it `None` for good, and
   then every daemon answers `bad origin` to the page, the `extra_checkouts`
   canonicalise-and-dedupe in `Config::parse` (re-homed to the host file), the
   ready-line handshake where the child mints its own token and prints it, the stdin
   EOF kill switch, the fold with counts, the waitbar spanning checkouts, the
   `callOn`/`getOn` split, the per-checkout `snap`/`snapAt`/`live` container in
   `core.js`, and **the palette with its identity keying whole** — `colours_for`,
   `slot_for`, the collision probe, the four tests, and drag-reorder with its
   localStorage order.
4. **Repair the two severed docblocks** (`open_url`, `until`) and the stale
   `TODO.md:98` sentence about the header button. That sentence is stale on `main`,
   so this commit stands on its own even if nothing else here is built.

### Stage 2 — one checkout daemon, hosted by the host

The refactor. No second checkout yet; the app runs the host and exactly one child.

1. **`host` as a library module** in `orchd`: serves the page, `/api/host/*`
   (checkouts, add, close, liveness), `/ws/host`, and window commands.
   **`/ws/host` carries the checkout list and liveness, and nothing else.** No
   aggregate attention: the page already holds N snapshots and the waitbar already
   aggregates over one, so aggregating over N is the same code reading a wider set.
   The host aggregating it would mean the host learning the session model it exists
   not to know, which is the conflation this whole plan is undoing. It holds the
   Tauri handle directly when hosted by the app. A solo `orchd` runs one for itself.
2. **The daemon loses the host's routes**: `index()`, **every SPA asset route**
   (`/app.js`, `/app.css`, `/review-preview`, `/js/:file`, `/vendor/:file`,
   `/vendor/fonts/:file` — they go with the page, and the host is an `orchd` module
   so `include_str!` still reaches them), window dispatch, and the checkout routes.
   It keeps `/api/*`, `/ws/events`, `/ws/pty`, **`/hooks/*`**, and accepts the host's
   origin.
   **`/hooks/*` cannot move, and an earlier draft's list omitted it.** There are
   eleven hook routes outside `/api/*` (`/hooks/pre-edit` plus the ten in
   `observer_hooks()`), and `hooks.rs` writes `http://127.0.0.1:{port}/hooks` into
   *that daemon's own* settings file — so the port in an agent's hook URL is the
   port of the daemon that spawned it, by construction. `api::guard` also has a
   dedicated arm for the prefix, which is the only reason a hook with no Origin and
   no token gets through at all. A daemon without them learns nothing: no
   `session_start`, no `Stop`, no `post_tool_use`, so no session state, no
   changed-files pane, no `wants_attention` — and **silently**, because a
   `type: "command"` hook's `|| true` swallows every 404.
   **Moving `index()` moves the token hand-off, and that is an open question, not a
   detail.** Today `GET /` is deliberately not token-gated and substitutes *the
   serving daemon's own* token into the page (`lib.rs:1500`), which is the whole
   reason the switcher's page substitution was "nearly free". After the split the
   page needs **one token per checkout daemon**, and the host is the only thing that
   has them — each arrives on its child's ready line. Three shapes, and they are
   different security postures rather than different spellings:
   - **The host substitutes them all.** One substitution instead of one string, and
     the page holds N tokens from first paint. Closest to today.
   - **The host serves them.** The page loads, then reads the checkout list with
     ports and tokens from `/api/host/checkouts` — which then needs its own token in
     the page to gate it, so it is one substitution *plus* a round trip.
   - **One token for the host and every child.** Simplest page, and it reverses "the
     token comes back out rather than going in", which this document praises the
     branch for and `triage.rs` asserts as an invariant.
   **Decided: the host substitutes them all, and the host socket carries every
   later change.** Substitution keeps the property today's page has — a complete,
   correctly-tokened page from first paint, with no pre-token round trip to gate.
   But it cannot be the *only* channel, and an earlier draft left that gap: a
   checkout added after load has no token in the page, one still starting answers
   after first paint, and a daemon that took its one retry has minted a **new**
   token, so the row's old one is dead. So the checkout list the host socket carries
   (Stage 2 item 1) carries each checkout's port and token with it, and the
   substitution is simply that list's first value. One shape, two deliveries.
   This does not re-open the aggregate-attention rule: a port and a token are the
   host's own facts about a child it spawned, while attention is session state the
   host does not model. And the page needs the **host's** token by substitution
   either way, exactly as today, to be allowed on that socket at all.
   The child still mints its own token and prints it, so the invariant `triage.rs`
   asserts is untouched. Stage 0 item 1 pins the behaviour this replaces.
3. **The app spawns its one daemon as a child**, running the `orchd` binary the
   bundle ships rather than re-executing itself — see the hosting decision for the
   `ldd` numbers that settled it. The child is in its own process group. The ready line
   carries port and token; stdin EOF is the second kill switch.
4. **One observer per child, and it needs to know why the child exited.** Armed
   inside `launch`: the stdout drain thread takes the `wait()` and the dispatch.
   On an *unintended* exit it marks the checkout down, pushes, and restarts it once
   — see the restart decision below.
   **An earlier draft had no intent channel, and that made `close` and quit
   indistinguishable from a crash.** Both produce the same EOF, so `close` would have
   restarted the daemon it just stopped, the restart runs `auto_resume`, and safety
   finding 4 comes straight back — a months-old conversation per workspace, on a
   checkout you just closed. Quit would have been the same fault N times over,
   defeating item 5. Flow 25 asserts "close one, assert nothing restarts", so the
   plan's own acceptance criterion could not pass.
   So a deliberate stop **sets the checkout's `stopping` flag before it signals**,
   and the observer reads it after `wait()` returns: set means report and stay down,
   clear means restart once. Two properties that make it correct rather than a flag
   that races: the flag is set before the signal, never after, so there is no window
   in which the exit arrives first; and it is set by the *host*, which is also the
   only thing that spawns, so one owner decides both.
   **Ownership follows from `Child`, not from taste.** `wait` and `try_wait` both
   need `&mut self` — which is why the branch's own `Secondary::stop` is
   `pub fn stop(mut self)` — so the observer thread owning the handle and a shutdown
   path holding it to signal cannot both exist. The handle therefore lives in the
   observer, and every stop path signals **by pid** (its own process group) and never
   touches the handle.
5. **Shutdown is concurrent**, one shared deadline, and the `stopping` flag set on
   every checkout before the first signal so no observer restarts anything. Quit must
   not be N × 20 s.
   **What the deadline buys and what it cannot.** Each child gets long enough to run
   its own `Server::shutdown`, which is the only thing that reaches its sessions —
   they are each their own process group, so no sweep from here can. A child past the
   deadline is `killpg`'d for its own sake, and its sessions are then orphans holding
   worktrees; say so in the log rather than implying the sweep covered them.
6. **Config split**: the host file, and
   `<config dir>/checkouts/<leaf>-<hash>/config.json` per checkout, the hash over
   the checkout path alone. The leaf is for reading a bug report by eye; the hash is
   what makes it unique, so a moved checkout gets a new directory either way.
   **Window geometry moves to the host file.** One window has one geometry, which
   follows from the host being the app; a `window.json` per daemon would be N files
   of which one happens to win — the four-representations fault in a new place. No
   daemon writes one after this. `migrate.rs` gains a rule that
   moves today's single `config.json` into that shape; it is idempotent and writes
   only when it applied.
   **This is not a `migrate.rs` table rule, and an earlier draft got that wrong
   twice.** That draft said the rule recognises "`main_checkout` at the root of
   `config.json`" and is idempotent because the key is then gone. Both halves fail.
   `main_checkout` **stays** at the root of every per-checkout file — this plan says
   so itself, and `Config::main_checkout` is mandatory with no serde default — so the
   rule would re-fire on every start of every checkout daemon, rewriting
   `config.json.premigrate` and the file each time. And the table cannot hold it:
   `Migration.apply` is `fn(&mut Map<String, Value>) -> bool` and `config_file` ends
   in one `fs::write`, so `apply` never sees a path and cannot create a directory or
   write a sibling file. Worse, a rule that removed `main_checkout` and then failed
   its sibling write would leave a config the parser refuses — which costs the
   **whole file**, and the app reads that as first run.
   So the layout move is **the host's own one-shot at start**, not an in-file rule.
   Its shape is a *location*: `<config dir>/checkouts/` does not exist. It writes the
   host file and the per-checkout directory, **copies** the existing `config.json`
   rather than moving it, and leaves the root file untouched so a downgrade keeps
   working — the reason `store::OnDiskKind` and the tracker names still read their old
   spellings. Idempotent because the directory exists afterwards; and somebody who
   deletes that directory has deleted every per-checkout config, so re-deriving from
   the root copy is the right answer there rather than a re-fire. `migrate.rs`'s table
   keeps in-file rules only, which is what its docblock is about.
7. **The instance lock keys on the checkout.** ✅ **Done, and it cost nothing**:
   item 6 did it. The lock is `<config dir>/instance.pid`, and a child's config dir
   is now `checkouts/<leaf>-<hash>`, derived from the checkout — so the file is too.
   No new location, no second mechanism, and no machine-wide lock root: the answer
   was to move the *state*, and the lock followed.
   What it now guards: two daemons for one checkout are refused, by name, with the
   holder's pid. What it still does not: two hosts with different `ORCHD_CONFIG_DIR`
   values on one checkout — and `mise run fixture` and `mise run e2e` are that shape
   and safe anyway, because each uses a throwaway clone.
   Two details the plan has to name, because both are load-bearing.
   **Where the file lives.** The lock is taken *on* a file, and removing or moving it
   is how a second daemon locks a fresh inode while the first holds the old one. So
   the path is derived from the checkout and rooted somewhere every host shares — not
   under `ORCHD_CONFIG_DIR`, which is the thing being defeated. A fixture daemon then
   still gets its own `sessions.json`, `hooks.json` and `window.json` and no longer
   gets its own *lock*, which is correct: it is a different checkout, so the key
   differs anyway.
   **Where it is taken.** `orchd::start` takes the lock "first, and before anything
   is written", ahead of `Config::load_or_init` — which itself writes. A key derived
   from the config would force the lock after that write, which is the ordering that
   comment exists to forbid. The checkout is known from `StartOptions` before any
   config is read, so the key comes from there.
8. **Timing lines carry the checkout name.** `timing.rs` exists so a number survives
   being pasted into a chat; two daemons' lines must not be indistinguishable.
9. **`init_logging` moves into `orchd`.** It lives in `desktop/src/main.rs` today, so
   shipping `orchd` as the child would give every checkout daemon stdout and no file
   — and a launcher-started app has no stdout, which is the whole reason that
   function exists. One log shape means the library owns it: same lines, same
   `<config dir>/orchd.log`, same one kept generation, following
   `ORCHD_CONFIG_DIR` per child. The host's own log stays where it is.
10. Every Stage 0 test still passes, unchanged. The single-checkout product is
    indistinguishable to a user, and that is the acceptance criterion for this stage.

### Stage 3 — N checkouts

Now the acceptance flow can go green.

1. **The host runs N children**, identical. `add` refuses three things, each with
   its reason named:
   - **Containment, in either direction.** A candidate under an open checkout, and
     an open checkout under the candidate. Both give one object store two daemons,
     and the reversed case is the same hazard: `firstrun::validate` accepts any
     directory where `.git` exists, and a worktree's `.git` is a file.
   - **The same path twice.**
   - **A checkout of a repository already open** — the widest of the three, see the
     decision below.
2. ~~**The header switcher goes.**~~ **Moved to Stage 4**, as its item 9. "Last,
   not first" was right and the stage boundary was wrong: `add` and `close` mean
   "switch" between them only once the *page* has them, and `reposwitch` is until
   then the one way to change checkout.
3. **`close` is symmetric, down to the last one.** Any checkout, including the one
   you are looking at, and including the only one — an empty host is the first-run
   page, which `firstrun::serve` already is. A refusal at N=1 would be the one place
   symmetry broke, and the switcher's deletion rests on `close` being symmetric.
   Session records are **left alone** — closing a checkout is not a decision about
   its conversations — and the resurrection is stopped at `add` instead: see the
   decision below.
4. **`add` offers recents and browse.** `recent.json` moves to the host.
5. **Publish each checkout as it answers.** The host never waits for the slowest
   daemon; a checkout still starting draws its wait state. The 90 s hold on the
   splash cannot recur.
6. **`checkouts/` cleanup, and it must not reap a conversation.** A host sweep over
   the directory Stage 2 item 6 creates — an earlier draft said `repos/`, which is
   the branch's name and would have swept a directory that no longer exists while the
   one that grows was never touched. It removes checkout dirs not in the list and
   older than a **configured** retention, `checkout_retention_days` in the host
   file.
   **What it may not do**, because an earlier draft's rule would have: a checkout dir
   holds `transcripts/`, which is the *only* remaining copy once a worktree is gone,
   and a session record survives precisely because its archived transcript exists. So
   deleting the directory drops both. `worktree::reap_old`'s docblock states the
   opposite intent for that content — "the tree, never the conversation … deleting a
   record is a separate decision, taken by a person, and is deliberately not
   automated" — and Stage 3 item 2 above says session records are left alone at
   `close`, so reaping them 60 days later contradicts it.
   The rule therefore sweeps **only what is derived**: the skills plugin copy, the
   hook settings file, `window.json`. `transcripts/` and `sessions.json` stay, and
   what the sweep says in the log is how much it left and where. Its safety cannot be
   borrowed from `reap_old` either — that reaper is safe because it routes through
   `teardown`, whose six checks refuse a live session, a dirty tree, unpushed work or
   an attached process, and a directory of JSON has no such gate. `0` disables the
   sweep, the way it already does for worktrees. Today the directory is append-only, including a full skills-plugin copy and
   an unbounded transcript archive per checkout ever tried.
   It is a setting rather than a constant because the number is a judgement about
   your own habits, and this repo already has the same judgement written down once:
   `worktree_retention_days` defaults to 60 with a docblock arguing why ("clearly
   longer than anyone's memory of a branch"). The host's default follows it — one
   number to learn, not two — and the reaper it mirrors is the model for the rest:
   count from the last write rather than from creation, and never take a directory
   with work in it.
7. **Flow 25's assertions go green.** They moved to `tests/host_checkouts.rs`
   rather than growing the e2e harness a host, which is the decision this stage
   opened with: that harness exists to put a fake `claude` on PATH, and the host
   owns no sessions. The `pending` mechanism went with the flow, having had exactly
   one user.

### Stage 4 — the page

The SPA work, on top of a host that already reports the truth.

1. **The checkout is derived from the selection.** `checkoutOf(selected)`, falling
   back to the first checkout. No independent `activeRepo` writer, no eight
   "activate, then act" call sites.
2. **`snap` has one writer path.** Both the host-driven switch and `receive` go
   through the one function that moves `snap` and `snapAt` together.
3. **Switching carries a selection**: `lastSelected` per checkout, then newest live,
   then a named empty state. No blank pane.
4. **Qualify the keys, and dispose terminals on checkout change.** Both, and an
   earlier draft claimed the disposal alone made the collision "unreachable rather
   than qualified in four maps". It does not. Only `terms` is keyed on the process id;
   `selectedProc[wsId]`, `shownTab[wsId]` and the persisted `orch.procOrder` are keyed
   on the **workspace** id, and `MAIN` is `"main"` in every daemon. `procOrder` is one
   `localStorage` entry read and written under the bare workspace id and applied with
   no membership check, so dragging checkout A's drawer tabs silently reorders
   checkout B's `main` tabs — permanently, across reloads, which no amount of
   disposing xterm instances can undo.
   So the keys carry the checkout, the way the rail already solved this for
   `showArchived` with `worktrees:${repo.id}` — which is the fix this document's own
   list praises. Disposal stays, for the socket rather than the collision: `term.js`
   takes its checkout at open and stores it on the entry, and `connect()` never reads
   a global.
5. **The host socket reconciles in place**: add a row, mark one down, offer
   `reopen`. No reload. A dead checkout's socket stops retrying; today it retries
   forever with no backoff.
6. **Attention is global.** `MOD+Space`, `Ctrl+Tab` and `announceWaiting` read every
   checkout, the way the waitbar already does. The waitbar names the destination
   when it is not the checkout you are in.
7. **Legibility.** Sticky checkout header, group headers offset beneath it; the
   header is a real button with `aria-current`; the block is `role="group"`; the
   swatch is `aria-hidden`. One binding, `Ctrl+Shift+[` and `]`, in the legend. An
   identity chip in the top-left slot the switcher vacated: swatch, name, and the
   parent path segment when two checkouts share a leaf.
8. **Colour is reinforcement.** Drop the two hues nearest `--attn` and `--bad`; past
   the palette length, no band rather than a repeated one. Colour is keyed on the
   checkout, not on its position, so closing one does not re-colour the rest.
9. **Delete the header switcher**, and `WindowCmd::Switcher` and `start_switcher`
   with it — carried over from Stage 3, and deliberately after item 1. `add` and
   `close` mean "switch" between them only once the *page* has them; until then
   `reposwitch` is the one way to change checkout, and taking it away first is a
   capability removed with nothing in its place.
   **`BootstrapHost::switching` and `cancel` stay.** `switching` is a *defaulted*
   trait method, so dropping the impl compiles silently — and it is the only branch
   of `TauriBootstrap::open` that reaches `request_restart`. Everything else falls
   to `BOOTING`, which its own comment says is "set by the first open that boots the
   daemon, **and never cleared**". So after the app's first boot every
   `POST /api/open` would take the latched branch, undo its config write and toast
   "Could not switch to that project". `cancel` has a page-side consumer too.
10. **Rename `repo` to `checkout` in the SPA — its own commit, before the rest of
   Stage 4.** A pure rename first, so every later diff in this stage is behaviour
   only. By hand, by line, never a sweep: CLAUDE.md records why, and `check-web`
   cannot catch a string whose meaning changed.

## Decisions

- **All checkouts are child daemons; the app embeds none — measured, and the child
  is `orchd`.** One hosting model. Argued above under "the one genuine trade", and
  the argument was held open until a number existed, because this repo has already
  spent a session on two performance suspects chased on a debug figure.

  **The spike.** Release `orchd`, WSL2 Linux, 2026-09-10. Wall clock from
  `Command::spawn` to the daemon printing its URL, minus the `daemon start` phase the
  daemon logs itself — so the delta is exactly what the child process costs *on top
  of* the `orchd::start` both hosting models run. Nine runs on a throwaway checkout
  and five on this one.

  | Checkout | `daemon start` | wall to serving | delta (median of per-run deltas) | execs |
  | --- | --- | --- | --- | --- |
  | throwaway, one commit | 23 ms | 25.2 ms | **2.6 ms** (2.1–3.4) | 11 |
  | orchestrator, 2 worktrees | 1334 ms | 1336 ms | **2.7 ms** (2.3–3.4) | 11 |

  The delta column is the median of the **per-run** differences, not the difference
  of the two medians beside it, which is why it does not subtract on the page.

  **The delta does not move when the repo work goes up 58×**, which is what says it
  is exec plus loader and nothing else. It is 0.2% of a real start, and the children
  start in parallel, so it is not paid N times in series. Idle cost is **9.3 MB RSS**
  per daemon (8.9 MB at the ready line, flat over 30 s).
  **These two numbers price the hosting model, not a checkout.** Opening a second
  checkout costs its own `daemon start` — about 1.3 s here, including a network
  `git fetch` — whichever model hosts it. What the split adds on top of that is
  2.7 ms and one process's memory.

  **But the branch's child is the wrong binary, and that is the one real finding
  here.** `secondary.rs:284` re-executes `std::env::current_exe()`, which is the
  *desktop* binary, on the reasoning that the bundle ships no `orchd`. `ldd` says
  what that costs: **`orchd` links 5 shared objects, `orchestrator-desktop` links
  133**, twenty of them WebKit, GTK and GStreamer. Every child would pay that
  loader work to serve a page it never serves — and per-exec loader cost on macOS is
  the exact failure `timing.rs`'s docblock was written about. So **the bundle ships
  `orchd`** and the app spawns it by name. Two things fall out for free: the child's
  argv reads as what it is (`orchd --main <checkout>`), and the `--secondary` flag
  the deletion list already wanted is gone rather than renamed.

  **Not measured: macOS.** No Mac here, and this is a per-exec cost, which is the one
  cost a Mac pays differently. What the number does buy is a bound: the start already
  makes 11 execs, a Mac's git exec measures 8–9 ms in this repo's own notes, and one
  more exec of a 5-DSO binary is that order. Against a 1.3 s start it stays under a
  few percent even at a 10× penalty. Re-measure it on the macos-14 runner if the
  hosting cost is ever doubted again; do not re-argue it.
- **A solo `orchd` in a terminal keeps working**, so the host server is a library either
  process can run: the app runs one for N daemons, a solo daemon runs one for itself.
  `cargo run -p orchd` plus a browser tab is the fastest way to debug the daemon,
  and it is also what makes flow 25 headlessly testable.
- **The switcher is deleted.** With symmetric `close`, "switch" is `add` then
  `close`, and nothing privileged remains for a switcher to change.
- **A dead checkout is restarted, sessions and all**, rather than left as a row
  offering `reopen`. This **reverses** the plan's earlier answer, and the reason it
  gave stays true and becomes a bound instead: a restarted daemon runs `auto_resume`,
  so an unbounded restart of a daemon that dies *because of that checkout* — a
  `config.json` this build refuses, an untrusted `mise.toml`, a checkout whose
  directory went away — respawns N agents on every attempt.
  So the restart is bounded to **one retry**: the first death is restarted, the
  second is final and the row offers `reopen`. A restart that reached a ready line
  resets the count, so a daemon that runs for an hour and then crashes gets its retry
  again. One retry rather than a backoff because the two failures are not alike — a
  one-off death is worth a free recovery, and a checkout that kills its daemon twice
  is a checkout to look at rather than to keep restarting.
- **Closing a checkout keeps its session records, and `add` asks.** A re-add of a
  path whose `sessions.json` holds live records asks whether to resume them or start
  empty. Two reasons over clearing `was_live` at close: closing a checkout is a
  statement about the *window*, not about conversations, and a re-add of a path you
  closed an hour ago is exactly when resuming is what you meant. The cost is a
  dialog on a path that used to be silent.
  **An ordinary app restart still resumes silently.** The ask lives on the `add`
  path only, so today's restart behaviour is unchanged and `auto_resume` keeps one
  meaning. `add` and restart therefore differ, deliberately: a restart is the same
  window coming back, while an `add` is a decision you just made.
- **The host substitutes every child's token into the page.** The three shapes and
  their costs are in Stage 2 item 2. Named as its own decision because it is the one
  place this refactor touches the security boundary rather than the process boundary:
  `api::guard`'s token rule is what stands between a loopback port and anything that
  can reach it. The child keeps minting its own token, so nothing is handed *into* a
  process that spawns sessions.
- **One port per daemon. The host proxies nothing.** An earlier draft priced this as
  "N+1 loopback ports and N+1 Origin rules that must agree", left it open, and was
  wrong about the second half. Reading `api::guard` settles it:
  - `host_allowed(host, port)` checks the `Host` header against **this daemon's own**
    port. The page fetches `http://127.0.0.1:<that daemon's port>`, so the browser
    sends exactly that. **No change at all.**
  - `origin_allowed(origin, port)` requires a *present* Origin to be this daemon's
    own, and the page's origin is the host's. So each daemon must accept **one**
    additional origin — the host's, the same string for every daemon, handed in at
    spawn. That is one rule instantiated N times with one value, not N rules that
    have to agree. It is the branch's `sibling_origin` idea, and its two properties
    carry over: `#[serde(skip)]` so the config file can never widen it, and the
    preflight arm answered before the token check.

  Against that, a proxy puts a **new component on the keystroke path**. The pty
  socket is nothing but small frames in both directions — this repo already had to
  set `TCP_NODELAY` because Nagle plus a delayed ACK is ~40 ms per round trip on
  exactly that traffic — and proxying means a second loopback hop, a copy, a task,
  and websocket upgrade, backpressure and close semantics that must not drop a frame.
  It also gives every checkout one shared point of failure, which is the isolation
  that made separate processes worth choosing in the first place. One extra accepted
  origin is cheaper than that by a wide margin.

  Two smaller points, in the same direction. Each daemon being its own origin gives
  each its own browser connection budget, where one proxied origin shares one —
  **unverified against WebKitGTK**, so it is a reason to prefer this shape and not a
  reason on its own. And a solo `orchd` in a browser tab is already the one-port
  case, so nothing special-cases it.
- **`host` is the word for the thing above the daemons**, and it was called "board"
  in the first draft of this document. Three reasons for the change. "Board" names a
  UI metaphor for what is a process role, and the plan then needed the same word for
  the concept, the library, the API, the file and the socket — five meanings, which
  is how the draft read. It pays no rent against the vocabulary in `README.md`, where
  every other word (session, workspace, worktree, checkout, swap, rail, drawer) names
  a thing in the product. And the thesis sentence argues against it: if it has to say
  "the board is the app", the name is fighting the sentence.
  `host` is already this codebase's word for exactly this role —
  `firstrun::BootstrapHost` is the trait by which the app gives a server it hosts the
  window side — so `host.rs`, `/api/host/*`, `/ws/host` and `host.json` extend a
  vocabulary rather than adding one.
  **Its one cost, named**: `api::guard`'s rules are about the HTTP `Host` header, so
  `/api/host/checkouts` sits beside "the Host rule" and reads confusingly for a
  moment. That collision is in one module; a metaphor would have been in every
  sentence.
  **The user-visible concept gets no name at all.** The rail lists checkouts. Nobody
  says "my board".
- **`checkout` is the word.** `repository` stays reserved for `state::Repos`, the
  GitHub owner/name pair. `peer` and `project` retire: a peer list containing
  yourself is not a peer list, and with the switcher gone no verb needs `project`.
- **One checkout per repository. The same repository twice is refused.** Not only
  the same path.
  **The key is the repository the daemon would poll, not the `Repos` pair**, and an
  earlier draft said the pair — which cannot refuse the case the rule exists for.
  `Repos` is `{ upstream, fork }` and `upstream_remote` defaults to `origin`, so a
  parent clone on defaults derives `{acme/mono, acme/mono}` while a fork checkout
  (`upstream_remote: "upstream"`, this repo's own layout) derives
  `{acme/mono, you/mono}`. Unequal pairs, so `add` would allow both — and both then
  poll `acme/mono`, which is the hazard itself. So the key is the polled repository:
  `cfg.repo` when set, else the owner/name derived from `upstream_remote`, which is
  the value the PR poller actually reads. Reason: "one live fix run per PR" reads *this*
  daemon's `automation.json` and `branch_busy` reads *this* daemon's workspaces, so
  two force-pushing runs against one head ref is reachable and neither guard can see
  the other. Two guards that cannot see each other are not a guard.
  This is a **standing rule, not a placeholder** — the refusal is not lifted by
  making the fix-pr run key forge-global, because the layout buys nothing a second
  worktree in one checkout does not buy more cheaply.
  **The check is two-phase, because `add` cannot know the answer on its own.** The
  identity needs `upstream_remote` and `repo`, and both live in that checkout's own
  `config.json` — the file the host is about to create.
  - **At `add`**, the host derives with whatever config already exists for that path
    (a re-add has one) and the default `origin` otherwise, and refuses a clash. That
    catches the ordinary case cheaply, before a process is spawned. It reads the
    remotes itself through `proc::run_blocking` — `forge::remote_url` is a
    `std::process::Command` and the host runs on a tokio worker.
  - **At start**, the daemon re-derives with its *real* `upstream_remote` and reports
    its identity on the ready line. The host compares, and a clash it could not see
    at `add` is named on the row rather than silently tolerated. This is the only
    place the authoritative answer exists.
  And both `Repos` fields are `Option` — a checkout with no matching remote has no
  repository identity, so two of those are **allowed**, because refusing them would
  refuse every local-only checkout after the first.

## Order of work, and what each step buys

| Step | Commits on `main` | Buys | State |
| --- | --- | --- | --- |
| Stage 0 | tests + pending flow 25 + CI clippy | A boundary you can move safely | **done** |
| Stage 1 | the portability fix, the stale TODO | Less to move | **done** |
| Stage 2 | host + one child | One hosting model; the lock keyed right | **done** |
| Stage 3 | N children | The host API; the last three safety findings closed | **done** |
| Stage 4 | the page | The product | **done** |

Stages 0 to 2 change nothing a single-checkout user can see. That is deliberate:
the refactor is justified as required for planned work, and it is paid for before
the feature exists so the feature arrives small.

## Where the work stands, for a session picking this up

**Stages 0 to 4 are on `main`, and the feature is in the product.** Several
checkouts show at once in one rail, each with its own daemon; `+ checkout` opens
another and a checkout header's menu closes it. Every gate is green: 526 lib
tests, 8 desktop, 4 integration binaries, `cargo clippy --workspace
--all-targets` clean, `mise run check-web` green, `mise run e2e` 24 passed (see
`TODO.md` for the swap flows' pre-existing flakiness).

**One thing is unverified and needs a person at a screen**: the Tauri window.
Everything else was driven headlessly — the integration tests for the host and
child, and a real Chrome against `orchd --host` for the page. Run
`cargo run -p orchestrator-desktop` once, and expect one process per checkout
plus the app.

### What exists now, and where

- **`src/host.rs`** — the page, the assets, the window commands, the checkout list
  and the four commands that change it (`add`, `close`, `reopen`, `pick`), plus
  `/ws/host` which pushes the list. `sweep_checkout_dirs` reaps derived state only.
- **`host.json`** — `checkouts` and `checkout_retention_days`, in the host's own
  config dir. No file falls back to `config.json`'s `main_checkout`.
- **`src/child.rs`** — `ready <port> <token> <repo>`, the one observer, the
  `stopping` flag, `--no-resume`.
- **`orchd --host <checkout>…`** — a host in a terminal. The app is the only other
  host and it needs a window, so this is the one way to drive the multi-checkout
  page without a screen, which is what `mise run shot` exists for.
- **`web/js/core.js`** — `CHECKOUTS` (a `let`, reconciled by `setCheckouts`),
  `activeCheckout()` derived from the selection, `snapshotOf`/`snapshotFor`,
  `callFor`, `everySession`, `enterCheckout`, `termKey`, `wsKey`, `bandOf`.
- **`web/js/rail.js`** — one block per checkout, each from its own snapshot.
- **`tests/host_and_child.rs`**, **`host_checkouts.rs`**, **`host_file.rs`**,
  **`host_sweep.rs`** — one binary per fixture, because `ORCHD_CONFIG_DIR` is
  process-global and cargo runs a binary's tests in parallel.

### What is left

- ~~**Per-checkout identity outside the rail.**~~ **Done, and narrower than the
  item said.** The diff header and the drawer were the named suspects and neither
  needed anything: every pane sits on one row beside the identity chip, and the
  diff overlay spans rail-to-files, so the chip stays visible with it open. What
  genuinely lacked identity was everything read *away* from that row — the three
  destructive confirmations (swap, move out of main, delete), the toasts that
  outlive the glance that raised them, and the waitbar, which counts across
  checkouts and can move you out of the one you are in. All say which checkout now,
  and only when there is more than one.
- **The rename in Stage 4 item 10 was a no-op.** The SPA never used `repo` to mean
  a checkout — only `state::Repos`, the GitHub owner/name pair, which keeps the
  word. Checked before writing anything.

### Five things learned building stages 3 and 4, that the plan did not know

- **Every titlebar button was silently dead under the app**, since Stage 2. `call`
  aims at the checkout's daemon, and a daemon answers `200 {}` to a route it does
  not have — so minimise, close, drag, resize and restart all succeeded at nothing.
  `core.HOST` is the fix, and `tests/host_and_child.rs` asserts the swallow so the
  seam cannot be tidied away.
- **A page served by a host is cross-origin to every child daemon, and nothing
  answered the preflight.** Every call carries `x-orch-token`, which makes it a
  non-simple request; the browser asks with `OPTIONS` and refuses to send the real
  one unless the answer names its origin. The board drew from its websockets, which
  CORS does not cover, and could then do nothing. This is the half of the
  "one extra accepted origin" decision that only the guard had.
- **The restart bound could never bite.** `open_checkout` cleared the retry on
  every successful start, and every start reaches a ready line — so a daemon that
  died on boot was restarted forever, each restart running `auto_resume`. A long
  life clears it now (`HEALTHY_UPTIME`), which is what the plan's two sentences
  about it mean together.
- **A hosted child must not write the host's files.** Its `ORCHD_CONFIG_DIR` is its
  own checkout directory, so `recent.json` written by a child leaves one
  single-entry list per checkout and none of them the one the add screen reads.
- **"Activate a checkout" has to mean "select something in it"**, because the
  checkout is derived from the selection — and a checkout with *nothing* in it
  needs one remembered path to hold you there, or the derivation puts you back in
  the first one. That fallback is the only state beside the selection, and a
  selection always outranks it.

### Three things learned while building Stage 2, that the plan did not know

- **The daemon answers `200 {}` to an unknown route.** So a test asserting "the child
  no longer serves the page" must check the *body*, not the status. That catch-all
  makes any misrouting look like an empty daemon.
- **`--announce` needs a live stdin pipe.** Run it by hand from a shell and the
  daemon exits at once: stdin is `/dev/null`, and EOF is the second kill switch.
  `tests/host_and_child.rs` is how this pair gets driven; a terminal cannot.
- **The tests that write a stub and exec it hit `ETXTBSY`**, intermittently. It is a
  fork race — a sibling thread's `fork` copies the write fd until its own `exec` —
  not a defect, and `launch_stub` retries it.
