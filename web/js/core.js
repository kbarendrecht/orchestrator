// The primitives every part of the SPA needs: the daemon's token, the two fetch
// wrappers, the DOM shorthands, and the snapshot itself.
//
// Extracted first because a module can only import from another module — a leaf
// like the review queue cannot be pulled out until the trunk it reaches for is
// importable. Everything here was already shared; the difference is that reaching
// for it now has to be written down.

/** @type {import("../snapshot").Snapshot} */
export let snap = /** @type {any} */ ({ workspaces: [], sessions: [] });

/* When the snapshot the current numbers came from landed. Durations are computed
 * server-side as the snapshot is built, so rendering them raw freezes the clock
 * between events: a session waiting on a permission prompt sat at "0s" until
 * something unrelated pushed a snapshot, then jumped to "1m". The rail redraws
 * every second; this is what makes those seconds mean anything. */
let snapAt = Date.now();

/** Take a new snapshot for one repository: the two have to move together, so
 *  they move here.
 *
 *  `snap` is a live binding — importers see this assignment without re-importing,
 *  which is what lets a hundred readers keep saying `snap.x`.
 *
 *  Every repository's snapshot is kept, because the rail draws all of them; only
 *  the **active** one moves `snap`, because that is what every other pane means
 *  by it. A snapshot for a repository you are not looking at must not silently
 *  re-point the centre pane at another checkout's sessions. */
export function receive(next, repoId = activeRepo.id) {
  const repo = repoById(repoId);
  repo.snap = next;
  repo.snapAt = Date.now();
  repo.live = true;
  if (repo === activeRepo) {
    snap = next;
    snapAt = repo.snapAt;
  }
}

export const sinceSnap = (ms) => (ms == null ? null : ms + (Date.now() - snapAt));

/** The PR whose head ref this workspace holds, if any. */
export function prForWorkspace(wsId, s = snap) {
  return (s.prs || []).find((p) => p.workspace === wsId) || null;
}

/* Which session the centre pane is showing. Owned here because the rail picks it
 * and the terminals and the render both react — leaving the state in `app.js`
 * meant the rail had to reach back into the module that renders it. */
export let selected = null;

const selectionListeners = [];
export function onSelection(fn) { selectionListeners.push(fn); }

/** Pick a session. What *happens* next is whoever registered's business.
 *
 *  `auto` marks the pick the app made for you, which is what the snapshot does
 *  when the session you were on ends. A listener that reads that as a gesture is
 *  reacting to a session finishing, so anything standing down on "you went
 *  somewhere else" has to be able to tell the two apart. */
export function setSelected(id, auto = false) {
  selected = id;
  for (const fn of selectionListeners) fn(id, auto);
}

export const TOKEN = window.__ORCH__.token;
export const WS_BASE = `ws://${location.host}`;

/** Whether the daemon is running on macOS. Told, not sniffed.
 *
 *  Declared up here rather than beside `MOD_LABEL` and `CHROME`, where it used to
 *  sit: `FONTS` reads it at module scope to label the system face, and a `const`
 *  read before its declaration is a temporal dead zone — a runtime crash on load
 *  that `tsc` does not see. */
export const IS_MAC = window.__ORCH__.platform === 'mac';

/** Whether the window was created see-through (`config.window_transparent`).
 *
 *  Fixed at window creation, so this is told at boot and never changes while the
 *  page lives — which is exactly why the *opacity* is a separate, live theme value
 *  and this is not. */
export const TRANSPARENT = window.__ORCH__.transparent === '1';

/* ---------------------------------------------------------------------------
 * Repositories
 * ------------------------------------------------------------------------- */

/* **One daemon per repository, one connection per daemon.** Working two
 * checkouts at once is two whole daemons (`src/peers.rs`) — the way two terminal
 * tabs are two whole shells — so this holds one entry per repository and the
 * board composes them. The one serving this page is first and is the only one
 * addressed relatively; a peer is addressed absolutely, on its own loopback port
 * with its own token.
 *
 * **The rail is the only pane that shows more than one.** Everything else —
 * the centre pane, the changed files, the diff, the review overlay, the queue,
 * the settings — describes *the session you are in*, so it follows the repository
 * that session belongs to. That is what makes `snap` still mean something, and
 * why the 58 places that say `call('/api/…')` did not have to learn about this:
 * they act on the active repository, which is the one you are looking at.
 *
 * `checkouts` is `[]` on the review-preview page, which builds its own
 * `__ORCH__`, and on any single-repository daemon — see `lone`.
 *
 * Named `checkouts` because `snap.repos` is already the GitHub pair this checkout
 * pushes to; `repo` as a local variable still reads as one of these.
 */
/** A blank snapshot: what a repository looks like before its first arrives.
 *
 *  The two lists every reader indexes, so the rail draws an empty repository
 *  rather than throwing on the first frame. Deliberately not a shared object —
 *  each repository gets its own, or `receive` on one would appear on all. */
const empty = () => /** @type {any} */ ({ workspaces: [], sessions: [] });

/** The page's own daemon, when no shell attached a list.
 *
 *  This is the single-repository install — every one that existed before this
 *  feature — so it must not be a special case downstream: one entry, same shape,
 *  and `multiRepo()` is false. The name is null because the daemon serving this
 *  page has no reason to tell it which folder it is; nothing draws a repository
 *  label when there is only one. */
const lone = () => [{
  id: 'local',
  name: null,
  colour: null,
  origin: '',
  ws: WS_BASE,
  token: TOKEN,
  snap: empty(),
  snapAt: Date.now(),
  live: false,
}];

export const checkouts = (window.__ORCH__.checkouts || []).length
  ? window.__ORCH__.checkouts.map((r) => {
      /* **The local one is the one on this page's own port.** Marked by port
         rather than by being first, because "first" is a property of how the
         shell happened to build the list and this has to stay true if that ever
         changes. Same-origin matters: an empty origin keeps every existing
         relative path working and keeps the calls out of CORS entirely. */
      const local = String(r.port) === (location.port || '80');
      return {
        id: r.id,
        name: r.name,
        colour: r.colour,
        origin: local ? '' : `http://127.0.0.1:${r.port}`,
        /* `127.0.0.1` and not `localhost`, matching the origin the daemon was
           told to expect: a websocket upgrade goes through the same Origin check
           as a POST, and the two spellings are two origins. */
        ws: local ? WS_BASE : `ws://127.0.0.1:${r.port}`,
        token: local ? TOKEN : r.token,
        snap: empty(),
        snapAt: Date.now(),
        live: false,
      };
    })
  : lone();

export const repoById = (id) => checkouts.find((r) => r.id === id) || checkouts[0];

/** The repository whose daemon served this page — the one holding the shell.
 *
 *  Identified by having no origin of its own (see the registry above), which is
 *  the same thing as being same-origin with this document. */
export const localRepo = checkouts.find((r) => !r.origin) || checkouts[0];

/** Routes that belong to the **app**, not to a repository.
 *
 *  **A secondary daemon has no Tauri handle**, because `attach_window` is only
 *  ever called on the one that opened the window ([`crate::peers`]). So a window
 *  command sent to a peer is refused with "no native window attached" — which is
 *  exactly what happened: `call` follows the *active* repository, so selecting a
 *  session in another checkout quietly moved the titlebar's buttons, the resize
 *  edges and Restart onto a daemon that cannot drive a window. Reported as an
 *  occasional toast, because it only shows up once you are working in a peer.
 *
 *  Prefix-matched rather than listed exactly: `/api/window/` has a command and a
 *  resize edge under it, and a new one must not have to be remembered here. */
const SHELL_ROUTES = ['/api/window/', '/api/open', '/api/client/timing', '/api/update/'];
const isShellRoute = (path) => SHELL_ROUTES.some((r) => path.startsWith(r));

/** Whether this window is showing more than one repository at all.
 *
 *  Read wherever the single-repository shape has to keep looking exactly as it
 *  did: no colour strips, no repository headers, no drag handles. Every install
 *  before this feature is this case, so it is the normal one. */
export const multiRepo = () => checkouts.length > 1;

/** The repository whose session the panes are describing. */
export let activeRepo = checkouts[0];

const repoListeners = [];
export function onActiveRepo(fn) { repoListeners.push(fn); }

/** Point the panes at another repository.
 *
 *  Re-points `snap` with it, which is the whole trick: a hundred readers keep
 *  saying `snap.x` and get the repository they are looking at, because `snap` is
 *  a live binding and this is the only other thing allowed to move it. */
export function setActiveRepo(id) {
  const next = repoById(id);
  if (next === activeRepo) return;
  activeRepo = next;
  snap = next.snap;
  snapAt = next.snapAt;
  for (const fn of repoListeners) fn(next);
}

/* ---------------------------------------------------------------------------
 * Boot timing
 * ------------------------------------------------------------------------- */

