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
      if (s.state.reason === 'ready') return 'ready';
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
  renderReviews();
  renderUpdate();
}

// The poll counter each pane captured when its refresh was pressed; the button
// spins until the live counter moves past it. null = not spinning.
const spinFloor = { pr: null, review: null };

/**
 * A ↻ that forces a poll and spins until the poll it triggered lands.
 * `pollCount` is the pane's monotonic poll counter from the snapshot; `endpoint`
 * is the POST that pulses that poller. Used by both the PR and review panes.
 */
function refreshButton(kind, pollCount, endpoint) {
  const btn = el('span', 'rvrefresh', '↻');
  btn.title = 'Refresh now';
  btn.setAttribute('role', 'button');
  if (spinFloor[kind] != null && pollCount > spinFloor[kind]) spinFloor[kind] = null;
  if (spinFloor[kind] != null) btn.classList.add('spin');
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
  add.title = 'New worktree session (shift-click to name it)';
  add.onclick = (ev) => newWorktree(ev.shiftKey);
  head.appendChild(add);
  wtGroup.appendChild(head);

  if (!worktrees.length) {
    wtGroup.appendChild(el('div', 'railbtn', 'none yet'));
  }
  for (const w of worktrees) {
    if (w.id === '\u2026creating') continue;
    if (!sessionsOf(w.id).length) {
      wtGroup.appendChild(emptyWorkspaceRow(w));
      continue;
    }
    appendSessions(wtGroup, w);
  }
  rail.appendChild(wtGroup);
  // Its own pane below the scroller, so it stays put while sessions scroll.
  $('prpane').replaceChildren(prGroup());

  renderWaitbar();
}


/** Dot colour for a PR, sharing the session legend so one key covers both (§9). */
function prDot(p) {
  if (p.session) return 'auto';           // a session is holding it
  if (p.is_draft) return 'idle';
  if (p.unresolved > 0 || p.changes_requested) return 'blocked';
  if (p.checks === 'failing' || p.mergeable === 'CONFLICTING') return 'build';
  if (p.checks === 'passing') return 'ok';
  return 'idle';
}

let showPrs = true;

/** Threads want answering. Opens the review overlay rather than spawning a
 *  session — which is the whole point of the overlay, and it also fixes the bug
 *  underneath: the old button typed `/resolve <pr>` into a pty, and that slash
 *  command resolves from the agent's own command path, which is not installed
 *  here. */
function reviewButton(p) {
  const b = el('button', 'pract', 'review');
  b.title = `Read and answer #${p.number}'s review threads`;
  b.onclick = (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    openReview(p.number);
  };
  return b;
}

/** A hand-triggered run against a PR. `action` is the endpoint name, which is
 *  also what the button shows. A refusal from the guard table is shown verbatim:
 *  it is the whole point of triggering by hand. */
