# Performance, measured

Every entry here cost something. `CLAUDE.md` indexes them by their first
line; this file is the rest — what happened, what was measured, and why the
code is the shape it is.

## A start is a pile of child processes, and that is why it is slow somewhere else.
Almost nothing in `orchd_serve::start` is CPU work, so "better hardware, worse
start" is not a contradiction: the cost is per exec, and a Mac pays dyld on every
one plus whatever endpoint-security software a managed laptop carries. Measured
on the real monorepo, release, **64 worktrees: 7836 ms and 447 child processes**,
of which `reconcile_all` was 6294 ms and the upstream fetch 1479 ms. Everything
else in `start` came to 59 ms. `crates/orchd-base/src/timing.rs` is what says so — a phase line per
start (`daemon start`, `session … start`, `shell start`, `window open`), each
carrying its own exec count and its own share of the time in them, plus `slow
git` for a single call over 300 ms and a `page start` line from the SPA.
**The sweep is now spawned rather than awaited**, which is what took the start to
~1.4 s: `reconcile_all` holds `AppState::sweeping` so boot and the PR poller's
first tick cannot overlap, walks [`sweep_order`] (sessions, then main, then the
rest) so the pane you land on fills first, and notifies per workspace so they
fill in as it goes. **The page was never the problem** — 570 ms to a painted
terminal in that same reading, which is why the SPA posts its own boot marks to
`/api/client/timing`. What is left on the critical path is the **upstream fetch,
which is a network round trip**. Two things still true and worth knowing:
`configure_repo` sets fsmonitor on main *only*, so every worktree's `git status`
is a full scan; and the poller's first tick deliberately skips the fetch and the
sweep, because boot has just done both.
One follow-on lives in the desktop crate: the login shell's PATH is remembered
in `<config_dir>/login-path` and refreshed in the background *for the next
launch*, because `set_var` is process-global and unsound beside threads — which
is why `adopt_login_path` runs before the runtime exists and why the refresh
must never apply itself.
**The sweep runs four wide, not one** (`SWEEP_WIDTH`), because its cost is execs:
seven git processes per tree at 8 to 9 ms each on a Mac, so 58 trees took 20 to
46 s in a row (#10) and the per-tree half is nothing a user can change. And it
**skips a workspace whose directory is gone without dropping the row**: the row
is where `revive` and the PR flows rebuild the tree, so dropping it would trade
a warning per sweep for a second tree on the same branch. The tally says how
many were skipped.

## A keystroke is one small frame, so the served sockets set `TCP_NODELAY`.
`axum::serve` defaults it to `None` and only calls `set_nodelay` when the
builder is told to, so every connection ran with Nagle on: a small write waits
for an ACK that waits for the peer's delayed-ACK timer, the classic ~40ms per
round trip. The pty websocket is nothing but small frames in both directions.
Loopback made it look like it could not matter, and on Linux it mostly does not.
**Three servers bound a port here and the third forgot it** — the bootstrap
server the first-run page ran on, which no test and no log could have told you
about. That one is gone with the page, and two are left. `serving::spawn` is the
one call, and `clippy::disallowed_methods` refuses `axum::serve` anywhere else,
so the next one gets it by construction.

## `mise env` per spawn is a decision, not an oversight.
`env_source`'s own
docblock says why: caching it needs invalidation against files the daemon does
not watch, and a session with a stale environment is a worse bug than a slow one.
The 50ms floor `proc::run_bounded` used to add is gone (it backs off from 2ms),
and the cost is now in the log per spawn. Revisit it with a number, not a guess.

## A resume rebuilds the record from `spawn::Carried`, and anything not named there is thrown away.
`SessionRecord` persists twenty fields and `restore`
puts them all back at boot — then `auto_resume` respawns through
`spawn_session`, which rebuilds the record under the same id from `Carried`
alone. So a field that persists but is not carried is restored and discarded a
moment later, and the only sign is a behaviour that quietly stops working after
a restart. `created_at` was the first (a resumed session claimed to have started
this second, which silenced `claim_stale_warning` for the one session that
needed it); `spawned_by`, `spawn_cut_worktree` and `forked_from` were the rest.
The first pair is the sharpest, because it is the one fact on the record with
**no other home**: everything else either persists or heals itself from disk — a
title from the transcript, a branch from the tree — while "which session spawned
which" exists nowhere else. Lost, an agent that restarts can no longer undo the
child it created, and `api::discard_spawned` refuses with the opposite of what
happened.
Deliberately *not* carried, each for a reason: `recovery` (the tree is rebuilt,
so the session is no longer archived), `ask_token` (re-minted per spawn by
design), `outside_grants` (a restart asks again rather than assuming), and
`state`/`pty`/`pid` (a new process). Before adding a field to `SessionRecord`,
decide which side of that line it is on.

## Every spawner records the session's branch, and the swap depends on it.
`Session::branch` is what `api::to_carry` matches on to decide which
conversation travels when a branch moves, so a record with `branch: None` is a
conversation the swap silently leaves behind — the branch goes into main and the
agent that was working on it stays put, with no error anywhere.
`spawn_worktree_session` built its own `Session` and never set it, so a worktree
session had no branch until a `reconcile` of its workspace happened to run.
Pressing swap before that sweep lost the conversation.
It surfaced as the swap e2e flows failing about one run in three, which reads as
a slow resume and is not: `E2E_TIME=1 mise run e2e` prints how long each wait
took, and every wait that *succeeds* lands in 3–7ms against a 10s deadline. A
flaky wait here is a condition that never becomes true, not one that is slow —
check the numbers before reaching for a longer timeout.

## Nothing may read `Tree` without asking whether it has been measured.
Every
field on it defaults to a value indistinguishable from a real answer: no changed
files is a clean tree, `changed_total` 0 is zero files, `(0,0)` divergence is up
to date, `branch: None` is... nothing. That was invisible while the first sweep
finished before the window opened, and became a lie the moment it did not.
`Tree::measured` is the difference, and it reaches the SPA on `WorkspaceView`.
The changed-files pane shows a loader on `false` (`.fempty.counting`, reusing
`.conn-dot` so the reduced-motion rule that names it already covers it) and its
footer says `counting…` rather than `0 files`. The two other readers already
degraded correctly and their comments say why: the rail's swap affordance treats
an unknown branch as the cautious answer, and `api.rs`'s post-swap mismatch
warning treats it as "not a mismatch". Teardown never trusted the cache at all —
`worktree::preflight` measures unpushed work with a fresh `git::unpushed`, which
is why deferring the sweep costs no safety.

## A launcher-started app has no stdout, so it used to leave no log at all.
That
is why a colleague's slow start could not be looked at: `tracing` went to a
terminal nobody had. `logging::init` writes the same lines to
`<config_dir>/orchd.log`, one generation kept as `orchd.log.1`, and says the path
in its first line. It follows `ORCHD_CONFIG_DIR`, so a fixture daemon does not
write over the real one.
**It lives in `orchd`, not in the desktop shell where it was written**, and both
hosts call it — so `cargo run -p orchd` leaves a file too, which it did not. The
reason is the multi-checkout work: a checkout's daemon is a child process whose
stdout the parent reads one line of and then drains, so a stdout-only subscriber
in a child logs nowhere anybody looks. `install_panic_hook` moved with it, for the
same reason and because it writes through that subscriber.

## The page's own boot timing is not visible from Rust.
The daemon can time up
to serving the page and sending the first snapshot; the vendored script parse, the
first render, and the centre pane's terminal attaching and painting only exist in
the webview. So `core.js` marks them and POSTs once to `/api/client/timing`, which
logs a `page start` line beside the daemon's. Marks are first-wins, because
`attach` and `paint` repeat on every session switch and a later one is not boot.
While reading those numbers: `index.html` loads `prism.min.js` (574 KB) and
`addon-webgl.js` (247 KB) as blocking classic scripts, and the webgl addon is
dead weight in the desktop window, where `CHROME !== 'none'` never loads it.

## Measure a release build, or do not quote the number.
"orchd uses 76 MB" was a
`cargo run` debug build — 113 MB of binary against release's 11 MB, nearly all
paged-in debug text. Release, idle, polling: 7.6 MB RSS and **1.1 MB** of heap.
Two performance suspects were chased on the strength of the wrong figure. For the
same reason `web/js/term.js` pins `scrollback: 2000`: xterm holds each line as a
`Uint32Array` of `cols * 3`, so depth costs process memory whether or not a
terminal paints (10000 lines cost +36.7 MB against 2000's +13.3 MB) — and the
daemon's ring buffer only replays ~3600 lines anyway, so a deeper buffer was
never durable. JS-heap metrics are useless here: CDP reported 0.9 MB for 9000
lines that cost ~23 MB, because typed-array stores are external memory.

## Creating a worktree is 4.7 seconds; claiming a pre-cut one is 84 milliseconds.
**Measured by the daemon, release build, against the monorepo** — 18,925 tracked
files, six creates, a fresh daemon per sample so none inherits the last one's page
cache. The `worktree ready` line in `spawn_worktree_session` is where the numbers
come from:

| | `worktree ready` | whole HTTP create |
|---|---|---|
| cut (no pool) | **4,742ms** | 4,930-5,136ms |
| claimed | **84ms** | 258-261ms |

A **56x** difference on the part the pool touches, and the create as a whole goes
from about five seconds to about a quarter of one. The pool's own background cut
costs the same as the create it replaces — `cut a spare worktree took_ms=4559` —
which is the whole point: the work is not avoided, it is moved off the path
somebody is waiting on.
**Where the five seconds go**, from `slow git` in the same run: `git fetch upstream
develop --no-tags` 1,367-1,417ms (the daemon's own, on the boot path), `git fetch
--quiet upstream HEAD` 1,517-1,568ms (the repo's `worktree-create` hook, which
fetches again), and `git worktree add` over 18,925 files, hand-timed at 1.4s.
**The remaining ~175ms of a warm create is not the tree**: it is the pty spawn and
`mise env` for the session's environment, which `mise env per spawn is a decision`
above already accounts for.
**Two corrections this run produced.** The claim was first bracketed *below* the
claim call, so the claimed arm logged `took_ms=0` — true and worthless. And the
first claim against a just-cut tree costs far more than the steady state: **524ms**
measured once, against 84ms afterwards, because `verdict`'s status walk is reading
a tree that was written seconds earlier. A hand-timed `git status` on a long-warm
tree (49ms) is the floor, not the figure.
**Dependencies cost nothing here and that is the repo's doing, not the daemon's.**
A worktree is 216 MB because `worktree-link` symlinks `node_modules` and `vendor`
out of main; 24 of them were 5.1 GB. A repo that copies instead pays that per
spare, which is what `spare_worktrees: 0` is for.
**The broken-symlink scan is cheap for the same reason it must not follow links**:
`find -xtype l` over that tree is 30ms across 29 links, because the heavy trees
*are* the links and nothing descends into them.