/* How long the window took to become a usable board, reported to the daemon so
 * it lands in the log with the daemon's own phases.
 *
 * Here because the client half of a slow start is not measurable from Rust: the
 * daemon can say when it served the page and when it sent the first snapshot,
 * and nothing on that side can say when the vendored scripts finished parsing or
 * when the terminal first painted. Reported rather than logged to the console,
 * because the app people are complaining about runs in a webview with no console
 * anybody is going to open.
 *
 * Measured from `timeOrigin`, so `scripts` includes the page fetch and the three
 * classic vendor scripts (xterm, the fit addon, prism) that block this module. */
const marks = {};

/** Record a boot milestone, the first time it happens.
 *
 *  First only: `attach` and `paint` repeat every time a session is switched, and
 *  a later one is not boot. */
export function mark(what) {
  if (marks[what] == null) marks[what] = Math.round(performance.now());
}
mark('scripts');

let reported = false;
let reportTimer = null;

/** Send the marks once, a moment after the last one that is going to arrive.
 *
 *  Debounced rather than fired on a particular mark, because which mark is last
 *  depends on the board: a cold start with no session never paints a terminal at
 *  all, and waiting for one would mean never reporting on exactly the start that
 *  is worth reporting. */
/** Put one line in the daemon's log, from the page.
 *
 *  For facts a bug report needs and cannot otherwise reach: which renderer a
 *  terminal opened with, which engine it is on, whether a WebGL context was lost.
 *  `orchd.log` is the daemon's own log and a packaged app has no console, so
 *  without this the answer to "which renderer were you on" is a screen recording.
 *
 *  Best effort and never awaited — a log line must not be able to fail anything. */
export function note(text) {
  call('/api/client/note', { note: text }).catch(() => {});
}

export function reportBoot() {
  if (reported) return;
  clearTimeout(reportTimer);
  reportTimer = setTimeout(() => {
    reported = true;
    // Failure is silence. This is a diagnostic, and a toast about it would be
    // the app complaining to the user on the user's behalf.
    callShell('/api/client/timing', { marks }).catch(() => {});
  }, 1500);
}

export const $ = (id) => document.getElementById(id);

/** `$` for a form control, where the caller wants `.value` or `.disabled`.
 *
 *  `getElementById` can only promise `HTMLElement`, so every read of `.value`
 *  through `$` is a type error even when the id certainly names an `<input>`.
 *  Deliberately untyped rather than a union of input/button/select: TypeScript
 *  reduces that intersection to `never`, and a union only offers what all three
 *  share. So this is one named escape hatch for controls — `$` stays typed, and
 *  everything fetched through it keeps being checked. */
export const ctl = (id) => /** @type {any} */ (document.getElementById(id));

export function el(tag, cls, text, title) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  // A truncated label is unreadable past the ellipsis, so pass the full text as
  // `title` and the native tooltip reveals the clipped tail on hover.
  if (title !== undefined) n.title = title;
  return n;
}

/** The chevron a collapsible header rotates — drawn rather than typed so it
 *  matches the gear and refresh and cannot fall out of the font. `1em`, so each
 *  header's own font-size still sets its size, and the `[aria-expanded]` rotate
 *  rule turns the SVG exactly as it turned the glyph. */
export function caret() {
  const s = el('span', 'caretr');
  s.innerHTML = '<svg viewBox="0 0 16 16" width="1em" height="1em" fill="none"'
    + ' stroke="currentColor" stroke-width="1.6" stroke-linecap="round"'
    + ' stroke-linejoin="round" aria-hidden="true"><path d="M6 4l4 4-4 4"/></svg>';
  return s;
}

/** A duration that keeps moving, without the tree being rebuilt to move it.
 *
 *  The base value is kept on the node, so [`tick`] can recompute it against the
 *  same `sinceSnap` clock a second later. That is the whole mechanism, and it
 *  exists because the alternative was calling `renderRail` on a timer: a rebuild
 *  destroys the node under your pointer, `:hover` is not re-targeted until the
 *  mouse moves, and a native `title` tooltip needs the pointer resting on one
 *  element for about half a second — which a 1 Hz rebuild never leaves it.
 *  Rebuilding was never slow (0.46 ms for a 430-node rail); it was simply the
 *  wrong verb for "one more second has passed".
 *
 *  Here rather than in the rail because `tick` was always document-wide, and the
 *  second pane to want a moving duration wrote its own instead: the review
 *  queue's "· 3s ago" was re-rendered by the rebuild this change is removing, so
 *  once the pane stopped rebuilding the clock stopped with it.
 */
export function clock(cls, ms, suffix = '', prefix = '') {
  // An absent base renders empty and is left un-marked. `Number('')` is 0, so a
  // null written into the dataset would come back as a clock counting up from the
  // epoch of nothing — a "0s" that grows where there had been no text at all.
  if (ms == null) return el('span', cls, '');
  /* **The instant it started, not its age when the snapshot was taken.**
     `sinceSnap` measures from the *newest* snapshot's arrival, so a node built
     from an older one reads its own age against a clock that has since been
     reset, and every push that does not rebuild the node walks the number
     backwards: measured at 55s, then 53s five seconds later. Invisible while
     every push rebuilt the rail, and the first thing the render guards exposed.
     An absolute instant does not care how often a snapshot lands. */
  const started = Date.now() - sinceSnap(ms);
  const span = el('span', cls, prefix + duration(Date.now() - started) + suffix);
  span.dataset.clock = String(started);
  if (suffix) span.dataset.clockSuffix = suffix;
  if (prefix) span.dataset.clockPrefix = prefix;
  return span;
}

/** Advance every duration on the page. The timer calls this, not render. */
export function tick() {
  for (const node of document.querySelectorAll('[data-clock]')) {
    const el_ = /** @type {HTMLElement} */ (node);
    const started = Number(el_.dataset.clock);
    if (!Number.isFinite(started)) continue;
    el_.textContent = (el_.dataset.clockPrefix || '')
      + duration(Date.now() - started)
      + (el_.dataset.clockSuffix || '');
  }
}

/* One row per message, stacked newest at the bottom. A receipt fades on its own;
 * an error stays until dismissed, because a refusal names a branch, a pid or a
 * path you may need to copy — and a second error must no longer erase the first
 * the way the single slot did. */
const MAX_TOASTS = 5;
const toastTimers = new WeakMap();

/** Whatever had the keyboard when an error row took it, to give back afterwards. */
let toastReturn = null;

/** In use: the pointer is in the row, or it holds a selection nobody has copied.
 *  7 seconds is not enough to read a refusal, aim at it and drag across it, so
 *  the clock does not run while you are working in the row. */
function toastHeld(row) {
  if (row.matches(':hover')) return true;
  const sel = window.getSelection();
  return !!sel && !sel.isCollapsed && !!sel.anchorNode && row.contains(sel.anchorNode);
}

function dismissToast(row) {
  clearTimeout(toastTimers.get(row));
  toastTimers.delete(row);
  /* Hand the keyboard back to the exact element the row took it from — the
     specific terminal, centre or drawer, not "a" terminal — or the next
     keystroke lands nowhere. */
  if (document.activeElement === row && toastReturn && document.contains(toastReturn)) {
    try {
      toastReturn.focus();
    } catch (e) { /* disposed while the toast was up */ }
    toastReturn = null;
  }
  row.remove();
}

/** A receipt's dismissal clock, restarted while the row is held. An error never
 *  arms one — it stays until the ✕. */
function armToast(row, ms) {
  clearTimeout(toastTimers.get(row));
  // Re-checked on a short beat, not on pointerleave: a selection left alone has
  // to keep the text up too, and there is no event for "still selected".
  toastTimers.set(row, setTimeout(() => {
    if (toastHeld(row)) return armToast(row, 1200);
    dismissToast(row);
  }, ms));
}

export function toast(message, bad) {
  const stack = $('toaststack');
  const row = el('div', 'toast on' + (bad ? ' bad' : ''));
  row.appendChild(el('span', 'toast-msg', message));
  if (bad) {
    // Errors persist and are copyable; the ✕ is the only thing that closes one.
    row.tabIndex = -1;
    const x = el('span', 'toast-x', '✕');
    x.setAttribute('role', 'button');
    x.title = 'Dismiss';
    x.onclick = () => dismissToast(row);
    row.appendChild(x);
    /* Take focus on pointerdown, or the copy never happens: with a terminal
       focused, Ctrl+C is an interrupt on its way to the pty, not a copy. The ✕ is
       exempt, so dismissing does not first steal focus for a copy nobody made. */
    row.addEventListener('pointerdown', (e) => {
      if (e.target === x) return;
      toastReturn = /** @type {HTMLElement} */ (document.activeElement);
      row.focus();
    });
  }
  stack.appendChild(row);
  if (!bad) armToast(row, 2600);
  // A burst must not fill the screen: drop the oldest past the cap.
  while (stack.children.length > MAX_TOASTS) {
    dismissToast(/** @type {HTMLElement} */ (stack.firstElementChild));
  }
}