function actionButton(p, action) {
  const b = el('button', 'pract', `/${action}`);
  b.title = 'Start a headless /green run on this PR';
  b.onclick = async (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    b.disabled = true;
    try {
      const r = await call(`/api/pr/${p.number}/${action}`);
      pendingSelect = r.session;
      toast(`/${action} ${p.number}`);
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
    const needs = prs.filter((p) => p.unresolved > 0 || p.changes_requested).length;
    const failing = prs.filter(
      (p) => p.checks === 'failing' || p.mergeable === 'CONFLICTING').length;
    const bits = [`${prs.length}`];
    if (needs) bits.push(`${needs} needs you`);
    if (failing) bits.push(`${failing} failing`);
    count.appendChild(el('b', null, bits.join(' · ')));
    if (needs) count.querySelector('b').classList.add('n');
  }
  head.appendChild(count);
  head.appendChild(refreshButton('pr', snap.pr_poll ?? 0, '/api/prs/refresh'));
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
    // ⌘-click, middle-click and copy-link all behave, and the browser already
    // holds the GitHub session.
    row.target = '_blank';
    row.rel = 'noreferrer';
    row.appendChild(el('span', 'dot ' + prDot(p)));
    row.appendChild(el('span', 'num', `#${p.number}`));
    row.appendChild(el('span', 'ttl', p.title));

    const auto0 = (snap.automation || {})[p.number];
    const needsResolve0 = p.unresolved > 0 || p.unresolved_capped || p.changes_requested;
    const needsGreen0 = p.checks === 'failing' || p.mergeable === 'CONFLICTING';

    // A reason chip next to a button just repeats it and steals width from the
    // title, which is the part you actually read.
    if (!needsResolve0 && !needsGreen0) {
      const why = [];
      if (p.unresolved_capped) why.push('50+ threads');
      if (p.children && p.children.length) why.push(`${p.children.length} stacked`);
      if (p.is_draft) why.push('draft');
      if (why.length) row.appendChild(el('span', 'link', why[0]));
    }

    // Both skills are hand-triggered. /green is deliberately not automatic:
    // the guard table is a gate you read, not one that trips behind you.
    const auto = auto0, needsResolve = needsResolve0, needsGreen = needsGreen0;

    if (auto && auto.state === 'running') {
      const b = el('span', 'pract running', '/green running');
      b.title = 'Jump to the run';
      b.onclick = (ev) => { ev.preventDefault(); ev.stopPropagation(); select(auto.session); };
      row.appendChild(b);
    } else {
      if (auto && auto.state === 'exhausted') {
        // The skill stopped without turning it green: it wants you.
        row.appendChild(el('span', 'why gaveup', 'gave up'));
      }
      if (needsResolve) row.appendChild(reviewButton(p));
      if (needsGreen) row.appendChild(actionButton(p, 'green'));
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

  // PRs are opened against upstream while branches live on the fork (§6), so
  // the header names both rather than collapsing them into one path.
  const repos = snap.repos || {};
  $('repoupstream').textContent = repos.upstream
    || (w ? w.path.split('/').slice(-2).join('/') : '—');
  $('repofork').textContent = repos.fork || '';
  $('ctxdot').className = 'dot ' + (s ? dotClass(s) : 'idle');
  $('ctxname').textContent = s ? (s.title || wsId) : (wsId || 'no session');
  $('ctxbranch').textContent = w ? (w.branches[0] || '') : '';
  const pr = wsId ? prForWorkspace(wsId) : null;
  const bits = [];
  if (s) bits.push(stateLabel(s));
  if (pr) {
    const state = pr.unresolved ? `${pr.unresolved} unresolved`
      : pr.mergeable === 'CONFLICTING' ? 'conflicted'
        : pr.checks === 'failing' ? 'checks failing'
          : pr.checks === 'pending' ? 'checks running'
            : pr.is_draft ? 'draft' : 'clean';
    bits.push(`#${pr.number} ${state}`);
  }
  $('ctxstate').textContent = bits.join(' · ');
  $('killbtn').style.display = s && s.alive ? '' : 'none';

  if (wsId) {
    get(`/api/merge-base?workspace=${encodeURIComponent(wsId)}`)
      .then((b) => {
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

  // Docker stack status, in place of the path: blue = up, red = down.
  const dcwd = $('dcwd');
  dcwd.replaceChildren();
  const up = snap.stack_up === true;
  dcwd.appendChild(el('span', 'stackdot ' + (up ? 'up' : 'down')));
  dcwd.appendChild(el('span', null, up ? 'stack up' : 'stack down'));

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

/** Behind/ahead against upstream/develop, with the one action worth offering.
 *
 *  The changed-file list is a poor summary of a branch that has simply fallen
 *  behind: what you want then is to take develop in, not to read a list. */
function renderDivergence(w) {
  const box = $('diverge');
  box.replaceChildren();
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
  const wsId = currentWorkspaceId();
  const w = snap.workspaces.find((x) => x.id === wsId);
  renderDivergence(w);
  const panes = $('filepanes');
  panes.replaceChildren();

  $('basebtn').textContent = BASES.find(([k]) => k === diffState.base)?.[1] ?? '';
  $('filestitle').textContent = diffState.open ? 'Changeset' : 'Changed files';

  if (!w) {
    panes.appendChild(el('div', 'fempty', 'No workspace selected.'));
    $('filesfoot').textContent = '';
    return;
  }

  // With the diff open the pane becomes the whole-changeset file list, which is
  // a different set from `git status`: it includes committed work (§5).
  if (diffState.open) {
    const sum = diffState.summary;
    if (!sum) {
      panes.appendChild(el('div', 'fempty', 'No diff available.'));
      $('filesfoot').textContent = '';
      return;
    }
    for (const f of sum.files) {
      const row = el('button', 'dfrow');
      row.setAttribute('aria-current', String(f.path === diffState.path));
      const letter = f.status[0] || 'M';
      row.appendChild(el('span', 'fst ' + letter, letter));
      const n = el('span', 'fname');
      n.textContent = '‪' + f.path;
      n.title = f.path;
      row.appendChild(n);
      const nums = el('span', 'dfnum');
      if (f.binary) {
        nums.textContent = 'bin';
      } else {
        nums.appendChild(el('span', 'p', `+${f.added}`));
        nums.appendChild(document.createTextNode(' '));
        nums.appendChild(el('span', 'm', `−${f.deleted}`));
      }
      row.appendChild(nums);
      row.onclick = () => { diffState.cursor = 0; diffState.context = 3; loadFile(f.path); };
      panes.appendChild(row);
    }
    if (!sum.files.length) panes.appendChild(el('div', 'fempty', 'Nothing changed against this base.'));
    $('filesfoot').textContent =
      `${sum.files.length} files · +${sum.added} −${sum.deleted} · base ${sum.base.slice(0, 7)}`;
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
      const row = el('button', 'frow');
      const letter = f.status === 'untracked' ? 'U'
        : (f.code.replace(/\./g, '')[0] || 'M');
      row.appendChild(el('span', 'fst ' + letter, letter));
      const n = el('span', 'fname');
      n.textContent = '‪' + f.path;
      n.title = f.path;
      row.appendChild(n);
      // Clicking a changed file is the fastest way into the diff.
      row.onclick = () => openDiff(f.path);
      panes.appendChild(row);
    }
  }

  if (!total) panes.appendChild(el('div', 'fempty', 'Clean tree.'));
  $('filesfoot').textContent = w.is_main
    ? `${total} changed · worktrees excluded`
    : `${total} changed`;
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
const BASES = [
  ['upstream', 'vs develop'],
  ['head', 'vs HEAD'],
  ['pr_base', 'vs PR base'],
];

const diffState = {
  open: false,
  base: 'upstream',
  summary: null,     // { base, files, added, deleted }
  path: null,
  file: null,        // { path, hunks, binary }
  split: true,
  cursor: 0,         // index into the current file's change blocks
  context: 3,
};

/* Byte offsets come from Rust; JS strings are UTF-16. Decode through the byte
   array rather than assuming ASCII, or a line with an accent in it highlights
   the wrong span. */
const ENC = new TextEncoder();
const DEC = new TextDecoder();
function lineEl(row, side) {
  // side: 'old' | 'new'. In split view each pane shows only its own side.
  const empty = !row || (side === 'old' && row.kind === 'add') ||
                        (side === 'new' && row.kind === 'del');
  const div = el('div', 'ln' + (empty ? ' empty' : row.kind === 'add' ? ' add' : row.kind === 'del' ? ' del' : ''));
  const num = el('i', null, empty ? '' : String((side === 'old' ? row.old : row.new) ?? ''));
  div.appendChild(num);
  const body = el('s');
  if (!empty) {
    if (row.words && row.words.length) {
      // Ranges arrive ordered and non-overlapping, so one pass covers them all.
      const bytes = ENC.encode(row.text);
      const cls = row.kind === 'add' ? 'w-add' : 'w-del';
      let at = 0;
      for (const [ws, we] of row.words) {
        if (ws > at) body.appendChild(document.createTextNode(DEC.decode(bytes.slice(at, ws))));
        body.appendChild(el('span', cls, DEC.decode(bytes.slice(ws, we))));
        at = we;
      }
      if (at < bytes.length) body.appendChild(document.createTextNode(DEC.decode(bytes.slice(at))));
    } else {
      body.textContent = row.text || ' ';
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
  diffState.cursor = Math.min(diffState.cursor, Math.max(anchors.length - 1, 0));
  markCursor();
}

function markCursor() {
  for (const e of $('diffbody').querySelectorAll('.ln.cur')) e.classList.remove('cur');
  const a = (diffState.anchors || [])[diffState.cursor];
  if (!a) return;
  a.classList.add('cur');
  a.scrollIntoView({ block: 'center', behavior: 'smooth' });
  $('ovcount').textContent = `change ${diffState.cursor + 1} of ${diffState.anchors.length}`;
}

function stepChange(delta) {
  const n = (diffState.anchors || []).length;
  if (!n) return;
  diffState.cursor = (diffState.cursor + delta + n) % n;
  markCursor();
}

async function loadSummary() {
  const ws = currentWorkspaceId();
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
  const ws = currentWorkspaceId();
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
  diffState.file = null;
  diffState.path = null;
  $('overlay').classList.remove('on');
  renderFiles();
}

function cycleBase() {
  const i = BASES.findIndex(([k]) => k === diffState.base);
  diffState.base = BASES[(i + 1) % BASES.length][0];
  diffState.context = 3;
  renderFiles();
  if (diffState.open) openDiff(diffState.path);
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
  const ws = currentWorkspaceId();
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
  screen: 'intake',    // intake | gate | overview | card | final | report
  i: 0,                // index into queue()
  picks: {},           // thread_id -> position index
  skipped: {},         // thread_id -> true
  /* Keyed per (thread, option), not per thread: looking at a second option must
     not be a punishment for having started typing. */
  drafts: {},
  report: null,
  busy: false,
};

const draftKey = (id, pos) => `${id} ${pos}`;

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
const offered = (pos) => pos.does !== 'story+reply' || !!reviewState.data.tracker;

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
  const row = el('div', 'willwrite');
  row.appendChild(document.createTextNode(verb || 'will write'));
  for (const f of patchStats(diff)) {
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
    if (/^(diff --git|index |new file|deleted file|old mode|new mode|similarity|rename |Binary )/.test(line)) continue;
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
 *  works inside the branch's own history. `/green` is offered only on the
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
    const b = el('button', 'head-btn', '/green');
    b.title = 'Start a headless /green run. It cannot run while a review is open.';
    b.onclick = () => rvAct(() => call(`/api/pr/${reviewState.pr}/green`), 'started /green', true);
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
    mid.appendChild(el('div', 'eyebrow', '/green is running on this branch'));
    mid.appendChild(el('div', 'big', 'Resolve opens when /green finishes.'));
    mid.appendChild(el('p', null,
      'Both rewrite this same worktree, so the two are mutually exclusive — two concurrent ' +
      'rebases in one working directory is index corruption, not a UI glitch. Starting ' +
      '/green during a review is refused from the other side too.'));
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
      (x) => positionOf(x).does === 'change+thumbsup' || positionOf(x).does === 'thumbsup'],
    ['Wants a decision', 'the recommendation comes with words you should read first',
      (x) => positionOf(x).does === 'change+reply' || positionOf(x).does === 'reply'],
    ['Out of scope', 'fair, but it belongs in a story rather than this PR',
      (x) => positionOf(x).does === 'story+reply'],
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

  // -- the positions
  const posSec = el('div', 'sec');
  posSec.appendChild(el('div', 'eyebrow', 'how you are handling it'));
  posSec.appendChild(rvPositions(item));
  body.appendChild(posSec);
  root.appendChild(body);

  const hint = `thread ${reviewState.i + 1} of ${q.length} · ` +
    (q.some(isHandled) ? stagedCount() : 'nothing written yet');
  root.appendChild(rvActs([
    actBtn('accept · enter', 'warm', () => acceptCard()),
    actBtn('skip', null, () => skipCard()),
    reviewState.i > 0 ? actBtn('back', null, () => moveCard(-1)) : null,
  ], hint));
}

/** The option list. The reply and the proposed change live *inside* the
 *  position, not beside it: picking a stance, writing the words and choosing the
 *  edit are one act, so they are one control — you can never do A and say B. */
function rvPositions(item) {
  const wrap = el('div', 'pos');
  const chosen = pickOf(item);

  item.p.positions.forEach((pos, i) => {
    // No tracker means nowhere to file, so the option is not shown at all — the
    // overlay is not welded to one particular tracker. The daemon refuses it too
    // if it arrives anyway; this is so it never has to.
    if (!offered(pos)) return;
    const opt = el('div', 'opt' + (i === chosen ? ' on' : ''));
    const head = el('button', 'optHead');
    head.appendChild(el('span', 'k', String(i + 1)));

    const t = el('span', 't', pos.label);
    if (i === item.p.recommend) t.appendChild(el('span', 'rec', 'recommended'));
    // "Is this my wording or the agent's?" is the first question on returning
    // to a card, and the posted footer cannot answer it.
    if (reviewState.drafts[draftKey(item.t.id, i)] !== undefined) {
      t.appendChild(el('span', 'edited', 'edited'));
    }
    if (pos.sub) t.appendChild(el('em', null, pos.sub));
    head.appendChild(t);
    head.appendChild(el('span', 'does', doesLabel(pos.does)));
    head.onclick = () => {
      reviewState.picks[item.t.id] = i;
      delete reviewState.skipped[item.t.id];
      renderReview();
    };
    opt.appendChild(head);

    // Options stay collapsed: comparing them means expanding each in turn,
    // which is the accepted cost of not letting a long diff push the choices
    // off screen.
    if (i === chosen) opt.appendChild(rvOptBody(item, pos, i));
    wrap.appendChild(opt);
  });
  return wrap;
}

/** What accepting a position actually does — so the option that rewrites a file
 *  cannot look as cheap as the one that posts a reaction. No thumbs-up glyph
 *  anywhere: the words. */
function doesLabel(does) {
  return {
    'thumbsup': 'thumbs up',
    'reply': 'reply',
    'change+thumbsup': 'change + thumbs up',
    'change+reply': 'change + reply',
    'story+reply': 'story + reply',
    'manual': 'by hand',
  }[does] || does;
}

function rvOptBody(item, pos, i) {
  const bodyEl = el('div', 'optBody');

  if (pos.patch) {
    bodyEl.appendChild(willWriteLabel(pos.patch));
    bodyEl.appendChild(hunkEl(pos.patch, false));
  }
  if (pos.story) {
    // `willWriteLabel` derives its object from a diff, and a story has none —
    // so the object is named here rather than leaving a verb with nothing after
    // it.
    const says = el('div', 'willwrite');
    says.appendChild(document.createTextNode('will create'));
    says.appendChild(el('b', null, 'a Shortcut story'));
    bodyEl.appendChild(says);
    const draft = el('div', 'storydraft');
    for (const [lbl, val] of [['title', pos.story.title], ['body', pos.story.body]]) {
      const l = el('div', 'sline');
      l.appendChild(el('span', 'lbl', lbl));
      l.appendChild(el('span', 'val', val));
      draft.appendChild(l);
    }
    bodyEl.appendChild(draft);
  }

  if (pos.reply !== null && pos.reply !== undefined) {
    /* A textarea, not a contenteditable: the text goes to GitHub as plain
       markdown, so rich paste is pure liability and browsers insert
       <div>/<br> where a newline belongs. `openEditor()` settled this. */
    const box = el('textarea', 'box');
    box.setAttribute('aria-label', 'Reply');
    box.value = replyOf(item);
    if (pos.does === 'manual') {
      box.placeholder = 'Written in the manual phase, once the work exists.';
    }
    // Recorded on input rather than on blur, or navigating away with the keys
    // would drop what was typed.
    box.oninput = () => {
      reviewState.drafts[draftKey(item.t.id, i)] = box.value;
      // Repaint only the footer: re-rendering the card here would move focus
      // out of the box mid-sentence.
      rvFootState(bodyEl, item, pos, i);
    };
    bodyEl.appendChild(box);

    const foot = el('div', 'foot');
    bodyEl.appendChild(foot);
    rvFootState(bodyEl, item, pos, i);
  }
  return bodyEl;
}

/** The footer under a reply box: what gets appended, and — only once the text
 *  actually differs from the draft — the offer to put it back. */
function rvFootState(bodyEl, item, pos, i) {
  const foot = bodyEl.querySelector('.foot');
  if (!foot) return;
  foot.replaceChildren();

  if (pos.does === 'story+reply') {
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
    else if (pos.does === 'manual') { kind = 'manual'; word = 'by hand'; }
    else if (pos.does === 'story+reply') { kind = 'story'; word = 'story'; }
    else if (pos.does === 'thumbsup') { kind = 'reply'; word = 'thumbs up'; }
    else if (pos.patch) { kind = 'apply'; word = 'apply'; }
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
  const blocked = q.filter((x) => isHandled(x) && positionOf(x).does === 'manual');
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
  root.appendChild(rvActs([
    actBtn(label, 'warm', () => sendBatch(), !bits.length || blocked.length > 0),
    actBtn('back', null, () => { reviewState.screen = 'card'; renderReview(); }),
  ], blocked.length
    ? `${blocked.length} thread${blocked.length === 1 ? '' : 's'} needs a phase that is not built yet`
    : 'ctrl+enter to send · enter does nothing here'));

  if (blocked.length) {
    const warn = el('div', 'banner');
    warn.appendChild(el('span', 'ico', '▲'));
    const tx = el('span', 'tx');
    tx.appendChild(el('b', null, 'The manual phase is not built yet.'));
    tx.appendChild(el('p', null,
      'It has to open after the accepted patches are committed, so you edit a tree that ' +
      'already reflects every other decision. The daemon refuses it by name rather than ' +
      'posting nothing quietly — pick another position on ' +
      blocked.map((x) => threadLabel(x.t)).join(', ') + ', or skip it.'));
    warn.appendChild(tx);
    root.insertBefore(warn, root.querySelector('.strip'));
  }
}

/** One sentence for what a handled thread will do. */
function planLine(item, pos) {
  const reply = replyOf(item).trim();
  const short = reply.length > 90 ? reply.slice(0, 90).trimEnd() + '…' : reply;
  if (pos.does === 'thumbsup') return 'Responds with a thumbs up, no written reply.';
  if (pos.does === 'change+thumbsup') return `${pos.label}. Responds with a thumbs up, no written reply.`;
  if (pos.does === 'story+reply') return `File “${pos.story?.title || 'a story'}”, then reply with its id.`;
  if (pos.does === 'manual') return 'You write the code, then comment in the phase that follows.';
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
    for (const f of patchStats(positionOf(item).patch)) {
      const seen = files.find((x) => x.path === f.path);
      if (seen) { seen.added += f.added; seen.deleted += f.deleted; }
      else files.push({ ...f });
    }
  }
  const replies = handled.filter((x) => replyOf(x).trim() &&
    ['reply', 'change+reply', 'story+reply', 'manual'].includes(positionOf(x).does)).length;
  // Counted apart from the GitHub writes: a story goes to a different system, and
  // it is the one thing in the batch that is not re-derivable from the PR.
  const stories = handled.filter((x) => positionOf(x).does === 'story+reply').length;
  const thumbs = handled.filter((x) =>
    ['thumbsup', 'change+thumbsup'].includes(positionOf(x).does)).length;

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
      sendBatch();
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

/* ---------- the controller ---------- */

function renderReview() {
  const root = $('rvoverlay');
  root.replaceChildren();
  if (!reviewState.open || !reviewState.data) return;

  if (reviewState.report) reviewState.screen = 'report';
  else if (reviewState.data.gate) reviewState.screen = 'gate';
  else if (!reviewState.data.proposals) reviewState.screen = 'intake';

  ({
    intake: rvIntake,
    gate: rvGate,
    overview: rvOverview,
    card: rvCard,
    final: rvFinal,
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
      toast('the branch moved — decisions cleared, re-read the cards');
    }
    reviewState.head = base;
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
 *  `/green` — because the useful next screen is the pty, not this one. */
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
  if (['reply', 'change+reply', 'story+reply'].includes(pos.does) && !replyOf(item).trim()) {
    return toast('this position posts a reply — write one, or pick another', true);
  }
  reviewState.picks[item.t.id] = pickOf(item);
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
async function sendBatch() {
  if (reviewState.busy) return;
  const q = queue();
  const decisions = q.filter(isHandled).map((item) => {
    const i = pickOf(item);
    const typed = reviewState.drafts[draftKey(item.t.id, i)];
    return {
      thread_id: item.t.id,
      position: i,
      reply: typed === undefined ? null : typed,
    };
  });
  if (!decisions.length) return toast('nothing to send — every thread was skipped', true);

  reviewState.busy = true;
  renderReview();
  try {
    reviewState.report = await call(`/api/pr/${reviewState.pr}/post`, {
      base_sha: reviewState.data.proposals.base_sha,
      decisions,
    });
    reviewState.screen = 'report';
  } catch (e) {
    // A rejected request is the daemon refusing before it wrote anything —
    // a bad index, a thread that has gone, a gate that closed under you.
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
  head.appendChild(el('span', 'eyebrow', 'Review queue'));
  const count = el('span', 'rvcount');

  const refresh = refreshButton('review', snap.reviews_poll ?? 0, '/api/reviews/refresh');

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
    // Review mode is the files-changed tab, which is where reviewing happens.
    a.href = `${r.url}/files`;
    a.target = '_blank';
    a.rel = 'noreferrer';
    a.appendChild(el('span', 'dot'));           // always grey here
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
$('basebtn').onclick = cycleBase;
$('reposwitch').onclick = () =>
  toast('switching repositories is not implemented yet', true);
$('addshell').onclick = newShell;
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
  if ((e.metaKey || e.ctrlKey) && e.key === 's' && editState.on) {
    e.preventDefault();
    saveEditor();
    return;
  }
  /* The overlay wants bare Enter, j/k and digits, and this handler is registered
     with capture:true — it runs before any element listener wherever focus is.
     So the focus guard is not optional here the way it was for Escape/F7/Alt+d. */
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
    if (e.key === 'F7') {
      e.preventDefault();
      stepChange(e.shiftKey ? -1 : 1);
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

setupChrome();
connect();
// The waiting clock has to tick even when nothing else changes.
setInterval(() => { renderRail(); }, 1000);

window.orchTeardown = teardown;
