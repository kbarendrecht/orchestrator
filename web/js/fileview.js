// One file, opened from a path somebody clicked.
//
// **Its own overlay rather than the search's, and that is the point.** Clicking
// a path an agent printed used to open the find overlay on a synthetic one-row
// result, which worked and was wrong twice over: it threw away the search you
// had, and it answered a question about *one file* with a machine built to list
// many. So the viewer moved down into `viewer.js` and this is the second thing
// standing on it — a header, a file, and nothing else.
//
// What it still shares with the finder: the renderer, the editor, and the
// stylesheet. The pane is `.fnsrc` with `.fnrow`s in it, so a fix to how a line
// is drawn is a fix in both places rather than a copy that drifts.

import {
  $, activeCheckout, activeWorkspaceId, borrowFocus, get, getOn, openMenu, reason, returnFocus, toast,
} from './core.js';
import * as Editor from './editor.js';
import { folders, level, matching } from './pathlink.js';
import { MAX, hitLines, matches, nextIndex, offsetOf, replaceAll, substitute } from './seek.js';
import * as Viewer from './viewer.js';

/** The open overlay. `ws` is pinned at open for the reason the finder's is:
 *  every fetch used to read the current workspace at call time, so switching
 *  sessions left the answers describing one tree while the next request asked
 *  about another.
 *
 *  `rendered` is the markdown mode, and an HTML file's preview, remembered
 *  across files: a person reading
 *  notes reads several, and having to press the same button for each one is the
 *  kind of small tax that makes a mode not worth having.
 *
 *  `pinned` is a pane a deep link opened ([`openLink`]): it stays when the
 *  selection is somewhere else, because a link names a file, not a session.
 *
 *  `asked` is the line the file was opened at as it was asked for, 0 for none,
 *  because `line` is clamped to 1 and the difference decides the mode.
 *
 *  `trail` is the files a link in a rendered page was followed from, newest
 *  last: **a link opens above the page it is in**, and backing out lands on that
 *  page where it was scrolled to, rather than closing the pane and losing it.
 *
 *  @type {{ open: boolean, ws: string | null, path: string | null, rendered: boolean,
 *           line: number, asked: number, last: number, pinned: boolean,
 *           trail: { path: string, asked: number, last: number, scroll: number }[] }} */
const state = {
  open: false, ws: null, path: null, rendered: true, line: 1, asked: 0, last: 0, pinned: false,
  trail: [],
};

/** @type {ReturnType<typeof Viewer.create> | null} */
let view = null;
const viewer = () => {
  view ??= Viewer.create({
    mount: $('fvsrc'), path: $('fvpath'), where: $('fvwhere'),
    onFile: (rel, line) => void follow(rel, line),
  });
  return view;
};

/** Open a file a rendered page links to, above the page. */
async function follow(/** @type {string} */ rel, /** @type {number} */ line) {
  if (!state.open || !state.ws || !state.path) return;
  const from = { path: state.path, asked: state.asked, last: state.last, scroll: $('fvsrc').scrollTop };
  state.trail.push(from);
  await show(state.ws, rel, line, 0, state.pinned, true);
}

/** Back to the page a link was followed from, or closed when there is none.
 *
 *  What `Escape` and the mouse's back button do: one step, like a browser's back,
 *  so a chain of links through the docs unwinds the way it was walked. */
export async function back() {
  const to = state.trail.at(-1);
  if (!to || !state.ws) return close();
  state.trail.pop();
  await show(state.ws, to.path, to.asked, to.last, state.pinned, true);
  // After the page is drawn, or there is nothing yet to scroll.
  $('fvsrc').scrollTop = to.scroll;
}

export const isOpen = () => state.open;

/** Move to a line, which means the source: a rendered page has no line numbers. */
export function goTo(/** @type {number} */ line) {
  if (!viewer().goTo(line)) return false;
  state.line = line;
  state.last = 0;
  if (state.rendered) {
    state.rendered = false;
    renderHead(true);
  }
  return true;
}