/* ---------------------------------------------------------------------------
 * Dialogs
 *
 * **`window.confirm` and `window.prompt` do not work in this app on macOS, and
 * they fail silently.** WKWebView shows a script dialog only if the host
 * application implements the matching `WKUIDelegate` method, and wry implements
 * exactly three of them — the file-open panel, the media-capture permission and
 * `window.open`. None of the JavaScript dialogs. With the delegate methods
 * absent, WebKit's documented behaviour is that `alert()` does nothing,
 * `confirm()` returns **false** and `prompt()` returns **null**.
 *
 * So on a Mac every guarded action read as dead: two `prompt()` flows (naming a
 * worktree, the commit message for existing work) returned null and took the
 * early `return`, and six `confirm()` guards returned false and refused —
 * move out of main, swap branch, delete session, remove worktree, start a fix
 * run, discard unsaved edits. Nothing was broken and nothing said anything.
 * WebKitGTK ships default script dialogs, which is why Linux never showed it.
 *
 * Drawn here rather than routed to a native dialog through the daemon. The app
 * already draws its own window controls, menus and rename box for the same
 * reason: what the webview will render is knowable, and what a host delegate
 * will do is not. It also means a browser tab behaves identically, and there is
 * one code path to reason about instead of two.
 * ------------------------------------------------------------------------- */

/** Resolve for the dialog currently on screen, or null when there is none. */
let dlgSettle = null;
/** What that dialog is asking, and the promise everyone waiting shares.
 *
 *  **Re-entrancy is not hypothetical here.** One of these guards is reached from
 *  `render`, which runs on every frame that has a new snapshot: a guard that
 *  refuses leaves the state it guards unchanged, so the next render asks again.
 *  `window.confirm` could not hit this because it blocked the thread. This one
 *  does not, so the same question arriving twice has to answer from the dialog
 *  already on screen rather than tearing it down and building it again, which
 *  would be a box that flickers once a frame and can never be answered. */
let dlgAsking = null;
let dlgPending = null;

/** Take the dialog down and answer whoever is waiting. */
function dlgClose(answer) {
  const host = $('dlg');
  host.hidden = true;
  host.replaceChildren();
  const settle = dlgSettle;
  dlgSettle = null;
  dlgAsking = null;
  dlgPending = null;
  if (settle) settle(answer);
}

/** Is a dialog waiting for an answer? For the `Esc` chain. */
export const dialogOpen = () => dlgSettle !== null;

/** Cancel the open dialog, however it was asked. */
export function dismissDialog() {
  if (dlgSettle) dlgClose(null);
}

/** The shared shell: a message, a body the caller fills, and two buttons.
 *
 *  Returns the promise the caller awaits. The same question asked again while it
 *  is still up hands back the promise already outstanding — see `dlgAsking`. A
 *  *different* question replaces it rather than stacking, because these are all
 *  guards on a gesture and two on screen means one of the gestures is lost. */
// `body` and `focus` default rather than being left off, so `checkJs` reads them
// as optional: a destructured parameter with no default is a required field.
function dlgOpen(message, { ok, danger, answer, body = null, focus = null }) {
  if (dlgSettle && dlgAsking === message) return dlgPending;
  if (dlgSettle) dlgClose(null);
  const host = $('dlg');
  host.replaceChildren();
  const card = el('div', 'dlgcard');
  card.setAttribute('role', 'dialog');
  card.setAttribute('aria-modal', 'true');

  // Newlines are how every one of these messages was written for `confirm`, and
  // they carry the detail under the question. `white-space: pre-wrap` in the
  // stylesheet keeps them rather than collapsing the lot into one paragraph.
  card.appendChild(el('div', 'dlgmsg', message));
  if (body) card.appendChild(body);

  const foot = el('div', 'dlgfoot');
  const cancel = el('button', 'dlgbtn', 'Cancel');
  cancel.onclick = () => dlgClose(null);
  const go = el('button', 'dlgbtn go' + (danger ? ' danger' : ''), ok || 'OK');
  go.onclick = () => dlgClose(answer());
  // Cancel first, so Tab reaches the safe one before the destructive one and the
  // row still reads left to right in the order everything else puts them.
  foot.appendChild(cancel);
  foot.appendChild(go);
  card.appendChild(foot);
  host.appendChild(card);
  host.hidden = false;

  card.onkeydown = (ev) => {
    // Enter commits, except in a textarea where it is a newline. None of these
    // use one today; the guard is here so adding one does not surprise anybody.
    if (ev.key === 'Enter' && !ev.shiftKey
      && /** @type {HTMLElement} */ (ev.target).tagName !== 'TEXTAREA') {
      ev.preventDefault();
      dlgClose(answer());
    }
  };
  (focus || go).focus();
  dlgAsking = message;
  dlgPending = new Promise((resolve) => { dlgSettle = resolve; });
  return dlgPending;
}

/** `window.confirm`, drawn by the app. Resolves true or false, never throws. */
export function confirmBox(message, { ok = 'Yes', danger = true } = {}) {
  return dlgOpen(message, { ok, danger, answer: () => true })
    .then((a) => a === true);
}

/** `window.prompt`, drawn by the app. Resolves the text, or null if cancelled.
 *
 *  Blank resolves as the empty string rather than null, because one caller means
 *  something by it: naming a worktree blank is "let Claude name it". Callers that
 *  need words check for them. */
export function promptBox(message, { value = '', placeholder = '', ok = 'OK' } = {}) {
  const box = el('div', 'dlgbody');
  const input = /** @type {HTMLInputElement} */ (el('input', 'dlginput'));
  input.type = 'text';
  input.value = value;
  input.placeholder = placeholder;
  input.setAttribute('aria-label', message);
  box.appendChild(input);
  return dlgOpen(message, {
    ok,
    danger: false,
    body: box,
    focus: input,
    answer: () => input.value,
  }).then((a) => (a === null ? null : String(a)));
}

/* **Which daemon a call goes to is a parameter with a default, not a rewrite.**
 * Defaulting to the active repository is what let this become multi-repository
 * without touching the 58 places that name a path and nothing else: they act on
 * the session you are looking at, which is the one whose daemon owns it.
 *
 * The rail is the exception and has to be explicit, because it is the one pane
 * showing rows from repositories you are *not* in — a kill sent to the active
 * daemon for a row belonging to another would either 404 or, far worse, name a
 * session id that daemon also has. Hence [`callOn`] and [`getOn`].
 *
 * `x-orch-token` per repository because each daemon minted its own. A peer is a
 * different origin, so these are cross-origin requests: the peer is started
 * knowing this board's origin and answers it (`Config::sibling_origin`), which is
 * what makes talking to it directly possible at all — the alternative was proxying
 * every call and both websockets through the primary. */
export async function callOn(repoId, path, body) {
  const repo = repoById(repoId);
  /* **A shell route sent anywhere but the shell is a bug here, not a refusal
     there.** The daemon's own answer ("no native window attached") is correct and
     unhelpful: it says the peer cannot do it, not that the caller should not have
     asked. Caught at the source so the next one is obvious. */
  if (repo !== localRepo && isShellRoute(path)) {
    throw new Error(`${path} belongs to the app, not a repository — use callShell`);
  }
  const res = await fetch(`${repo.origin}${path}`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'x-orch-token': repo.token,
    },
    body: JSON.stringify(body ?? {}),
  });
  const json = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(json.error || res.statusText);
  return json;
}

export async function getOn(repoId, path) {
  const repo = repoById(repoId);
  const res = await fetch(`${repo.origin}${path}`, {
    headers: { 'x-orch-token': repo.token },
  });
  const json = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(json.error || res.statusText);
  return json;
}

export const call = (path, body) => callOn(activeRepo.id, path, body);
export const get = (path) => getOn(activeRepo.id, path);

/** For the routes the *app* owns: the window, the OS opener, the page's own boot
 *  timing, an upgrade. Always the daemon that served this page, whatever
 *  repository you happen to be working in. */
export const callShell = (path, body) => callOn(localRepo.id, path, body);

