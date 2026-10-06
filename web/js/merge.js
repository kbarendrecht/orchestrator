// A three-way merge by line, for the editor when the file moved under it.
//
// No imports, so `tools/check-merge.mjs` drives it in node. `base` is what the
// buffer was loaded from, `mine` is the buffer, `theirs` is the file on disk
// now. Edits that touch different lines both land; edits to the same lines
// that differ are kept side by side between conflict markers, the way git
// leaves them, and the person resolves them in the buffer.

export const MINE = '<<<<<<< yours';
export const SPLIT = '=======';
export const THEIRS = '>>>>>>> on disk';

/** Past this many cells the line table is not built: an LCS over two 3,000
 *  line middles is 9M cells, and a merge is not worth a frozen page. */
const MAX_CELLS = 4_000_000;

/** @typedef {{ bs: number, be: number, lines: string[], side: 'a' | 'b' }} Change */

/** What `other` did to `base`, as ranges of base lines replaced by new lines,
 *  or null when the files are too far apart to compare here. */
function changes(/** @type {string[]} */ base, /** @type {string[]} */ other, /** @type {'a' | 'b'} */ side) {
  let pre = 0;
  while (pre < base.length && pre < other.length && base[pre] === other[pre]) pre++;
  let suf = 0;
  while (suf < base.length - pre && suf < other.length - pre
    && base[base.length - 1 - suf] === other[other.length - 1 - suf]) suf++;
  const b = base.slice(pre, base.length - suf);
  const o = other.slice(pre, other.length - suf);
  if ((b.length + 1) * (o.length + 1) > MAX_CELLS) return null;
  // lcs[i * w + j]: the longest common run of b[i..] and o[j..].
  const w = o.length + 1;
  const lcs = new Uint32Array((b.length + 1) * w);
  for (let i = b.length - 1; i >= 0; i--) {
    for (let j = o.length - 1; j >= 0; j--) {
      lcs[i * w + j] = b[i] === o[j]
        ? lcs[(i + 1) * w + j + 1] + 1
        : Math.max(lcs[(i + 1) * w + j], lcs[i * w + j + 1]);
    }
  }
  /** @type {Change[]} */
  const out = [];
  /** @type {Change | null} */
  let cur = null;
  let i = 0;
  let j = 0;
  while (i < b.length || j < o.length) {
    if (i < b.length && j < o.length && b[i] === o[j]) {
      cur = null;
      i++;
      j++;
      continue;
    }
    if (!cur) {
      cur = { bs: pre + i, be: pre + i, lines: [], side };
      out.push(cur);
    }
    if (j < o.length && (i >= b.length || lcs[i * w + j + 1] >= lcs[(i + 1) * w + j])) {
      cur.lines.push(o[j++]);
    } else {
      i++;
      cur.be = pre + i;
    }
  }
  return out;
}

/** Whether two changes touch the same base lines, so applying one would move
 *  what the other replaced. Two insertions at the same point count. */
const clash = (/** @type {Change} */ x, /** @type {Change} */ y) =>
  x.bs === y.bs || (x.bs < y.be && y.bs < x.be);

/** Merge, or null when the files are too large to compare.
 *
 *  @returns {{ text: string, conflicts: number } | null} */
export function merge3(/** @type {string} */ base, /** @type {string} */ mine, /** @type {string} */ theirs) {
  if (mine === theirs) return { text: mine, conflicts: 0 };
  if (mine === base) return { text: theirs, conflicts: 0 };
  if (theirs === base) return { text: mine, conflicts: 0 };
  const B = base.split('\n');
  const a = changes(B, mine.split('\n'), 'a');
  const b = changes(B, theirs.split('\n'), 'b');
  if (!a || !b) return null;
  const all = [...a, ...b].sort((x, y) => x.bs - y.bs || x.be - y.be);
  /** @type {string[]} */
  const out = [];
  let at = 0;
  let conflicts = 0;
  for (let k = 0; k < all.length;) {
    // A group is every change that clashes with one already in it.
    const group = [all[k]];
    let end = all[k].be;
    let m = k + 1;
    while (m < all.length && group.some((g) => clash(g, all[m]))) {
      group.push(all[m]);
      end = Math.max(end, all[m].be);
      m++;
    }
    const start = group[0].bs;
    out.push(...B.slice(at, start));
    /** The group's base range with one side's changes applied. */
    const take = (/** @type {'a' | 'b'} */ s) => {
      const res = [];
      let p = start;
      for (const g of group.filter((x) => x.side === s)) {
        res.push(...B.slice(p, g.bs), ...g.lines);
        p = g.be;
      }
      res.push(...B.slice(p, end));
      return res;
    };
    if (new Set(group.map((g) => g.side)).size === 1) {
      out.push(...take(group[0].side));
    } else {
      const yours = take('a');
      const disk = take('b');
      if (yours.join('\n') === disk.join('\n')) {
        out.push(...yours);
      } else {
        conflicts++;
        out.push(MINE, ...yours, SPLIT, ...disk, THEIRS);
      }
    }
    at = end;
    k = m;
  }
  out.push(...B.slice(at));
  return { text: out.join('\n'), conflicts };
}

/** Whether a buffer still holds a conflict this merge left in it. */
export const hasMarkers = (/** @type {string} */ text) =>
  text.split('\n').some((l) => l === MINE || l === THEIRS);