/** Show one file, at one line.
 *
 *  `candidates` is what the caller resolved the text to: a name an agent printed
 *  can be two files in a large repo, and the answer to that is to ask rather than
 *  to guess. One is opened; several put the choice under the pointer, which is
 *  the same menu the right-click uses and needs no list of its own.
 *
 *  @param {string} ws
 *  @param {string[]} candidates workspace-relative, best first
 *  @param {number} line 1-based, or 0 for the top of the file
 *  @param {number} [last] the end of a `124-129` range
 *  @param {MouseEvent} [ev] where to hang the picker, when there is a choice
 */
export async function open(ws, candidates, line, last, ev) {
  if (!candidates.length) return;
  if (candidates.length > 1 && ev) {
    return openMenu(ev, candidates.slice(0, 12).map((p) => [p, null, () => void show(ws, p, line, last)]));
  }
  await show(ws, candidates[0] ?? '', line, last);
}

/** Open a file in this pane and go straight into editing it, with the caret on
 *  `line`. The finder's Edit, which used to edit in its own short bottom half. */
export async function openToEdit(/** @type {string} */ ws, /** @type {string} */ path, /** @type {number} */ line) {
  await show(ws, path, line);
  if (!state.open || state.path !== path) return;
  await edit();
  if (line > 0) Editor.goTo(line);
}

/** Where a session's scratchpad image is served, on the daemon at `base`.
 *  @param {string} base @param {string} session @param {string} abs */
export const scratchUrl = (base, session, abs) =>
  `${base}/api/scratchpad/image?session=${encodeURIComponent(session)}&path=${encodeURIComponent(abs)}`;

/** Open an image from a session's Claude Code scratchpad, which is outside every
 *  workspace and so has its own route (`preview::scratchpad_image`). Shown in the
 *  same pane, held to the session's own workspace so switching away closes it the
 *  way it closes any file.
 *
 *  @param {string} ws the session's workspace
 *  @param {string} session
 *  @param {string} abs the path as the agent printed it
 *  @param {string} [base] the daemon of the terminal it was printed in */
export async function openScratch(ws, session, abs, base = activeCheckout().base) {
  if (Editor.isOpen() && !await Editor.close()) return;
  if (state.open && state.ws !== ws && !await close()) return;
  state.trail = [];
  state.open = true;
  state.pinned = false;
  state.ws = ws;
  state.path = abs;
  borrowFocus('fileview');
  $('fvoverlay').classList.add('on');
  viewer().image(abs, scratchUrl(base, session, abs));
  renderHead(true);
}

/** Put the files under a folder an agent printed under the pointer, one menu level
 *  per subfolder, so a folder is somewhere you can go and not only a name.
 *
 *  A name several folders share gets a level for each, named in full, which is
 *  the choice [`open`] offers for a file two folders hold.
 *
 *  @param {string} ws
 *  @param {{ folder: string, files: string[] }[]} found as [`foldersOf`] answers
 *  @param {MouseEvent} ev
 *  @param {import('./core.js').MenuItem[]} [after] rows to put below the files */
export function openFolder(ws, found, ev, after = []) {
  /** @returns {import('./core.js').MenuItem[]} */
  const rows = (/** @type {string} */ folder, /** @type {string[]} */ files) => {
    const { dirs, files: here } = level(files);
    return [
      ...dirs.map(([d, under]) => /** @type {import('./core.js').MenuItem} */ (
        [`${d}/`, null, rows(`${folder}/${d}`, under)])),
      ...here.map((f) => /** @type {import('./core.js').MenuItem} */ (
        [f, null, () => void show(ws, `${folder}/${f}`, 0, 0)])),
    ];
  };
  const [only] = found;
  const items = found.length === 1 && only
    ? rows(only.folder, only.files)
    : found.map(({ folder, files }) => /** @type {import('./core.js').MenuItem} */ (
      [`${folder}/`, null, rows(folder, files)]));
  openMenu(ev, [...items, ...after]);
}

