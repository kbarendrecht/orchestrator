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

import { activeCheckout, call, copyText, el, get, openMenu, reason, safeHref, toast } from './core.js';
import { byteRange, charRanges, hlTokens, langFor, paintRanges } from './source.js';
import { parse } from './markdown.js';

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
const IMAGE = /\.(png|jpe?g|gif|webp|avif|svg|ico|bmp)$/i;

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
 *  @param {{ mount: HTMLElement, path?: HTMLElement | null, where?: HTMLElement | null,
 *            onFile?: (rel: string, line: number) => void }} on
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
   *  @type {{ file: Loaded | null, spot: Spot | null, from: number, to: number,
   *           rowH: number, seq: number, mode: 'source' | 'markdown' | 'preview' | 'image',
   *           ws: string | null }} */
  const view = { file: null, spot: null, from: 0, to: 0, rowH: 0, seq: 0, mode: 'source', ws: null };
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

  /** Swap the ruler alone when its marks arrive. **Not the band**: a rebuilt
   *  band can move the pane's scroll, and a scroll closes an open menu, so the
   *  marks landing a moment after a right-click took the menu away with them. */
  function redrawRuler() {
    const old = on.mount.querySelector(':scope > .ruler');
    if (old && view.file && view.mode === 'source') old.replaceWith(ruler(view.file.lines.length));
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
      m.style.top = `${((line - 1) / total) * 100}%`;
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
    const a = /** @type {HTMLElement | null} */ (/** @type {HTMLElement} */ (ev.target).closest?.('a.md-file'));
    if (!a || !on.onFile || !view.file) return;
    ev.preventDefault();
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
    if (n) items.push([`copy ${path}:${n}`, null, copy(`${path}:${n}`)]);
    items.push(['copy path', null, copy(path)]);
    if (view.mode === 'source') items.push([blameOn ? 'hide blame' : 'show blame', null, toggleBlame]);
    openMenu(ev, items);
  });

  /** Keep the band under the viewport as it is scrolled. */
  on.mount.addEventListener('scroll', () => {
    // Nothing to keep under the viewport when the viewport holds a document.
    if (view.mode !== 'source') return;
    if (!view.file || !view.rowH || !view.spot) return;
    const first = Math.floor(on.mount.scrollTop / view.rowH);
    const last = Math.ceil((on.mount.scrollTop + on.mount.clientHeight) / view.rowH);
    if (first >= view.from + MARGIN && last <= view.to - MARGIN) return;
    if (view.from === 0 && last <= view.to - MARGIN) return;
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
    if (!file) return;
    const total = file.lines.length;
    const to = Math.min(total, from + BAND);
    view.from = from;
    view.to = to;

    const rows = el('div', 'fnrows' + (blameOn ? ' blamed' : ''));
    if (blameOn && !blame.has(from + 1)) void askBlame(from, to);
    for (let i = from; i < to; i++) {
      const text = file.lines[i] ?? '';
      /* A range lights every row in it. `overlay.service.ts:124-129` is what an
         agent writes when it means a block, and marking only the first line would
         answer a question nobody asked. */
      const n = i + 1;
      const lit = n === spot.line || (!!spot.last && n > spot.line && n <= spot.last);
      const row = el('div', 'fnrow' + (lit ? ' on' : ''));
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
          const prev = blame.get(n - 1);
          const who = b.commit ? (b.author.split(' ')[0] || '?') : 'you';
          if (i === from || prev?.commit !== b.commit) g.dataset.b = b.commit ? `${who} · ${age(b.time)}` : who;
          g.title = b.commit
            ? `${b.commit} ${b.author}, ${new Date(b.time * 1000).toLocaleDateString()}\n${b.summary}`
            : 'not committed yet';
        }
        row.appendChild(g);
      }
      const num = el('i');
      num.dataset.n = String(n);
      row.appendChild(num);
      const body = el('s');
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
      rows.appendChild(row);
    }

    const top = el('div', 'fnpad');
    const bot = el('div', 'fnpad');
    on.mount.replaceChildren(ruler(total), top, rows, bot);
    if (!view.rowH) {
      const one = rows.firstElementChild;
      view.rowH = one ? one.getBoundingClientRect().height : 0;
    }
    top.style.height = `${from * view.rowH}px`;
    bot.style.height = `${Math.max(0, total - to) * view.rowH}px`;
  }

  /** Draw the loaded file around `spot`, and say where that is.
   *
   *  @param {Spot} spot */
  function paint(spot) {
    const file = view.file;
    if (!file) return;
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
    band(Math.max(0, Math.min(at - 1 - Math.floor(BAND / 2), Math.max(0, last - BAND))), spot);
    // The line, in the middle of the viewport rather than at its edge.
    on.mount.querySelector('.fnrow.on')?.scrollIntoView({ block: 'center' });
  }

  /** Draw the loaded file as markdown rather than as lines. */
  function paintAsMarkdown() {
    const file = view.file;
    if (!file) return;
    view.mode = 'markdown';
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
    if (/\.html?$/i.test(path)) return 'html';
    return null;
  };

  return {
    /** Whether the file on screen is one this can render. */
    renderable: () => kind() != null,

    /** `'markdown'`, `'html'` or `null`, for the button that says which. */
    kind,

    /** Show an image from its own route; see [`paintImageFrom`]. */
    image: paintImageFrom,

    /** Whether a picture is on screen, which has no source to edit. */
    isImage: () => view.mode === 'image',

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
      const n = Math.floor((on.mount.scrollTop + on.mount.clientHeight / 2) / view.rowH) + 1;
      return Math.min(Math.max(1, n), view.file.lines.length);
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
          if (on.path) on.path.textContent = path;
          on.mount.replaceChildren(el('div', 'fnsay', reason(e)));
          return false;
        }
        // A newer `show` has been asked for while this one was in the air.
        if (mine !== view.seq) return false;
        const plain = (answer.content?.length ?? 0) > HUGE;
        view.file = {
          path,
          lines: String(answer.content ?? '').split('\n'),
          lang: plain ? null : langFor(path),
          plain,
        };
        view.rowH = 0;
        blame = new Map();
        blameAsked = new Set();
        changed = [];
        hits = [];
        void askChanges();
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
    if (m.index) into.appendChild(document.createTextNode(rest.slice(0, m.index)));
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
  if (rest) into.appendChild(document.createTextNode(rest));
  return into;
}
