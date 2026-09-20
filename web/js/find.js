// Find in a workspace: the query, the index of hits, and the file under it.
//
// **The overlay is the viewer, and that is the design.** A search result is
// usually a file nobody changed, and the diff viewer has nothing to show for one
// — `openDiff` takes its path from the changeset and `openEditor` refuses without
// a base revision to sit beside. So a search that opened results over there would
// find a line and have nowhere to put it.
//
// Two modes, one overlay: `text` searches the contents, `names` searches the
// paths. They differ in what the index lists and where the answer comes from;
// everything below the index is the same file viewer, so the two are one habit.
//
// **The index is `path:line` and nothing else.** The line itself is on screen
// already, six rows down, and a snippet column costs the width the path needs.
// The trade is that two hits in one file look alike — which is cheap here,
// because moving the cursor moves the viewer with no click and no load.

import {
  $, activeWorkspaceId, el, get, reason, toast,
} from './core.js';
import * as Editor from './editor.js';
import { charRanges, hlTokens, langFor, paintRanges } from './source.js';

/** How long the query rests before it is sent. Long enough that typing a symbol
 *  is one search rather than eight, short enough to feel like none. */
const DEBOUNCE = 120;
/** Rows rendered around the cursor, and how close to the edge the viewport gets
 *  before the band moves. A whole file would be a DOM node per line — `core.js`
 *  is 2,002 of them — and this app draws into WebKitGTK. */
const BAND = 320;
const MARGIN = 80;
/** Past this the file is shown without colour, and the header says so. Prism
 *  tokenises a line at a time here, so the cost is the band; the cap is about the
 *  fetch and the string, not the highlighting. */
const HUGE = 512 * 1024;
/** Paths offered at once in `names` mode. The ranking is the useful part; a
 *  thousand rows of it is a list nobody reads. */
const SHOWN = 200;

/** The open overlay.
 *
 *  `ws` is pinned at open for the reason `diffState.ws` is: every fetch used to
 *  read the current workspace at call time, so switching sessions left the answers
 *  describing one tree while the next request asked about another.
 *
 *  @type {{
 *    open: boolean, mode: 'text' | 'names', ws: string | null,
 *    hits: { path: string, line: number, col: number, len: number }[],
 *    truncated: boolean, cursor: number,
 *    paths: string[] | null, pathsFor: string | null,
 *    file: { path: string, lines: string[], lang: string | null, plain: boolean } | null,
 *    from: number, to: number, rowH: number,
 *    seq: number, timer: ReturnType<typeof setTimeout> | null,
 *    inflight: AbortController | null,
 *  }} */
const state = {
  open: false,
  mode: 'text',
  ws: null,
  hits: [],
  truncated: false,
  cursor: 0,
  paths: null,
  pathsFor: null,
  file: null,
  from: 0,
  to: 0,
  rowH: 0,
  seq: 0,
  timer: null,
  inflight: null,
};

export const isOpen = () => state.open;

/** Open the overlay in one of its two modes, or switch mode while it is up.
 *
 *  @param {'text' | 'names'} mode */
export async function open(mode) {
  const ws = activeWorkspaceId();
  if (!ws) return toast('no session open');
  /* Re-pointing at another workspace is a close and a reopen, and the close can
     ask — so it is awaited, and a "keep editing" abandons the open rather than
     leaving the overlay pointed at one tree with a buffer from another. */
  if (state.open && state.ws !== ws && !await close()) return;
  state.open = true;
  state.ws = ws;
  if (state.mode !== mode) {
    state.mode = mode;
    /* The two modes ask different questions, so one's answers are not the
       other's. Cleared rather than re-ranked, or a mode switch shows hits that
       have nothing to do with the mode now on screen.

       **And drawn immediately, not left to the first answer.** Clearing the array
       alone left the previous mode's rows on screen for the debounce plus a round
       trip — long enough to click one, and `state.hits` no longer had it. */
    state.hits = [];
    state.cursor = 0;
    renderHits();
    void showCursor();
  }
  $('fnoverlay').classList.add('on');
  renderHead();
  const box = /** @type {HTMLInputElement} */ ($('fnq'));
  box.placeholder = mode === 'text' ? 'find in files' : 'find a file';
  box.focus();
  box.select();
  run();
}

