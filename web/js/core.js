// The primitives every part of the SPA needs: the daemon's token, the two fetch
// wrappers, the DOM shorthands, and the snapshot itself.
//
// Extracted first because a module can only import from another module — a leaf
// like the review queue cannot be pulled out until the trunk it reaches for is
// importable. Everything here was already shared; the difference is that reaching
// for it now has to be written down.
//
// **It imports nothing of the SPA's**, and that is what a floor means: every pane
// imports this, so anything here that only two panes want is a pane's worth of
// code every pane pays for. The theme went out for exactly that reason.

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
  // Picking a session while a worktree is being cut is not cancelling the cut; it
  // is saying the pane is about something else now. `auto` is excluded because the
  // pick the app makes for you when a session ends is not that statement.
  if (id && !auto) startingWatched = false;
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

/** A URL fit to put in an `href`, or `#`.
 *
 *  **Every one of these comes from outside.** A PR's URL and a release's come from
 *  GitHub, a review row's from `reviews_command` — a command the repo configures,
 *  so its output is whatever that prints — and a story's from an agent whose own
 *  input is third-party review comments. Four `href =` sites took the string as it
 *  arrived, and `javascript:` in one of them is a script running with the page's
 *  token, on a click that looks like a link.
 *
 *  `http` and `https` only, judged by parsing rather than by prefix: `URL` resolves
 *  the scheme the way the browser will, so ` javascript:…`, `JavaScript:…` and a
 *  `data:` URL are refused by one rule. A relative URL has no scheme of its own and
 *  resolves against this page, which is where it belongs.
 *
 *  `#` rather than no anchor, because the row is still the row: it reads and copies
 *  the same, and only the navigation is refused.
 *
 *  @param {string | null | undefined} url
 */
export function safeHref(url) {
  if (!url) return '#';
  try {
    const parsed = new URL(url, location.href);
    return parsed.protocol === 'https:' || parsed.protocol === 'http:' ? parsed.href : '#';
  } catch {
    return '#';
  }
}

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

/** An inline SVG icon, built as nodes rather than written as markup.
 *
 *  **Node by node so the SPA has no `innerHTML` at all.** Both icons here used
 *  to be a string of markup, which is harmless — the string is a literal — but it
 *  left the page with a live HTML sink for the next person to reach for, on a page
 *  that renders PR titles, review-thread bodies and diff text from GitHub and
 *  carries the app token on `window.__ORCH__`. There is no Content-Security-Policy
 *  behind it either: the window loads the daemon over `http://127.0.0.1`, which is
 *  a *remote* origin to Tauri, so `app.security.csp` never applies to this page.
 *  Zero sinks is the cheaper property to hold, and `no-restricted-syntax` holds it.
 *
 *  `1em`, so the caller's own font-size sets the size.
 *
 *  @param {string} cls class for the wrapping span
 *  @param {number} strokeWidth
 *  @param {...string} ds one `path` per `d`
 *  @returns {HTMLSpanElement}
 */
