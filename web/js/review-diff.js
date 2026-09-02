// Reading a unified diff, for the review cards.
//
// Split out of `review.js` because it is the one part of that file with nothing
// to do with the overlay's state: four pure functions over diff text, no
// `reviewState`, no screens. Everything else in there mutates one shared object
// 62 times, and moving those into separate files would distribute the coupling
// rather than reduce it.

import { el } from './core.js';
import { langFor, hlTokens, paintRanges } from './diff.js';

/** Parse a unified diff into per-path counts — the same arithmetic as
 *  `git apply --numstat`, which is what the daemon re-derives authoritatively
 *  before it writes. Done here so a card can label what it would write without
 *  a round trip. */
function patchStats(diff) {
  const out = [];
  let cur = null;
  for (const line of (diff || '').split('\n')) {
    const to = /^\+\+\+ (?:b\/)?(.+)$/.exec(line);
    if (to) {
      const path = to[1].trim();
      cur = out.find((f) => f.path === path);
      if (!cur) out.push((cur = { path, added: 0, deleted: 0 }));
      continue;
    }
    if (!cur || line.startsWith('---') || line.startsWith('@@')) continue;
    if (line.startsWith('+')) cur.added++;
    else if (line.startsWith('-')) cur.deleted++;
  }
  return out;
}

/** `will write renovate.json5 +3 -1`, every path, derived rather than
 *  hand-written — there is no deny-list, so showing what will be written is
 *  what stands in for one. */
function willWriteLabel(diff, verb) {
  return fileListLabel(patchStats(diff), verb || 'will write');
}

/** `<verb> renovate.json5 +3 −1`, every path.
 *
 *  Shared by the card (from a proposed patch) and the manual phase (from
 *  `git diff`), because in both places the point is the same: the list is derived,
 *  so it cannot be wrong about what is being written. */
function fileListLabel(files, verb) {
  const row = el('div', 'willwrite');
  row.appendChild(document.createTextNode(verb));
  for (const f of files) {
    const b = el('b');
    b.appendChild(document.createTextNode(f.path + ' '));
    if (f.added) {
      const a = el('span', null, `+${f.added}`);
      a.style.color = 'var(--ok)';
      b.appendChild(a);
      b.appendChild(document.createTextNode(' '));
    }
    if (f.deleted) {
      const d = el('span', null, `−${f.deleted}`);
      d.style.color = 'var(--bad)';
      b.appendChild(d);
    }
    row.appendChild(b);
  }
  return row;
}

/** The code of one diff row, syntax-coloured with the viewer's own palette.
 *
 *  `hlTokens` gives non-overlapping ranges; anything it does not cover is plain
 *  text. No language, no grammar, or a tokenizer that threw → one text node, which
 *  is the same row minus colour rather than an error. */
function codeEl(text, lang) {
  return paintRanges(el('s'), text, lang ? hlTokens(text, lang) : []);
}

/** Render diff text as hunk rows.
 *
 *  Takes both shapes it is given: GitHub's `diffHunk` (one hunk, no file
 *  headers) and a full `git diff` (headers, possibly several files). The row
 *  classes are app.css's — `.ln`/`.add`/`.del` — not copies of them.
 *
 *  `hitLast` marks the final row with `.hit`: on a GitHub diff hunk that is the
 *  line the comment is anchored to.
 *
 *  `path` names the language for a bare GitHub hunk, which carries no header to
 *  read one from. A full `git diff` re-reads it at every `diff --git`, so a
 *  multi-file diff is coloured per file rather than all as the first one. */
function hunkEl(text, hitLast, path) {
  const box = el('div', 'hunk');
  let oldNo = 0;
  let newNo = 0;
  let last = null;
  let lang = langFor(path);
  for (const line of (text || '').split('\n')) {
    /* A file boundary, and the states that have no hunk at all. Skipping these
       rendered a binary replacement, a pure rename and a deletion as *nothing* —
       on the manual phase's screen, whose whole premise is that you looked at what
       is about to be committed — and ran multi-file diffs together with line numbers
       that jump at the seam. */
    const file = /^diff --git (?:a\/)?(.+?) (?:b\/)?(.+)$/.exec(line);
    if (file) {
      const [, from, to] = file;
      box.appendChild(el('div', 'hh', from === to ? from : `${from} → ${to}`));
      oldNo = 0;
      newNo = 0;
      // The destination name, so a rename colours as what the file now is.
      lang = langFor(to);
      continue;
    }
    const said = /^(new file|deleted file|Binary files|rename from|rename to)/.exec(line);
    if (said) {
      box.appendChild(el('div', 'hh', line));
      continue;
    }
    if (/^(index |old mode|new mode|similarity|dissimilarity)/.test(line)) continue;
    if (/^(--- |\+\+\+ )/.test(line)) continue;
    const at = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
    if (at) {
      oldNo = +at[1];
      newNo = +at[2];
      box.appendChild(el('div', 'hh', line));
      continue;
    }
    const kind = line[0];
    // A "\ No newline at end of file" marker is not a line of the file.
    if (kind !== '+' && kind !== '-' && kind !== ' ') continue;
    const row = el('div', 'ln' + (kind === '+' ? ' add' : kind === '-' ? ' del' : ''));
    row.appendChild(el('i', null, kind === '-' ? String(oldNo) : String(newNo)));
    row.appendChild(codeEl(line.slice(1), lang));
    if (kind === '-') oldNo++;
    else if (kind === '+') newNo++;
    else { oldNo++; newNo++; }
    box.appendChild(row);
    last = row;
  }
  if (hitLast && last) last.classList.add('hit');
  return box;
}

export { patchStats, hunkEl, fileListLabel, willWriteLabel };
