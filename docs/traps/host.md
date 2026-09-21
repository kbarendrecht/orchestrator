# The host, its children, and their state

Every entry here cost something. `CLAUDE.md` indexes them by their first
line; this file is the rest — what happened, what was measured, and why the
code is the shape it is.

## A checkout's daemon is a child process, and `crates/orchd-base/src/child.rs` is the protocol.
`child::launch` runs `orchd --main <checkout> --host-origin <origin> --announce`,
reads one line — `ready <port> <token>` — and arms **one** observer thread that
owns `wait()`. Four things about it are load-bearing.
**The child mints its own token and reports it.** Handing one down through the
environment would put it in the environment of every session that child spawns,
which is the invariant `triage.rs` asserts.
**The observer has to know *why* the child exited.** A close, a quit and a crash
produce the same EOF, so an observer that restarts on exit restarts the daemon a
close just stopped — and a restart runs `auto_resume`, which spawns an agent per
live record. So a deliberate stop sets `stopping` **before** it signals, and the
observer reads it after `wait` returns. Ownership follows from `std::process`:
`wait` needs `&mut self`, so the handle lives in the observer and every stop path
signals **by pid**, through `pty::signal_group_of` (which refuses pid 0 and our
own group — `killpg(0, …)` would take the host).
**Stdin EOF is the second kill switch**, so a `SIGKILL`ed host still takes its
children down, and `--announce` turns the child's *stdout* subscriber off: that
pipe is the parent's protocol channel, and every line is in the file log anyway.
**The binary is `orchd`, beside the running executable — never a re-exec of the
app.** `ldd` says why: `orchd` links 5 shared objects and
`orchestrator-desktop` links 133, twenty of them WebKit and GTK, and a child would
pay that loader cost to serve a page it never serves.
**A child process rather than an embedded daemon is measured, not assumed**, and
the number is the answer to the question somebody will ask again. Release build,
wall clock from `Command::spawn` to the daemon serving, minus the `daemon start`
phase the daemon logs itself — so it is exactly what the extra process costs on
top of the `orchd_serve::start` both shapes run: **2.6–2.7 ms and 11 execs**, and
**9.3 MB RSS** idle per daemon. The delta does not move when the repo work goes
up 58× (a throwaway checkout against this one, 23 ms against 1334 ms of
`daemon start`), which is what says it is exec plus loader and nothing else.
It is 0.2% of a real start, and the children start in parallel.
One test-only wrinkle worth knowing: those tests write a stub and exec it, and
`ETXTBSY` there is a **fork race** (a sibling thread's `fork` copies the write fd
until its own `exec`), not a defect — `launch_stub` retries it and says so.

## A checkout's durable state lives in its own directory, and `ORCHD_CONFIG_DIR` is how it gets there.
`host::checkout_dir` is
`<config dir>/checkouts/<leaf>-<hash>`, hashed over the **checkout path alone** so
the directory is a function of the checkout and nothing else; the leaf is for
reading a bug report by eye and is not the key, because two checkouts can share
one. The host hands that path to the child as `ORCHD_CONFIG_DIR`, which relocates
every durable thing at once — config, `sessions.json`, `automation.json`,
`hooks.json`, the skills plugin dir, transcripts, the log and the instance lock.
One variable rather than a flag per store, because a flag per store is one
somebody forgets and two checkouts then share a file. It is also what makes a
fixture daemon safe. **Overriding `HOME` would relocate the same things for free
and is wrong** — `claude` reads its credentials from there, so every spawned
session would come up unauthenticated. The one exception is `mise run e2e`,
where the agent is a fake with no credentials to lose, so relocating `HOME` is
what keeps transcripts out of your `~/.claude/projects`.
**The move from the old single `config.json` is a one-shot in `host.rs`, not a
`migrate.rs` rule**, for two reasons that are easy to get wrong. That table's
`apply` is `fn(&mut Map<String, Value>) -> bool` and `config_file` ends in one
`fs::write`, so a rule there cannot create a directory or write a sibling file.
And its shape would have been wrong anyway: `main_checkout` **stays** at the root
of every per-checkout file, so a rule keyed on that key re-fires on every start of
every daemon, forever. The shape recognised instead is a *location* — this
checkout has no directory yet — and the old file is **copied, not moved**, so an
older build still finds its config. The copy happens only when the root config
names *this* checkout, because a copy carries `main_checkout` and seeding a second
checkout from it would point that daemon at the wrong tree.
**Window geometry did not move**, and that is worth knowing rather than checking:
`store::save_window` / `load_window` are called only from the desktop crate, which
is the host process, so the geometry already lives in the host's own config dir
and one window still has one geometry.

