'use strict';

// The daemon owns all state. This SPA is stateless and disposable: closing the
// browser kills nothing, and reopening replays from the daemon's buffers (§1).

const TOKEN = window.__ORCH__.token;
const WS_BASE = `ws://${location.host}`;

let snap = { workspaces: [], sessions: [] };
let selected = null;          // session id
let selectedProc = {};        // workspace id -> process id
const terms = new Map();      // target -> { term, fit, sock, host }

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const $ = (id) => document.getElementById(id);

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  return n;
}

let toastTimer = null;
function toast(message, bad) {
  const t = $('toast');
  t.textContent = message;
  t.className = 'toast on' + (bad ? ' bad' : '');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.className = 'toast'; }, bad ? 7000 : 2600);
}

async function call(path, body) {
  const res = await fetch(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-orch-token': TOKEN },
    body: JSON.stringify(body ?? {}),
  });
  const json = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(json.error || res.statusText);
  return json;
}

async function get(path) {
  const res = await fetch(path, { headers: { 'x-orch-token': TOKEN } });
  const json = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(json.error || res.statusText);
  return json;
}

/** Compact enough to sit on a rail row without pushing the name out. */
/* When the snapshot the current numbers came from landed. Durations are computed
 * server-side as the snapshot is built, so rendering them raw freezes the clock
 * between events: a session waiting on a permission prompt sat at "0s" until
 * something unrelated pushed a snapshot, then jumped to "1m". The rail redraws
 * every second; this is what makes those seconds mean anything. */
let snapAt = Date.now();
const sinceSnap = (ms) => (ms == null ? null : ms + (Date.now() - snapAt));