export function duration(ms) {
  if (ms == null) return '';
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  // Archived conversations are days old soon enough, and "51h 0m" is not a
  // number anybody reads as two days.
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

/** Compact age from hours: `now`, `5h`, `2d`. The review card and the queue row
 *  share the 48h cut-over, so it lives once. */
export function compactAge(hours) {
  if (hours < 1) return 'now';
  if (hours < 48) return `${Math.round(hours)}h`;
  return `${Math.round(hours / 24)}d`;
}

// The poll counter each pane captured when its refresh was pressed; the button
// spins until the live counter moves past it. null = not spinning.
const spinFloor = { pr: null, review: null };

/** Give a `role="button"` span what a real <button> has for free: a tab stop and
 *  Enter/Space activation. Without this a span-button is mouse-only, which is a
 *  keyboard trap for the refresh icons and the update-nudge dismiss. */
export function keyActivate(el) {
  el.tabIndex = 0;
  // Property assignment, not addEventListener: renderUpdate re-wires #updatex on
  // every snapshot, and a stacked listener would fire click N times.
  el.onkeydown = (e) => {
    if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); el.click(); }
  };
}

/*
 * A ↻ that forces a poll and spins until the poll it triggered lands.
 * `pollCount` is the pane's monotonic poll counter from the snapshot; `endpoint`
 * is the POST that pulses that poller. Used by both the PR and review panes.
 */
/** A signature of what a pane draws from, for skipping a rebuild that would
 *  change nothing.
 *
 *  **The daemon pushes a whole snapshot on every state change** — `notify` is
 *  called from about seventy places, and three running sessions measured at ~7
 *  pushes a second. A pane that rebuilds on each one destroys the node under the
 *  pointer seven times a second: `:hover` is re-targeted on every rebuild, so a
 *  highlight or a border strobes; a native `title` never gets the half second of
 *  rest it needs; and a click whose mousedown and mouseup land on two different
 *  elements is not delivered at all. That is the review row that does not open
 *  and the `continue` button that flickers.
 *
 *  **Every `_ms` field is left out on purpose.** They are measured when the
 *  snapshot is taken, so they differ on every push by construction, and none of
 *  them is drawn directly: each goes through `clock()` into a `data-clock` node
 *  that `Rail.tick` rewrites in place once a second. Keeping them would make
 *  every signature differ and the guard a no-op.
 *
 *  `drop` names the fields a *particular* pane does not draw. Passing the whole
 *  snapshot is the safe way to build one of these — a signature that lists its
 *  inputs is one refactor away from freezing its pane — but safe is not free: the
 *  rail was rebuilt on every edit an agent made, because a `PostToolUse` sweep
 *  rewrites the workspace's changed-file list and that rides the same snapshot,
 *  and the rail does not draw it. Naming what to ignore keeps "any change
 *  rebuilds" as the default and takes the churn out one pane at a time. */
function paintSig(value, drop = []) {
  return JSON.stringify(value, (k, v) => (k.endsWith('_ms') || drop.includes(k) ? undefined : v));
}

/** True when `value` renders the same as it did last time this box was asked.
 *
 *  The box is the pane's own `{ sig: null }`: five panes were each carrying a
 *  module-level `let xSig`, the same three lines of compare-and-remember, and the
 *  same comment with one noun changed. One name for the idiom means a reader
 *  confirms it once. */
export function unchanged(box, value, drop = []) {
  const sig = paintSig(value, drop);
  if (box.sig === sig) return true;
  box.sig = sig;
  return false;
}

export function refreshButton(kind, pollCount, endpoint, polling) {
  // Drawn, not typed — see the files-header refresh in index.html for why the
  // reload glyph is an SVG rather than U+21BB. 1em tracks the font-size setting.
  const btn = el('span', 'rvrefresh');
  btn.innerHTML = '<svg viewBox="0 0 16 16" width="1em" height="1em" fill="none"'
    + ' stroke="currentColor" stroke-width="1.5" stroke-linecap="round"'
    + ' stroke-linejoin="round" aria-hidden="true">'
    + '<path d="M13.4 8A5.4 5.4 0 1 1 11.7 4"/><path d="M12 1.6V4.3H9.3"/></svg>';
  btn.title = 'Refresh now';
  btn.setAttribute('role', 'button');
  keyActivate(btn);
  if (spinFloor[kind] != null && pollCount > spinFloor[kind]) spinFloor[kind] = null;
  // Two reasons to spin, and the second is the honest one: the daemon says a
  // fetch is running, whoever started it. `spinFloor` covers the gap between the
  // click and the daemon reporting the fetch, which is a round trip away.
  if (spinFloor[kind] != null || polling) btn.classList.add('spin');
  btn.onclick = (e) => {
    e.stopPropagation();               // the header's own click toggles the pane
    spinFloor[kind] = pollCount;
    btn.classList.add('spin');
    call(endpoint).catch((err) => { spinFloor[kind] = null; toast(err.message, true); });
  };
  return btn;
}

// ---------------------------------------------------------------------------
// UI scale
// ---------------------------------------------------------------------------

/* One panel, one setting so far. Font size is a `zoom` on the grid rather than a
 * sweep of every px in the stylesheet: it scales the terminal, the rail and the
 * diff together, which is what "font size" means when the whole window is text.
 *
 * Kept in localStorage, like the column widths — it is this browser's opinion,
 * not something the daemon owns. */
/* What "100%" means: 1.155 of the stylesheet's own sizes, because the design was
 * drawn a little small for a full-screen window. Was 1.1, which read a step small
 * in practice: what used to be the 105% setting is now the default.
 *
 * Every text size in the sheet is
 * `calc(Npx * var(--fs))`, so this scales type and leaves layout alone — no
 * `zoom`, which is a legacy property that WebKitGTK mispaints at scale. */
/* Who wants to know the UI scale changed. A list rather than a direct call so
 * `setZoom` needs no opinion about what is scalable. */
const scaleListeners = [];
export function onScaleChange(fn) { scaleListeners.push(fn); }

const FS_BASE = 1.155;
export const ZOOM = { key: 'orch.uiZoom', def: 1, min: 0.8, max: 1.5, step: 0.05 };

/** The user-facing scale, where 1 is the default. */
export let zoomScale = ZOOM.def;

/** The multiplier the stylesheet and the terminal both read. */
export const uiScale = () =>
  Number(getComputedStyle(document.documentElement).getPropertyValue('--fs')) || FS_BASE;

export function setZoom(z) {
  const next = Math.min(ZOOM.max, Math.max(ZOOM.min, Math.round(z * 100) / 100));
  zoomScale = next;
  document.documentElement.style.setProperty('--fs', String(next * FS_BASE));
  $('fsval').textContent = `${Math.round(next * 100)}%`;
  ctl('fsdown').disabled = next <= ZOOM.min;
  ctl('fsup').disabled = next >= ZOOM.max;
  // Announced rather than applied: the terminals' own font is xterm's business,
  // and reaching into it from here is what made zoom and the terminals depend on
  // each other. Whoever owns a scalable thing registers for this.
  for (const fn of scaleListeners) fn(next);
  return next;
}

/* ---------------------------------------------------------------------------
 * Theme
 * ------------------------------------------------------------------------- */

/* **A theme is three colours and a font, and everything else is derived from
 * them.** The sheet has some twenty tokens; offering twenty colour pickers would
 * be a reliable way to produce an unreadable board — a border the same value as
 * its background, dim text on a light ground. So the user sets the ground, the
 * panel and the text, and the steps between them (`--raised`, `--hover`,
 * `--line`, `--dim`, `--ghost`, …) are mixed from that pair here.
 *
 * **The semantic colours are deliberately not offered.** `--attn` is "needs you",
 * `--bad` is a red build, `--ok` is passing: they are a legend the rail, the PR
 * rows and the review queue all read, and a user who set amber to grey would not
 * be theming, they would be turning a signal off. `--focus` stays out for the
 * same reason.
 *
 * **In `localStorage`, like the UI scale and the column widths.** Nothing about
 * which colours you like belongs in `config.json`, where a daemon that never
 * reads it would have to carry it — the same reasoning `ZOOM` and the rail order
 * already stand on.
 *
 * **Derived in JavaScript rather than with `color-mix()`**, which the sheet could
 * have done: xterm takes hex strings and cannot read a CSS colour, so a mix the
 * stylesheet owned would leave the terminal on a second, hand-written palette —
 * which is exactly the duplicate this replaces (`term.js` had `#101010` and
 * `#D2D2D2` written out again). One derivation, two consumers.
 */

export const THEME = { key: 'orch.theme' };

/** Monospace families worth *asking* about.
 *
 *  **A list, because a page cannot enumerate installed fonts here.**
 *  `queryLocalFonts()` is the API for that and it is Chromium-only, behind a
 *  permission prompt — absent from WebKit, so absent from WKWebView on macOS and
 *  WebKitGTK on Linux, which is every window this app opens. What *is*
 *  engine-agnostic is asking whether one named family exists ([`installed`]), so
 *  the offer is a generous list filtered down to what is really there.
 *
 *  Notably **not** `SF Mono`. It is on every Mac — `/System/Library/Fonts/
 *  SFNSMono.ttf` — and Apple does not expose the system faces to web content
 *  under their own names: measured, and both `SF Mono` and `SFMono-Regular` come
 *  back absent while `ui-monospace` measures 1015.88 against `monospace`'s 864.14,
 *  which is SF Mono answering to the generic. So the generic is the way in, and
 *  `system` below is that door with the right label on it. */
