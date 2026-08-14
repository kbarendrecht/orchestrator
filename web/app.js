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
function duration(ms) {
  if (ms == null) return '';
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
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
      return 'turn complete';
    case 'build_failing': return s.state.summary || 'build failing';
    case 'error': return s.state.message || 'error';
    case 'exited': return 'exited';
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
  if (k === 'archived') return 'archived';
  return 'idle';
}

function stateClass(s) {
  const k = s.state.state;
  if (k === 'build_failing' || k === 'error') return 'build';
  if (k === 'your_turn') return 'blocked';
  return '';
}

const isWaiting = (s) =>
  s.state.state === 'your_turn' || s.state.state === 'build_failing';

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

/** Attach to a pty, replaying the daemon's buffer first. */
function openTerm(target, parent) {
  if (terms.has(target)) return terms.get(target);

  const host = el('div', 'termhost');
  parent.appendChild(host);

  const term = new Terminal({
    theme: THEME,
    fontFamily: "'IBM Plex Mono', ui-monospace, monospace",
    fontSize: 12,
    lineHeight: 1.25,
    cursorBlink: true,
    scrollback: 10000,
    allowProposedApi: true,
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(host);
  try {
    term.loadAddon(new WebglAddon.WebglAddon());
  } catch (e) {
    // Software rendering is slower but correct; not worth failing over.
  }

  const sock = new WebSocket(
    `${WS_BASE}/ws/pty?token=${encodeURIComponent(TOKEN)}&target=${encodeURIComponent(target)}`
  );
  sock.binaryType = 'arraybuffer';
  const entry = { term, fit, sock, host, ready: false };

  sock.onopen = () => {
    entry.ready = true;
    resize(entry);
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
  try {
    entry.fit.fit();
  } catch (e) {
    return;
  }
  if (entry.ready && entry.sock.readyState === WebSocket.OPEN) {
    entry.sock.send(JSON.stringify({
      type: 'resize', rows: entry.term.rows, cols: entry.term.cols,
    }));
  }
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
    requestAnimationFrame(() => resize(entry));
  }
  return entry;
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

function sessionsOf(wsId) {
  return snap.sessions.filter((s) => s.workspace === wsId);
}

function render() {
  renderRail();
  renderContext();
  renderDrawer();
  renderFiles();
}

function renderRail() {
  const rail = $('rail');
  rail.replaceChildren();

  const main = snap.workspaces.find((w) => w.is_main);
  const worktrees = snap.workspaces.filter((w) => !w.is_main);

  // Main is pinned first (§9).
  if (main) rail.appendChild(groupFor(main, 'Main checkout'));

  const wtGroup = el('div', 'ws');
  const head = el('div', 'ws-head');
  const name = el('div', 'ws-name');
  name.appendChild(el('span', 'eyebrow', 'Worktrees'));
  head.appendChild(name);
  const add = el('button', 'plus', '+');
  add.title = 'New worktree session';
  add.onclick = newWorktree;
  head.appendChild(add);
  wtGroup.appendChild(head);

  if (!worktrees.length) {
    wtGroup.appendChild(el('div', 'railbtn', 'none yet'));
  }
  for (const w of worktrees) {
    if (!sessionsOf(w.id).length) {
      wtGroup.appendChild(emptyWorkspaceRow(w));
      continue;
    }
    appendSessions(wtGroup, w);
  }
  rail.appendChild(wtGroup);

  renderWaitbar();
}

function groupFor(w, label) {
  const group = el('div', 'ws');
  const head = el('div', 'ws-head');
  const name = el('div', 'ws-name');
  name.appendChild(el('span', 'eyebrow', label));
  head.appendChild(name);

  const sessions = sessionsOf(w.id).filter((s) => s.state.state !== 'archived');
  const occupant = sessions.find((s) => s.id === w.occupant && s.alive);

  // Main is exclusive. No queue: while it is occupied the button is disabled
  // and the rail says which session holds it (§2).
  const add = el('button', 'plus', '+');
  add.disabled = !!occupant;
  add.title = occupant
    ? `main is held by ${occupant.title || occupant.id.slice(0, 8)}`
    : 'New session in main';
  add.onclick = () => newSession(w.id);
  head.appendChild(add);
  group.appendChild(head);

  if (occupant) {
    const note = el('div', 'railbtn', `held by ${occupant.title || occupant.id.slice(0, 8)}`);
    note.style.paddingBottom = '4px';
    group.appendChild(note);
  }

  appendSessions(group, w);
  return group;
}

const isDone = (s) => s.state.state === 'archived' || s.state.state === 'exited';

let showArchived = false;

/** Live sessions always; finished ones behind a toggle, so a long-running rail
 *  does not fill with history you are not acting on. */
function appendSessions(group, w) {
  const all = sessionsOf(w.id);
  const live = all.filter((s) => !isDone(s));
  const done = all.filter(isDone);

  for (const s of live) group.appendChild(sessionRow(s, w));
  if (!live.length && !done.length) {
    group.appendChild(el('div', 'railbtn', 'no session'));
  }
  if (!done.length) return;

  // The session you are looking at always has a row, even when it has finished
  // and the rest are collapsed. Otherwise closing Claude hides the row while
  // the right pane and context bar still describe it, and nothing on screen
  // says what you are looking at.
  const pinned = !showArchived && done.find((s) => s.id === selected);
  if (pinned) group.appendChild(sessionRow(pinned, w));

  const remaining = done.length - (pinned ? 1 : 0);
  if (remaining > 0) {
    const toggle = el('button', 'arctoggle');
    toggle.setAttribute('aria-expanded', String(showArchived));
    toggle.appendChild(el('span', 'caretr', '›'));
    toggle.appendChild(el('span', null, `${remaining} finished`));
    toggle.onclick = () => { showArchived = !showArchived; renderRail(); };
    group.appendChild(toggle);
  }

  if (showArchived) for (const s of done) group.appendChild(sessionRow(s, w));
}

function emptyWorkspaceRow(w) {
  const btn = el('button', 'railbtn', `${w.id} · no session · start one`);
  btn.onclick = () => newSession(w.id);
  return btn;
}

/** Two lines: dot + name, then state and duration. No dirty-file count — that
 *  lives in the right column, one click away (§9). */
function sessionRow(s, w) {
  const btn = el('button', 'sess' + (s.kind.kind === 'automation' ? ' auto' : ''));
  btn.setAttribute('aria-current', String(s.id === selected));

  const row = el('div', 'sess-row');
  row.appendChild(el('span', 'dot ' + dotClass(s)));
  row.appendChild(el('span', 'sess-name', s.title || w.id));
  // Main holds one live session at a time but accumulates finished ones, so
  // without this every row in the group reads the same word.
  row.appendChild(el('span', 'sess-id', s.id.slice(0, 8)));
  btn.appendChild(row);

  const sub = el('div', 'sess-sub');
  sub.appendChild(el('span', 'sess-state ' + stateClass(s), stateLabel(s)));
  // The waiting duration is the number to optimise down (§2).
  if (isWaiting(s) && s.waiting_ms != null) {
    sub.appendChild(el('span', null, duration(s.waiting_ms)));
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
  return btn;
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
  bar.textContent = `${waiting.length} waiting · longest ${duration(longest.waiting_ms ?? 0)}`;
  bar.onclick = () => select(longest.id);
}

function currentSession() {
  return snap.sessions.find((s) => s.id === selected) || null;
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

  $('repo').textContent = w ? w.path.split('/').slice(-2).join('/') : '—';
  $('ctxdot').className = 'dot ' + (s ? dotClass(s) : 'idle');
  $('ctxname').textContent = s ? (s.title || wsId) : (wsId || 'no session');
  $('ctxbranch').textContent = w ? (w.branches[0] || '') : '';
  $('ctxstate').textContent = s ? stateLabel(s) : '';
  $('killbtn').style.display = s && s.alive ? '' : 'none';

  if (wsId) {
    get(`/api/merge-base?workspace=${encodeURIComponent(wsId)}`)
      .then((b) => {
        $('upstream').textContent = b.upstream;
        $('ctxbase').textContent = `merge-base ${b.merge_base.slice(0, 7)}`;
      })
      .catch(() => { $('ctxbase').textContent = ''; });
  }
}

// ---------------------------------------------------------------------------
// Drawer — available on every workspace, not just main (§9)
// ---------------------------------------------------------------------------

function renderDrawer() {
  const wsId = currentWorkspaceId();
  const w = snap.workspaces.find((x) => x.id === wsId);
  const tabs = $('dtabs');
  tabs.replaceChildren();
  $('dcwd').textContent = w ? w.path : '';

  const procs = w ? w.processes : [];
  const drawer = $('drawer');

  // On a worktree the drawer starts empty and is a thin bar until you open
  // something.
  drawer.className = procs.length ? 'drawer' : 'drawer empty';

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

  showTerm(active ? `proc:${active}` : null, $('drawerbody'));

  // Auto-expand when a managed process goes red.
  const failing = procs.find((p) => p.health.health === 'failing');
  if (failing && selectedProc[wsId] !== failing.id && !drawerTouched) {
    selectedProc[wsId] = failing.id;
    showTerm(`proc:${failing.id}`, $('drawerbody'));
  }
}

let drawerTouched = false;

// ---------------------------------------------------------------------------
// Files — changed files for the selected session's workspace (§9)
// ---------------------------------------------------------------------------

function renderFiles() {
  const wsId = currentWorkspaceId();
  const w = snap.workspaces.find((x) => x.id === wsId);
  const panes = $('filepanes');
  panes.replaceChildren();

  if (!w) {
    panes.appendChild(el('div', 'fempty', 'No workspace selected.'));
    $('filesfoot').textContent = '';
    return;
  }

  const groups = [
    ['Staged', w.files.staged],
    ['Unstaged', w.files.unstaged],
    ['Untracked', w.files.untracked],
  ];
  let total = 0;
  for (const [label, files] of groups) {
    if (!files.length) continue;
    total += files.length;
    panes.appendChild(el('div', 'fgroup')).appendChild(
      el('span', 'eyebrow', `${label} · ${files.length}`));
    for (const f of files) {
      const row = el('div', 'frow');
      const letter = f.status === 'untracked' ? 'U'
        : (f.code.replace(/\./g, '')[0] || 'M');
      row.appendChild(el('span', 'fst ' + letter, letter));
      const n = el('span', 'fname');
      // RTL truncation keeps the filename visible and elides the directory.
      n.textContent = '‪' + f.path;
      n.title = f.path;
      row.appendChild(n);
      panes.appendChild(row);
    }
  }

  if (!total) {
    panes.appendChild(el('div', 'fempty', 'Clean tree.'));
  }
  $('filesfoot').textContent = w.is_main
    ? `${total} changed · worktrees excluded`
    : `${total} changed`;
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
  if (s) showTerm(`session:${s.id}`, $('termwrap'));
  render();
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

async function newWorktree() {
  const name = prompt('Worktree name');
  if (!name) return;
  try {
    const r = await call('/api/worktree', { name: name.trim() });
    pendingSelect = r.session;
    toast(`creating worktree ${name}`);
  } catch (e) {
    toast(e.message, true);
  }
}

async function newShell() {
  const wsId = currentWorkspaceId();
  if (!wsId) return;
  drawerTouched = true;
  try {
    const r = await call(`/api/workspace/${encodeURIComponent(wsId)}/shell`);
    selectedProc[wsId] = r.process;
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

$('addshell').onclick = newShell;
$('shellbtn').onclick = newShell;
$('refreshbtn').onclick = () => {
  const wsId = currentWorkspaceId();
  if (wsId) call(`/api/workspace/${encodeURIComponent(wsId)}/reconcile`).catch((e) => toast(e.message, true));
};
$('killbtn').onclick = () => {
  const s = currentSession();
  if (s) call(`/api/session/${s.id}/kill`).catch((e) => toast(e.message, true));
};

// ---------------------------------------------------------------------------
// Keyboard (§9)
// ---------------------------------------------------------------------------

window.addEventListener('keydown', (e) => {
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

window.addEventListener('resize', () => {
  for (const e of terms.values()) resize(e);
});

// ---------------------------------------------------------------------------
// Live state
// ---------------------------------------------------------------------------

function connect() {
  const sock = new WebSocket(`${WS_BASE}/ws/events?token=${encodeURIComponent(TOKEN)}`);
  sock.onmessage = (ev) => {
    snap = JSON.parse(ev.data);
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
    if (selected && !snap.sessions.some((s) => s.id === selected)) selected = null;

    // Switch to a session we asked for as soon as the daemon reports it.
    if (pendingSelect && snap.sessions.some((s) => s.id === pendingSelect)) {
      const id = pendingSelect;
      pendingSelect = null;
      select(id);
      return;
    }

    if (!selected) {
      // Default to whatever most needs you.
      const first = snap.sessions.find(isWaiting)
        || snap.sessions.find((x) => !isDone(x))
        || snap.sessions[0];
      if (first) {
        selected = first.id;
        showTerm(`session:${first.id}`, $('termwrap'));
      }
    }
    render();
  };
  sock.onclose = () => {
    toast('daemon disconnected — retrying', true);
    setTimeout(connect, 1500);
  };
}

connect();
// The waiting clock has to tick even when nothing else changes.
setInterval(() => { renderRail(); }, 1000);

window.orchTeardown = teardown;
