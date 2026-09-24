# The gates, and what each one caught

Every entry here cost something. `CLAUDE.md` indexes them by their first
line; this file is the rest — what happened, what was measured, and why the
code is the shape it is.

## The SPA is compiled in.
Everything under `web/` is `include_str!`d, so a
CSS or JS change is invisible until the daemon is rebuilt *and* restarted. No
amount of reloading the page helps — and a stale process holding the port makes
this worse, because you are then debugging a build from ten minutes ago.
`pkill -x orchd` will not always do it: Linux truncates the process name to 15
characters, so `orchestrator-desktop` needs killing by pid.
**And never `pkill -f orchd`.** It matches the *agents* too: the daemon passes
the vendored prompts on the command line, they contain the word, and `-f` reads
the whole command line — so it killed two live Claude sessions along with the
daemon. Kill by pid, or `pgrep -x orchestrator-de` for the app.

## There is a pre-commit hook, and it needs enabling once per clone.
`git config core.hooksPath .githooks` — git will not let a repo point at its own
hooks, so a fresh clone has none until you say this. It runs only what the staged
files could break, and its own header says why that matters; `--no-verify` is a
fine thing to reach for mid-refactor, and the real gate is `mise run check-web`.
**Every Rust-or-`tools/e2e/` commit also runs the e2e flows**, ~50s instead of
~2s. It was every *fifth* such commit, on a counter in `.git/`, and the argument
for that was the cost — which is real and unchanged. What retired it is what the
count bought: a regression up to four commits from its cause, found by a bisect
somebody still has to sit down and run. A docs or SPA commit still runs nothing,
and `--no-verify` is still the way past a mid-refactor commit.
**`check.yml` runs them too now, and the hook is no longer the only thing that
does.** It was: no workflow ran `tools/e2e/run.mjs` at all, so a fresh clone, a
`--no-verify` habit or anybody who never said `git config core.hooksPath
.githooks` skipped all 41 flows, and the class of fault they exist for reached
nobody. The bar this entry set was to measure the flake rate first, because the
flows had flaked twice and a flaky gate is worse than no gate: **seven
consecutive clean runs, 168 flow executions**, and both known flakes have a fix
behind them (`t.settled` before each call, and `spawn_worktree_session`
recording the branch). One machine and a fast one, so a runner may yet find a
timing fault this could not — if it does, read the numbers `E2E_TIME=1` prints
before reaching for a longer timeout. The hook runs them anyway, because a local
answer now is worth more than the same answer from CI in ten minutes.