function duration(ms) {
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

// ---------------------------------------------------------------------------
// State presentation
// ---------------------------------------------------------------------------

function stateLabel(s) {
  switch (s.state.state) {
    case 'starting': return 'starting';
    case 'working': return 'working';
    case 'your_turn':
      if (s.state.reason === 'asked_a_question') return 'asked a question';
      if (s.state.reason === 'needs_permission') return 'needs permission';
      if (s.state.reason === 'ready') return 'ready';
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
function dotClass(s) {
  const k = s.state.state;
  if (k === 'build_failing' || k === 'error') return 'build';
  if (k === 'your_turn') return 'blocked';
  // Teal outranks the underlying state: while automation holds a session,
  // "already being handled" is the more useful signal.
  if (s.kind.kind === 'automation') return 'auto';
  if (k === 'working' || k === 'starting') return 'working';
  if (k === 'archived' || k === 'exited') return 'archived';
  return 'idle';
}

function stateClass(s) {
  const k = s.state.state;
  if (k === 'build_failing' || k === 'error') return 'build';
  if (k === 'your_turn' && s.state.reason !== 'ready') return 'blocked';
  return '';
}

/** Idle time worth surfacing. A session you opened and have not typed into is
 *  idle, but shouting about it the moment you open it is noise. */
const isWaiting = (s) => s.wants_attention;

// ---------------------------------------------------------------------------
// Terminals
// ---------------------------------------------------------------------------

const THEME = {
  background: '#101010', foreground: '#D2D2D2', cursor: '#D2D2D2',
  black: '#101010', red: '#C9615A', green: '#5FA97C', yellow: '#E0A244',
  blue: '#4C9AAF', magenta: '#9A7AA0', cyan: '#3E9AAF', white: '#D2D2D2',
  brightBlack: '#5B5B5B', brightRed: '#D6756E', brightGreen: '#74BB90',
  brightYellow: '#EDB55C', brightBlue: '#63AEC2', brightMagenta: '#B08FB6',
  brightCyan: '#57AEC2', brightWhite: '#F0F0F0',
  selectionBackground: '#2C2C2C',
};

/** The terminal's font in px. xterm draws its own text, so the stylesheet's
 *  multiplier cannot reach it; this applies the same factor natively, which is
 *  also why it stays crisp. */
const TERM_FONT = 12;
const termFontSize = () => Math.round(TERM_FONT * uiScale());

/** Attach to a pty, replaying the daemon's buffer first. */
function openTerm(target, parent) {
  if (terms.has(target)) return terms.get(target);

  const host = el('div', 'termhost');
  parent.appendChild(host);

  const term = new Terminal({
    theme: THEME,
    fontFamily: "'IBM Plex Mono', ui-monospace, monospace",
    fontSize: termFontSize(),
    lineHeight: 1.25,
    cursorBlink: true,
    scrollback: 10000,
    allowProposedApi: true,
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(host);
  /* No WebGL in the webview. WebKitGTK is the engine this app actually runs on,
   * and xterm's WebGL renderer garbles glyphs there: text arrives as noise and
   * comes back only when a scroll or a selection forces a full redraw, which is
   * the canvas being composited wrong rather than the buffer being wrong.
   * Repainting after every refit and dropping the addon on context loss both
   * failed to fix it, so the canvas goes instead: the DOM renderer draws real
   * text, which cannot garble. It is slower under heavy output, and that is the
   * trade.
   *
   * A browser tab is Chromium or Firefox, where the fast path is fine, so it
   * keeps it. `chrome` comes from the daemon, which is the side that knows
   * whether it is being shown in a window it owns. */
  if (CHROME === 'none') {
    try {
      const webgl = new WebglAddon.WebglAddon();
      // A lost context leaves the canvas frozen on whatever it last painted, and
      // nothing in xterm notices. Dropping the addon puts the DOM renderer back.
      webgl.onContextLoss?.(() => webgl.dispose());
      term.loadAddon(webgl);
    } catch (e) {
      // Software rendering is slower but correct; not worth failing over.
    }
  }

  const sock = new WebSocket(
    `${WS_BASE}/ws/pty?token=${encodeURIComponent(TOKEN)}&target=${encodeURIComponent(target)}`
  );
  sock.binaryType = 'arraybuffer';
  const entry = { term, fit, sock, host, ready: false };

  sock.onopen = () => {
    entry.ready = true;
    // A fresh socket knows nothing about the size, whatever the last one was told.
    entry.sent = null;
    entry.box = null;
    resize(entry);
    // A session you just created is selected before there is anything to type
    // into, so the focus `select` asked for landed on nothing. Take it once the
    // pty is actually attached, but only if this is still the session you are
    // in, or a slow one would steal the keyboard back later.
    if (terms.get(`session:${selected}`) === entry) {
      try {
        term.focus();
      } catch (e) { /* disposed while the socket was opening */ }
    }
  };
  sock.onmessage = (ev) => {
    if (typeof ev.data === 'string') term.write(ev.data);
    else term.write(new Uint8Array(ev.data));
  };
  sock.onclose = () => { entry.ready = false; };

  term.onData((d) => {
    if (sock.readyState === WebSocket.OPEN) sock.send(new TextEncoder().encode(d));
  });

  terms.set(target, entry);
  return entry;
}

function resize(entry) {
  if (!entry || entry.host.hidden) return;
  // Nothing moved, nothing to do. Without this a repeated observation refits at
  // the same size, and a box whose width lands between two whole cells can flip
  // the answer back and forth — which reads as the terminal resizing itself.
  const box = entry.host.getBoundingClientRect();
  const seen = `${Math.round(box.width)}x${Math.round(box.height)}`;
  if (entry.box === seen) return;
  // A host that is visible but not laid out yet measures as nothing, and fitting
  // to that hands the pty a couple of columns. The TUI on the other end redraws
  // itself to fit and its previous frame is gone, so the pane comes back shrunk
  // and full of the wreckage of the old one. There is no useful terminal this
  // small, so wait for a real box instead.
  if (box.width < 80 || box.height < 40) return;
  entry.box = seen;
  try {
    entry.fit.fit();
  } catch (e) {
    return;
  }
  // The canvas was just resized under the renderer. On WebKitGTK that is where
  // the glyphs come back as garbage that a scroll or a selection cleans up: the
  // buffer is right and the paint is not, so ask for the paint.
  repaint(entry);
  const { rows, cols } = entry.term;
  // Only tell the pty when the geometry actually moved. That makes a refit
  // idempotent, which is what lets the observer below fire as often as it likes
  // instead of costing a resize message per frame of a drag.
  if (entry.sent && entry.sent.rows === rows && entry.sent.cols === cols) return;
  entry.sent = { rows, cols };
  if (entry.ready && entry.sock.readyState === WebSocket.OPEN) {
    entry.sock.send(JSON.stringify({ type: 'resize', rows, cols }));
  }
}

/** Redraw a terminal from its buffer, glyph atlas and all.
 *
 *  Dropping the atlas is the half that matters after a resize or a spell hidden:
 *  it is the piece that survives the canvas being sized to something else, and
 *  it is what the leftover garbage is made of. */
function repaint(entry) {
  requestAnimationFrame(() => {
    try {
      entry.term.clearTextureAtlas?.();
      entry.term.refresh(0, Math.max(0, entry.term.rows - 1));
    } catch (e) { /* a disposed terminal has nothing to refresh */ }
  });
}

function closeTerm(target) {
  const entry = terms.get(target);
  if (!entry) return;
  try { entry.sock.close(); } catch (e) { /* already gone */ }
  entry.term.dispose();
  entry.host.remove();
  terms.delete(target);
}

/** Tab switch replays the daemon buffer; it never respawns (§9). */
function showTerm(target, parent) {
  const entry = target ? openTerm(target, parent) : null;
  for (const [key, e] of terms) {
    if (e.host.parentElement !== parent) continue;
    e.host.hidden = key !== target;
  }
  // Only the centre pane owns the empty state. Without this guard, every
  // drawer render un-hides it and "No session selected" sits on top of a
  // perfectly working terminal.
  if (parent === $('termwrap')) $('termempty').hidden = !!target;
  if (entry) {
    requestAnimationFrame(() => {
      resize(entry);
      // A hidden xterm has no dimensions, so its renderer parks; coming back
      // does not always repaint what is already in the buffer, which is the
      // black pane you get from switching sessions quickly. Ask for the redraw
      // rather than hope for one.
      repaint(entry);
    });
  }
  return entry;
}

// ---------------------------------------------------------------------------
// Context menu
// ---------------------------------------------------------------------------

/**
 * A menu at the cursor. `items` are `[label, extraClass, handler]`; a null
 * handler renders the row disabled, so right-clicking a session that has
 * already ended still says what the menu would have offered.
 */
function openMenu(ev, items) {
  ev.preventDefault();
  const menu = $('ctxmenu');
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

function closeMenu() {
  $('ctxmenu').hidden = true;
}

const menuOpen = () => !$('ctxmenu').hidden;

// Anything that moves what the menu is pointing at dismisses it. On mousedown
// rather than click, and captured, so the row underneath still gets its own
// click; a rail that rebuilds every second would otherwise leave the menu
// hanging over a row that no longer exists.
document.addEventListener('mousedown', (e) => {
  if (menuOpen() && !e.target.closest('#ctxmenu')) closeMenu();
}, true);
document.addEventListener('scroll', closeMenu, true);
window.addEventListener('blur', closeMenu);

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

function sessionsOf(wsId) {
  return snap.sessions.filter((s) => s.workspace === wsId);
}

/** An open diff belongs to the session it was opened from.
 *
 *  So switching away closes it. It used to re-point at the new workspace, which
 *  meant a switch silently swapped the file under you — and a diff is a thing you
 *  opened deliberately, not a pane that should follow you around. */
function syncDiffToSession() {
  if (!diffState.open) return;
  const ws = activeWorkspaceId();
  if (!ws || ws !== diffState.ws) closeDiff();
}

function render() {
  syncDiffToSession();
  renderRail();
  renderContext();
  renderDrawer();
  renderFiles();
  renderReviews();
  renderInteraction();
  renderUpdate();
}

/** The question the selected session is blocked on.
 *
 *  Rendered from the snapshot rather than held locally, so it survives a reload
 *  and shows up in every window at once: the agent is stopped until somebody
 *  answers, and which browser you happen to be looking at is not part of that.
 *
 *  Over the terminal on purpose. The agent could print the question into its own
 *  pane, but then answering means typing into a wall of scrollback, and a
 *  question that scrolls away is one nobody notices. */
function renderInteraction() {
  const host = $('oq');
  const s = currentSession();
  const q = s && s.interaction && !s.interaction.answer ? s.interaction : null;
  if (!q) { host.hidden = true; host.replaceChildren(); return; }

  host.replaceChildren();
  const head = el('div', 'oqh');
  head.appendChild(el('span', 'dia', '\u25C6'));
  head.appendChild(el('span', null, 'needs your call'));
  if (q.thread_id) head.appendChild(el('span', 'oqt', q.thread_id));
  host.appendChild(head);

  host.appendChild(el('div', 'oqq', q.question));
  // Whatever the agent thought you needed to see to decide: a diff, a file, the
  // reviewer's words. Shown verbatim, in the diff's own type.
  if (q.detail) host.appendChild(detailEl(q.detail));

  const opts = el('div', 'oqopts');
  for (const o of q.options) {
    const b = el('button', 'oqopt' + (o.free ? ' esc' : ''));
    b.appendChild(el('div', 'ol', o.label));
    if (o.sub) b.appendChild(el('div', 'od', o.sub));
    // An option that asks for words does not answer on click: it opens the box.
    // Answering straight through would be the button saying "let me write it"
    // and then not letting you.
    b.onclick = o.free
      ? () => openFreeAnswer(opts, s.id, q.id, o)
      : () => answerInteraction(s.id, q.id, o.value, host);
    opts.appendChild(b);
  }
  host.appendChild(opts);
  host.hidden = false;
}

/** The escape hatch's box. Replaces the option row it belongs to, so there is one
 *  thing on screen to finish rather than a form beside a button that also works. */
function openFreeAnswer(opts, session, ask, option) {
  if (opts.querySelector('.oqfree')) return;
  const wrap = el('div', 'oqfree');
  const box = el('textarea', 'box');
  box.setAttribute('aria-label', option.label);
  box.placeholder = 'Say what you want instead. It reaches the agent as written.';
  wrap.appendChild(box);

  const row = el('div', 'oqfoot');
  const send = el('button', 'oqsend', 'send');
  send.onclick = () => {
    if (!box.value.trim()) return toast('nothing written yet', true);
    answerInteraction(session, ask, option.value, opts.parentElement, box.value);
  };
  const back = el('button', 'oqback', 'back to the options');
  back.onclick = () => renderInteraction();
  row.appendChild(send);
  row.appendChild(back);
  wrap.appendChild(row);

  opts.replaceChildren(wrap);
  box.focus();
}

/** Answer it, and let the next snapshot take the card away.
 *
 *  The buttons go dead immediately: the agent is released the moment the daemon
 *  has the answer, and a second click would be answering a question that is no
 *  longer open. */
async function answerInteraction(session, ask, answer, host, text) {
  const buttons = host.querySelectorAll('.oqopt, .oqsend');
  for (const b of buttons) b.disabled = true;
  try {
    await call(`/api/session/${session}/answer`, { ask, answer, text: text ?? null });
  } catch (e) {
    toast(e.message, true);
    for (const b of buttons) b.disabled = false;
  }
}

// The poll counter each pane captured when its refresh was pressed; the button
// spins until the live counter moves past it. null = not spinning.
const spinFloor = { pr: null, review: null };

/**
 * A ↻ that forces a poll and spins until the poll it triggered lands.
 * `pollCount` is the pane's monotonic poll counter from the snapshot; `endpoint`
 * is the POST that pulses that poller. Used by both the PR and review panes.
 */
function refreshButton(kind, pollCount, endpoint, polling) {
  const btn = el('span', 'rvrefresh', '↻');
  btn.title = 'Refresh now';
  btn.setAttribute('role', 'button');
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

// The version the user dismissed this session. A newer release than this shows
// again; the same one stays hidden until the next launch.
let updateDismissed = null;
function renderUpdate() {
  const bar = $('updatebar');
  const u = snap.update;
  if (!u || updateDismissed === u.latest) { bar.hidden = true; return; }
  const link = $('updatelink');
  link.textContent = `Update available — v${u.latest} (you have v${u.current}). Run mise up`;
  link.href = u.url || '#';
  $('updatex').onclick = () => { updateDismissed = u.latest; bar.hidden = true; };
  bar.hidden = false;
}

/* A session is one of two things: active, or a past conversation you can come
 * back to. The daemon's `exited` and `archived` are the same fact from here, and
 * neither is a state worth a word of its own in the rail. */
const isArchived = (s) => s.state.state === 'archived' || s.state.state === 'exited';

/** `spawn::PENDING_WORKTREE`: the workspace a worktree session sits in until
 *  `SessionStart` reports the name Claude Code gave it. */
const PENDING_WORKTREE = '\u2026creating';

/* A finished session that never had a turn wrote no transcript, so there is no
 * conversation to come back to — `claude --resume` answers "no conversation
 * found" and exits. Listing one is offering something that cannot work, so the
 * archive is conversations, not every session that ever stopped. */
const isConversation = (s) => isArchived(s) && s.has_transcript;

/** Newest first: `created_ms` is an age, so the smallest number is the newest. */
const byNewest = (a, b) => a.created_ms - b.created_ms;

/* Expanded per group and kept across renders. Main's two conversations and the
 * worktrees' twenty are not the same question. */
const showArchived = { main: false, worktrees: false };

function renderRail() {
  const rail = $('rail');
  rail.replaceChildren();

  const main = snap.workspaces.find((w) => w.is_main);
  const worktrees = snap.workspaces.filter((w) => !w.is_main);

  // Main is pinned first (§9).
  if (main) rail.appendChild(mainGroup(main));
  rail.appendChild(worktreeGroup(main?.id));

  // Its own pane below the scroller, so it stays put while sessions scroll.
  $('prpane').replaceChildren(prGroup());

  renderWaitbar();
}


/** Dot colour for a PR, sharing the session legend so one key covers both (§9). */
function prDot(p) {
  if (p.session) return 'auto';           // a session is holding it
  if (p.is_draft) return 'idle';
  if (p.needs_you) return 'blocked';
  if (p.checks === 'failing' || p.mergeable === 'CONFLICTING') return 'build';
  if (p.checks === 'passing') return 'ok';
  return 'idle';
}

let showPrs = true;

/** How to answer a PR's threads: asked per PR, not remembered.
 *
 *  `/resolve` spawns a session pinned to the PR worktree and runs the vendored
 *  prompt in a pane — the agent doing the reading, fixing and drafting while you
 *  supervise, the daemon making no irreversible write itself. The overlay answers
 *  the threads here instead: triage, a card per thread, one batched post. The
 *  overlay is still the unstable one, so the menu says so rather than one of the
 *  two being the default you fall into. */
function reviewButtons(p) {
  const wrap = el('span', 'prpair');

  const btn = el('button', 'pract', 'review ▾');
  btn.title = `Answer #${p.number}'s review threads`;
  btn.onclick = (ev) => {
    ev.stopPropagation();
    openMenu(ev, prMenu(p, btn));
  };
  wrap.appendChild(btn);

  return wrap;
}

/** Everything you can start from a PR row.
 *
 *  The same list behind the `review` button and behind a right-click, because
 *  they are the same question — "do something with this PR" — and having two
 *  different menus for it is how you end up hunting for the one that has the item
 *  you want. */
function prMenu(p, btn) {
  return [
    ['open in main checkout', null, () => openPr(p.number, 'main')],
    ['open in worktree', null, () => openPr(p.number, 'worktree')],
    ['resolve', null, () => runResolve(p.number, btn)],
    ['resolve in ui [beta]', null, () => openReview(p.number)],
  ];
}

/** Start a plain session on a PR: a worktree pinned to its head branch, or the
 *  main checkout moved onto it. */
async function openPr(number, where) {
  try {
    const r = await call(`/api/pr/${number}/open`, { where });
    pendingSelect = r.session;
    toast(`#${number} in ${r.workspace}`);
  } catch (e) {
    toast(e.message, true);
  }
}

/** Spawn the session that answers #`number`'s threads, and switch to its pane. */
async function runResolve(number, btn) {
  // No button when this came from a right-click on the row.
  if (btn) btn.disabled = true;
  try {
    const r = await call(`/api/pr/${number}/resolve`);
    pendingSelect = r.session;
    toast(`resolve ${number}`);
  } catch (e) {
    toast(e.message, true);
  } finally {
    if (btn) btn.disabled = false;
  }
}

/** A hand-triggered run against a PR. `action` is the endpoint; `label` is what
 *  the button says, because the endpoint's name is not the useful word on a row.
 *  A refusal from the guard table is shown verbatim: it is the whole point of
 *  triggering by hand. */
function actionButton(p, action, label) {
  const b = el('button', 'pract', label);
  b.title = 'Rebase on develop, fix what CI says, push — in a pane you can take over';
  b.onclick = async (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    b.disabled = true;
    try {
      const r = await call(`/api/pr/${p.number}/${action}`);
      pendingSelect = r.session;
      toast(`${label} ${p.number}`);
    } catch (e) {
      toast(e.message, true);
    } finally {
      b.disabled = false;
    }
  };
  return b;
}

function prGroup() {
  const prs = snap.prs || [];
  // Just `ws`: the pinned pane it lives in owns the sizing, and carrying
  // `prblock` here too applied max-height twice, nested.
  const group = el('div', 'ws');

  const head = el('button', 'prgroup-head');
  head.setAttribute('aria-expanded', String(showPrs));
  head.appendChild(el('span', 'caretr', '›'));
  head.appendChild(el('span', 'eyebrow', 'PRs'));

  // The summary sits where the detail already is, rather than duplicated at
  // the top of the rail (§9).
  const count = el('span', 'prcount');
  if (snap.pr_error) {
    count.appendChild(el('b', 'f', 'unavailable'));
    head.title = snap.pr_error;
  } else {
    const needs = prs.filter((p) => p.needs_you).length;
    const failing = prs.filter(
      (p) => p.checks === 'failing' || p.mergeable === 'CONFLICTING').length;
    const bits = [`${prs.length}`];
    if (needs) bits.push(`${needs} needs you`);
    if (failing) bits.push(`${failing} failing`);
    count.appendChild(el('b', null, bits.join(' · ')));
    if (needs) count.querySelector('b').classList.add('n');
  }
  head.appendChild(count);
  head.appendChild(refreshButton('pr', snap.pr_poll ?? 0, '/api/prs/refresh', snap.pr_polling));
  head.onclick = () => { showPrs = !showPrs; renderRail(); };
  group.appendChild(head);

  if (!showPrs) return group;

  if (snap.pr_error) {
    const e = el('div', 'railbtn', snap.pr_error.slice(0, 120));
    e.style.color = 'var(--bad)';
    group.appendChild(e);
    return group;
  }
  if (!prs.length) {
    group.appendChild(el('div', 'railbtn', 'none open'));
    return group;
  }

  for (const p of prs) {
    // Rows for PRs that already have a session are dimmed and jump to it (§9).
    const row = el('a', 'prrow' + (p.session ? ' linked' : ''));
    row.href = p.url || '#';
    row.oncontextmenu = (ev) => openMenu(ev, prMenu(p, null));
    // ⌘-click, middle-click and copy-link all behave, and the browser already
    // holds the GitHub session.
    row.target = '_blank';
    row.rel = 'noreferrer';
    row.appendChild(el('span', 'dot ' + prDot(p)));
    row.appendChild(el('span', 'num', `#${p.number}`));
    row.appendChild(el('span', 'ttl', p.title));

    const auto0 = (snap.automation || {})[p.number];
    const needsResolve0 = p.needs_you;
    const needsFix0 = p.checks === 'failing' || p.mergeable === 'CONFLICTING';

    // A reason chip next to a button just repeats it and steals width from the
    // title, which is the part you actually read.
    if (!needsResolve0 && !needsFix0) {
      const why = [];
      if (p.unresolved_capped) why.push('50+ threads');
      if (p.children && p.children.length) why.push(`${p.children.length} stacked`);
      if (p.is_draft) why.push('draft');
      if (why.length) row.appendChild(el('span', 'link', why[0]));
    }

    // Both skills are hand-triggered. fix-pr is deliberately not automatic:
    // the guard table is a gate you read, not one that trips behind you.
    const auto = auto0, needsResolve = needsResolve0, needsFix = needsFix0;

    if (auto && auto.state === 'running') {
      const b = el('span', 'pract running', 'fixing');
      b.title = 'Jump to the run';
      b.onclick = (ev) => { ev.preventDefault(); ev.stopPropagation(); select(auto.session); };
      row.appendChild(b);
    } else {
      if (auto && auto.state === 'exhausted') {
        // The skill stopped without turning it green: it wants you.
        row.appendChild(el('span', 'why gaveup', 'gave up'));
      }
      if (needsResolve) row.appendChild(reviewButtons(p));
      if (needsFix) row.appendChild(actionButton(p, 'fix-pr', 'fix'));
    }

    // The row opens the PR; jumping to its session is the explicit chip, so
    // one does not swallow the other.
    if (p.session) {
      const j = el('button', 'jump', 'jump');
      j.title = 'Go to the session on this branch';
      j.onclick = (ev) => {
        ev.preventDefault();
        ev.stopPropagation();
        select(p.session);
      };
      row.appendChild(j);
    }
    group.appendChild(row);
  }
  return group;
}

/** A label and the button that adds to the group under it. */
function groupHead(label, add) {
  const head = el('div', 'ws-head');
  const name = el('div', 'ws-name');
  name.appendChild(el('span', 'eyebrow', label));
  head.appendChild(name);
  head.appendChild(add);
  return head;
}

/** Main is exclusive: one active session at a time, and no queue. While it is
 *  occupied the button is disabled and the row that holds it says so (§2). */
function mainGroup(w) {
  const group = el('div', 'ws');
  const sessions = sessionsOf(w.id);
  const active = sessions.filter((s) => !isArchived(s));
  const occupant = active.find((s) => s.id === w.occupant && s.alive);

  const add = el('button', 'plus', '+');
  add.disabled = !!occupant;
  add.title = occupant
    ? `main is held by ${occupant.title || occupant.id.slice(0, 8)}`
    : 'New session in main';
  add.onclick = () => newSession(w.id);
  group.appendChild(groupHead('Main checkout', add));

  for (const s of active.sort(byNewest)) group.appendChild(sessionRow(s, w));
  if (!active.length) group.appendChild(el('div', 'railbtn', 'no sessions'));
  appendArchived(group, 'main', sessions.filter(isConversation));
  return group;
}

/** Every worktree session under one header.
 *
 *  Rows come from sessions, not from worktrees: a worktree with nothing running
 *  in it is not something you can act on, so it gets no row. The one exception
 *  is a session whose worktree has no name yet, which shows as `…creating`
 *  rather than nothing at all — an invisible session is how you end up
 *  starting a second one. */
function worktreeGroup(mainId) {
  const group = el('div', 'ws');
  const add = el('button', 'plus', '+');
  add.title = 'New worktree session (shift-click to name it)';
  add.onclick = (ev) => newWorktree(ev.shiftKey);
  group.appendChild(groupHead('Worktrees', add));

  /* Anything that is not main's belongs here — by session, not by workspace.
   * A worktree Claude Code has not named yet has no workspace record at all,
   * only a session pointing at the placeholder, so filtering on the known
   * workspaces dropped exactly the row that says something is happening. */
  const sessions = snap.sessions.filter((s) => s.workspace !== mainId);
  const active = sessions.filter((s) => !isArchived(s));

  for (const s of active.sort(byNewest)) {
    // The workspace is only needed for the name it lends the row.
    group.appendChild(sessionRow(s, { id: s.workspace }));
  }
  if (!active.length) group.appendChild(el('div', 'railbtn', 'no sessions'));
  appendArchived(group, 'worktrees', sessions.filter(isConversation));
  return group;
}

/** The group's past conversations, behind a count.
 *
 *  Collapsed by default, because history is not what the rail is for — but
 *  opened whenever the conversation you are looking at is in here, so the rail
 *  never goes silent about what the centre pane is showing.
 */
function appendArchived(group, key, sessions) {
  if (!sessions.length) return;
  const open = showArchived[key] || sessions.some((s) => s.id === selected);

  const toggle = el('button', 'arctoggle');
  toggle.setAttribute('aria-expanded', String(open));
  toggle.appendChild(el('span', 'caretr', '\u203a'));
  toggle.appendChild(el('span', null, 'archived'));
  toggle.appendChild(el('span', 'arccount', String(sessions.length)));
  toggle.onclick = () => { showArchived[key] = !open; renderRail(); };
  group.appendChild(toggle);

  if (!open) return;
  for (const s of sessions.sort(byNewest)) group.appendChild(archivedRow(s));
}

/** A past conversation: which worktree it was in, and how long ago.
 *
 *  No state word — `archived` is the state, and the section it sits in already
 *  says it. Clicking rebuilds what it needs and resumes it. */
function archivedRow(s) {
  const btn = el('button', 'sess arc');
  btn.setAttribute('aria-current', String(s.id === selected));

  const row = el('div', 'sess-row');
  row.appendChild(el('span', 'dot archived'));
  row.appendChild(el('span', 'sess-name', railName(s, { id: s.workspace })));
  row.appendChild(el('span', 'sess-id', duration(sinceSnap(s.created_ms)) + ' ago'));
  btn.appendChild(row);

  if (!s.resumable) {
    // The transcript is readable, the conversation cannot be continued (§2).
    btn.appendChild(el('div', 'sess-sub', 'transcript only'));
  }
  btn.onclick = () => openArchived(s);
  btn.oncontextmenu = (ev) => openMenu(ev, [
    ['Fork session', null, s.has_transcript && s.resumable ? () => forkSession(s) : null],
    ['Delete session', 'bad', () => deleteSession(s)],
  ]);
  return btn;
}

/** Continue a past conversation, rebuilding its worktree first if it is gone. */
async function openArchived(s) {
  if (!s.resumable) {
    toast('transcript only: the branch is gone and the commit is unreachable', true);
    return;
  }
  try {
    const r = await call(`/api/session/${s.id}/resume`);
    // A resumed session keeps its id, because `claude --resume <id>` continues
    // that same conversation. So the dead terminal is still in `terms` under the
    // key the new pty wants, and `openTerm` would hand back the corpse — you
    // resume and stare at the old scrollback with a closed socket.
    closeTerm(`session:${r.session}`);
    pendingSelect = r.session;
    // The branch moved since the conversation happened, so the files it talks
    // about are not the files on disk. Worth saying, not worth refusing over.
    if (r.warning) toast(r.warning, true);
  } catch (e) {
    toast(e.message, true);
  }
}

/** Two lines: dot + name, then state and duration. No dirty-file count — that
 *  lives in the right column, one click away (§9). */
/** A worktree Claude Code has not named yet (§2): the daemon knows the session
 *  before it knows where it lives. */
const pending = (s) => s.workspace === PENDING_WORKTREE;

/** What the row calls itself.
 *
 *  In order of how much it tells you: the PR an automation run is working on, the
 *  name Claude Code gave the conversation, then the workspace it sits in. The
 *  workspace is last because several sessions share one, so on its own it is the
 *  fact that distinguishes them least.
 *
 *  The placeholder workspace id is the daemon's own bookkeeping, so a worktree
 *  still being cut says what is happening instead. */
function railName(s, w) {
  if (pending(s)) return 'creating worktree';
  // An automation row's workspace is `pr-10006`, which repeats the number it is
  // about to print and says nothing else. The PR's own title is already in the
  // snapshot, put there for the pane at the bottom of this rail.
  if (s.kind.kind === 'automation') {
    const pr = (snap.prs || []).find((p) => p.number === s.kind.pr);
    return pr ? `#${s.kind.pr} ${pr.title}` : `#${s.kind.pr}`;
  }
  return s.title || w.id;
}

function sessionRow(s, w) {
  const btn = el('button', 'sess' + (s.kind.kind === 'automation' ? ' auto' : ''));
  btn.setAttribute('aria-current', String(s.id === selected));

  const row = el('div', 'sess-row');
  row.appendChild(el('span', 'dot ' + dotClass(s)));
  row.appendChild(el('span', 'sess-name' + (pending(s) ? ' pending' : ''), railName(s, w)));
  // Worktree rows all carry their worktree's name, so without this the ones
  // sharing a worktree read identically.
  row.appendChild(el('span', 'sess-id', s.id.slice(0, 8)));
  btn.appendChild(row);

  const sub = el('div', 'sess-sub');
  // Which run it is. The name above says which PR, and `fix-pr` and `resolve` do
  // very different things to it.
  if (s.kind.kind === 'automation') sub.appendChild(el('span', 'sess-cmd', s.kind.command));
  sub.appendChild(el('span', 'sess-state ' + stateClass(s), stateLabel(s)));
  // The waiting duration is the number to optimise down (§2). A start has a
  // clock for a different reason: `claude --worktree` cuts the worktree and runs
  // the repo's link hooks before it says anything, which is ten seconds of
  // nothing. A number that moves is the difference between slow and hung.
  if (isWaiting(s) && s.waiting_ms != null) {
    sub.appendChild(el('span', null, duration(sinceSnap(s.waiting_ms))));
  } else if (s.state.state === 'starting') {
    sub.appendChild(el('span', null, duration(sinceSnap(s.created_ms))));
  }
  btn.appendChild(sub);

  // An agent editing outside its worktree is a prompt problem worth seeing,
  // not noise to swallow (§11).
  if (s.boundary_violations.length) {
    btn.appendChild(el('span', 'sess-warn',
      `${s.boundary_violations.length} blocked edit(s) outside the worktree`));
  }

  btn.appendChild(el('div', 'sess-pad'));
  btn.onclick = () => select(s.id);
  // The header's ✕ only ever closes the selected session, so closing any other
  // one meant switching to it first.
  btn.oncontextmenu = (ev) => openMenu(ev, [
    // Nothing to branch off until the conversation has had a turn.
    ['Fork session', null, s.has_transcript ? () => forkSession(s) : null],
    ['Close session', 'bad', s.alive ? () => closeSession(s.id) : null],
    ['Delete session', 'bad', () => deleteSession(s)],
  ]);
  return btn;
}

/** Branch off a conversation: same context, new session, original untouched.
 *
 *  No `closeTerm` unlike resume, which keeps the old id and would otherwise hand
 *  back the dead terminal. A fork has an id of its own and nothing to collide
 *  with. */
async function forkSession(s) {
  try {
    const r = await call(`/api/session/${s.id}/fork`);
    pendingSelect = r.session;
    toast('forked');
    // The branch moved on since the conversation, same as resume: worth saying,
    // not worth refusing over.
    if (r.warning) toast(r.warning, true);
  } catch (e) {
    toast(e.message, true);
  }
}

/** Forget a session: the row, the record, and the daemon's own copy of the
 *  transcript.
 *
 *  Confirmed, unlike closing, because closing is reversible in the way that
 *  matters — the conversation is still there to resume — and this is not. The
 *  wording says what survives, so "delete" does not have to be read as deleting
 *  the conversation itself. */
function deleteSession(s) {
  const name = railName(s, { id: s.workspace });
  const ending = s.alive ? 'It is still running, so this ends it first. ' : '';
  if (!confirm(`Delete "${name}"?\n\n${ending}The row and orchd's copy of the `
    + "transcript go for good. Claude Code's own transcript is left where it is.")) return;
  call(`/api/session/${s.id}/delete`)
    .then(() => toast('deleted'))
    .catch((e) => toast(e.message, true));
}

/** End a session: kills the pty, keeps the row and its scrollback (§2). */
function closeSession(id) {
  // Claude takes several seconds to shut down and the row only turns `exited`
  // once the daemon sees it go, so without this the click reads as a no-op.
  call(`/api/session/${id}/kill`)
    .then(() => toast('closing session'))
    .catch((e) => toast(e.message, true));
}

/** The rail exists to surface idle agents, so the count sits at the top of it. */
function renderWaitbar() {
  const waiting = snap.sessions.filter(isWaiting);
  const bar = $('waitbar');
  if (!waiting.length) {
    bar.className = 'waitbar';
    return;
  }
  const longest = waiting.reduce(
    (a, b) => ((a.waiting_ms ?? 0) >= (b.waiting_ms ?? 0) ? a : b));
  bar.className = 'waitbar on';
  bar.textContent = `${waiting.length} waiting · longest ${duration(sinceSnap(longest.waiting_ms) ?? 0)}`;
  bar.onclick = () => select(longest.id);
}

function currentSession() {
  return snap.sessions.find((s) => s.id === selected) || null;
}

/** The workspace the right pane describes: the one you are working in.
 *
 *  Deliberately not `currentWorkspaceId`, which falls back to main so the drawer
 *  and the shell button always have somewhere to act. A file list has no such
 *  duty: main's tree is not "your changes" just because you closed your session,
 *  and a pane still listing a finished session's work reads as live. */
function activeWorkspaceId() {
  const s = currentSession();
  return s && !isArchived(s) ? s.workspace : null;
}

function currentWorkspaceId() {
  const s = currentSession();
  if (s) return s.workspace;
  return snap.workspaces.find((w) => w.is_main)?.id ?? null;
}

function renderContext() {
  const s = currentSession();
  const wsId = currentWorkspaceId();
  const w = snap.workspaces.find((x) => x.id === wsId);

  // PRs are opened against upstream while branches live on the fork (§6), so
  // the header names both rather than collapsing them into one path.
  const repos = snap.repos || {};
  $('repoupstream').textContent = repos.upstream
    || (w ? w.path.split('/').slice(-2).join('/') : '—');
  $('repofork').textContent = repos.fork || '';
  $('ctxdot').className = 'dot ' + (s ? dotClass(s) : 'idle');
  $('ctxname').textContent = s ? railName(s, { id: wsId }) : (wsId || 'no session');
  $('ctxbranch').textContent = w ? (w.branches[0] || '') : '';
  const pr = wsId ? prForWorkspace(wsId) : null;
  const bits = [];
  if (s) bits.push(stateLabel(s));
  if (pr) {
    const state = pr.awaiting_you ? `${pr.awaiting_you} waiting on you`
      : pr.mergeable === 'CONFLICTING' ? 'conflicted'
        : pr.checks === 'failing' ? 'checks failing'
          : pr.checks === 'pending' ? 'checks running'
            : pr.is_draft ? 'draft' : 'clean';
    bits.push(`#${pr.number} ${state}`);
  }
  $('ctxstate').textContent = bits.join(' · ');
  $('killbtn').style.display = s && s.alive ? '' : 'none';

}

// ---------------------------------------------------------------------------
// Drawer — available on every workspace, not just main (§9)
// ---------------------------------------------------------------------------

function renderDrawer() {
  const wsId = currentWorkspaceId();
  const w = snap.workspaces.find((x) => x.id === wsId);
  const tabs = $('dtabs');
  tabs.replaceChildren();

  // Docker stack status, in place of the path: blue = up, red = down.
  const dcwd = $('dcwd');
  dcwd.replaceChildren();
  const up = snap.stack_up === true;
  dcwd.appendChild(el('span', 'stackdot ' + (up ? 'up' : 'down')));
  dcwd.appendChild(el('span', null, up ? 'stack up' : 'stack down'));

  const procs = w ? w.processes : [];
  const drawer = $('drawer');

  // On a worktree the drawer starts empty and is a thin bar until you open
  // something. `collapsed` is that same bar chosen on purpose while processes
  // run — only offered, and only honoured, when there is a body to hide.
  const collapsed = drawerCollapsed && procs.length > 0;
  drawer.className = 'drawer' + (procs.length ? '' : ' empty') + (collapsed ? ' collapsed' : '');
  const toggle = $('dcollapse');
  toggle.hidden = procs.length === 0;
  toggle.textContent = collapsed ? '▸' : '▾';
  toggle.title = collapsed ? 'Expand processes' : 'Collapse processes';

  const alive = (p) =>
    p.kind.kind === 'shell' ? p.kind.exit_code == null : p.health.health !== 'dead';

  let active = selectedProc[wsId];
  if (!procs.some((p) => p.id === active)) {
    // Prefer something still running; a dead shell is only shown when it is
    // all there is, or when you picked it yourself.
    active = (procs.find(alive) ?? procs[0])?.id ?? null;
  }
  selectedProc[wsId] = active;

  // Shells are numbered per workspace. Without this every dead one renders as
  // the same "shell (0)" and a drawer with three corpses in it is unreadable.
  let shellNo = 0;
  for (const p of procs) {
    const isShell = p.kind.kind === 'shell';
    if (isShell) shellNo += 1;
    const dead = isShell ? p.kind.exit_code != null : p.health.health === 'dead';

    const tab = el('button', 'dtab' + (dead ? ' dead' : ''));
    tab.setAttribute('aria-selected', String(p.id === active));
    const health = p.health.health;
    const cls = health === 'failing' ? 'build'
      : health === 'ok' ? 'working'
        : health === 'dead' ? 'idle' : 'working';
    tab.appendChild(el('span', 'dot ' + cls));
    const label = isShell
      ? (dead ? `shell ${shellNo} · exit ${p.kind.exit_code}` : `shell ${shellNo}`)
      : p.name;
    tab.appendChild(el('span', null, label));
    tab.onclick = () => { selectedProc[wsId] = p.id; drawerTouched = true; renderDrawer(); };

    const x = el('span', 'x', '×');
    x.title = dead ? 'Dismiss' : 'Close';
    x.onclick = (ev) => {
      ev.stopPropagation();
      closeTerm(`proc:${p.id}`);
      call(`/api/process/${encodeURIComponent(p.id)}/close`).catch((e) => toast(e.message, true));
    };
    tab.appendChild(x);

    if (p.kind.kind === 'managed') {
      const r = el('span', 'x', '⟳');
      r.title = 'Restart';
      r.onclick = (ev) => {
        ev.stopPropagation();
        closeTerm(`proc:${p.id}`);
        call(`/api/workspace/${encodeURIComponent(wsId)}/process/${encodeURIComponent(p.name)}/restart`)
          .catch((e) => toast(e.message, true));
      };
      tab.appendChild(r);
    }
    tabs.appendChild(tab);
  }

  const shown = showTerm(active ? `proc:${active}` : null, $('drawerbody'));
  if (shown && pendingProcFocus && active === pendingProcFocus) {
    pendingProcFocus = null;
    // After the frame that un-hides it: xterm refuses focus while its host has no
    // dimensions, which is exactly the state it is in right now.
    requestAnimationFrame(() => {
      try {
        shown.term.focus();
      } catch (e) { /* disposed while we waited */ }
    });
  }

  // Auto-expand when a managed process goes red.
  const failing = procs.find((p) => p.health.health === 'failing');
  if (failing && selectedProc[wsId] !== failing.id && !drawerTouched) {
    selectedProc[wsId] = failing.id;
    showTerm(`proc:${failing.id}`, $('drawerbody'));
  }
}

let drawerTouched = false;
/* Collapsed to its header on purpose, remembered across reloads like the column
 * widths and the drawer height. Persisted so the next render (and the next boot)
 * does not silently reopen it — the whole point, now that ng-watch means main
 * always has a process and so the drawer is otherwise always open there. */
let drawerCollapsed = localStorage.getItem('orch.drawerCollapsed') === '1';

function setDrawerCollapsed(v) {
  drawerCollapsed = v;
  try {
    localStorage.setItem('orch.drawerCollapsed', v ? '1' : '0');
  } catch (e) { /* private mode: the toggle still holds for this session */ }
  renderDrawer();
  // The terminal above reclaims (or yields) the drawer's height; xterm only
  // refits on an explicit nudge, not on a sibling's size change.
  refitTerms();
}
/** A shell whose terminal should take the cursor as soon as it exists. */
let pendingProcFocus = null;

// ---------------------------------------------------------------------------
// Files — changed files for the selected session's workspace (§9)
// ---------------------------------------------------------------------------

/** Behind/ahead against upstream/develop, with the one action worth offering.
 *
 *  The changed-file list is a poor summary of a branch that has simply fallen
 *  behind: what you want then is to take develop in, not to read a list. */
function renderDivergence(w) {
  const box = $('diverge');
  box.replaceChildren();
  // Reset the class too, not just the children: `on` is what makes this visible,
  // and leaving it behind left an empty bar above the file list.
  box.className = 'diverge';
  if (!w) return;

  if (w.rebasing) {
    box.className = 'diverge on bad';
    box.appendChild(el('span', 'dvtext', 'rebase stopped on conflicts'));
    const a = el('button', 'dvbtn', 'Abort');
    a.onclick = () => act(`/api/workspace/${encodeURIComponent(w.id)}/rebase/abort`, 'aborted');
    box.appendChild(a);
    return;
  }
  if (!w.behind) {
    box.className = 'diverge';
    return;
  }

  box.className = 'diverge on';
  const ahead = w.ahead ? `, ${w.ahead} ahead` : '';
  box.appendChild(el('span', 'dvtext',
    `${w.behind} behind upstream/develop${ahead}`));
  const b = el('button', 'dvbtn', 'Rebase');
  // Never a merge: history stays linear.
  b.title = `git rebase upstream/develop in ${w.id}`;
  b.onclick = async () => {
    b.disabled = true;
    await act(`/api/workspace/${encodeURIComponent(w.id)}/rebase`, 'rebased');
    b.disabled = false;
  };
  box.appendChild(b);
}

async function act(path, verb) {
  try {
    await call(path);
    toast(verb);
  } catch (e) {
    toast(e.message, true);
  }
}

function renderFiles() {
  // The diff overlay is opened against a workspace and keeps describing it while
  // it is open, session or no session.
  const wsId = diffState.open ? diffState.ws : activeWorkspaceId();
  const w = snap.workspaces.find((x) => x.id === wsId);
  renderDivergence(w);
  const panes = $('filepanes');
  panes.replaceChildren();

  $('filestitle').textContent = diffState.open ? 'Changeset' : 'Changes';

  if (!w) {
    const s = currentSession();
    panes.appendChild(el('div', 'fempty', s && pending(s)
      ? 'Creating the worktree…'
      : 'No session open.'));
    $('filesfoot').textContent = '';
    $('filesbase').textContent = '';
    return;
  }

  /* One list, one meaning: everything this workspace changed since it branched.
   *
   * Not `git status`, which is uncommitted work only — a session that commits
   * would empty its own pane. Not a diff against develop's tip either, which
   * would add every file a colleague landed meanwhile. The base is the
   * merge-base, so the list is what happened *here*.
   *
   * With the diff open the same question is asked of the diff's own summary,
   * which carries line counts per file and a cursor. */
  const sum = diffState.open ? diffState.summary : null;
  const files = sum ? sum.files : (w.changed || []);
  const since = sum ? sum.base : w.changed_since;

  for (const f of files) {
    const row = el('button', sum ? 'dfrow' : 'frow');
    if (sum) row.setAttribute('aria-current', String(f.path === diffState.path));
    const letter = (f.status || 'M')[0];
    row.appendChild(el('span', 'fst ' + letter, letter));
    const n = el('span', 'fname');
    n.textContent = '\u202a' + f.path;
    n.title = f.old_path ? `${f.old_path} → ${f.path}` : f.path;
    row.appendChild(n);
    const nums = el('span', 'dfnum');
    if (f.binary) {
      nums.textContent = 'bin';
    } else if (f.status === '?') {
      // Untracked: entirely new by definition, so a count would only ever say
      // "all of it".
      nums.appendChild(el('span', 'p', 'new'));
    } else {
      nums.appendChild(el('span', 'p', `+${f.added}`));
      nums.appendChild(document.createTextNode(' '));
      nums.appendChild(el('span', 'm', `\u2212${f.deleted}`));
    }
    row.appendChild(nums);
    row.onclick = () => {
      if (sum) {
        diffState.cursor = 0;
        diffState.context = 3;
        loadFile(f.path);
      } else {
        openDiff(f.path);
      }
    };
    panes.appendChild(row);
  }

  if (!files.length) {
    panes.appendChild(el('div', 'fempty',
      w.is_main ? 'Nothing changed in the main checkout.' : 'Nothing changed in this worktree yet.'));
  }

  const bits = [`${files.length} file${files.length === 1 ? '' : 's'}`];
  if (sum) bits.push(`+${sum.added} \u2212${sum.deleted}`);
  if (w.is_main) bits.push('worktrees excluded');
  $('filesfoot').textContent = bits.join(' \u00b7 ');
  // The base belongs in the header, where the toggle used to be: it is the one
  // thing you need to know to read the list, and it is not a choice.
  $('filesbase').textContent = since ? `since ${since.slice(0, 7)}` : '';
}

/** The PR whose head ref this workspace holds, if any. */
function prForWorkspace(wsId) {
  return (snap.prs || []).find((p) => p.workspace === wsId) || null;
}

// ---------------------------------------------------------------------------
// Diff (§5)
// ---------------------------------------------------------------------------

// Kept short: the right header also carries the title and the refresh control,
// and a long label wraps it onto two lines.
const diffState = {
  open: false,
  /* Which workspace the open diff describes. Every fetch used to read the
   * current one at call time, so switching sessions left the loaded hunks
   * describing the old worktree while the next request quietly asked about the
   * new one. Pinned here, and re-pointed by `syncDiffToSession`. */
  ws: null,
  base: 'upstream',
  summary: null,     // { base, files, added, deleted }
  path: null,
  file: null,        // { path, hunks, binary }
  split: true,
  cursor: 0,         // index into the current file's change blocks
  /* Where to land the cursor once the next file's blocks are built, when a step
   * crossed a file boundary: 'first' arriving from below, 'last' from above.
   * Null on a normal load so the cursor is just clamped to what fits. */
  pendingCursor: null,
  context: 3,
};

/* Byte offsets come from Rust; JS strings are UTF-16. Decode through the byte
   array rather than assuming ASCII, or a line with an accent in it highlights
   the wrong span. */
const ENC = new TextEncoder();
const DEC = new TextDecoder();

/* Prism is vendored whole (every grammar) and driven for its token stream only,
 * never its markup: the daemon already marks the changed slices of a line, and
 * those `.w-add`/`.w-del` ranges have to interleave with the syntax spans rather
 * than nest inside them. So highlighting is flattened to ranges and merged with
 * the word ranges below. Language is the file's extension; anything unmapped —
 * or a grammar Prism does not carry — falls back to plain text, never an error. */
const EXT_LANG = {
  js: 'javascript', mjs: 'javascript', cjs: 'javascript', jsx: 'jsx',
  ts: 'typescript', tsx: 'tsx', rs: 'rust', py: 'python', rb: 'ruby', go: 'go',
  c: 'c', h: 'c', cpp: 'cpp', cc: 'cpp', cxx: 'cpp', hpp: 'cpp', cs: 'csharp',
  java: 'java', kt: 'kotlin', swift: 'swift', php: 'php', sh: 'bash', bash: 'bash',
  zsh: 'bash', fish: 'bash', css: 'css', scss: 'scss', sass: 'sass', less: 'less',
  html: 'markup', htm: 'markup', xml: 'markup', svg: 'markup', vue: 'markup',
  json: 'json', yaml: 'yaml', yml: 'yaml', toml: 'toml', ini: 'ini', cfg: 'ini',
  md: 'markdown', markdown: 'markdown', sql: 'sql', graphql: 'graphql', gql: 'graphql',
  lua: 'lua', pl: 'perl', r: 'r', dart: 'dart', scala: 'scala', clj: 'clojure',
  ex: 'elixir', exs: 'elixir', erl: 'erlang', hs: 'haskell', ml: 'ocaml',
  tf: 'hcl', hcl: 'hcl', proto: 'protobuf', diff: 'diff', patch: 'diff',
  vim: 'vim', nix: 'nix', zig: 'zig', jl: 'julia', groovy: 'groovy', gradle: 'groovy',
};
const BASENAME_LANG = {
  dockerfile: 'docker', makefile: 'makefile', 'cargo.lock': 'toml',
  'go.mod': 'go', 'go.sum': 'go',
};
function langFor(path) {
  if (!path || !window.Prism) return null;
  const base = path.split('/').pop().toLowerCase();
  const byName = BASENAME_LANG[base];
  if (byName) return Prism.languages[byName] ? byName : null;
  const dot = base.lastIndexOf('.');
  const lang = EXT_LANG[dot >= 0 ? base.slice(dot + 1) : ''];
  return lang && Prism.languages[lang] ? lang : null;
}

/** Prism's nested token tree, flattened to non-overlapping `{s,e,cls}` ranges in
 *  character offsets. The deepest token wins, which is what falls out of only
 *  emitting a range at each string leaf. */
function hlTokens(text, lang) {
  if (!lang) return [];
  let tree;
  try { tree = Prism.tokenize(text, Prism.languages[lang]); }
  catch (e) { return []; }
  const out = [];
  let pos = 0;
  (function walk(arr, inherited) {
    for (const t of arr) {
      if (typeof t === 'string') {
        if (inherited) out.push({ s: pos, e: pos + t.length, cls: inherited });
        pos += t.length;
      } else {
        const ty = (t.alias && (Array.isArray(t.alias) ? t.alias[0] : t.alias)) || t.type;
        if (typeof t.content === 'string') {
          out.push({ s: pos, e: pos + t.content.length, cls: ty });
          pos += t.content.length;
        } else {
          walk(t.content, ty);
        }
      }
    }
  })(tree, null);
  return out;
}

/** Split a line at every boundary — syntax-token edges and word-diff edges both
 *  — so each segment can carry a token colour and a change background at once. */
function lineSegments(text, words, lang) {
  const toks = hlTokens(text, lang);
  const bset = new Set([0, text.length]);
  for (const t of toks) { bset.add(t.s); bset.add(t.e); }
  for (const w of words) { bset.add(w.s); bset.add(w.e); }
  const pts = [...bset].filter((p) => p >= 0 && p <= text.length).sort((a, b) => a - b);
  const segs = [];
  for (let k = 0; k < pts.length - 1; k++) {
    const s = pts[k], e = pts[k + 1];
    if (s === e) continue;
    const tok = toks.find((t) => t.s <= s && t.e >= e);
    const word = words.some((w) => w.s <= s && w.e >= e);
    segs.push({ s, e, cls: tok ? tok.cls : null, word });
  }
  return segs;
}

/** The open-question detail is usually a commit diff and a reply, with no file to
 *  name a language from, so it is coloured as a *diff*: whole +/- lines, headers
 *  neutral. Prism's diff grammar is line-aware — a `---`/`+++` header is `coord`,
 *  not a deletion, so the `--- the reply ---` separator does not read as removed.
 *  Only the top-level (per-line) token is taken, so the whole line is coloured
 *  rather than the sign alone. Non-diff prose has no diff tokens and stays plain. */
function tokenLen(x) {
  if (typeof x === 'string') return x.length;
  if (Array.isArray(x)) return x.reduce((a, c) => a + tokenLen(c), 0);
  return tokenLen(x.content);
}
function diffRanges(text) {
  if (!window.Prism || !Prism.languages.diff) return [];
  let toks;
  try { toks = Prism.tokenize(text, Prism.languages.diff); }
  catch (e) { return []; }
  const out = [];
  let pos = 0;
  for (const t of toks) {
    if (typeof t === 'string') { pos += t.length; continue; }
    const ty = (t.alias && (Array.isArray(t.alias) ? t.alias[0] : t.alias)) || t.type;
    const len = tokenLen(t);
    out.push({ s: pos, e: pos + len, cls: ty });
    pos += len;
  }
  return out;
}
function detailEl(text) {
  const pre = el('pre', 'oqd');
  const ranges = diffRanges(text);
  if (!ranges.length) { pre.textContent = text; return pre; }
  let at = 0;
  for (const r of ranges) {
    if (r.s > at) pre.appendChild(document.createTextNode(text.slice(at, r.s)));
    pre.appendChild(el('span', 'tok-' + r.cls, text.slice(r.s, r.e)));
    at = r.e;
  }
  if (at < text.length) pre.appendChild(document.createTextNode(text.slice(at)));
  return pre;
}

function lineEl(row, side) {
  // side: 'old' | 'new'. In split view each pane shows only its own side.
  const empty = !row || (side === 'old' && row.kind === 'add') ||
                        (side === 'new' && row.kind === 'del');
  const div = el('div', 'ln' + (empty ? ' empty' : row.kind === 'add' ? ' add' : row.kind === 'del' ? ' del' : ''));
  const num = el('i', null, empty ? '' : String((side === 'old' ? row.old : row.new) ?? ''));
  div.appendChild(num);
  const body = el('s');
  if (!empty) {
    // Word ranges arrive as byte offsets (from Rust); Prism works on the JS
    // string. Convert the ranges to character offsets so the two line up, then
    // merge. A blank line still needs a space so the row has height.
    const bytes = ENC.encode(row.text);
    const b2c = (b) => DEC.decode(bytes.slice(0, b)).length;
    const words = (row.words || []).map(([ws, we]) => ({ s: b2c(ws), e: b2c(we) }));
    const segs = lineSegments(row.text, words, langFor(diffState.path));
    if (!segs.length) {
      body.textContent = row.text || ' ';
    } else {
      const wcls = row.kind === 'add' ? 'w-add' : 'w-del';
      for (const g of segs) {
        const t = row.text.slice(g.s, g.e);
        if (!g.cls && !g.word) { body.appendChild(document.createTextNode(t)); continue; }
        const cls = (g.cls ? 'tok-' + g.cls : '') + (g.word ? (g.cls ? ' ' : '') + wcls : '');
        body.appendChild(el('span', cls, t));
      }
    }
  }
  div.appendChild(body);
  return div;
}

/** Align a hunk's rows into side-by-side pairs.
 *
 *  The server emits deletions then additions; split view needs them abreast,
 *  padding the shorter run so the two panes stay in step. */
function pairRows(rows) {
  const out = [];
  let i = 0;
  while (i < rows.length) {
    if (rows[i].kind === 'context') {
      out.push([rows[i], rows[i]]);
      i += 1;
      continue;
    }
    const dels = [];
    const adds = [];
    while (i < rows.length && rows[i].kind === 'del') dels.push(rows[i++]);
    while (i < rows.length && rows[i].kind === 'add') adds.push(rows[i++]);
    const n = Math.max(dels.length, adds.length);
    for (let k = 0; k < n; k++) out.push([dels[k] ?? null, adds[k] ?? null]);
    // A row that is neither context nor a del/add run would loop forever.
    if (!dels.length && !adds.length) i += 1;
  }
  return out;
}

function renderDiff() {
  const body = $('diffbody');
  body.replaceChildren();
  body.className = 'diff' + (diffState.split ? ' split' : '');
  const f = diffState.file;

  $('ovpath').replaceChildren();
  if (diffState.path) {
    const parts = diffState.path.split('/');
    const name = parts.pop();
    $('ovpath').appendChild(el('span', null, parts.length ? parts.join('/') + '/' : ''));
    $('ovpath').appendChild(document.createTextNode(name));
  }
  $('ovmode').textContent = diffState.split ? 'Unified' : 'Split';

  const note = (t) => {
    body.appendChild(el('div', 'diffnote', t));
    $('ovcount').textContent = '';
    diffState.anchors = [];
  };
  if (!f) return note('Select a file.');
  if (f.binary) return note('Binary file — not shown.');
  if (!f.hunks.length) return note('No textual changes against this base.');

  const anchors = [];
  let block = -1;

  // Every row is three grid cells in split view and one in unified, so a fold
  // spanning the full width interleaves naturally between hunks.
  const push3 = (a, b) => {
    body.appendChild(a);
    body.appendChild(el('div', 'gutter'));
    body.appendChild(b);
  };

  for (const h of f.hunks) {
    if (h.gap_before > 0) {
      const b = el('div', 'fold', `⋯ ${h.gap_before} unchanged lines — click to expand`);
      b.onclick = () => {
        diffState.context = Math.min(diffState.context + Math.max(h.gap_before, 20), 10000);
        loadFile(diffState.path);
      };
      body.appendChild(b);
    }

    if (diffState.split) {
      let splitInBlock = false;
      for (const [o, n] of pairRows(h.rows)) {
        const lo = lineEl(o, 'old');
        const ro = lineEl(n, 'new');
        const changed = o?.kind === 'del' || n?.kind === 'add';
        if (changed) {
          if (!splitInBlock) { block += 1; anchors.push(lo); splitInBlock = true; }
          lo.dataset.blk = ro.dataset.blk = String(block);
        } else {
          splitInBlock = false;
        }
        push3(lo, ro);
      }
    } else {
      let inBlock = false;
      for (const r of h.rows) {
        const e = lineEl(r, r.kind === 'del' ? 'old' : 'new');
        if (r.kind !== 'context') {
          if (!inBlock) { block += 1; anchors.push(e); inBlock = true; }
          e.dataset.blk = String(block);
        } else {
          inBlock = false;
        }
        body.appendChild(e);
      }
    }
  }

  diffState.anchors = anchors;
  const last = Math.max(anchors.length - 1, 0);
  diffState.cursor = diffState.pendingCursor === 'last' ? last
    : diffState.pendingCursor === 'first' ? 0
      : Math.min(diffState.cursor, last);
  diffState.pendingCursor = null;
  markCursor();
}

function markCursor() {
  for (const e of $('diffbody').querySelectorAll('.ln.cur')) e.classList.remove('cur');
  const a = (diffState.anchors || [])[diffState.cursor];
  if (!a) return;
  a.classList.add('cur');
  a.scrollIntoView({ block: 'center', behavior: 'smooth' });
  // Within-file position, plus which file of the changeset when there is more
  // than one — the stepper walks the whole PR, so "3 of 7" alone would not say
  // where in it you are.
  const files = diffState.summary?.files || [];
  const fi = files.findIndex((f) => f.path === diffState.path);
  const where = files.length > 1 && fi >= 0 ? ` · file ${fi + 1} of ${files.length}` : '';
  $('ovcount').textContent = `change ${diffState.cursor + 1} of ${diffState.anchors.length}${where}`;
}

/** Walk to the next/previous change block, carrying on into the next file in the
 *  changeset's order rather than wrapping inside the current one. Files with no
 *  change blocks (binary, or nothing textual) are hopped over, and the whole
 *  changeset wraps end to end so the stepper never dead-ends. */
async function stepChange(delta) {
  const n = (diffState.anchors || []).length;
  const next = diffState.cursor + delta;
  if (n && next >= 0 && next < n) {
    diffState.cursor = next;
    markCursor();
    return;
  }

  const files = diffState.summary?.files || [];
  if (files.length < 2) {
    // Nowhere else to go: keep the old wrap so a single file still cycles.
    if (n) { diffState.cursor = (next + n) % n; markCursor(); }
    return;
  }
  let idx = files.findIndex((f) => f.path === diffState.path);
  if (idx < 0) idx = 0;
  // At most one lap; if every other file is blank we land back where we started.
  for (let hop = 0; hop < files.length; hop++) {
    idx = (idx + delta + files.length) % files.length;
    diffState.pendingCursor = delta > 0 ? 'first' : 'last';
    await loadFile(files[idx].path);
    if ((diffState.anchors || []).length) return;
  }
}

async function loadSummary() {
  const ws = diffState.ws || activeWorkspaceId();
  if (!ws) return;
  const q = new URLSearchParams({ workspace: ws, base: diffState.base });
  const pr = prForWorkspace(ws);
  if (pr && pr.base_ref) q.set('pr_base', pr.base_ref);
  try {
    diffState.summary = await get(`/api/diff?${q}`);
  } catch (e) {
    diffState.summary = null;
    toast(e.message, true);
  }
  renderFiles();
}

async function loadFile(path) {
  const ws = diffState.ws || activeWorkspaceId();
  if (!ws) return;
  if (editState.on && path !== editState.path && !closeEditor()) return;
  diffState.path = path;
  const q = new URLSearchParams({
    workspace: ws, base: diffState.base, path, context: String(diffState.context),
  });
  const pr = prForWorkspace(ws);
  if (pr && pr.base_ref) q.set('pr_base', pr.base_ref);
  try {
    diffState.file = await get(`/api/diff/file?${q}`);
  } catch (e) {
    diffState.file = null;
    toast(e.message, true);
  }
  renderDiff();
  renderFiles();
}

async function openDiff(path) {
  diffState.open = true;
  diffState.ws = activeWorkspaceId() || currentWorkspaceId();
  diffState.context = 3;
  $('overlay').classList.add('on');
  await loadSummary();
  const first = path || diffState.summary?.files?.[0]?.path;
  if (first) {
    diffState.cursor = 0;
    await loadFile(first);
  } else {
    renderDiff();
  }
}

function closeDiff() {
  if (editState.on && !closeEditor()) return;
  diffState.open = false;
  diffState.ws = null;
  diffState.file = null;
  diffState.path = null;
  $('overlay').classList.remove('on');
  renderFiles();
}




// ---------------------------------------------------------------------------
// Editable right pane (§5, step 9)
// ---------------------------------------------------------------------------

const editState = {
  on: false,
  path: null,
  version: null,     // what the buffer was loaded at
  dirty: false,
  watch: null,       // polls for someone editing underneath you
};

function editQuery(extra) {
  const ws = diffState.ws || activeWorkspaceId();
  const q = new URLSearchParams({ workspace: ws, path: diffState.path, ...extra });
  const pr = prForWorkspace(ws);
  if (pr && pr.base_ref) q.set('pr_base', pr.base_ref);
  return q;
}

async function openEditor() {
  if (!diffState.path || !diffState.file || diffState.file.binary) {
    return toast('nothing editable here', true);
  }
  let live, base;
  try {
    [live, base] = await Promise.all([
      get(`/api/file?${editQuery({})}`),
      get(`/api/file?${editQuery({ base: diffState.base })}`),
    ]);
  } catch (e) {
    return toast(e.message, true);
  }

  editState.on = true;
  editState.path = diffState.path;
  editState.version = live.version;
  editState.dirty = false;
  $('ovsave').hidden = false;
  $('ovedit').textContent = 'Cancel';

  const body = $('diffbody');
  body.replaceChildren();
  body.className = 'diff split editing';

  // The left pane stays the base revision, read-only: this is an editable
  // right pane, not a free-floating text editor.
  const left = el('pre', 'editbase');
  left.textContent = base.content;
  body.appendChild(left);
  body.appendChild(el('div', 'gutter'));

  const ta = el('textarea', 'editarea');
  ta.value = live.content;
  ta.spellcheck = false;
  ta.oninput = () => {
    editState.dirty = true;
    $('ovsave').textContent = 'Save •';
  };
  body.appendChild(ta);
  ta.focus();

  // Invalidation: an agent editing the same file underneath you must not be
  // discovered only at save time (§5).
  clearInterval(editState.watch);
  editState.watch = setInterval(checkUnderneath, 4000);
}

async function checkUnderneath() {
  if (!editState.on) return;
  try {
    const now = await get(`/api/file?${editQuery({})}`);
    if (now.version !== editState.version) {
      clearInterval(editState.watch);
      editState.watch = null;
      $('ovsave').textContent = 'Save (conflict)';
      toast('this file changed on disk — an agent is editing it too. Saving will be refused.', true);
    }
  } catch (e) {
    // A file that vanished is also a change worth knowing about, but not worth
    // a second alarm; the save will report it.
  }
}

function closeEditor(silent) {
  if (editState.on && editState.dirty && !silent
      && !confirm('Discard unsaved edits?')) return false;
  clearInterval(editState.watch);
  editState.watch = null;
  editState.on = false;
  editState.dirty = false;
  $('ovsave').hidden = true;
  $('ovsave').textContent = 'Save ⌘S';
  $('ovedit').textContent = 'Edit';
  renderDiff();
  return true;
}

async function saveEditor() {
  if (!editState.on) return;
  const ta = $('diffbody').querySelector('.editarea');
  if (!ta) return;
  let out;
  try {
    out = await call('/api/file', {
      workspace: currentWorkspaceId(),
      path: editState.path,
      content: ta.value,
      version: editState.version,
    });
  } catch (e) {
    return toast(e.message, true);
  }
  if (out.result === 'conflict') {
    return toast(
      'refused: the file changed on disk since you opened it. Cancel and reopen to see their version.',
      true);
  }
  editState.version = out.version;
  editState.dirty = false;
  $('ovsave').textContent = 'Save ⌘S';
  toast('saved');
  // Re-diff so the changeset reflects the write.
  await loadSummary();
  const q = editQuery({ context: String(diffState.context) });
  try {
    diffState.file = await get(`/api/diff/file?${q}`);
  } catch (e) {
    /* the editor is still the source of truth on screen */
  }
}

// ---------------------------------------------------------------------------
// Review overlay
// ---------------------------------------------------------------------------

/* Replaces typing `/resolve <pr>` into a terminal pane. The agent reads every
   thread and proposes; you go through them and decide. Nothing is written until
   the final action.

   `design/review-overlay.html` is the spec for anything visual here.

   Local state is NOT derived from the snapshot. `render()` redraws from a full
   Snapshot on every websocket tick, and `diffState`/`editState` survive only
   because `render()` never touches them; this follows that idiom, or every tick
   would reset the scroll position and drop focus out of a half-typed reply. */
const reviewState = {
  open: false,
  pr: null,
  head: null,          // the head sha the proposals were generated against
  data: null,          // the /review payload
  screen: 'intake',    // intake | gate | overview | card | final | manual | report
  i: 0,                // index into queue()
  picks: {},           // thread_id -> position index
  /* thread_id -> 'manual'. Who writes the code the decision implies. Absent means
     the agent does, which is the ordinary case, so only the exceptions are held. */
  modes: {},
  skipped: {},         // thread_id -> true
  /* Keyed per (thread, option), not per thread: looking at a second option must
     not be a punishment for having started typing. */
  drafts: {},
  report: null,
  busy: false,
};

/** The manual phase's own state.
 *
 *  Separate from `reviewState` because it has a different lifetime: the phase opens
 *  once a batch has already committed, and its comments are written after the fact
 *  rather than being the card drafts. Cleared when a batch is sent, not when the
 *  overlay closes — walking away from a phase and coming back should not lose what
 *  you typed about work that is already on disk. */
const manualState = {
  comments: {},     // thread_id -> the comment, required
  /* The payload the last `/manual/done` sent, so the report's retry can go back to
     the same endpoint. Retrying a manual batch through `/post` cannot work: the
     branch it pushed is the remote head now, and it would resolve with no comments. */
  finished: null,
  /* `git diff HEAD` for the whole tree — one object, not one per thread. Two
     manual threads editing the same file cannot be told apart, and the commit is
     the tree's anyway, so attributing it per thread would be a guess dressed as a
     fact. */
  changed: null,
};

const draftKey = (id, pos) => `${id} ${pos}`;

/** Who writes the code for this thread. The third of the three decisions, and
 *  the one the agent has no say in. */
const modeOf = (item) => reviewState.modes[item.t.id] || 'agent';

/** Whether a position would have the agent change code. Under `manual` the same
 *  position stages the same fix, but you are the one who writes it. */
const writesCode = (item, pos) => !!pos.patch && modeOf(item) === 'agent';

/** Compact age off an ISO timestamp: `4h`, `6d`. */
function commentAge(iso) {
  const then = Date.parse(iso);
  if (!then) return '';
  const h = (Date.now() - then) / 36e5;
  if (h < 1) return 'now';
  if (h < 48) return `${Math.round(h)}h`;
  return `${Math.round(h / 24)}d`;
}

/** The threads triage proposed for, in the order the daemon sorted them —
 *  GitHub's Files-changed order, which is the view people review in. */
function queue() {
  const set = reviewState.data?.proposals?.proposals || [];
  const by = new Map(set.map((p) => [p.thread_id, p]));
  return (reviewState.data?.threads || [])
    .filter((t) => by.has(t.id))
    .map((t) => ({ t, p: by.get(t.id) }));
}

/** Whether a position can be acted on at all.
 *
 *  Only one thing makes a position unavailable: a story with no tracker
 *  configured. It is hidden rather than offered-and-refused. */
const offered = (pos) => pos.stance !== 'story' || !!reviewState.data.tracker;

/** Which position is selected on a card: your pick, else the recommendation.
 *
 *  Falls through to the first position that is actually offered, because the
 *  recommendation is often the story one — that is the whole point of a story on a
 *  review summary — and with no tracker it is not on screen. Leaving the pick
 *  pointing at a hidden row would have Enter send a decision the daemon refuses. */
function pickOf(item) {
  const own = reviewState.picks[item.t.id];
  const want = own === undefined ? item.p.recommend : own;
  if (offered(item.p.positions[want])) return want;
  const first = item.p.positions.findIndex(offered);
  return first < 0 ? want : first;
}

const positionOf = (item) => item.p.positions[pickOf(item)];

/** The reply that would be posted: your wording if you typed one. */
function replyOf(item) {
  const i = pickOf(item);
  const drafted = item.p.positions[i]?.reply ?? '';
  return reviewState.drafts[draftKey(item.t.id, i)] ?? drafted;
}

const isHandled = (item) =>
  !reviewState.skipped[item.t.id] && reviewState.picks[item.t.id] !== undefined;

/** `renovate.json5:161 · carol`, matching what the daemon's report uses. */
function threadLabel(t) {
  const who = t.comments?.[0]?.author || 'ghost';
  const line = t.line ?? t.original_line;
  if (!t.path) return `review summary · ${who}`;
  return line ? `${t.path}:${line} · ${who}` : `${t.path} · ${who}`;
}

/* ---------- diffs ---------- */

/** Parse a unified diff into per-path counts — the same arithmetic as
 *  `git apply --numstat`, which is what the daemon re-derives authoritatively
 *  before it writes. Done here so a card can label what it would write without
 *  a round trip. */
function patchStats(diff) {
  const out = [];
  let cur = null;
  for (const line of (diff || '').split('\n')) {
    const to = /^\+\+\+ (?:b\/)?(.+)$/.exec(line);
    if (to) {
      const path = to[1].trim();
      cur = out.find((f) => f.path === path);
      if (!cur) out.push((cur = { path, added: 0, deleted: 0 }));
      continue;
    }
    if (!cur || line.startsWith('---') || line.startsWith('@@')) continue;
    if (line.startsWith('+')) cur.added++;
    else if (line.startsWith('-')) cur.deleted++;
  }
  return out;
}

/** `will write renovate.json5 +3 -1`, every path, derived rather than
 *  hand-written — there is no deny-list, so showing what will be written is
 *  what stands in for one. */
function willWriteLabel(diff, verb) {
  return fileListLabel(patchStats(diff), verb || 'will write');
}

/** `<verb> renovate.json5 +3 −1`, every path.
 *
 *  Shared by the card (from a proposed patch) and the manual phase (from
 *  `git diff`), because in both places the point is the same: the list is derived,
 *  so it cannot be wrong about what is being written. */
function fileListLabel(files, verb) {
  const row = el('div', 'willwrite');
  row.appendChild(document.createTextNode(verb));
  for (const f of files) {
    const b = el('b');
    b.appendChild(document.createTextNode(f.path + ' '));
    if (f.added) {
      const a = el('span', null, `+${f.added}`);
      a.style.color = 'var(--ok)';
      b.appendChild(a);
      b.appendChild(document.createTextNode(' '));
    }
    if (f.deleted) {
      const d = el('span', null, `−${f.deleted}`);
      d.style.color = 'var(--bad)';
      b.appendChild(d);
    }
    row.appendChild(b);
  }
  return row;
}

/** Render diff text as hunk rows.
 *
 *  Takes both shapes it is given: GitHub's `diffHunk` (one hunk, no file
 *  headers) and a full `git diff` (headers, possibly several files). The row
 *  classes are app.css's — `.ln`/`.add`/`.del` — not copies of them.
 *
 *  `hitLast` marks the final row with `.hit`: on a GitHub diff hunk that is the
 *  line the comment is anchored to. */
function hunkEl(text, hitLast) {
  const box = el('div', 'hunk');
  let oldNo = 0;
  let newNo = 0;
  let last = null;
  for (const line of (text || '').split('\n')) {
    /* A file boundary, and the states that have no hunk at all. Skipping these
       rendered a binary replacement, a pure rename and a deletion as *nothing* —
       on the manual phase's screen, whose whole premise is that you looked at what
       is about to be committed — and ran multi-file diffs together with line numbers
       that jump at the seam. */
    const file = /^diff --git (?:a\/)?(.+?) (?:b\/)?(.+)$/.exec(line);
    if (file) {
      const [, from, to] = file;
      box.appendChild(el('div', 'hh', from === to ? from : `${from} → ${to}`));
      oldNo = 0;
      newNo = 0;
      continue;
    }
    const said = /^(new file|deleted file|Binary files|rename from|rename to)/.exec(line);
    if (said) {
      box.appendChild(el('div', 'hh', line));
      continue;
    }
    if (/^(index |old mode|new mode|similarity|dissimilarity)/.test(line)) continue;
    if (/^(--- |\+\+\+ )/.test(line)) continue;
    const at = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
    if (at) {
      oldNo = +at[1];
      newNo = +at[2];
      box.appendChild(el('div', 'hh', line));
      continue;
    }
    const kind = line[0];
    // A "\ No newline at end of file" marker is not a line of the file.
    if (kind !== '+' && kind !== '-' && kind !== ' ') continue;
    const row = el('div', 'ln' + (kind === '+' ? ' add' : kind === '-' ? ' del' : ''));
    row.appendChild(el('i', null, kind === '-' ? String(oldNo) : String(newNo)));
    row.appendChild(el('s', null, line.slice(1)));
    if (kind === '-') oldNo++;
    else if (kind === '+') newNo++;
    else { oldNo++; newNo++; }
    box.appendChild(row);
    last = row;
  }
  if (hitLast && last) last.classList.add('hit');
  return box;
}

/* ---------- chrome shared by every screen ---------- */

/** The header. `sub` is the right-hand half of the title line. */
function rvHead(sub, count) {
  const head = el('div', 'ov-head');
  const path = el('div', 'ov-path', `#${reviewState.pr} `);
  path.appendChild(el('span', null, `· ${sub}`));
  head.appendChild(path);
  head.appendChild(rvHealth());

  const nav = el('div', 'ov-nav');
  if (count) nav.appendChild(el('span', 'ov-count', count));
  const esc = el('button', 'head-btn', 'esc');
  esc.onclick = () => closeReview();
  nav.appendChild(esc);
  head.appendChild(nav);
  return head;
}

/** Branch health — CI colour and a develop conflict.
 *
 *  Information, never a gate: neither touches the apply/push machinery, which
 *  works inside the branch's own history. `fix-pr` is offered only on the
 *  pre-decision screens, because the two flows are mutually exclusive and
 *  offering it from an active card would just be refused. */
function rvHealth() {
  const d = reviewState.data || {};
  const wrap = el('div', 'health');
  const said = [];
  if (d.checks === 'failing') { wrap.classList.add('bad'); said.push('checks failing'); }
  else if (d.checks === 'passing') { wrap.classList.add('ok'); said.push('checks passing'); }
  else if (d.checks === 'pending') { wrap.classList.add('pending'); said.push('checks running'); }
  if (d.mergeable === 'CONFLICTING') said.push('conflicts with develop');
  if (!said.length) return wrap;

  wrap.appendChild(el('span', 'hdot'));
  said.forEach((s, i) => {
    if (i) wrap.appendChild(el('span', 'sep', '·'));
    const n = el('span', 's', s);
    if (i) n.style.color = 'var(--dim)';
    wrap.appendChild(n);
  });
  const preDecision = reviewState.screen === 'intake' || reviewState.screen === 'overview';
  if (preDecision && (d.checks === 'failing' || d.mergeable === 'CONFLICTING')) {
    const b = el('button', 'head-btn', 'fix');
    b.title = 'Rebase on develop and fix what CI says. It cannot run while a review is open.';
    b.onclick = () => rvAct(() => call(`/api/pr/${reviewState.pr}/fix-pr`), 'started the fix run', true);
    wrap.appendChild(b);
  }
  return wrap;
}

/** One bar per thread, filled by outcome. Bars read as progress through a
 *  queue; dots would read as status lights and invite a colour per verdict,
 *  which is not what varies. */
function rvStrip(cur) {
  const strip = el('div', 'strip');
  const q = queue();
  q.forEach((item, i) => {
    let cls = 'pip';
    if (reviewState.skipped[item.t.id]) cls += ' skip';
    else if (reviewState.picks[item.t.id] !== undefined) cls += ' staged';
    if (i === cur) cls += ' cur';
    strip.appendChild(el('span', cls));
  });

  const staged = q.filter(isHandled).length;
  const skipped = q.filter((x) => reviewState.skipped[x.t.id]).length;
  let left;
  if (cur !== null && cur !== undefined) left = `${cur + 1} / ${q.length}`;
  else if (!staged && !skipped) left = 'not started';
  else if (staged + skipped === q.length) left = `${q.length} / ${q.length} handled`;
  else left = `${staged + skipped} / ${q.length}`;
  strip.appendChild(el('span', 'left', left));
  return strip;
}

/** What the header's count says. */
function stagedCount() {
  const q = queue();
  const staged = q.filter(isHandled).length;
  const skipped = q.filter((x) => reviewState.skipped[x.t.id]).length;
  const bits = [];
  if (staged) bits.push(`${staged} staged`);
  if (skipped) bits.push(`${skipped} skipped`);
  return bits.length ? bits.join(' · ') : 'nothing staged';
}

function rvActs(buttons, hint) {
  const bar = el('div', 'acts');
  for (const b of buttons) if (b) bar.appendChild(b);
  if (hint) bar.appendChild(el('span', 'hint', hint));
  return bar;
}

function actBtn(label, cls, onclick, disabled) {
  const b = el('button', 'act' + (cls ? ' ' + cls : ''), label);
  b.disabled = !!disabled || reviewState.busy;
  b.onclick = onclick;
  return b;
}

function headBtn(label, cls, onclick) {
  const b = el('button', 'head-btn' + (cls ? ' ' + cls : ''), label);
  b.disabled = reviewState.busy;
  b.onclick = onclick;
  return b;
}

/* ---------- screen 1: the intake ---------- */

/** Nothing triaged yet. The line under the heading is what the old design could
 *  not say: reading is all that happens. */
function rvIntake(root) {
  const d = reviewState.data;
  root.appendChild(rvHead(reviewState.data.title || 'review'));

  const mid = el('div', 'mid');
  const n = d.answerable || 0;
  mid.appendChild(el('div', 'eyebrow', `${n} thread${n === 1 ? '' : 's'} awaiting an answer`));

  const big = el('div', 'big', 'No triage for ');
  const sha = el('span', 'm', (d.head_sha || '').slice(0, 7));
  sha.style.fontSize = '13px';
  big.appendChild(sha);
  big.appendChild(document.createTextNode(' yet.'));
  mid.appendChild(big);

  mid.appendChild(el('p', null,
    'Triage reads the code at each thread and works out what it would do about it. ' +
    'It changes nothing — not a file, not a commit, not a comment. You decide thread by thread.'));

  const row = el('div');
  row.style.cssText = 'display:flex;gap:8px;margin-top:4px';
  row.appendChild(headBtn('read the threads', 'go', () =>
    rvAct(() => call(`/api/pr/${reviewState.pr}/triage`), 'triage started', true)));
  if (d.url) {
    const gh = headBtn('open on github', null, () => window.open(d.url, '_blank', 'noreferrer'));
    row.appendChild(gh);
  }
  mid.appendChild(row);

  if (n === 0) {
    mid.appendChild(el('p', null,
      'Nothing is awaiting an answer right now, so triage would have nothing to read.'));
  }
  root.appendChild(mid);
}

/* ---------- screen 1b: the worktree gate ---------- */

/** The tree is not mine to write to yet. Three reasons, one screen: the design
 *  rests on the tree being clean until the final action, so it must start
 *  clean. CI colour and a develop conflict are deliberately not here. */
function rvGate(root) {
  const g = reviewState.data.gate;
  root.appendChild(rvHead(reviewState.data.title || 'review'));

  const mid = el('div', 'mid');
  if (g.gate === 'dirty') {
    mid.appendChild(el('div', 'eyebrow', 'uncommitted changes in this worktree'));
    mid.appendChild(el('div', 'big', 'Commit or stash first.'));

    const list = el('div', 'hunk');
    list.style.cssText = 'text-align:left;max-width:420px;width:100%';
    for (const f of g.files) {
      const ln = el('div', 'ln');
      ln.appendChild(el('i'));
      ln.appendChild(el('s', null, f));
      list.appendChild(ln);
    }
    mid.appendChild(list);
    mid.appendChild(el('p', null,
      'Resolve commits the changes you accept; work already sitting in the tree would be ' +
      'swept into that commit, so "only what you approved" would stop being true. Clear the ' +
      'tree and this opens to the threads.'));

    const row = el('div');
    row.style.cssText = 'display:flex;gap:8px;margin-top:4px';
    row.appendChild(headBtn('commit…', 'go', async () => {
      const message = prompt('Commit message for the work already in this worktree:');
      if (!message || !message.trim()) return;
      rvAct(() => call(`/api/pr/${reviewState.pr}/commit`, { message: message.trim() }), 'committed');
    }));
    // Never popped automatically: popping onto a branch the review just amended
    // can conflict, and silently juggling your work is worse than leaving it.
    row.appendChild(headBtn('stash', 'go', () =>
      rvAct(() => call(`/api/pr/${reviewState.pr}/stash`), 'stashed — pop it yourself')));
    row.appendChild(headBtn('open a shell', null, () => { closeReview(); newShell(); }));
    mid.appendChild(row);
  } else if (g.gate === 'rebasing') {
    mid.appendChild(el('div', 'eyebrow', 'a rebase is stopped part-way'));
    mid.appendChild(el('div', 'big', 'Finish or abort the rebase first.'));
    mid.appendChild(el('p', null,
      'The tree is mid-conflict-resolution and cannot take a patch at all. ' +
      'Resolve opens once the rebase is out of the way.'));
    const row = el('div');
    row.style.cssText = 'display:flex;gap:8px;margin-top:4px';
    row.appendChild(headBtn('open a shell', null, () => { closeReview(); newShell(); }));
    mid.appendChild(row);
  } else {
    mid.appendChild(el('div', 'eyebrow', 'a fix run is going on this branch'));
    mid.appendChild(el('div', 'big', 'Resolve opens when it finishes.'));
    mid.appendChild(el('p', null,
      'Both rewrite this same worktree, so the two are mutually exclusive — two concurrent ' +
      'rebases in one working directory is index corruption, not a UI glitch. Starting ' +
      'a fix run during a review is refused from the other side too.'));
  }
  root.appendChild(mid);
  root.appendChild(rvActs([actBtn('re-check', 'pri', () => loadReview(reviewState.pr))]));
}

/* ---------- screen 2: what it found ---------- */

/** The shape of the work, not a summary of things already done: how long this
 *  will take and where the hard part is. */
function rvOverview(root) {
  const q = queue();
  root.appendChild(rvHead('review', `${q.length} thread${q.length === 1 ? '' : 's'}`));
  root.appendChild(rvFreshBar());
  root.appendChild(rvStrip(null));

  const body = el('div', 'body');
  const sec = el('div', 'sec');
  const tally = el('div', 'tally');
  /* Grouped by what the agent's recommendation costs you, because that is what
     decides how long the queue takes — not by a category it would have to
     invent and keep consistent. */
  const buckets = [
    ['Straightforward', 'they are right and a diff is ready — one keystroke each',
      (x) => positionOf(x).stance === 'agree'],
    ['Wants a decision', 'the recommendation comes with words you should read first',
      (x) => positionOf(x).stance === 'reply'],
    ['Out of scope', 'fair, but it belongs in a story rather than this PR',
      (x) => positionOf(x).stance === 'story'],
  ];
  for (const [label, why, match] of buckets) {
    const hits = q.filter(match);
    if (!hits.length) continue;
    const line = el('div', 'tline');
    line.appendChild(el('span', 'n', String(hits.length)));
    const l = el('span', 'l', label);
    l.appendChild(el('em', null, hits.length === 1 ? threadLabel(hits[0].t) + ' — ' + why : why));
    line.appendChild(l);
    tally.appendChild(line);
  }
  sec.appendChild(tally);
  body.appendChild(sec);

  const note = el('div', 'sec');
  const p = el('p', null, 'Working tree is clean and stays that way until you accept something.');
  p.style.cssText = 'color:var(--dim);font-size:12px';
  note.appendChild(p);
  body.appendChild(note);
  root.appendChild(body);

  const handled = q.filter((x) => isHandled(x) || reviewState.skipped[x.t.id]).length;
  root.appendChild(rvActs([
    actBtn(handled ? `back to thread ${reviewState.i + 1} of ${q.length}` : `start · thread 1 of ${q.length}`,
      'pri', () => { reviewState.screen = 'card'; renderReview(); }),
    handled === q.length && q.length
      ? actBtn('review the batch', null, () => { reviewState.screen = 'final'; renderReview(); })
      : null,
  ], 'enter accepts the recommendation · j / k to move'));
}

/** Comments that landed after the queue was built.
 *
 *  Deliberately not appended to it: a new thread has had no triage behind it, so
 *  it would arrive with no patch, no positions and no read — a visibly worse
 *  card among good ones — and the "4 of 4" target would keep moving.
 *
 *  The load-bearing half is the last clause. Missing one mostly means handling
 *  it next session; the exception is re-request, and that needs no mechanism:
 *  the post step derives re-requests from a fresh fetch, so a new thread counts
 *  as open and holds its author back on its own. */
function rvFreshBar() {
  const q = new Set(queue().map((x) => x.t.id));
  const fresh = (reviewState.data?.threads || []).filter((t) => t.answerable && !q.has(t.id));
  // A comment node, not null: `appendChild(null)` is a TypeError, and the
  // callers append this unconditionally — which silently killed the rest of
  // every card on a PR with no new threads, the common case.
  if (!fresh.length || !reviewState.data.proposals) return document.createComment('no new threads');

  const bar = el('div', 'autobar');
  bar.appendChild(el('b', null,
    `${fresh.length} thread${fresh.length === 1 ? '' : 's'} not in this queue`));
  const who = [...new Set(fresh.map((t) => t.comments?.[0]?.author).filter(Boolean))];
  bar.appendChild(el('span', null, who.join(', ') + ' — arrived after triage ran'));
  bar.appendChild(actBtn('re-triage', null, () =>
    rvAct(() => call(`/api/pr/${reviewState.pr}/triage`), 'triage started', true)));
  bar.appendChild(el('span', 'why', 'they will hold their author back from a re-request'));
  return bar;
}

/* ---------- screen 3/4: a thread ---------- */

/** One card per thread, including the obvious ones — the agent just recommends
 *  the obvious thing and pre-selects it. That costs one keystroke on an easy
 *  thread and buys back the property that nothing happens you did not choose. */
function rvCard(root) {
  const q = queue();
  const item = q[reviewState.i];
  if (!item) { reviewState.screen = 'overview'; return rvOverview(root); }
  const { t, p } = item;

  root.appendChild(rvHead(`thread ${reviewState.i + 1} of ${q.length}`, stagedCount()));
  root.appendChild(rvFreshBar());
  root.appendChild(rvStrip(reviewState.i));

  const body = el('div', 'body');

  // -- the comment, and what it is anchored to
  const top = el('div', 'sec');
  const anchor = el('div', 'anchor');
  const line = t.line ?? t.original_line;
  if (t.path) {
    anchor.appendChild(el('span', 'p', line ? `${t.path}:${line}` : t.path));
  } else {
    anchor.appendChild(el('span', 'p none', 'review summary'));
  }
  anchor.appendChild(el('span', 'who', t.comments?.[0]?.author || 'ghost'));
  anchor.appendChild(el('span', 'age', commentAge(t.comments?.[0]?.created_at)));
  if (!t.path) anchor.appendChild(el('span', 'flag', 'no line'));
  if (t.is_outdated) anchor.appendChild(el('span', 'flag out', 'outdated'));
  // Not "re-review": that is GitHub's own phrase for requesting a fresh review,
  // which the final screen already uses.
  if (p.continued) anchor.appendChild(el('span', 'flag cont', 'continued'));
  top.appendChild(anchor);

  const hunk = t.comments?.[0]?.diff_hunk;
  if (hunk) top.appendChild(hunkEl(hunk, true));

  const chain = el('div', 'chain');
  for (const c of t.comments || []) {
    const mine = c.author === reviewState.data.viewer;
    const cmt = el('div', 'cmt' + (mine ? ' mine' : ''));
    const hd = el('div', 'hd');
    hd.appendChild(el('b', null, c.author));
    // On a continued thread the thing that matters most is what you committed
    // to last time: the new reply has to be consistent with it, in public.
    if (mine) hd.appendChild(el('span', 'you', 'you'));
    hd.appendChild(el('span', 'age', commentAge(c.created_at)));
    cmt.appendChild(hd);
    cmt.appendChild(el('p', null, c.body));
    chain.appendChild(cmt);
  }
  top.appendChild(chain);
  body.appendChild(top);

  // -- the agent's read
  const readSec = el('div', 'sec');
  const read = el('div', 'read');
  read.appendChild(el('div', 'eyebrow', 'the read'));
  read.appendChild(el('p', null, p.read));
  if (p.verified) {
    const v = el('p', null, p.verified);
    v.style.cssText = 'font-family:var(--mono);font-size:10.5px;color:var(--faint-solid)';
    read.appendChild(v);
  }
  readSec.appendChild(read);
  body.appendChild(readSec);

  /* -- the three decisions, in the order they depend on each other: what you
        are saying, the words that say it, and the code it implies. */
  const decide = el('div', 'sec');
  decide.appendChild(rvStance(item));
  decide.appendChild(rvReply(item));
  const fix = rvFix(item);
  if (fix) decide.appendChild(fix);
  body.appendChild(decide);
  root.appendChild(body);

  const hint = `thread ${reviewState.i + 1} of ${q.length} · ` +
    (q.some(isHandled) ? stagedCount() : 'nothing written yet');
  /* Three peers, because they are three answers to the same question rather than
     one action and two escapes. `manual` is not a lesser `accept`: it stages the
     same stance and the same words, and says you are writing the code. */
  root.appendChild(rvActs([
    actBtn('accept · ⏎', 'warm', () => acceptCard()),
    actBtn('manual · m', modeOf(item) === 'manual' ? 'on' : null, () => manualCard()),
    actBtn('skip · s', null, () => skipCard()),
    reviewState.i > 0 ? actBtn('back', null, () => moveCard(-1)) : null,
  ], hint));
}

/** A labelled field, the way the design lays the card out: a small caption, an
 *  optional hint to its right, then the control. */
function rvField(label, hint) {
  const wrap = el('div', 'field');
  const lab = el('div', 'flab', label);
  if (hint) lab.appendChild(el('span', 'fhint', hint));
  wrap.appendChild(lab);
  return wrap;
}

/** The stances this proposal actually offers, in the order the card shows them.
 *
 *  Derived rather than fixed: triage decides which ways out exist for a thread,
 *  and offering `story` where it proposed none would be a button that picks
 *  nothing. `skip` is always there because it is the one answer that needs no
 *  proposal. */
const STANCE_ORDER = ['reply', 'agree', 'story'];
const STANCE_LABEL = { reply: 'Reply', agree: 'Agree 👍', story: 'Story' };

function stancesOf(item) {
  return STANCE_ORDER.filter((st) =>
    item.p.positions.some((pos, i) => pos.stance === st && offered(pos)));
}

/** The position to select when you pick a stance: the recommendation if it is in
 *  that stance, else its first. */
function positionForStance(item, stance) {
  const rec = item.p.positions[item.p.recommend];
  if (rec && rec.stance === stance && offered(rec)) return item.p.recommend;
  return item.p.positions.findIndex((pos) => pos.stance === stance && offered(pos));
}

/** What you are saying back. One row of peers, because they are alternatives to
 *  each other and not a primary with escapes. */
function rvStance(item) {
  const wrap = rvField('Stance');
  const seg = el('div', 'seg');
  const chosen = reviewState.skipped[item.t.id] ? 'skip' : positionOf(item).stance;

  for (const st of stancesOf(item)) {
    const b = el('button', 'segbtn' + (st === chosen ? ' on' : ''), STANCE_LABEL[st]);
    b.onclick = () => {
      const i = positionForStance(item, st);
      if (i < 0) return;
      reviewState.picks[item.t.id] = i;
      delete reviewState.skipped[item.t.id];
      renderReview();
    };
    seg.appendChild(b);
  }
  // Skip is a stance on this row and a button on the action row, deliberately:
  // it is both an answer to "what are you saying" and a way past the card.
  const sk = el('button', 'segbtn' + (chosen === 'skip' ? ' on' : ''), 'Skip');
  sk.onclick = () => {
    reviewState.skipped[item.t.id] = true;
    delete reviewState.picks[item.t.id];
    delete reviewState.modes[item.t.id];
    renderReview();
  };
  seg.appendChild(sk);
  wrap.appendChild(seg);

  // Triage can offer two ways of taking the same stance — a short apology and a
  // long one, two different fixes. The segment cannot say which, so when there
  // is a choice left to make it stays on screen.
  const alts = item.p.positions
    .map((pos, i) => ({ pos, i }))
    .filter(({ pos, i }) => pos.stance === chosen && offered(pos));
  if (alts.length > 1) {
    const row = el('div', 'alts');
    for (const { pos, i } of alts) {
      const b = el('button', 'alt' + (i === pickOf(item) ? ' on' : ''));
      b.appendChild(el('span', 'k', String(i + 1)));
      b.appendChild(el('span', 't', pos.label));
      if (i === item.p.recommend) b.appendChild(el('span', 'rec', 'recommended'));
      if (reviewState.drafts[draftKey(item.t.id, i)] !== undefined) {
        b.appendChild(el('span', 'edited', 'edited'));
      }
      b.onclick = () => {
        reviewState.picks[item.t.id] = i;
        delete reviewState.skipped[item.t.id];
        renderReview();
      };
      row.appendChild(b);
    }
    wrap.appendChild(row);
  }
  return wrap;
}

/** The words. A box when there are words to write, and an honest note when there
 *  are not — a thumbs up posts none, and a hand-written thread's comment belongs
 *  to the phase, after the work exists. */
function rvReply(item) {
  const pos = positionOf(item);
  const i = pickOf(item);

  if (reviewState.skipped[item.t.id]) {
    const wrap = rvField('Reply');
    wrap.appendChild(el('div', 'note', 'Skipped: the thread stays open and nothing is posted.'));
    return wrap;
  }
  if (modeOf(item) === 'manual') {
    const wrap = rvField('Reply', 'written in the phase, not now');
    wrap.appendChild(el('div', 'note',
      'You are writing this one live. The session stops here, and the comment is '
      + 'written once the work exists.'));
    return wrap;
  }
  if (!pos.stance || !['reply', 'story'].includes(pos.stance)) {
    const wrap = rvField('Reply');
    wrap.appendChild(el('div', 'note', 'A thumbs up posts no words.'));
    return wrap;
  }

  const wrap = rvField('Reply', 'AI draft · edit freely');
  const box = el('textarea', 'box');
  box.setAttribute('aria-label', 'Reply');
  box.value = replyOf(item);
  /* A textarea, not a contenteditable: the text goes to GitHub as plain
     markdown, so rich paste is pure liability and browsers insert <div>/<br>
     where a newline belongs. `openEditor()` settled this. */
  box.oninput = () => {
    reviewState.drafts[draftKey(item.t.id, i)] = box.value;
    // Repaint only the footer: re-rendering the card here would move focus out
    // of the box mid-sentence.
    rvFootState(wrap, item, pos, i);
  };
  wrap.appendChild(box);
  wrap.appendChild(el('div', 'foot'));
  rvFootState(wrap, item, pos, i);
  return wrap;
}

/** The code the decision implies: the staged fix, and the story when there is
 *  one. Absent entirely when the answer is words only, rather than an empty
 *  heading. */
function rvFix(item) {
  if (reviewState.skipped[item.t.id]) return null;
  const pos = positionOf(item);
  if (!pos.patch && !pos.story) return null;

  const manual = modeOf(item) === 'manual';
  const wrap = rvField(
    pos.patch ? 'Proposed fix' : 'Story',
    pos.patch ? (manual ? 'staged, but you write it' : 'the session applies this') : null
  );
  if (pos.patch) {
    wrap.appendChild(willWriteLabel(pos.patch));
    wrap.appendChild(hunkEl(pos.patch, false));
  }
  if (pos.story) {
    // `willWriteLabel` derives its object from a diff, and a story has none — so
    // the object is named here rather than leaving a verb with nothing after it.
    const says = el('div', 'willwrite');
    says.appendChild(document.createTextNode('will create'));
    says.appendChild(el('b', null, 'a Shortcut story'));
    wrap.appendChild(says);
    const draft = el('div', 'storydraft');
    for (const [lbl, val] of [['title', pos.story.title], ['body', pos.story.body]]) {
      const l = el('div', 'sline');
      l.appendChild(el('span', 'lbl', lbl));
      l.appendChild(el('span', 'val', val));
      draft.appendChild(l);
    }
    wrap.appendChild(draft);
  }
  return wrap;
}

/** The footer under a reply box: what gets appended, and — only once the text
 *  actually differs from the draft — the offer to put it back. */
function rvFootState(bodyEl, item, pos, i) {
  const foot = bodyEl.querySelector('.foot');
  if (!foot) return;
  foot.replaceChildren();

  if (pos.stance === 'story') {
    const said = el('span');
    const tok = el('span', 'm', '{story}');
    tok.style.color = 'var(--work)';
    said.appendChild(tok);
    said.appendChild(document.createTextNode(' becomes the id once it exists · (via orchestrator) is appended'));
    foot.appendChild(said);
  } else {
    foot.appendChild(el('span', null, '(via orchestrator) is appended when it posts'));
  }

  const key = draftKey(item.t.id, i);
  const typed = reviewState.drafts[key];
  if (typed !== undefined && typed !== (pos.reply ?? '')) {
    const revert = el('button', 'revert', 'revert to draft');
    revert.onclick = () => {
      delete reviewState.drafts[key];
      renderReview();
    };
    foot.appendChild(revert);
  }
}

/* ---------- screen 5: everything, then one go ---------- */

/** The plan you are approving, then the only irreversible button in the flow.
 *
 *  Nothing has left the machine yet and the working tree is still clean:
 *  accepting staged the edits, it did not apply them. */
function rvFinal(root) {
  const q = queue();
  root.appendChild(rvHead('ready'));
  root.appendChild(rvStrip(null));

  const body = el('div', 'body');

  // -- one row per thread, in queue order
  const plan = el('div', 'sec');
  for (const item of q) {
    const pos = positionOf(item);
    const skipped = reviewState.skipped[item.t.id];
    const row = el('div', 'stage-row');

    /* APPLY, present tense on purpose: nothing is applied at this point, not on
       the branch and not even in the working tree. The row says what the button
       is about to do, so the list reads as a plan rather than a receipt. */
    let kind = 'reply';
    let word = 'reply';
    if (skipped) { kind = 'skip'; word = 'skipped'; }
    else if (!isHandled(item)) { kind = 'skip'; word = 'not handled'; }
    else if (modeOf(item) === 'manual') { kind = 'manual'; word = 'by hand'; }
    else if (pos.stance === 'story') { kind = 'story'; word = 'story'; }
    else if (pos.patch) { kind = 'apply'; word = 'apply'; }
    else if (pos.stance === 'agree') { kind = 'reply'; word = 'thumbs up'; }
    row.appendChild(el('span', 'k ' + kind, word));

    const c = el('span', 'c');
    c.appendChild(el('span', 'p', threadLabel(item.t)));
    c.appendChild(el('span', 't', skipped || !isHandled(item)
      /* All three consequences stated, because "leaves it open" alone reads as
         harmless. The button itself stays a bare `skip`. */
      ? `Not handled. Stays open, nothing written, ${item.t.comments?.[0]?.author || 'they'} not re-requested.`
      : planLine(item, pos)));
    row.appendChild(c);
    plan.appendChild(row);
  }
  body.appendChild(plan);

  // -- re-request, per reviewer
  const req = rvRerequestSec(q);
  if (req) body.appendChild(req);

  // -- what leaves this machine
  body.appendChild(rvLeavesSec(q));
  root.appendChild(body);

  const out = outward(q);
  // Nothing is blocked any more. A Manual thread does not disable the button — it
  // changes what pressing it does: the batch commits the rest and stops, and a
  // second screen finishes it. That cost was recorded deliberately, since "one go"
  // was a property the design bought and asking for a human step spends it.
  const byHand = q.filter((x) => isHandled(x) && modeOf(x) === 'manual');
  const bits = [];
  if (out.commits) bits.push('push 1 commit');
  // Named separately from the GitHub count, because it is a write to another
  // system and the one thing a retry cannot simply re-derive.
  if (out.stories) bits.push(`file ${out.stories} story${out.stories === 1 ? '' : 's'}`);
  if (out.total) bits.push(`post ${out.total} to github`);
  const label = bits.length ? bits.join(' · ') : 'nothing to send';

  /* Enter is unbound here. Across the cards it means "accept this one thing"; on
     a batch it has no natural meaning, and three cards' worth of "enter is safe"
     should not land on the one irreversible button. ctrl+enter is GitHub's own
     comment-box convention, so it is already in the right muscle memory. */
  /* The button names what it will actually do. With a Manual thread in the batch
     that is not the push — it is committing and then handing back to you, and
     saying "push" would be a lie you only discover after pressing it. */
  const goes = byHand.length
    ? `commit, then ${byHand.length === 1 ? 'your turn' : `your turn on ${byHand.length}`}`
    : label;
  root.appendChild(rvActs([
    // The session is the way this is meant to go now: it adapts a fix to a branch
    // that moved instead of refusing it, and it can ask. The batch stays because
    // it is proven, and because a words-only review does not need an agent.
    actBtn('hand it to a session', 'warm', () => startRun(), !bits.length && !byHand.length),
    actBtn(`or ${goes}`, null, () => sendBatch(), !bits.length && !byHand.length),
    actBtn('back', null, () => { reviewState.screen = 'card'; renderReview(); }),
  ], byHand.length
    ? 'nothing is pushed or posted until you have written your comment'
    : 'ctrl+enter to send · enter does nothing here'));

  if (byHand.length) {
    const warn = el('div', 'banner');
    warn.appendChild(el('span', 'ico', '▲'));
    const tx = el('span', 'tx');
    tx.appendChild(el('b', null, 'This stops for you part-way.'));
    tx.appendChild(el('p', null,
      `${byHand.map((x) => threadLabel(x.t)).join(', ')} ` +
      (byHand.length === 1 ? 'is yours to write' : 'are yours to write') +
      '. Everything else is written and committed first, so you edit a tree that ' +
      'already reflects every other decision — then a second screen takes your ' +
      'comment and finishes the batch. Nothing is pushed or posted before that.'));
    warn.appendChild(tx);
    root.insertBefore(warn, root.querySelector('.strip'));
  }
}

/** One sentence for what a handled thread will do. */
function planLine(item, pos) {
  const reply = replyOf(item).trim();
  const short = reply.length > 90 ? reply.slice(0, 90).trimEnd() + '…' : reply;
  if (modeOf(item) === 'manual') {
    return pos.stance === 'agree'
      ? 'You write the code, then it responds with a thumbs up.'
      : 'You write the code, then comment in the phase that follows.';
  }
  if (pos.stance === 'agree') {
    return pos.patch
      ? `${pos.label}. Responds with a thumbs up, no written reply.`
      : 'Responds with a thumbs up, no written reply.';
  }
  if (pos.stance === 'story') return `File “${pos.story?.title || 'a story'}”, then reply with its id.`;
  if (pos.patch) return `${pos.label}. Replies “${short}”`;
  return `Replies “${short}”`;
}

/** Per reviewer, not per PR: one whose every thread is addressed is
 *  re-requested even while another's are still open. The daemon recomputes this
 *  from a fresh fetch at post time; this is the same rule, shown early. */
function rvRerequestSec(q) {
  const viewer = reviewState.data.viewer;
  const mine = new Map();   // login -> { open: [labels] }
  for (const t of reviewState.data.threads || []) {
    if (!t.answerable) continue;
    const who = t.comments?.[0]?.author;
    if (!who || who === viewer) continue;
    const entry = mine.get(who) || { open: [] };
    const item = q.find((x) => x.t.id === t.id);
    // Not `threadLabel`: the row is already labelled with this reviewer, and
    // repeating their name inside the reason reads as a stutter.
    if (!item || !isHandled(item)) {
      const line = t.line ?? t.original_line;
      entry.open.push(t.path ? (line ? `${t.path}:${line}` : t.path) : 'the review summary');
    }
    mine.set(who, entry);
  }
  if (!mine.size) return null;

  const sec = el('div', 'sec');
  sec.appendChild(el('div', 'eyebrow', 're-request'));
  for (const [who, { open }] of [...mine].sort()) {
    const row = el('div', 'stage-row');
    row.appendChild(el('span', 'k ' + (open.length ? 'skip' : 'req'), who));
    const c = el('span', 'c');
    c.appendChild(el('span', open.length ? 't held' : 't', open.length
      ? `held back by ${open[0]}, which you did not handle`
      : 'Every thread of theirs addressed.'));
    row.appendChild(c);
    sec.appendChild(row);
  }
  return sec;
}

/** What is local and what is outward, separated by a single glyph. The last
 *  thing read before the irreversible button, so it is set apart rather than
 *  being one more list among the others. */
function rvLeavesSec(q) {
  const sec = el('div', 'sec leaves');
  sec.appendChild(el('div', 'eyebrow', 'what leaves this machine'));
  const group = el('div', 'group');
  const out = outward(q);

  if (out.files.length) {
    const row = el('div', 'res wait');
    row.appendChild(el('span', 'st', '·'));
    const c = el('span', 'c');
    const t = el('span', 't', 'writes ');
    for (const f of out.files) {
      const p = el('span', 'm', f.path);
      t.appendChild(p);
      const a = el('span', null, ` +${f.added}`);
      a.style.color = 'var(--ok)';
      t.appendChild(a);
      const d = el('span', null, ` −${f.deleted} `);
      d.style.color = 'var(--bad)';
      t.appendChild(d);
    }
    t.appendChild(document.createTextNode('— local only'));
    c.appendChild(t);
    row.appendChild(c);
    group.appendChild(row);
  }
  if (out.commits) {
    const row = el('div', 'res wait');
    row.appendChild(el('span', 'st', '↑'));
    const c = el('span', 'c');
    const t = el('span', 't', '1 commit to ');
    t.appendChild(el('span', 'm', `origin/${reviewState.data.head_ref || 'this branch'}`));
    c.appendChild(t);
    row.appendChild(c);
    group.appendChild(row);
  }
  if (out.stories) {
    const row = el('div', 'res wait');
    row.appendChild(el('span', 'st', '↑'));
    const c = el('span', 'c');
    c.appendChild(el('span', 't',
      `${out.stories} story${out.stories === 1 ? '' : 's'} filed in the tracker, and ` +
      `${out.stories === 1 ? 'its id' : 'their ids'} put into a reply.`));
    row.appendChild(c);
    group.appendChild(row);
  }
  if (out.total) {
    const row = el('div', 'res wait');
    row.appendChild(el('span', 'st', '↑'));
    const c = el('span', 'c');
    const bits = [];
    if (out.replies) bits.push(`${out.replies} repl${out.replies === 1 ? 'y' : 'ies'}`);
    if (out.thumbs) bits.push(`${out.thumbs} thumbs up`);
    if (out.rerequests) bits.push(`${out.rerequests} re-request${out.rerequests === 1 ? '' : 's'}`);
    c.appendChild(el('span', 't',
      `${out.total} thing${out.total === 1 ? '' : 's'} to github — ${bits.join(', ')}. Cannot be unsent.`));
    row.appendChild(c);
    group.appendChild(row);
  }
  if (!group.children.length) {
    const row = el('div', 'res wait');
    row.appendChild(el('span', 'st', '·'));
    const c = el('span', 'c');
    c.appendChild(el('span', 't', 'Nothing. Every thread was skipped.'));
    row.appendChild(c);
    group.appendChild(row);
  }
  sec.appendChild(group);
  return sec;
}

/** Everything the batch would do, counted. */
function outward(q) {
  const handled = q.filter(isHandled);
  const files = [];
  for (const item of handled) {
    if (!writesCode(item, positionOf(item))) continue;
    for (const f of patchStats(positionOf(item).patch)) {
      const seen = files.find((x) => x.path === f.path);
      if (seen) { seen.added += f.added; seen.deleted += f.deleted; }
      else files.push({ ...f });
    }
  }
  const replies = handled.filter((x) => replyOf(x).trim() &&
    ['reply', 'story'].includes(positionOf(x).stance)).length;
  // Counted apart from the GitHub writes: a story goes to a different system, and
  // it is the one thing in the batch that is not re-derivable from the PR.
  const stories = handled.filter((x) => positionOf(x).stance === 'story').length;
  const thumbs = handled.filter((x) => positionOf(x).stance === 'agree').length;

  const viewer = reviewState.data.viewer;
  const open = new Set();
  const all = new Set();
  for (const t of reviewState.data.threads || []) {
    if (!t.answerable) continue;
    const who = t.comments?.[0]?.author;
    if (!who || who === viewer) continue;
    all.add(who);
    const item = q.find((x) => x.t.id === t.id);
    if (!item || !isHandled(item)) open.add(who);
  }
  const rerequests = [...all].filter((w) => !open.has(w)).length;

  return {
    files,
    commits: files.length ? 1 : 0,
    stories,
    replies, thumbs, rerequests,
    total: replies + thumbs + rerequests,
  };
}

/* ---------- screens 7 and 8: what the batch actually did ---------- */

/** The report. Two different screens wearing one shape:
 *
 *  `refused` is the local half saying no — a stale patch, a hook that rewrote a
 *  file, pre-commit failing. Nothing was committed and nothing was pushed, so
 *  every decision is still staged and the button can simply be pressed again.
 *
 *  A push with failures after it is the hard one: the code is public and cannot
 *  be recalled, so landed / failed / not attempted are separated before anything
 *  is offered. */
function rvReport(root) {
  const r = reviewState.report;
  root.appendChild(rvHead(r.refused ? 'stopped' : r.failed.length ? 'posted with errors' : 'posted'));

  const banner = el('div', 'banner' + (r.pushed ? '' : ' clean'));
  banner.appendChild(el('span', 'ico', r.pushed ? '▲' : '✓'));
  const tx = el('span', 'tx');
  if (r.pushed) {
    tx.appendChild(el('b', null, 'The code is pushed.'));
    const p = el('p');
    p.appendChild(el('span', 'm', r.pushed.slice(0, 7)));
    p.appendChild(document.createTextNode(
      ` is on origin/${reviewState.data.head_ref || 'this branch'}. ` +
      'Retrying posts only what is missing — it will not push again, and it re-reads the ' +
      'threads first so nothing is sent twice.'));
    tx.appendChild(p);
  } else {
    tx.appendChild(el('b', null, 'Nothing was pushed or posted.'));
    tx.appendChild(el('p', null,
      'The worktree is as it was, and every decision is still staged — fix what it names ' +
      'and press the button again, or hand it to a session.'));
  }
  banner.appendChild(tx);
  root.appendChild(banner);

  const body = el('div', 'body');

  if (r.refused) {
    const sec = el('div', 'sec');
    const group = el('div', 'group');
    const row = el('div', 'res no');
    row.appendChild(el('span', 'st', '✕'));
    const c = el('span', 'c');
    c.appendChild(el('span', 't', 'the batch was refused'));
    // Verbatim, per the daemon's own rule: a refused call is information, not
    // noise to swallow behind a friendly paraphrase.
    c.appendChild(el('span', 'err', r.refused));
    row.appendChild(c);
    group.appendChild(row);
    group.appendChild(waitRow('push — not attempted'));
    group.appendChild(waitRow('nothing posted to github'));
    sec.appendChild(group);
    body.appendChild(sec);
  }

  if (r.files?.length) {
    const sec = el('div', 'sec');
    const group = el('div', 'group');
    const row = el('div', 'res ok');
    row.appendChild(el('span', 'st', '✓'));
    const c = el('span', 'c');
    c.appendChild(el('span', 't',
      `Wrote ${r.files.map((f) => f.path).join(', ')} — ${r.amend || 'committed'}.`));
    row.appendChild(c);
    group.appendChild(row);
    sec.appendChild(group);
    body.appendChild(sec);
  }

  body.appendChild(resultSec('landed', r.landed, (x) => {
    if (x.what === 'story') {
      return {
        cls: 'ok', st: '✓',
        t: x.already
          /* A reused story is the retry working, and it carries a consequence
             worth stating: the fields are whatever the first run filed, so any
             edit made since is not in the tracker. */
          ? `Story ${x.story.id} was already filed — reused, not filed again. ` +
            'Its fields are as they were then, so any later edit is not in it.'
          : `Story ${x.story.id} filed. Its reply carries the link; a retry reuses ` +
            'this id rather than filing a second one.',
        link: x.story.url,
      };
    }
    return {
      cls: 'ok', st: '✓',
      t: x.already
        ? `${whatWord(x.what)} was already there — nothing sent.`
        : `${whatWord(x.what)} posted. Cannot be unsent.`,
    };
  }));
  body.appendChild(resultSec('failed', r.failed, (x) => ({
    // A story is filed, not posted. The verb is the difference between "a
    // colleague can see this" and "a record exists somewhere else".
    cls: 'no', st: '✕',
    t: x.what === 'story' ? 'Story not filed.' : `${whatWord(x.what)} not posted.`,
    err: x.error,
  })));
  /* `skipped` and `held_back` are the same row: never tried, and here is what it
     is waiting on. They arrive as two lists only because one is per-write and the
     other is per-reviewer — rendering only `held_back` dropped every reply that
     was skipped because its story did not land. */
  body.appendChild(resultSec('not attempted', [...r.skipped, ...r.held_back], (x) => ({
    cls: 'wait', st: '·',
    t: x.what === 'reply' ? `Reply not posted — ${x.waiting_on}.` : x.waiting_on,
    held: true,
  })));
  root.appendChild(body);

  const retry = r.failed.length || r.refused;
  root.appendChild(rvActs([
    retry ? actBtn(r.refused ? 'back' : `retry ${r.failed.length}`, 'pri', () => {
      if (r.refused) { reviewState.report = null; reviewState.screen = 'final'; return renderReview(); }
      /* Back to whichever endpoint the batch used. A manual batch retried through
         `/post` is refused every time — the branch it pushed is now the remote head —
         and it would resolve with no comments, so the Manual thread would post
         nothing at all. */
      if (manualState.finished) finishManual(manualState.finished);
      else sendBatch();
    }) : actBtn('done', 'pri', () => closeReview()),
    retry ? actBtn('leave it', null, () => closeReview()) : null,
  ], r.refused
    ? 'decisions kept · worktree unchanged'
    : 're-reads the threads first · never reposts'));
}

const whatWord = (w) =>
  ({ story: 'Story', reply: 'Reply', thumbs_up: 'Thumbs up', rerequest: 'Re-request' }[w] || w);

function waitRow(text) {
  const row = el('div', 'res wait');
  row.appendChild(el('span', 'st', '·'));
  const c = el('span', 'c');
  c.appendChild(el('span', 't', text));
  row.appendChild(c);
  return row;
}

function resultSec(title, rows, shape) {
  if (!rows?.length) return document.createComment(`no ${title}`);
  const sec = el('div', 'sec');
  sec.appendChild(el('div', 'eyebrow', title));
  const group = el('div', 'group');
  for (const x of rows) {
    const s = shape(x);
    const row = el('div', 'res ' + s.cls);
    row.appendChild(el('span', 'st', s.st));
    const c = el('span', 'c');
    if (x.label) c.appendChild(el('span', 'p', x.label));
    c.appendChild(el('span', s.held ? 't held' : 't', s.t));
    if (s.link) {
      const a = el('a', 'm', s.link);
      a.href = s.link;
      a.target = '_blank';
      a.rel = 'noreferrer';
      a.style.color = 'var(--work)';
      c.appendChild(a);
    }
    if (s.err) c.appendChild(el('span', 'err', s.err));
    row.appendChild(c);
    group.appendChild(row);
  }
  sec.appendChild(group);
  return sec;
}

/* ---------- screen 6: manual — the phase that waits for you ---------- */

/** The batch stopped after committing everything else, and is waiting for you.
 *
 *  The ordering is the whole design. Hand-editing breaks the propose-only
 *  invariant that the tree stays clean until the final action, and merging your
 *  edits into the batch was rejected — the commit would sweep up whatever you
 *  happened to have touched, so "exactly what you approved" stops being true. So
 *  the accepted patches are written and committed *first*, and you then edit a tree
 *  that already reflects every other decision, which is often why this thread
 *  needed hands in the first place.
 *
 *  Two things fall out for free: you cannot describe work you have not done, so the
 *  comment is written here rather than guessed at on the card; and `git diff` makes
 *  the file list complete, because nobody had to declare it. */
function rvManual(root) {
  const m = reviewState.report.manual;
  const refused = reviewState.report.refused;
  root.appendChild(rvHead(refused ? 'stopped' : 'your turn', `${m.threads.length} by hand`));

  if (refused) {
    // Amber rather than the clean banner: this is not "nothing happened". Half
    // one's commit is on the branch and your edits are still on disk — what did
    // *not* happen is the push and the posting.
    const warn = el('div', 'banner');
    warn.appendChild(el('span', 'ico', '▲'));
    const tx = el('span', 'tx');
    tx.appendChild(el('b', null, 'Nothing was pushed or posted.'));
    tx.appendChild(el('p', null, refused));
    warn.appendChild(tx);
    root.appendChild(warn);
  }
  root.appendChild(rvStrip(null));

  const body = el('div', 'body');

  // What is already committed. First, because it changes what you are editing.
  const done = el('div', 'sec');
  const group = el('div', 'group');
  const row = el('div', 'res ok');
  row.appendChild(el('span', 'st', '✓'));
  const c = el('span', 'c');
  const t = el('span', 't');
  if (m.files.length) {
    t.appendChild(document.createTextNode(
      `${m.files.length} accepted change${m.files.length === 1 ? '' : 's'} written and ` +
      'committed locally — '));
    t.appendChild(el('span', 'm', m.committed.slice(0, 7)));
    t.appendChild(document.createTextNode(
      `${m.amend ? ', ' + m.amend : ''}. Not pushed.`));
  } else {
    // Every other thread was reply-only, so there was nothing to write.
    t.appendChild(document.createTextNode('Nothing was written — every other thread was words only. '));
    t.appendChild(el('span', 'm', m.committed.slice(0, 7)));
    t.appendChild(document.createTextNode(' is unchanged, and nothing is pushed.'));
  }
  c.appendChild(t);
  row.appendChild(c);
  group.appendChild(row);
  done.appendChild(group);
  body.appendChild(done);

  // What you have edited, once, above the threads. Derived from `git diff`, not
  // from anything anyone declared — which is what keeps the batch only what you
  // approved even though nobody listed these files.
  // `?.` throughout: this renders whatever the diff fetch last returned, and a
  // screen that throws mid-render leaves the overlay blank with no way back — the
  // phase is the one screen where the batch is already half-done.
  const ch = manualState.changed;
  const mine = el('div', 'sec');
  const head = el('div', 'eyebrow', ch?.files?.length ? 'what you changed ' : 'nothing changed yet ');
  const again = el('button', 'revert', 're-read the tree');
  again.onclick = () => loadManualDiff();
  head.appendChild(again);
  mine.appendChild(head);
  if (ch?.files?.length) {
    mine.appendChild(fileListLabel(ch.files, 'you changed'));
    if (ch.diff) mine.appendChild(hunkEl(ch.diff, false));
  } else {
    const none = el('p', null,
      'Edit the files in a session or your editor, then re-read. A manual thread ' +
      'does not have to change code — the comment is what is required.');
    none.style.cssText = 'color:var(--dim);font-size:12px;max-width:70ch';
    mine.appendChild(none);
  }
  body.appendChild(mine);

  // The threads themselves, each with the reviewer's words and your comment.
  const sec = el('div', 'sec');
  sec.appendChild(el('div', 'eyebrow', 'needs your hands'));
  for (const th of m.threads) {
    sec.appendChild(rvManualRow(th));
  }
  body.appendChild(sec);
  root.appendChild(body);

  const ready = m.threads.every((th) => (manualState.comments[th.thread_id] || '').trim());
  root.appendChild(rvActs([
    actBtn('continue · push and post', 'warm', () => finishManual(), !ready),
    actBtn('back', null, () => {
      /* The commit stays — it is on the branch and unpushed, which is a state git
         is perfectly happy in. Only the phase is left. */
      reviewState.report = null;
      reviewState.screen = 'final';
      renderReview();
    }),
  ], ready
    ? 'writes nothing further · your edits are already on disk'
    : `a comment is required on ${m.threads.length === 1 ? 'this thread' : 'each thread'}`));
}

/** One thread waiting on you: what they said, what you changed, what you will say. */
function rvManualRow(th) {
  const wrap = el('div', 'manrow');

  const top = el('div', 'top');
  const [where, who] = splitLabel(th.label);
  top.appendChild(el('span', 'p', where));
  top.appendChild(el('span', 'who', who));
  const said = (manualState.comments[th.thread_id] || '').trim();
  top.appendChild(said
    ? el('span', 'state got', 'answered')
    : el('span', 'state wait', 'needs a comment'));
  wrap.appendChild(top);

  wrap.appendChild(el('blockquote', null, th.comment));

  const label = el('div', 'eyebrow', 'your comment ');
  label.appendChild(el('span', 'req', '· required'));
  wrap.appendChild(label);

  const box = el('textarea', 'box');
  box.setAttribute('aria-label', 'Comment');
  // The card's box was a draft; this is the comment. Seeded from it, because a
  // half-written intention is still a starting point.
  box.value = manualState.comments[th.thread_id] ?? th.draft ?? '';
  box.oninput = () => {
    manualState.comments[th.thread_id] = box.value;
    // Only the button's enabled state depends on this, so nothing is re-rendered:
    // repainting here would drop focus out of the box mid-sentence.
    const send = $('rvoverlay').querySelector('.acts .act.warm');
    if (send) {
      const m = reviewState.report.manual;
      send.disabled = !m.threads.every((x) => (manualState.comments[x.thread_id] || '').trim());
    }
    // The row's own chip tracks the same thing, so it is flipped by hand rather
    // than by a repaint that would take the focus with it.
    const chip = wrap.querySelector('.state');
    if (chip) {
      const answered = !!box.value.trim();
      chip.className = 'state ' + (answered ? 'got' : 'wait');
      chip.textContent = answered ? 'answered' : 'needs a comment';
    }
  };
  wrap.appendChild(box);

  const foot = el('div', 'foot');
  foot.appendChild(el('span', null, '(via orchestrator) is appended when it posts'));
  wrap.appendChild(foot);
  return wrap;
}

/** `a.ts:12 · alice` back into its two halves, for the row's own layout. */
function splitLabel(label) {
  const at = label.lastIndexOf(' · ');
  return at < 0 ? [label, ''] : [label.slice(0, at), label.slice(at + 3)];
}

/** What you have edited since the phase opened.
 *
 *  One `git diff` for the whole tree, shown against every waiting thread rather
 *  than split between them: two manual threads editing one file cannot be told
 *  apart, and the commit is the tree's anyway. Pretending to attribute it would be
 *  a guess dressed as a fact. */
async function loadManualDiff() {
  try {
    manualState.changed = await get(`/api/pr/${reviewState.pr}/manual`);
  } catch (e) {
    toast(e.message, true);
  }
  renderReview();
}

/** Send the comments and let the batch finish.
 *
 *  Carries the decisions again, and the sha the phase reported. There is no pending
 *  state on the daemon to go stale: what the first half produced is a commit, so
 *  git is the record, and `HEAD` moving is what a refusal is made of. */
async function finishManual(replay) {
  if (reviewState.busy) return;
  // Only reachable from the phase's own button and the report's retry, but it reads a
  // phase out of the report and a throw here would blank the screen mid-batch.
  const m = replay ? { threads: [] } : reviewState.report?.manual;
  if (!m) return;
  const missing = m.threads.filter((th) => !(manualState.comments[th.thread_id] || '').trim());
  if (missing.length) {
    return toast('a comment is required on a manual thread — the reviewer would get ' +
                 'a commit and silence otherwise', true);
  }

  /* Replayed verbatim on a retry. Rebuilding it would re-derive `batchPayload()`
     from live state and re-read the tree, and the tree has moved on — the fold has
     already happened — so the retry has to be the same request, not a new one. */
  const payload = replay || {
    batch: batchPayload(),
    committed: m.committed,
    comments: manualState.comments,
    // What the screen showed you, which is what you pressed the button under. The
    // daemon refuses anything dirty that is not in here rather than sweeping it into
    // the commit.
    files: (manualState.changed?.files || []).map((f) => f.path),
  };

  reviewState.busy = true;
  renderReview();
  try {
    const got = await call(`/api/pr/${reviewState.pr}/manual/done`, payload);
    manualState.finished = payload;
    /* A refusal carries no phase, and taking it at face value would drop the only
       record of one — landing on a report that says "nothing was pushed, the
       worktree is as it was" when half one's commit is on the branch and your edits
       are still on disk, with no way back to the phase. So the phase is kept and the
       refusal is shown on top of it. */
    /* Only when pressing again could work. A stray file or a failing hook is
       something you act on and retry; the branch having moved under the phase is
       not — that sha can never match again, so restoring the phase would pin you to
       a screen whose only button is guaranteed to fail. */
    if (got.refused && got.retryable && !got.manual) got.manual = m;
    reviewState.report = got;
    reviewState.screen = got.manual ? 'manual' : 'report';
    /* Cleared only when there is nothing left to come back to. The comments
       described work that is now pushed, and keeping them would arm the next phase
       on this PR with an answer to a different question — but the report's own
       `retry` button is live while anything failed, and that retry re-enters the
       phase, so throwing them away first would lose the words that were already
       posted against. */
    if (!got.manual && !got.refused && !(got.failed || []).length) {
      manualState.comments = {};
      manualState.changed = null;
    }
    // A stray-file refusal is about a tree that has moved on, so re-read it: the
    // screen then shows what is actually there and pressing continue is a real
    // second look rather than the same refusal again.
    if (got.refused && got.retryable) loadManualDiff();
  } catch (e) {
    toast(e.message, true);
  }
  reviewState.busy = false;
  renderReview();
}

/* ---------- the controller ---------- */

function renderReview() {
  const root = $('rvoverlay');
  root.replaceChildren();
  if (!reviewState.open || !reviewState.data) return;

  if (reviewState.report?.manual) reviewState.screen = 'manual';
  else if (reviewState.report) reviewState.screen = 'report';
  else if (reviewState.data.gate) reviewState.screen = 'gate';
  else if (!reviewState.data.proposals) reviewState.screen = 'intake';

  ({
    intake: rvIntake,
    gate: rvGate,
    overview: rvOverview,
    card: rvCard,
    final: rvFinal,
    run: rvRun,
    manual: rvManual,
    report: rvReport,
  })[reviewState.screen](root);
}

/** Open the overlay on a PR, or refresh what it is showing.
 *
 *  The proposals ride the snapshot too, but this fetch is what the overlay reads:
 *  it also carries the gate state and the thread bodies, and it is deliberately
 *  explicit rather than a side effect of a tick. */
async function loadReview(pr) {
  const p = (snap.prs || []).find((x) => x.number === pr);
  reviewState.pr = pr;
  try {
    const data = await get(`/api/pr/${pr}/review`);
    // Carried through so the header and the push row can name them without a
    // second lookup; the endpoint answers about threads, not about the PR row.
    data.title = p?.title || 'review';
    data.url = p?.url;
    data.head_ref = p?.head_ref;
    reviewState.data = data;

    /* A force-push between triage and now invalidates every proposed patch, so
       the decisions made against the old head are dropped rather than sent
       against code that is no longer there. */
    const base = data.proposals?.base_sha || null;
    if (reviewState.head && base && reviewState.head !== base) {
      reviewState.picks = {};
      reviewState.skipped = {};
      reviewState.drafts = {};
      reviewState.i = 0;
      // A comment describes work against a tree that has moved, so it is no longer
      // an answer to anything.
      manualState.comments = {};
      manualState.changed = null;
      toast('the branch moved — decisions cleared, re-read the cards');
    }
    reviewState.head = base;

    /* A batch that stopped for the manual phase now lives on the daemon, so a reload,
       a restart, or coming back to this PR resumes it instead of stranding a branch
       whose patches are already committed. Only adopted when the screen is not
       already showing one, so a live phase's own state is never clobbered by a tick. */
    if (data.manual && !reviewState.report) {
      reviewState.report = {
        refused: null, files: [], amend: null, pushed: null,
        landed: [], failed: [], skipped: [], rerequested: [], held_back: [],
        manual: data.manual,
      };
      reviewState.screen = 'manual';
      loadManualDiff();
    }
    renderReview();
  } catch (e) {
    toast(e.message, true);
    if (!reviewState.data) closeReview();
  }
}

async function openReview(pr) {
  // Two overlays at the same z-index would stack; the diff viewer goes first.
  if (diffState.open) closeDiff();
  if (reviewState.pr !== pr) {
    manualState.comments = {};
    manualState.changed = null;
    reviewState.picks = {};
    reviewState.skipped = {};
    reviewState.drafts = {};
    reviewState.report = null;
    reviewState.head = null;
    reviewState.i = 0;
    reviewState.screen = 'intake';
    reviewState.data = null;
  }
  reviewState.open = true;
  $('rvoverlay').classList.add('on');
  await loadReview(pr);
}

/** Close, keeping every decision. Reopening the same PR resumes where you were:
 *  nothing has been written, so there is nothing to lose by leaving. */
function closeReview() {
  reviewState.open = false;
  reviewState.busy = false;
  $('rvoverlay').classList.remove('on');
  $('rvoverlay').replaceChildren();
}

/** Run something that changes the daemon's side, then refetch.
 *
 *  `andClose` is for the two calls that hand you to a session — a triage run and
 *  `fix-pr`, because the useful next screen is the pty, not this one. */
async function rvAct(fn, said, andClose) {
  if (reviewState.busy) return;
  reviewState.busy = true;
  renderReview();
  try {
    const r = await fn();
    if (said) toast(said);
    if (andClose) {
      if (r?.session) pendingSelect = r.session;
      reviewState.busy = false;
      return closeReview();
    }
  } catch (e) {
    toast(e.message, true);
  }
  reviewState.busy = false;
  await loadReview(reviewState.pr);
}

/* ---------- moving through the cards ---------- */

/** Take the selected position and move on. On the last card that means the
 *  final screen, which is the only place the batch can be sent from. */
function acceptCard() {
  const q = queue();
  const item = q[reviewState.i];
  if (!item) return;
  const pos = positionOf(item);

  // A reply-only position with an empty box would post a blank comment, which
  // cannot be deleted from here — the daemon refuses it too.
  if (pos.stance !== 'agree' && modeOf(item) === 'agent' && !replyOf(item).trim()) {
    return toast('this position posts a reply — write one, or pick another', true);
  }
  reviewState.picks[item.t.id] = pickOf(item);
  delete reviewState.skipped[item.t.id];
  // Accepting is the agent doing the work. Says so out loud, so pressing it after
  // `manual` takes the thread back rather than leaving the older answer standing.
  delete reviewState.modes[item.t.id];
  advance();
}

/** Same decision, different hands: you write the code, the session waits.
 *
 *  Deliberately not a fourth stance. The words and the position are unchanged —
 *  only who implements them — and the reply is written later, in the phase, once
 *  the work exists. So an empty box is not a refusal here the way it is under
 *  `accept`. */
function manualCard() {
  const item = queue()[reviewState.i];
  if (!item) return;
  reviewState.picks[item.t.id] = pickOf(item);
  reviewState.modes[item.t.id] = 'manual';
  delete reviewState.skipped[item.t.id];
  advance();
}

/** One skip state, not two. "Deliberately leaving this" and "ran out of time"
 *  have identical consequences — thread stays open, nothing posted, reviewer
 *  held back — so the distinction would be bookkeeping the tool cannot act on. */
function skipCard() {
  const item = queue()[reviewState.i];
  if (!item) return;
  reviewState.skipped[item.t.id] = true;
  delete reviewState.picks[item.t.id];
  delete reviewState.modes[item.t.id];
  advance();
}

function advance() {
  const q = queue();
  const next = q.findIndex((x, i) =>
    i > reviewState.i && !isHandled(x) && !reviewState.skipped[x.t.id]);
  if (next >= 0) reviewState.i = next;
  else if (q.every((x) => isHandled(x) || reviewState.skipped[x.t.id])) reviewState.screen = 'final';
  else reviewState.i = Math.min(reviewState.i + 1, q.length - 1);
  renderReview();
}

/** j / k. Both ways on purpose: a later thread often changes what an earlier one
 *  deserves as an answer, and decisions stage rather than post, so revising one
 *  is free right up to the final action. */
function moveCard(delta) {
  const q = queue();
  if (!q.length) return;
  reviewState.i = Math.max(0, Math.min(q.length - 1, reviewState.i + delta));
  reviewState.screen = 'card';
  renderReview();
}

/* ---------- the batch ---------- */

/** Send it. The payload carries thread ids and position *indices*, never
 *  content: the daemon already holds the proposals, and echoing them back would
 *  let a client substitute a different patch than the one that was reviewed. A
 *  `reply` overrides the drafted wording, and that is all it can override. */
/** Thread ids and position indices, never content. Built in one place because the
 *  manual phase has to send exactly the same thing again to finish. */
function batchPayload() {
  const decisions = queue().filter(isHandled).map((item) => {
    const i = pickOf(item);
    const typed = reviewState.drafts[draftKey(item.t.id, i)];
    return {
      thread_id: item.t.id,
      position: i,
      reply: typed === undefined ? null : typed,
      mode: modeOf(item),
    };
  });
  return { base_sha: reviewState.data.proposals.base_sha, decisions };
}

async function sendBatch() {
  if (reviewState.busy) return;
  const { decisions } = batchPayload();
  if (!decisions.length) return toast('nothing to send — every thread was skipped', true);

  reviewState.busy = true;
  renderReview();
  try {
    reviewState.report = await call(`/api/pr/${reviewState.pr}/post`, batchPayload());
    // A batch that stopped for the manual phase is not a report yet.
    reviewState.screen = reviewState.report.manual ? 'manual' : 'report';
    if (reviewState.report.manual) loadManualDiff();
  } catch (e) {
    // A rejected request is the daemon refusing before it wrote anything —
    // a bad index, a thread that has gone, a gate that closed under you.
    toast(e.message, true);
  }
  reviewState.busy = false;
  renderReview();
}

/** Hand the decisions to a session and watch it work.
 *
 *  The other half of `sendBatch`, and the one the flow is being moved to: instead
 *  of the daemon applying every patch in one go and refusing whatever will not
 *  apply, a session works down the same plan, adapts each fix to the branch as it
 *  is now, and stops to ask you when only you can answer. */
async function startRun() {
  if (reviewState.busy) return;
  const { decisions } = batchPayload();
  if (!decisions.length) return toast('nothing to hand over — every thread was skipped', true);

  reviewState.busy = true;
  renderReview();
  try {
    const r = await call(`/api/pr/${reviewState.pr}/resolve-run`, batchPayload());
    reviewState.screen = 'run';
    // The session is where the work is now, so the rail should be pointing at it
    // when you close the overlay.
    if (r.session) pendingSelect = r.session;
  } catch (e) {
    toast(e.message, true);
  }
  reviewState.busy = false;
  renderReview();
}

/** What a thread's row says while the session works. Present tense until it is
 *  settled, because a run is watched, not read afterwards. */
const RUN_STATE = {
  pending: ['wait', 'waiting its turn'],
  committed: ['work', 'committed — your call on the reply'],
  replied: ['done', 'answered'],
  held: ['held', 'committed, reply kept back'],
  manual: ['manual', 'yours to write'],
  words_only: ['reply', 'words only'],
  needs_you: ['stop', 'needs you'],
};

/** Phase 3: an account of what happened, while it is happening.
 *
 *  Reads the daemon's own record rather than a report handed back at the end, so
 *  a run you are half-way through is as legible as a finished one — and a run
 *  whose session died still shows exactly how far it got. */
function rvRun(root) {
  const run = (snap.resolve_runs || {})[reviewState.pr];
  root.appendChild(rvHead('the session is working'));

  const body = el('div', 'body');
  if (!run) {
    body.appendChild(el('div', 'note',
      'No run on this PR. If you just started one, the daemon has not reported it yet.'));
    root.appendChild(body);
    return;
  }

  const sec = el('div', 'sec');
  for (const t of run.threads) {
    const [kind, word] = RUN_STATE[t.status] || ['wait', t.status];
    const row = el('div', 'stage-row');
    row.appendChild(el('span', 'k ' + kind, word));
    const c = el('span', 'c');
    c.appendChild(el('span', 'p', t.location));
    if (t.commit) c.appendChild(el('span', 't', t.commit.slice(0, 7)));
    if (t.note) c.appendChild(el('span', 't', t.note));
    row.appendChild(c);
    sec.appendChild(row);
  }
  body.appendChild(sec);

  // The count that matters is not "how many done" but which kinds, so the tail
  // buttons can be read against it.
  const by = (st) => run.threads.filter((t) => t.status === st).length;
  const left = by('pending') + by('committed');
  const foot = el('div', 'sec');
  foot.appendChild(el('div', 'note', left
    ? `${left} still moving. The buttons below are yours whenever you want them; `
      + 'nothing here fires on its own.'
    : 'Nothing is moving. What is on the branch is what the session finished.'));
  body.appendChild(foot);
  root.appendChild(body);

  root.appendChild(rvActs([
    actBtn('push the branch', 'warm', () => runTail('push')),
    actBtn('re-request review', null, () => runTail('rerequest')),
    actBtn('back to the threads', null, () => { reviewState.screen = 'card'; renderReview(); }),
  ], 'resolving a thread stays the reviewer\'s own button, by design'));
}

/** The two claims about the whole branch. Explicit, and never a side effect of
 *  the last reply going out. */
async function runTail(what) {
  if (reviewState.busy) return;
  reviewState.busy = true;
  renderReview();
  try {
    const r = await call(`/api/pr/${reviewState.pr}/run/${what}`);
    if (what === 'push') toast(`pushed ${r.pushed}`);
    else {
      const n = (r.rerequested || []).length;
      toast(n ? `re-requested ${r.rerequested.join(', ')}` : 'nobody to re-request yet');
      for (const f of r.failed || []) toast(f, true);
    }
  } catch (e) {
    toast(e.message, true);
  }
  reviewState.busy = false;
  renderReview();
}

/** Keys claimed while the overlay is open. Returns whether it handled one.
 *
 *  Only bare keys, and only when nothing is focused for typing: the capture
 *  handler runs before any element listener wherever focus is, so a reply
 *  containing "check the job" would otherwise jump cards mid-sentence. */
function reviewKey(e) {
  if (e.key === 'Enter') {
    if (reviewState.screen === 'card') { acceptCard(); return true; }
    // Deliberately dead on the final screen: across the cards Enter means
    // "accept this one thing", and on a batch it has no natural meaning.
    return reviewState.screen === 'final';
  }
  if (e.key === 'j' || e.key === 'k') {
    if (reviewState.screen !== 'card') return false;
    moveCard(e.key === 'j' ? 1 : -1);
    return true;
  }
  if (e.key === 's' && reviewState.screen === 'card') { skipCard(); return true; }
  if (e.key === 'm' && reviewState.screen === 'card') { manualCard(); return true; }
  if (/^[1-9]$/.test(e.key) && reviewState.screen === 'card') {
    const item = queue()[reviewState.i];
    const i = +e.key - 1;
    if (item && item.p.positions[i] && offered(item.p.positions[i])) {
      reviewState.picks[item.t.id] = i;
      delete reviewState.skipped[item.t.id];
      renderReview();
    }
    return true;
  }
  return false;
}

// ---------------------------------------------------------------------------
// Review queue (§6b)
// ---------------------------------------------------------------------------

let showReviews = true;
let showBlockedReviews = false;

/** Compact age: `just now`, `5h`, `2d`. */
function reviewAge(hours) {
  if (hours < 1) return 'now';
  if (hours < 48) return `${Math.round(hours)}h`;
  return `${Math.round(hours / 24)}d`;
}

/** Why this row is in your queue, when there is a reason worth the width. */
function reviewReason(r) {
  if (r.blockers && r.blockers.length) return r.blockers.join(', ');
  if (r.needs_re_review) return 're-requested';
  if (r.is_draft) return 'draft';
  if (r.prio === 0) return 'prio stopper';
  if (r.prio === 1) return 'prio';
  if (r.prio === 3) return 'team';
  return '';
}

function renderReviews() {
  const block = $('rvblock');
  const head = $('rvhead');
  const list = $('rvlist');
  head.replaceChildren();
  list.replaceChildren();
  block.classList.toggle('closed', !showReviews);

  const rv = snap.reviews;
  head.setAttribute('aria-expanded', String(showReviews));
  head.appendChild(el('span', 'caretr', '\u203a'));
  head.appendChild(el('span', 'eyebrow', 'Review queue'));
  const count = el('span', 'rvcount');

  const refresh = refreshButton('review', snap.reviews_poll ?? 0, '/api/reviews/refresh',
    snap.reviews_polling);

  if (!rv || rv.state !== 'ok') {
    // Never an empty queue: a broken command reads as broken (§6b). Startup is
    // not broken, so it says so differently.
    const pending = !rv || rv.state === 'pending';
    count.appendChild(el('span', pending ? null : 'f', pending ? 'polling…' : 'unavailable'));
    head.appendChild(count);
    head.appendChild(refresh);
    head.title = rv?.reason || '';
    list.appendChild(el('div', 'fempty', pending
      ? 'waiting for the first poll'
      : `reviews unavailable\n${(rv?.reason || '').slice(0, 160)}`));
    head.onclick = () => { showReviews = !showReviews; renderReviews(); };
    return;
  }

  const rows = rv.actionable || [];
  const blocked = rv.blocked || [];
  count.appendChild(el('span', rows.length ? 'n' : null,
    rows.length ? `${rows.length} waiting` : 'clear'));
  head.appendChild(count);
  head.appendChild(refresh);
  head.onclick = () => { showReviews = !showReviews; renderReviews(); };

  // The file-count column only earns its width once the source emits it.
  const anyFiles = [...rows, ...blocked].some((r) => r.changed_files != null);

  const rowFor = (r, dim) => {
    // Rows are anchors, so ⌘-click and copy-link behave, and the browser
    // already holds the GitHub session (§6b).
    const a = el('a', 'rvrow' + (dim ? ' dim' : ''));
    // The conversation tab: what a reviewer needs first is the description and
    // what has already been said, not a wall of diff with none of the context.
    a.href = r.url;
    a.target = '_blank';
    a.rel = 'noreferrer';
    /* Grey unless it is a re-review: the source only sets `needsReReview` for
     * rows in *your* queue, so it is the one thing here that is waiting on you
     * rather than on a colleague. Amber is the legend's "needs you" (§9).
     * It cannot tell a personal re-request from a team one — `prio` splits that
     * only for first requests.
     * A `prio` or `prio stopper` label outranks both: red, because that queue is
     * somebody's release waiting on you. */
    const dot = r.prio <= 1 ? ' prio' : r.needs_re_review ? ' blocked' : '';
    a.appendChild(el('span', 'dot' + dot));
    // Age, not the PR number: how long it has waited is what tells you to pick
    // it up. The whole row already links to the PR, so the number earns nothing.
    const age = el('span', 'num', reviewAge(r.age_hours || 0));
    age.title = `#${r.number}`;
    a.appendChild(age);
    a.appendChild(el('span', 'ttl', r.title));
    a.appendChild(el('span', 'who', r.author));
    // File count stands in for review cost — 37 files is a different
    // commitment from 1 — but an empty column just steals width from the title.
    if (anyFiles) a.appendChild(el('span', 'fc', r.changed_files != null ? String(r.changed_files) : '·'));
    const why = reviewReason(r);
    if (why) a.appendChild(el('span', 'why', why));
    return a;
  };

  for (const r of rows) list.appendChild(rowFor(r, false));
  if (!rows.length) list.appendChild(el('div', 'fempty', 'Nothing waiting on you.'));

  // Blocked on conflicts or red checks: waiting on their author, not on you.
  // Sunk rather than dropped, because sometimes you still want to look — but
  // folded, so they do not pad the queue you actually work from.
  if (blocked.length) {
    const t = el('button', 'arctoggle');
    t.setAttribute('aria-expanded', String(showBlockedReviews));
    t.appendChild(el('span', 'caretr', '\u203a'));
    t.appendChild(el('span', null, `${blocked.length} not reviewable`));
    t.title = blocked.map((r) => `#${r.number} — ${r.blockers.join(', ')}`).join('\n');
    t.onclick = () => { showBlockedReviews = !showBlockedReviews; renderReviews(); };
    list.appendChild(t);
    if (showBlockedReviews) for (const r of blocked) list.appendChild(rowFor(r, true));
  }
}



// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

function select(id) {
  selected = id;
  const s = currentSession();
  // A session created a moment ago is not in the snapshot yet. Blanking the
  // terminal here would strand it: the next snapshot sees `selected` already
  // set and never opens one.
  const shown = s ? showTerm(`session:${s.id}`, $('termwrap')) : null;
  render();
  // Picking a session is picking where you are about to type. After the frame
  // that un-hides it, for the same reason the drawer waits: xterm refuses focus
  // while its host still has no dimensions.
  if (shown) {
    requestAnimationFrame(() => {
      try {
        shown.term.focus();
      } catch (e) { /* disposed while we waited */ }
    });
  }
}

/** A session the daemon has just been asked to create.
 *
 *  Setting `selected` alone is not enough: the terminal is only opened when a
 *  session is shown, and the snapshot handler skips that once something is
 *  already selected. */
let pendingSelect = null;

async function newSession(workspace) {
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
async function newWorktree(named) {
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

async function newShell() {
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
  } catch (e) {
    toast(e.message, true);
  }
}

/** Teardown is offered, never automatic, and the preflight is shown in full
 *  before anything is removed (§2). */
async function teardown(wsId) {
  let pf;
  try {
    pf = await get(`/api/workspace/${encodeURIComponent(wsId)}/preflight`);
  } catch (e) {
    return toast(e.message, true);
  }
  const lines = pf.checks.map((c) => `${c.passed ? '✓' : '✗'} ${c.name} — ${c.detail}`);
  if (!pf.can_remove) {
    return toast(`cannot remove ${wsId}:\n${lines.join('\n')}`, true);
  }
  if (!confirm(`Remove worktree ${wsId}?\n\n${lines.join('\n')}`)) return;
  try {
    await call(`/api/workspace/${encodeURIComponent(wsId)}/teardown`);
    toast(`removed ${wsId}`);
  } catch (e) {
    toast(e.message, true);
  }
}

$('ovclose').onclick = closeDiff;
$('ovprev').onclick = () => stepChange(-1);
$('ovnext').onclick = () => stepChange(1);
$('ovmode').onclick = () => {
  if (editState.on && !closeEditor()) return;
  diffState.split = !diffState.split;
  renderDiff();
};
$('ovedit').onclick = () => (editState.on ? closeEditor() : openEditor());
$('ovsave').onclick = saveEditor;
$('reposwitch').onclick = () =>
  toast('switching repositories is not implemented yet', true);
$('addshell').onclick = newShell;
$('dcollapse').onclick = () => setDrawerCollapsed(!drawerCollapsed);
$('refreshbtn').onclick = () => {
  const wsId = currentWorkspaceId();
  if (wsId) call(`/api/workspace/${encodeURIComponent(wsId)}/reconcile`).catch((e) => toast(e.message, true));
};
$('killbtn').onclick = () => {
  const s = currentSession();
  if (s) closeSession(s.id);
};

// ---------------------------------------------------------------------------
// Keyboard (§9)
// ---------------------------------------------------------------------------

window.addEventListener('keydown', (e) => {
  // First, or Escape closes the overlay underneath and leaves the menu floating
  // over it.
  if (e.key === 'Escape' && menuOpen()) {
    e.preventDefault();
    closeMenu();
    return;
  }
  if (e.key === 'Escape' && settingsOpen()) {
    e.preventDefault();
    closeSettings();
    return;
  }
  if ((e.metaKey || e.ctrlKey) && e.key === 's' && editState.on) {
    e.preventDefault();
    saveEditor();
    return;
  }
  /* The overlay wants bare Enter, j/k and digits, and this handler is registered
     with capture:true — it runs before any element listener wherever focus is.
     So the focus guard is not optional here the way it was for Escape/Ctrl+←/Alt+d. */
  if (reviewState.open) {
    const typing = !!e.target.closest?.('textarea, input, [contenteditable="true"]');
    if (e.key === 'Escape') {
      e.preventDefault();
      // Blur rather than close, or Escape out of a half-typed reply discards it.
      if (typing) e.target.blur();
      else closeReview();
      return;
    }
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      // Through the button rather than straight to `sendBatch`, or the shortcut
      // sends a batch the button itself refuses — which it did until now.
      if (reviewState.screen === 'final') {
        const send = $('rvoverlay').querySelector('.acts .act.warm');
        if (send && !send.disabled) send.click();
      }
      return;
    }
    // Alt is left alone so the session switcher keeps working underneath.
    if (!typing && !e.altKey && !e.ctrlKey && !e.metaKey && reviewKey(e)) {
      e.preventDefault();
      return;
    }
  }
  if (diffState.open) {
    if (e.key === 'Escape') { e.preventDefault(); closeDiff(); return; }
    // Ctrl+←/→ steps through the changeset. Was F7/⇧F7 — one key doing two jobs
    // by modifier, and a reach; the arrows read as "next/previous" on their own.
    if (e.ctrlKey && (e.key === 'ArrowLeft' || e.key === 'ArrowRight')) {
      e.preventDefault();
      stepChange(e.key === 'ArrowLeft' ? -1 : 1);
      return;
    }
  }
  if (e.altKey && e.key === 'd') {
    e.preventDefault();
    if (reviewState.open) return toast('close the review first');
    diffState.open ? closeDiff() : openDiff();
    return;
  }
  // Modifier combinations xterm does not claim, so they work with the terminal
  // focused.
  if (e.ctrlKey && e.key === '`') {
    e.preventDefault();
    newShell();
    return;
  }
  if (!e.altKey || e.ctrlKey || e.metaKey) return;

  const ordered = snap.sessions;
  const idx = ordered.findIndex((s) => s.id === selected);

  if (e.key === 'j' || e.key === 'k') {
    e.preventDefault();
    if (!ordered.length) return;
    const next = e.key === 'j' ? idx + 1 : idx - 1;
    select(ordered[(next + ordered.length) % ordered.length].id);
  } else if (e.key === 'b') {
    // Next *blocked* session, which is the one costing you the most.
    e.preventDefault();
    const blocked = ordered.filter(isWaiting);
    if (!blocked.length) return toast('nothing waiting on you');
    const after = blocked.find((s) => ordered.indexOf(s) > idx) || blocked[0];
    select(after.id);
  } else if (e.key === 'm') {
    e.preventDefault();
    const main = snap.workspaces.find((w) => w.is_main);
    if (main) newSession(main.id);
  }
}, true);

/* A terminal sizes itself to its host, and the host changes size for more reasons
 * than any one event covers: a window resize, a column drag, a font-size step, or
 * a compositor handing the window back at a different size than it took it —
 * which is the one that left the centre pane short of full height until you
 * switched sessions. So watch the box rather than enumerate the causes.
 *
 * Coalesced to one refit per frame, and a refit that changes nothing sends
 * nothing. */
let refitTimer = null;
function queueRefit() {
  // Settled, not per-frame: a drag or a compositor animation fires this dozens of
  // times, and fitting mid-flight is how a terminal ends up sized to a box that
  // is still moving.
  if (refitTimer) clearTimeout(refitTimer);
  refitTimer = setTimeout(() => {
    refitTimer = null;
    refitTerms();
  }, 120);
}

const hostObserver = new ResizeObserver(queueRefit);
for (const id of ['termwrap', 'drawerbody']) {
  const host = $(id);
  if (host) hostObserver.observe(host);
}
// Belt and braces for the focus case: if the window comes back with the same box
// but a parked renderer, nothing above fires and this costs nothing.
window.addEventListener('focus', queueRefit);
document.addEventListener('visibilitychange', queueRefit);

/* Both bottom panes are lists of links you follow out of the app, and what you
 * do out there (approve, comment, merge) is the very thing they list. Coming
 * back to a queue that still holds the review you just finished is the pane
 * lying until the next poll, so returning to the window pulses both pollers.
 *
 * Throttled, because alt-tabbing is not a reason to spend the GitHub budget, and
 * silent, because nobody asked for this one: the ↻ buttons stay the loud path. */
const RETURN_REFRESH_MS = 30_000;
let lastReturnRefresh = 0;
function refreshOnReturn() {
  if (document.hidden) return;
  if (Date.now() - lastReturnRefresh < RETURN_REFRESH_MS) return;
  lastReturnRefresh = Date.now();
  call('/api/prs/refresh').catch(() => {});
  call('/api/reviews/refresh').catch(() => {});
}
window.addEventListener('focus', refreshOnReturn);
document.addEventListener('visibilitychange', refreshOnReturn);

// ---------------------------------------------------------------------------
// Live state
// ---------------------------------------------------------------------------

function connect() {
  const sock = new WebSocket(`${WS_BASE}/ws/events?token=${encodeURIComponent(TOKEN)}`);
  sock.onmessage = (ev) => {
    snap = JSON.parse(ev.data);
    snapAt = Date.now();
    // A session whose pty is gone keeps its scrollback until it is dismissed,
    // so terminals are only torn down when the session disappears entirely.
    const liveProcs = new Set(
      snap.workspaces.flatMap((w) => w.processes.map((p) => `proc:${p.id}`))
    );
    for (const target of [...terms.keys()]) {
      if (target.startsWith('session:')) {
        const id = target.slice('session:'.length);
        if (!snap.sessions.some((s) => s.id === id)) closeTerm(target);
      } else if (!liveProcs.has(target)) {
        // A shell that closed cleanly is gone from the snapshot; drop its
        // terminal rather than leaving a hidden host behind forever.
        closeTerm(target);
      }
    }
    // The three panes describe one thing: the session you are in. The rail says
    // which, the centre shows its pty, the right pane its changes. So the
    // selection only ever points at something running — a session that finished
    // is not something to land on, and its scrollback is not what the centre is
    // for once it has stopped.
    if (selected) {
      const cur = snap.sessions.find((s) => s.id === selected);
      if (!cur || isArchived(cur)) selected = null;
    }

    // Switch to a session we asked for as soon as the daemon reports it.
    if (pendingSelect && snap.sessions.some((s) => s.id === pendingSelect)) {
      const id = pendingSelect;
      pendingSelect = null;
      select(id);
      return;
    }

    if (!selected) {
      // Default to whatever most needs you, among what is actually running.
      const first = snap.sessions.filter((x) => !isArchived(x));
      const pick = first.find(isWaiting) || first[0];
      if (pick) {
        selected = pick.id;
        showTerm(`session:${pick.id}`, $('termwrap'));
      } else {
        showTerm(null, $('termwrap'));
      }
    }
    render();
  };
  sock.onclose = () => {
    toast('daemon disconnected — retrying', true);
    setTimeout(connect, 1500);
  };
}

// ---------------------------------------------------------------------------
// Native window chrome
// ---------------------------------------------------------------------------

// The daemon decides this, not the user agent string: it is the side that knows
// whether it is being shown in a window it owns or in somebody's browser tab.
//
// The commands go over the same authenticated HTTP the rest of the UI uses,
// and the daemon — running inside the desktop process — calls Tauri's window
// API in Rust. No IPC bridge, so nothing here depends on which port we bound.
const CHROME = window.__ORCH__.chrome || 'none';

function setupChrome() {
  document.body.dataset.chrome = CHROME;
  if (CHROME === 'none') return;

  // The webview opens no target=_blank windows and wires no shell, so external
  // links (review rows, PR rows, the update nudge) go nowhere on their own —
  // under WSLg especially. Route them through the daemon's OS opener. A browser
  // tab (chrome 'none') returns above and opens them natively.
  document.addEventListener('click', (e) => {
    const a = e.target.closest && e.target.closest('a[target="_blank"]');
    if (!a || !/^https?:/i.test(a.href || '')) return;
    e.preventDefault();
    call('/api/open', { url: a.href }).catch((err) => toast(err.message, true));
  });

  const wcmd = (cmd) => call(`/api/window/${cmd}`).catch((e) => toast(e.message, true));

  for (const b of document.querySelectorAll('.wctl-btn')) {
    b.addEventListener('click', () => wcmd(b.dataset.cmd));
  }

  for (const bar of document.querySelectorAll('.top')) {
    bar.addEventListener('mousedown', (e) => {
      // Left button only, and only on the bar's own background: a drag that
      // swallowed clicks on the session name or the close button would make
      // the header unusable.
      if (e.button !== 0) return;
      if (e.target.closest('button, input, a, kbd, .ctx-btn')) return;
      // A double-click is the OS gesture for maximise, so it must not also
      // start a drag; the compositor keeps the drag alive past mouseup, which
      // would eat the second click.
      if (e.detail > 1) return;
      wcmd('start-drag');
    });
    bar.addEventListener('dblclick', (e) => {
      if (e.target.closest('button, input, a, kbd, .ctx-btn')) return;
      wcmd('toggle-maximize');
    });
  }

  for (const rz of document.querySelectorAll('.rz')) {
    rz.addEventListener('mousedown', (e) => {
      if (e.button !== 0) return;
      // Stop the browser starting a text selection that outlives the resize.
      e.preventDefault();
      wcmd(`resize/${rz.dataset.edge}`);
    });
  }
}

// ---------------------------------------------------------------------------
// Column widths
// ---------------------------------------------------------------------------

/* The three-column grid is two CSS variables wide, so a drag is a variable
 * write and nothing re-renders. Widths are a preference of this browser, not
 * state the daemon owns, so they live in localStorage — same reasoning as the
 * rail's collapsed sections. */
const COLS = {
  rail: { prop: '--rail', key: 'orch.railWidth', def: 290, min: 210 },
  files: { prop: '--files', key: 'orch.filesWidth', def: 296, min: 230 },
};
/* The centre pane holds a terminal; squeezing it to nothing to admire a wide
 * rail is not a layout anybody wants to be one drag away from. */
const CENTRE_MIN = 420;

/* Same idea on the other axis: the process drawer is one more variable, and the
 * terminal above it gets the same protection the centre column gets. */
const DRAWER = { prop: '--drawer', key: 'orch.drawerHeight', def: 210, min: 96 };
const TERM_MIN = 150;

const colWidth = (col) =>
  parseInt(getComputedStyle(document.documentElement).getPropertyValue(col.prop), 10) || col.def;

/** Set a column, clamped so the centre always survives and so does the other one. */
function setCol(col, px) {
  const other = col === COLS.rail ? COLS.files : COLS.rail;
  const room = window.innerWidth - CENTRE_MIN - colWidth(other);
  const width = Math.round(Math.max(col.min, Math.min(px, Math.max(col.min, room))));
  document.documentElement.style.setProperty(col.prop, `${width}px`);
  return width;
}

const drawerHeight = () =>
  parseInt(getComputedStyle(document.documentElement).getPropertyValue(DRAWER.prop), 10)
  || DRAWER.def;

/** Set the drawer height, clamped so the terminal above it stays usable. */
function setDrawer(px) {
  const centre = document.querySelector('.center');
  const room = (centre ? centre.clientHeight : window.innerHeight) - TERM_MIN;
  const h = Math.round(Math.max(DRAWER.min, Math.min(px, Math.max(DRAWER.min, room))));
  document.documentElement.style.setProperty(DRAWER.prop, `${h}px`);
  return h;
}

/** xterm sizes itself to its host, and a column drag is not a window resize. */
function refitTerms() {
  for (const entry of terms.values()) resize(entry);
}

function dragColumn(handle, col, fromLeft) {
  handle.addEventListener('mousedown', (e) => {
    if (e.button !== 0) return;
    // The titlebar's own drag handler lives under this strip.
    e.preventDefault();
    e.stopPropagation();
    handle.classList.add('dragging');
    document.body.classList.add('col-resizing');

    const move = (ev) => setCol(col, fromLeft ? ev.clientX : window.innerWidth - ev.clientX);
    const done = () => {
      window.removeEventListener('mousemove', move);
      handle.classList.remove('dragging');
      document.body.classList.remove('col-resizing');
      try {
        localStorage.setItem(col.key, String(colWidth(col)));
      } catch (err) { /* private mode: the drag still worked for this session */ }
      // Once, at the end: fitting on every mousemove means a pty resize per
      // mouse event, and the terminal reflows fine on release.
      refitTerms();
    };
    window.addEventListener('mousemove', move);
    window.addEventListener('mouseup', done, { once: true });
  });

  handle.addEventListener('dblclick', () => {
    setCol(col, col.def);
    try {
      localStorage.removeItem(col.key);
    } catch (err) { /* nothing to forget */ }
    refitTerms();
  });
}

/* The drawer's own drag. Not `dragColumn` with a flag: it reads clientY against
 * the centre pane rather than clientX against the window, and it has no sibling
 * column to leave room for. */
function dragDrawer(handle) {
  handle.addEventListener('mousedown', (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.stopPropagation();
    handle.classList.add('dragging');
    document.body.classList.add('row-resizing');

    const bottom = document.querySelector('.center').getBoundingClientRect().bottom;
    const move = (ev) => setDrawer(bottom - ev.clientY);
    const done = () => {
      window.removeEventListener('mousemove', move);
      handle.classList.remove('dragging');
      document.body.classList.remove('row-resizing');
      try {
        localStorage.setItem(DRAWER.key, String(drawerHeight()));
      } catch (err) { /* private mode: the drag still worked for this session */ }
      refitTerms();
    };
    window.addEventListener('mousemove', move);
    window.addEventListener('mouseup', done, { once: true });
  });

  handle.addEventListener('dblclick', () => {
    setDrawer(DRAWER.def);
    try {
      localStorage.removeItem(DRAWER.key);
    } catch (err) { /* nothing to forget */ }
    refitTerms();
  });
}

function setupColumns() {
  for (const col of Object.values(COLS)) {
    const saved = Number(localStorage.getItem(col.key));
    if (saved) setCol(col, saved);
  }
  const savedDrawer = Number(localStorage.getItem(DRAWER.key));
  if (savedDrawer) setDrawer(savedDrawer);
  dragColumn($('splitl'), COLS.rail, true);
  dragColumn($('splitr'), COLS.files, false);
  dragDrawer($('splitd'));
  // A window that got smaller can leave a stored size with no room for it.
  window.addEventListener('resize', () => {
    for (const col of Object.values(COLS)) setCol(col, colWidth(col));
    setDrawer(drawerHeight());
  });
}

// ---------------------------------------------------------------------------
// Settings
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
const FS_BASE = 1.155;
const ZOOM = { key: 'orch.uiZoom', def: 1, min: 0.8, max: 1.5, step: 0.05 };

/** The user-facing scale, where 1 is the default. */
let zoomScale = ZOOM.def;

/** The multiplier the stylesheet and the terminal both read. */
const uiScale = () =>
  Number(getComputedStyle(document.documentElement).getPropertyValue('--fs')) || FS_BASE;

function setZoom(z) {
  const next = Math.min(ZOOM.max, Math.max(ZOOM.min, Math.round(z * 100) / 100));
  zoomScale = next;
  document.documentElement.style.setProperty('--fs', String(next * FS_BASE));
  $('fsval').textContent = `${Math.round(next * 100)}%`;
  $('fsdown').disabled = next <= ZOOM.min;
  $('fsup').disabled = next >= ZOOM.max;
  // xterm draws its own text, so its font is set rather than inherited, and the
  // new glyph size means new rows and cols.
  const px = termFontSize();
  for (const entry of terms.values()) {
    if (entry.term.options.fontSize !== px) entry.term.options.fontSize = px;
  }
  refitTerms();
  return next;
}

function saveZoom(z) {
  try {
    if (z === ZOOM.def) localStorage.removeItem(ZOOM.key);
    else localStorage.setItem(ZOOM.key, String(z));
  } catch (e) { /* private mode: it still applies for this session */ }
}

const settingsOpen = () => !$('settings').hidden;

function closeSettings() {
  $('settings').hidden = true;
  $('gearbtn').setAttribute('aria-expanded', 'false');
}

function openSettings() {
  const panel = $('settings');
  // Read at open time rather than at setup: the first snapshot has usually not
  // landed when the page wires itself up.
  $('settingsver').textContent = snap.version ? `orchd ${snap.version}` : '';
  panel.hidden = false;
  $('gearbtn').setAttribute('aria-expanded', 'true');
  const r = $('gearbtn').getBoundingClientRect();
  const box = panel.getBoundingClientRect();
  panel.style.left = `${Math.min(r.left, window.innerWidth - box.width - 8)}px`;
  panel.style.top = `${r.bottom + 6}px`;
}

function setupSettings() {
  setZoom(Number(localStorage.getItem(ZOOM.key)) || ZOOM.def);

  $('gearbtn').onclick = (ev) => {
    ev.stopPropagation();
    if (settingsOpen()) closeSettings();
    else openSettings();
  };
  $('fsdown').onclick = () => saveZoom(setZoom(zoomScale - ZOOM.step));
  $('fsup').onclick = () => saveZoom(setZoom(zoomScale + ZOOM.step));
  $('fsreset').onclick = () => saveZoom(setZoom(ZOOM.def));

  // Same dismissal as the context menu: a click anywhere else puts it away.
  document.addEventListener('mousedown', (e) => {
    if (settingsOpen() && !e.target.closest('#settings, #gearbtn')) closeSettings();
  }, true);
}

setupSettings();
setupColumns();
setupChrome();
connect();
// The waiting clock has to tick even when nothing else changes.
setInterval(() => { renderRail(); }, 1000);

window.orchTeardown = teardown;
