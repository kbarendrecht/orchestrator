#!/usr/bin/env node
// Every file a doc names in backticks has to exist, and the flow count has to be
// the number of flows.
//
// **A pointer that leads nowhere is worse than none**, because it costs a reader a
// search to find that out — the argument `mise run check-docs` already makes for
// the doc comments' intra-doc links. The prose had no such check, and a crate move
// is what breaks it: CLAUDE.md's own entry on the layout says a move broke four
// things that read a path, and the fourth failed silently.
//
// Two rules, and the second exists because the first cannot see a number:
//   - a backticked span that looks like a path resolves to a tracked file;
//   - `<n> flows` equals the number of files in `tools/e2e/flows/`.
//
// **A bare basename is allowed to resolve anywhere**, because the prose says
// `diff.rs` and `host.rs` and should keep being able to. A span with a `/` must
// match a tracked path's tail, which is what makes `src/names.rs` in a doc a
// weaker claim than `crates/orchd/src/names.rs` — write the full path where you
// want the check to be strict.
//
// Run by `mise run check-docs`.

import fs from 'node:fs';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// Named on purpose and absent on purpose. Each is a file the tree does not hold,
// for a reason the prose is stating rather than a path that rotted.
const ABSENT = new Map([
  // Written by a running daemon into its config dir, never committed.
  ['sessions.json', 'a daemon writes it'],
  ['config.json', 'a daemon writes it'],
  ['automation.json', 'a daemon writes it'],
  ['hooks.json', 'a daemon writes it'],
  ['recent.json', 'a daemon writes it'],
  ['host.json', 'a daemon writes it'],
  ['window.json', 'a daemon writes it'],
  ['resolve-runs.json', 'a daemon writes it'],
  ['stories.json', 'a daemon writes it'],
  ['manual.json', 'a daemon writes it'],
  ['plan.json', 'a run writes it'],
  ['ping.yml', 'the fixture script writes it into a throwaway repo'],
  // Named as gone, which is the point of the sentence.
  ['prompt.rs', 'deleted with the prompt-to-skill conversion'],
  ['25-host.mjs', 'moved to tests/host_checkouts.rs'],
  ['rust-toolchain.toml', 'deliberately not here — mise overrides it'],
  // Not a file at all.
  ['/vendor/addon-webgl.js', 'a served route'],
  ['skills/<name>/SKILL.md', 'a shape, not a path'],
]);

const EXTS = /\.(rs|mjs|js|ts|toml|md|json|ya?ml|css|html)$/;

const tracked = execFileSync('git', ['ls-files'], { cwd: root, encoding: 'utf8' })
  .split('\n')
  .filter(Boolean);
const basenames = new Set(tracked.map((p) => path.posix.basename(p)));

// **`TODO.md` is not scanned**, and that is not an oversight: it names work that
// has not happened, so `skills/restack/SKILL.md` is a file it is proposing and
// `reviews.js` one it is remembering. Both are the point of the sentence.
const docs = ['CLAUDE.md', 'README.md']
  .concat(fs.readdirSync(path.join(root, 'docs')).filter((f) => f.endsWith('.md')).map((f) => `docs/${f}`));

const problems = [];
let checked = 0;
for (const doc of docs) {
  const lines = fs.readFileSync(path.join(root, doc), 'utf8').split('\n');
  lines.forEach((line, i) => {
    for (const [, span] of line.matchAll(/`([^`\n]+)`/g)) {
      const t = span.trim();
      // Prose, a command, a glob or a URL — none of them a claim about a file.
      if (/\s|\*|\$|^https?:|^~\//.test(t) || t.startsWith('.') || !EXTS.test(t)) continue;
      checked += 1;
      if (ABSENT.has(t)) continue;
      // Tracked, or simply there: a file added in the same change as the doc that
      // names it is not staged yet, and refusing that would teach people to write
      // the doc afterwards.
      const ok = t.includes('/')
        ? tracked.some((p) => p === t || p.endsWith(`/${t}`)) || existsSync(path.join(root, t))
        : basenames.has(t);
      if (!ok) problems.push(`${doc}:${i + 1}  ${t}`);
    }
  });
}

const flows = fs.readdirSync(path.join(root, 'tools', 'e2e', 'flows')).filter((f) => f.endsWith('.mjs')).length;
for (const doc of docs) {
  const lines = fs.readFileSync(path.join(root, doc), 'utf8').split('\n');
  lines.forEach((line, i) => {
    for (const [, n] of line.matchAll(/(\d+) flows\b/g)) {
      if (Number(n) !== flows) problems.push(`${doc}:${i + 1}  says ${n} flows; there are ${flows}`);
    }
  });
}

if (problems.length) {
  console.error('the docs name files that are not there:\n');
  for (const p of problems) console.error(`  ${p}`);
  console.error(
    '\nfix the path, or — if the file is absent on purpose — add it to ABSENT in'
    + '\ntools/check-doc-paths.mjs with the reason.',
  );
  process.exit(1);
}

console.log(`✔ doc paths: ${checked} named files, all present; ${flows} e2e flows`);
