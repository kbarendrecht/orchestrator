// One file on screen, drawn as a band around the line you were sent to.
//
// **Lifted out of `find.js` when a second pane needed it**, which is the same
// move `source.js` was: a search result and a path an agent printed want exactly
// the same picture of a file, and the one that had it was welded to the search's
// state — its hits, its cursor, its query. What is here is the picture and
// nothing else. Which file, and why you are looking at it, belong to whoever
// opened it.
//
// The band is the whole design. 320 rows are drawn and the rest of the file is
// two spacers, because a row is a DOM node per line — `core.js` is 2,002 of them
// — and this app paints into WebKitGTK with xterm's DOM renderer on the same
// page. The spacers are not padding, they are the scrollbar: without them the
// document is as tall as the band and scrolling stops after 320 lines, which
// reads as a truncated file rather than a broken viewer.

import { activeCheckout, appMod, call, copyText, el, FORGE_LABEL, get, openMenu, reason, safeHref, snap, toast } from './core.js';
import { byteRange, charRanges, hlTokens, langFor, paintRanges } from './source.js';
import { parse } from './markdown.js';
import { refsIn } from './pathlink.js';
import { indentUnit } from './editkeys.js';
import { lineOf } from './seek.js';

/** Rows drawn around the line, and how close to the edge the viewport gets
 *  before the band moves. */
const BAND = 320;
const MARGIN = 80;
/** Past this the file is shown without colour, and the header says so. Prism
 *  tokenises a line at a time here, so the cost is the band; the cap is about the
 *  fetch and the string, not the highlighting. */
const HUGE = 512 * 1024;
/** Files shown as a picture rather than as lines: what the daemon's image route
 *  serves, so the two cannot disagree about which files are pictures. */
export const IMAGE = /\.(png|jpe?g|gif|webp|avif|svg|ico|bmp)$/i;
/** Scope lines kept at the top at most. Past three they are a second header
 *  taking the pane, and the breadcrumbs name the rest. */
const STICKY = 3;
/** The width a tab is drawn at: the browser's default, which `.fnsrc` keeps. */
const TAB = 8;

/**
 * @typedef {{ line: number, last?: number, col: number, len: number }} Spot
 * @typedef {{ path: string, lines: string[], lang: string | null, plain: boolean }} Loaded
 */

/* **Blame is one switch for every viewer**, remembered per browser like the
   pane sizes: it is a way of reading code, not a property of one file. Each
   viewer registers its redraw, so flipping it repaints whatever is on screen. */
const BLAME_KEY = 'orch.blame';
let blameOn = false;
try {
  blameOn = localStorage.getItem(BLAME_KEY) === '1';
} catch (err) { /* no storage: blame starts off */ }
/** @type {Set<() => void>} */
const redraws = new Set();

/** Turn the blame gutter on or off in every viewer. */
export function toggleBlame() {
  blameOn = !blameOn;
  try {
    localStorage.setItem(BLAME_KEY, blameOn ? '1' : '0');
  } catch (err) { /* the switch still works for this page */ }
  for (const r of redraws) r();
}

/** `3d`, `5mo`, `2y`: how long ago, at the width a gutter has. */
function age(/** @type {number} */ secs) {
  const d = Math.max(0, Date.now() / 1000 - secs) / 86400;
  if (d < 1) return 'today';
  if (d < 30) return `${Math.floor(d)}d`;
  if (d < 365) return `${Math.floor(d / 30)}mo`;
  return `${Math.floor(d / 365)}y`;
}

/** A viewer over one mount point.
 *
 *  `path` and `where` are the two header elements it writes: which file, and
 *  where in it. A pane that shows neither passes `null` for them.
 *
 *  `onFile` is how a rendered page follows a link to another file in the repo,
 *  already resolved against the page's own directory: a pane that can open files
 *  passes it, and without one those links do nothing.
 *
 *  `crumbs` is where the scope the pane is scrolled to is named, `Order › total`,
 *  and a click on it lists the file's outline. A pane without one passes nothing.
 *
 *  @param {{ mount: HTMLElement, path?: HTMLElement | null, where?: HTMLElement | null,
 *            crumbs?: HTMLElement | null, onFile?: (rel: string, line: number) => void }} on
 */
