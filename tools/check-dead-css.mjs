#!/usr/bin/env node
// Every class the stylesheet styles must be one the page can produce.
//
// `app.css` is 442 classes and the page is rebuilt from `el()` calls, so a rule
// whose class nothing writes any more is invisible: it costs no render, breaks no
// test, and reads to the next person as a style that is in use. Thirteen families
// had gone that way — the review overlay's segmented control, a settings row, a
// workspace note — and the only thing that finds them is a name nothing names.
//
// **Concatenated names are the trap this has to handle.** `diff.js` writes
// `el('span', 'tok-' + r.cls, …)`, so 37 `tok-*` classes appear nowhere as a
// literal and are all live. So a class counts as used when its own name appears,
// or when a string literal ending in `-` is a prefix of it — which is how a name
// gets built at all.
//
// Run by `mise run check-web`.

import { readFileSync, readdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const read = (p) => readFileSync(`${root}/${p}`, 'utf8');

// Comments and `url(…)` go first: a font's `.woff2` and a namespace's `.w3` are
// not classes, and this is a text scan rather than a parser.
const css = read('web/app.css')
  .replace(/\/\*[\s\S]*?\*\//g, '')
  .replace(/url\([^)]*\)/g, '');
const classes = [...new Set([...css.matchAll(/\.([A-Za-z][A-Za-z0-9_-]*)/g)].map((m) => m[1]))].sort();

const sources = [
  ...readdirSync(`${root}/web/js`).filter((f) => f.endsWith('.js')).map((f) => `web/js/${f}`),
  'web/app.js',
  'web/index.html',
  'web/review-preview.html',
];
const src = sources.map(read).join('\n');
const prefixes = [...src.matchAll(/['"`]([A-Za-z][A-Za-z0-9_-]*-)['"`]/g)].map((m) => m[1]);

const dead = classes.filter((c) => {
  if (new RegExp(`\\b${c}\\b`).test(src)) return false;
  return !prefixes.some((p) => c.startsWith(p));
});

if (dead.length) {
  console.error(`app.css styles ${dead.length} class(es) nothing can produce:\n`);
  for (const c of dead) console.error(`  .${c}`);
  console.error('\ndelete the rules, or — if the name is built some way this cannot see —');
  console.error('write it once as a literal where it is built.');
  process.exit(1);
}

console.log(`✔ app.css: ${classes.length} classes, every one reachable from the page`);