export function icon(cls, strokeWidth, ...ds) {
  const ns = 'http://www.w3.org/2000/svg';
  const span = el('span', cls);
  const svg = document.createElementNS(ns, 'svg');
  for (const [k, v] of [
    ['viewBox', '0 0 16 16'], ['width', '1em'], ['height', '1em'], ['fill', 'none'],
    ['stroke', 'currentColor'], ['stroke-width', String(strokeWidth)],
    ['stroke-linecap', 'round'], ['stroke-linejoin', 'round'], ['aria-hidden', 'true'],
  ]) svg.setAttribute(k, v);
  for (const d of ds) {
    const path = document.createElementNS(ns, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  span.append(svg);
  return span;
}

/** The chevron a collapsible header rotates — drawn rather than typed so it
 *  matches the gear and refresh and cannot fall out of the font. `1em`, so each
 *  header's own font-size still sets its size, and the `[aria-expanded]` rotate
 *  rule turns the SVG exactly as it turned the glyph. */
export function caret() {
  return icon('caretr', 1.6, 'M6 4l4 4-4 4');
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
  /* Before the children go: `returnFocus` asks whether the dialog still holds the
     keyboard, and an emptied host holds nothing — the answer would be "somewhere
     else has it" for a dialog that had it a line ago. */
  returnFocus('dlg', host);
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
  borrowFocus('dlg');
  (focus || go).focus();
  dlgAsking = message;
  dlgPending = new Promise((resolve) => { dlgSettle = resolve; });
  return dlgPending;
}

/** `window.confirm`, drawn by the app. Resolves true or false, never throws.
 *
 *  **Only for a destructive action, and that is the whole rule.** Destructive means
 *  work that cannot be got back: orchd's copy of a transcript, banked work git
 *  keeps no copy of, a file's uncommitted content, unsaved typing in the editor.
 *  Those four ask. Nothing else does.
 *
 *  It used to be eight, and the four that went are the reason the rule is written
 *  here. Closing a checkout, swapping a branch with main, moving a session out of
 *  main and removing a worktree all asked — and every one of them is reversible:
 *  the conversations are kept and offered on reopen, a swap is undone by swapping
 *  back, and the teardown preflight refuses a tree that is dirty, unpushed or
 *  occupied and writes the record `revive` rebuilds from. Meanwhile `fix` starts a
 *  force-pushing run and `open in main checkout` moves main's branch, and neither
 *  asked. A gate that fires on the loud rather than on the lossy teaches people to
 *  click through it, and then it is not there for the four that matter.
 *
 *  Loudness is answered with a toast that says what happened, not with a question
 *  before it happens. Refusals are the daemon's: the swap lock, the push guard and
 *  the teardown preflight are what actually stop a wrong move, and they work
 *  whether or not anybody read a box.
 *
 *  This cannot be a lint — nothing static can tell destructive from loud — so it is
 *  a rule, written at the one function it governs. */
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
 *  `signal` is for a caller whose next request supersedes this one — the search
 *  makes one per keystroke, and an answer to a query nobody is asking any more is
 *  worse than no answer, because it arrives after the right one.
 *
 *  @param {Endpoint} c
 *  @param {string} path
 *  @param {AbortSignal} [signal]
 */
export async function getOn(c, path, signal) {
  const res = await fetch(c.base + path, { headers: { 'x-orch-token': c.token }, signal });
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
export const get = (/** @type {string} */ path, /** @type {AbortSignal} */ signal) => getOn(activeCheckout(), path, signal);

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
 *  rebuilds" as the default and takes the churn out one pane at a time.
 *
 *  **`tools/check-drop-lists.mjs` holds `drop` to names the daemon still sends**,
 *  because those are hand-written strings matched against generated ones: rename
 *  a field in Rust and this list keeps the old spelling, drops nothing, and the
 *  pane churns again with nothing saying so.
 *
 *  Nothing checks the other half, and nothing cheaply can: whether a signature
 *  that *lists* its inputs listed them all is a question about the whole function
 *  body. The cost is a pane that freezes, and `renderRail`'s own comments name
 *  the two inputs it was missing — each found by pressing something. So prefer
 *  the whole-snapshot-plus-`drop` shape wherever stale would be worse than an
 *  extra rebuild. */
export function paintSig(/** @type {any} */ value, /** @type {string[]} */ drop = []) {
  return JSON.stringify(value, (k, v) => (k.endsWith('_ms') || drop.includes(k) ? undefined : v));
}

/** A pane's paint box: what it was last built from, and what that has cost.
 *
 * @typedef {object} PaintBox
 * @property {string | null | undefined} sig  The last signature, or `null` before the first paint.
 * @property {string} [name]    What to call this pane in `paintStats`.
 * @property {number} [n]       Rebuilds since the page loaded, or since `orchPaint`.
 * @property {string[]} [paths] Where the signature moved on the last rebuild.
 */

/** Every named box that has been asked at least once, for `paintStats`.
 *
 *  The boxes are module-level constants in the panes, so this holds them for the
 *  life of the page and never grows past the number of panes. */
const paintBoxes = new Set();

/** Whether to work out *which* field moved, which costs two parses and a walk.
 *
 *  Off by default and switched on from `orchPaint`, because the counting half is
 *  one integer add and the attributing half is not. */
let paintPaths = false;

/** The paths at which two signatures differ, as the drop lists spell them.
 *
 *  Array indices collapse to `[]`, so sixty worktrees report
 *  `workspaces[].processes[].health` once rather than sixty near-identical
 *  paths — and the leaf of that path is the bare name a `drop` entry matches.
 *  Capped, because a signature that differs everywhere has already answered the
 *  question and walking the rest of a snapshot is the cost this is measuring. */
function sigDiff(/** @type {string | null | undefined} */ before, /** @type {string} */ after) {
  if (before == null) return ['(first paint)'];
  /** @type {Set<string>} */
  const out = new Set();
  const walk = (/** @type {any} */ a, /** @type {any} */ b, /** @type {string} */ path) => {
    if (out.size >= 12 || a === b) return;
    const both = a !== null && b !== null && typeof a === 'object' && typeof b === 'object'
      && Array.isArray(a) === Array.isArray(b);
    if (!both) {
      if (JSON.stringify(a) !== JSON.stringify(b)) out.add(path || '(root)');
      return;
    }
    if (Array.isArray(a)) {
      if (a.length !== b.length) out.add(`${path}[] (length)`);
      for (let i = 0; i < Math.min(a.length, b.length); i += 1) walk(a[i], b[i], `${path}[]`);
      return;
    }
    for (const k of new Set([...Object.keys(a), ...Object.keys(b)])) {
      walk(a[k], b[k], path ? `${path}.${k}` : k);
    }
  };
  try {
    walk(JSON.parse(before), JSON.parse(after), '');
  } catch {
    return ['(unparseable)'];
  }
  /* **A rebuild with nowhere to point is a rebuild for key order**, and it is
     worth its own sentence rather than an empty list. `paintSig` compares JSON
     text; this walks the parsed values by key, so the one difference it cannot
     see is the order the keys came in. The daemon has two places that can move
     without changing anything — `WorkspaceView.branches` is collected off a
     `HashSet` and never sorted, and `automation` is serialized straight from a
     `HashMap` — and a rehash of either tears down every session row and every PR
     row for a snapshot that says the same thing. Measured idle: one rail rebuild
     in 35 seconds, this and nothing else. */
  if (out.size === 0) return ['(key order only — a HashSet or HashMap in the snapshot)'];
  return [...out];
}

/** True when `value` renders the same as it did last time this box was asked.
 *
 *  The box is the pane's own `{ sig: null }`: five panes were each carrying a
 *  module-level `let xSig`, the same three lines of compare-and-remember, and the
 *  same comment with one noun changed. One name for the idiom means a reader
 *  confirms it once.
 *
 *  **`name` is what makes a rebuild attributable**, and it is why every box
 *  carries one. `paintSig` above says the unchecked half of this guard is whether
 *  a pane rebuilds for something it does not draw; the cost of that is a rail
 *  that tears down the row under the pointer several times a second, and the
 *  symptom is a hover that strobes. Nothing said which field moved, so the answer
 *  was read out of the type file and guessed at. `box.n` counts the rebuilds and
 *  `box.paths` names them — see `orchPaint`. */
export function unchanged(/** @type {PaintBox} */ box, /** @type {any} */ value, /** @type {string[]} */ drop = []) {
  const sig = paintSig(value, drop);
  paintBoxes.add(box);
  if (box.sig === sig) return true;
  box.n = (box.n ?? 0) + 1;
  if (paintPaths) box.paths = sigDiff(box.sig, sig);
  box.sig = sig;
  return false;
}

/** What each guarded pane has rebuilt, and what moved when it last did.
 *
 *  **A counter rather than a gate**, deliberately: the number that matters is per
 *  workload — three agents editing is not one agent idling — so there is no bound
 *  to deny against, and a rule nobody can run is worse than a number somebody
 *  reads. `tools/paint-check.mjs` is the thing that runs it.
 *
 *  Counts are since the page loaded, or since the last `reset`. */
function paintStats() {
  return [...paintBoxes]
    .map((b) => ({ name: b.name ?? '(unnamed)', rebuilds: b.n ?? 0, paths: b.paths ?? [] }))
    .sort((a, b) => b.rebuilds - a.rebuilds);
}

/** Read the counters, and with `true` zero them and start naming what moved.
 *
 *  Hung on the global object rather than exported to a pane, because the caller
 *  is a person at a console or `tools/paint-check.mjs` — the same reason
 *  `orchTeardown` is there. One name with two verbs: a second global for the read
 *  would be the same idea spelled twice.
 *
 *  **Arming is the half that costs**, two `JSON.parse`s and a walk per rebuild,
 *  so it stays off until somebody asks. Nothing turns it back off: a page that
 *  has been asked to measure itself is a page somebody is measuring, and it is
 *  reloaded rather than un-armed. */
window.orchPaint = (reset = false) => {
  if (reset) {
    paintPaths = true;
    for (const b of paintBoxes) {
      b.n = 0;
      b.paths = [];
    }
  }
  return paintStats();
};

/** What each keyed parent last put where — see `reconcile`. */
const keyed = new WeakMap();

/** Fill `parent` with `items`, keeping every node whose signature has not moved.
 *
 *  **`unchanged` decides whether a pane repaints; this decides what a repaint
 *  costs.** A pane that must repaint still has no business destroying the rows
 *  that did not change, and `replaceChildren` destroys all of them. The row under
 *  the pointer is the one that matters: `:hover` re-resolves onto the replacement
 *  in every engine — measured, not assumed — but the replacement starts from no
 *  hover, so `.sess`'s 120ms fade restarts and the highlight never arrives.
 *
 *  **Measured, at the rebuild rate three real sessions produce (~7/s), with the
 *  pointer parked on a row — the fraction of the time its highlight was actually
 *  painted:**
 *
 *  | how the list is rebuilt                        | chromium | webkit/gtk |
 *  | ---------------------------------------------- | -------- | ---------- |
 *  | `replaceChildren`, fresh nodes (what this was) |      0%  |       13%  |
 *  | `replaceChildren`, *same* node objects reused  |      5%  |       13%  |
 *  | this: never detach a node that is still wanted |    100%  |      100%  |
 *
 *  The middle row is why this is not three lines of caching. Re-appending the very
 *  same element still detaches and re-inserts it, and the engines treat that as a
 *  new box: keeping the object is not enough, the node has to stay where it is.
 *  `tools/hover-engine-check.mjs` is that table.
 *
 *  An item is `{ key, sig, build }`. `key` identifies the row across repaints —
 *  a session id, not an index, or a reorder renames every row. `sig` is what the
 *  row was drawn from, through `paintSig`; when it matches, the node is left
 *  alone. **`sig` has to cover the handlers too, not only the pixels**: a reused
 *  row keeps the closures it was built with, so anything an `onclick` reads from
 *  outside the row belongs in the signature or the row will act on a stale copy
 *  of it.
 *
 *  `fill(node)` is for a container whose own children are reconciled: give it a
 *  constant `sig` so the container itself is built once, and reconcile inside it.
 *
 *  A row whose `sig` did move is rebuilt and swapped in place, so it flickers —
 *  correctly, since it changed. Its neighbours do not. */
export function reconcile(/** @type {Element} */ parent, /** @type {{key: string, sig: string, build: () => Element, fill?: (node: any) => void}[]} */ items) {
  const cache = keyed.get(parent) ?? new Map();
  const next = new Map();
  let cursor = parent.firstChild;
  for (const it of items) {
    const had = cache.get(it.key);
    const reuse = had && had.sig === it.sig;
    const node = reuse ? had.node : it.build();
    /* The stale node goes before the new one arrives, and the cursor steps over
       it first — `insertBefore` against a node that is no longer a child throws,
       and that is exactly what a removed cursor would be. */
    if (!reuse && had && had.node.parentNode === parent) {
      if (had.node === cursor) cursor = had.node.nextSibling;
      had.node.remove();
    }
    if (node === cursor) cursor = node.nextSibling;
    else parent.insertBefore(node, cursor);
    if (it.fill) it.fill(node);
    next.set(it.key, { node, sig: it.sig });
  }
  // Whatever is left is what this pass did not ask for.
  while (cursor) {
    const after = cursor.nextSibling;
    cursor.remove();
    cursor = after;
  }
  keyed.set(parent, next);
}


/** How many rows either bottom pane will draw.
 *
 *  **A bound on the DOM, not on the truth.** Both heads count what the daemon
 *  actually found, so a repo with ninety open PRs still says ninety — this only
 *  stops the pane building ninety rows nobody scrolls to. Fifty is already what
 *  the review search asks GitHub for, so for that pane it changes nothing and only
 *  binds a configured `reviews_command` that answers with more.
 *
 *  Shared, because the two panes are a pair and a cap on one of them is the kind
 *  of number that drifts the moment it is written twice.
 */
export const QUEUE_MAX = 50;

export function refreshButton(/** @type {'pr' | 'review'} */ kind, /** @type {number} */ pollCount, /** @type {string} */ endpoint, /** @type {boolean} */ polling) {
  // Drawn, not typed — see the files-header refresh in index.html for why the
  // reload glyph is an SVG rather than U+21BB. 1em tracks the font-size setting.
  const btn = icon('rvrefresh', 1.5, 'M13.4 8A5.4 5.4 0 1 1 11.7 4', 'M12 1.6V4.3H9.3');
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
    // Inert while it spins: the fetch it would ask for is already running.
    if (btn.classList.contains('spin')) return;
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

/** Show the keyboard legend, or put it away if it is already up.
 *
 *  Here rather than in `app.js` for the reason [`closeLegend`] gives, and one
 *  function rather than three `hidden = !hidden` lines, because the focus it
 *  borrows has to be given back on every way out — and a toggle written in four
 *  places is a toggle where one of them forgets. */
export function toggleLegend() {
  if ($('keyhelp').hidden) {
    borrowFocus('legend');
    $('keyhelp').hidden = false;
  } else closeLegend();
}

/** Dismiss the keyboard legend.
 *
 *  Here rather than in `app.js`, which owns the overlay, because the settings
 *  panel closes it too and `settings` cannot reach back up to the app layer —
 *  the same reason `closeMenu` lives down here. */
export function closeLegend() {
  $('keyhelp').hidden = true;
  returnFocus('legend', $('keyhelp'));
}

/* ---------------------------------------------------------------------------
 * Who gets the keyboard back
 * ------------------------------------------------------------------------- */

/** What had the keyboard when each open dialog took it, by the dialog's name.
 *
 *  **A map rather than a stack**, because the dialogs nest in more than one
 *  order: the finder opens the file viewer, the viewer opens the editor, and any
 *  of them can raise a confirm on the way out. A stack would be right only while
 *  they closed in the order they opened.
 */
/** @type {Map<string, HTMLElement>} */
const focusReturn = new Map();

/** Remember what has the keyboard, before a dialog takes it.
 *
 *  **Called on open, and ignored if that dialog already holds a record.** The
 *  finder's `open` is also its mode switch — Shift-Shift while it is up — and a
 *  second borrow there would record the finder's own input box as the thing to go
 *  back to, which is a dialog that closes into itself.
 *
 *  `document.body` is not somewhere to return to: it is what the engine focuses
 *  when nothing is focused, so recording it would let a later close steal the
 *  keyboard from wherever it had legitimately gone.
 */
export function borrowFocus(/** @type {string} */ who) {
  if (focusReturn.has(who)) return;
  const on = document.activeElement;
  if (on && on !== document.body) focusReturn.set(who, /** @type {HTMLElement} */ (on));
}

/** Give the keyboard back to whatever [`borrowFocus`] saw, and forget it.
 *
 *  `from` is the dialog's own element, and it is what makes this safe to call on
 *  every close path. Focus is handed back only when the dialog still holds it, or
 *  when nothing does — which is the state hiding the dialog leaves behind, and
 *  the whole of the defect this exists for (#27): the keystrokes after `Escape`
 *  went nowhere, and the pane had to be clicked.
 *
 *  What is refused is the other case: a close that has already put the keyboard
 *  somewhere on purpose, which several of these do.
 *
 *  Call it **after** the dialog is hidden. The element is checked for still being
 *  in the document because a pane can be torn down while a dialog sits over it,
 *  and `focus()` is wrapped because a disposed terminal's textarea throws.
 */
export function returnFocus(/** @type {string} */ who, /** @type {HTMLElement | null} */ from = null) {
  const back = focusReturn.get(who);
  focusReturn.delete(who);
  if (!back || !document.contains(back)) return;
  const on = document.activeElement;
  if (on && on !== document.body && !(from && from.contains(on))) return;
  try {
    back.focus();
  } catch (e) { /* disposed while the dialog was up */ }
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

/** Which checkout the create in flight belongs to, so the rail can put its
 *  placeholder row in the right block rather than in whichever one is first.
 *
 *  A second value beside `creatingWhat` rather than a shape, because every reader
 *  wants one or the other: the buttons and the centre pane ask *whether*, and only
 *  the rail asks *where*.
 */
/** @type {string | null} */
let creatingWhere = null;
export const creatingIn = () => creatingWhere;

/** Whether the centre pane is still about the create in flight.
 *
 *  **A create is not a modal, and it used to behave like one.** The overlay covers
 *  the terminal region for as long as the POST takes — a fetch, an 18k-file
 *  checkout and a `claude` boot — and it covered it whichever session you picked,
 *  so the rail answered a click and the pane went on saying `creating a worktree`.
 *  Nothing was ever blocked; there was simply nothing to see.
 *
 *  So the overlay belongs to the placeholder row rather than to the app: it is up
 *  while the create is what you are looking at, and picking any session says you
 *  are looking at something else. The placeholder row puts it back — see
 *  `startingRow`, which is a button now for exactly that reason.
 */
let startingWatched = false;
export const startingShown = () => startingWatched && creatingWhat !== null;
export function watchStarting(/** @type {boolean} */ on) {
  startingWatched = on;
  for (const fn of creatingListeners) fn(creatingWhat);
}

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
async function asTheOnlyCreate(/** @type {string} */ what, /** @type {Target} */ where, /** @type {() => Promise<any>} */ go) {
  if (creatingWhat) {
    toast(`still ${creatingWhat}`);
    return;
  }
  creatingWhat = what;
  creatingWhere = where.path;
  // You pressed `+`, so the create is what you are looking at — until you say
  // otherwise by picking a session.
  startingWatched = true;
  for (const fn of creatingListeners) fn(creatingWhat);
  try {
    await go();
  } finally {
    creatingWhat = null;
    creatingWhere = null;
    for (const fn of creatingListeners) fn(null);
  }
}

/** @param {string} workspace
 *  @param {Target} [where] the checkout to create in; the active one by default */
export async function newSession(workspace, where) {
  const target = where ?? activeCheckout();
  await asTheOnlyCreate('starting a session', target, async () => {
    try {
      const r = await callOn(target, '/api/session', { workspace });
      if (startingWatched) pendingSelect = r.session;
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
  const target = where ?? activeCheckout();
  await asTheOnlyCreate(name ? `creating worktree ${name}` : 'creating a worktree', target, async () => {
    try {
      const r = await callOn(target, '/api/worktree', name ? { name } : {});
      /* **Only if you are still watching it.** Landing you on what you asked for is
         right when you waited for it and wrong when you did not: a worktree cut is
         ten seconds, being able to work in those ten seconds is the point, and a
         pane taken back at a moment you did not choose is the same interruption the
         overlay used to be. Decided here rather than where the selection is applied,
         because this is the line that asks for it — and by then `creating` has
         already been cleared by the `finally` below, so nothing downstream can still
         tell the two cases apart. */
      if (startingWatched) pendingSelect = r.session;
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
 * Takes a mouse event as readily as a key one: the question is which modifiers
 * are down, and a modifier-click has the same answer to give. ⌘-click on a Mac
 * matters for a reason beyond consistency — `Ctrl`-click there is a right-click,
 * so a binding spelled with Ctrl would open a context menu instead.
 *
 * @param {KeyboardEvent | MouseEvent} e
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
/** Is this PR in trouble — its checks red, or it cannot merge?
 *
 *  **One test, because it was written out four times.** The rail's dot, its header
 *  count and the `fix` button each spelled
 *  `p.checks === 'failing' || p.mergeable === 'CONFLICTING'` for themselves, which
 *  is the shape the daemon's own `Pr::rank` doc warns about: that precedence was
 *  "decided here rather than in four places that each got it slightly
 *  differently", and the page then grew its own four.
 *
 *  @param {import('../snapshot').PrView} p
 */
export function inTrouble(p) {
  return p.checks === 'failing' || p.mergeable === 'CONFLICTING';
}

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

/* The order you dragged the rail's session rows into, per checkout, as a list of
   session ids. A view preference like [`procOrder`] beside it, and kept the same
   way: the rail sorts itself by what needs you, which is right for triage and
   wrong when you are working through a list in an order only you know.

   **Keyed by the checkout path, not by workspace.** A session id is unique across
   every checkout, so the key is not there to stop a collision — it is there so
   "put this list back to newest first" is one checkout's answer rather than every
   checkout's. Closing a checkout does **not** take its order with it: the key
   stays, and reopening applies it again. That is deliberate for a checkout you come
   back to, and it does mean the map only ever grows.

   An id the list has never seen is a session created since you last dragged one,
   and `rail.js` puts those *above* the rows you placed — a worktree you have just
   cut is the newest thing there is, and sent to the bottom it fell off the end of
   the rail the moment it appeared. See `inRailOrder`. */
export let sessionOrder = (() => {
  try {
    return JSON.parse(localStorage.getItem('orch.sessionOrder') || '{}') || {};
  } catch (e) {
    return {};
  }
})();

/** @param {string} path @param {string[]} ids — empty clears the manual order. */
export function setSessionOrder(path, ids) {
  sessionOrder = { ...sessionOrder };
  if (ids.length) sessionOrder[path] = ids;
  else delete sessionOrder[path];
  try {
    localStorage.setItem('orch.sessionOrder', JSON.stringify(sessionOrder));
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
