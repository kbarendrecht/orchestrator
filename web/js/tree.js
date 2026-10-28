// The changes pane's other view: every file in the workspace, as folders.
//
// **Built from the list the name search already fetches** (`/api/paths`), so the
// tree honours `.gitignore` and leaves main's worktrees out for the same reason
// the finder does, and there is no second walk to keep in step with it. A folder
// the walk left out as too big is drawn anyway and listed when you open it, the
// way the finder lists it when you type its name.

import { el, get, icon, paintSig, reason, reconcile, workspaceById } from './core.js';
import * as FileView from './fileview.js';

/** How long a fetched list is trusted without a reason to ask again. A changed
 *  file list that moves is that reason, so this only covers an edit git cannot
 *  see, such as a new ignored file. */
const TTL = 60_000;

/** One workspace's list and which of its folders are open.
 *
 *  `tree` is per folder, `''` being the root: the folders and files directly in
 *  it. `big` is the left-out folders, `under` their listings once opened, or
 *  `'loading'`, or why they could not be listed. `rev` moves whenever any of it
 *  does, for the pane's paint check.
 *
 *  @typedef {{ dirs: Set<string>, files: Set<string> }} Folder
 *  @type {{ rev: number, ws: string | null, key: string | null, at: number, loading: boolean, failed: string | null,
 *           tree: Map<string, Folder>, total: number, truncated: boolean, big: Set<string>,
 *           under: Map<string, string | string[]>, open: Map<string, Set<string>> }} */
const state = {
  rev: 0, ws: null, key: null, at: 0, loading: false, failed: null,
  tree: new Map(), total: 0, truncated: false, big: new Set(), under: new Map(), open: new Map(),
};

/** Every caller that asked while a fetch was in flight. The pane and the finder
 *  both draw from this one cache, so the answer has to reach whichever asked, not
 *  only the one whose ask started the fetch. */
const waiting = new Set();

/** Fold `paths` into `tree`, every ancestor folder included. */
function fold(/** @type {Map<string, Folder>} */ tree, /** @type {string[]} */ paths) {
  const folder = (/** @type {string} */ dir) => {
    let f = tree.get(dir);
    if (!f) {
      f = { dirs: new Set(), files: new Set() };
      tree.set(dir, f);
      if (dir) {
        const cut = dir.lastIndexOf('/');
        folder(cut < 0 ? '' : dir.slice(0, cut)).dirs.add(dir);
      }
    }
    return f;
  };
  folder('');
  for (const p of paths) {
    const cut = p.lastIndexOf('/');
    const f = folder(cut < 0 ? '' : p.slice(0, cut));
    f.files.add(p);
  }
}

/** Ask for the list again when `key` moved or the answer is stale, keeping the
 *  last tree on screen until the new one lands. */
function load(/** @type {string} */ ws, /** @type {string} */ key, /** @type {() => void} */ redraw) {
  const fresh = state.ws === ws && state.key === key && Date.now() - state.at < TTL;
  if (fresh) return;
  waiting.add(redraw);
  if (state.loading) return;
  if (state.ws !== ws) {
    state.tree = new Map();
    state.under = new Map();
    state.failed = null;
  }
  state.loading = true;
  get(`/api/paths?workspace=${encodeURIComponent(ws)}`).then((/** @type {import('../repo').Paths} */ a) => {
    const tree = new Map();
    fold(tree, a.paths);
    for (const dir of a.left_out) fold(tree, [`${dir}/`]);
    // A big folder already opened keeps its listing across a refresh.
    for (const got of state.under.values()) if (Array.isArray(got)) fold(tree, got);
    Object.assign(state, {
      ws, key, at: Date.now(), tree, total: a.paths.length, truncated: a.truncated,
      big: new Set(a.left_out), failed: null,
    });
  }, (e) => {
    Object.assign(state, { ws, key, at: Date.now(), failed: reason(e) });
  }).finally(() => {
    state.loading = false;
    state.rev++;
    const all = [...waiting];
    waiting.clear();
    for (const r of all) r();
  });
}