const MONO_CANDIDATES = [
  'Menlo', 'Monaco', 'Andale Mono', 'PT Mono', 'Courier New',
  'Cascadia Mono', 'Cascadia Code', 'Consolas', 'Lucida Console',
  'Fira Code', 'Fira Mono', 'Hack', 'Source Code Pro', 'Roboto Mono',
  'Ubuntu Mono', 'DejaVu Sans Mono', 'Liberation Mono', 'Noto Sans Mono',
  'Inconsolata', 'Iosevka', 'Victor Mono', 'Geist Mono', 'Berkeley Mono',
  'Operator Mono', 'Anonymous Pro', 'Space Mono', 'Recursive Mono',
  'SF Mono',
];

/** Is this family actually on the machine?
 *
 *  Measured rather than asked, and against **two** different fallbacks: a family
 *  that is missing falls back to each of them and so measures two different
 *  widths, while one that exists measures the same width whatever sits behind it.
 *  `document.fonts.check` looks like the direct answer and is not — it reports
 *  true for a family the engine merely intends to substitute.
 *
 *  The probe mixes wide and narrow glyphs so two similar monospace faces still
 *  differ; a single `m` would collide too easily. */
function installed(name, ctx) {
  const w = (family) => {
    ctx.font = `72px ${family}`;
    return Math.round(ctx.measureText('mmmmmmmmmmlliWWWW0Oo').width * 100) / 100;
  };
  const a = w(`'${name}',serif`);
  return a === w(`'${name}',sans-serif`) && a === w(`'${name}',monospace`);
}

/** The vendored faces, the system one, and whatever else is really here.
 *
 *  The first three are `@font-face`d from `/vendor/fonts` in `app.css`, so they
 *  work offline and look the same on every machine — and are excluded from the
 *  detected list below, because a `@font-face`d family measures as *installed*
 *  whether or not the machine has it. IBM Plex Mono came back "present" on a
 *  machine that has no such file, for exactly that reason. */
export const FONTS = (() => {
  const vendored = {
    plex: { label: 'IBM Plex Mono', stack: "'IBM Plex Mono',ui-monospace,monospace" },
    jetbrains: { label: 'JetBrains Mono', stack: "'JetBrains Mono','IBM Plex Mono',ui-monospace,monospace" },
    martian: { label: 'Martian Mono', stack: "'Martian Mono',ui-monospace,monospace" },
    system: {
      // Named for what it resolves to, because "System monospace" is not what
      // anybody is looking for when they want SF Mono — and on a Mac this is it.
      label: IS_MAC ? 'SF Mono (system)' : 'System monospace',
      stack: 'ui-monospace,SFMono-Regular,Menlo,Consolas,monospace',
    },
  };
  const shipped = new Set(Object.values(vendored).map((f) => f.label));
  let found = [];
  try {
    const ctx = document.createElement('canvas').getContext('2d');
    if (ctx) {
      found = MONO_CANDIDATES
        .filter((n) => !shipped.has(n))
        .filter((n) => installed(n, ctx));
    }
  } catch {
    // No canvas, no detection: the vendored faces and the system stack are still
    // a complete offer, so this degrades to what it was.
  }
  const detected = {};
  for (const name of found.sort()) {
    detected[name] = { label: name, stack: `'${name}',ui-monospace,monospace` };
  }
  return { ...vendored, ...detected };
})();

/** Ground, panel and text — the three a preset has to answer.
 *
 *  `orchd` is the palette the app shipped with, so "Reset" is a real answer and
 *  the default is not a preset that merely resembles it. The rest are starting
 *  points to dial in from rather than an attempt to reproduce somebody's terminal
 *  theme, which cannot be guessed. */
export const PRESETS = {
  orchd: { label: 'Orchd dark', bg: '#101010', panel: '#171717', text: '#D2D2D2' },
  ink: { label: 'Ink', bg: '#0B0D12', panel: '#141821', text: '#C8D0DC' },
  contrast: { label: 'High contrast', bg: '#000000', panel: '#0C0C0C', text: '#F2F2F2' },
  paper: { label: 'Paper', bg: '#F4F2ED', panel: '#EAE7E0', text: '#22201C' },
};

/** How opaque the ground is, when the window lets light through at all.
 *
 *  A theme value rather than a config one, unlike `window_transparent`: once the
 *  window is see-through the page can change *how much* on every frame, so this
 *  belongs beside the colours in `localStorage` and needs no restart. Floored well
 *  above zero — a fully invisible board is not a theme, it is a lost window. */
export const OPACITY = { min: 0.35, max: 1, step: 0.05, def: 0.9 };

const THEME_DEF = { ...PRESETS.orchd, font: 'plex', custom: null, opacity: OPACITY.def };

/** @type {{bg:string, panel:string, text:string, font:string, custom:string|null, opacity:number}} */
export let theme = THEME_DEF;

const themeListeners = [];
export function onThemeChange(fn) { themeListeners.push(fn); }

/* ---- colour arithmetic ---------------------------------------------------- */

export const clampOpacity = (v) => {
  const n = Number(v);
  if (!Number.isFinite(n)) return OPACITY.def;
  /* Rounded, the way `setZoom` rounds its own: stepping by 0.05 accumulates
     binary error, and this value is written to `localStorage` — so without it a
     board that reads 60% stores `0.5999999999999998`. */
  return Math.round(Math.min(OPACITY.max, Math.max(OPACITY.min, n)) * 100) / 100;
};

const hex = (v) => {
  const m = /^#?([0-9a-f]{6})$/i.exec(String(v || '').trim());
  return m ? m[1] : null;
};
const rgb = (v) => {
  const h = hex(v) || '000000';
  return [parseInt(h.slice(0, 2), 16), parseInt(h.slice(2, 4), 16), parseInt(h.slice(4, 6), 16)];
};
const out = ([r, g, b]) =>
  '#' + [r, g, b].map((x) => Math.max(0, Math.min(255, Math.round(x))).toString(16).padStart(2, '0')).join('');

/** A colour with an alpha, for the two surfaces that show the desktop through. */
const alpha = (v, a) => {
  const [r, g, b] = rgb(v);
  return `rgba(${r}, ${g}, ${b}, ${a})`;
};

/** `t` of the way from `a` to `b`. */
const mix = (a, b, t) => {
  const [x, y, z] = rgb(a);
  const [p, q, r] = rgb(b);
  return out([x + (p - x) * t, y + (q - y) * t, z + (r - z) * t]);
};

/* ---- the derived tokens --------------------------------------------------- */

/** Every token the sheet needs, from the three the user set.
 *
 *  **Mixed toward the text colour, never "lighter".** That is what makes a light
 *  theme work: on `paper` a border has to be *darker* than its ground, and a rule
 *  that added white would have drawn it invisible. Mixing toward the foreground is
 *  the same instruction in both directions. */
function tokens(t) {
  const { bg, panel, text } = t;
  /* **The alpha goes on the two *surfaces*, never on `--bg`.** That token is used
     eighteen other times in the sheet — input grounds, hover fills, a select's
     background — and making it translucent would leave you reading a text field
     through the desktop. So the window's ground and the panels get their own
     tokens, and every component keeps a solid one.

     Only when the window is actually see-through: with an opaque window an alpha
     ground composites over Tauri's `background_color` and does nothing but cost a
     blend, and the setting is disabled in the pane to say so. */
  const see = TRANSPARENT && t.opacity < 1;
  return {
    '--bg': bg,
    '--ground': see ? alpha(bg, t.opacity) : bg,
    '--surface': see ? alpha(panel, t.opacity) : panel,
    // A row you are on, and a row under the pointer one step below it. Off the
    // panel rather than the ground, because that is what they sit on.
    '--raised': mix(panel, text, 0.1),
    '--hover': mix(panel, text, 0.05),
    '--line': mix(panel, text, 0.16),
    '--line-soft': mix(panel, text, 0.1),
    '--text': text,
    '--dim': mix(bg, text, 0.72),
    '--faint-solid': mix(bg, text, 0.54),
    '--ghost': mix(bg, text, 0.42),
    '--mono': fontStack(t),
    // Read line by line, so it keeps its own face unless the choice *is* the
    // reading font: a diff in Martian Mono is not something to inflict by
    // accident.
    '--code': t.font === 'plex' ? FONTS.jetbrains.stack : fontStack(t),
  };
}

