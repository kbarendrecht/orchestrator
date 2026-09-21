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
// **The index draws the matched line, and the path sits on the right.** It used
// to be `path:line` and nothing else, on the argument that the line is on screen
// six rows down anyway. What that costs is the scan: two hits in one file read
// alike, and telling them apart means moving the cursor to each one. The line is
// the answer, so it takes the left edge where the eye already is, and the path
// keeps the right where a column of them still lines up.

import {
  $, activeWorkspaceId, el, get, MOD_LABEL, reason, toast,
} from './core.js';
import * as Editor from './editor.js';
import { charRanges, paintRanges } from './source.js';
import * as Viewer from './viewer.js';
import * as FileView from './fileview.js';

/** How long the query rests before it is sent. Long enough that typing a symbol
 *  is one search rather than eight, short enough to feel like none. */
const DEBOUNCE = 120;
/** The pane under the index, and the one thing the finder does not own: a search
 *  result and a path an agent printed want the same picture of a file, so the
 *  picture lives in `viewer.js` and this is one of its two callers. */
let view = null;
const viewer = () => {
  view ??= Viewer.create({ mount: $('fnsrc'), path: $('fnpath'), where: $('fnwhere') });
  return view;
};
/** Paths offered at once in `names` mode. The ranking is the useful part; a
 *  thousand rows of it is a list nobody reads. */
const SHOWN = 200;
/** How long the workspace's file list is trusted.
 *
 *  **It used to be forever**, which meant `find files` could not see a file an
 *  agent had just written — and in this app that is the normal case rather than
 *  an edge one. The walk is 100-130ms over 19,029 files on the repo this is
 *  developed against, so half a minute of staleness is the whole price of not
 *  paying it per keystroke. */
const PATHS_TTL = 30_000;
/** Characters of a matched line drawn in the index, and how much of the line
 *  before the match is kept when the window has to move to reach it. */
const SNIP = 240;
const LEADIN = 40;

/** The open overlay.
 *
 *  `ws` is pinned at open for the reason `diffState.ws` is: every fetch used to
 *  read the current workspace at call time, so switching sessions left the answers
 *  describing one tree while the next request asked about another.
 *
 *  @type {{
 *    open: boolean, mode: 'text' | 'names', ws: string | null,
 *    hits: { path: string, line: number, last?: number, col: number, len: number, text?: string }[],
 *    truncated: boolean, cursor: number,
 *    paths: string[] | null, pathsFor: string | null, pathsAt: number,
 *    pathsTruncated: boolean,
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
  pathsAt: 0,
  pathsTruncated: false,
  seq: 0,
  timer: null,
  inflight: null,
};

export const isOpen = () => state.open;

/** Where each jump came from, newest last, so the mouse's back button can return.
 *
 *  **Only a jump pushes.** Typing a new query is not a place you were sent to, it
 *  is a place you went, and a back button that undid your own typing would be a
 *  different feature. A jump from a closed overlay pushes a closed snapshot, so
 *  the way back out of the first one is the same gesture as the way back through
 *  the rest.
 *
 *  @type {{ open: boolean, mode: 'text' | 'names', q: string,
 *           hits: typeof state.hits, truncated: boolean, cursor: number }[]} */
const trail = [];
/** How far back the button goes. A jump chain longer than this is somebody
 *  reading, not somebody who means to walk it back. */