/** List one left-out folder, the first time it is opened. */
function loadUnder(/** @type {string} */ ws, /** @type {string} */ dir, /** @type {() => void} */ redraw) {
  if (state.under.has(dir)) return;
  state.under.set(dir, 'loading');
  get(`/api/paths?workspace=${encodeURIComponent(ws)}&under=${encodeURIComponent(dir)}`).then((/** @type {import('../repo').Paths} */ a) => {
    if (state.ws !== ws) return;
    state.under.set(dir, a.paths);
    fold(state.tree, a.paths);
  }, (e) => {
    state.under.set(dir, reason(e));
  }).finally(() => {
    state.rev++;
    redraw();
  });
}

const leaf = (/** @type {string} */ p) => p.slice(p.lastIndexOf('/', p.length - 2) + 1);
const byName = (/** @type {string} */ a, /** @type {string} */ b) => leaf(a).localeCompare(leaf(b));

/** Draw the tree into `mount`.
 *
 *  `changed` is the pane's own list, so a file in the tree carries the letter its
 *  row there carries and a folder that holds one says so with a dot. A deleted
 *  file is on that list and not on disk, so it is put back in, struck through,
 *  where it was.
 *
 *  `pick` is what a file row does, opening the file pane when not given; the
 *  finder passes its own, to show the file beside the tree. `picked` is the row
 *  to mark as the one shown.
 *
 *  @param {HTMLElement} mount
 *  @param {string} ws
 *  @param {{ path: string, status: string }[]} changed
 *  @param {() => void} redraw
 *  @param {{ pick?: (path: string, ev: MouseEvent) => void, picked?: string | null }} [how]
 *  @returns {string} what the footer says */
export function render(mount, ws, changed, redraw, how = {}) {
  /* Keyed on the snapshot's own list, not on `changed`: the pane hands over its
     fetched list when the snapshot's is capped, the finder the snapshot's, and
     two keys for one cache refetched the whole walk every time the other drew. */
  const w = workspaceById(ws);
  load(ws, paintSig([w?.changed?.map((f) => f.path) ?? [], w?.changed_total, w?.changed_since]), redraw);
  if (state.ws !== ws) {
    reconcile(mount, [{ key: 'twait', sig: '', build: () => el('div', 'fempty', 'Listing the files…') }]);
    return '';
  }
  if (state.failed) {
    const failed = state.failed;
    reconcile(mount, [{ key: 'tfail', sig: failed, build: () => el('div', 'fempty', failed) }]);
    return '';
  }

  const status = new Map(changed.map((f) => [f.path, (f.status || 'M')[0] ?? 'M']));
  /* Git reports a new folder as one row, `src/new/`, so what is in it carries that
     row's mark rather than none. */
  const fresh = [...status.keys()].filter((p) => p.endsWith('/'));
  const inFresh = (/** @type {string} */ p) => fresh.some((d) => p.startsWith(d));
  const gone = changed.filter((f) => f.status === 'D').map((f) => f.path);
  const tree = state.tree;
  fold(tree, gone);
  /* The folders on the way to a change, for the dot. An untracked directory comes
     as one row, `src/new/`, so it marks itself as well as its parents. */
  const holds = new Set();
  for (const p of status.keys()) {
    const dir = p.endsWith('/') ? p.slice(0, -1) : p;
    for (let i = dir.indexOf('/'); i >= 0; i = dir.indexOf('/', i + 1)) holds.add(dir.slice(0, i));
    if (p.endsWith('/')) holds.add(dir);
  }

  let open = state.open.get(ws);
  if (!open) state.open.set(ws, open = new Set());
  const opened = open;

  /** @type {{ key: string, sig: string, build: () => Element }[]} */
  const items = [];
  const walk = (/** @type {string} */ dir, /** @type {number} */ depth) => {
    const f = tree.get(dir);
    if (!f) return;
    for (const d of [...f.dirs].sort(byName)) {
      const isOpen = opened.has(d);
      const big = state.big.has(d);
      const got = state.under.get(d);
      const marked = holds.has(d) || inFresh(`${d}/`);
      const note = !big ? '' : got === 'loading' ? 'listing…' : typeof got === 'string' ? got : '';
      items.push({
        key: `td:${d}`,
        sig: paintSig([isOpen, marked, note, depth]),
        build: () => dirRow(d, depth, isOpen, marked, note, () => {
          if (opened.has(d)) opened.delete(d);
          else {
            opened.add(d);
            if (big) loadUnder(ws, d, redraw);
          }
          state.rev++;
          redraw();
        }),
      });
      if (isOpen) walk(d, depth + 1);
    }
    for (const p of [...f.files].sort(byName)) {
      // `src/new/` is a folder git reported whole, already drawn as one.
      if (p.endsWith('/')) continue;
      const st = status.get(p) ?? (inFresh(p) ? '?' : '');
      const on = p === how.picked;
      items.push({ key: `tf:${p}`, sig: paintSig([st, depth, ws, on]), build: () => fileRow(ws, p, depth, st, on, how.pick) });
    }
  };
  walk('', 0);
  if (items.length === 0) items.push({ key: 'tnone', sig: '', build: () => el('div', 'fempty', 'No files here.') });
  reconcile(mount, items);

  const n = state.total.toLocaleString();
  return state.truncated ? `first ${n} files` : `${n} file${state.total === 1 ? '' : 's'}`;
}

