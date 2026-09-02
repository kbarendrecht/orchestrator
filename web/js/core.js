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
export function reportBoot() {
  if (reported) return;
  clearTimeout(reportTimer);
  reportTimer = setTimeout(() => {
    reported = true;
    // Failure is silence. This is a diagnostic, and a toast about it would be
    // the app complaining to the user on the user's behalf.
    call('/api/client/timing', { marks }).catch(() => {});
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
  ctl('fsdown').disabled = next <= ZOOM.min;
  ctl('fsup').disabled = next >= ZOOM.max;
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
  // Teal outranks the underlying state: while automation holds a session,
  // "already being handled" is the more useful signal.
  if (s.kind.kind === 'automation') return 'auto';
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

export function sessionsOf(wsId) {
  return snap.sessions.filter((s) => s.workspace === wsId);
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

export function currentWorkspaceId() {
  const s = currentSession();
  if (s) return s.workspace;
  return snap.workspaces.find((w) => w.is_main)?.id ?? null;
}

export async function newSession(workspace) {
  try {
    const r = await call('/api/session', { workspace });
    pendingSelect = r.session;
  } catch (e) {
    toast(e.message, true);
  }
}

/** Claude Code names the worktree unless you shift-click and name it yourself.
 *  Naming one every time is friction for something you rarely refer to by
 *  name, and an unnamed one cannot collide with an archived worktree either. */
export async function newWorktree(named) {
  let name = null;
  if (named) {
    name = prompt('Worktree name (blank to let Claude name it)');
    // Cancel means cancel; blank means auto.
    if (name === null) return;
    name = name.trim() || null;
  }
  try {
    const r = await call('/api/worktree', name ? { name } : {});
    pendingSelect = r.session;
    toast(name ? `creating worktree ${name}` : 'creating worktree');
  } catch (e) {
    toast(e.message, true);
  }
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
/** The PR a session's work belongs to, whether by branch or by automation. */
export function prOf(s) {
  if (!s) return null;
  if (s.kind.kind === 'automation') {
    return (snap.prs || []).find((p) => p.number === s.kind.pr) || null;
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
export function redrawDrawer() {
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