const TRAIL = 20;

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
  clampSplit();
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
  // The trail is a walk through one open overlay. Closing ends the walk, or the
  // next open would hand the button somewhere nobody has been.
  trail.length = 0;
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
  /* The label says which question is being asked, not which one the button would
     ask next. "contents" and "names" were the shorter words and the wrong ones:
     they name the *object* of the search, so neither says that the thing in front
     of you is a search at all. */
  const text = state.mode === 'text';
  $('fnmode').textContent = text ? 'find contents' : 'find files';
  $('fnmode').title = text
    ? 'Searching file contents. Click to search file names instead (Shift Shift)'
    : `Searching file names. Click to search contents instead (${MOD_LABEL}+Shift+F)`;
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
      /* **The daemon's own flag, not `false`.** `MAX_PATHS` is 20,000 and this
         repo lists 19,043 of them, so the cap is reachable — and past it the
         footer said nothing while the list was quietly short. Worse, a path click
         then toasts "no such file in this workspace" about a file that plainly
         exists, blaming the workspace for the cap. */
      state.truncated = !!state.pathsTruncated;
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
async function loadPaths(/** @type {AbortSignal | undefined} */ signal) {
  const fresh = state.paths && state.pathsFor === state.ws
    && Date.now() - state.pathsAt < PATHS_TTL;
  if (fresh) return;
  const url = `/api/paths?workspace=${encodeURIComponent(state.ws ?? '')}`;
  const answer = signal ? await get(url, signal) : await get(url);
  state.paths = answer.paths;
  state.pathsFor = state.ws;
  state.pathsAt = Date.now();
  state.pathsTruncated = !!answer.truncated;
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

/** The index: the line that matched on the left, the path it is in on the right.
 *
 *  In `names` mode the path *is* what was found, so it is the row's content and
 *  there is nothing to its left. */
function renderHits() {
  const box = $('fnhits');
  box.replaceChildren();
  for (const [i, hit] of state.hits.entries()) {
    const row = el('div', 'fnhit' + (i === state.cursor ? ' sel' : ''));
    if (state.mode === 'text') row.appendChild(snippet(hit));
    row.appendChild(where(hit));
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

/** The matched line, with the match marked the way the viewer marks it.
 *
 *  **Not syntax-coloured, and that is the difference from the viewer.** Prism
 *  would tokenise up to 400 lines here for a colour nobody reads at this size,
 *  and the row's job is to say which of the matches this one is. The mark is the
 *  one range worth drawing, and it is the same `tok-find` the file below carries,
 *  so the eye follows one colour from the index into the viewer.
 *
 *  @param {{ text?: string, col: number, len: number }} hit */
function snippet(hit) {
  const line = hit.text ?? '';
  const [span] = hit.len ? charRanges(line, [[hit.col, hit.col + hit.len]]) : [];
  const { text, cut } = windowed(line, span);
  const ranges = [];
  if (span) {
    const s = Math.max(0, span.s - cut);
    const e = Math.min(text.length, span.e - cut);
    if (e > s) ranges.push({ s, e, cls: 'find' });
  }
  return paintRanges(el('span', 'fnsnip'), text, ranges);
}

/** The part of a line worth drawing, and how many characters were cut off its
 *  left.
 *
 *  Two things are being cut. The indent, because a row that starts eight levels
 *  in is a row of nothing; and the length, because one minified file is a single
 *  line of tens of thousands of characters and the index holds up to 400 rows.
 *  When the match sits past the window the window moves to it rather than
 *  dropping it — a snippet that does not contain what you searched for is worse
 *  than no snippet.
 *
 *  @param {string} line
 *  @param {{ s: number, e: number } | undefined} span */
function windowed(line, span) {
  const indent = line.length - line.trimStart().length;
  let cut = indent;
  if (span && span.s - cut > SNIP - LEADIN) cut = Math.max(indent, span.s - LEADIN);
  return { text: line.slice(cut, cut + SNIP), cut };
}

/** Which file the hit is in, and which line of it.
 *
 *  @param {{ path: string, line: number }} hit */
function where(hit) {
  const box = el('span', 'fnat');
  const slash = hit.path.lastIndexOf('/') + 1;
  box.appendChild(el('span', 'fndir', hit.path.slice(0, slash)));
  box.appendChild(el('span', 'fnbase', hit.path.slice(slash)));
  if (state.mode === 'text') box.appendChild(el('span', 'fnline', `:${hit.line}`));
  return box;
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
  // The editor owns the pane while it is up, so a redraw would tear a buffer out
  // from under somebody typing into it.
  if (Editor.isOpen()) return;
  const hit = state.hits[state.cursor];
  if (!hit) return viewer().clear();
  await viewer().show(state.ws ?? '', hit.path, hit);
}

/** The path the viewer is showing, for a caller that needs to say which file a
 *  click happened in. */
export const shownPath = () => viewer().shownPath() ?? state.hits[state.cursor]?.path ?? null;

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
  /* Pushed before anything is replaced, so the button returns to the search that
     was on screen and not to the jump's own answer. */
  trail.push({
    open: state.open,
    mode: state.mode,
    q: /** @type {HTMLInputElement} */ ($('fnq')).value,
    hits: state.hits,
    truncated: state.truncated,
    cursor: state.cursor,
  });
  if (trail.length > TRAIL) trail.shift();
  state.open = true;
  state.ws = ws;
  state.mode = 'text';
  $('fnoverlay').classList.add('on');
  clampSplit();
  renderHead();
  // The query box carries the symbol, so the search is reproducible by hand. The
  // way back is the mouse's back button; see `trail`.
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

/** Undo the last jump: back to the search it was made from.
 *
 *  **The answers are restored, not asked for again.** The hits are what was on
 *  screen, so putting them back is instant and cannot come back different because
 *  a file changed underneath. The query box is set for the same reason, and
 *  setting `value` raises no `input` event, so nothing re-runs.
 *
 *  The one thing it will not do is discard an edit: the buffer answers first,
 *  exactly as closing does, and a "keep editing" puts the step back on the trail.
 */
export async function back() {
  const prev = trail.pop();
  if (!prev) return false;
  if (Editor.isOpen() && !await Editor.close()) {
    trail.push(prev);
    return false;
  }
  // The overlay was not up when the jump was made, so the way back is out.
  if (!prev.open) return close();
  // Any answer still in flight belongs to the jump being undone.
  state.inflight?.abort();
  state.inflight = null;
  clearTimeout(state.timer ?? undefined);
  state.timer = null;
  state.seq++;
  state.mode = prev.mode;
  state.hits = prev.hits;
  state.truncated = prev.truncated;
  state.cursor = prev.cursor;
  /** @type {HTMLInputElement} */ ($('fnq')).value = prev.q;
  renderHead();
  renderHits();
  await showCursor();
  return true;
}

/** Hand what the cursor is on to the file viewer.
 *
 *  **The index is for finding and the file pane is for reading**, and the
 *  difference is the whole window: below an index the file gets two thirds of the
 *  height and no markdown mode, because a search result is a line you are looking
 *  *at* rather than a document you are reading. So this is the bridge, on the key
 *  the overlay contract gives it — a bare one, which belongs to whatever is open.
 */
export function openCurrent() {
  const hit = state.hits[state.cursor];
  if (!hit || !state.ws) return;
  void FileView.open(state.ws, [hit.path], hit.line, hit.last);
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
  if (!hit || !viewer().shownPath()) return toast('nothing to edit here', true);
  return Editor.open({
    mount: $('fnsrc'),
    mountClass: 'fnsrc editing',
    workspace: state.ws ?? '',
    path: hit.path,
    base: null,
    save: $('fnsave'),
    edit: $('fnedit'),
    // Back to the viewer, on the file as it now is: the band is rebuilt from
    // the viewer's own copy, so a discarded edit must not leave a stale one.
    onClosed: () => { viewer().drop(); void showCursor(); },
    onSaved: () => { viewer().drop(); },
  });
}

/** How tall the index is, as the stylesheet reads it, and where that is
 *  remembered. The default is `--fnhits` on `:root`, so nothing here has to agree
 *  with it: a drag sets the property on `documentElement`, which wins over the
 *  rule, and the reset removes it again. */
const SPLIT = { prop: '--fnhits', key: 'orch.findIndexHeight', min: 44, viewerMin: 120 };

/** Put the index at `px`, never so tall that the file below it has nowhere to go
 *  and never so short that the handle is off the end of what it moves. */
function setSplit(/** @type {number} */ px) {
  const box = $('fnoverlay').getBoundingClientRect();
  const room = Math.max(SPLIT.min, box.height - SPLIT.viewerMin);
  document.documentElement.style.setProperty(
    SPLIT.prop, `${Math.round(Math.max(SPLIT.min, Math.min(px, room)))}px`);
}

/** Re-clamp what was remembered against the window as it is now.
 *
 *  A height dragged on one screen is a height that can bury the viewer on a
 *  shorter one, and the overlay has no size to measure while it is closed — so
 *  the check happens when it opens rather than when it is read. Left alone while
 *  the stylesheet's own default is in force: a percentage cannot be out of range.
 */
function clampSplit() {
  const now = $('fnhits').getBoundingClientRect().height;
  if (document.documentElement.style.getPropertyValue(SPLIT.prop)) setSplit(now);
}

/** Drag the line between the index and the file.
 *
 *  **The same handle the drawer has**, on the same axis and with the same two
 *  gestures, because a window that resizes one way in one place and another way
 *  in another is a window you have to learn twice.
 *
 *  Clamped from both ends: an index with no rows in it and a viewer with no file
 *  in it are each a pane you cannot get back by dragging, because the handle
 *  would be off the end of what it moves.
 */
function dragSplit() {
  const handle = $('fnsplit');
  handle.addEventListener('mousedown', (ev) => {
    const e = /** @type {MouseEvent} */ (ev);
    if (e.button !== 0) return;
    e.preventDefault();
    handle.classList.add('dragging');
    document.body.classList.add('row-resizing');
    const top = $('fnhits').getBoundingClientRect().top;
    const move = (/** @type {MouseEvent} */ m) => setSplit(m.clientY - top);
    const done = () => {
      window.removeEventListener('mousemove', move);
      handle.classList.remove('dragging');
      document.body.classList.remove('row-resizing');
      try {
        localStorage.setItem(SPLIT.key, String($('fnhits').getBoundingClientRect().height));
      } catch (err) { /* private mode: the drag still worked for this session */ }
    };
    window.addEventListener('mousemove', move);
    window.addEventListener('mouseup', done, { once: true });
  });
  handle.addEventListener('dblclick', () => {
    document.documentElement.style.removeProperty(SPLIT.prop);
    try {
      localStorage.removeItem(SPLIT.key);
    } catch (err) { /* nothing to forget */ }
  });
  const saved = Number(localStorage.getItem(SPLIT.key));
  // Applied at boot rather than on open: the overlay has no size while it is
  // `display:none`, so a clamp measured then would read zero and pin it to `min`.
  if (saved) document.documentElement.style.setProperty(SPLIT.prop, `${Math.round(saved)}px`);
}

/** Wire the chrome. Called once, at boot. */
export function init() {
  dragSplit();
  const box = /** @type {HTMLInputElement} */ ($('fnq'));
  box.oninput = () => run();
  /** @type {HTMLInputElement} */ ($('fnglob')).oninput = () => run();
  for (const id of ['fncase', 'fnre', 'fnword']) {
    $(id).onclick = () => { $(id).classList.toggle('on'); run(); };
  }
  $('fnmode').onclick = () => void open(state.mode === 'text' ? 'names' : 'text');
  $('fnopen').onclick = () => openCurrent();
  $('fnedit').onclick = () => (Editor.isOpen() ? Editor.close() : edit());
  $('fnsave').onclick = () => Editor.save();
  $('fnclose').onclick = () => void close();
  /* The mouse's back button undoes a jump, which is what that button means
     everywhere else. Button 3 is the back one (4 is forward) and `mousedown` is
     where it arrives; `preventDefault` keeps the webview from treating it as its
     own history, which in an app with one page would be a navigation to nothing.
     Bound to the overlay rather than the window, because outside it there is no
     trail and the button should stay the platform's. */
  $('fnoverlay').addEventListener('mousedown', (ev) => {
    if (/** @type {MouseEvent} */ (ev).button !== 3) return;
    ev.preventDefault();
    void back();
  });
}
