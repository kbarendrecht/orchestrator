// Syntax colour for source, as ranges rather than markup.
//
// **Lifted out of `diff.js`, where four modules were already reaching in for it.**
// `review.js` and `review-diff.js` import these three functions and nothing else
// about diffs; `diff.js` itself is a thousand lines of changed-files pane, hunk
// renderer and editor around them. A search viewer would have been the fourth
// importer of a module it has no other business with — so the shared thing moved
// down rather than growing a fourth caller, and `review-diff.js` now depends on
// no viewer at all.
//
// The rule that keeps it here: this file is **pure presentation of a string**. No
// fetch, no state, no snapshot. Anything that needs to know which workspace or
// which file belongs to the module that opened it.

import { el } from './core.js';

/* Prism is vendored whole (every grammar) and driven for its token stream only,
 * never its markup: the daemon already marks the changed slices of a line, and
 * those `.w-add`/`.w-del` ranges have to interleave with the syntax spans rather
 * than nest inside them. So highlighting is flattened to ranges and merged with
 * the word ranges below. Language is the file's extension; anything unmapped —
 * or a grammar Prism does not carry — falls back to plain text, never an error. */
const EXT_LANG = {
  js: 'javascript', mjs: 'javascript', cjs: 'javascript', jsx: 'jsx',
  ts: 'typescript', tsx: 'tsx', rs: 'rust', py: 'python', rb: 'ruby', go: 'go',
  c: 'c', h: 'c', cpp: 'cpp', cc: 'cpp', cxx: 'cpp', hpp: 'cpp', cs: 'csharp',
  java: 'java', kt: 'kotlin', swift: 'swift', php: 'php', sh: 'bash', bash: 'bash',
  zsh: 'bash', fish: 'bash', css: 'css', scss: 'scss', sass: 'sass', less: 'less',
  html: 'markup', htm: 'markup', xml: 'markup', svg: 'markup', vue: 'markup',
  json: 'json', yaml: 'yaml', yml: 'yaml', toml: 'toml', ini: 'ini', cfg: 'ini',
  md: 'markdown', markdown: 'markdown', sql: 'sql', graphql: 'graphql', gql: 'graphql',
  lua: 'lua', pl: 'perl', r: 'r', dart: 'dart', scala: 'scala', clj: 'clojure',
  ex: 'elixir', exs: 'elixir', erl: 'erlang', hs: 'haskell', ml: 'ocaml',
  tf: 'hcl', hcl: 'hcl', proto: 'protobuf', diff: 'diff', patch: 'diff',
  vim: 'vim', nix: 'nix', zig: 'zig', jl: 'julia', groovy: 'groovy', gradle: 'groovy',
};
const BASENAME_LANG = {
  dockerfile: 'docker', makefile: 'makefile', 'cargo.lock': 'toml',
  'go.mod': 'go', 'go.sum': 'go',
};
export function langFor(/** @type {string} */ path) {
  if (!path || !window.Prism) return null;
  const base = (path.split('/').pop() ?? '').toLowerCase();
  const byName = BASENAME_LANG[/** @type {keyof typeof BASENAME_LANG} */ (base)];
  if (byName) return Prism.languages[byName] ? byName : null;
  const dot = base.lastIndexOf('.');
  const lang = EXT_LANG[/** @type {keyof typeof EXT_LANG} */ (dot >= 0 ? base.slice(dot + 1) : '')];
  return lang && Prism.languages[lang] ? lang : null;
}

/** Prism's nested token tree, flattened to non-overlapping `{s,e,cls}` ranges in
 *  character offsets. The deepest token wins, which is what falls out of only
 *  emitting a range at each string leaf. */