/** Show a file an `orchestrator://` link named, whatever session is selected.
 *
 *  **Pinned, because a link names a file and not a session.** The workspace it is
 *  in often has no session at all — main, most of the time — and the rule that
 *  closes this pane when the selection leaves its workspace would then close it
 *  the frame after it opened.
 *
 *  @param {string} ws @param {string} rel @param {number} line */
export async function openLink(ws, rel, line) {
  await show(ws, rel, line, 0, true);
}

/** @param {string} ws @param {string} path @param {number} line @param {number} [last]
 *  @param {boolean} [pinned]
 *  @param {boolean} [onTrail] a step along the trail rather than a fresh open, which
 *  starts one */
async function show(ws, path, line, last, pinned = false, onTrail = false) {
  /* **The buffer answers first, whatever changed.** This used to ask the editor
     only when the *workspace* was different, so clicking a second path an agent
     printed in the same workspace tore the textarea out from under somebody
     typing: no prompt, the edit gone, and `Editor.save()` then finding no
     textarea and returning without a word. The finder's own `showCursor` has
     refused to redraw over a live buffer from the start. */
  if (Editor.isOpen() && !await Editor.close()) return;
  // Re-pointing at another workspace is a close and a reopen, and the close can
  // ask — so it is awaited, and a "keep editing" abandons the open.
  if (state.open && state.ws !== ws && !await close()) return;
  if (!onTrail) state.trail = [];
  state.open = true;
  state.pinned = pinned;
  state.ws = ws;
  state.path = path;
  state.asked = line;
  state.line = Math.max(1, line);
  state.last = Math.max(0, last ?? 0);
  borrowFocus('fileview');
  $('fvoverlay').classList.add('on');
  const drawn = await viewer().show(ws, path, {
    line: state.line, last: state.last, col: 0, len: 0,
  });
  /* **A line number is a reason to show the source.** `notes.md:42` means that
     line, and a rendered page cannot point at it — so the mode gives way to the
     thing that was actually asked for, and the button is right there. */
  if (drawn && viewer().renderable() && state.rendered && !line) viewer().render();
  renderHead(drawn);
  // The bar outlives the file: a query asked of one file is usually a query
  // about the next one too, and re-asking it is cheaper than retyping it.
  if (bar.on) runSeek(false);
}

/** The header's two buttons: what this file can be shown as, and what it is
 *  being shown as. */
function renderHead(/** @type {boolean} */ drawn) {
  // Where back goes, named, so the step it takes is not a guess.
  const prev = state.trail.at(-1);
  $('fvback').hidden = !prev;
  $('fvback').textContent = prev ? `← ${prev.path.split('/').pop()}` : '';
  $('fvback').title = prev ? `Back to ${prev.path}  (Esc)` : '';
  const can = drawn && viewer().renderable();
  $('fvmode').hidden = !can;
  // A picture has no source to edit, and the editor's refusal would say so late.
  $('fvedit').hidden = drawn && viewer().isImage();
  const html = viewer().kind() === 'html';
  $('fvmode').textContent = state.rendered ? 'Source' : html ? 'Preview' : 'Rendered';
  $('fvmode').title = state.rendered
    ? 'Show the file as it is written'
    : html ? 'Run the page in a sandbox' : 'Show the file as markdown';
}

export async function close() {
  if (!state.open) return false;
  // The buffer answers first: closing over a half-written edit would discard it
  // without asking, which is the one thing the editor exists to refuse.
  if (Editor.isOpen() && !await Editor.close()) return false;
  closeSeek();
  state.open = false;
  state.pinned = false;
  state.trail = [];
  state.path = null;
  $('fvoverlay').classList.remove('on');
  returnFocus('fileview', $('fvoverlay'));
  return true;
}

/** The overlay belongs to the session it was opened from, so switching away
 *  closes it — the same rule the diff and the finder hold. */
export function syncToSession() {
  if (state.open && !state.pinned && activeWorkspaceId() !== state.ws) void close();
}

/** The file on screen, so a modifier-click in this pane can say which file the
 *  word it read was in. */
