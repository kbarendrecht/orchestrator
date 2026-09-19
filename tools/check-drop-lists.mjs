#!/usr/bin/env node
// A name a pane says it does not draw has to be a name the daemon still sends.
//
// `core.unchanged(box, value, drop)` decides whether a pane rebuilds, and
// `core.paintSig(value, drop)` signs one row for `core.reconcile`. The safe
// shape passes the whole snapshot and names the fields that pane ignores — the
// rail's `NOT_DRAWN`, which is what stops every agent edit rebuilding it, because
// a `PostToolUse` sweep rewrites the changed-file counts and those ride the same
// snapshot.
//
// **Those names are hand-written strings matched against generated ones.** Rename
// a field in Rust and `snapshot.d.ts` follows, `tsc` names every reader that has
// to change — and this list is the one place the name is a string, so nothing
// says a word. The entry then drops nothing and the rail churns again, silently,
// which is the state it was in before somebody measured it.
//
// The other half of the guard cannot be checked this way and is not: a signature
// that *lists* its inputs is one refactor away from freezing its pane, and
// whether a renderer reads something it did not list is a question about the
// whole function body. `paintSig` says so where the idiom is defined.
//
// Run by `mise run check-web`.

import { readFileSync, readdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const read = (p) => readFileSync(`${root}/${p}`, 'utf8');

// Every field name the daemon can send, from the four generated type files.
const fields = new Set();
for (const f of readdirSync(`${root}/web`).filter((f) => f.endsWith('.d.ts'))) {
  for (const m of read(`web/${f}`).matchAll(/(?:^|[{,]\s*)([a-z_][a-z0-9_]*)\s*:/gm)) {
    fields.add(m[1]);
  }
}
if (fields.size < 50) {
  console.error('check-drop-lists: found almost no generated field names — has the layout changed?');
  process.exit(1);
}

const sources = [
  ...readdirSync(`${root}/web/js`).filter((f) => f.endsWith('.js')).map((f) => `web/js/${f}`),
  'web/app.js',
];

const problems = [];
let checked = 0;
for (const file of sources) {
  const src = read(file);
  // The last argument of an `unchanged(…)` or `paintSig(…)` call: an inline
  // array, or the name of a const array in the same file. Found by balancing
  // parentheses rather than by regex, because the argument before it is itself a
  // list full of calls.
  //
  // **Both, because both take a drop list.** `unchanged` decides whether a pane
  // repaints; `paintSig` signs one row for `reconcile`, and it is exported for
  // exactly that. A name misspelled in either one drops nothing and churns
  // silently, which is the whole reason this file exists.
  for (const at of [...src.matchAll(/\b(?:unchanged|paintSig)\(/g)].map((m) => m.index + m[0].length)) {
    let depth = 1;
    let end = at;
    while (end < src.length && depth > 0) {
      const c = src[end];
      if (c === '(' || c === '[') depth += 1;
      else if (c === ')' || c === ']') depth -= 1;
      end += 1;
    }
    const args = src.slice(at, end - 1);
    const tail = args.slice(args.lastIndexOf(']') + 1);
    const inline = /,\s*\[([^\]]*)\]\s*$/.exec(args);
    let list = null;
    if (inline) list = inline[1];
    else {
      const named = /,\s*([A-Za-z_][A-Za-z0-9_]*)\s*$/.exec(tail);
      if (!named) continue;
      // Balanced, not `[^\]]*`, for the same reason the call above is: a comment
      // inside the declaration may carry a bracket. One did — `branches` sat
      // behind a comment mentioning `[0]`, the capture stopped at that bracket,
      // and this printed a count one short and checked the entry never. A gate
      // that quietly stops checking is worse than no gate, so it counts brackets.
      const open = new RegExp(`const\\s+${named[1]}\\s*=\\s*\\[`).exec(src);
      if (!open) continue;
      let d = 1;
      let i = open.index + open[0].length;
      const from = i;
      while (i < src.length && d > 0) {
        if (src[i] === '[') d += 1;
        else if (src[i] === ']') d -= 1;
        i += 1;
      }
      if (d !== 0) continue;
      list = src.slice(from, i - 1);
    }
    for (const s of list.matchAll(/['"]([a-z_][a-z0-9_]*)['"]/g)) {
      checked += 1;
      if (!fields.has(s[1])) problems.push(`${file}  "${s[1]}" is not a field the daemon sends`);
    }
  }
}

if (problems.length) {
  console.error('a pane names a field that no longer exists:\n');
  for (const p of problems) console.error(`  ${p}`);
  console.error('\nthe name is dropped from the paint signature, so the pane it belongs to');
  console.error('is back to rebuilding on every snapshot — or ignoring one it draws.');
  process.exit(1);
}

console.log(`✔ drop lists: ${checked} ignored field name(s), all still sent`);
