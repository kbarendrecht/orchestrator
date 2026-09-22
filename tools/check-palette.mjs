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
  DEFAULT, contrast, parseHex, tokens, readable, legible, termColours,
  MIN_CONTRAST, TERM_BOARD, TERM_SCHEMES,
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

/* A named scheme is the one palette this app does not mix, so it is the one
   palette nothing else can check.
   Three things are asserted, and each is a defect that would otherwise ship in
   silence. A **missing key** is invisible: xterm paints what it is not given in
   its own defaults, so a scheme short of `brightCyan` is one wrong colour and no
   error anywhere. A **misspelled hex** is the same — `#28A36` is not a colour and
   the renderer falls back. And the **foreground against the background** is the
   promise `MIN_CONTRAST` makes about every other board here; a scheme is not
   exempt from it just because somebody else chose the two colours.
   With no exception list, deliberately: Solarized Light's own body pair is
   4.13:1, so it is not in the table, and a floor with one name written beside it
   is a floor that grows a second. */
const TERM_KEYS = Object.keys(termColours(DEFAULT, TERM_BOARD));
for (const [key, scheme] of Object.entries(TERM_SCHEMES)) {
  if (!scheme.label) problems.push(`terminal scheme ${key} has no label`);
  const painted = termColours(DEFAULT, key);
  for (const name of TERM_KEYS) {
    if (!parseHex(painted[name])) {
      problems.push(`terminal scheme ${key}: ${name} is ${painted[name] ?? 'missing'}, not a colour`);
    }
  }
  const ratio = contrast(scheme.foreground, scheme.background);
  if (ratio < MIN_CONTRAST) {
    problems.push(
      `terminal scheme ${key}: its text on its ground is ${ratio.toFixed(2)}:1, `
      + `under the ${MIN_CONTRAST}:1 floor every other board here clears`,
    );
  }
}

/** Every `var(--x)` the stylesheet reads has to be a token somebody writes.
 *
 *  **`.addco:hover{color:var(--fg)}` shipped, and no token has ever been called
 *  `--fg`.** An undefined custom property makes the declaration invalid at
 *  computed-value time, so the colour falls back to the inherited one: the hover
 *  simply did nothing, on the "+ open project" button, silently. Nothing could
 *  have caught it — the check above compares `:root` against the theme engine and
 *  says nothing about a name neither of them has.
 *
 *  Two sources count as defining one: the `:root` block, and `palette.js`, whose
 *  tokens `applyTheme` sets on the root element at boot. A name in neither is a
 *  typo for a name in one of them. */
function undefinedTokens() {
  // Three places legitimately define one, and a check that knew only the first
  // would report seven false alarms — measured, on the first run of this.
  //   1. any declaration in app.css, `:root` or not (`--setw` is on a panel,
  //      `--add`/`--del` on the diff);
  //   2. the palette, whose tokens `applyTheme` sets on the root element;
  //   3. `setProperty` in the SPA (`--band` per rail group, `--code-px` per
  //      theme, the font stacks and the UI scale).
  // **Every module, read rather than listed.** This named seven files, and the
  // moment the theme moved out of `core.js` into `theme.js` — taking every
  // `setProperty` with it — the check reported `--code-px` as undefined. A scan
  // that names its inputs stops covering the thing it was written for as soon as
  // somebody adds a file, and says so as a false alarm rather than a silence,
  // which is the lucky half.
  const jsDir = path.join(here, '..', 'web', 'js');
  const js = fs.readdirSync(jsDir)
    .filter((f) => f.endsWith('.js'))
    .map((f) => fs.readFileSync(path.join(jsDir, f), 'utf8'))
    .concat(fs.readFileSync(path.join(here, '..', 'web', 'app.js'), 'utf8'))
    .join('\n');
  const defined = new Set([
    ...[...css.matchAll(/(--[a-z0-9-]+)\s*:/g)].map((m) => m[1]),
    ...Object.keys(tokens(DEFAULT, { opacity: DEFAULT.opacity })),
    ...[...js.matchAll(/setProperty\(\s*['"`](--[a-z0-9-]+)/g)].map((m) => m[1]),
  ]);
  // Names built at run time — `var(--co-${band})` — cannot be matched literally,
  // so the family is accepted rather than each member invented here.
  const dynamic = /^--co-/;
  const used = new Set([...css.matchAll(/var\((--[a-z0-9-]+)/g)].map((m) => m[1]));
  return [...used].filter((n) => !defined.has(n) && !dynamic.test(n)).sort();
}

for (const name of undefinedTokens()) {
  problems.push(`app.css reads \`var(${name})\`, and nothing defines ${name}`);
}

if (problems.length) {
  console.error('the palettes do not hold:\n');
  for (const p of problems) console.error(`  ${p}`);
  console.error(
    '\npalette.js and app.css describe one board, so make them agree rather than'
    + '\npicking one. A terminal scheme answers only to itself: complete, spelled'
    + '\nas colours, and readable on its own ground.',
  );
  process.exit(1);
}

console.log(
  `✔ the default theme reproduces :root (${Object.keys(derived).length - 1} tokens) `
  + `and the shipped terminal (${Object.keys(SHIPPED_TERM).length}); `
  + `${Object.keys(TERM_SCHEMES).length} terminal schemes hold`,
);