export async function close() {
  if (!state.open) return false;
  // The buffer answers first: closing over a half-written edit would discard it
  // without asking, which is the one thing the editor exists to refuse.
  if (Editor.isOpen() && !await Editor.close()) return false;
  state.open = false;
  state.inflight?.abort();
  state.inflight = null;
  clearTimeout(state.timer ?? undefined);
  state.timer = null;
  $('fnoverlay').classList.remove('on');
  return true;
}

/** The overlay belongs to the session it was opened from, so switching away
 *  closes it — the same rule `syncDiffToSession` holds for the diff. */
export function syncToSession() {
  // `void`, like `syncDiffToSession`: the close may draw a confirm, and a render
  // cannot wait for an answer.
  if (state.open && activeWorkspaceId() !== state.ws) void close();
}

function renderHead() {
  $('fnmode').textContent = state.mode === 'text' ? 'contents' : 'names';
  for (const id of ['fncase', 'fnre', 'fnword']) {
    // The three toggles ask about text. In `names` mode the ranking is the page's
    // own, so they would be controls that do nothing.
    $(id).hidden = state.mode !== 'text';
  }
}

const toggled = (/** @type {string} */ id) => $(id).classList.contains('on');

/** Ask again, after a rest. Every keystroke and every toggle comes through here. */
function run() {
  clearTimeout(state.timer ?? undefined);
  state.timer = setTimeout(() => void search(), DEBOUNCE);
}

/** The query, answered by whichever mode is up.
 *
 *  **A superseded answer is worse than none**, because it arrives after the right
 *  one and replaces it. Two guards, because either alone leaks: the request is
 *  aborted, and the sequence number is checked after the await for the answer that
 *  was already in flight when the abort was called.
 */
async function search() {
  const q = /** @type {HTMLInputElement} */ ($('fnq')).value;
  const mine = ++state.seq;
  state.inflight?.abort();
  const ctl = new AbortController();
  state.inflight = ctl;

  try {
    if (state.mode === 'names') {
      await loadPaths(ctl.signal);
      if (mine !== state.seq) return;
      state.hits = rank(state.paths ?? [], q).map((path) => ({ path, line: 1, col: 0, len: 0 }));
      state.truncated = false;
    } else if (!q) {
      state.hits = [];
      state.truncated = false;
    } else {
      const p = new URLSearchParams({ workspace: state.ws ?? '', pattern: q });
      if (toggled('fnre')) p.set('regex', 'true');
      if (toggled('fnword')) p.set('word', 'true');
      const glob = /** @type {HTMLInputElement} */ ($('fnglob')).value.trim();
      if (glob) p.set('glob', glob);
      const answer = await get(`/api/search?${p}`, ctl.signal);
      if (mine !== state.seq) return;
      state.hits = answer.hits;
      state.truncated = answer.truncated;
    }
  } catch (e) {
    // An aborted fetch is this function's own doing, not a failure to report.
    if (/** @type {Error} */ (e).name === 'AbortError' || mine !== state.seq) return;
    state.hits = [];
    state.truncated = false;
    $('fnfoot').textContent = reason(e);
    renderHits();
    return;
  }
  state.cursor = 0;
  renderHits();
  void showCursor();
}

/** The workspace's paths, fetched once and kept.
 *
 *  Per workspace rather than per keystroke: the ranking is a string walk over a
 *  list this size, and a round trip per character would be slower than the answer.
 */
async function loadPaths(/** @type {AbortSignal} */ signal) {
  if (state.paths && state.pathsFor === state.ws) return;
  const answer = await get(`/api/paths?workspace=${encodeURIComponent(state.ws ?? '')}`, signal);
  state.paths = answer.paths;
  state.pathsFor = state.ws;
}