## Splitting one working tree into several commits has two traps, and neither fails loudly.
`git diff -U0` splits finely, but `git apply --cached
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

## A filtered `cargo test` rewrites the generated types with only what it reached.
`cargo test -p orchd --lib export_bindings` leaves
`web/base.d.ts` holding one type where it held six, and `web/repo.d.ts` six where
it held fourteen. Nothing says a word: the file is valid TypeScript, `tsc` is
happy because nothing imported what went missing, and the loss is only visible if
you count.

**ts-rs exports a type's dependencies too, and it rewrites the whole target file
rather than merging into it.** All six of `base.d.ts`'s types are declared in
`orchd-base`, but `orchd`'s `Snapshot` reaches `DiffFile` — so exporting from
`orchd` writes `base.d.ts` containing `DiffFile` and nothing else. Measured, each
crate run alone from a full set, counting types left in each file:

| run alone | base | repo | snapshot | serve |
| ---------- | ---- | ---- | -------- | ----- |
| `orchd-serve` |  6 | 14 | 23 | 4 |
| `orchd`       |  **1** | **6** | 23 | 4 |
| `orchd-repo`  |  **1** | 14 | 23 | 4 |
| `orchd-base`  |  6 | 14 | 23 | 4 |

So the two ends of the stack are safe and the two in the middle are not, and
**that is what makes the order load-bearing** where the hook and `check-web` both
spell it — `orchd-serve orchd orchd-repo orchd-base`. Each later crate restores
what an earlier one thinned, and the crate that owns a file comes last.

Nothing wrong can reach a commit: the hook and `mise run check-web` both
regenerate all four and then refuse a generated file that differs from the index.
What this costs is a session, not a build — a thinned file sat in the tree four
times in one day, was twice mistaken for a real deletion, and once made a commit
look like it was dropping types somebody else had added. `mise run types` is the
four-crate loop under a name, so the manual path stops being a thing to remember
the order of.

## Inserting a test can unregister the one next to it.
An anchor on
`fn other_test() {` puts your test *between* that test's `#[test]` and its `fn`,
which leaves yours with two attributes and its neighbour with none — so it stops
running, and the count barely moves because yours now registers twice.
`swapping_exchanges_two_branches_and_is_its_own_inverse` sat unregistered in a
pushed commit that way. Anchor after the previous test's closing brace, and read
the test count.

## `mise run check-web` is the SPA's gate, and it bites.
The list of checks
lives in `tools/check-web.sh`, because this task and `check.yml`'s own step each
used to spell it out — and the third copy, in the pre-commit hook, had already
lost three entries. The hook still runs a subset deliberately; the two that
claim to be the whole gate now read one file. It regenerates
`web/snapshot.d.ts` and fails if the committed copy drifted, runs
`tsc --noEmit --checkJs` over every SPA file, runs `dependency-cruiser` over the
module graph, runs `eslint` with typescript-eslint's *typed* rules, holds the
palette, refuses a class `app.css` styles that nothing can produce, holds a
pane's `drop` list to field names the daemon still sends, and checks that every
module in `web/js/` has a route serving it. Each was checked against
deliberate breakage — a `#[serde(rename)]`, a typo'd `snap.` field, an added
cycle, an un-awaited `confirmBox`, an unwritten class, a misspelt dropped field
and a new module file each
fail it. There is still **no build step**: `tsc` only checks, and the files ship
exactly as written.
**`web/snapshot.d.ts` is generated, never hand-written**: it comes from the Rust
structs via `ts-rs`, derived under `cfg(test)`, so nothing of it reaches the
binary and it is never `include_str!`d or served. Rename a snapshot field and
the diff shows up there — which is the point, since the old failure mode was a
renamed field reading as `undefined` and rendering as nothing. Commit the
regenerated file with the Rust change; the entry on the crates says why four of
them are written in a fixed order.

## ESLint answers what `tsc` structurally cannot: the promise nobody awaited.
`tsc` knows a name and its type; it has nothing to say about a `Promise` used as
a boolean, and one shipped. Three settings are load-bearing and
`tools/eslint.config.mjs` says why beside each. The one to know before writing a
handler: `no-floating-promises` runs with `ignoreVoid`, so **`void f()` is how
you say "fire and forget" out loud** — 37 handlers do, and the next dropped
promise that did not mean to is the one the rule catches.

## The SPA type-checks under `strict`, and getting there found two bugs.
`tools/tsconfig.json` says what each pass cost and what it caught — the review
overlay reading three fields the daemon never sent, `diffState.anchors`
annotated `number[]` while holding elements. Three things it does not say.
**`$` throws rather than returning `null`**: every id it is asked for is in
`index.html`, which is compiled into the same binary, so a miss is the page and
the code out of step rather than a state to handle, and the throw names the id
at the call instead of surfacing three lines later.
**A parameter annotation goes in by line and column**, so never insert a line
into the same file in the same pass — everything after it lands in the middle of
a word, and the repair is manual.
And `el()` is generic on its tag (`@template {keyof HTMLElementTagNameMap}`)
rather than returning `HTMLElement`, which is what keeps `el('input').value`
checked instead of sending every form control through `ctl`.
The types come from Rust wherever there is a struct to take them from, so the
diff pane and the review overlay are checked against the daemon rather than a
hand-written guess. `/api/pr/:n/review` builds a `json!` literal with no struct
behind it, and that is the one shape `review.js` still describes by hand; it
says so where it does.

## `catch (e)` gives you `unknown`, and `core.reason(e)` is the one answer.
46 catch blocks all said `e.message`, which is `undefined` for a thrown string,
a `DOMException`, or anything else that is not an `Error` — and that word then
goes in a toast. `useUnknownInCatchVariables` is on, so the next one cannot.

## `mise run page-check` asserts what the page must never *show*.
Four faults
that are text rather than pixels, every one of which has happened here.
`tools/e2e/page.mjs` names them and argues why this is deliberately not a
screenshot test.

## `mise run check-ship` asserts what a release would *pack*, which no test can see.
v2026.9.14 shipped without `orchd` and could not start (#16). The app had just been
split into a host and a child daemon — `child.rs::daemon_binary` resolves `orchd`
beside the running executable — while `release.yml` still built `--bin orch` and the
three bundle maps in `desktop/tauri.conf.json` still copied `orch` alone. The macOS
tarball held two files. Every install method was affected, not the tarball alone,
and the first sign was a user on a fresh install reading "Orchestrator could not
start".

**Everything in this file was green for it.** `cargo test --workspace`, clippy with
warnings denied, `check-web`, `check-docs`, `check-modules`, `page-check` and 25 e2e
flows all passed on the commit that shipped it, because not one of them looks at
what gets packed. That is the gap the gate fills, and it is the argument for it: the
failure was expensive and silent, which is the standing reason to build a tool
rather than write a rule.

The rule is **derived, never listed**. `cargo metadata` says what binaries the
workspace produces; everything but the app itself must be built by the release job,
carried in the tarball, and copied by each bundle's `files` map. Add a fourth binary
and this fails until it is placed — the list nobody remembered to update is not
written down anywhere any more. Two subtler things it also pins: every entry in one
bundle map must land in **one directory**, because `daemon_binary` looks in
`current_exe().parent()` and nowhere else, so a bundle that installed `orchd`
somewhere of its own would fail exactly the way #16 did; and each source must be the
release build's own output. **Checked against deliberate breakage**: run against the
v2026.9.14 tree it fails on all five counts.

It runs in three places for one reason each. The hook, on a commit touching a
manifest, `release.yml` or `tauri.conf.json` — #16 was three files and not one of
them is `.rs`, so the Rust condition would have missed it. `check.yml`, so `main` is
never in that state. And `release.yml` itself, before the build, because a tag is
the one build nobody re-runs and the cost of finding out afterwards is a spent
version number.

## Type-checking found bugs clicking around did not.
Turning `checkJs` on after
the module split surfaced five modules referencing names that had stayed behind
in `app.js` (`pendingSelect`, `TOKEN`, `WS_BASE`, `selected`, `prOf`) — every one
a `ReferenceError` waiting for a code path the browser checks never hit. Treat a
green page as weaker evidence than a green `check-web`.

## A panic is denied where it can take the daemon down, and `clippy.toml` is why that became affordable.
`unwrap_used`, `expect_used`, `panic`, `print_stdout`,
`print_stderr` and `await_holding_lock` are all `deny` at the workspace; that
file says what made the trade payable. The shape that argued for it:
**`host.rs` had 20 `lock().unwrap()`**, where one panic under any of them
poisons the mutex and every later caller panics too — in the host, which owns
every checkout's child process. `host::locked` and the desktop's
`poisoned_is_still_usable` recover instead, which is safe because every one of
those locks holds a map or a vector updated whole.
Three things to know before adding a site. An integration test in `tests/` is
**not** covered by `allow-*-in-tests` — that setting reaches `#[test]` functions
and `#[cfg(test)]` modules, and a helper in an integration crate is neither, so
those four files carry a file-level `allow` with the reason. A CLI binary allows
the print lints at the top of the file, and that allow *is* the statement that it
is a CLI — the daemon library cannot print, because a launcher-started app has no
terminal and the line would reach nobody. And what is left uses
`#[expect(…, reason = "…")]` rather than `#[allow]`, so the exemption fails the
build when the code stops needing it.
**`crates/orchd-serve/src/main.rs` gave that allow back, and the reason is a
panic.** `println!` unwraps its write, so a closed stdout takes the process out
with `failed printing to stdout: Broken pipe (os error 32)` — reported out of
`orchd::main` in #18. A daemon whose parent has already gone is exactly when that
happens and exactly when a panic helps least. A four-line `say` writes through
`writeln!` and drops the error, and dropping the file-level allow is what makes
the workspace deny the guard: a `println!` cannot come back, so there is no rule
left for anyone to forget. Two tests in `tests/cli_version.rs` hold the other end
— one runs the binary with its stdout closed before the first write and fails on
a panic or a signal; the other is about `--version`, which used to fall through
to an ordinary start and take the instance lock, rotate the log, fetch upstream
and poll GitHub on whatever checkout the config named. It asserts the version
*and* that no config dir came into being, because a test reading only stdout
would have passed against the old behaviour.

## `health.yml` runs `cargo deny`, `cargo about`, `cargo machete`, `typos` and `zizmor` — on every push and weekly, in its own workflow.
An advisory against
one of the ~490 crates the bundle redistributes arrives without anybody pushing
a commit, and "go and read an advisory" is a different message from "your commit
is broken". The workflow and `deny.toml` carry the rest, each beside the setting
it explains: why the five tool versions are pinned, which advisory is accepted
rather than fixed, why `openssl` is banned outright, and why there is no
`paths:` filter. Every action in all three workflows is pinned by hash, and
there is no bot moving them — a bump is a deliberate commit, which is the trade.
**`THIRD-PARTY-RUST.md` is generated, never edited.** `THIRD-PARTY.md` already
argues the obligation for the vendored JavaScript — it is `include_str!`d, so it
is redistributed in binary form and its notice has to travel — and every crate in
the bundle is in the same position. `cargo about` writes it from `Cargo.lock`,
and CI regenerates and diffs it, exactly as it does `web/snapshot.d.ts`.

## The doc comments are checked now, and they were not.
`[`like this`]` is an
intra-doc link, `cargo doc` is the only thing that reads one, and it had never
been run: twelve were dangling, pointing at items renamed or deleted months
before — `render_prompt_file` outlived the whole prompt-to-skill conversion.
A pointer that leads nowhere is worse than none, because it costs a reader a
search to find that out. `mise run check-docs` and CI deny
`rustdoc::broken_intra_doc_links`; `private_intra_doc_links` is **allowed**,
because nearly every module here is private and documents itself for whoever
reads the source next, so that warning fires on the normal case. Two spellings
that will not resolve and are not worth fighting: a private item reached by a
`crate::…` path from another module, and `Self::` inside a trait `impl` — use a
plain code span or name the trait.

## `ctl(id)` is the one deliberate `any` in the SPA.
`getElementById` can only
promise `HTMLElement`, so reading `.value` through `$` is a type error even when
the id certainly names an `<input>`. `ctl` is the named escape hatch for form
controls; `$` stays typed so everything else fetched through it keeps being
checked. Do not widen `$`.


## `dpkg-deb -c` lists a package; only a container installs one.
The Linux half of `bundle.yml` read the `.deb` with `dpkg-deb -c` and asserted the
three binaries were in it. That cannot see a dependency the bundler failed to
declare, a maintainer script that exits non-zero, or a binary that will not start
on a clean machine — and all three reach a user as "it does not open", which is
what #16 and #26 both were.

`tools/apt-check.sh` installs it in `ubuntu:22.04` instead: `apt-get install
./file.deb` rather than `dpkg -i`, because only the first resolves the
dependencies the package declares. Then the three binaries, `orchd --version`
against the version the tree was built from, a daemon started against a throwaway
repository, and one request answered on its own port.

**And the line no sandbox can reach.** `install::classify` keys on the executable
living under `/usr/bin`, so an apt install cannot be simulated on a developer's
machine — a real install in a real container can. The daemon says what installed
it at start (`installed by install="the apt package"`), which is both what this
asserts and the first fact any report about the update bar needs: two reports in
one week were about an upgrade that changed nothing and an app that came back on
the version it started on, and neither said which installer it was.

**The apt repository already installs the package, and that is not the same
check.** `kbarendrecht/apt` installs what has **already been published** — the
right check for the index, the wrong moment for the package. This one runs before
the tag exists, which is the whole reason it is here as well.

`mise run release` now waits for `bundle` as well as `check`, and dispatches one
when the commit has none: `bundle` is path-filtered on `desktop/`, so an ordinary
release never built a package at all until the tag did.

**And the same script runs again after the tag, against what was published.**
`release.yml`'s `verify` job downloads the release's own `.deb` and hands it to
this script, and on macOS mounts the `.dmg`, copies the app out the way a person
drags it and drives that copy. The script owns its own `docker run`: written at
each call site instead, the two copies drifted within a day — one staged the
package at a fixed path, the other mounted it in place, and each read the version
its own way — and neither could be run by hand. The two questions are not the same one: `bundle`
asks whether the package this repository *makes* works, and `verify` asks whether
the release it *published* does — which differs the moment an asset is renamed, a
job uploads the wrong directory, or a dependency is declared for a machine the
builder happened to have.

**A release is uploaded drafted and only published once that passes.** The first
shape of this published first and retracted on failure, which needed three pieces
of machinery — a compensating `gh release edit --draft`, the prose explaining that
drafting is not deleting, and a clause in `mise run release`'s refusal — all
downstream of one ordering choice. Uploading drafted removes them: a draft is not
in the API `mise` and `ubi` resolve from, so a release that fails `verify` was
never installable, and `publish` is simply the job that does not run.

`verify` still reads the release rather than `dist/`, which is the point: a
renamed asset or a job that uploaded the wrong directory is exactly what it is
there to catch, and a draft carries its assets like any other release.

**What that gates is the two publishing repositories, without moving a token into
CI.** `mise run release` waits for the whole release run before it dispatches the
tap and the apt index. `--retry` accepts a release that never left draft, for the
same reason its refusal exists: a version people can install must not change
meaning, and one nobody could install has none to change.

**Quarantine is still not covered and cannot be here.** `gh` writes no
`com.apple.quarantine` attribute, so the approval a downloaded build asks for is
something only a real download shows.

The install is retried once. The image ships an index, a mirror rotates a point
release out from under it, and the fetch 404s — seen on the second run of this
script. A gate that goes red on somebody else's mirror is one people learn to
re-run without reading.


## The update button is one chain, and every link had a test but the chain had none.
A press resolves the install, builds an argv, runs it bounded and reports either
nothing or a tail. `plan`, `offer_for` and `explain` were each unit-tested, and
two failures still shipped in a week: a cask told to run `mise up`, and an
installer that exited 0 having done nothing while the bar offered a restart that
changed nothing.

Three tests now cover it, and each one is where the thing it covers is reachable:

- **The decision, per install kind** — `every_install_kind_either_names_its_installer_or_says_why_it_cannot`
  in `update.rs`. `Subject::offer` takes the `Install` rather than reading the
  running process's own, which is the whole reason five of the six kinds can be
  tested at all: a test binary lives under `target/`, so it *is* a checkout, and
  nothing else could ever be asserted from one.
- **The chain** — `tools/e2e/flows/35-upgrade-button.mjs`, through the HTTP route,
  with a `mise` on PATH that records what it was asked to do. It asserts the nudge,
  the pair the route answers with, the argv that ran, the empty tail of a success,
  the refusal of a second press mid-run, and the reason surviving a failure.
  **Checked against deliberate breakage**: ignoring the installer's exit status
  fails it with an empty tail where the reason should be.
- **The installed build** — `landed` and `upgrade_landed`, because an installer
  exiting 0 is not an upgrade. See the macOS group. Three properties, and each was
  a gap found by asking what the tests above could *not* see: that the app asks at
  all and the agent does not (`only_the_app_asks_what_a_restart_would_start` — the
  agent's is applied when installed, so asking spawns a process about the wrong
  thing); that the answer comes off the layout a restart resolves
  (`the_installed_version_is_read_from_the_build_a_restart_would_start`, over a
  tarball, a mise `latest` and a bundle); and that "newer" is decided by number
  rather than text (`a_newer_release_is_decided_by_number_and_not_by_text` — a
  month rolling over makes `2026.10.1` sort below `2026.9.27`).

  Both reads are handed in rather than taken: `installed_version` gets the running
  path and `landed` gets a closure, because a test binary can only ever ask about
  itself — and it answers `--version` with libtest's usage.

**It drives the agent's button, and that is not a shortcut.** The app's nudge comes
from the release poller, which a debug build does not start, so no sandbox can ever
have one; the agent's comes from `mise`, which is a PATH lookup. `start_upgrade` is
one implementation for both subjects, and the implementation is what this asserts.
The part that stays untested is the app subject's own wiring — one line in
`upgrade_app` — and the container job is what covers the install it produces.