/** Moves whenever the tree would draw differently for the same snapshot. */
export const rev = () => state.rev;

/* Outline glyphs on the 16px grid `core.icon` draws: a folder shut and open,
   and a page with its corner turned. They do the chevron's job for a folder and
   give a file the same left edge. */
const FOLDER = ['M2 4.6c0-.9.7-1.6 1.6-1.6h2.5l1.5 1.7h4.8c.9 0 1.6.7 1.6 1.6v5.1c0 .9-.7 1.6-1.6 1.6H3.6c-.9 0-1.6-.7-1.6-1.6z'];
const FOLDER_OPEN = ['M2 11.4V4.6c0-.9.7-1.6 1.6-1.6h2.5l1.5 1.7h3.8c.9 0 1.6.7 1.6 1.6v.9',
  'M2.3 12.4 3.8 8.3c.2-.6.8-1 1.4-1h8.1c.7 0 1.2.7 1 1.4l-1.1 3.3c-.2.6-.8 1-1.4 1H3.4'];
const FILE = ['M9.3 2H4.6C3.7 2 3 2.7 3 3.6v8.8c0 .9.7 1.6 1.6 1.6h6.8c.9 0 1.6-.7 1.6-1.6V5.7z', 'M9.3 2v3.7H13'];

/** What a change dot stands for, for its tooltip: the dot carries no letter. */
const SAID = /** @type {Record<string, string>} */ ({ M: 'modified', A: 'added', D: 'deleted', R: 'renamed', '?': 'untracked' });

function dirRow(
  /** @type {string} */ dir, /** @type {number} */ depth, /** @type {boolean} */ isOpen,
  /** @type {boolean} */ holds, /** @type {string} */ note, /** @type {() => void} */ toggle,
) {
  const row = el('button', 'trow dir' + (isOpen ? ' open' : ''));
  row.style.setProperty('--d', String(depth));
  row.setAttribute('aria-expanded', String(isOpen));
  row.append(icon('tic', 1.3, ...(isOpen ? FOLDER_OPEN : FOLDER)), el('span', 'tname', leaf(dir)));
  if (note) row.appendChild(el('span', 'ttag', note));
  if (holds) row.appendChild(el('span', 'tdot'));
  row.title = `${dir}/`;
  row.onclick = toggle;
  return row;
}

function fileRow(
  /** @type {string} */ ws, /** @type {string} */ path, /** @type {number} */ depth, /** @type {string} */ st,
  /** @type {boolean} */ on, /** @type {((path: string, ev: MouseEvent) => void) | undefined} */ pick,
) {
  const row = el('button', 'trow' + (st === 'D' ? ' gone' : '') + (on ? ' on' : ''));
  row.style.setProperty('--d', String(depth));
  row.append(icon('tic', 1.3, ...FILE), el('span', 'tname', leaf(path)));
  /* The letter is the class, as in the changes list: `.tcd.M`, `.tcd.A`,
     `.tcd.D`, `.tcd.R`, and `?` for untracked, which CSS spells `.tcd.U`. */
  if (st) row.appendChild(el('span', 'tcd ' + (st === '?' ? 'U' : st)));
  row.title = st ? `${path} · ${SAID[st] ?? st}` : path;
  // A deleted file has nothing on disk to show; its row is there to say where it was.
  if (st !== 'D') row.onclick = (ev) => (pick ? pick(path, ev) : void FileView.open(ws, [path], 0, undefined, ev));
  return row;
}
