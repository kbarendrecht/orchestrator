# Splitting `orchd` into crates

A sketch, not a plan of record. Everything here is measured against the tree at
the commit that added it; re-measure before acting on any of it with
`mise run check-modules` and `node tools/rust-modules.mjs --dot`.

**Steps 1 and 2 are done.** `crates/orchd-base` holds the primitives;
`crates/orchd-serve` holds the daemon — the server, the boot sequence, the
pollers and the two binaries. The two *cost* sections at the end are the part
worth reading before starting step 3.

## Why this is on the table at all

`tools/rust-modules.mjs` exists because nothing reported the daemon's module
graph, and what it found was 17 mutual imports and a strongly connected component
of 16 modules. Seven of those pairs are gone. What is left is a ratchet — a
script that holds a number — and a script holding a number is a rule with a
runtime. **`cargo` enforces this for free between crates**, and the day the split
happens that script can be deleted.

That is the whole argument. It is not about build times; see *What it does not
buy* below.

## The graph today

41 modules, 155 edges, 10 mutual pairs. Condense the strongly connected component
to one node and what is left is a **clean nine-layer DAG** — the split needs no
code change to be *possible*, only the mechanical work of moving files.

```
layer 0  edit guard headroom migrate names proposal pty secret timing window
layer 1  child model proc review_commit
layer 2  git
layer 3  config patch
layer 4  diff env_source forge instance logging machine reviews skills
layer 5  launch
layer 6  CORE  (api fix_pr health post spawn state store story triage update worktree)
layer 7  firstrun hooks ws
layer 8  host
```

## Four crates, and the proof they work

Any downward-closed set of that DAG is a legal crate. These four were checked
against the real edges: **zero upward edges**, so nothing here needs a cycle
broken first.

| crate | modules | lines | what it is |
| --- | --- | --- | --- |
| `orchd-base` | 13 | ~10,000 | the primitives: processes, ptys, git, the data model, the guard, secrets — **done** |
| `orchd-repo` | 11 | 9,044 | what a checkout *is*: config, the forge, diffs, patches, skills, the launch argv |
| `orchd-run` | 11 | 20,556 | the runtime core, unchanged and still one crate |
| `orchd-serve` | 4 + `lib.rs` | ~6,900 | the daemon: `host`, `hooks`, `firstrun`, `ws`, `start`, the pollers and both binaries — **done** |

`orchestrator-desktop` stays where it is and depends on `orchd-serve`. The two
binaries (`orchd`, `orch`) go in a thin top crate.

**`orchd-run` is the one that cannot be split**, and that is the honest shape of
this repo: `api` (5,853 lines), `spawn` (3,479), `post` (2,701), `state` (2,193)
and seven more genuinely call each other. A session's state, the store behind it
and the PR flows that drive both are one thing today. Splitting *that* is a
design change; everything else here is a move.

## What splits with them, and what fights

**`testutil` splits along exactly the same lines**, which is the strongest single
sign the boundaries are right. Measured by which fixtures each layer's tests
actually call:

| layer | uses |
| --- | --- |
| base | `scratch` |
| repo | `scratch` `scratch_repo` `git` `pr` `comment` `thread` |
| run | `app` `app_with` `app_at` `git` `scratch` `scratch_repo` `pr` |
| serve | `app_at` `git` `scratch` |

So the filesystem and git fixtures belong to `orchd-base` behind a `test-util`
feature, the forge fixtures (`pr`, `comment`, `thread`, whose types live in
`forge`) to `orchd-repo`, and the three `app*` builders to `orchd-run`, where
`AppState` is. No fixture wants to be in two places.

Four things will actually fight, and none is subtle:

1. **`include_str!` paths.** `host.rs` embeds 20 files from `web/` and
   `reviews.rs` one from `reviews/`, both by relative path. Moving a module three
   directories down changes every one. `web/` is also named by
   `tools/tsconfig.json`, `.dependency-cruiser.cjs`, `check-module-routes.mjs`
   and `eslint.config.mjs`, so **moving `web/` under the serve crate is a
   change to the SPA tooling as well** — leave it at the repo root and pay the
   `../../../` instead.
2. **`ts-rs` export paths.** `export_to = "../web/snapshot.d.ts"` appears on
   types in `model` (base), `forge` and `proposal` (repo) and `post` (run). Each
   needs the right relative path, and `check-web` fails loudly if one is wrong,
   which is the good case.
3. **41 `pub(crate)` items.** Each one that crosses a new crate line becomes
   `pub`, and each is a chance to ask whether it should have been public at all.
4. **`tests/`.** Four integration tests, all `host_*`, all belonging to
   `orchd-serve`.

## What step 1 actually cost

Every prediction above held, and four more things bit that the sketch did not
see. None was hard; all four are the same fact wearing different clothes —
**`#[cfg(test)]` does not cross a crate line.**

1. **Two modules stayed behind.** `migrate` and `names` are layer-0 leaves that
   belonged in `orchd-base` by the graph, and their *tests* assert against
   `config::Config::parse` and `spawn::validate_worktree_name`. A pair test
   belongs with the pair, and nothing in base depends on either module, so they
   stayed in `orchd`. Expect one or two of these per crate.
