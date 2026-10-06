// Where a query matches in one file, over a string the page already holds.
//
// No imports, so `tools/check-seek.mjs` drives it in node.
//
// **Not the daemon's search, and the difference is the question.** `/api/search`
// answers "which files in this workspace", walks the tree and sends byte offsets
// from ripgrep. This answers "where in the text in front of me", which is a
// different thing twice over: the file is already loaded, so a round trip would
// buy nothing, and when the editor is open the text is a buffer somebody is
// typing in that the daemon has never seen.
//
// Smart case is the daemon's rule on purpose — `case_smart` in
// `crates/orchd-repo/src/search.rs`: an upper-case letter anywhere in the query
// makes the search case-sensitive. Two find boxes in one app that disagree about
// case is a difference nobody can see and everybody trips over.

/** One match.
 *
 *  `line` is 1-based. `col` and `len` are **characters** within that line, and
 *  `at` is the character offset in the whole text.
 *
 *  Characters rather than the bytes the daemon sends, because both readers are
 *  JS strings: a textarea's selection has no other unit, and the viewer converts
 *  once on its way to the painter.
 *
 *  @typedef {{ line: number, col: number, len: number, at: number }} Hit */

/** Matches past this are not collected. A file with more than this many hits is
 *  a query that needs narrowing, and the cost of saying so is a number — the
 *  ruler draws 600 marks and the caller shows a count, so nothing downstream
 *  wants the rest. */
export const MAX = 2000;

/** Escape a literal query, so a symbol with a `(` in it searches for that `(`. */
const literal = (/** @type {string} */ q) => q.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/** How to read the query: as a pattern rather than a literal, and in the case it
 *  is written rather than smart-cased. The two toggles in the bar, and the same
 *  two the workspace search has.
 *
 *  @typedef {{ regex?: boolean, exact?: boolean }} How */

/** The query as a regular expression, or null when it is one that does not
 *  compile. `flags` is what the caller needs on top of the case.
 *
 *  @param {string} query @param {How} how @param {string} flags */
function pattern(query, how, flags) {
  const insensitive = !how.exact && query === query.toLowerCase();
  try {
    return new RegExp(how.regex ? query : literal(query), flags + (insensitive ? 'i' : ''));
  } catch (err) {
    return null;
  }
}

/** Every match of `query` in `text`, or null when the query is a regex that does
 *  not compile — which is a thing to say in the box, not a thing to throw.
 *
 *  `m` is set because `^` and `$` in a hand-written regex mean the line here:
 *  this is a search within one file, and a user who writes `^fn ` means the start
 *  of a line and never the start of the file.
 *
 *  @param {string} text
 *  @param {string} query
 *  @param {How} how
 *  @returns {Hit[] | null} */
export function matches(text, query, how) {
  if (!query) return [];
  const re = pattern(query, how, 'gm');
  if (!re) return null;
  // One pass for the line starts, so each match is placed by a search over an
  // array of numbers rather than by counting newlines in the text again.
  const starts = [0];
  for (let i = text.indexOf('\n'); i >= 0; i = text.indexOf('\n', i + 1)) starts.push(i + 1);

  /** @type {Hit[]} */
  const out = [];
  for (let m = re.exec(text); m; m = re.exec(text)) {
    // A pattern that can match nothing — `a*`, `^`, `\b` — otherwise matches it
    // at the same offset forever, because `lastIndex` does not move.
    if (!m[0].length) {
      re.lastIndex++;
      continue;
    }
    const at = m.index;
    const i = lineOf(starts, at);
    out.push({ line: i + 1, col: at - (starts[i] ?? 0), len: m[0].length, at });
    if (out.length >= MAX) break;
  }
  return out;
}

/** Which line `at` is on, as an index into `starts`. Binary search: a 20,000 line
 *  file with 2,000 hits is 40M comparisons walked and 28,000 halved. */
function lineOf(/** @type {number[]} */ starts, /** @type {number} */ at) {
  let lo = 0;
  let hi = starts.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if ((starts[mid] ?? 0) <= at) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

/** The hit to move to from character offset `from`, wrapping at both ends.
 *
 *  `from` is where the reader is rather than which hit they are on, so the arrows
 *  do the right thing after a jump that came from somewhere else — a click in the
 *  file, a line number, the editor's caret.
 *
 *  @param {Hit[]} hits
 *  @param {number} from
 *  @param {number} dir -1 or 1
 *  @returns {number} an index into `hits`, or -1 when there are none */
export function nextIndex(hits, from, dir) {
  if (!hits.length) return -1;
  if (dir > 0) {
    const i = hits.findIndex((h) => h.at > from);
    return i < 0 ? 0 : i;
  }
  for (let i = hits.length - 1; i >= 0; i--) {
    if ((hits[i]?.at ?? 0) < from) return i;
  }
  return hits.length - 1;
}

/** The character offset the 1-based `line` starts at, so a viewer that knows only
 *  which line is on screen can say where "from here" is. A line past the end of
 *  the text answers the start of the last one.
 *
 *  @param {string} text @param {number} line */
export function offsetOf(text, line) {
  let at = 0;
  for (let n = 1; n < line; n++) {
    const next = text.indexOf('\n', at);
    if (next < 0) return at;
    at = next + 1;
  }
  return at;
}

/** The lines the hits are on, each once, for the ruler beside the scrollbar. */
export const hitLines = (/** @type {Hit[]} */ hits) => [...new Set(hits.map((h) => h.line))];

/** What one match becomes.
 *
 *  A literal replacement is itself, `$` included: a person replacing `cost$` with
 *  `price$` means the dollar. A pattern's replacement is the browser's own, so
 *  `$1` is the first group — which is the whole reason the regex toggle is worth
 *  having here, since renaming `foo_bar` to `fooBar` across a file is one
 *  substitution and forty hand edits.
 *
 *  @param {string} matched exactly the text the hit covered
 *  @param {string} query @param {string} to @param {How} how */
export function substitute(matched, query, to, how) {
  if (!how.regex) return to;
  const re = pattern(query, how, '');
  return re ? matched.replace(re, to) : matched;
}

/** `text` with every hit replaced, and how many that was.
 *
 *  **Right to left**, because a replacement of a different length moves every
 *  offset after it: walking forwards would need each hit re-measured against the
 *  text it had already changed.
 *
 *  @param {string} text
 *  @param {Hit[]} hits in the order [`matches`] answers, which is left to right
 *  @param {string} query @param {string} to @param {How} how */
export function replaceAll(text, hits, query, to, how) {
  let out = text;
  for (let i = hits.length - 1; i >= 0; i--) {
    const h = hits[i];
    if (!h) continue;
    out = out.slice(0, h.at) + substitute(out.slice(h.at, h.at + h.len), query, to, how)
      + out.slice(h.at + h.len);
  }
  return { text: out, count: hits.length };
}