export const shownPath = () => viewer().shownPath();

/** Which files in the workspace a printed path could mean.
 *
 *  **An agent names a file, not a path.** A component's file name with no
 *  directory in front of it, joined onto the pty's own directory, names a file
 *  that is not there — which is what the viewer then said, correctly and
 *  uselessly. The workspace's own list of files is the answer: match on the tail,
 *  which handles a bare name and a partial path with one rule.
 *
 *  **Walked fresh every time, and the measurement is why.** The file an agent is
 *  telling you about is very often one it wrote a moment ago, and a cached list
 *  does not fail visibly — it returns *one* match where there are now two. The
 *  walk is 19,029 files in 100-130ms on the monorepo this is developed against,
 *  against a click somebody makes a few times a minute.
 *
 *  @param {string} ws
 *  @param {string} path workspace-relative, as the click resolved it
 *  @returns {Promise<string[]>} */
export async function candidates(ws, path) {
  let list = [];
  try {
    list = (await get(`/api/paths?workspace=${encodeURIComponent(ws)}`)).paths ?? [];
  } catch (e) {
    toast(reason(e), true);
    return [];
  }
  return matching(list, path);
}

/** The folders a printed path could mean, with every file under each, when it
 *  names no file. Walked fresh, for the reason [`candidates`] is.
 *
 *  @param {string} ws
 *  @param {string} path workspace-relative, as the click resolved it */
export async function foldersOf(ws, path) {
  try {
    return folders((await get(`/api/paths?workspace=${encodeURIComponent(ws)}`)).paths ?? [], path);
  } catch (e) {
    toast(reason(e), true);
    return [];
  }
}

/** How long the hover trusts a workspace's file list for a path it *has*.
 *
 *  Long enough to cover one sweep of the mouse down a pane: xterm asks once per
 *  line it enters, and each ask would otherwise be a whole-tree walk. */
const HOVER_TTL = 2_000;
/** And for a path it has not. **Shorter, because the file an agent just wrote is
 *  the one most likely to be clicked**, and a list from before the write is the
 *  one place it is missing. `page-check` caught exactly that: a file written a
 *  second after the last hover, with no underline. A sweep over lines with no file
 *  in them costs two walks a second at this, not one per line. */
const MISS_TTL = 500;
/** `at` is when the answer landed, and `null` while it is still on its way, so a
 *  slow walk is joined rather than started twice. `got` is the same answer once it
 *  is in, for a caller that has to decide inside an event.
 *
 *  @type {Map<string, { at: number | null, got?: string[],
 *                       list: Promise<{ paths: string[], truncated: boolean }> }>} */
const hovered = new Map();

/** Whether a click on `path` would find anything, which is what earns it an
 *  underline: a file, or a folder with files under it. The click itself still
 *  walks fresh, in [`candidates`].
 *
 *  **A list that was cut short says yes to everything.** `/api/paths` stops at
 *  20,000 and the monorepo is 19,043, so the day it crosses, a miss means "not in
 *  the part that fit" rather than "not there", and a missing underline is worse
 *  than one that answers "no such file".
 *
 *  @param {import('./core.js').Target} at the terminal's own checkout
 *  @param {string} ws
 *  @param {string} path workspace-relative */
export async function known(at, ws, path) {
  const has = (/** @type {{ paths: string[], truncated: boolean }} */ l) =>
    l.truncated || matching(l.paths, path).length > 0 || folders(l.paths, path).length > 0;
  if (has(await listed(at, ws, HOVER_TTL))) return true;
  return has(await listed(at, ws, MISS_TTL));
}

/** Whether the underline under a right-click was drawn for a folder rather than a
 *  file, read from the list the hover already has. **No walk**, because the menu
 *  decides inside the event; with no list in yet it says no, and the file menu
 *  is what opens, as it did before folders were links.
 *
 *  @param {string} checkout the terminal's own checkout, by path
 *  @param {string} ws
 *  @param {string} path workspace-relative */
