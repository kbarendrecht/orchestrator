#!/usr/bin/env node
// The daemon's module graph, held where it is and no worse.
//
//   node tools/rust-modules.mjs           check against the baseline
//   node tools/rust-modules.mjs --write   re-record it, after breaking a pair
//   node tools/rust-modules.mjs --dot     graphviz, to look at
//
// **A ratchet, not a gate.** The SPA's graph is a DAG and `dependency-cruiser`
// keeps it one; the daemon's is the inverse — it started at 39 modules, 154
// edges, 17 mutual pairs and a 16-module strongly connected component, and
// nothing reported it. That is a fair part of why `api.rs` is 5,681 lines and `spawn.rs`
// 3,371: inside an SCC no module can be read, tested or moved on its own.
//
// Making it a DAG today is not a change anybody can review, so this holds the
// line instead: a **new** mutual pair fails, and a pair that disappears fails
// too until it is taken out of `rust-modules.json`. The second half is what
// makes it a ratchet rather than a permanent list of exceptions — the number can
// only go down, and going down is a commit that says so.
//
// **Seven pairs are gone, in two passes, and each pass had one shape.**
//
// `model` was mutual with `state`, `git` and `diff`, and all three were a *shape*
// living in the module that produces it: `state::random_token` moved to the leaf
// `secret.rs`, and `git::Bank` and `diff::DiffFile` into `model`, beside
// `ChangedFile` and `FileSet`, which were already right. That pass did **not**
// shrink the SCC, which was 16 and never contained `model` at all — a 23-module
// figure was reported once and was this script's own bug, see `strip` below.
//
// `config` was mutual with `story`, `skills`, `reviews` and `env_source`, and all
// four were *behaviour* living in the module that holds the settings:
// `session_env` and `session_flags` built a session's process from inside
// `config`, reaching into the three features `config` configures. They are
// `launch.rs` now, a layer above both. `story::token_env_pair` went the other
// way, into `config` beside the `Tracker` field it reads. That pass took the SCC
// from 16 to 11.
//
// **A third pass took six more, and every one was the same shape as the first.**
// `state::Inner` held four feature modules' own types — `fix_pr::AutomationStore`,
// `post::ManualPhase`, `story::Cache`, `update::UpdateInfo` and its two siblings —
// and `store` persisted them, so `state` imported the four modules that import
// `state`. They are in `model` now, beside `Bank` and `DiffFile`, which is the
// rule this file already recorded: a shape lives in `model`, the module that
// fills it depends on `model`. `state::ResolveRun` went with them because `store`
// loads it. The sixth was `spawn::run_worktree_hooks`, one function `worktree`
// called back into `spawn` for — it is in `worktree` now, beside `revive`, the
// caller that needed it.
//
// **Two are left, and they are one fact.** `spawn` owns the only `pty.wait()`, so
// it is where a run's end is learned, and it settles that run by naming
// `fix_pr::settle`, `fix_pr::start` and `triage`'s guards — while `fix_pr` and
// `triage` call `spawn_run` to start the run. Inverting it means publishing the
// exit and letting the feature subscribe, and the subscription has to outlive a
// restart: `auto_resume` rebuilds a run's session from its persisted `Pass`, so a
// hook stored on `RunSpec` would be gone by the time the run ends.
//
// **The split is done and this script stayed.** Four crates now, and `cargo`
// refuses a cycle that crosses any of their lines — but it cannot see one *inside*
// a crate, and every remaining pair is inside one. Splitting the runtime core is a
// design change rather than a move, so this is what watches them until then.
// Two more have gone since: `api <-> post`, when `workspace_for` and `push_branch`
// moved onto `AppState` — a question about the workspace map is not an HTTP
// question — and `git <-> review_commit`, when `fold_in` stopped taking the
// decision type and took `git::Fold` instead.

import { readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const BASELINE = `${root}/tools/rust-modules.json`;

/** Every crate's `src/`.
 *
 *  **Written when `orchd-base` was split out**, because the thirteen modules that
 *  moved took a mutual pair with them and this script — reading the root `src/`
 *  alone — reported it as *fixed*. A tool that stops watching what it was watching
 *  is worse than no tool: it says the number went down. Cargo enforces that no
 *  cycle crosses a crate line, so what is left to count is inside each one.
 *
 *  There is no root `src/` any more: every crate lives under `crates/`, so this
 *  needs no special case for the one that used to sit at the top. */
const ROOTS = readdirSync(`${root}/crates`, { withFileTypes: true })
  .filter((d) => d.isDirectory())
  .map((d) => `${root}/crates/${d.name}/src`);

const files = [];
for (const src of ROOTS) {
  (function walk(d) {
    for (const e of readdirSync(d)) {
      const p = `${d}/${e}`;
      if (statSync(p).isDirectory()) walk(p);
      else if (e.endsWith('.rs')) files.push(p);
    }
  })(src);
}

/** `src/forge/github.rs` belongs to `forge`; `src/lib.rs` to no module. */
function moduleOf(path) {
  const src = ROOTS.find((r) => path.startsWith(`${r}/`));
  const rel = path.slice(src.length + 1).replace(/\.rs$/, '');
  const first = rel.split('/')[0];
  return ['lib', 'main', 'bin', 'mod'].includes(first) ? null : first;
}

/** Comments are not dependencies, and neither is a `#[cfg(test)] mod`.
 *
 *  **Every one of them is cut, not the first.** This used to slice the file at
 *  the first `mod tests` attribute and keep the head, on the written assumption
 *  that the test module is the last item in the file — and `api.rs` broke it,
 *  carrying 858 lines of handlers below its tests. Their imports (`fix_pr`,
 *  `git`, `forge`) were read nowhere, so a mutual pair among them could have
 *  landed while this printed "no worse". A tool that quietly stops watching part
 *  of the tree is worse than no tool, and clippy does not cover the gap:
 *  `items_after_test_module` reads the crate root alone, and every file here is a
 *  submodule.
 *
 *  Each module is cut from its attribute to the first `}` in the first column,
 *  which `cargo fmt` — a gate here — makes exact: nothing nested can start a
 *  line there. `names.rs` has a second such module (`sample`), so the name is
 *  not matched either.
 *
 *  **The visibility is optional and that is not cosmetic.** `pty.rs` writes
 *  `pub(crate) mod tests` so its fixtures can be shared, and a pattern that only
 *  matched a bare `mod tests` read that whole module as shipped code — which put
 *  `pty -> testutil -> state -> model` into the graph and reported a 23-module
 *  strongly connected component that does not exist. */
const TEST_MOD = /#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+[a-z_]+\s*\{/;

function strip(src) {
  let out = src.replace(/\/\/.*$/gm, '').replace(/\/\*[\s\S]*?\*\//g, '');
  for (;;) {
    const at = out.search(TEST_MOD);
    if (at < 0) return out;
    const close = out.indexOf('\n}\n', at);
    // An unclosed module is the end of the file, so there is nothing after it.
    if (close < 0) return out.slice(0, at);
    out = out.slice(0, at) + out.slice(close + 3);
  }
}

/** Modules `lib.rs` declares under `#[cfg(test)]` — `testutil` — are not in the
 *  shipped binary, so an edge into one is not a dependency of the daemon. */
function testOnlyModules() {
  const out = new Set();
  for (const src of ROOTS) {
    const lib = readFileSync(`${src}/lib.rs`, 'utf8');
    for (const [, name] of lib.matchAll(/#\[cfg\(test\)\]\s*(?:pub\s+)?mod\s+([a-z_]+)\s*;/g)) out.add(name);
    // A fixture module behind a feature is test code too, whatever cargo calls it.
    for (const [, name] of lib.matchAll(/#\[cfg\(any\(test,\s*feature = "test-util"\)\)\]\s*(?:pub\s+)?mod\s+([a-z_]+)\s*;/g)) out.add(name);
  }
  return out;
}
const testOnly = testOnlyModules();

/** @type {Map<string, Set<string>>} */
const edges = new Map();
const mods = new Set();
for (const f of files) {
  const m = moduleOf(f);
  if (!m || testOnly.has(m)) continue;
  mods.add(m);
  const to = edges.get(m) ?? new Set();
  edges.set(m, to);
  const s = strip(readFileSync(f, 'utf8'));
  // `crate::x` covers both a path and a `use`; the braced form is the one shape
  // that names several modules at once and so needs its own pass.
  for (const [, name] of s.matchAll(/\bcrate::([a-z_][a-z0-9_]*)/g)) if (name !== m && !testOnly.has(name)) to.add(name);
  for (const [, body] of s.matchAll(/\buse\s+crate::\{([^}]*)\}/g)) {
    for (const [, name] of body.matchAll(/(?:^|,)\s*([a-z_][a-z0-9_]*)/g)) {
      if (name !== m && !testOnly.has(name)) to.add(name);
    }
  }
}
// `crate::model` is a module; `crate::MAIN` is not.
for (const to of edges.values()) for (const t of [...to]) if (!mods.has(t)) to.delete(t);

const pairs = [];
for (const [a, to] of edges) {
  for (const b of to) if (a < b && edges.get(b)?.has(a)) pairs.push(`${a} <-> ${b}`);
}
pairs.sort();

if (process.argv.includes('--dot')) {
  console.log('digraph orchd {');
  for (const [a, to] of [...edges].sort()) for (const b of [...to].sort()) console.log(`  "${a}" -> "${b}";`);
  console.log('}');
  process.exit(0);
}

const edgeCount = [...edges.values()].reduce((n, s) => n + s.size, 0);
if (process.argv.includes('--write')) {
  // **Only what is checked.** This file used to record the module and edge counts
  // beside the pairs, and nothing read them back — so they sat at 155 edges while
  // the tree had 122. A number a file states and nothing verifies is a number that
  // rots. The live counts are printed on every run instead.
  writeFileSync(BASELINE, `${JSON.stringify({
    '//': 'Mutual imports between modules, and nothing else. A ratchet: a new pair '
        + 'fails, and a pair that goes away has to be deleted here. Counts are not '
        + 'recorded — see tools/rust-modules.mjs for why.',
    mutual: pairs,
  }, null, 2)}\n`);
  console.log(`rust-modules: recorded ${pairs.length} mutual pair(s)`);
  process.exit(0);
}

const want = JSON.parse(readFileSync(BASELINE, 'utf8')).mutual;
const added = pairs.filter((p) => !want.includes(p));
const gone = want.filter((p) => !pairs.includes(p));

if (added.length) {
  console.error('rust-modules: a new mutual import — these two modules are now one:');
  for (const p of added) console.error(`  ${p}`);
  console.error('Break it, or record it with `node tools/rust-modules.mjs --write` and say why.');
}
if (gone.length) {
  console.error('rust-modules: these pairs are gone — good. Take them out of the baseline:');
  for (const p of gone) console.error(`  ${p}`);
  console.error('  node tools/rust-modules.mjs --write');
}
if (added.length || gone.length) process.exit(1);
console.log(`rust-modules: ${mods.size} modules, ${edgeCount} edges, ${pairs.length} mutual pairs (no worse)`);