/** Rank paths against a query, best first.
 *
 *  A subsequence match, scored on where the letters landed rather than on how
 *  many there were: a run of adjacent characters and a hit in the basename are
 *  what make one path obviously the one you meant. Case-insensitive throughout —
 *  a path is not a place anybody wants to be precise about shift.
 *
 *  @param {string[]} paths
 *  @param {string} q
 */
export function rank(paths, q) {
  const needle = q.trim().toLowerCase();
  if (!needle) return paths.slice(0, SHOWN);
  const scored = [];
  for (const path of paths) {
    const hay = path.toLowerCase();
    const cut = hay.lastIndexOf('/') + 1;
    let score = 0;
    let at = 0;
    let run = 0;
    for (const ch of needle) {
      const found = hay.indexOf(ch, at);
      if (found < 0) { score = -1; break; }
      // Adjacent beats scattered, and the run compounds so `revapi` ranks
      // `review_api.rs` over a path that merely contains those six letters.
      run = found === at ? run + 1 : 0;
      score += 1 + run * 3;
      if (found >= cut) score += 4;            // in the file's own name
      if (found === cut || hay[found - 1] === '_' || hay[found - 1] === '-') score += 2;
      at = found + 1;
    }
    if (score < 0) continue;
    // A shorter path that scored the same is the better answer: it has less in it
    // that the query did not ask for.
    scored.push({ path, score: score - path.length / 100 });
  }
  scored.sort((a, b) => b.score - a.score || a.path.localeCompare(b.path));
  return scored.slice(0, SHOWN).map((s) => s.path);
}

/** The index: one row per hit, `path:line`, nothing else. */
function renderHits() {
  const box = $('fnhits');
  box.replaceChildren();
  for (const [i, hit] of state.hits.entries()) {
    const row = el('div', 'fnhit' + (i === state.cursor ? ' sel' : ''));
    const slash = hit.path.lastIndexOf('/') + 1;
    row.appendChild(el('span', 'fndir', hit.path.slice(0, slash)));
    row.appendChild(el('span', 'fnbase', hit.path.slice(slash)));
    if (state.mode === 'text') row.appendChild(el('span', 'fnline', `:${hit.line}`));
    row.onclick = () => { state.cursor = i; renderHits(); void showCursor(); };
    box.appendChild(row);
  }
  const n = state.hits.length;
  const what = state.mode === 'text'
    ? `${n} match${n === 1 ? '' : 'es'}`
    : `${n} file${n === 1 ? '' : 's'}`;
  $('fnfoot').textContent = n
    ? what + (state.truncated ? ' — and more; narrow the query' : '')
    : '';
}

/** Move the cursor, and the file under it.
 *
 *  @param {number} step */
export function step(step) {
  if (!state.hits.length) return;
  state.cursor = Math.min(state.hits.length - 1, Math.max(0, state.cursor + step));
  renderHits();
  const sel = $('fnhits').querySelector('.fnhit.sel');
  sel?.scrollIntoView({ block: 'nearest' });
  void showCursor();
}

/** Load and draw whatever the cursor is on. */
async function showCursor() {
  // The editor owns `#fnsrc` while it is up, so a redraw would tear a buffer out
  // from under somebody typing into it.
  if (Editor.isOpen()) return;
  const hit = state.hits[state.cursor];
  if (!hit) {
    $('fnpath').textContent = '';
    $('fnsrc').replaceChildren();
    return;
  }
  if (state.file?.path !== hit.path) {
    const mine = state.seq;
    let answer;
    try {
      answer = await get(
        `/api/file?workspace=${encodeURIComponent(state.ws ?? '')}&path=${encodeURIComponent(hit.path)}`);
    } catch (e) {
      // A refusal is a sentence in the pane, never a blank one: an empty viewer
      // is indistinguishable from a broken viewer.
      state.file = null;
      $('fnpath').textContent = hit.path;
      $('fnsrc').replaceChildren(el('div', 'fnsay', reason(e)));
      return;
    }
    if (mine !== state.seq && state.hits[state.cursor]?.path !== hit.path) return;
    const plain = (answer.content?.length ?? 0) > HUGE;
    state.file = {
      path: hit.path,
      lines: String(answer.content ?? '').split('\n'),
      lang: plain ? null : langFor(hit.path),
      plain,
    };
    state.rowH = 0;
  }
  paint(hit);
}