export function isFolder(checkout, ws, path) {
  const got = hovered.get(`${checkout}\0${ws}`)?.got;
  return !!got && !matching(got, path).length && folders(got, path).length > 0;
}

/** The workspace's file list, if one landed within `ttl`, or the walk already
 *  running, or a new one. **Shared as well as cached**, which is the part that
 *  matters during a sweep: every line asks before the first answer is back.
 *
 *  @param {import('./core.js').Target} at
 *  @param {string} ws
 *  @param {number} ttl */
function listed(at, ws, ttl) {
  const key = `${at.path}\0${ws}`;
  const entry = hovered.get(key);
  if (entry && (entry.at === null || Date.now() - entry.at <= ttl)) return entry.list;
  /** @type {{ at: number | null, got?: string[], list: Promise<{ paths: string[], truncated: boolean }> }} */
  const made = { at: null, list: Promise.resolve({ paths: [], truncated: false }) };
  made.list = getOn(at, `/api/paths?workspace=${encodeURIComponent(ws)}`)
    .then((a) => {
      made.at = Date.now();
      const paths = a.paths ?? [];
      made.got = paths;
      return { paths, truncated: !!a.truncated };
    });
  hovered.set(key, made);
  // A failure is not cached: the next hover asks again.
  void made.list.catch(() => { if (hovered.get(key) === made) hovered.delete(key); });
  return made.list;
}

/** Open what is on screen for editing.
 *
 *  No base pane, for the reason the search viewer has none: a file somebody
 *  clicked in a terminal is usually one nobody changed, so there is no revision
 *  to sit beside it.
 */
function edit() {
  if (!state.path || !state.ws) return toast('nothing to edit here', true);
  return Editor.open({
    mount: $('fvsrc'),
    mountClass: 'fnsrc editing',
    workspace: state.ws,
    path: state.path,
    base: null,
    save: $('fvsave'),
    edit: $('fvedit'),
    // Back to the viewer, on the file as it now is.
    onClosed: () => { viewer().drop(); void redraw(); },
    onSaved: () => { viewer().drop(); },
  // The buffer and the file are two different strings, and the bar was searching
  // the other one a moment ago.
  }).then(() => { seekChrome(); if (bar.on) runSeek(false); });
}

/** Draw the file again, where you were.
 *
 *  The line is remembered rather than reset: cancelling an edit on
 *  `src/main.rs:842` used to put you back at the top of the file, which is not
 *  where you were looking. */
async function redraw() {
  if (!state.open || !state.ws || !state.path) return;
  await viewer().show(state.ws, state.path, { line: state.line, last: state.last, col: 0, len: 0 });
  if (state.rendered && viewer().renderable()) viewer().render();
  // `redraw` is what runs when a buffer is cancelled, so the replace half goes
  // away here rather than being left over a file nobody can write to.
  seekChrome();
  if (bar.on) runSeek(false);
}

// ---------------------------------------------------------------------------
// Find in the file on screen
// ---------------------------------------------------------------------------
//
// **The workspace search cannot answer this, and not for want of trying.** It
// walks the tree and lists files; asking it about the file already in front of
// you throws away the file pane you are in and answers with an index of one. And
// once the editor is open it cannot answer at all — the text is a buffer nobody
// has written to disk, so the daemon would search the version you are editing
// away from. The match is therefore made in the page, over the string whichever
// of the two already holds, and `seek.js` is that and nothing else.
//
// Here rather than in `viewer.js` for the reason the viewer exists: the viewer
// draws one file and owns no reason for looking at it. A query is a reason.

/** The bar. `hits` is kept rather than recomputed because the arrows, the count
 *  and the ruler all read it between keystrokes, and `at` is which one is
 *  current — `-1` until something has been jumped to.
 *
 *  @type {{ on: boolean, hits: import('./seek.js').Hit[], at: number }} */
const bar = { on: false, hits: [], at: -1 };

/** Open the bar, or put the keyboard back in it when it is already open — which
 *  is what pressing the chord again means everywhere else. */