## A migration that copies a list of files is a migration that loses the file nobody listed.
The per-checkout state directory seeded itself with one `fs::copy` of
`config.json`. Everything else the daemon keeps beside that file — `sessions.json`,
`automation.json`, `stories.json`, `transcripts/` — stayed at the top level, so
v2026.9.14 opened the board with an **empty rail** while 28 session records sat on
disk in a file the new daemon no longer read (#17). Nothing was deleted and nothing
said so, which is the worst way for a migration to fail: a user who rebuilds the
rail by hand pays for a copy that never happened, and the only sign is a board that
looks like a fresh install.

**The fix is the inversion, not the longer list.** An allow list is wrong by
omission, and it is wrong again the day somebody adds a state file — a day nobody
will be looking at the seed. `NOT_INHERITED` is a deny list of three structural
names (`checkouts` is the parent of the destination, `host.json` is the host's, and
`instance.pid` is the file the `flock` is taken on) plus `DERIVED` by reference and
`orchd.log*` by prefix. Everything else is carried without anyone remembering.
`DERIVED` is taken rather than restated because two hand-kept copies of one list is
the same fault one level up, and `crates/orchd-serve/tests/host_seed.rs` reads it
too.

The assertion that holds it is not "the session store arrives" — an allow list
passes that. It is a fixture file named for the fact that nothing knows it: state
the seed has never heard of has to reach the checkout, or the test fails. **Checked against
deliberate breakage**: restoring the single `fs::copy` fails it.

**Keying the one-shot on "the directory exists" was the second half of the bug.**
That test cannot tell a directory this build created from one v2026.9.14 created
and half-filled, so every machine that had already updated was permanently past its
only chance to be seeded — a fix for fresh installs and nobody else. A `.seeded`
marker separates *seeded* from *present*, and the repair may overwrite only what
cannot lose anything: a destination that is absent, or that holds `[]`/`{}`. `[]` is
exactly what the new daemon wrote over the empty rail. A file with content in it is
left alone and **named in a warning**, because choosing between two versions of
somebody's work is not a migration's to make.

One thing found while reading that file and worth its own line: `HostFile` derived
`Default` while `checkout_retention_days` carried a serde default of 60, and
`remember_checkouts` writes `read_host_file().unwrap_or_default()` — so the first
`host.json` a machine ever wrote pinned retention to `0` and the documented 60 never
applied to anybody. `0` turns the sweep off rather than on, so it erred toward
keeping data and nobody noticed. `Default` is hand-written from the same function
now, and `the_two_defaults_agree` compares the whole struct rather than that one
field, so the next one is caught without anyone extending the test.

## The app is the host, and every checkout is a child `orchd`.
`boot_daemon` in
`desktop/src/main.rs` runs `host::serve` on an ephemeral port, calls
`Host::open_checkout` for the configured checkout, points the webview at the
host's URL and calls `host.stop_all()` at quit. `cargo run -p orchestrator-desktop`
therefore starts **two** processes, and `cargo run -p orchd` still starts one that
serves its own page.
Four consequences, each of which has already bitten or nearly did.
**The page's calls do not go to the page's own port.** `core.LOCAL` reads the
substituted checkout list and aims `base`/`wsBase` at *that checkout's* daemon —
relative when they share a port (a solo `orchd`), absolute when they do not (the
app). A relative fetch under the app would reach the host, which answers `{}` to
an unknown route, so the failure would look like an empty daemon rather than a
misrouted call.
**A hosted child does not serve the page.** `orchd_serve::start` mounts the host router
only when `host_origin` is absent, which is exactly the question "did somebody
host me". Its `/` then falls through to the catch-all and answers `200 {}` — so a
test for this must assert on the *body*, not the status.
**The instance lock is the child's**, taken inside its own `orchd_serve::start`. A
second app on one checkout now surfaces as a child that never reported ready,
which is a worse message than the old refusal and is why the lock re-key is still
on the list.
**`--announce` needs a live stdin pipe.** Run that flag by hand from a shell and
the daemon exits at once, because stdin is `/dev/null` and EOF is the second kill
switch. `tests/host_and_child.rs` is the way to drive this pair; a terminal is not.

## First run is a screen, not a second application.
`firstrun.rs` used to serve
its own axum server on its own port, with its own router, its own Host/Origin
guard, a `BootstrapHost` trait for the two things that need a window, and a
500-line HTML page carrying a copy of the SPA's palette and a titlebar of its
own. That titlebar had to learn the macOS window-drag rule a second time, four
days after the board did, and the same split put two sets of window buttons on a
Mac.
**The board already had the journey.** `+ open project` lists the same recents
and raises the same dialog through `/api/host/recent` and `/api/host/pick`. What
it did not have was the *review* — base branch, GitHub repo, environment tool,
the repo's own dev processes — which therefore ran once per install and never for
a checkout added from the rail.
So the app opens one window whatever is configured, `boot_daemon` treats an empty
host as a screen rather than a `fail`, and `web/js/open.js` is that screen:
welcome when no checkout is open, review before any folder the host has not seen.
`host::validate` and `host::detect` are the two routes it needed.
What went with it: the bootstrap server, `BootstrapHost`, `TauriBootstrap`, the
`BOOTSTRAP` and `BOOTING` statics, `Written::undo`, `/api/context`,
`/api/cancel`, that page, and a test which string-searched its own
JavaScript for a `DRAG_SLOP` — a test whose only reason to exist was the
duplication.

## The page is served by `host.rs`, not by the daemon.
`crates/orchd-serve/src/host.rs` owns
`GET /`, every asset route, the window commands, the checkout list and the four
commands that change it (`add`, `close`, `reopen`, `pick`); the daemon keeps
`/api/*`, `/ws/*` and `/hooks/*`.
**So a window command must go to the host**, and the page's `call` does not —
it aims at `core.LOCAL`, the checkout's own daemon, which answers `200 {}` to a
route it does not have. Every titlebar button shipped silently dead under the
app that way: minimise, close, drag, resize and restart all succeeded at
nothing. `core.HOST` and `callHost` are the seam, and
`tests/host_and_child.rs` asserts the swallow so the reason cannot be tidied
away. **They are separate processes on separate ports with separate tokens**:
the host serves the page from the app, and every checkout is a child `orchd`
that mints its own. The two routers answer to different owners — a daemon
manages one checkout, and there is one page over all of them.
Three consequences worth knowing. The window handle is on `host::Host`, not
`AppState`, so `server.host.attach_window` is what the shell calls. The page's
token now comes from the substituted checkout list (`core.CHECKOUTS[0].token`),
falling back to `__ORCH__.token` for the review-preview page. And `/hooks/*`
**cannot** move: `hooks.rs` writes that daemon's own port into its own settings
file, so an agent's hook URL is the port of the daemon that spawned it.

## A child that will not start leaves one sentence, and it used to name nothing.
Issue #18: an app that showed `the daemon never said it was ready` and nothing
else. The daemon had died 644 ms in, the per-checkout log was empty because it
never got as far as writing one, and the reporter spent a day reconstructing a
timeline from two log files to arrive at "the cause is not in either of them".
Three separate places threw the diagnosis away. **The exit status was in hand and
dropped** — `launch_at` called `child.wait()` and discarded the result, so a
binary the kernel refused (no exit code at all, only `SIGKILL`) and a daemon that
exited 1 were the same sentence. **stderr was `inherit()`**, which for a
launcher-started host is `/dev/null`; an `anyhow` chain out of `orchd`'s `main`
exists *only* there, because `--announce` sends the log to the file and stdout is
the protocol pipe. And **the log path was never printed**, so the one file that
might have held something was under `checkouts/<leaf>-<hash>` with no way to find
the hash from a checkout that never opened.
It carries all three now — `(it exited with code 9); it said: … ; its log is at
…` — with the status read **before** the kill, or the only thing it could report
would be the signal it was about to send. stderr is piped and drained on its own
thread, logged line by line and kept to the last `STDERR_KEPT`, because a pipe
nobody reads fills and then blocks the daemon writing into it.
`tests/host_and_child.rs` asserts all three against a **real** `orchd` refused by
the instance lock, which is what proves the trip works end to end; the unit tests
beside it drive shell stubs and cannot.
The same report's other half was a day earlier and read `starting orchd for
<checkout>: No such file or directory`. The binary in that message has no
directory in front of it, which is the only sign that `daemon_binary`'s sibling
lookup had missed and it had fallen back to a PATH lookup. The fallback says so
now, one line before the failure.
**Not a lint.** `clippy::let_underscore_must_use` would have caught the dropped
`wait()` and fires at **273 sites** across the workspace, nearly all of them a
deliberate `let _ = tx.send(…)` — a lint that size is a lint people learn to
`#[allow]`, and the habit then covers the real ones.

## A deadline checked after a blocking read is not a deadline.
`READY_TIMEOUT` is 60 seconds and could never fire. `read_ready` iterated the
child's stdout lines and tested the deadline **inside the loop body**, so the test
was only reached once a line had arrived — and a child that is alive and silent
sends none. The host's thread parked for ever, and the constant read as a
guarantee in a doc comment that nothing enforced. It is nobody's fault in review:
the timeout looks right on the line it is written on.
The pipe is read by its own thread now, one line per channel message, and
`recv_timeout` is what the deadline is on. Three outcomes rather than one, because
the next step differs for each: stdout closed (`Unready::Gone`, and there is a
status to report), nothing said in time (`Unready::Silent`, the daemon is wedged
rather than broken), and a malformed ready line.
**The test for it answers over a channel rather than asserting in place**, because
the regression does not return a wrong value — it never returns, and a test that
called `read_ready` directly would hang the binary and surface as a CI timeout.

## A buffer another thread is filling is empty when you read it, and a release paid for that.
`child::launch` reads the daemon's stderr on a thread of its own, because a child
that is alive and silent must not park the launcher — `drain_stderr` keeps the last
`STDERR_KEPT` lines in a `Mutex<VecDeque>` and the failure path quotes them.
**Nothing made the failure wait for that thread.** A child that writes its
complaint and exits loses the race on a loaded machine: the parent sees the missing
`ready` line, formats the diagnosis, and reads a buffer the reader has not been
scheduled to fill. The sentence then names the exit status and says the child said
nothing — which is precisely the silence #18 exists to end, reintroduced by the
code written to end it.
**It read as flake and was a release.** `a_failed_launch_names_the_status_the_stderr_and_the_log`
had failed occasionally for days and passed on every rerun, so it read as noise.
Then it went red in the `build` leg of a tag: v2026.9.20 was never published, and
`release` was skipped rather than failed, so the run looked half-fine.
`settle` waits for the reader with a 500ms bound, on the failure path only.
**Bounded, and never a `join`**: the pipe of a child that is alive and silent never
reaches EOF, and that is exactly the case this path may be in.
**Measured, because a race that is argued about is a race nobody fixed.** With
sixteen `yes` processes competing, the wait removed again fails 2 runs in 8; with
it, 0 in 12.

## The checkout's daemon is a second binary, and building the app alone leaves it stale.
`cargo build --release -p orchestrator-desktop` builds the window. Every checkout
runs `target/release/orchd`, which comes out of `orchd-serve` — so a route added
to the daemon and tested by rebuilding only the app is a route the running child
does not have. Build both: `cargo build --release -p orchd-serve --bins -p
orchestrator-desktop`.

**It fails silently, and that is the expensive half.** An unknown route on this
daemon answers `200 {}` rather than a 404 (the same fact `docs/traps/git.md`
records for driving the API by hand), so `callOn` sees a successful call with no
error field and the page does nothing at all — no toast, no log line, nothing to
search for. It cost a bug report that read as "both right-click actions did not
work", on a feature whose daemon half had never been built.