/** Draw a band of the file around the hit, and say where it is.
 *
 *  @param {{ line: number, col: number, len: number }} hit */
function paint(hit) {
  const file = state.file;
  if (!file) return;
  const total = file.lines.length;
  $('fnpath').textContent = file.path;
  $('fnwhere').textContent = `${hit.line} of ${total}`
    + (file.plain ? ' · too large to colour' : file.lang ? ` · ${file.lang}` : '');
  const from = Math.max(0, hit.line - 1 - Math.floor(BAND / 2));
  band(from, hit);
  // The hit, in the middle of the viewport rather than at its edge.
  const row = $('fnsrc').querySelector('.fnrow.on');
  row?.scrollIntoView({ block: 'center' });
}

/** Render lines `[from, from + BAND)` with spacers standing in for the rest.
 *
 *  The spacers are what keep the scrollbar honest: without them the file is as
 *  tall as the band and scrolling ends after 320 lines. Their height is the
 *  measured height of a real row, taken once per file — a CSS-derived guess goes
 *  wrong the moment the font-size setting moves.
 *
 *  @param {number} from
 *  @param {{ line: number, col: number, len: number }} hit */
function band(from, hit) {
  const file = state.file;
  if (!file) return;
  const total = file.lines.length;
  const to = Math.min(total, from + BAND);
  state.from = from;
  state.to = to;

  const src = $('fnsrc');
  const rows = el('div', 'fnrows');
  for (let i = from; i < to; i++) {
    const text = file.lines[i] ?? '';
    const row = el('div', 'fnrow' + (i + 1 === hit.line ? ' on' : ''));
    // Drawn, not written: a `user-select:none` gutter is still taken by a
    // selection that crosses it, so the numbers would ride along into every
    // copied snippet. Generated content is not in the document to be taken.
    const num = el('i');
    num.dataset.n = String(i + 1);
    row.appendChild(num);
    const body = el('s');
    const ranges = hlTokens(text, file.lang);
    if (i + 1 === hit.line && hit.len) {
      // The match, marked through the same range machinery the word-diff paints
      // with. Byte offsets from Rust, so they are converted rather than assumed.
      const [span] = charRanges(text, [[hit.col, hit.col + hit.len]]);
      if (span) ranges.push({ ...span, cls: 'find' });
      ranges.sort((a, b) => a.s - b.s || a.e - b.e);
    }
    paintRanges(body, text || ' ', dedupe(ranges));
    row.appendChild(body);
    rows.appendChild(row);
  }

  const top = el('div', 'fnpad');
  const bot = el('div', 'fnpad');
  src.replaceChildren(top, rows, bot);
  if (!state.rowH) {
    const one = rows.firstElementChild;
    state.rowH = one ? one.getBoundingClientRect().height : 0;
  }
  top.style.height = `${from * state.rowH}px`;
  bot.style.height = `${Math.max(0, total - to) * state.rowH}px`;
}

/** `paintRanges` needs ranges that do not overlap, and a syntax token can sit
 *  under the match. The match wins, because it is why you are looking. */
function dedupe(/** @type {{ s: number, e: number, cls?: string }[]} */ ranges) {
  const out = [];
  let at = 0;
  for (const r of ranges) {
    if (r.e <= at) continue;
    out.push(r.s < at ? { ...r, s: at } : r);
    at = r.e;
  }
  return out;
}

/** Keep the band under the viewport as it is scrolled. */
function onScroll() {
  const file = state.file;
  if (!file || !state.rowH) return;
  const src = $('fnsrc');
  const first = Math.floor(src.scrollTop / state.rowH);
  const last = Math.ceil((src.scrollTop + src.clientHeight) / state.rowH);
  if (first >= state.from + MARGIN && last <= state.to - MARGIN) return;
  if (state.from === 0 && last <= state.to - MARGIN) return;
  const hit = state.hits[state.cursor];
  if (!hit) return;
  const keep = src.scrollTop;
  band(Math.max(0, first - Math.floor(BAND / 3)), hit);
  src.scrollTop = keep;
}