export function seek() {
  if (!state.open) return;
  if (viewer().isImage() && !Editor.isOpen()) return toast('a picture has nothing to search', true);
  bar.on = true;
  $('fvseek').hidden = false;
  seekChrome();
  const box = /** @type {HTMLInputElement} */ ($('fvseekq'));
  box.focus();
  box.select();
  runSeek(false);
}

/** Show the replace half only while a buffer is open. A file being read has
 *  nothing to write to, and a Replace button over it would be a refusal waiting
 *  to happen. */
function seekChrome() {
  const editing = bar.on && Editor.isOpen();
  for (const id of ['fvseekr', 'fvseekdo', 'fvseekall']) $(id).hidden = !editing;
}

export const seeking = () => bar.on;

/** Put the bar away, and the marks with it. The caret stays on the last match:
 *  in a buffer that is where you were going, and the pane has no caret to move. */
export function closeSeek() {
  if (!bar.on) return false;
  bar.on = false;
  bar.hits = [];
  bar.at = -1;
  $('fvseek').hidden = true;
  if (!Editor.isOpen()) viewer().setHits([]);
  else Editor.focus();
  return true;
}

/** How the query is to be read, from the two toggles. The same pair the
 *  workspace search carries, so one habit covers both boxes. */
const how = () => ({
  regex: $('fvseekre').classList.contains('on'),
  exact: $('fvseekcase').classList.contains('on'),
});

/** The text being searched, and where "from here" is in it. One function,
 *  because every caller has to ask both questions of the same source. */
function searched() {
  if (Editor.isOpen()) return { text: Editor.text(), from: Editor.caret() };
  const text = viewer().text();
  return { text, from: text == null ? 0 : offsetOf(text, state.line) };
}

/** Search again and say what was found.
 *
 *  `move` is whether to go to a match as well as count them. Typing moves the
 *  pane, because watching the file arrive under the query is the whole of what a
 *  find box is for. It does not move the *caret* in a buffer, which is a
 *  different thing: [`Editor.select`] says why.
 *
 *  @param {boolean} move */
function runSeek(move) {
  const q = /** @type {HTMLInputElement} */ ($('fvseekq')).value;
  const { text, from } = searched();
  bar.hits = [];
  bar.at = -1;
  if (text == null) return seekCount('nothing to search');
  const found = matches(text, q, how());
  if (found === null) return seekCount('bad pattern');
  bar.hits = found;
  if (!Editor.isOpen()) viewer().setHits(hitLines(found));
  if (!q) return seekCount('');
  if (!found.length) return seekCount('no matches');
  if (move) goToHit(nextIndex(found, from - 1, 1));
  else seekCount(countLabel());
}

/** `3 of 48`, or `of 48` before anything has been jumped to. The cap is said out
 *  loud: a count that stops at a round number without saying so is a lie about
 *  the file. */
function countLabel() {
  const n = bar.hits.length;
  const total = `${n}${n >= MAX ? '+' : ''}`;
  return bar.at < 0 ? `of ${total}` : `${bar.at + 1} of ${total}`;
}

function seekCount(/** @type {string} */ what) {
  $('fvseekn').textContent = what;
}

/** Show hit `i`, in whichever of the two is up. */
function goToHit(/** @type {number} */ i) {
  const hit = bar.hits[i];
  if (!hit) return;
  bar.at = i;
  if (Editor.isOpen()) Editor.select(hit.at, hit.len);
  else {
    /* A rendered page has no lines to point at, so a search is a reason to show
       the source — the same rule a line number follows in [`show`]. */
    if (state.rendered && viewer().renderable()) {
      state.rendered = false;
      viewer().renderSource();
      renderHead(true);
    }
    viewer().seek(hit.line, hit.col, hit.len);
    state.line = hit.line;
    state.last = 0;
  }
  seekCount(countLabel());
}

/** The arrows, and Enter in the box. Stepping is from the *current match* once
 *  there is one and from where the reader is before that, so the first press
 *  after typing goes forward from the line on screen rather than back to the top
 *  of the file.
 *
 *  @param {number} dir */
