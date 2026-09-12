// The primitives every part of the SPA needs: the daemon's token, the two fetch
// wrappers, the DOM shorthands, and the snapshot itself.
//
// Extracted first because a module can only import from another module — a leaf
// like the review queue cannot be pulled out until the trunk it reaches for is
// importable. Everything here was already shared; the difference is that reaching
// for it now has to be written down.

import * as Palette from './palette.js';

/** @type {import("../snapshot").Snapshot} */
export let snap = /** @type {any} */ ({ workspaces: [], sessions: [] });

/* When the snapshot the current numbers came from landed. Durations are computed
 * server-side as the snapshot is built, so rendering them raw freezes the clock
 * between events: a session waiting on a permission prompt sat at "0s" until
 * something unrelated pushed a snapshot, then jumped to "1m". The rail redraws
 * every second; this is what makes those seconds mean anything. */
let snapAt = Date.now();

/** Every checkout's latest snapshot, keyed on its path.
 *
 *  The rail reads all of them; every other pane reads [`snap`], which is whichever
 *  one belongs to the checkout you are in. Private, because a reader that reached
 *  in here by path would be the second way of asking "which checkout" — and
 *  [`active`] is the first.
 *
 *  @type {Map<string, import("../snapshot").Snapshot>}
 */
const snaps = new Map();

/** One checkout's snapshot, for the rail, which composes all of them.
 *
 *  @param {string} path
 */
export function snapshotOf(path) {
  return snaps.get(path) ?? null;
}

/** What a checkout works on, as the lines a tooltip shows.
 *
 *  **The repository moved out of the top bar and onto the rail**, where the thing it
 *  describes already has a row. The strip used to carry it because it used to be a
 *  switcher; once the rail listed every open project, the line was a second copy of
 *  a name the rail was already showing, sitting where the app's own mark belongs.
 *
 *  Upstream and fork are both named rather than collapsed: PRs are opened against
 *  upstream while branches live on the fork, so one path cannot stand for the pair.
 *  The checkout's own path is last, because it is the answer to a different question
 *  — which folder — and that is the one this used to answer alone.
 *
 *  @param {Target} c
 */
export function repoSummary(c) {
  const repos = snapshotOf(c.path)?.repos;
  const lines = [];
  if (repos?.upstream) lines.push(repos.upstream);
  // Only when it is a different repository. A checkout with no fork layout reports
  // the same name twice, and "example/app / fork example/app" reads as a fork of
  // itself rather than as the ordinary case it is.
  if (repos?.fork && repos.fork !== repos.upstream) lines.push(`fork ${repos.fork}`);
  if (c.path) lines.push(c.path);
  return lines.join('\n');
}

/** Take a new snapshot for one checkout.
 *
 *  **The only writer of `snap`**, together with [`adopt`] which it calls: `snap`
 *  and the clock it is measured against have to move together or every duration
 *  freezes, and a second writer is how they come apart.
 *
 *  `snap` is a live binding — importers see this assignment without re-importing,
 *  which is what lets a hundred readers keep saying `snap.x`.
 *
 *  @param {Target} checkout
 *  @param {import("../snapshot").Snapshot} next
 */
export function receive(checkout, next) {
  snaps.set(checkout.path, next);
  adopt();
}

/** Point `snap` at the active checkout's snapshot.
 *
 *  Called whenever either half could have moved: a snapshot landed, or the
 *  selection moved to another checkout. Private, because every caller is in here:
 *  an exported one would be a second way to move `snap`, which is the thing this
 *  function exists to prevent. Idempotent, and cheap — a map read and two
 *  assignments.
 */
function adopt() {
  const next = snaps.get(activeCheckout().path);
  if (!next) return;
  snap = next;
  snapAt = Date.now();
}

const sinceSnap = (/** @type {number | null | undefined} */ ms) => (ms == null ? null : ms + (Date.now() - snapAt));

/** The PR whose head ref this workspace holds, if any. */
export function prForWorkspace(/** @type {string | null} */ wsId) {
  return (snap.prs || []).find((p) => p.workspace === wsId) || null;
}

/* Which session the centre pane is showing. Owned here because the rail picks it
 * and the terminals and the render both react — leaving the state in `app.js`
 * meant the rail had to reach back into the module that renders it. */
/** @type {string | null} */
export let selected = null;

/** @type {((id: string | null, auto: boolean) => void)[]} */
const selectionListeners = [];
export function onSelection(/** @type {(id: string | null, auto: boolean) => void} */ fn) { selectionListeners.push(fn); }

/** Pick a session. What *happens* next is whoever registered's business.
 *
 *  `auto` marks the pick the app made for you, which is what the snapshot does
 *  when the session you were on ends. A listener that reads that as a gesture is
 *  reacting to a session finishing, so anything standing down on "you went
 *  somewhere else" has to be able to tell the two apart. */
export function setSelected(/** @type {string | null} */ id, auto = false) {
  selected = id;
  // Picking a session is also saying which checkout you are in, which is what
  // holds the pane still when that session ends.
  if (id) lastCheckout = checkoutOf(id)?.path ?? lastCheckout;
  // The checkout is derived from this, so `snap` moves with it — before the
  // listeners run, since every one of them reads the snapshot to decide what the
  // new selection means.
  adopt();
  for (const fn of selectionListeners) fn(id, auto);
}

/** One checkout: what the host said about it, and how to reach its daemon.
 *
 *  The host's own `Checkout` plus the two strings a fetch needs. Named rather
 *  than three loose globals because there is one of these per checkout, and every
 *  call has to say which one it is for — `call` and `get` are the shorthand for
 *  "the active one" and nothing else may assume there is only one.
 *
 *  @typedef {import("../serve").Checkout & { base: string, wsBase: string }} Target
 */

/** Anything a fetch can be aimed at: a checkout's daemon, or the host.
 *
 *  Narrower than [`Target`] on purpose. The host is not a checkout — it has no
 *  path, no repository and no daemon — so typing it as one would be a lie the
 *  checker then enforces everywhere.
 *
 *  @typedef {{ base: string, token: string }} Endpoint
 */

/** Where a checkout's daemon answers.
 *
 *  `base` is empty for a daemon on the page's own origin, not `location.origin`,
 *  so its fetches stay relative and a request cannot be sent to a spelling of this
 *  origin the guard would refuse: `api::guard` matches the `Host` header against
 *  `127.0.0.1:<port>` or `localhost:<port>` exactly, and those two are not
 *  interchangeable. That is the solo `orchd` case, where one process serves the
 *  page and manages the checkout.
 *
 *  A different port is the host-and-child shape: the page comes from the host and
 *  every call is aimed at the daemon that manages *that* checkout. The Host header
 *  then names the child's port, which is what the child's guard wants, and the
 *  Origin is the host's — the one extra string the child accepts, handed to it on
 *  its argv.
 *
 *  @param {import("../serve").Checkout} c
 *  @returns {Target}
 */
function reachable(c) {
  const sameOrigin = String(c.port) === location.port;
  const authority = sameOrigin ? location.host : `127.0.0.1:${c.port}`;
  return { ...c, base: sameOrigin ? '' : `http://${authority}`, wsBase: `ws://${authority}` };
}