2. **`testutil` had to become a feature**, not a `#[cfg(test)]` module: a
   dependent crate's tests cannot see one. `orchd-base` exports `scratch`,
   `git`, `scratch_repo` and `TRUE_BIN` behind `test-util`, and `orchd`'s
   `testutil` re-exports them, so there is still one `scratch` in the workspace.
   `TRUE_BIN` was a `pub(crate) const` inside `pty`'s test module that `spawn`
   used — invisible the moment `pty` moved.
3. **`ts-rs` had to become an optional dependency**, gated by that same feature.
   As a dev-dependency the `TS` derives exist only while *base's own* tests
   build, and `orchd`'s tests build base as an ordinary dependency — so the
   derives vanished exactly when `orchd`'s generated types needed them.
4. **One generated file became two, and that reaches the SPA.** ts-rs truncates
   `export_to` per crate, so two crates cannot write one file: `orchd-base`
   writes `web/base.d.ts` and `orchd` writes `web/snapshot.d.ts`, with a
   generated `import type … from "./base.d"` between them. That import is only
   right if both crates resolve against one root, which is why
   `.cargo/config.toml` sets `TS_RS_EXPORT_DIR` — there rather than in the three
   gates, so a bare `cargo test` does not leave a stray `bindings/` behind. The
   SPA's `import('../snapshot').DiffFile` became `import('../base').DiffFile` for
   the fifteen types that moved; `tsc` named every one.

Two gates earned their keep on the way: **`cargo machete`** caught `portable-pty`
left declared in `orchd` after `pty.rs` moved, and **`tools/rust-modules.mjs`**
reported `git <-> review_commit` as *fixed* — because it read `src/` alone and
both modules had left. It scans every crate's `src/` now. A tool that quietly
stops watching what it watched is worse than no tool: it says the number went
down.

The `pub(crate)` tax was smaller than feared: nine items in base, six of them
`git`'s. The three `#[cfg(test)]` ones stayed crate-private.

## What it does not buy

**Not build time.** Cold build is 19 s and an incremental rebuild after touching
`lib.rs` is 3 s. Crates parallelise, but `orchd-run` is half the lines and would
dominate any build that touches `base` — which is most of them, since `git` and
`model` live there. Expect a wash, and measure rather than assume.

**Not fewer cycles.** `cargo` enforces the boundaries a split *draws*; it draws
none through a crate. Nine of the ten remaining pairs are inside the runtime
core, and the tenth — `git <-> review_commit` — is inside `orchd-base`, which is
why `tools/rust-modules.mjs` still has work to do after every step.

## How to do it without one unreviewable commit

The argument against making the graph a DAG today is that the change could not be
reviewed. The same argument applies here, and the answer is the same: one crate
at a time, bottom up. Each step compiles, passes every gate, and is a diff a
person can read.

1. ~~**`orchd-base`.**~~ **Done.** The biggest mechanical step and the one with
   no callers to surprise. *What step 1 actually cost*, above, is what to read
   before starting the next one.
2. ~~**`orchd-serve`.**~~ **Done.** The other end, and the one that paid the
   `include_str!` tax.
3. **`orchd-repo`.** Now sandwiched, so its boundary is already proven.
4. **`orchd-run`** is what is left. Delete `tools/rust-modules.mjs` at this step
   — or keep it pointed at that crate alone, since the ten pairs inside it are
   still worth ratcheting.

Steps 1 and 2 are worth doing on their own. Step 4 is the one that lets the
script go, and nothing is lost by stopping before it.


## What step 2 actually cost

**The sketch undercounted it, and the reason is worth keeping: `lib.rs` is not a
module, so the module graph never saw it.** It was the router *and* the pollers,
and it reached all four of the modules being moved — so `start`, `StartOptions`,
`Server`, `router` and every `start_*_poller` came too, 1,765 lines of it. The
boundary that leaves is a better one to say out loud than the sketch's:
**`orchd` is what the daemon knows, and `orchd-serve` is the daemon.**

The binaries moved with it, because `main.rs` calls `start`. `orchd` is a library
only now; `[[bin]]` lives in `orchd-serve`, which is why every build command that
said `cargo build --bin orchd` says `-p orchd-serve --bin orchd`. The binaries
keep their names — `child::launch` execs an `orchd` beside the running executable,
and a session finds `orch` on its PATH.

Everything step 1 learned repeated exactly:

- `orchd` needed the same **`test-util` feature** for its `AppState` fixtures,
  because `orchd-serve`'s tests build it as an ordinary dependency.
- `ts-rs` had to become **optional** in `orchd` too, and `orchd-serve` needed it
  both optional *and* as a dev-dependency: `cfg(test)` is true for its own tests
  while the feature is not, so the optional copy is not linked then.
- A **third** generated file. `host::Checkout` is the crate's one exported type,
  so `web/serve.d.ts` joined `base.d.ts` and `snapshot.d.ts`, and the SPA's
  `import('../snapshot').Checkout` followed.
- Four `pub(crate)` items in `api` widened, and two doc links that pointed up
  became plain code spans.

**Three gates found things the move did not cause.** `check-module-routes.mjs`
read `src/host.rs` by path and crashed — the same class as the module ratchet in
step 1, and the reason both now name where they look. And `cargo about` and
`cargo deny` were both rooting at the `orchd` package, which stopped producing a
binary: `--workspace` on each turned up that **the desktop shell's ~130
dependencies had never been checked or listed at all**. The notices went from 111
crates to 355, and six unmaintained advisories under `tauri` are recorded in
`deny.toml` with reasons. That hole predates the split; the split is what showed
it.
