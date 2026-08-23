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

/** Take a new snapshot: the two have to move together, so they move here.
 *
 *  `snap` is a live binding — importers see this assignment without re-importing,
 *  which is what lets a hundred readers keep saying `snap.x`. */
export function receive(next) {
  snap = next;
  snapAt = Date.now();
}

export const sinceSnap = (ms) => (ms == null ? null : ms + (Date.now() - snapAt));

/** The PR whose head ref this workspace holds, if any. */
export function prForWorkspace(wsId) {
  return (snap.prs || []).find((p) => p.workspace === wsId) || null;
}

/* Which session the centre pane is showing. Owned here because the rail picks it
 * and the terminals and the render both react — leaving the state in `app.js`
 * meant the rail had to reach back into the module that renders it. */
export let selected = null;

const selectionListeners = [];
export function onSelection(fn) { selectionListeners.push(fn); }

/** Pick a session. What *happens* next is whoever registered's business. */
export function setSelected(id) {
  selected = id;
  for (const fn of selectionListeners) fn(id);
}

export const TOKEN = window.__ORCH__.token;
export const WS_BASE = `ws://${location.host}`;

export const $ = (id) => document.getElementById(id);

export function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  return n;
}

let toastTimer = null;
export function toast(message, bad) {
  const t = $('toast');
  t.textContent = message;
  t.className = 'toast on' + (bad ? ' bad' : '');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.className = 'toast'; }, bad ? 7000 : 2600);
}

export async function call(path, body) {
  const res = await fetch(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-orch-token': TOKEN },
    body: JSON.stringify(body ?? {}),
  });
  const json = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(json.error || res.statusText);
  return json;
}

export async function get(path) {
  const res = await fetch(path, { headers: { 'x-orch-token': TOKEN } });
  const json = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(json.error || res.statusText);
  return json;
}

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
export function refreshButton(kind, pollCount, endpoint, polling) {
  const btn = el('span', 'rvrefresh', '↻');
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

export const FS_BASE = 1.155;
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
  $('fsdown').disabled = next <= ZOOM.min;
  $('fsup').disabled = next >= ZOOM.max;
  // Announced rather than applied: the terminals' own font is xterm's business,
  // and reaching into it from here is what made zoom and the terminals depend on
  // each other. Whoever owns a scalable thing registers for this.
  for (const fn of scaleListeners) fn(next);
  return next;
}

export function saveZoom(z) {
  try {
    if (z === ZOOM.def) localStorage.removeItem(ZOOM.key);
    else localStorage.setItem(ZOOM.key, String(z));
  } catch (e) { /* private mode: it still applies for this session */ }
}