export function fontStack(t = theme) {
  if (t.font === 'custom' && t.custom) {
    /* Quoted because a family name with a space is otherwise two names, and
       stripped of the characters that would end the declaration — this string goes
       into a `style` property, so a stray `;` or `}` is the one way a font name
       could reach further than a font name should. */
    const name = String(t.custom).replace(/["';{}]/g, '').trim();
    if (name) return `'${name}',ui-monospace,monospace`;
  }
  return (FONTS[t.font] || FONTS.plex).stack;
}

/** What xterm should paint with, from the same three colours.
 *
 *  The ANSI sixteen keep their hues — a red that is not red stops being an error
 *  — but are mixed toward the ground so they sit on it rather than glowing off
 *  it, which is what an unadjusted palette does on a light theme. */
export function termColours(t = theme) {
  const { bg, text } = t;
  const on = (c, amount = 0.12) => mix(c, bg, amount);
  const see = TRANSPARENT && t.opacity < 1;
  return {
    /* The terminal is most of the window, so a see-through board that stopped at
       the pane edges would not be see-through at all. Needs `allowTransparency`
       on the `Terminal`, which is fixed at construction — hence `TRANSPARENT`
       being told at boot rather than looked up. */
    background: see ? alpha(bg, t.opacity) : bg,
    foreground: text,
    cursor: text,
    selectionBackground: mix(bg, text, 0.18),
    black: bg,
    red: on('#C9615A'), green: on('#5FA97C'), yellow: on('#E0A244'),
    blue: on('#4C9AAF'), magenta: on('#9A7AA0'), cyan: on('#3E9AAF'),
    white: text,
    brightBlack: mix(bg, text, 0.42),
    brightRed: '#D6756E', brightGreen: '#74BB90', brightYellow: '#EDB55C',
    brightBlue: '#63AEC2', brightMagenta: '#B08FB6', brightCyan: '#57AEC2',
    brightWhite: mix(text, '#FFFFFF', 0.4),
  };
}

/* ---- reading, writing, applying ------------------------------------------- */

function loadTheme() {
  try {
    const raw = localStorage.getItem(THEME.key);
    const got = raw ? JSON.parse(raw) : null;
    if (!got || typeof got !== 'object') return THEME_DEF;
    // Field by field, and every colour re-validated: this is a file a person can
    // hand-edit, and a token set from `"red; }"` would be writing CSS rather than
    // picking a colour.
    return {
      bg: hex(got.bg) ? got.bg : THEME_DEF.bg,
      panel: hex(got.panel) ? got.panel : THEME_DEF.panel,
      text: hex(got.text) ? got.text : THEME_DEF.text,
      // A stored family that is no longer installed falls back rather than
      // rendering as something else: the list is built from this machine.
      font: got.font === 'custom' || FONTS[got.font] ? got.font : THEME_DEF.font,
      custom: typeof got.custom === 'string' ? got.custom : null,
      opacity: clampOpacity(got.opacity),
    };
  } catch {
    return THEME_DEF;
  }
}

/** Write the tokens onto the root, where the whole sheet reads them. */
function applyTheme(t) {
  const root = document.documentElement;
  for (const [k, v] of Object.entries(tokens(t))) root.style.setProperty(k, v);
  /* `html` has to stop painting its own ground, or it sits opaque behind the
     translucent `body` and nothing shows through. Set to `transparent` rather
     than to the rgba value, so the two are not blended twice. */
  if (TRANSPARENT && t.opacity < 1) {
    root.style.background = 'transparent';
    return;
  }
  /* **`html` too, and not only the token.** `index.html` carries an inline
     `html,body{background:#101010}` so the window is not white while `app.css`
     parses, and Tauri paints the same value under the webview — both compiled in,
     both dark. A light theme would keep that near-black behind every scroll
     overshoot and rubber-band. This is as far as the page can reach: the *first*
     frame is still the compiled-in colour, because it is painted before any script
     runs. Fixing that last frame means the daemon substituting the colour into the
     page and into `background_color`, which is a restart — see the note in
     `TODO.md`. */
  root.style.background = t.bg;
}

/** Change some of the theme and keep the rest.
 *
 *  A patch rather than a whole theme, because the controls set one field each and
 *  a preset sets three; making every caller pass all of them is how one of them
 *  would eventually reset the font by omission. */
export function setTheme(patch) {
  theme = { ...theme, ...patch };
  applyTheme(theme);
  try {
    localStorage.setItem(THEME.key, JSON.stringify(theme));
  } catch {
    // A remembered theme is a convenience; failing to keep it is not worth a toast.
  }
  // Announced rather than applied, the same shape as `setZoom`: the terminals'
  // own colours are xterm's business and `term.js` registers for this.
  for (const fn of themeListeners) fn(theme);
  return theme;
}

/** Read the stored theme and paint it. Called once, before the first render. */
export function initTheme() {
  theme = loadTheme();
  applyTheme(theme);
  return theme;
}

/* **How far one wheel event travels in an agent pane.** A multiplier on the pixel
 * delta, defaulting to 1 — which is exactly today's behaviour, so a trackpad keeps
 * the fix that put this handler here in the first place (a slow drag needs every
 * pixel of its ~13px median delta) and nobody who has not asked for a change gets
 * one.
 *
 * It exists because a *discrete* wheel is the opposite case: macOS accelerates a
 * notch and reports it as a ~180px delta with `deltaMode === 0`, so it never takes
 * the line-mode escape, and at a ~15px cell one notch travels about twelve lines.
 * That is far enough to lose your place in the transcript. Some people want that
 * speed, which is why this is a setting rather than a new fixed number.
 *
 * Applied to the *accumulated pixel delta*, never before a threshold — that is what
 * makes it work where xterm's own `scrollSensitivity` cannot: it multiplies before
 * a test that reads the raw delta, so a value that suits a mouse breaks a trackpad.
 *
 * localStorage, beside the zoom, for the reason stated there: it is this browser's
 * opinion — this machine and this mouse — not something the daemon owns. */
export const WHEEL = { key: 'orch.wheelScale', def: 1, min: 0.1, max: 2, step: 0.1 };

/** The multiplier `term.js` reads. A live binding, so lowering it takes effect on
 *  the next wheel event without the terminals re-importing anything. */
export let wheelScale = WHEEL.def;

export function setWheel(w) {
  const next = Math.min(WHEEL.max, Math.max(WHEEL.min, Math.round(w * 10) / 10));
  wheelScale = next;
  $('wsval').textContent = `${Math.round(next * 100)}%`;
  ctl('wsdown').disabled = next <= WHEEL.min;
  ctl('wsup').disabled = next >= WHEEL.max;
  return next;
}

export function saveWheel(w) {
  try {
    if (w === WHEEL.def) localStorage.removeItem(WHEEL.key);
    else localStorage.setItem(WHEEL.key, String(w));
  } catch (e) { /* private mode: it still applies for this session */ }
}

export function saveZoom(z) {
  try {
    if (z === ZOOM.def) localStorage.removeItem(ZOOM.key);
    else localStorage.setItem(ZOOM.key, String(z));
  } catch (e) { /* private mode: it still applies for this session */ }
}

// ---------------------------------------------------------------------------
// The shared vocabulary
// ---------------------------------------------------------------------------
//
// What every pane needs to say about a session, a workspace or a menu. It lived
// in `app.js` because that was the only file; the seams all reached for it, which
// is what made them seams rather than modules.

export const terms = new Map();      // target -> { term, fit, sock, host }

export function stateLabel(s) {
  const handed = handedToPr(s);
  if (handed) return `#${handed.number} ${prState(handed)}`;
  switch (s.state.state) {
    case 'starting': return 'starting';
    case 'working': return 'working';
    case 'your_turn':
      if (s.state.reason === 'asked_a_question') return 'asked a question';
      if (s.state.reason === 'needs_permission') return 'needs permission';
      if (s.state.reason === 'ready') return 'ready';
      // Said rather than folded into "turn complete", because it is the opposite
      // claim: the turn did not complete, you stopped it, and there is more of it
      // owed. Same word the resume nudge uses about a session it offers to continue.
      if (s.state.reason === 'interrupted') return 'interrupted';
      return 'turn complete';
    case 'build_failing': return s.state.summary || 'build failing';
    case 'error': return s.state.message || 'error';
    // One word for both: a session whose process ended and one archived by a
    // restart are the same thing to you, a conversation you are not in.
    case 'exited': return 'archived';
    case 'archived': return s.state.resumable ? 'archived' : 'archived, transcript only';
    default: return s.state.state;
  }
}

/** Dot colours are shared across every row so one legend covers them all (§9). */
export function dotClass(s) {
  const k = s.state.state;
  if (k === 'build_failing' || k === 'error') return 'build';
  if (handedToPr(s)) return 'pr';
  /* `ready` is not blocked, and this was the one predicate that thought it was.
     `stateClass` below and the daemon's `wants_attention` both read
     `your_turn && reason !== 'ready'`; this read `your_turn`. So a session that
     had only just resumed wore the attention colour while the bar deliberately
     left it out of the count — the dot shouting about the one thing the rail had
     decided not to shout about. */
  if (k === 'your_turn') return s.state.reason === 'ready' ? 'idle' : 'blocked';
  /* No colour of its own for a session started as a pass. It used to wear azure
     ahead of its state, on the reasoning that "already being handled" outranks
     what the session is doing — which stopped being true the moment a pass meant
     a pane you sit in as often as a run nobody watches. The state is the signal. */
  if (k === 'working' || k === 'starting') return 'working';
  if (k === 'archived' || k === 'exited') return 'archived';
  return 'idle';
}

export function stateClass(s) {
  const k = s.state.state;
  if (k === 'build_failing' || k === 'error') return 'build';
  if (handedToPr(s)) return 'pr';
  if (k === 'your_turn' && s.state.reason !== 'ready') return 'blocked';
  return '';
}

/** Idle time worth surfacing. A session you opened and have not typed into is
 *  idle, but shouting about it the moment you open it is noise. */
export const isWaiting = (s) => s.wants_attention;

/**
 * A menu at the cursor. `items` are `[label, extraClass, handler]`; a null
 * handler renders the row disabled, so right-clicking a session that has
 * already ended still says what the menu would have offered.
 */
/** Put text on the clipboard, whatever the webview allows.
 *
 *  WebKitGTK refuses the async clipboard API in a webview often enough that its
 *  `NotAllowedError` was showing up as a toast that read like a bug. The old
 *  `execCommand` path has no permission to refuse: inside a user gesture it just
 *  copies, which is what a keypress or a menu item is.
 *
 *  Here rather than in `term.js` because the rail's `copy id` needs the same two
 *  attempts, and the fallback is the part that is easy to get subtly wrong. */
export async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (e) { /* fall through to the one that works */ }
  try {
    const ta = el('textarea');
    ta.value = text;
    // Off-screen rather than hidden: a `display:none` textarea cannot be selected.
    ta.style.cssText = 'position:fixed;top:-1000px;opacity:0';
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand('copy');
    ta.remove();
    if (!ok) throw new Error('refused');
    return true;
  } catch (e) {
    toast('this window is not allowed to write to the clipboard', true);
    return false;
  }
}

export function openMenu(ev, items) {
  ev.preventDefault();
  const menu = $('ctxmenu');
  menuAnchor = /** @type {HTMLElement} */ (ev.currentTarget || ev.target);
  menu.replaceChildren();
  for (const [label, cls, handler] of items) {
    const item = el('button', 'ctxmenu-item' + (cls ? ` ${cls}` : ''), label);
    if (handler) item.onclick = () => { closeMenu(); handler(); };
    else item.disabled = true;
    menu.appendChild(item);
  }
  // Un-hidden before it is measured, or there is no box to clamp.
  menu.hidden = false;
  const box = menu.getBoundingClientRect();
  // Keyboard activation reports no cursor, so hang it off the button instead of
  // pinning it to the top-left corner.
  let { clientX: x, clientY: y } = ev;
  if (!x && !y) {
    const r = (ev.currentTarget || ev.target).getBoundingClientRect();
    [x, y] = [r.left, r.bottom];
  }
  menu.style.left = `${Math.min(x, window.innerWidth - box.width - 6)}px`;
  menu.style.top = `${Math.min(y, window.innerHeight - box.height - 6)}px`;
}

/** Dismiss the keyboard legend.
 *
 *  Here rather than in `app.js`, which owns the overlay, because the settings
 *  panel closes it too and `settings` cannot reach back up to the app layer —
 *  the same reason `closeMenu` lives down here. */
export function closeLegend() {
  $('keyhelp').hidden = true;
}

export function closeMenu() {
  $('ctxmenu').hidden = true;
  menuAnchor = null;
}

/** What the open menu is pointing at, so a scroll can tell "the row this menu
 *  belongs to moved" from "a terminal three panes away printed a line". */
let menuAnchor = null;

/* `s = snap` on each of these is what keeps this change small: every existing
   caller means "the repository I am looking at" and says nothing, while the rail
   — the one pane that draws all of them — passes the repository's own snapshot
   in. Adding a required parameter instead would have been forty edits in seven
   modules to say what the default already says. */
export function sessionsOf(wsId, s = snap) {
  return s.sessions.filter((x) => x.workspace === wsId);
}

/* A session is one of two things: active, or a past conversation you can come
 * back to. The daemon's `exited` and `archived` are the same fact from here, and
 * neither is a state worth a word of its own in the rail. */
export const isArchived = (s) => s.state.state === 'archived' || s.state.state === 'exited';

/** `spawn::PENDING_WORKTREE`: the workspace a worktree session sits in until
 *  `SessionStart` reports the name Claude Code gave it. */
const PENDING_WORKTREE = '\u2026creating';

/** A worktree Claude Code has not named yet (§2): the daemon knows the session
 *  before it knows where it lives. */
export const pending = (s) => s.workspace === PENDING_WORKTREE;

/* A finished session that never had a turn wrote no transcript, so there is no
 * conversation to come back to — `claude --resume` answers "no conversation
 * found" and exits. Listing one is offering something that cannot work, so the
 * archive is conversations, not every session that ever stopped. */
export const isConversation = (s) => isArchived(s) && s.has_transcript;

/** Newest first: `created_ms` is an age, so the smallest number is the newest. */
export const byNewest = (a, b) => a.created_ms - b.created_ms;

export function currentSession() {
  return snap.sessions.find((s) => s.id === selected) || null;
}

/** The workspace the right pane describes: the one you are working in.
 *
 *  Deliberately not `currentWorkspaceId`, which falls back to main so the drawer
 *  and the shell button always have somewhere to act. A file list has no such
 *  duty: main's tree is not "your changes" just because you closed your session,
 *  and a pane still listing a finished session's work reads as live. */
export function activeWorkspaceId() {
  const s = currentSession();
  return s && !isArchived(s) ? s.workspace : null;
}

/** The two questions every pane asks the workspace list. */
export const mainWorkspace = (s = snap) => s.workspaces.find((w) => w.is_main);
export const workspaceById = (id, s = snap) => s.workspaces.find((w) => w.id === id);

export function currentWorkspaceId() {
  const s = currentSession();
  if (s) return s.workspace;
  return mainWorkspace()?.id ?? null;
}

/* One create at a time, and the `+` says so.
 *
 * A session is a worktree, a set of repo hooks and a `claude` boot, which is
 * seconds during which the rail had nothing new on it — so the second press was
 * the reasonable thing to do and it made a second session. Blocked here rather
 * than at the buttons because the keyboard map calls the same two functions
 * (`MOD⇧N`, `MOD N`), and a guard on the click alone would leave the chord able
 * to do what the button refuses.
 *
 * It does not replace the daemon's own refusals: main is exclusive
 * (`refuse_if_occupied`) and says so with a disabled `+`. This is about the gap
 * *before* any of that state exists. */
let creatingWhat = null;
export const creating = () => creatingWhat;

const creatingListeners = [];
export function onCreatingChange(fn) { creatingListeners.push(fn); }

/** Run `go` as the one create in flight, or say what is already going.
 *
 *  Announced rather than rendered, on the seam `setDrawerCollapsed` uses: this
 *  layer must not reach into the rail that sits on it. Announced *both* ways,
 *  because the interesting frame is the one where the button goes dead — the
 *  snapshot that would have redrawn it is not promised to arrive while a worktree
 *  is being cut. */
async function asTheOnlyCreate(what, go) {
  if (creatingWhat) {
    toast(`still ${creatingWhat}`);
    return;
  }
  creatingWhat = what;
  for (const fn of creatingListeners) fn(creatingWhat);
  try {
    await go();
  } finally {
    creatingWhat = null;
    for (const fn of creatingListeners) fn(null);
  }
}

export async function newSession(workspace) {
  await asTheOnlyCreate('starting a session', async () => {
    try {
      const r = await call('/api/session', { workspace });
      pendingSelect = r.session;
    } catch (e) {
      toast(e.message, true);
    }
  });
}

/** Claude Code names the worktree unless you shift-click and name it yourself.
 *  Naming one every time is friction for something you rarely refer to by
 *  name, and an unnamed one cannot collide with an archived worktree either. */
export async function newWorktree(named) {
  let name = null;
  if (named) {
    name = await promptBox('Worktree name', {
      placeholder: 'blank to let Claude name it',
      ok: 'Create',
    });
    // Cancel means cancel; blank means auto.
    if (name === null) return;
    name = name.trim() || null;
  }
  // Claimed after the name box, not before: the prompt is open for as long as you
  // take to type, and holding the claim across it would disable the `+` on a
  // dialog you might cancel.
  await asTheOnlyCreate(name ? `creating worktree ${name}` : 'creating a worktree', async () => {
    try {
      const r = await call('/api/worktree', name ? { name } : {});
      pendingSelect = r.session;
      toast(name ? `creating worktree ${name}` : 'creating worktree');
    } catch (e) {
      toast(e.message, true);
    }
  });
}

export async function newShell() {
  const wsId = currentWorkspaceId();
  if (!wsId) return;
  drawerTouched = true;
  // You pressed + to work in a shell; a collapsed drawer would hide the one you
  // just asked for.
  if (drawerCollapsed) setDrawerCollapsed(false);
  try {
    const r = await call(`/api/workspace/${encodeURIComponent(wsId)}/shell`);
    selectedProc[wsId] = r.process;
    // You pressed + to type in it. The pty does not exist until the daemon says
    // so, so this is claimed here and spent when the terminal appears.
    pendingProcFocus = r.process;
    // The snapshot with it in has usually landed already, so ask for the render
    // rather than waiting for one that has been.
    redrawDrawer();
  } catch (e) {
    toast(e.message, true);
  }
}

// The daemon decides this, not the user agent string: it is the side that knows
// whether it is being shown in a window it owns or in somebody's browser tab.
//
// The commands go over the same authenticated HTTP the rest of the UI uses,
// and the daemon — running inside the desktop process — calls Tauri's window
// API in Rust. No IPC bridge, so nothing here depends on which port we bound.
export const CHROME = window.__ORCH__.chrome || 'none';

/** The modifier the app's own chords wear: ⌘ on a Mac, Ctrl elsewhere. */
export const MOD_LABEL = IS_MAC ? '⌘' : 'Ctrl';

/**
 * Whether `e` carries the app modifier and nothing that would make it a
 * different chord.
 *
 * The split is not only convention. On a Mac ⌘ never reaches the pty, so the
 * app layer costs the terminal *nothing* there — which is why `⌘N` is free while
 * `Ctrl+N` on Linux has to shadow readline's next-history to exist. Keeping Ctrl
 * for the terminal on macOS is the whole point: `Ctrl+C` must stay an interrupt.
 *
 * @param {KeyboardEvent} e
 */
export const appMod = (e) => (IS_MAC ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey) && !e.altKey;

export const menuOpen = () => !$('ctxmenu').hidden;

// Anything that moves what the menu is pointing at dismisses it. On mousedown
// rather than click, and captured, so the row underneath still gets its own
// click; a rail that rebuilds every second would otherwise leave the menu
// hanging over a row that no longer exists.
document.addEventListener('mousedown', (e) => {
  if (menuOpen() && !/** @type {HTMLElement} */ (e.target).closest('#ctxmenu')) closeMenu();
}, true);
/* Only a scroller the menu's own row sits in has actually moved it. This used to
   be `closeMenu` on any scroll at all, and `capture` catches scroll — which does
   not bubble — from every element on the page: a terminal printing a line, or a
   rail whose rebuild clamps its `scrollTop`, dismissed a menu you had just
   opened, roughly once a second while anything was running. */
document.addEventListener('scroll', (e) => {
  if (!menuOpen()) return;
  const t = /** @type {any} */ (e.target);
  const page = t === document || t === document.scrollingElement;
  if (page || (menuAnchor && t.contains?.(menuAnchor))) closeMenu();
}, true);
window.addEventListener('blur', closeMenu);

// ---------------------------------------------------------------------------
// Shared UI state
// ---------------------------------------------------------------------------

export let selectedProc = {};        // workspace id -> process id

/** What a PR is doing, in the two or three words a row has space for. */
export function prState(p) {
  if (p.awaiting_you) return `${p.awaiting_you} waiting on you`;
  if (p.mergeable === 'CONFLICTING') return 'conflicted';
  if (p.checks === 'failing') return 'checks failing';
  if (p.checks === 'pending') return 'checks running';
  if (p.is_draft) return 'draft';
  return 'open';
}

/** A stopped session whose work sits on a PR is not waiting on you *here* — the
 *  next move is on the PR, and the PR's own state is the useful thing to show.
 *  A question or a permission prompt is still about this session, so those keep
 *  the amber and their own words. */
/** The PR a session's work belongs to, whether by branch or by its pass. */
export function prOf(s) {
  if (!s) return null;
  if (s.pass) {
    return (snap.prs || []).find((p) => p.number === s.pass.pr) || null;
  }
  return prForWorkspace(s.workspace);
}

export function handedToPr(s) {
  // `renderContext` asks this about `currentSession()`, which is null whenever
  // nothing is selected — the state the app opens in. Without this the context
  // bar threw on every render until you clicked a row.
  if (!s) return null;
  if (s.state.state !== 'your_turn') return null;
  const r = s.state.reason;
  if (r === 'asked_a_question' || r === 'needs_permission') return null;
  return prOf(s);
}

export let drawerTouched = false;

/* Collapsed to its header on purpose, remembered across reloads like the column
 * widths and the drawer height. Persisted so the next render (and the next boot)
 * does not silently reopen it — the whole point, now that ng-watch means main
 * always has a process and so the drawer is otherwise always open there. */
export let drawerCollapsed = localStorage.getItem('orch.drawerCollapsed') === '1';

const drawerListeners = [];
export function onDrawerChange(fn) { drawerListeners.push(fn); }

export function setDrawerCollapsed(v) {
  drawerCollapsed = v;
  try {
    localStorage.setItem('orch.drawerCollapsed', v ? '1' : '0');
  } catch (e) { /* private mode: the toggle still holds for this session */ }
  // Announced, not applied: redrawing the drawer and nudging xterm to refit are
  // the app's business, and reaching for them from here would make this layer
  // depend on the panes that sit on it.
  redrawDrawer();
}

/** Redraw the drawer now, on the same seam, without changing anything about it.
 *
 *  `newShell` needs it because the daemon notifies *before* it answers the POST
 *  (`spawn::spawn_shell`), so the render that would have picked the new shell has
 *  already been and gone by the time we know its id — and the next snapshot may
 *  be a poll away. Waiting for one is what made a new shell take the cursor
 *  sometimes and not others. */
function redrawDrawer() {
  for (const fn of drawerListeners) fn(drawerCollapsed);
}

/* The order you dragged the drawer's tabs into, per workspace, as a list of tab
   keys. A view preference like the column widths and the drawer height, so it
   lives beside them in `localStorage` rather than in the daemon: the order is
   yours, not the machine's, and the processes it describes do not outlive the
   daemon anyway. Keys are the caller's to choose — `app.js` uses a managed
   process's name, so `docker` keeps its place across a restart, and a shell's id,
   which is the only thing telling two of them apart. */
export let procOrder = (() => {
  try {
    return JSON.parse(localStorage.getItem('orch.procOrder') || '{}') || {};
  } catch (e) {
    return {};
  }
})();

export function setProcOrder(wsId, keys) {
  procOrder = { ...procOrder, [wsId]: keys };
  try {
    localStorage.setItem('orch.procOrder', JSON.stringify(procOrder));
  } catch (e) { /* private mode: the order still holds for this session */ }
}

/** Is the keyboard in a text box that is not a terminal?
 *
 *  The pty takes focus on its own in two places — a socket that has just opened,
 *  and a session you just picked — and neither is a gesture you made at that
 *  moment. Renaming a session in the rail is: the input is open, you are typing
 *  into it, and a terminal attaching underneath pulled the keyboard away and blurred
 *  the box, which commits the half-typed name.
 *
 *  xterm's own focus target is a `<textarea>`, so it has to be excluded by name or
 *  this would read "a terminal has focus" as "you are typing" and no session switch
 *  would ever move the cursor. */
export function typingElsewhere() {
  const a = document.activeElement;
  if (!a || a.classList.contains('xterm-helper-textarea')) return false;
  return a.tagName === 'INPUT' || a.tagName === 'TEXTAREA'
    || /** @type {HTMLElement} */ (a).isContentEditable;
}

/** A shell whose terminal should take the cursor as soon as it exists. */
export let pendingProcFocus = null;

/** A session the daemon has just been asked to create.
 *
 *  Setting `selected` alone is not enough: the terminal is only opened when a
 *  session is shown, and the snapshot handler skips that once something is
 *  already selected. */
export let pendingSelect = null;

/* Written from more than one module, and an imported binding is read-only, so the
 * writes come through here. The alternative — leaving the state in `app.js` and
 * letting modules reach back for it — is the coupling the modules exist to end. */
export function setPendingSelect(id) { pendingSelect = id; }
export function setPendingProcFocus(id) { pendingProcFocus = id; }
export function setDrawerTouched(v) { drawerTouched = v; }
export function setSelectedProc(wsId, procId) { selectedProc[wsId] = procId; }
