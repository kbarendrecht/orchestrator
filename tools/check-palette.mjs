// The default theme must reproduce `app.css`'s `:root`, exactly.
//
// **Why this exists rather than a comment asking nicely.** `:root` paints the
// first frame and the theme engine paints everything after it, so the two are one
// palette written twice. The branch this feature came from had them four shades
// apart — `--line` `#2C2C2C` against a derived `#353535`, and six more — with two
// consequences nobody would look for: the board visibly shifted colour between
// first paint and the theme settling, and "Reset to the palette the app shipped
// with" restored a palette that merely resembled it.
//
// `palette.js` is pure for this reason: no DOM, no `window`, so node can import it
// and answer the question at check time instead of a person noticing a shade.
//
// Run by `mise run check-web`.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  DEFAULT, contrast, tokens, readable, legible, termColours, MIN_CONTRAST,
} from '../web/js/palette.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const css = fs.readFileSync(path.join(here, '..', 'web', 'app.css'), 'utf8');

/** The `:root` block's own declarations, as a map. */
function rootTokens() {
  const block = /:root\{([\s\S]*?)\n\}/.exec(css);
  if (!block) throw new Error('no :root block in app.css');
  const out = {};
  // Comments first: one of them contains a `;`, which would otherwise split a
  // declaration in half and leave a key nobody can match.
  for (const line of block[1].replace(/\/\*[\s\S]*?\*\//g, '').split(';')) {
    const m = /^\s*(--[a-z-]+)\s*:\s*(\S+)\s*$/.exec(line);
    if (m) out[m[1]] = m[2].toLowerCase();
  }
  return out;
}

const root = rootTokens();
const derived = tokens(DEFAULT);
const problems = [];

for (const [name, want] of Object.entries(derived)) {
  // The three see-through fills have no `:root` declaration, because there is
  // nothing for them to say while the board is solid: each falls back in the sheet
  // to the colour it equals at opacity 1. Nothing to compare.
  if (name === '--ground' || name === '--panel-ground' || name === '--base') continue;
  if (!(name in root)) {
    problems.push(`${name} is derived but missing from :root`);
  } else if (root[name] !== want.toLowerCase()) {
    problems.push(`${name}: :root says ${root[name]}, the default theme derives ${want}`);
  }
}

// The other half of the promise: the shipped palette is legible, and every signal
// colour already clears the floor, so `readable` returns them untouched. If that
// stops being true the default board has moved and somebody meant it to.
if (!legible(DEFAULT)) problems.push('the default theme does not clear the contrast floor');
for (const [name, hue] of Object.entries({
  '--attn': '#E0A244',
  '--work': '#4C9AAF',
  '--ok': '#5FA97C',
  '--bad': '#D4726B',
  '--focus': '#C9C9C9',
  '--auto': '#5B8FC9',
})) {
  const kept = readable(hue, DEFAULT);
  if (kept !== hue) {
    const ratio = contrast(hue, DEFAULT.bg).toFixed(2);
    problems.push(
      `${name} ${hue} no longer clears ${MIN_CONTRAST} on the shipped ground `
      + `(${ratio}), so the default board moved: it would be lifted to ${kept}`,
    );
  }
}

/* The terminal is the other half of the same promise, and it has no `:root` to be
   checked against — so the shipped values are written here. They came out of the
   literal table `termColours` replaced, which is what "Reset restores what shipped"
   has to mean for the pane as well as for the board. */
const SHIPPED_TERM = {
  background: '#101010', foreground: '#d2d2d2', cursor: '#d2d2d2',
  black: '#101010', red: '#c9615a', green: '#5fa97c', yellow: '#e0a244',
  blue: '#4c9aaf', magenta: '#9a7aa0', cyan: '#3e9aaf', white: '#d2d2d2',
  brightBlack: '#5b5b5b', brightRed: '#d6756e', brightGreen: '#74bb90',
  brightYellow: '#edb55c', brightBlue: '#63aec2', brightMagenta: '#b08fb6',
  brightCyan: '#57aec2', brightWhite: '#f0f0f0',
  selectionBackground: '#2c2c2c',
};
const term = termColours(DEFAULT);
for (const [name, want] of Object.entries(SHIPPED_TERM)) {
  const got = String(term[name]).toLowerCase();
  if (got !== want) problems.push(`terminal ${name}: shipped ${want}, derived ${got}`);
}

if (problems.length) {
  console.error('the default theme and app.css disagree:\n');
  for (const p of problems) console.error(`  ${p}`);
  console.error(
    '\nboth describe one palette. Either the ratios in palette.js or the constants'
    + '\nin app.css moved; make them agree rather than picking one.',
  );
  process.exit(1);
}

console.log(
  `✔ the default theme reproduces :root (${Object.keys(derived).length - 1} tokens) `
  + `and the shipped terminal (${Object.keys(SHIPPED_TERM).length})`,
);
