#!/usr/bin/env node
// `docs/assumptions.md` says what it is for, and this is what holds it to it.
//
// The file's own header: "Every entry names where it is relied on **and what goes
// wrong when it is false, because that second half is the only reason to write the
// list**". Four of twenty-seven entries had no `*Breaks:*` line at all — the half
// the header calls the only reason — and nothing said so, because a markdown file
// has no compiler.
//
// It checks three things, and each is the file's own stated rule rather than a
// house style invented here:
//   - every entry carries a `*Kind:*`, since the header says the three kinds are
//     mixed on purpose and "each entry says which it is";
//   - every entry carries a `*Breaks:*`;
//   - no number is used twice, because two entries numbered 15 make a reference to
//     "assumption 15" ambiguous, and CLAUDE.md makes those references.
//
// Deliberately *not* checked: that the numbers are contiguous. `6b` exists because
// an assumption was inserted where it belonged rather than renumbering twenty
// entries and every reference to them, and that is the right trade.
//
// Run by `mise run check-docs`.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const file = path.join(here, '..', 'docs', 'assumptions.md');
const text = fs.readFileSync(file, 'utf8');

// An entry starts at a bold number at the start of a line: `**12. …`.
const entries = text.split(/\n(?=\*\*\d+[a-z]?\.)/).slice(1);
if (!entries.length) {
  console.error('check-assumptions: no numbered entries in docs/assumptions.md — has the format changed?');
  process.exit(1);
}

const problems = [];
const seen = new Map();
for (const entry of entries) {
  const num = /^\*\*(\d+[a-z]?)\./.exec(entry)[1];
  // The first line is enough to name the entry in a failure.
  const title = entry.split('\n')[0].replace(/^\*\*/, '').slice(0, 60);
  if (seen.has(num)) problems.push(`${num} is used twice — also on "${seen.get(num)}"`);
  else seen.set(num, title);
  if (!entry.includes('*Kind:*')) problems.push(`${num} has no *Kind:* — contract, convention, or orchd's own rule?`);
  if (!entry.includes('*Breaks:*')) problems.push(`${num} has no *Breaks:* — what goes wrong when it is false?`);
}

if (problems.length) {
  console.error('docs/assumptions.md does not keep its own format:\n');
  for (const p of problems) console.error(`  ${p}`);
  console.error(
    '\nthe file\'s header says every entry names what goes wrong when the assumption'
    + '\nis false, "because that second half is the only reason to write the list".',
  );
  process.exit(1);
}

console.log(`✔ docs/assumptions.md: ${entries.length} assumptions, each with its kind and what it breaks`);
