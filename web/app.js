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

/** A hand-triggered skill run against a PR. A refusal from the guard table is
 *  shown verbatim: it is the whole point of triggering by hand. */
function skillButton(p, skill) {
  const b = el('button', 'pract', `/${skill}`);
  b.title = skill === 'green'
    ? 'Start a headless /green run on this PR'
    : 'Start a session on this PR and run /resolve';
  b.onclick = async (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    b.disabled = true;
    try {
      const r = await call(`/api/pr/${p.number}/${skill}`);
      pendingSelect = r.session;
      toast(`/${skill} ${p.number}`);
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
      if (needsResolve) row.appendChild(skillButton(p, 'resolve'));
      if (needsGreen) row.appendChild(skillButton(p, 'green'));
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
// Review queue (§6b)
// ---------------------------------------------------------------------------

let showReviews = true;
let showBlockedReviews = false;

function reviewAge(hours) {
  if (hours < 1) return 'just now';
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

  if (!rv || rv.state !== 'ok') {
    // Never an empty queue: a broken command reads as broken (§6b). Startup is
    // not broken, so it says so differently.
    const pending = !rv || rv.state === 'pending';
    count.appendChild(el('span', pending ? null : 'f', pending ? 'polling…' : 'unavailable'));
    head.appendChild(count);
    head.title = rv?.reason || '';
    list.appendChild(el('div', 'fempty', pending
      ? 'waiting for the first poll'
      : `reviews unavailable\n${(rv?.reason || '').slice(0, 160)}`));
    head.onclick = () => { showReviews = !showReviews; renderReviews(); };
    return;
  }

  const rows = rv.actionable || [];
  const blocked = rv.blocked || [];
  const oldest = rows.reduce((a, r) => Math.max(a, r.age_hours || 0), 0);
  count.appendChild(el('span', rows.length ? 'n' : null,
    rows.length ? `${rows.length} waiting · oldest ${reviewAge(oldest)}` : 'clear'));
  head.appendChild(count);
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
    a.appendChild(el('span', 'num', `#${r.number}`));
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

connect();
// The waiting clock has to tick even when nothing else changes.
setInterval(() => { renderRail(); }, 1000);

window.orchTeardown = teardown;
