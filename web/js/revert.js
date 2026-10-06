// One change block of a diff, put back as the base had it.
//
// No imports, so `tools/check-revert.mjs` drives it in node: every case is a
// string in and a string out, and the ones that matter (a file without a final
// newline, CRLF, a block that moved) are cheaper to list than to click.

/** @typedef {{ dels: import('../repo').Row[], adds: import('../repo').Row[], at: number }} Block */

/** A hunk's change blocks, in the order the split view numbers them: each run
 *  of removed and added lines with no context between, and the line of the
 *  file on disk where its added lines start (or where removed lines would go
 *  back, for a block that only removes). */
export function changeBlocks(/** @type {import('../repo').Hunk} */ h) {
  /** @type {Block[]} */
  const out = [];
  let prevNew = h.new_start - 1;
  /** @type {Block | null} */
  let cur = null;
  for (const r of h.rows) {
    if (r.kind === 'context') {
      cur = null;
      prevNew = r.new ?? prevNew;
      continue;
    }
    if (!cur) { cur = { dels: [], adds: [], at: prevNew + 1 }; out.push(cur); }
    if (r.kind === 'del') {
      cur.dels.push(r);
    } else {
      if (!cur.adds.length) cur.at = r.new ?? cur.at;
      cur.adds.push(r);
      prevNew = r.new ?? prevNew;
    }
  }
  return out;
}

/** The file with one change block put back as the left side has it, or null
 *  when the lines on disk are no longer the ones the diff drew.
 *
 *  Line by line on the file as it is now, not a patch: git's patch would want
 *  the whole hunk to still apply, and this only needs the block's own lines. */
export function revertedText(/** @type {string} */ content, /** @type {Block} */ b) {
  const crlf = content.includes('\r\n');
  const endsNl = content.endsWith('\n');
  const body = endsNl ? content.slice(0, -1) : content;
  const lines = content === '' ? [] : body.split('\n');
  const bare = (/** @type {string} */ l) => (l.endsWith('\r') ? l.slice(0, -1) : l);
  const i = Math.max(0, b.at - 1);
  const now = lines.slice(i, i + b.adds.length).map(bare);
  if (now.length !== b.adds.length || now.some((l, k) => l !== b.adds[k].text)) return null;
  lines.splice(i, b.adds.length, ...b.dels.map((r) => r.text + (crlf ? '\r' : '')));
  /* The newline at the very end follows the left side when the block decides it:
     a removed last line without one means the old file had none, and an added
     one without one means the old file did. Anywhere else it stays as it is. */
  const nl = b.dels.some((r) => r.no_eol) ? false : b.adds.some((r) => r.no_eol) ? true : endsNl;
  if (!lines.length) return '';
  const last = lines.length - 1;
  if (crlf && nl && !endsNl) lines[last] += '\r';
  if (crlf && !nl && endsNl) lines[last] = bare(lines[last]);
  return lines.join('\n') + (nl ? '\n' : '');
}