export function seekStep(dir) {
  if (!bar.on || !bar.hits.length) return;
  const here = bar.hits[bar.at];
  const from = here ? here.at + (dir > 0 ? here.len - 1 : 0) : searched().from - 1;
  goToHit(nextIndex(bar.hits, from, dir));
}

/** Wire the bar, once, at boot. */
function initSeek() {
  const box = /** @type {HTMLInputElement} */ ($('fvseekq'));
  box.oninput = () => runSeek(true);
  box.onkeydown = (e) => {
    if (e.key !== 'Enter') return;
    e.preventDefault();
    seekStep(e.shiftKey ? -1 : 1);
  };
  for (const id of ['fvseekcase', 'fvseekre']) {
    $(id).onclick = () => {
      $(id).classList.toggle('on');
      box.focus();
      runSeek(true);
    };
  }
  $('fvseekprev').onclick = () => seekStep(-1);
  $('fvseeknext').onclick = () => seekStep(1);
  $('fvseekx').onclick = () => closeSeek();
  const repl = /** @type {HTMLInputElement} */ ($('fvseekr'));
  repl.onkeydown = (e) => {
    if (e.key !== 'Enter') return;
    e.preventDefault();
    replaceOne();
  };
  $('fvseekdo').onclick = () => replaceOne();
  $('fvseekall').onclick = () => replaceEvery();
}

/** The query and the replacement as they stand, with the hits they apply to.
 *  Null when there is nothing to do, which both buttons answer the same way. */
function toReplace() {
  if (!bar.on || !Editor.isOpen() || !bar.hits.length) return null;
  const query = /** @type {HTMLInputElement} */ ($('fvseekq')).value;
  if (!query) return null;
  return { query, to: /** @type {HTMLInputElement} */ ($('fvseekr')).value };
}

/** Replace the match you are on, then go to the next.
 *
 *  **The one you are on, or the next one if you are not on one yet.** A Replace
 *  that silently started at the top of the file would rewrite a line nobody had
 *  looked at. */
function replaceOne() {
  const what = toReplace();
  if (!what) return;
  if (bar.at < 0) seekStep(1);
  const hit = bar.hits[bar.at];
  const text = Editor.text();
  if (!hit || text == null) return;
  const was = text.slice(hit.at, hit.at + hit.len);
  Editor.overwrite(hit.at, hit.len, substitute(was, what.query, what.to, how()));
  /* The buffer is a different string now, so every offset after this one has
     moved: the hits are found again rather than adjusted. The caret is where the
     replacement ended, so the next match is the one after it. */
  runSeek(false);
  seekStep(1);
}

/** Replace every match, as one edit — so one `Ctrl+Z` puts the file back. */
function replaceEvery() {
  const what = toReplace();
  if (!what) return;
  const text = Editor.text();
  if (text == null) return;
  const { text: out, count } = replaceAll(text, bar.hits, what.query, what.to, how());
  if (out === text) return toast('nothing to replace');
  Editor.overwrite(0, text.length, out);
  toast(`replaced ${count}`);
  runSeek(false);
}

/** Wire the chrome. Called once, at boot. */
export function init() {
  initSeek();
  $('fvmode').onclick = () => {
    state.rendered = !state.rendered;
    if (state.rendered) viewer().render();
    else viewer().renderSource();
    renderHead(true);
  };
  $('fvedit').onclick = () => (Editor.isOpen() ? Editor.close() : edit());
  $('fvsave').onclick = () => Editor.save();
  $('fvclose').onclick = () => void close();
  $('fvback').onclick = () => void back();
  /* The mouse's back button steps back, like `Escape`: through the files a
     rendered page's links led to, and then out. Button 3 is the back one. */
  $('fvoverlay').addEventListener('mousedown', (ev) => {
    if (/** @type {MouseEvent} */ (ev).button !== 3) return;
    ev.preventDefault();
    void back();
  });
}
