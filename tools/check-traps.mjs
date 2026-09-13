#!/usr/bin/env node
// `CLAUDE.md`'s index and `docs/traps/` are one list written twice.
//
// The traps were 1,537 lines inside `CLAUDE.md`, which is read in full at the
// start of every session. The rules stayed there and the histories moved down, so
// the file is now an index: one line per entry, grouped, each group pointing at
// its file. Nothing was deleted — but a list written twice is a list that drifts,
// and the drift is silent: an entry added to one side is simply missing from the
// other, and the only symptom is a reader who never learns the trap.
//
// So both sides are checked against each other: same entries, same order, same
// groups. The heading in `docs/traps/<group>.md` is the entry's own first
// sentence, which is what makes the comparison possible at all.
//
// Run by `mise run check-docs` and by CI.
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const read = (p) => readFileSync(`${root}/${p}`, 'utf8');

const claude = read('CLAUDE.md');
const section = claude.slice(claude.indexOf('## Things that will bite you'), claude.indexOf('## Releases'));

/** The index: each `### group` with its file link and its one-line entries. */
const indexed = [];
let group = null;
for (const line of section.split('\n')) {
  const head = /^### (.+)$/.exec(line);
  if (head) {
    group = { name: head[1], file: null, entries: [] };
    indexed.push(group);
    continue;
  }
  if (!group) continue;
  const link = /^\[docs\/traps\/([a-z0-9-]+\.md)\]/.exec(line);
  if (link) group.file = link[1];
  const entry = /^- (.+)$/.exec(line);
  if (entry) group.entries.push(entry[1].trim());
}

const files = new Set(readdirSync(`${root}/docs/traps`).filter((f) => f.endsWith('.md')));
const problems = [];

for (const g of indexed) {
  if (!g.file) {
    problems.push(`"${g.name}" links to no file in docs/traps/`);
    continue;
  }
  if (!files.delete(g.file)) {
    problems.push(`"${g.name}" points at docs/traps/${g.file}, which is not there`);
    continue;
  }
  const headings = [...read(`docs/traps/${g.file}`).matchAll(/^## (.+)$/gm)].map((m) => m[1].trim());
  const n = Math.max(headings.length, g.entries.length);
  for (let i = 0; i < n; i += 1) {
    if (headings[i] === g.entries[i]) continue;
    problems.push(
      `docs/traps/${g.file} entry ${i + 1} disagrees with the index:\n`
      + `    index: ${g.entries[i] ?? '(nothing)'}\n`
      + `    file:  ${headings[i] ?? '(nothing)'}`,
    );
  }
}
for (const orphan of files) problems.push(`docs/traps/${orphan} is in no group of the index`);

if (problems.length) {
  console.error('the traps index and docs/traps/ disagree:\n');
  for (const p of problems) console.error(`  ${p}`);
  console.error('\nBoth sides carry the same entries in the same order. The index line is the');
  console.error('entry\'s first sentence; the file heading is the same sentence.');
  process.exit(1);
}
const entries = indexed.reduce((n, g) => n + g.entries.length, 0);
console.log(`✔ traps: ${entries} entries in ${indexed.length} groups, index and files agree`);