export function create(on) {
  /** `mode` is which of the two pictures is mounted. It exists because the band
   *  is rebuilt on scroll, and a rebuild while the markdown page is up replaces
   *  it with rows — reported as "scrolling toggled it back to source", which is
   *  exactly what it was.
   *
   *  `ws` is the workspace the file was read from, which the preview needs to
   *  ask for its token.
   *
   *  `readOnly` is the daemon saying the file was reached through a link out of
   *  the checkout (#48), so it is shown and never written.
   *
   *  @type {{ file: Loaded | null, spot: Spot | null, from: number, to: number,
   *           rowH: number, seq: number, mode: 'source' | 'markdown' | 'preview' | 'image',
   *           ws: string | null, readOnly: boolean }} */
  const view = {
    file: null, spot: null, from: 0, to: 0, rowH: 0, seq: 0, mode: 'source', ws: null, readOnly: false,
  };
  /** Blame for the file on screen, by line, and the bands already asked for so
   *  a scroll back does not ask git again. Both cleared with the file. */
  /** @type {Map<number, import('../repo').BlameLine>} */
  let blame = new Map();
  /** @type {Set<string>} */
  let blameAsked = new Set();
  /** Where the file differs from its last commit, and where a search hit it, by
   *  line: what the ruler beside the scrollbar marks. */
  /** @type {{ line: number, kind: 'add' | 'del' }[]} */
  let changed = [];
  /** @type {number[]} */
  let hits = [];

  /** The file's folds and outline, from the daemon's parse, and each fold by the
   *  line its marker is on. Empty for a language it has no grammar for, which
   *  draws the file as it always was. */
  /** @type {import('../repo').Structure} */
  let shape = { folds: [], symbols: [] };
  /** @type {Map<number, import('../repo').Fold>} */
  let foldAt = new Map();
  /** The marker lines of the folds that are closed. */
  /** @type {Set<number>} */
  let closed = new Set();
  /** **Which file lines are rows**, 0-based and in order, while anything is
   *  folded; null when every line is one. Everything the band measures — its
   *  start, the spacers, the scroll — is in rows, so a fold is just fewer of them
   *  and the scrollbar stays honest. */
  /** @type {number[] | null} */
  let shown = null;
  /** Each line's indent in columns, a blank line taking the smaller of its
   *  neighbours' so a guide runs through it, and the file's indent step. */
  /** @type {number[]} */
  let indents = [];
  let step = 2;

  const rowCount = () => (shown ? shown.length : view.file?.lines.length ?? 0);
  /** The file line, 0-based, drawn as row `r`. */
  const lineAt = (/** @type {number} */ r) => (shown ? shown[r] ?? 0 : r);
  /** The row a file line is drawn on, or the row of the fold hiding it. */
  const rowOf = (/** @type {number} */ i) => (shown ? lineOf(shown, i) : i);
  /** Recount the rows after a fold opened or closed. */
  function refold() {
    const total = view.file?.lines.length ?? 0;
    if (!closed.size || !total) {
      shown = null;
      return;
    }
    const hide = new Uint8Array(total);
    for (const n of closed) {
      const f = foldAt.get(n);
      // `line + 1 ..= last`, 1-based, is `line ..< last` 0-based.
      if (f) hide.fill(1, f.line, Math.min(f.last, total));
    }
    shown = [];
    for (let i = 0; i < total; i++) if (!hide[i]) shown.push(i);
  }
  /** Open every fold hiding any of lines `lo ..= hi`, so a jump never lands
   *  inside one. */
  function reveal(/** @type {number} */ lo, /** @type {number} */ hi) {
    let opened = false;
    for (const m of closed) {
      const f = foldAt.get(m);
      if (f && f.line < hi && f.last >= lo) {
        closed.delete(m);
        opened = true;
      }
    }
    if (opened) refold();
  }

  /** The symbols whose extent holds line `n` and starts above it, outermost
   *  first: what the sticky lines and the breadcrumbs both name. */
  const scopesAt = (/** @type {number} */ n) => shape.symbols.filter((s) => s.line < n && s.end >= n);

  /** Ask the daemon how the loaded file is built, and fold its imports. */
  async function askStructure() {
    const file = view.file;
    if (!file || file.plain) return;
    /** @type {import('../repo').Structure} */
    let answer;
    try {
      answer = await call('/api/structure', { path: file.path, text: file.lines.join('\n') });
    } catch (e) {
      // No structure is the viewer it always was; nothing to say about it.
      return;
    }
    if (view.file !== file) return;
    shape = answer;
    foldAt = new Map(answer.folds.map((f) => [f.line, f]));
    /* **Imports start folded**: the top of nearly every file, and the part
       nobody opened it to read. Not the run the line you were sent to is in —
       an agent pointing at an import is pointing at the import. */
    const at = view.spot?.line ?? 0;
    const upTo = Math.max(at, view.spot?.last ?? 0);
    for (const f of answer.folds) {
      if (f.kind === 'imports' && !(upTo > f.line && at <= f.last)) closed.add(f.line);
    }
    /* **The line at the top stays at the top.** The answer lands after the file
       is drawn, and a caller may have scrolled since — the file pane's back
       button puts you where you were — so this must not jump to the spot. */
    if (view.mode !== 'source' || !view.spot || !view.rowH || held()) return refold();
    const into = on.mount.scrollTop % view.rowH;
    const top = lineAt(Math.floor(on.mount.scrollTop / view.rowH));
    refold();
    const r = rowOf(top);
    band(Math.max(0, r - Math.floor(BAND / 3)), view.spot);
    on.mount.scrollTop = r * view.rowH + into;
  }

  /** Each line's indent, in columns, for the guides. */
  function measureIndents(/** @type {string[]} */ lines) {
    const out = lines.map((l) => {
      if (!l.trim()) return -1;
      let col = 0;
      for (const c of l) {
        if (c === ' ') col++;
        else if (c === '\t') col += TAB - (col % TAB);
        else break;
      }
      return col;
    });
    // A blank line runs the guide through at the smaller of the indents around
    // it, so a gap inside a block does not break the line down its side.
    let before = 0;
    const back = [];
    for (let i = out.length - 1, after = 0; i >= 0; i--) {
      const v = out[i] ?? -1;
      if (v >= 0) after = v;
      back[i] = after;
    }
    for (let i = 0; i < out.length; i++) {
      const v = out[i] ?? -1;
      if (v >= 0) before = v;
      else out[i] = Math.min(before, back[i] ?? 0);
    }
    return out;
  }

  /** The uncommitted changes in the file on screen, for the ruler. */
  async function askChanges() {
    const file = view.file;
    if (!file || !view.ws) return;
    try {
      const q = new URLSearchParams({ workspace: view.ws, path: file.path, base: 'head', context: '0' });
      /** @type {import('../repo').FileDiff} */
      const d = await get(`/api/diff/file?${q}`);
      if (view.file !== file) return;
      /** @type {{ line: number, kind: 'add' | 'del' }[]} */
      const out = [];
      for (const h of d.hunks) {
        let next = h.new_start;
        for (const r of h.rows) {
          if (r.kind === 'add' && r.new != null) { out.push({ line: r.new, kind: 'add' }); next = r.new + 1; }
          if (r.kind === 'del') out.push({ line: Math.max(1, next), kind: 'del' });
        }
      }
      changed = out;
      redrawRuler();
    } catch (e) {
      // No ruler is the answer for a file git cannot diff; the file still shows.
    }
  }

  /** Whether the editor has this pane. **The viewer then draws nothing**: it
   *  shares the mount with the buffer, and every redraw it makes replaces the
   *  mount's children. "Show blame" from a right-click in the buffer redrew the
   *  rows over it and the pane went empty; a blame answer or the ruler's marks
   *  landing late would have done the same. */
  const held = () => !!on.mount.querySelector(':scope > .editwrap, :scope > .editarea');

  /* **The select-all chord selects the file, not the page.** Nothing in the pane
     takes focus, so the browser's own select-all took the whole window with it.
     A text field keeps its own; the editor's buffer gets it even when the focus
     was elsewhere. The rows are a band, so the selection can only cover what is
     drawn, and `all` is what lets the copy below hand over every line. */
  let all = false;
  document.addEventListener('keydown', (ev) => {
    if (ev.key.toLowerCase() !== 'a' || !appMod(ev) || ev.shiftKey) return;
    if (!on.mount.offsetParent) return;
    // Every viewer listens on the document, and two can be up at once: the file
    // pane opened over the finder. Only the one actually on top may answer, or
    // the later one selected its file under the one you were looking at.
    const box = on.mount.getBoundingClientRect();
    const top = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
    if (!top || !on.mount.contains(top)) return;
    const t = /** @type {HTMLElement} */ (ev.target);
    if (t.closest?.('input, textarea, [contenteditable]')) return;
    ev.preventDefault();
    const ta = /** @type {HTMLTextAreaElement | null} */ (on.mount.querySelector('.editarea'));
    if (ta) {
      ta.focus();
      ta.select();
      return;
    }
    const range = document.createRange();
    range.selectNodeContents(on.mount);
    const sel = getSelection();
    sel?.removeAllRanges();
    sel?.addRange(range);
    all = view.mode === 'source';
  });
  document.addEventListener('copy', (ev) => {
    // By its ends, not by identity: WebKit hands back a copy of the range.
    const r = getSelection()?.rangeCount ? getSelection()?.getRangeAt(0) : null;
    const still = all && r && r.startContainer === on.mount && r.endContainer === on.mount
      && r.startOffset === 0 && r.endOffset === on.mount.childNodes.length;
    if (!still || !view.file || !ev.clipboardData) return;
    ev.preventDefault();
    ev.clipboardData.setData('text/plain', view.file.lines.join('\n'));
  });

  /** Make `content` the loaded file, and forget everything measured on the last
   *  one. Both ways in go through here, so per-file state is reset in one place. */
  function load(/** @type {string} */ path, /** @type {string} */ content) {
    const plain = content.length > HUGE;
    view.file = { path, lines: content.split('\n'), lang: plain ? null : langFor(path), plain };
    view.rowH = 0;
    blame = new Map();
    blameAsked = new Set();
    changed = [];
    hits = [];
    shape = { folds: [], symbols: [] };
    foldAt = new Map();
    closed = new Set();
    shown = null;
    scopesKey = '';
    indents = plain ? [] : measureIndents(view.file.lines);
    const unit = plain ? '' : indentUnit(content);
    step = unit === '\t' ? TAB : unit.length || 2;
  }

  /** Swap the ruler alone when its marks arrive. **Not the band**: a rebuilt
   *  band can move the pane's scroll, and a scroll closes an open menu, so the
   *  marks landing a moment after a right-click took the menu away with them. */
  function redrawRuler() {
    const old = on.mount.querySelector(':scope > .ruler');
    if (old && view.file && view.mode === 'source') old.replaceWith(ruler(rowCount()));
  }

  /** A strip beside the scrollbar with the changes and the hits on it, each a
   *  click away. Sticky inside the scrolling pane, so it is the pane's height
   *  wherever the band is, and takes no room from the lines. */
  function ruler(/** @type {number} */ total) {
    const h = on.mount.clientHeight;
    const strip = el('div', 'ruler');
    strip.style.height = `${h}px`;
    strip.style.marginBottom = `${-h}px`;
    if (!total || !h) return strip;
    const mark = (/** @type {number} */ line, /** @type {string} */ cls, /** @type {string} */ tip) => {
      const m = el('i', `rm ${cls}`);
      // By row, not by line: the scrollbar is rows, and a mark has to sit where
      // the thumb goes to reach it. A line in a fold marks the fold.
      m.style.top = `${(rowOf(line - 1) / total) * 100}%`;
      m.title = tip;
      m.onclick = () => paint({ line, col: 0, len: 0 });
      strip.appendChild(m);
    };
    // One mark per line is plenty: a file with a thousand changed lines is a
    // strip of colour either way, and the DOM is the cost.
    for (const c of changed.slice(0, 600)) mark(c.line, c.kind, `line ${c.line}, ${c.kind === 'add' ? 'changed' : 'removed'} since the last commit`);
    for (const n of hits.slice(0, 600)) mark(n, 'hit', `line ${n}, a search hit`);
    return strip;
  }
  redraws.add(() => {
    if (view.mode === 'source' && view.file && view.spot) band(view.from, view.spot);
  });

  /** Fetch blame for the drawn band, once, and draw it when it lands. */
  async function askBlame(/** @type {number} */ from, /** @type {number} */ to) {
    const file = view.file;
    const key = `${file?.path}:${from}`;
    if (!file || !view.ws || blameAsked.has(key)) return;
    blameAsked.add(key);
    try {
      const q = new URLSearchParams({ workspace: view.ws, path: file.path, from: String(from + 1), to: String(to) });
      /** @type {import('../repo').BlameLine[]} */
      const lines = await get(`/api/blame?${q}`);
      if (view.file !== file) return;
      for (const l of lines) blame.set(l.line, l);
      // Lines git had nothing for are still answered, or the band asks forever.
      for (let n = from + 1; n <= to; n++) {
        if (!blame.has(n)) blame.set(n, { line: n, commit: '', author: '', time: 0, summary: '' });
      }
      if (blameOn && view.mode === 'source' && view.spot) band(view.from, view.spot);
    } catch (e) {
      toast(reason(e), true);
    }
  }

  /* A click on a file link in the rendered page. Resolved the way the repo's own
     docs mean it — against the directory of the file that holds the link, and a
     leading `/` against the workspace root, as GitHub reads it — and `#L42` as a
     line. A link that climbs out of the workspace goes nowhere. */
  on.mount.addEventListener('click', (ev) => {
    // A run's blame label opens its commit, the way a forge's blame view does.
    const label = /** @type {HTMLElement | null} */ (/** @type {HTMLElement} */ (ev.target).closest?.('b.blame[data-b]'));
    const n = label?.parentElement?.querySelector('i')?.dataset.n;
    const sha = n ? blame.get(Number(n))?.commit : '';
    if (sha) return openCommit(sha);
    // A ref in prose, `#36356` or `sc-54841`: the daemon knows where it lives.
    const ref = /** @type {HTMLElement | null} */ (/** @type {HTMLElement} */ (ev.target).closest?.('a.md-ref'));
    if (ref?.dataset.ref) {
      ev.preventDefault();
      call('/api/open/ref', { ref: ref.dataset.ref }).catch((err) => toast(reason(err), true));
      return;
    }
    const a = /** @type {HTMLElement | null} */ (/** @type {HTMLElement} */ (ev.target).closest?.('a.md-file'));
    if (!a || !on.onFile || !view.file) return;
    ev.preventDefault();
    // A scratchpad file has no workspace for its links to be in.
    if (!view.ws) return;
    const [target = '', frag = ''] = (a.dataset.file ?? '').split('#');
    let wanted = target;
    try {
      wanted = decodeURIComponent(target);
    } catch { /* not an escape, just a percent sign */ }
    const dir = view.file.path.split('/').slice(0, -1);
    const parts = wanted.startsWith('/') ? [] : dir;
    for (const p of wanted.split('/')) {
      if (!p || p === '.') continue;
      if (p === '..') {
        if (!parts.length) return;
        parts.pop();
      } else parts.push(p);
    }
    if (!parts.length) return;
    on.onFile(parts.join('/'), Number(frag.match(/^L(\d+)/)?.[1] ?? 0));
  });

  /* **The file's own menu: its path, and the line under the pointer.** Workspace-
     relative, `src/Domain/Order.php:42`, because that is what an agent, a PR
     comment and every editor's "go to file" take. Here rather than in each pane,
     so the finder and the file pane cannot drift. The app's menu replaces the
     webview's, so a selection gets its plain copy back as the first item. */
  on.mount.addEventListener('contextmenu', (ev) => {
    // The buffer's own menu while it is up: cut, copy, paste are its, and this
    // one's items would draw rows over it.
    if (held()) return;
    const path = view.file?.path ?? (view.mode === 'image' ? on.path?.textContent : null);
    if (!path) return;
    const row = /** @type {HTMLElement | null} */ (
      /** @type {HTMLElement} */ (ev.target).closest?.('.fnrow'));
    const n = row?.querySelector('i')?.dataset.n;
    const copy = (/** @type {string} */ text) => () => {
      void copyText(text).then((ok) => { if (ok) toast(`copied ${text}`); });
    };
    /** @type {[string, string | null, (() => void) | null][]} */
    const items = [];
    const picked = String(window.getSelection() ?? '');
    if (picked) items.push(['copy selection', null, copy(picked)]);
    items.push(['copy path', null, copy(path)]);
    if (n) items.push(['copy path:line', null, copy(`${path}:${n}`)]);
    if (view.mode === 'source') items.push([blameOn ? 'hide blame' : 'show blame', null, toggleBlame]);
    if (view.mode === 'source' && foldAt.size) {
      items.push(closed.size ? ['unfold all', null, () => foldAll(false)] : ['fold all', null, () => foldAll(true)]);
    }
    // The commit behind the line, from the blame column only: it is the column's
    // answer, and on the code it read as a fact about the code. A line nobody has
    // committed has no commit to open.
    const inGutter = /** @type {HTMLElement} */ (ev.target).closest?.('b.blame');
    const sha = inGutter && n ? blame.get(Number(n))?.commit : '';
    if (sha) items.push([`open ${sha.slice(0, 7)} on ${FORGE_LABEL[snap.forge]}`, null, () => openCommit(sha)]);
    openMenu(ev, items);
  });

  /** Open a blamed commit on the forge. */
  const openCommit = (/** @type {string} */ sha) => {
    call('/api/open/commit', { sha }).catch((err) => toast(reason(err), true));
  };

  /** Keep the band under the viewport as it is scrolled. */
  on.mount.addEventListener('scroll', () => {
    // Nothing to keep under the viewport when the viewport holds a document.
    if (view.mode !== 'source') return;
    if (!view.file || !view.rowH || !view.spot) return;
    const first = Math.floor(on.mount.scrollTop / view.rowH);
    const last = Math.ceil((on.mount.scrollTop + on.mount.clientHeight) / view.rowH);
    // A band that moves draws the scopes itself.
    if (last <= view.to - MARGIN && (view.from === 0 || first >= view.from + MARGIN)) return drawScopes();
    const keep = on.mount.scrollTop;
    band(Math.max(0, first - Math.floor(BAND / 3)), view.spot);
    on.mount.scrollTop = keep;
  });

  /** Render lines `[from, from + BAND)` with spacers standing in for the rest.
   *
   *  The spacers' height is the *measured* height of a real row, taken once per
   *  file: a height derived from the CSS goes wrong the moment the font-size
   *  setting moves, and that setting is a slider in this app.
   *
   *  @param {number} from
   *  @param {Spot} spot */
  function band(from, spot) {
    const file = view.file;
    if (!file || held()) return;
    const total = rowCount();
    const to = Math.min(total, from + BAND);
    view.from = from;
    view.to = to;

    const rows = el('div', 'fnrows' + (blameOn ? ' blamed' : ''));
    rows.style.setProperty('--iu', `${step}ch`);
    const firstLine = lineAt(from);
    const lastLine = to > from ? lineAt(to - 1) + 1 : firstLine;
    if (blameOn && !blame.has(firstLine + 1)) void askBlame(firstLine, lastLine);
    for (let r = from; r < to; r++) {
      const row = drawRow(lineAt(r), spot, r === from ? -1 : lineAt(r - 1));
      rows.appendChild(row);
    }

    const top = el('div', 'fnpad');
    const bot = el('div', 'fnpad');
    const stuck = el('div', 'fnstick');
    on.mount.replaceChildren(ruler(total), stuck, top, rows, bot);
    if (!view.rowH) {
      const one = rows.firstElementChild;
      view.rowH = one ? one.getBoundingClientRect().height : 0;
    }
    top.style.height = `${from * view.rowH}px`;
    bot.style.height = `${Math.max(0, total - to) * view.rowH}px`;
    drawScopes();
  }

  /** One line as a row: blame, number, fold marker, code.
   *
   *  `above` is the file line drawn in the row above, which a blame run is
   *  compared with — not `i - 1`, which a closed fold may hide. -1 where there is
   *  no row above, and a run is then labelled even when it began higher up.
   *
   *  @param {number} i the file line, 0-based
   *  @param {Spot} spot
   *  @param {number} above */
  function drawRow(i, spot, above) {
    const file = /** @type {Loaded} */ (view.file);
    const text = file.lines[i] ?? '';
    /* A range lights every row in it. `overlay.service.ts:124-129` is what an
       agent writes when it means a block, and marking only the first line would
       answer a question nobody asked. */
    const n = i + 1;
    const lit = n === spot.line || (!!spot.last && n > spot.line && n <= spot.last);
    const fold = foldAt.get(n);
    const row = el('div', 'fnrow' + (lit ? ' on' : '') + (fold ? ' fh' : '') + (closed.has(n) ? ' folded' : ''));
    // Drawn, not written: a `user-select:none` gutter is still taken by a
    // selection that crosses it, so the numbers would ride along into every
    // copied snippet. Generated content is not in the document to be taken.
    if (blameOn) {
      /* Drawn like the number, so a copied snippet does not carry it. Only
         the first line of a run from one commit is labelled: the same name
         down forty lines is noise, and the gap is what shows where it ends. */
      const b = blame.get(n);
      const g = el('b', 'blame');
      if (b) {
        // The row drawn above, which is not `n - 1` when a fold sits between.
        const prev = above < 0 ? undefined : blame.get(above + 1);
        const who = b.commit ? (b.author.split(' ')[0] || '?') : 'you';
        if (above < 0 || prev?.commit !== b.commit) g.dataset.b = b.commit ? `${who} · ${age(b.time)}` : who;
        g.title = b.commit
          ? `${b.commit} ${b.author}, ${new Date(b.time * 1000).toLocaleDateString()}\n${b.summary}`
          : 'not committed yet';
      }
      row.appendChild(g);
    }
    const num = el('i');
    num.dataset.n = String(n);
    if (fold) num.title = closed.has(n) ? `unfold ${fold.last - fold.line} lines` : `fold ${fold.last - fold.line} lines`;
    row.appendChild(num);
    const body = el('s');
    // What a closed fold hides, drawn after the line rather than written into it,
    // so a copy of the rows does not carry it either.
    if (fold && closed.has(n)) body.dataset.more = `+${fold.last - fold.line}`;
    // The indent guides: a repeating line every step, as wide as the indent. A
    // background rather than elements, so it costs no DOM.
    const ind = indents[i] ?? 0;
    if (ind > 0) body.style.setProperty('--ind', `${ind}ch`);
    const ranges = hlTokens(text, file.lang);
    if (n === spot.line && spot.len) {
      // The match, marked through the same range machinery the word-diff paints
      // with. Byte offsets from Rust, so they are converted rather than assumed.
      const [span] = charRanges(text, [[spot.col, spot.col + spot.len]]);
      if (span) ranges.push({ ...span, cls: 'find' });
      ranges.sort((a, b) => a.s - b.s || a.e - b.e);
    }
    paintRanges(body, text || ' ', dedupe(ranges));
    row.appendChild(body);
    return row;
  }

  /** The scope lines pinned at the top, and the breadcrumbs, for where the pane
   *  is scrolled to. Rebuilt on every scroll, which is cheap: a handful of rows
   *  and a filter over the outline. */
  let scopesKey = '';
  function drawScopes() {
    const file = view.file;
    const stuck = on.mount.querySelector(':scope > .fnstick');
    if (!file || !view.rowH) return;
    const first = Math.floor(on.mount.scrollTop / view.rowH);
    /* Asked twice, because the pinned rows cover the lines under them: the
       scopes of the top line say how many rows to pin, and the line below those
       rows is the one whose scopes they must name. */
    const covered = scopesAt(lineAt(first) + 1).length;
    const below = lineAt(Math.min(first + Math.min(covered, STICKY), Math.max(0, rowCount() - 1))) + 1;
    const scopes = scopesAt(below);
    /* **Nothing is touched when the scopes have not changed**, which is nearly
       every scroll event: the band exists so that a scroll inside it costs no
       DOM, and rebuilding three rows per event would undo that. A rebuilt band
       has a fresh, empty sticky box, which is what the second half asks. */
    const key = `${scopes.map((s) => s.line).join()}|${blameOn}|${shape.symbols.length}`;
    if (key === scopesKey && stuck?.childElementCount === (scopes.length ? 1 : 0)) return;
    scopesKey = key;
    if (stuck) {
      const box = el('div', 'fnrows' + (blameOn ? ' blamed' : ''));
      box.style.setProperty('--iu', `${step}ch`);
      // The innermost, when there are more: which method you are in is the
      // question, and the breadcrumbs carry the rest.
      for (const s of scopes.slice(-STICKY)) {
        const row = drawRow(s.line - 1, { line: 0, col: 0, len: 0 }, -1);
        row.classList.add('stuck');
        row.onclick = () => paint({ line: s.line, col: 0, len: 0 });
        box.appendChild(row);
      }
      stuck.replaceChildren(...(scopes.length ? [box] : []));
    }
    if (on.crumbs) {
      on.crumbs.textContent = scopes.length ? scopes.map((s) => s.name).join(' › ') : 'outline';
      on.crumbs.hidden = !shape.symbols.length;
    }
  }

  /** Breadcrumbs belong to lines, so a page, a picture or nothing has none. */
  const noCrumbs = () => {
    if (on.crumbs) on.crumbs.hidden = true;
  };

  /** Open or close the fold whose marker is on line `n`, keeping that line
   *  where it is on screen: nothing above it moves, so the scroll does not. */
  function toggle(/** @type {number} */ n) {
    if (!foldAt.has(n) || !view.spot) return;
    if (closed.has(n)) closed.delete(n);
    else closed.add(n);
    refold();
    const keep = on.mount.scrollTop;
    band(Math.min(view.from, Math.max(0, rowCount() - 1)), view.spot);
    on.mount.scrollTop = keep;
  }

  /** Close every fold, or open every one. */
  function foldAll(/** @type {boolean} */ shut) {
    closed = shut ? new Set(foldAt.keys()) : new Set();
    refold();
    if (view.spot) paint(view.spot);
  }

  /* A click on a fold's marker toggles it. On the number rather than the code,
     so a click meant to select text never folds anything. */
  on.mount.addEventListener('click', (ev) => {
    const num = /** @type {HTMLElement | null} */ (/** @type {HTMLElement} */ (ev.target).closest?.('.fnrow.fh:not(.stuck) > i'));
    if (num?.dataset.n) toggle(Number(num.dataset.n));
  });

  /* The breadcrumbs open the outline, each symbol a jump, indented by depth. */
  on.crumbs?.addEventListener('click', (ev) => {
    if (!shape.symbols.length) return;
    openMenu(ev, shape.symbols.slice(0, 400).map((s) => [
      `${'  '.repeat(s.depth)}${s.name}  ${s.kind}`, null, () => paint({ line: s.line, col: 0, len: 0 }),
    ]));
  });

  /** Draw the loaded file around `spot`, and say where that is.
   *
   *  @param {Spot} spot */
  function paint(spot) {
    const file = view.file;
    if (!file || held()) return;
    view.mode = 'source';
    view.spot = spot;
    const total = file.lines.length;
    if (on.path) on.path.textContent = file.path;
    if (on.where) {
      on.where.textContent = `${spot.line} of ${total}`
        + (file.plain ? ' · too large to colour' : file.lang ? ` · ${file.lang}` : '');
    }
    /* **Clamped from above as well as below.** A line past the end of the file
       put `from` past the end too, so the band was empty and the pane went blank
       under a header that still said "N of 743". The numbers are real: a file
       that shrank since an agent printed the reference, or a `file.ext:<number>`
       that was never a line at all — a byte count, a pid, a timestamp. */
    const last = Math.max(1, total);
    const at = Math.min(Math.max(spot.line, 1), last);
    // A jump into a fold opens it: the line you were sent to is never hidden.
    reveal(at, Math.max(at, Math.min(spot.last || at, last)));
    const rows = Math.max(1, rowCount());
    band(Math.max(0, Math.min(rowOf(at - 1) - Math.floor(BAND / 2), Math.max(0, rows - BAND))), spot);
    // The line, in the middle of the viewport rather than at its edge.
    on.mount.querySelector('.fnrow.on')?.scrollIntoView({ block: 'center' });
  }

  /** Draw the loaded file as markdown rather than as lines. */
  function paintAsMarkdown() {
    const file = view.file;
    if (!file) return;
    view.mode = 'markdown';
    noCrumbs();
    if (on.path) on.path.textContent = file.path;
    if (on.where) on.where.textContent = `${file.lines.length} lines · rendered`;
    const page = el('div', 'md');
    page.appendChild(paintMarkdown(parse(file.lines.join('\n'))));
    on.mount.replaceChildren(page);
    on.mount.scrollTop = 0;
  }

  /** Run the loaded page in a sandboxed frame.
   *
   *  **`allow-scripts` and never `allow-same-origin`.** Together they would let
   *  the page's scripts take the sandbox off, and without the second one they run
   *  in an opaque origin that cannot read this page or its token. The frame loads
   *  from the daemon's `/preview/` route rather than `srcdoc`, so relative CSS and
   *  JS resolve; `preview.rs` says what that route will and will not serve. */
  async function paintAsPreview() {
    const file = view.file;
    if (!file || !view.ws) return;
    const mine = view.seq;
    view.mode = 'preview';
    noCrumbs();
    if (on.path) on.path.textContent = file.path;
    if (on.where) on.where.textContent = 'preview · scripts on';
    let token;
    try {
      ({ token } = await call('/api/preview', { workspace: view.ws, path: file.path }));
    } catch (e) {
      if (mine === view.seq) on.mount.replaceChildren(el('div', 'fnsay', reason(e)));
      return;
    }
    // Another file, or back to source, while the token was in the air.
    if (mine !== view.seq || view.mode !== 'preview') return;
    const frame = /** @type {HTMLIFrameElement} */ (el('iframe', 'preview'));
    frame.setAttribute('sandbox', 'allow-scripts');
    frame.setAttribute('referrerpolicy', 'no-referrer');
    frame.title = file.path;
    frame.src = `${activeCheckout().base}/preview/${token}/`
      + file.path.split('/').map(encodeURIComponent).join('/');
    on.mount.replaceChildren(frame);
  }

  /** Show an image, fetched by the `<img>` itself from `/api/file/image`.
   *
   *  **Not through `/api/file`**, which refuses a binary file on purpose rather
   *  than mangle it into a string. `preview.rs` has the route and why it serves
   *  pictures and nothing else.
   *
   *  @param {string} ws @param {string} path */
  function paintImage(ws, path) {
    paintImageFrom(path, `${activeCheckout().base}/api/file/image?workspace=${encodeURIComponent(ws)}`
      + `&path=${encodeURIComponent(path)}`);
  }

  /** Show an image from any route that serves one, under `label`. A scratchpad
   *  image is not in a workspace, so it has a route of its own; everything a
   *  picture needs on screen is the same.
   *
   *  @param {string} label @param {string} src */
  function paintImageFrom(label, src) {
    const path = label;
    const mine = ++view.seq;
    view.file = null;
    view.spot = null;
    view.mode = 'image';
    noCrumbs();
    if (on.path) on.path.textContent = path;
    if (on.where) on.where.textContent = 'image';
    const img = /** @type {HTMLImageElement} */ (el('img', 'fvimage'));
    img.alt = path;
    img.onload = () => {
      if (mine === view.seq && on.where) on.where.textContent = `${img.naturalWidth} × ${img.naturalHeight}`;
    };
    // An empty pane is indistinguishable from a broken one, for the reason `show` gives.
    img.onerror = () => {
      if (mine === view.seq) on.mount.replaceChildren(el('div', 'fnsay', `${path} could not be shown as an image`));
    };
    img.src = src;
    const box = el('div', 'fvimagebox');
    box.appendChild(img);
    on.mount.replaceChildren(box);
    on.mount.scrollTop = 0;
  }

  /** What the file on screen can be rendered as, if anything. */
  const kind = () => {
    const path = view.file?.path ?? '';
    if (/\.(md|markdown)$/i.test(path)) return 'markdown';
    // A preview runs on a workspace's token, and a scratchpad file has none.
    if (/\.html?$/i.test(path) && view.ws) return 'html';
    return null;
  };

  return {
    /** Whether the file on screen is one this can render. */
    renderable: () => kind() != null,

    /** `'markdown'`, `'html'` or `null`, for the button that says which. */
    kind,

    /** Show an image from its own route; see [`paintImageFrom`]. */
    image: paintImageFrom,

    /** Show text that is in no workspace, a scratchpad file, under `label`.
     *  With no workspace there is no ruler and no blame: `askChanges` and
     *  `askBlame` both ask git about a workspace path, and stop on a null `ws`.
     *
     *  @param {string} label @param {string} content */
    showText(label, content) {
      ++view.seq;
      view.ws = null;
      view.readOnly = false;
      load(label, content);
      paint({ line: 1, last: 0, col: 0, len: 0 });
      void askStructure();
    },

    /** Whether a picture is on screen, which has no source to edit. */
    isImage: () => view.mode === 'image',

    /** Whether the file on screen may be shown and not written. */
    readOnly: () => view.readOnly,

    /** Draw what is loaded as a page. The caller decides when: a line number is
     *  a reason to show the source, and no line number is a reason not to. */
    render: () => (kind() === 'html' ? void paintAsPreview() : paintAsMarkdown()),

    /** Draw what is loaded as lines again, at the spot it was opened on. */
    renderSource: () => {
      if (view.spot) paint(view.spot);
    },

    /** The line in the middle of the pane, for a caller that hands the file to
     *  the editor and wants it to open where you were looking. Null when no
     *  lines are on screen. */
    middleLine: () => {
      if (!view.file || view.mode !== 'source' || !view.rowH) return null;
      const r = Math.floor((on.mount.scrollTop + on.mount.clientHeight / 2) / view.rowH);
      return lineAt(Math.min(Math.max(0, r), Math.max(0, rowCount() - 1))) + 1;
    },

    /** The lines a search hit in the file on screen, for the ruler. */
    setHits: (/** @type {number[]} */ lines) => {
      hits = lines;
      redrawRuler();
    },

    /** The file on screen as one string, for a search made in the page. Null
     *  when there is nothing to search: a picture, or nothing loaded yet. */
    text: () => view.file?.lines.join('\n') ?? null,

    /** Put a match in the middle and mark it.
     *
     *  `col` and `len` are **characters**, which is what a search over the
     *  string answers in; the spot a band paints from is in bytes, because every
     *  other filler of it is Rust. The conversion is here so that one caller
     *  cannot forget it.
     *
     *  @param {number} line @param {number} col @param {number} len */
    seek: (line, col, len) => {
      if (!view.file || view.mode === 'image') return false;
      const [s, e] = byteRange(view.file.lines[line - 1] ?? '', col, col + len);
      paint({ line, col: s, len: e - s });
      return true;
    },

    /** Put line `line` in the middle, as lines. False when there is no text to
     *  move through: nothing loaded, or a picture. */
    goTo: (/** @type {number} */ line) => {
      if (!view.file || view.mode === 'image') return false;
      paint({ line, col: 0, len: 0 });
      return true;
    },

    /** Show `path` at `spot`, fetching it unless it is already the one on screen.
     *
     *  A refusal — binary, too large, deleted underneath you — is a sentence in
     *  the pane rather than an empty one, because an empty viewer is
     *  indistinguishable from a broken viewer.
     *
     *  @param {string} ws
     *  @param {string} path
     *  @param {Spot} spot */
    async show(ws, path, spot) {
      view.ws = ws;
      if (IMAGE.test(path)) {
        paintImage(ws, path);
        return true;
      }
      if (view.file?.path !== path) {
        const mine = ++view.seq;
        let answer;
        try {
          answer = await get(
            `/api/file?workspace=${encodeURIComponent(ws)}&path=${encodeURIComponent(path)}`);
        } catch (e) {
          /* **The generation is checked here too, not only on the way out.** A
             slow failing read for one file used to paint its error over another
             file that had already arrived — two hits stepped through quickly, or
             two terminal clicks. A refusal about a file nobody is looking at any
             more is not an answer, it is noise in the place the answer was. */
          if (mine !== view.seq) return false;
          view.file = null;
          view.readOnly = false;
          noCrumbs();
          if (on.path) on.path.textContent = path;
          on.mount.replaceChildren(el('div', 'fnsay', reason(e)));
          return false;
        }
        // A newer `show` has been asked for while this one was in the air.
        if (mine !== view.seq) return false;
        view.readOnly = answer.read_only === true;
        load(path, String(answer.content ?? ''));
        void askChanges();
        void askStructure();
      }
      paint(spot);
      return true;
    },

    /** Forget what is loaded, so the next `show` reads the file again. Called
     *  after an edit: a discarded buffer must not leave a stale copy behind it. */
    drop() {
      view.file = null;
      view.spot = null;
    },

    /** Empty the pane, and the header with it. */
    clear() {
      view.file = null;
      view.spot = null;
      if (on.path) on.path.textContent = '';
      if (on.where) on.where.textContent = '';
      noCrumbs();
      on.mount.replaceChildren();
    },

    /** The file on screen, for a caller that needs to say which file a click
     *  happened in. */
    shownPath: () => view.file?.path ?? null,

  };
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


/** A markdown tree as nodes, ready to mount.
 *
 *  **The painting is here rather than beside the parser** because the parser has
 *  no imports on purpose — a node script drives it — and this is where the app
 *  already turns text into elements.
 *
 *  @param {import('./markdown.js').Block[]} blocks
 *  @returns {DocumentFragment} */
export function paintMarkdown(blocks) {
  const frag = document.createDocumentFragment();
  for (const b of blocks) {
    if (b.kind === 'heading') {
      /* **Both the tag and the class are written out.** `el` is typed against
         the tag names the DOM knows, and a computed `h${n}` is not one of them —
         and a computed *class* is one `check-dead-css` cannot see, so it reads
         the rule for it as dead and refuses the stylesheet. Six literals answer
         both. */
      const LEVELS = /** @type {const} */ ([
        ['h1', 'md-h md-h1'], ['h2', 'md-h md-h2'], ['h3', 'md-h md-h3'],
        ['h4', 'md-h md-h4'], ['h5', 'md-h md-h5'], ['h6', 'md-h md-h6'],
      ]);
      const [tag, cls] = LEVELS[Math.min(Math.max(b.level, 1), 6) - 1] ?? LEVELS[5];
      frag.appendChild(inline(el(tag, cls), b.text));
    } else if (b.kind === 'code') {
      /* Coloured through the same tokeniser the file viewer uses, so a fenced
         `rust` block in a note reads like the file it was copied from. The info
         string is a language name rather than a path, so it is mapped through a
         stand-in file name. */
      const pre = el('pre', 'md-code');
      const lang = b.lang ? langFor(`x.${b.lang}`) : null;
      paintRanges(pre, b.text, hlTokens(b.text, lang));
      frag.appendChild(pre);
    } else if (b.kind === 'quote') {
      const q = el('blockquote', 'md-quote');
      /* `append(...)` rather than a loop over `childNodes`: that list is live, so
         appending each node removes it from the collection being walked and the
         index-based iterator then skips every other child. A quote of two
         paragraphs lost one of them, in silence. */
      q.append(...paintMarkdown(parse(b.lines.join('\n'))).childNodes);
      frag.appendChild(q);
    } else if (b.kind === 'rule') {
      frag.appendChild(el('hr', 'md-rule'));
    } else if (b.kind === 'list') {
      frag.appendChild(list(b));
    } else if (b.kind === 'table') {
      frag.appendChild(table(b));
    } else {
      frag.appendChild(inline(el('p', 'md-p'), b.text));
    }
  }
  return frag;
}

/** A list, nested by the depth each item carries. */
function list(/** @type {{ ordered: boolean, items: { text: string, depth: number }[] }} */ b) {
  const root = el(b.ordered ? 'ol' : 'ul', 'md-list');
  /** @type {HTMLElement[]} */
  const open = [root];
  for (const item of b.items) {
    /* `open.length`, not `open.length - 1`: clamping to the latter made `want`
       at most `open.length`, so the branch that opens a nested list below could
       never run and every indented item rendered flat. One level per item, which
       is also what stops a jump from depth 0 to depth 3 building three lists
       nobody asked for. */
    const want = Math.min(item.depth, open.length) + 1;
    while (open.length > want) open.pop();
    while (open.length < want) {
      const nested = el(b.ordered ? 'ol' : 'ul', 'md-list');
      (open[open.length - 1]?.lastElementChild ?? open[open.length - 1])?.appendChild(nested);
      open.push(nested);
    }
    open[open.length - 1]?.appendChild(inline(el('li', 'md-li'), item.text));
  }
  return root;
}

function table(/** @type {{ head: string[], rows: string[][] }} */ b) {
  const t = el('table', 'md-table');
  const head = el('tr', 'md-tr');
  for (const c of b.head) head.appendChild(inline(el('th', 'md-th'), c));
  t.appendChild(head);
  for (const row of b.rows) {
    const tr = el('tr', 'md-tr');
    for (const c of row) tr.appendChild(inline(el('td', 'md-td'), c));
    t.appendChild(tr);
  }
  return t;
}

/** Plain text, with any ref in it as a link the click handler above opens.
 *
 *  Only plain text: a ref inside a code span is code somebody quoted, and inside
 *  a link it is already that link's. The story prefix is the active checkout's,
 *  the same daemon `/api/open/ref` goes to.
 *
 *  @param {HTMLElement} into
 *  @param {string} text */
function plain(into, text) {
  let at = 0;
  for (const r of refsIn(text, snap.story_prefix)) {
    if (r.start > at) into.appendChild(document.createTextNode(text.slice(at, r.start)));
    const a = el('a', 'md-link md-ref', r.ref);
    a.dataset.ref = r.ref;
    a.title = `open ${r.ref}`;
    into.appendChild(a);
    at = r.end;
  }
  if (at < text.length) into.appendChild(document.createTextNode(text.slice(at)));
}

/** A markdown link that names a file rather than a place on the web: no scheme,
 *  not protocol-relative, and not only a fragment. */
const FILE_LINK = /^(?![a-z][a-z0-9+.-]*:|\/\/|#)./i;

/** Inline markup, appended to `into` as nodes.
 *
 *  One pass, longest marker first, because `**bold**` has to win over the `*` of
 *  emphasis. A marker that never closes is text, which is the reading that never
 *  eats the rest of a paragraph.
 *
 *  @param {HTMLElement} into
 *  @param {string} text */
function inline(into, text) {
  const pattern = /`([^`]+)`|\*\*([^*]+)\*\*|__([^_]+)__|\*([^*]+)\*|_([^_]+)_|!?\[([^\]]*)\]\(([^)\s]+)[^)]*\)/;
  let rest = text;
  for (;;) {
    const m = pattern.exec(rest);
    if (!m) break;
    if (m.index) plain(into, rest.slice(0, m.index));
    const [whole, code, strongA, strongB, emA, emB, label, href] = m;
    if (code !== undefined) {
      into.appendChild(el('code', 'md-tick', code));
    } else if (strongA ?? strongB) {
      into.appendChild(el('strong', null, strongA ?? strongB ?? ''));
    } else if (emA ?? emB) {
      into.appendChild(el('em', null, emA ?? emB ?? ''));
    } else if (href !== undefined && FILE_LINK.test(href)) {
      /* **A link to another file in the repo**, which is most links in a repo's
         own docs. It used to go through `safeHref` like a URL, which resolved it
         against this page's address and opened the daemon's own page in a new
         window. Drawn as a link that navigates nowhere; the viewer that painted
         it opens the file (`create`'s `onFile`), and where nothing does, it is
         inert rather than wrong. */
      const a = el('a', 'md-link md-file', label || href);
      a.dataset.file = href;
      a.title = `open ${href}`;
      into.appendChild(a);
    } else if (href !== undefined) {
      /* Through `safeHref`, which is the SPA's one rule for a URL it would
         navigate to: a note is text somebody else wrote, and `javascript:` in it
         must not become a link this page will follow. A link it refuses is drawn
         as its own label, so nothing disappears. */
      const url = safeHref(href);
      if (url) {
        const a = el('a', 'md-link', label || href);
        a.setAttribute('href', url);
        a.setAttribute('target', '_blank');
        a.setAttribute('rel', 'noreferrer');
        into.appendChild(a);
      } else {
        into.appendChild(document.createTextNode(label || href));
      }
    }
    rest = rest.slice(m.index + (whole ?? '').length);
  }
  if (rest) plain(into, rest);
  return into;
}
