// What Tab, Enter and the comment chord do to the editor's text.
//
// No imports, so `tools/check-editkeys.mjs` drives it in node. The editor is a
// plain textarea, which knows none of this: Tab left the field, Enter lost the
// indent, and every edit to code meant counting spaces by hand.
//
// Each function takes the text and the selection and answers the range to
// replace and what to put there, so the caller can hand it to the browser's own
// insert and keep the undo stack.

/** @typedef {{ from: number, to: number, text: string, selStart: number, selEnd: number }} Edit */

/** The file's indent: a tab when lines start with one, else the smallest run of
 *  two or more spaces that starts a line, else two spaces. One space is never
 *  the unit: that is a JSDoc star's margin, not an indent. */
export function indentUnit(/** @type {string} */ text) {
  if (/^\t/m.test(text)) return '\t';
  let best = 0;
  for (const m of text.matchAll(/^( +)\S/gm)) {
    const n = m[1].length;
    if (n >= 2 && (!best || n < best)) best = n;
  }
  return ' '.repeat(best || 2);
}

/** The start of the line `pos` is on. */
const lineStart = (/** @type {string} */ t, /** @type {number} */ pos) => t.lastIndexOf('\n', pos - 1) + 1;

/** Indent or outdent every line the selection touches. A caret with nothing
 *  selected and no outdent inserts one unit where it is, as a tab key would. */
export function indent(
  /** @type {string} */ text, /** @type {number} */ s, /** @type {number} */ e,
  /** @type {string} */ unit, /** @type {boolean} */ out,
) {
  if (s === e && !out) return { from: s, to: s, text: unit, selStart: s + unit.length, selEnd: s + unit.length };
  const from = lineStart(text, s);
  // A selection ending at the start of a line does not take that line along.
  const endAt = e > s && text[e - 1] === '\n' ? e - 1 : e;
  let to = text.indexOf('\n', endAt);
  if (to < 0) to = text.length;
  const lines = text.slice(from, to).split('\n');
  let first = 0;
  let total = 0;
  const done = lines.map((l, i) => {
    if (!out) {
      const add = l.length ? unit.length : 0;
      if (i === 0) first = add;
      total += add;
      return l.length ? unit + l : l;
    }
    const m = unit === '\t' ? (l.startsWith('\t') ? 1 : /^ */.exec(l)?.[0].length ?? 0)
      : Math.min(unit.length, /^ */.exec(l)?.[0].length ?? 0) || (l.startsWith('\t') ? 1 : 0);
    const cut = Math.min(m, unit === '\t' ? 1 : unit.length);
    if (i === 0) first = -cut;
    total -= cut;
    return l.slice(cut);
  }).join('\n');
  return {
    from, to, text: done,
    selStart: Math.max(from, s + first),
    selEnd: Math.max(from, e + total),
  };
}

/** Enter: a newline plus the indent of the line the caret is on, so the next
 *  line starts where this one did. */
export function newline(/** @type {string} */ text, /** @type {number} */ s, /** @type {number} */ e) {
  const ls = lineStart(text, s);
  const lead = /^[ \t]*/.exec(text.slice(ls, s))?.[0] ?? '';
  const ins = '\n' + lead;
  return { from: s, to: e, text: ins, selStart: s + ins.length, selEnd: s + ins.length };
}

/** The line comment for a Prism language, or null where there is none. */
export function commentFor(/** @type {string | null} */ lang) {
  if (!lang) return null;
  if (['python', 'ruby', 'bash', 'yaml', 'toml', 'perl', 'r', 'elixir', 'makefile', 'docker', 'nix', 'julia', 'ini', 'graphql', 'hcl'].includes(lang)) return '#';
  if (['sql', 'lua', 'haskell'].includes(lang)) return '--';
  if (['markup', 'markdown', 'json', 'css', 'scss', 'sass', 'less', 'diff', 'ocaml'].includes(lang)) return null;
  return '//';
}

/** Toggle a line comment on every line the selection touches: off when every
 *  non-blank line already has it, on otherwise, at the shallowest indent so the
 *  block stays aligned. */
export function toggleComment(
  /** @type {string} */ text, /** @type {number} */ s, /** @type {number} */ e, /** @type {string} */ mark,
) {
  const from = lineStart(text, s);
  const endAt = e > s && text[e - 1] === '\n' ? e - 1 : e;
  let to = text.indexOf('\n', endAt);
  if (to < 0) to = text.length;
  const lines = text.slice(from, to).split('\n');
  const real = lines.filter((l) => l.trim());
  const commented = (/** @type {string} */ l) => l.trimStart().startsWith(mark);
  const off = real.length > 0 && real.every(commented);
  const depth = Math.min(...real.map((l) => /^[ \t]*/.exec(l)?.[0].length ?? 0), Infinity);
  const done = lines.map((l) => {
    if (!l.trim()) return l;
    if (off) {
      const at = l.indexOf(mark);
      const after = l.slice(at + mark.length);
      return l.slice(0, at) + (after.startsWith(' ') ? after.slice(1) : after);
    }
    const d = Number.isFinite(depth) ? depth : 0;
    return l.slice(0, d) + mark + ' ' + l.slice(d);
  }).join('\n');
  return { from, to, text: done, selStart: from, selEnd: from + done.length };
}