/** Every open checkout, as the host substituted them into the page.
 *
 *  Substituted rather than fetched, because a page cannot ask for a token it has
 *  not been given — the same reason `GET /` has never been token-gated.
 *
 *  **A `let`, because the set changes while the page is open.** An add, a close
 *  and a daemon that died are all the host telling the page something the
 *  substitution could not know, and a page that reloaded to learn it would take
 *  every terminal down with it.
 *
 *  @type {Target[]}
 */
export let CHECKOUTS = (window.__ORCH__.checkouts ?? []).map(reachable);

/** Replace the checkout list with what the host now says.
 *
 *  One writer, so a row and the socket aimed at it cannot disagree. The caller
 *  reconciles the sockets; this only moves the list.
 *
 *  @param {import("../serve").Checkout[]} next
 */
export function setCheckouts(next) {
  CHECKOUTS = next.map(reachable);
}

/** The review-preview page's fallback target.
 *
 *  That page is served by the host with no checkout list, and it reaches the
 *  daemon for a diff. One entry on this origin with the page's own token, which is
 *  the host's — the same value a solo `orchd` gives both.
 *
 *  @type {Target}
 */
const PAGE_ONLY = {
  path: '', name: '', port: Number(location.port) || 0, live: true, repo: null, clash: null,
  base: '', wsBase: `ws://${location.host}`, token: window.__ORCH__.token,
};

/** The host, which is whatever served this page.
 *
 * **Not a checkout, and that is the distinction.** A daemon manages one checkout;
 * the host owns the window and knows which checkouts are open. So `/api/window/*`
 * and `/api/host/*` belong here and everything else belongs to a checkout.
 *
 * Always relative, because the page came from here. The token is the page's own —
 * `CHECKOUTS[0].token` is a *child's*, and a child's guard would refuse it.
 *
 * This existing is a bug fix, not a tidy-up: `call` aims at [`LOCAL`], so under the
 * app every titlebar button reached the child daemon, whose catch-all answers
 * `200 {}`. Minimise, maximise, close, drag, resize and restart all silently did
 * nothing, and nothing failed. That is the exact trap `CLAUDE.md` names — a
 * misrouted call reads as an empty daemon.
 *
 *  @type {Endpoint & { wsBase: string }}
 */
export const HOST = {
  base: '',
  wsBase: `ws://${location.host}`,
  token: window.__ORCH__.token,
};

/** POST to the host. */
export const callHost = (/** @type {string} */ path, /** @type {any} */ body) => callOn(HOST, path, body);

/** GET from the host. */
export const getHost = (/** @type {string} */ path) => getOn(HOST, path);

/** The checkout everything that is not the rail follows.
 *
 *  **Derived from the selection, never written.** A session belongs to a workspace
 *  belongs to a checkout, so which checkout you are in is a *fact about what you
 *  have selected* — a second variable saying so is a second source of truth, and
 *  the one that goes stale is whichever the next reader forgets to update. With
 *  nothing selected it is the first checkout, which is the only checkout on a
 *  single-checkout install.
 *
 *  @returns {Target}
 */
export function activeCheckout() {
  return (selected && checkoutOf(selected))
    || CHECKOUTS.find((c) => c.path === lastCheckout)
    || CHECKOUTS[0]
    || PAGE_ONLY;
}

/* Where you were, for when nothing is selected.
 *
 * **A fallback, not a second answer.** A selection always outranks it, and it is
 * only ever written to the checkout of the session you just selected — so it is
 * "the checkout you were last in" rather than a variable anyone sets to mean
 * something else. Without it, stepping into a checkout with no sessions puts you
 * back in the first one, because the derivation has nothing to derive from. */
/** @type {string | null} */
let lastCheckout = null;

/* Which band each checkout wears, by path.
 *
 * **Assigned on first sight and kept**, rather than computed from the position in
 * the list. Closing a checkout would otherwise re-colour every one after it, which
 * is the colour changing to mean something that did not happen. Assigning in order
 * also makes collisions impossible up to the palette length, where a hash would
 * need a probe to say the same thing.
 *
 * There are four; past that a checkout gets no band rather than a repeat, because
 * two blocks sharing a colour is worse than one having none. */
const bands = new Map();
const BANDS = 4;

/** The band number for a checkout, or `null` past the palette.
 *
 *  @param {string} path
 */
export function bandOf(path) {
  if (!bands.has(path)) bands.set(path, bands.size);
  const n = bands.get(path);
  return n < BANDS ? n + 1 : null;
}

/** Go to a checkout, carrying a selection with you.
 *
 *  **"Activate that checkout" has to mean "select something in it"**, because the
 *  checkout is derived from the selection and nothing else. Landing on the newest
 *  live session, then the newest conversation, then nothing — and the remembered
 *  path is what holds you there in the last case, where there is nothing to
 *  derive from.
 *
 *  @param {Target} c
 *  @returns {boolean} whether anything was selected
 */
export function enterCheckout(c) {
  // Before the selection moves, so a checkout with nothing in it still becomes
  // the one you are in.
  lastCheckout = c.path;
  const sessions = (snaps.get(c.path)?.sessions ?? []).slice().sort(byNewest);
  const landing = sessions.find((s) => !isArchived(s)) || sessions[0];
  setSelected(landing ? landing.id : null);
  return !!landing;
}

/** Every checkout's sessions, as `{ checkout, session }` pairs.
 *
 *  **What "everything running" means once there is more than one checkout.** The
 *  waitbar, `MOD+Space` and `Ctrl+Tab` all answer questions about attention, and
 *  attention does not stop at the checkout you happen to be looking at — a bar
 *  reading "2 need you" while the chord it advertises answers "nothing waiting on
 *  you" is the two disagreeing about the same fact.
 *
 *  @returns {{ checkout: Target, session: any }[]}
 */
export function everySession() {
  return CHECKOUTS.flatMap((c) =>
    (snaps.get(c.path)?.sessions ?? []).map((session) => ({ checkout: c, session })));
}

/** Which checkout holds a session, by searching every snapshot.
 *
 *  By search rather than by a map kept beside the sessions, because the snapshots
 *  are the only record of what exists and a second index is a second thing to
 *  invalidate. There are at most a handful of checkouts and the rail already walks
 *  all of them every second.
 *
 *  @param {string} id
 *  @returns {Target | null}
 */
