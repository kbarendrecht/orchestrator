# Splitting `orchd` into crates

A sketch, not a plan of record. Everything here is measured against the tree at
the commit that added it; re-measure before acting on any of it with
`mise run check-modules` and `node tools/rust-modules.mjs --dot`.

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
| `orchd-base` | 15 | 11,035 | the primitives: processes, ptys, git, the data model, the guard, names, secrets |
| `orchd-repo` | 11 | 9,044 | what a checkout *is*: config, the forge, diffs, patches, skills, the launch argv |
| `orchd-run` | 11 | 20,556 | the runtime core, unchanged and still one crate |
| `orchd-serve` | 4 | 5,058 | HTTP and the window: `host`, `hooks`, `firstrun`, `ws` |

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

## What it does not buy

**Not build time.** Cold build is 19 s and an incremental rebuild after touching
`lib.rs` is 3 s. Crates parallelise, but `orchd-run` is half the lines and would
dominate any build that touches `base` — which is most of them, since `git` and
`model` live there. Expect a wash, and measure rather than assume.

**Not fewer cycles.** The ten mutual pairs that remain are all inside
`orchd-run`, which is one crate. `cargo` would enforce the boundaries the split
*draws*; it would not draw a new one through the core.

## How to do it without one unreviewable commit

The argument against making the graph a DAG today is that the change could not be
reviewed. The same argument applies here, and the answer is the same: one crate
at a time, bottom up. Each step compiles, passes every gate, and is a diff a
person can read.

1. **`orchd-base`.** The biggest mechanical step and the one with no callers to
   surprise. Split `testutil`'s filesystem half with it.
2. **`orchd-serve`.** The other end, and the one that pays the `include_str!`
   tax. Move `tests/host_*.rs` with it.
3. **`orchd-repo`.** Now sandwiched, so its boundary is already proven.
4. **`orchd-run`** is what is left. Delete `tools/rust-modules.mjs` at this step
   — or keep it pointed at that crate alone, since the ten pairs inside it are
   still worth ratcheting.

Steps 1 and 2 are worth doing on their own. Step 4 is the one that lets the
script go, and nothing is lost by stopping before it.
