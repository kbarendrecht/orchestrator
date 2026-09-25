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
  $, activeWorkspaceId, borrowFocus, get, getOn, openMenu, reason, returnFocus, toast,
} from './core.js';
import * as Editor from './editor.js';
import { matching } from './pathlink.js';
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
 *  @type {{ open: boolean, ws: string | null, path: string | null, rendered: boolean,
 *           line: number, last: number }} */
const state = { open: false, ws: null, path: null, rendered: true, line: 1, last: 0 };

/** @type {ReturnType<typeof Viewer.create> | null} */
let view = null;
const viewer = () => {
  view ??= Viewer.create({ mount: $('fvsrc'), path: $('fvpath'), where: $('fvwhere') });
  return view;
};

export const isOpen = () => state.open;

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

/** @param {string} ws @param {string} path @param {number} line @param {number} [last] */
async function show(ws, path, line, last) {
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
  state.open = true;
  state.ws = ws;
  state.path = path;
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
}

/** The header's two buttons: what this file can be shown as, and what it is
 *  being shown as. */
function renderHead(/** @type {boolean} */ drawn) {
  const can = drawn && viewer().renderable();
  $('fvmode').hidden = !can;
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
  state.open = false;
  state.path = null;
  $('fvoverlay').classList.remove('on');
  returnFocus('fileview', $('fvoverlay'));
  return true;
}

/** The overlay belongs to the session it was opened from, so switching away
 *  closes it — the same rule the diff and the finder hold. */
export function syncToSession() {
  if (state.open && activeWorkspaceId() !== state.ws) void close();
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
 *  slow walk is joined rather than started twice.
 *
 *  @type {Map<string, { at: number | null,
 *                       list: Promise<{ paths: string[], truncated: boolean }> }>} */
const hovered = new Map();

/** Whether a click on `path` would find anything, which is what earns it an
 *  underline. The click itself still walks fresh, in [`candidates`].
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
    l.truncated || matching(l.paths, path).length > 0;
  if (has(await listed(at, ws, HOVER_TTL))) return true;
  return has(await listed(at, ws, MISS_TTL));
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
  /** @type {{ at: number | null, list: Promise<{ paths: string[], truncated: boolean }> }} */
  const made = { at: null, list: Promise.resolve({ paths: [], truncated: false }) };
  made.list = getOn(at, `/api/paths?workspace=${encodeURIComponent(ws)}`)
    .then((a) => { made.at = Date.now(); return { paths: a.paths ?? [], truncated: !!a.truncated }; });
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
  });
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
}

/** Wire the chrome. Called once, at boot. */
export function init() {
  $('fvmode').onclick = () => {
    state.rendered = !state.rendered;
    if (state.rendered) viewer().render();
    else viewer().renderSource();
    renderHead(true);
  };
  $('fvedit').onclick = () => (Editor.isOpen() ? Editor.close() : edit());
  $('fvsave').onclick = () => Editor.save();
  $('fvclose').onclick = () => void close();
  /* The mouse's back button closes it, because this overlay is only ever a place
     you were sent to — there is no trail through it the way there is through a
     chain of definition jumps. Button 3 is the back one. */
  $('fvoverlay').addEventListener('mousedown', (ev) => {
    if (/** @type {MouseEvent} */ (ev).button !== 3) return;
    ev.preventDefault();
    void close();
  });
}