export function checkoutOf(id) {
  for (const c of CHECKOUTS) {
    if ((snaps.get(c.path)?.sessions ?? []).some((s) => s.id === id)) return c;
  }
  return null;
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
/** @type {Record<string, number>} */
const marks = {};

/** Record a boot milestone, the first time it happens.
 *
 *  First only: `attach` and `paint` repeat every time a session is switched, and
 *  a later one is not boot. */
export function mark(/** @type {string} */ what) {
  if (marks[what] == null) marks[what] = Math.round(performance.now());
}
mark('scripts');

let reported = false;
/** @type {ReturnType<typeof setTimeout> | null} */
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
export function note(/** @type {string} */ text) {
  call('/api/client/note', { note: text }).catch(() => {});
}

export function reportBoot() {
  if (reported) return;
  clearTimeout(reportTimer ?? undefined);
  reportTimer = setTimeout(() => {
    reported = true;
    // Failure is silence. This is a diagnostic, and a toast about it would be
    // the app complaining to the user on the user's behalf.
    call('/api/client/timing', { marks }).catch(() => {});
  }, 1500);
}

/** The element with this id, which the page is expected to have.
 *
 *  **It throws rather than returning `null`**, and that is what lets the other
 *  161 call sites read `.hidden` and `.replaceChildren()` without a guard each.
 *  Every id `$` is asked for is in `index.html`, which is `include_str!`d into
 *  the same binary as this file — so a miss is not a condition to handle, it is
 *  the page and the code having gone out of step, and the throw says so at the
 *  call instead of surfacing three lines later as "cannot read properties of
 *  null". Under `strictNullChecks` the alternative was 300 guards that can never
 *  run. */
/** What went wrong, as a sentence, from whatever was thrown.
 *
 *  `catch (e)` hands you `unknown`, and that is not pedantry: a `throw 'nope'`,
 *  a `DOMException` or a rejected fetch with no `message` all reach these
 *  handlers, and `e.message` on one of them puts the word "undefined" in a
 *  toast. One helper, so the 46 catch blocks that all said `e.message` say the
 *  same thing and say it correctly.
 *
 *  @param {unknown} e
 */
export const reason = (e) => (e instanceof Error ? e.message : String(e));

export const $ = (/** @type {string} */ id) => {
  const found = document.getElementById(id);
  if (!found) throw new Error(`no element #${id} — index.html and the code disagree`);
  return found;
};

/** `$` for a form control, where the caller wants `.value` or `.disabled`.
 *
 *  `getElementById` can only promise `HTMLElement`, so every read of `.value`
 *  through `$` is a type error even when the id certainly names an `<input>`.
 *  Deliberately untyped rather than a union of input/button/select: TypeScript
 *  reduces that intersection to `never`, and a union only offers what all three
 *  share. So this is one named escape hatch for controls — `$` stays typed, and
 *  everything fetched through it keeps being checked. */
export const ctl = (/** @type {string} */ id) => /** @type {any} */ (document.getElementById(id));

/** `document.createElement` with the three things every call here sets.
 *
 *  Generic on the tag so `el('input')` is an `HTMLInputElement` and its `.value`
 *  type-checks: a plain `HTMLElement` return would send every form control in the
 *  app through `ctl`, which is the deliberate `any` and should stay rare.
 *
 *  @template {keyof HTMLElementTagNameMap} K
 *  @param {K} tag
 *  @param {string | null} [cls]
 *  @param {string} [text]
 *  @param {string} [title]
 *  @returns {HTMLElementTagNameMap[K]}
 */
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
export function clock(/** @type {string} */ cls, /** @type {number | null | undefined} */ ms, suffix = '', prefix = '') {
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
  // `ms` is non-null here — the guard above returned — so the fallback never runs.
  const started = Date.now() - (sinceSnap(ms) ?? 0);
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
/** @type {HTMLElement | null} */
let toastReturn = null;

/** In use: the pointer is in the row, or it holds a selection nobody has copied.
 *  7 seconds is not enough to read a refusal, aim at it and drag across it, so
 *  the clock does not run while you are working in the row. */
function toastHeld(/** @type {HTMLElement} */ row) {
  if (row.matches(':hover')) return true;
  const sel = window.getSelection();
  return !!sel && !sel.isCollapsed && !!sel.anchorNode && row.contains(sel.anchorNode);
}

function dismissToast(/** @type {HTMLElement} */ row) {
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
function armToast(/** @type {HTMLElement} */ row, /** @type {number} */ ms) {
  clearTimeout(toastTimers.get(row));
  // Re-checked on a short beat, not on pointerleave: a selection left alone has
  // to keep the text up too, and there is no event for "still selected".
  toastTimers.set(row, setTimeout(() => {
    if (toastHeld(row)) return armToast(row, 1200);
    dismissToast(row);
  }, ms));
}

export function toast(/** @type {string} */ message, /** @type {boolean | undefined} */ bad) {
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
    row.addEventListener('pointerdown', (/** @type {PointerEvent} */ e) => {
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
/** @type {((answer: any) => void) | null} */
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
/** @type {string | null} */
let dlgAsking = null;
/** @type {Promise<any> | null} */
let dlgPending = null;

/** Take the dialog down and answer whoever is waiting. */
function dlgClose(/** @type {any} */ answer) {
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
//
// The types are spelled out because a default of `null` infers the type `null`,
// which is what refused `cancelValue: false` and `focus: <input>` the moment
// `strictNullChecks` came on.
/**
 *  @param {string} message
 *  @param {{ ok: string, danger?: boolean, answer: () => any,
 *            body?: HTMLElement | null, focus?: HTMLElement | null,
 *            cancel?: string, cancelValue?: any }} opts
 */
function dlgOpen(message, {
  ok, danger, answer, body = null, focus = null, cancel = 'Cancel', cancelValue = null,
}) {
  // Non-null whenever `dlgSettle` is: the two are set and cleared together.
  if (dlgSettle && dlgAsking === message) return /** @type {Promise<any>} */ (dlgPending);
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
  const no = el('button', 'dlgbtn', cancel);
  no.onclick = () => dlgClose(cancelValue);
  const go = el('button', 'dlgbtn go' + (danger ? ' danger' : ''), ok || 'OK');
  go.onclick = () => dlgClose(answer());
  // Cancel first, so Tab reaches the safe one before the destructive one and the
  // row still reads left to right in the order everything else puts them.
  foot.appendChild(no);
  foot.appendChild(go);
  card.appendChild(foot);
  host.appendChild(card);
  host.hidden = false;

  card.onkeydown = (/** @type {KeyboardEvent} */ ev) => {
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
export function confirmBox(/** @type {string} */ message, { ok = 'Yes', danger = true } = {}) {
  return dlgOpen(message, { ok, danger, answer: () => true })
    .then((/** @type {any} */ a) => a === true);
}

/** A question with two *actions* rather than a yes and a refusal.
 *
 *  Three outcomes, and the third is why this is not [`confirmBox`]: `true` for the
 *  primary button, `false` for the other one, and `null` for `Esc` — which means
 *  "I did not mean to be asked this", not either answer. A two-outcome box would
 *  make dismissing the dialog silently pick one of the two actions.
 *
 *  @param {string} message
 *  @param {{ ok: string, other: string }} labels
 */
export function chooseBox(message, { ok, other }) {
  return dlgOpen(message, {
    ok,
    danger: false,
    cancel: other,
    cancelValue: false,
    answer: () => true,
  }).then((/** @type {any} */ a) => (a === null ? null : a === true));
}

/** `window.prompt`, drawn by the app. Resolves the text, or null if cancelled.
 *
 *  Blank resolves as the empty string rather than null, because one caller means
 *  something by it: naming a worktree blank is "let Claude name it". Callers that
 *  need words check for them. */
export function promptBox(/** @type {string} */ message, { value = '', placeholder = '', ok = 'OK' } = {}) {
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
  }).then((/** @type {any} */ a) => (a === null ? null : String(a)));
}

/** POST to one checkout's daemon.
 *
 *  @param {Endpoint} c
 *  @param {string} path
 *  @param {unknown} [body]
 */
export async function callOn(c, path, body) {
  const res = await fetch(c.base + path, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-orch-token': c.token },
    body: JSON.stringify(body ?? {}),
  });
  const json = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(json.error || res.statusText);
  return json;
}

/** GET from one checkout's daemon.
 *
 *  @param {Endpoint} c
 *  @param {string} path
 */
export async function getOn(c, path) {
  const res = await fetch(c.base + path, { headers: { 'x-orch-token': c.token } });
  const json = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(json.error || res.statusText);
  return json;
}

/* The two every caller uses, aimed at the checkout this page came from.
   `callOn`/`getOn` exist for the one that is aimed somewhere else, and keeping the
   short pair means ~90 call sites do not have to say which checkout they meant when
   there is only ever one answer. */
/** POST to the checkout a session belongs to.
 *
 *  **Derived from the session, not from what you are looking at.** A rail listing
 *  several checkouts can act on a row in any of them — a kill, a rename, a fork —
 *  and `call` would send every one of those to whichever daemon happens to hold
 *  the selection. Falls back to the active checkout for a session no snapshot has
 *  yet, which is the moment between a create and the snapshot that carries it.
 *
 *  @param {string} id a session id
 *  @param {string} path
 *  @param {unknown} [body]
 */
export const callFor = (id, path, body) => callOn(checkoutOf(id) ?? activeCheckout(), path, body);

/** The snapshot a session lives in. See [`callFor`] for why it is derived.
 *
 *  @param {string} id a session id
 */
export const snapshotFor = (id) => snapshotOf((checkoutOf(id) ?? activeCheckout()).path) ?? snap;

/* The shorthand for the checkout you are in. Every other call names its target,
   because "the active one" is only ever right for the panes that follow the
   selection — the rail does not. */
export const call = (/** @type {string} */ path, /** @type {any} */ body) => callOn(activeCheckout(), path, body);
export const get = (/** @type {string} */ path) => getOn(activeCheckout(), path);

function duration(/** @type {number | null | undefined} */ ms) {
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
export function compactAge(/** @type {number | null | undefined} */ hours) {
  // `null < 1` was already true, so an absent age has always read `now`.
  if (hours == null || hours < 1) return 'now';
  if (hours < 48) return `${Math.round(hours)}h`;
  return `${Math.round(hours / 24)}d`;
}

// The poll counter each pane captured when its refresh was pressed; the button
// spins until the live counter moves past it. null = not spinning.
/** @type {Record<string, number | null>} */
const spinFloor = { pr: null, review: null };

/** Give a `role="button"` span what a real <button> has for free: a tab stop and
 *  Enter/Space activation. Without this a span-button is mouse-only, which is a
 *  keyboard trap for the refresh icons and the update-nudge dismiss. */
export function keyActivate(/** @type {HTMLElement} */ el) {
  el.tabIndex = 0;
  // Property assignment, not addEventListener: renderUpdate re-wires #updatex on
  // every snapshot, and a stacked listener would fire click N times.
  el.onkeydown = (/** @type {KeyboardEvent} */ e) => {
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
function paintSig(/** @type {any} */ value, /** @type {string[]} */ drop = []) {
  return JSON.stringify(value, (k, v) => (k.endsWith('_ms') || drop.includes(k) ? undefined : v));
}

/** True when `value` renders the same as it did last time this box was asked.
 *
 *  The box is the pane's own `{ sig: null }`: five panes were each carrying a
 *  module-level `let xSig`, the same three lines of compare-and-remember, and the
 *  same comment with one noun changed. One name for the idiom means a reader
 *  confirms it once. */
export function unchanged(/** @type {{ sig: string | null | undefined }} */ box, /** @type {any} */ value, /** @type {string[]} */ drop = []) {
  const sig = paintSig(value, drop);
  if (box.sig === sig) return true;
  box.sig = sig;
  return false;
}

export function refreshButton(/** @type {'pr' | 'review'} */ kind, /** @type {number} */ pollCount, /** @type {string} */ endpoint, /** @type {boolean} */ polling) {
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
/** @type {((scale: number) => void)[]} */
const scaleListeners = [];
export function onScaleChange(/** @type {(scale: number) => void} */ fn) { scaleListeners.push(fn); }

const FS_BASE = 1.155;
export const ZOOM = { key: 'orch.uiZoom', def: 1, min: 0.8, max: 1.5, step: 0.05 };

/** The body's own font size in px at a given scale, which is what the pane shows.
 *
 *  **Pixels rather than a percentage, so the three sizes are one question asked
 *  three times.** The stored value is still a scale — it has to be, because `--fs`
 *  multiplies every font size in the sheet and the terminal reads it as a ratio —
 *  but "115%" answers a different question from "12px" and the pane was asking both
 *  under one heading. 13px is the body's declared size before `--fs`, so the default
 *  reads as 15px, which is the size a ruler would give you.
 */
const UI_PX_AT = (/** @type {number} */ z) => Math.round(13 * FS_BASE * z);
export const uiPx = () => UI_PX_AT(zoomScale);
export const UI_PX_MIN = UI_PX_AT(ZOOM.min);
export const UI_PX_MAX = UI_PX_AT(ZOOM.max);

/** Set the interface size by the px the pane shows. Rounds back to a scale. */
export const setUiPx = (/** @type {number} */ px) => setZoom(px / (13 * FS_BASE));

/** The user-facing scale, where 1 is the default. */
export let zoomScale = ZOOM.def;

/** The multiplier the stylesheet and the terminal both read. */
export const uiScale = () =>
  Number(getComputedStyle(document.documentElement).getPropertyValue('--fs')) || FS_BASE;

export function setZoom(/** @type {number} */ z) {
  const next = Math.min(ZOOM.max, Math.max(ZOOM.min, Math.round(z * 100) / 100));
  zoomScale = next;
  document.documentElement.style.setProperty('--fs', String(next * FS_BASE));
  // Written here as well as by the settings renderer, because the chords change the
  // scale with the pane open and a readout that only the buttons update is a readout
  // that disagrees with the board.
  $('fsval').textContent = `${UI_PX_AT(next)}px`;
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

/* Three colours, three fonts and two numbers, in `localStorage` beside the zoom
 * and the column widths. Nothing about which colours you like belongs in
 * `config.json`, where a daemon that never reads it would have to carry it.
 *
 * The *arithmetic* is in `palette.js`, which is pure so `mise run check-web` can
 * import it in node and assert the default reproduces `app.css`'s `:root`. What
 * lives here is everything that touches the page: reading the store, refusing a
 * pair that cannot be read, writing the custom properties, and telling the
 * terminals. */

const THEME = { key: 'orch.theme' };

/** The vendored families, which are the only ones certain to be there.
 *
 *  `label` is what the dropdown shows and `stack` is what the token becomes. The
 *  `system` entry is the generic rather than a name: macOS does not expose its
 *  system faces by name — `ui-monospace` answers where `SF Mono` does not — so
 *  asking for the name would come back absent on the one platform that has it.
 */
export const FONTS = {
  plex: { label: 'IBM Plex Mono', stack: "'IBM Plex Mono',ui-monospace,monospace", mono: true },
  jetbrains: { label: 'JetBrains Mono', stack: "'JetBrains Mono',ui-monospace,monospace", mono: true },
  martian: { label: 'Martian Mono', stack: "'Martian Mono',ui-monospace,monospace", mono: true },
  system: { label: 'System monospace', stack: 'ui-monospace,monospace', mono: true },
  plexsans: { label: 'IBM Plex Sans', stack: "'IBM Plex Sans',system-ui,sans-serif", mono: false },
  sans: { label: 'System sans', stack: 'system-ui,sans-serif', mono: false },
};

/** What a fresh install gets: the palette in `app.css`, and the fonts it names. */
/* Declared above `theme` on purpose: `loadTheme` reads it, and a `const` is in its
   temporal dead zone until the line that defines it runs. With this below, the
   whole module threw on import — so the page loaded its markup and no behaviour at
   all, which looks like a dead board rather than an error. */
/** @typedef {'ui' | 'mono' | 'code'} Role */

const THEME_DEF = {
  ...Palette.DEFAULT,
  ui: 'plexsans',
  mono: 'plex',
  code: 'jetbrains',
  /** Terminal font size before the interface scale multiplies it — `term.js`'s old
   *  constant. */
  termSize: 12,
  /** Diff and code font size, the same way. 12 rather than the 11.5 the stylesheet
   *  used to hard-code: a size control has to show a whole number, and half a pixel
   *  is not a size anyone chose. */
  diffSize: 12,
  /** 1 is opaque. Floored well above zero: a board you cannot read is the problem
   *  transparency causes rather than the effect it is for. */
  opacity: 1,
};

/** Whole palettes, because one colour at a time cannot get you from dark to light.
 *
 *  **This is not a convenience.** Each change is judged against the other two, so
 *  walking a dark theme toward a light one is refused at every step: a white ground
 *  under light text is unreadable, and so is dark text on a dark ground. A preset
 *  moves all three at once, which is the only path between them — and it is how
 *  anybody switches anyway.
 *
 *  `orchd` is the palette the app ships with, so "Reset" is a real answer rather
 *  than something that resembles it; `check-palette.mjs` asserts that.
 */
export const PRESETS = {
  orchd: { label: 'orchd', ...Palette.DEFAULT },
  paper: { label: 'Paper', bg: '#F4F2ED', panel: '#EAE7E0', text: '#26231E' },
  contrast: { label: 'High contrast', bg: '#000000', panel: '#0C0C0C', text: '#FFFFFF' },
};

/** Which preset the current colours are, or `null` for a hand-tuned set.
 *
 *  Derived rather than stored, so a theme edited back to a preset's exact colours
 *  reads as that preset again. Compared lowercase: an `input[type=color]` always
 *  reports lowercase, and the constants above are written the way a person writes
 *  them.
 */
export function currentPreset() {
  const same = (/** @type {string} */ a, /** @type {string} */ b) => a.toLowerCase() === b.toLowerCase();
  const roles = /** @type {const} */ (['bg', 'panel', 'text']);
  return Object.keys(PRESETS).find((k) => roles
    .every((role) => same(PRESETS[/** @type {keyof typeof PRESETS} */ (k)][role], theme[role]))) ?? null;
}

/* Monospace and sans families worth *asking* about.
 *
 * **A list, because a page cannot enumerate installed fonts here.**
 * `queryLocalFonts()` is the API for that, and it is Chromium-only behind a
 * permission prompt — absent from WebKit, so absent from WKWebView on macOS and
 * from WebKitGTK on Linux, which is every window this app opens. Asking whether
 * one named family resolves does work everywhere.
 *
 * Notably **not** `SF Mono`: macOS does not expose its system faces by name, and
 * the generic answers where the name does not — which is why `system` is in
 * [`FONTS`] as `ui-monospace` rather than as a name that comes back absent. */
const MONO_CANDIDATES = [
  'Berkeley Mono', 'Cascadia Code', 'Cascadia Mono', 'Comic Mono', 'Consolas',
  'Courier New', 'DejaVu Sans Mono', 'Fira Code', 'Fira Mono', 'Geist Mono',
  'Hack', 'Iosevka', 'Inconsolata', 'Liberation Mono', 'Menlo', 'Monaco',
  'MonoLisa', 'Noto Sans Mono', 'Roboto Mono', 'Source Code Pro',
  'SF Mono Powerline', 'Ubuntu Mono', 'Victor Mono', 'Zed Mono',
];
const SANS_CANDIDATES = [
  'Arial', 'Avenir Next', 'DejaVu Sans', 'Helvetica Neue', 'Inter', 'Lato',
  'Noto Sans', 'Open Sans', 'Roboto', 'Segoe UI', 'Source Sans 3', 'Ubuntu',
];

/** Whether asking for `name` gets you anything other than the default face.
 *
 *  **It cannot tell an installed family from an aliased one, and that is a real
 *  limit rather than a bug to fix.** fontconfig — WebKitGTK, the Linux target —
 *  substitutes by design: `Courier New` resolves to Liberation Mono on a machine
 *  that has never had it, and nothing the page can measure sees the difference.
 *  The branch this comes from probed against three generics and called agreement
 *  proof, which is a Chrome-shaped assumption: under fontconfig all three agree
 *  *because* the alias resolves the same way regardless of what follows it.
 *
 *  So this asks the narrower question it can actually answer — does this name
 *  resolve to something other than the fallback — against a family that certainly
 *  does not exist. One comparison rather than three, and immune to the agreement
 *  trap. **The preview beside each control is what makes the remaining error
 *  harmless**: you see the face you will get before you keep it.
 */
function resolves(/** @type {string} */ name, /** @type {CanvasRenderingContext2D} */ ctx) {
  const NOTHING = '__orchd_no_such_family__';
  const sample = 'MWil10O—mmmiii';
  const width = (/** @type {string} */ family) => {
    ctx.font = `48px ${family}`;
    return ctx.measureText(sample).width;
  };
  return width(`'${NOTHING}'`) !== width(`'${name}','${NOTHING}'`);
}

/** The candidate families that resolve here, by role. Measured once, lazily.
 *
 *  **Not at module scope.** Two lists of measurements on the boot path lengthens
 *  the near-black window before the first paint, for a list nothing reads until
 *  somebody opens the settings pane. The branch this comes from did it at import.
 */
/** @type {{ mono: string[], sans: string[] } | null} */
let detected = null;
export function detectedFonts() {
  if (detected) return detected;
  const ctx = document.createElement('canvas').getContext('2d');
  if (!ctx) return { mono: [], sans: [] };
  const shipped = new Set(Object.values(FONTS).map((f) => f.label));
  const find = (/** @type {string[]} */ names) => names.filter((/** @type {string} */ n) => !shipped.has(n) && resolves(n, ctx));
  detected = { mono: find(MONO_CANDIDATES), sans: find(SANS_CANDIDATES) };
  return detected;
}

/** The theme as it stands. Replaced whole by [`setTheme`], never mutated. */
export let theme = loadTheme();

/** @type {((theme: Theme) => void)[]} */
const themeListeners = [];
/** Register for theme changes. The terminals are the one consumer that cannot
 *  read a CSS custom property — xterm takes hex strings — so they are told. */
export function onThemeChange(/** @type {(theme: Theme) => void} */ fn) { themeListeners.push(fn); }

/** Read the stored theme, keeping only what is valid.
 *
 *  **Field by field, and normalised on the way in.** A stored `"D2D2D2"` passes a
 *  tolerant hex test and is then not a colour: written to a custom property it
 *  kills every rule that reads it, and the colour well shows `#000000` while the
 *  board says otherwise. So what comes back is what `parseHex` accepted, spelled
 *  `#rrggbb`.
 *
 *  A font key is checked with `Object.hasOwn`, not `FONTS[key]` — `"constructor"`
 *  and `"toString"` pass the latter, and the token then becomes the literal string
 *  `undefined`.
 */
/** The board's theme: three colours, three font families, two sizes and the
 *  window opacity. Spelled out because the settings pane indexes it by a role
 *  name, and a checker with no shape to index cannot tell `theme.termSize` from
 *  a typo.
 *
 *  @typedef {{ bg: string, panel: string, text: string,
 *              ui: string, mono: string, code: string,
 *              termSize: number, diffSize: number, opacity: number }} Theme
 */

/** @returns {Theme} */
function loadTheme() {
  /** @type {Record<string, unknown>} */
  let got = {};
  try {
    got = JSON.parse(localStorage.getItem(THEME.key) || '{}') || {};
  } catch (e) {
    got = {};
  }
  const hex = (/** @type {unknown} */ v, /** @type {string} */ fallback) => {
    const rgb = Palette.parseHex(/** @type {string | null | undefined} */ (v));
    return rgb ? Palette.toHex(rgb) : fallback;
  };
  const family = (/** @type {unknown} */ v, /** @type {string} */ fallback) =>
    (typeof v === 'string' && (v.startsWith('custom:') || Object.hasOwn(FONTS, v)) ? v : fallback);
  const next = {
    bg: hex(got.bg, THEME_DEF.bg),
    panel: hex(got.panel, THEME_DEF.panel),
    text: hex(got.text, THEME_DEF.text),
    ui: family(got.ui, THEME_DEF.ui),
    mono: family(got.mono, THEME_DEF.mono),
    code: family(got.code, THEME_DEF.code),
    termSize: clampSize(got.termSize, THEME_DEF.termSize),
    diffSize: clampSize(got.diffSize, THEME_DEF.diffSize),
    opacity: clampOpacity(got.opacity),
  };
  /* A pair that cannot be read never reaches the page, however it got into the
     store — a hand edit, or a build that once allowed it. Falling back to the
     default is the only recovery that does not need a readable settings pane to
     reach. */
  return Palette.legible(next) ? next : { ...next, ...Palette.DEFAULT };
}

/** 8 to 24 px, and not `NaN`. The floor is where a terminal stops being one. */
export const SIZE_MIN = 8;
export const SIZE_MAX = 24;
function clampSize(/** @type {unknown} */ v, /** @type {number} */ def) {
  const n = Number(v);
  return Number.isFinite(n) ? Math.min(SIZE_MAX, Math.max(SIZE_MIN, Math.round(n))) : def;
}

/** 0.35 to 1. Floored well above zero for the reason `THEME_DEF.opacity` gives. */
function clampOpacity(/** @type {unknown} */ v) {
  const n = Number(v);
  if (!Number.isFinite(n)) return 1;
  return Math.min(1, Math.max(0.35, Math.round(n * 100) / 100));
}

/** Whether a family name is one this app will put in a declaration.
 *
 *  **Refused rather than mangled**, and one spelling so the pane and the stack
 *  cannot disagree about what is allowed. Deleting the characters that could break
 *  a declaration corrupts legitimate names, and what survives can still inject a
 *  second family — `Comic, monospace` is two. Letters, digits, spaces, dots and
 *  hyphens cover every real family name and nothing that can end a declaration.
 */
export const validFontName = (/** @type {string | null | undefined} */ name) => /^[\w .-]{1,64}$/.test(name ?? '');

/** The CSS stack for one role, or the vendored default if the key is unknown. */
export function fontStack(/** @type {Role} */ role) {
  const key = theme[role];
  if (typeof key === 'string' && key.startsWith('custom:')) {
    /* A name that does not pass falls back to the vendored stack rather than
       being cleaned up — see `validFontName`. The pane refuses it before it gets
       here; this is the second line of defence for a hand-edited store. */
    const name = key.slice('custom:'.length);
    if (validFontName(name)) {
      const generic = role === 'ui' ? 'system-ui,sans-serif' : 'ui-monospace,monospace';
      return `'${name}',${generic}`;
    }
  }
  return (FONTS[/** @type {keyof typeof FONTS} */ (key)]
    || FONTS[/** @type {keyof typeof FONTS} */ (THEME_DEF[role])]).stack;
}

/** Write the theme to the page.
 *
 *  Everything derived goes on `documentElement` as a custom property, so the
 *  stylesheet keeps saying `var(--line)` and knows nothing about themes. The
 *  semantic colours are set here too — with their hue kept and their luminance
 *  lifted only where the ground would swallow them; `Palette.readable` has why.
 */
function applyTheme() {
  const root = document.documentElement;
  for (const [name, value] of Object.entries(Palette.tokens(theme, { opacity: theme.opacity }))) {
    root.style.setProperty(name, value);
  }
  for (const [name, hue] of Object.entries(SIGNALS)) {
    root.style.setProperty(name, Palette.readable(hue, theme));
  }
  root.style.setProperty('--sans', fontStack('ui'));
  root.style.setProperty('--label', fontStack('ui'));
  root.style.setProperty('--mono', fontStack('mono'));
  root.style.setProperty('--code', fontStack('code'));
  /* The diff's own size, before `--fs` multiplies it — the stylesheet does that
     multiplication, so the three code blocks that share this size keep sharing it. */
  root.style.setProperty('--code-px', `${theme.diffSize}px`);
  /* **What the engine paints a `<select>`, a scrollbar and a range track.** Those
     are the browser's own widgets, and without this it draws them for a light page
     whatever the stylesheet says — so the settings pane's dropdowns came up white on
     a black board under WebKitGTK, along with the list each one opens, which no CSS
     of ours can reach at all. Read from the palette rather than stored: a theme
     whose ground is darker than its text is a dark theme, and that is true of a
     preset and of a hand-edited pair alike. */
  root.style.colorScheme =
    Palette.luminance(theme.bg) < Palette.luminance(theme.text) ? 'dark' : 'light';
  for (const fn of themeListeners) fn(theme);
}

/* The colours that mean something, with their shipped hues.
 *
 * Out of `tokens()` on purpose: these are the legend three panes read, and a
 * theme that could set amber to grey would be turning a signal off rather than
 * restyling it. Their *luminance* still follows the theme — see `Palette.readable`
 * for the light-ground failure that forced it. */
const SIGNALS = {
  '--attn': '#E0A244',
  '--work': '#4C9AAF',
  '--ok': '#5FA97C',
  '--bad': '#D4726B',
  '--auto': '#5B8FC9',
  '--focus': '#C9C9C9',
};

/** Change part of the theme, or refuse.
 *
 *  Returns `null` on success and a sentence on refusal, so the pane can say why
 *  rather than snapping a control back with no explanation.
 *
 *  **A pair below the floor is refused, not corrected.** A board is the colours
 *  you picked, and quietly moving them is the worse answer — and the refusal is
 *  what keeps the way back reachable, since a theme that made the settings pane
 *  invisible could only be undone by clearing browser storage.
 */
export function setTheme(/** @type {Partial<Theme>} */ patch) {
  const next = { ...theme, ...patch };
  if (!Palette.legible(next)) {
    const got = Palette.contrast(next.bg, next.text).toFixed(1);
    return `Text on that ground is ${got}:1 — under ${Palette.MIN_CONTRAST}:1 the board `
      + 'stops being readable, so this is not applied.';
  }
  theme = {
    ...next,
    termSize: clampSize(next.termSize, THEME_DEF.termSize),
    diffSize: clampSize(next.diffSize, THEME_DEF.diffSize),
    opacity: clampOpacity(next.opacity),
  };
  try {
    localStorage.setItem(THEME.key, JSON.stringify(theme));
  } catch (e) { /* private mode: the theme still holds for this session */ }
  applyTheme();
  return null;
}

/** Back to the palette the app shipped with — which `check-palette.mjs` asserts is
 *  exactly what `:root` declares, so this is a real answer rather than one that
 *  resembles it. */
export function resetTheme() {
  return setTheme(THEME_DEF);
}


/* Applied at module scope, which is the earliest the page can be themed: modules
   are deferred, so `documentElement` is there, and this runs before `app.js` has
   rendered anything. Any later and the board paints `:root`'s palette first and
   then visibly changes colour — which is the defect the ratio solving removes for
   the *default* theme and cannot remove for anybody else's. */
applyTheme();

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

export function setWheel(/** @type {number} */ w) {
  const next = Math.min(WHEEL.max, Math.max(WHEEL.min, Math.round(w * 10) / 10));
  wheelScale = next;
  $('wsval').textContent = `${Math.round(next * 100)}%`;
  ctl('wsdown').disabled = next <= WHEEL.min;
  ctl('wsup').disabled = next >= WHEEL.max;
  return next;
}

export function saveWheel(/** @type {number} */ w) {
  try {
    if (w === WHEEL.def) localStorage.removeItem(WHEEL.key);
    else localStorage.setItem(WHEEL.key, String(w));
  } catch (e) { /* private mode: it still applies for this session */ }
}

export function saveZoom(/** @type {number} */ z) {
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

/** One attached terminal, keyed by `termKey`.
 *
 *  `term` and `fit` are xterm's, which ships no types here, so they stay `any`;
 *  everything `term.js` hangs on the entry itself is spelled out, because that is
 *  the half a typo can silently add a second copy of.
 *
 *  @typedef {{ term: any, fit: any, host: HTMLDivElement,
 *              checkout: Target, key: string,
 *              badge?: HTMLElement, sock?: WebSocket,
 *              closed?: boolean, everOpened?: boolean,
 *              needsReset?: boolean, backoff?: number, box?: string | null,
 *              reconnectTimer?: ReturnType<typeof setTimeout>,
 *              pending: (string | Uint8Array)[], pendingBytes: number,
 *              queued: (string | Uint8Array)[], queuedBytes: number,
 *              sent?: { rows: number, cols: number } | null }} TermEntry
 */

/** @type {Map<string, TermEntry>} */
export const terms = new Map();      // termKey -> a TermEntry

/** The key a terminal is held under: its checkout and its wire target.
 *
 *  **Qualified, because a target is only unique within one daemon.** `MAIN` is
 *  `"main"` in every checkout and a managed process id is `<workspace>:<name>`, so
 *  `proc:main:ng-watch` names a different pty in each one — and an unqualified map
 *  would hand you the other checkout's live terminal under this checkout's tab.
 *
 *  NUL as the separator, because it is the one byte a path cannot contain.
 *
 *  @param {{ path: string }} checkout
 *  @param {string} target
 */
export const termKey = (checkout, target) => `${checkout.path}\u0000${target}`;

export function stateLabel(/** @type {import('../snapshot').SessionView} */ s) {
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
    default: return /** @type {{ state: string }} */ (s.state).state;
  }
}

/** Dot colours are shared across every row so one legend covers them all (§9). */
export function dotClass(/** @type {import('../snapshot').SessionView} */ s) {
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

export function stateClass(/** @type {import('../snapshot').SessionView} */ s) {
  const k = s.state.state;
  if (k === 'build_failing' || k === 'error') return 'build';
  if (handedToPr(s)) return 'pr';
  if (k === 'your_turn' && s.state.reason !== 'ready') return 'blocked';
  return '';
}

/** Idle time worth surfacing. A session you opened and have not typed into is
 *  idle, but shouting about it the moment you open it is noise. */
export const isWaiting = (/** @type {import('../snapshot').SessionView} */ s) => s.wants_attention;

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
export async function copyText(/** @type {string} */ text) {
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

export function openMenu(/** @type {MouseEvent} */ ev, /** @type {([string, string | null, (() => void) | null])[]} */ items) {
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
    const r = /** @type {HTMLElement} */ (ev.currentTarget || ev.target).getBoundingClientRect();
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
/** @type {HTMLElement | null} */
let menuAnchor = null;

export function sessionsOf(/** @type {string | null} */ wsId, state = snap) {
  return state.sessions.filter((s) => s.workspace === wsId);
}

/* A session is one of two things: active, or a past conversation you can come
 * back to. The daemon's `exited` and `archived` are the same fact from here, and
 * neither is a state worth a word of its own in the rail. */
export const isArchived = (/** @type {import('../snapshot').SessionView} */ s) => s.state.state === 'archived' || s.state.state === 'exited';

/** `spawn::PENDING_WORKTREE`: the workspace a worktree session sits in until
 *  `SessionStart` reports the name Claude Code gave it. */
const PENDING_WORKTREE = '\u2026creating';

/** A worktree Claude Code has not named yet (§2): the daemon knows the session
 *  before it knows where it lives. */
export const pending = (/** @type {import('../snapshot').SessionView} */ s) => s.workspace === PENDING_WORKTREE;

/* A finished session that never had a turn wrote no transcript, so there is no
 * conversation to come back to — `claude --resume` answers "no conversation
 * found" and exits. Listing one is offering something that cannot work, so the
 * archive is conversations, not every session that ever stopped. */
export const isConversation = (/** @type {import('../snapshot').SessionView} */ s) => isArchived(s) && s.has_transcript;

/** Newest first: `created_ms` is an age, so the smallest number is the newest. */
export const byNewest = (/** @type {import('../snapshot').SessionView} */ a, /** @type {import('../snapshot').SessionView} */ b) => a.created_ms - b.created_ms;

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
/* The two questions every pane asks the workspace list.
   `state` defaults to the active checkout's snapshot, which is what every pane
   that follows the selection wants. The rail passes the checkout's own, because
   it draws all of them. One spelling either way: a second pair of functions for
   "but in that checkout" is how the two answers drift apart. */
export const mainWorkspace = (state = snap) => state.workspaces.find((w) => w.is_main);
export const workspaceById = (/** @type {string | null} */ id, state = snap) => state.workspaces.find((w) => w.id === id);

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
/** @type {string | null} */
let creatingWhat = null;
export const creating = () => creatingWhat;

/** @type {((what: string | null) => void)[]} */
const creatingListeners = [];
export function onCreatingChange(/** @type {(what: string | null) => void} */ fn) { creatingListeners.push(fn); }

/** Run `go` as the one create in flight, or say what is already going.
 *
 *  Announced rather than rendered, on the seam `setDrawerCollapsed` uses: this
 *  layer must not reach into the rail that sits on it. Announced *both* ways,
 *  because the interesting frame is the one where the button goes dead — the
 *  snapshot that would have redrawn it is not promised to arrive while a worktree
 *  is being cut. */
async function asTheOnlyCreate(/** @type {string} */ what, /** @type {() => Promise<any>} */ go) {
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

/** @param {string} workspace
 *  @param {Target} [where] the checkout to create in; the active one by default */
export async function newSession(workspace, where) {
  await asTheOnlyCreate('starting a session', async () => {
    try {
      const r = await callOn(where ?? activeCheckout(), '/api/session', { workspace });
      pendingSelect = r.session;
    } catch (e) {
      toast(reason(e), true);
    }
  });
}

/** Claude Code names the worktree unless you shift-click and name it yourself.
 *  Naming one every time is friction for something you rarely refer to by
 *  name, and an unnamed one cannot collide with an archived worktree either. */
/** @param {boolean} [named]
 *  @param {Target} [where] the checkout to cut the worktree in */
export async function newWorktree(named, where) {
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
      const r = await callOn(where ?? activeCheckout(), '/api/worktree', name ? { name } : {});
      pendingSelect = r.session;
      toast(name ? `creating worktree ${name}` : 'creating worktree');
    } catch (e) {
      toast(reason(e), true);
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
    selectedProc[wsKey(wsId)] = r.process;
    // You pressed + to type in it. The pty does not exist until the daemon says
    // so, so this is claimed here and spent when the terminal appears.
    pendingProcFocus = r.process;
    // The snapshot with it in has usually landed already, so ask for the render
    // rather than waiting for one that has been.
    redrawDrawer();
  } catch (e) {
    toast(reason(e), true);
  }
}

// The daemon decides this, not the user agent string: it is the side that knows
// whether it is being shown in a window it owns or in somebody's browser tab.
//
// The commands go over the same authenticated HTTP the rest of the UI uses,
// and the daemon — running inside the desktop process — calls Tauri's window
// API in Rust. No IPC bridge, so nothing here depends on which port we bound.
export const CHROME = window.__ORCH__.chrome || 'none';

/** Whether the window behind this page is see-through.
 *
 *  Told by the host, which read it out of `host.json` when it built the window —
 *  so the page never has to guess whether lowering the opacity will show the
 *  desktop or nothing at all. False in a browser tab, where the tab's own ground
 *  is behind the page.
 */
export const SEE_THROUGH = window.__ORCH__.seeThrough === true;

/** Whether the daemon is running on macOS. Told, not sniffed. */
export const IS_MAC = window.__ORCH__.platform === 'mac';

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

/** @type {Record<string, string | null>} */
export const selectedProc = {};        // wsKey -> process id

/** The key per-workspace UI state is held under: its checkout and its id.
 *
 *  **Qualified, for the same reason [`termKey`] is.** `MAIN` is `"main"` in every
 *  daemon, so a bare workspace id names a different workspace in each checkout —
 *  and `orch.procOrder` is *persisted* under it, so dragging one checkout's drawer
 *  tabs silently reordered another's, permanently and across reloads. No amount of
 *  disposing terminals undoes a wrong key in `localStorage`.
 *
 *  Always the checkout you are in: every reader of these three is a pane that
 *  follows the selection.
 *
 *  `null` is a real caller: `currentWorkspaceId()` answers it when nothing is
 *  selected, and the key has to stay a string either way — "no workspace in this
 *  checkout" is its own slot, not an error.
 *
 *  @param {string | null} wsId
 */
export const wsKey = (wsId) => `${activeCheckout().path}\u0000${wsId}`;
/** What a PR is doing, in the two or three words a row has space for. */
export function prState(/** @type {import('../snapshot').PrView} */ p) {
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
function prOf(/** @type {import('../snapshot').SessionView} */ s) {
  if (!s) return null;
  if (s.pass) {
    return (snap.prs || []).find((p) => p.number === s.pass?.pr) || null;
  }
  return prForWorkspace(s.workspace);
}

export function handedToPr(/** @type {import('../snapshot').SessionView} */ s) {
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
/** @type {((collapsed: boolean) => void)[]} */
const drawerListeners = [];
export function onDrawerChange(/** @type {(collapsed: boolean) => void} */ fn) { drawerListeners.push(fn); }

export function setDrawerCollapsed(/** @type {boolean} */ v) {
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
   which is the only thing telling two of them apart.

   **Keyed by [`wsKey`], which carries the checkout.** This is the one piece of
   per-workspace state that *persists*, so a bare workspace id here was the worst
   of the collisions: `main` names a workspace in every checkout, and the order was
   written and applied with no membership check. */
export let procOrder = (() => {
  try {
    return JSON.parse(localStorage.getItem('orch.procOrder') || '{}') || {};
  } catch (e) {
    return {};
  }
})();

export function setProcOrder(/** @type {string | null} */ wsId, /** @type {string[]} */ keys) {
  procOrder = { ...procOrder, [wsKey(wsId)]: keys };
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
/** @type {string | null} */
export let pendingProcFocus = null;

/** A session the daemon has just been asked to create.
 *
 *  Setting `selected` alone is not enough: the terminal is only opened when a
 *  session is shown, and the snapshot handler skips that once something is
 *  already selected. */
/** @type {string | null} */
export let pendingSelect = null;

/* Written from more than one module, and an imported binding is read-only, so the
 * writes come through here. The alternative — leaving the state in `app.js` and
 * letting modules reach back for it — is the coupling the modules exist to end. */
export function setPendingSelect(/** @type {string | null} */ id) { pendingSelect = id; }
export function setPendingProcFocus(/** @type {string | null} */ id) { pendingProcFocus = id; }
export function setDrawerTouched(/** @type {boolean} */ v) { drawerTouched = v; }
export function setSelectedProc(/** @type {string | null} */ wsId, /** @type {string | null} */ procId) { selectedProc[wsKey(wsId)] = procId; }