/** The path the viewer is showing, for a caller that needs to say which file a
 *  click happened in. */
export const shownPath = () => state.file?.path ?? state.hits[state.cursor]?.path ?? null;

/** Go to where `symbol` is defined, as far as a regular expression can tell.
 *
 *  **The branch is the design.** One hit is a jump; anything else is a list, and
 *  nothing is a plain search for the word — which is what you wanted anyway. A
 *  heuristic that jumps when it is sure and shows its working when it is not is
 *  usable; one that guesses silently is worse than no jump at all.
 *
 *  @param {string} inFile the file it was clicked in — its extension names the
 *         language, because a symbol has none of its own
 *  @param {string} symbol */
export async function definitionOf(inFile, symbol) {
  const ws = activeWorkspaceId();
  if (!ws) return;
  if (state.open && state.ws !== ws && !await close()) return;
  state.open = true;
  state.ws = ws;
  state.mode = 'text';
  $('fnoverlay').classList.add('on');
  renderHead();
  // The query box carries the symbol, so the search is reproducible by hand and
  // re-typing is the way back — there is no history chord, deliberately.
  /** @type {HTMLInputElement} */ ($('fnq')).value = symbol;
  const mine = ++state.seq;
  state.inflight?.abort();
  clearTimeout(state.timer ?? undefined);

  let answer;
  try {
    const p = new URLSearchParams({ workspace: ws, path: inFile, symbol });
    answer = await get(`/api/def?${p}`);
  } catch (e) {
    toast(reason(e), true);
    return;
  }
  if (mine !== state.seq) return;
  if (!answer.hits.length) {
    // Nothing that looks like a definition: an unknown language, a generated
    // name, or a shape the table does not describe. The ordinary search for the
    // word is the useful answer, and it is one call away.
    run();
    return;
  }
  state.hits = answer.hits;
  state.truncated = answer.truncated;
  state.cursor = 0;
  renderHits();
  await showCursor();
  const n = state.hits.length;
  $('fnfoot').textContent = n === 1
    ? `one definition of ${symbol}`
    : `${n} definitions of ${symbol} — pick one`;
}

/** Open what the cursor is on for editing.
 *
 *  **No base pane, and that is the difference from the diff's editor.** A search
 *  result is usually a file nobody changed, so there is no revision to sit beside
 *  it — which is why this had to be lifted out of `diff.js` rather than reached
 *  into.
 */
function edit() {
  const hit = state.hits[state.cursor];
  if (!hit || !state.file) return toast('nothing to edit here', true);
  return Editor.open({
    mount: $('fnsrc'),
    mountClass: 'fnsrc editing',
    workspace: state.ws ?? '',
    path: hit.path,
    base: null,
    save: $('fnsave'),
    edit: $('fnedit'),
    // Back to the viewer, on the file as it now is: the band is rebuilt from
    // `state.file`, so a discarded edit must not leave a stale copy behind it.
    onClosed: () => { state.file = null; void showCursor(); },
    onSaved: () => { state.file = null; },
  });
}

/** Wire the chrome. Called once, at boot. */
export function init() {
  const box = /** @type {HTMLInputElement} */ ($('fnq'));
  box.oninput = () => run();
  /** @type {HTMLInputElement} */ ($('fnglob')).oninput = () => run();
  for (const id of ['fncase', 'fnre', 'fnword']) {
    $(id).onclick = () => { $(id).classList.toggle('on'); run(); };
  }
  $('fnedit').onclick = () => (Editor.isOpen() ? Editor.close() : edit());
  $('fnsave').onclick = () => Editor.save();
  $('fnclose').onclick = () => void close();
  $('fnsrc').onscroll = () => onScroll();
}