export function hlTokens(/** @type {string} */ text, /** @type {string | null} */ lang) {
  if (!lang) return [];
  let tree;
  try { tree = Prism.tokenize(text, Prism.languages[lang]); }
  catch (e) { return []; }
  const out = [];
  let pos = 0;
  (function walk(arr, inherited) {
    for (const t of arr) {
      if (typeof t === 'string') {
        if (inherited) out.push({ s: pos, e: pos + t.length, cls: inherited });
        pos += t.length;
      } else {
        const ty = (t.alias && (Array.isArray(t.alias) ? t.alias[0] : t.alias)) || t.type;
        if (typeof t.content === 'string') {
          out.push({ s: pos, e: pos + t.content.length, cls: ty });
          pos += t.content.length;
        } else {
          walk(t.content, ty);
        }
      }
    }
  })(tree, null);
  return out;
}

/** Split a line at every boundary — syntax-token edges and word-diff edges both
 *  — so each segment can carry a token colour and a change background at once. */
export function lineSegments(/** @type {string} */ text, /** @type {{ s: number, e: number }[]} */ words, /** @type {string | null} */ lang) {
  const toks = hlTokens(text, lang);
  const bset = new Set([0, text.length]);
  for (const t of toks) { bset.add(t.s); bset.add(t.e); }
  for (const w of words) { bset.add(w.s); bset.add(w.e); }
  const pts = [...bset].filter((p) => p >= 0 && p <= text.length).sort((a, b) => a - b);
  const segs = [];
  for (let k = 0; k < pts.length - 1; k++) {
    const s = pts[k], e = pts[k + 1];
    if (s === e) continue;
    const tok = toks.find((t) => t.s <= s && t.e >= e);
    const word = words.some((/** @type {{ s: number, e: number }} */ w) => w.s <= s && w.e >= e);
    segs.push({ s, e, cls: tok ? tok.cls : null, word });
  }
  return segs;
}

/** The open-question detail is usually a commit diff and a reply, with no file to
 *  name a language from, so it is coloured as a *diff*: whole +/- lines, headers
 *  neutral. Prism's diff grammar is line-aware — a `---`/`+++` header is `coord`,
 *  not a deletion, so the `--- the reply ---` separator does not read as removed.
 *  Only the top-level (per-line) token is taken, so the whole line is coloured
 *  rather than the sign alone. Non-diff prose has no diff tokens and stays plain. */
function tokenLen(/** @type {any} */ x) {
  if (typeof x === 'string') return x.length;
  if (Array.isArray(x)) return x.reduce((a, c) => a + tokenLen(c), 0);
  return tokenLen(x.content);
}
function diffRanges(/** @type {string} */ text) {
  if (!window.Prism || !Prism.languages.diff) return [];
  let toks;
  try { toks = Prism.tokenize(text, Prism.languages.diff); }
  catch (e) { return []; }
  const out = [];
  let pos = 0;
  for (const t of toks) {
    if (typeof t === 'string') { pos += t.length; continue; }
    const ty = (t.alias && (Array.isArray(t.alias) ? t.alias[0] : t.alias)) || t.type;
    const len = tokenLen(t);
    out.push({ s: pos, e: pos + len, cls: ty });
    pos += len;
  }
  return out;
}
/** Fill `node` with `text`, each range wrapped in a `tok-<cls>` span and the rest
 *  plain. Ranges do not overlap. No ranges → one text node: the same content minus
 *  colour, never an error. Shared by every painter of highlighted code, because
 *  three copies of this loop had already started to drift. */
export function paintRanges(/** @type {HTMLElement} */ node, /** @type {string} */ text, /** @type {{ s: number, e: number, cls?: string }[]} */ ranges) {
  if (!ranges.length) { node.textContent = text; return node; }
  let at = 0;
  for (const r of ranges) {
    if (r.s > at) node.appendChild(document.createTextNode(text.slice(at, r.s)));
    node.appendChild(el('span', 'tok-' + r.cls, text.slice(r.s, r.e)));
    at = r.e;
  }
  if (at < text.length) node.appendChild(document.createTextNode(text.slice(at)));
  return node;
}
export function detailEl(/** @type {string} */ text) {
  return paintRanges(el('pre', 'oqd'), text, diffRanges(text));
}
