// The rail: what is running, what is waiting on you, and the PRs beside it.
// Twenty-four names, three out; the rest is how a row decides what it says.

import { $, byNewest, call, caret, dotClass, duration, el, isArchived, isConversation, isWaiting, mainWorkspace, MOD_LABEL, newSession, newWorktree, openMenu, pending, refreshButton, selected, sessionsOf, setSelected, sinceSnap, snap, stateClass, stateLabel, toast, setPendingSelect } from './core.js';
import * as Review from './review.js';
import * as Term from './term.js';

/* Expanded per group and kept across renders. Main's two conversations and the
 * worktrees' twenty are not the same question. */
const showArchived = { main: false, worktrees: false };

/* The session whose name is being edited in place, or null. A snapshot lands
 * every second and rebuilds the rail, which would blow the input away mid-type —
 * so the rebuild is held off while it is open, the same way `tabDrag` holds off
 * `renderDrawer`. `renameSession` sets it and clears it. */
let editingName = null;

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
 */
function clock(cls, ms, suffix = '', prefix = '') {
  // An absent base renders empty and is left un-marked. `Number('')` is 0, so a
  // null written into the dataset would come back as a clock counting up from the
  // epoch of nothing — a "0s" that grows where there had been no text at all.
  if (ms == null) return el('span', cls, '');
  const span = el('span', cls, prefix + duration(sinceSnap(ms)) + suffix);
  span.dataset.clock = String(ms);
  if (suffix) span.dataset.clockSuffix = suffix;
  if (prefix) span.dataset.clockPrefix = prefix;
  return span;
}

/** Advance every duration the rail is showing. The timer calls this, not render. */
function tick() {
  for (const node of document.querySelectorAll('[data-clock]')) {
    const el_ = /** @type {HTMLElement} */ (node);
    const base = Number(el_.dataset.clock);
    if (!Number.isFinite(base)) continue;
    el_.textContent = (el_.dataset.clockPrefix || '')
      + duration(sinceSnap(base))
      + (el_.dataset.clockSuffix || '');
  }
}

function renderRail() {
  if (editingName !== null) return;
  const rail = $('rail');
  rail.replaceChildren();

  const main = mainWorkspace();
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
  // Red first, above everything. A PR that is failing or conflicting is failing
  // whoever happens to be sitting in it, and the teal "a session holds this" used
  // to hide exactly that: you opened a session on a red PR and the row went calm.
  if (p.checks === 'failing' || p.mergeable === 'CONFLICTING') return 'build';
  if (p.session) return 'auto';           // a session is holding it
  if (p.is_draft) return 'idle';
  if (p.needs_you) return 'blocked';
  if (p.checks === 'passing') return 'ok';
  return 'idle';
}

let showPrs = true;

/** The session a pointer just picked, so the `click` behind it does not pick it
 *  again. See `sessionRow`. */
let picked = null;

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

/* Menu copy, one convention for all of them: a lowercase verb phrase, and the
   object only when it is *not* the row you right-clicked — the row already says
   which session, so "fork session" says it twice, while "swap branch with main"
   names something else and earns it. No trailing ellipsis on the ones that open a
   prompt or a picker: every item here leads somewhere, so marking three of them
   is noise rather than a distinction. */

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
    ['resolve in UI [beta]', null, () => Review.open(p.number)],
  ];
}

/** Start a plain session on a PR: a worktree pinned to its head branch, or the
 *  main checkout moved onto it. */
async function openPr(number, where) {
  try {
    const r = await call(`/api/pr/${number}/open`, { where });
    setPendingSelect(r.session);
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
    setPendingSelect(r.session);
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
      setPendingSelect(r.session);
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
  head.appendChild(caret());
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
    // How long since a poll actually landed. Live-ticked off the snapshot clock
    // like the rail's other ages, so a poller that is stuck without erroring
    // reads as stale rather than current. Hidden while a fetch is in flight.
    if (snap.pr_age_ms != null && !snap.pr_polling) {
      count.appendChild(clock('prage', snap.pr_age_ms, ' ago', ' · '));
    }
  }
  head.appendChild(count);
  /* No badge for the token's source. It used to carry a `⚠` when the token came
     from `gh auth token`, whose scopes are wider than §6 wants — but that is the
     fallback which makes the app work at all, so the mark was permanent, could not
     be acted on without setting up a PAT, and sat next to the PR count as if
     something were wrong. `token_source` is still in the snapshot for anyone
     diagnosing over the API; it is just not a thing to look at every day. */
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
    row.appendChild(el('span', 'ttl', p.title, p.title));

    const auto = (snap.automation || {})[p.number];
    const needsResolve = p.needs_you;
    const needsFix = p.checks === 'failing' || p.mergeable === 'CONFLICTING';

    // A reason chip next to a button just repeats it and steals width from the
    // title, which is the part you actually read.
    if (!needsResolve && !needsFix) {
      const why = [];
      if (p.unresolved_capped) why.push('50+ threads');
      if (p.children && p.children.length) why.push(`${p.children.length} stacked`);
      if (p.is_draft) why.push('draft');
      if (why.length) row.appendChild(el('span', 'link', why[0]));
    }

    // Both buttons are hand-triggered, and no poll result ever presses one: a PR
    // going red is not a reason for anything to start, which is what keeps the
    // guard table a gate you read rather than one that trips behind you. A run can
    // also arrive here already going, from a review handing on the CI it is not
    // allowed to fix — still something a person set off, by sending the decisions.
    if (auto && auto.state === 'running') {
      const b = el('span', 'pract running', 'fixing');
      b.title = 'Jump to the run';
      b.onclick = (ev) => { ev.preventDefault(); ev.stopPropagation(); setSelected(auto.session); };
      row.appendChild(b);
    } else {
      if (auto && auto.state === 'exhausted') {
        // The skill stopped without turning it green: it wants you.
        row.appendChild(el('span', 'why gaveup', 'gave up'));
      }
      if (needsResolve) row.appendChild(reviewButtons(p));
      /* No `fix` while a session holds the branch, because the daemon refuses that
         run: `spawn_fix_pr_session` bails with "already has a live session for
         #<n>" the moment `branch_busy` answers, and `PrView.session` is that same
         live session. A button whose only outcome is an error toast is worse than
         no button — the `jump` chip beside it is the thing to press. Review keeps
         its buttons: `/resolve` takes you *to* that session rather than refusing. */
      if (needsFix && !p.session) row.appendChild(actionButton(p, 'fix-pr', 'fix'));
    }

    // The row opens the PR; jumping to its session is the explicit chip, so
    // one does not swallow the other.
    if (p.session) {
      const j = el('button', 'jump', 'jump');
      j.title = 'Go to the session on this branch';
      j.onclick = (ev) => {
        ev.preventDefault();
        ev.stopPropagation();
        setSelected(p.session);
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
 *  occupied the button is disabled and the row that holds it says so (§2).
 *
 *  Unless `allow_several_in_main` is set, which is a config-file decision the
 *  daemon reports in the snapshot. Then `+` stays live and the holder's name is
 *  still worth saying, because a second session in one checkout is a thing to do
 *  on purpose rather than by accident. */
function mainGroup(w) {
  const group = el('div', 'ws');
  const sessions = sessionsOf(w.id);
  const active = sessions.filter((s) => !isArchived(s));
  const occupant = active.find((s) => s.id === w.occupant && s.alive);
  const several = !!snap.several_in_main;

  const add = el('button', 'plus', '+');
  add.disabled = !!occupant && !several;
  /* The chord belongs in the tooltip of the button that does the same thing:
     finding the button once is how you stop needing it, which is the argument the
     legend button already makes for itself. `MOD_LABEL` rather than a literal —
     the modifier is ⌘ on macOS and Ctrl everywhere else. */
  add.title = occupant
    ? `main is held by ${occupant.title || occupant.id.slice(0, 8)}${several ? ' · another is allowed' : ''}`
    : `New session in main · ${MOD_LABEL} Shift N`;
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
  add.title = `New worktree session · ${MOD_LABEL} N (shift-click to name it)`;
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
  toggle.appendChild(caret());
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
  // So a rename can find this row's name span again after any re-render.
  btn.dataset.id = s.id;

  const row = el('div', 'sess-row');
  row.appendChild(el('span', 'dot archived'));
  const arcName = railName(s, { id: s.workspace });
  row.appendChild(el('span', 'sess-name', arcName, arcName));
  const forked = forkBadge(s);
  if (forked) row.appendChild(forked);
  row.appendChild(clock('sess-id', s.created_ms, ' ago'));
  btn.appendChild(row);

  if (!s.resumable) {
    // The transcript is readable, the conversation cannot be continued (§2).
    btn.appendChild(el('div', 'sess-sub', 'transcript only'));
  } else if (!snap.workspaces.some((w) => w.id === s.workspace)) {
    /* Its worktree is gone, which the snapshot says by omission: only teardown
       drops a workspace record, and the retention timer is what usually calls it.
       Worth a line, because "archived" alone would leave you to discover on the
       next resume that the directory is not there — and the point of the setting
       is that the row still works, so the row should say so. Main is never
       missing, so this can only read on a worktree. */
    btn.appendChild(el('div', 'sess-sub', 'tree removed · rebuilds on resume'));
  }
  btn.onclick = () => openArchived(s);
  btn.oncontextmenu = (ev) => openMenu(ev, [
    // Worth more here than on a live row: the archive is the list you scan weeks
    // later, and two conversations Claude Code named the same thing are what you
    // are scanning past.
    ['rename', null, () => renameSession(s)],
    // Not gated on `resumable` the way opening it is: a fork cuts its own
    // worktree, so a conversation whose branch is gone can still be branched off.
    ['fork', null, s.has_transcript ? () => forkSession(s) : null],
    ['delete', 'bad', () => deleteSession(s)],
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
    Term.close(`session:${r.session}`);
    setPendingSelect(r.session);
    // The branch moved since the conversation happened, so the files it talks
    // about are not the files on disk. Worth saying, not worth refusing over.
    if (r.warning) toast(r.warning, true);
  } catch (e) {
    toast(e.message, true);
  }
}

/** Two lines: dot + name, then state and duration. No dirty-file count — that
 *  lives in the right column, one click away (§9). */

/** What the row calls itself.
 *
 *  In order of how much it tells you: the PR an automation run is working on, the
 *  name Claude Code gave the conversation, then the workspace it sits in. The
 *  workspace is last because it is the coarsest — a worktree holds one session at a
 *  time, so its name says where, not which conversation.
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

/** Marks a conversation that was cut from another one.
 *
 *  A fork keeps its parent's title, so two rows read identically and the only
 *  thing telling them apart is an eight-character id. This says which is the
 *  copy, and the title says what it is a copy of. */
function forkBadge(s) {
  return s.forked_from ? el('span', 'forked', 'forked') : null;
}

function sessionRow(s, w) {
  const btn = el('button', 'sess' + (s.kind.kind === 'automation' ? ' auto' : ''));
  btn.setAttribute('aria-current', String(s.id === selected));
  // So a rename can find this row's name span again after any re-render.
  btn.dataset.id = s.id;

  const row = el('div', 'sess-row');
  row.appendChild(el('span', 'dot ' + dotClass(s)));
  const liveName = railName(s, w);
  row.appendChild(el('span', 'sess-name' + (pending(s) ? ' pending' : ''), liveName, liveName));
  const forked = forkBadge(s);
  if (forked) row.appendChild(forked);
  // The session's age, not its id. A hex slice told worktree-sharing rows apart
  // but was unreadable — a value you never recognise — and age is worth reading on
  // every row and moves as the session does. Same token the archive rows show.
  row.appendChild(clock('sess-id', s.created_ms, ' ago'));
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
    sub.appendChild(clock(null, s.waiting_ms));
  } else if (s.state.state === 'starting') {
    sub.appendChild(clock(null, s.created_ms));
  }
  btn.appendChild(sub);

  // An agent editing outside its worktree is a prompt problem worth seeing,
  // not noise to swallow (§11).
  if (s.boundary_violations.length) {
    btn.appendChild(el('span', 'sess-warn',
      `${s.boundary_violations.length} blocked edit(s) outside the worktree`));
  }

  btn.appendChild(el('div', 'sess-pad'));
  /* Picked on pointerdown, not on click.
   *
   * A `click` is only dispatched when the press and the release land on the
   * *same* element, and this row is rebuilt whenever a snapshot arrives — which
   * is what selecting a session causes. Move between sessions at a normal pace
   * and about one pick in fifteen simply never happens: the render fell inside
   * the few tens of milliseconds the click was being made in, the row the mouse
   * went down on was gone by the time it came up, and nothing fired. No error,
   * no toast, just a row that did not take.
   *
   * Pointerdown cannot be caught out that way — it fires on the row that is
   * under the pointer at the time, before any of this can be replaced.
   *
   * The click handler stays because a keyboard activates a `<button>` without a
   * pointer ever going down. `picked` is module-level rather than per node, so
   * it survives the row being rebuilt between the two events; without that the
   * pair would select twice and the second one would re-announce the selection,
   * stealing focus back into a terminal you had just left. */
  btn.onpointerdown = (ev) => {
    if (ev.button !== 0) return;      // the secondary button opens the menu
    picked = s.id;
    setSelected(s.id);
  };
  btn.onclick = () => {
    if (picked === s.id) { picked = null; return; }
    setSelected(s.id);
  };
  /* Which way the branch moves, as one item with three answers. Asked of the
     snapshot's own main row rather than of `w`, which is a `{ id }` stub on a
     worktree row — `w.is_main` is `undefined` there, and relying on that being
     falsy would make this right by accident.

       on main                        → move out of main, into a tree of its own
       on a worktree, main is free    → move to main
       on a worktree, main holds work → swap branch with main

     Never two of them. A session is in main or it is not, so the other could only
     ever be dead, and a greyed "move out of main" on a worktree row reads as the
     app thinking that row is in main — the opposite of what the rail says two
     lines above it. */
  const mainWs = mainWorkspace();
  const inMain = s.workspace === mainWs?.id;
  const moveLabel = inMain
    ? 'move out of main'
    : mainHoldsWork(mainWs) ? 'swap branch with main' : 'move to main';
  // A worktree Claude Code has not named yet has no path to swap.
  const moveDo = inMain
    ? () => moveOutOfMain(s)
    : pending(s) ? null : () => swapWithMain(s.workspace);
  // The header's ✕ only ever closes the selected session, so closing any other
  // one meant switching to it first.
  btn.oncontextmenu = (ev) => openMenu(ev, [
    ['rename', null, () => renameSession(s)],
    // Nothing to branch off until the conversation has had a turn.
    ['fork', null, s.has_transcript ? () => forkSession(s) : null],
    // Claude Code's own picker, reached rather than rebuilt. Greyed in the states
    // where the daemon refuses it — mid-turn an escape interrupts the turn, and at
    // a question or a permission prompt it answers instead of rewinding.
    ['rewind', null, isRewindable(s) ? () => rewindSession(s) : null],
    // The worktree, not the session: the row is the only place a worktree is
    // visible, so its workspace-level action lives here too.
    [moveLabel, null, moveDo],
    ['close', 'bad', s.alive ? () => closeSession(s.id) : null],
    ['delete', 'bad', () => deleteSession(s)],
  ]);
  return btn;
}

/** Sessions whose prompt would take a double-escape as "rewind".
 *
 *  The picker opens at the prompt and nowhere else, so this is narrower than
 *  "waiting": mid-turn the escape interrupts the turn, and the two waiting states
 *  that expect an answer would take it as one — cancelling a question, declining a
 *  permission prompt. The daemon refuses the same three, so this only decides
 *  whether the item is offered, never whether it is safe. */
const isRewindable = (s) =>
  s.alive
  && s.state.state === 'your_turn'
  && s.state.reason !== 'asked_a_question'
  && s.state.reason !== 'needs_permission'
  // Nothing to rewind to: the picker would open with nothing in it.
  && s.has_transcript;

/** Open Claude Code's rewind picker in this session's terminal.
 *
 *  Selects first, because the picker draws in the pane and pressing this on a row
 *  you cannot see would put a modal somewhere out of sight. */
async function rewindSession(s) {
  setSelected(s.id);
  try {
    await call(`/api/session/${s.id}/rewind`);
    toast('opened the rewind picker — pick a point in the pane');
  } catch (e) {
    toast(e.message, true);
  }
}

/** Whether main is holding work of its own, or just sitting on the base branch.
 *
 *  What the swap is *called* turns on this: with a branch of its own on each side
 *  the two trade places, and with only base in main your branch goes there and
 *  base comes back — which is a move, and saying "swap" for it describes an
 *  exchange nobody asked for. A *session* in main counts as work too: swapping
 *  then displaces that conversation into this worktree, which is an exchange
 *  however empty main's branch is.
 *
 *  The base branch comes off `upstream_ref` (`upstream/develop` → `develop`),
 *  which is the same split the daemon makes. `origin/HEAD` cannot be split that
 *  way — the daemon resolves it against the remote and the SPA cannot — so an
 *  unresolvable base answers "yes, it holds something", keeping the wording that
 *  is right either way.
 *
 *  Read off the whole branch *set*, not `branches[0]`: that set is built from a
 *  `HashSet`, so its order says nothing, and "the only thing main has is base" is
 *  a question about the set rather than about its first element. */
function mainHoldsWork(main) {
  /* A conversation in main is work, whatever branch main is on. Without this the
     item read the git side only: main sitting on its base with somebody working in
     it answered "nothing of its own", so the menu offered `move to main` and the
     confirm promised "main has nothing of its own checked out" — while a swap
     would have carried that person's conversation out into this worktree. Reported
     from a Mac: `move to main` on a row while a session was in main.

     The same rule the daemon uses for "is anyone in main" since it learned to
     allow more than one: any live session whose workspace is main, rather than the
     recorded claim, which can name none of them. */
  const busy = snap.sessions.some(
    (x) => x.workspace === main?.id && x.alive && !isArchived(x));
  if (busy) return true;
  const leaf = (snap.upstream_ref || '').split('/').pop();
  if (!leaf || leaf === 'HEAD') return true;
  // What main has checked out *now*. This used to ask `branches`, which accumulates
  // every branch a tree has ever held and is never pruned — so one visit from any
  // other branch made main look occupied for the rest of the daemon's life, and the
  // row went on offering "swap branch with main" over a main sitting on its base.
  // Unknown before the first reconcile, and unknown is the cautious answer: a swap
  // refuses when there is nothing to exchange, a move would move onto a branch
  // somebody else holds.
  const on = main?.branch;
  return !on || on !== leaf;
}

/** Move a session out of main, into a worktree of its own.
 *
 *  The swap's missing direction: a swap needs a second branch to exchange, and
 *  this has none — you started something in main, it turned into real work, and
 *  main should be free again. The branch gets a tree named after it, uncommitted
 *  changes travel with it, main goes back to base, and the conversation follows
 *  keeping its id and its place in the rail.
 *
 *  Confirmed for the same reason the swap is: every file under main changes, and
 *  the daemon's refusals are about what it can see, not about whether you meant
 *  it. */
async function moveOutOfMain(s) {
  if (!confirm(
    'Move this session out of main?\n\n'
    + 'Its branch gets a worktree of its own and main goes back to its base branch \u2014 '
    + 'or, if main is already on base, the work gets a branch cut for it and main stays put. '
    + 'Uncommitted changes travel; untracked files stay in main. '
    + 'The conversation moves too, keeping its history.'
  )) return;
  try {
    const r = await call(`/api/session/${s.id}/out-of-main`);
    // A relocated session keeps its id, so the dead terminal is still in `terms`
    // under the key the new pty wants — the same reason the swap and resume close it.
    if (r.session && r.session.session) {
      Term.close(`session:${r.session.session}`);
      setPendingSelect(r.session.session);
    }
    toast(r.created
      ? `cut ${r.branch} in ${r.workspace}; main is still on ${r.main}`
      : `${r.branch} is in ${r.workspace}; main is on ${r.main}`);
    // The branch moved even if the conversation could not follow, so these are
    // second lines rather than errors over the top of a success.
    if (r.wip_error) toast(`the branch moved, but ${r.wip_error}`, true);
    if (r.session && r.session.error) toast(`the branch moved, but ${r.session.error}`, true);
    else if (r.session && r.session.degraded) {
      toast('the conversation would not resume there, so it was forked instead', true);
    }
  } catch (e) {
    toast(e.message, true);
  }
}

/** Put this worktree's branch in main, and main's here.
 *
 *  Main is where the managed processes and the dev stack live, so work that needs
 *  them has to be *in* main. Both directories stay put — only what each has
 *  checked out is exchanged — and the conversations follow their branches in both
 *  directions, keeping their ids.
 *
 *  Confirmed rather than immediate: every file under two trees changes, and the
 *  daemon's refusals (mid-turn agent, dirty tree, stopped rebase) are about what
 *  it can see, not about whether you meant it. */
/* A swap takes seconds — two checkouts change every file, and the conversations
   that follow the branches are killed and resumed — and until it lands the rail
   still shows the world as it was. That silence is what gets it pressed twice, and
   the second press is not a no-op: it swaps straight back. The daemon refuses a
   concurrent one outright; this says so without the round trip, and says the first
   is running rather than leaving you to guess. */
let swapInFlight = false;

async function swapWithMain(wsId) {
  if (swapInFlight) return toast('a swap is already running — watch the rail', true);
  const holds = mainHoldsWork(mainWorkspace());
  if (!confirm(holds
    ? `Swap branches between main and ${wsId}?\n\n`
      + `main takes this worktree's branch, and this worktree takes main's. `
      + `Uncommitted changes travel with their branch. Each conversation follows `
      + `its branch — this one moves into main, and main's moves here — keeping its `
      + `history and its place in the rail.`
    : `Move this worktree's branch to main?\n\n`
      + `main has nothing of its own checked out, so its base branch comes back `
      + `here in exchange. Uncommitted changes travel with the branch, and this `
      + `conversation follows it into main, keeping its history and its place in `
      + `the rail.`
  )) return;
  swapInFlight = true;
  toast(`swapping ${wsId} with main…`);
  try {
    const r = await call(`/api/workspace/${encodeURIComponent(wsId)}/swap-main`);
    // A relocated session keeps its id, so the dead terminal is still in `terms`
    // under the key the new pty wants and `openTerm` would hand back the corpse —
    // the same reason resume closes it. Both directions, since both were respawned.
    for (const dir of [r.into_main, r.into_worktree]) {
      if (dir && dir.session) Term.close(`session:${dir.session}`);
    }
    // Land in main, where the branch now is — the whole point of pressing this.
    if (r.select) setPendingSelect(r.select);
    toast(`main is on ${r.main}; ${wsId} is on ${r.worktree}`);
    // The branches moved even if a conversation could not follow, so these are
    // second lines rather than errors over the top of a success.
    for (const [dir, where] of [[r.into_main, 'into main'], [r.into_worktree, `into ${wsId}`]]) {
      if (!dir) continue;
      if (dir.error) toast(`the branches swapped, but ${dir.error}`, true);
      // A fork, not the move that was promised: the id changed, so there is a new
      // row rather than the one you were looking at.
      else if (dir.degraded) toast(`the conversation ${where} would not resume, so it was forked instead`, true);
    }
    // The other partial success: the branches exchanged but the banked work would
    // not re-apply. A second line for the same reason the relocation errors are —
    // the swap happened, and the message says where the work still is.
    if (r.wip_error) toast(`the branches swapped, but ${r.wip_error}`, true);
    // Untracked files cannot be carried, so say which stayed rather than leaving
    // you to notice that half the work did not travel.
    if (r.untracked_left && r.untracked_left.length) {
      toast(
        `left in ${wsId} (untracked, so not carried): ${r.untracked_left.slice(0, 4).join(', ')}`
        + (r.untracked_left.length > 4 ? ` and ${r.untracked_left.length - 4} more` : ''),
        true,
      );
    }
  } catch (e) {
    toast(e.message, true);
  } finally {
    swapInFlight = false;
  }
}

/** Branch off a conversation: same context, new worktree, original untouched.
 *
 *  The new session appears under worktrees rather than next to its parent, which
 *  is the point — the two are no longer editing the same files.
 *
 *  No `closeTerm` unlike resume, which keeps the old id and would otherwise hand
 *  back the dead terminal. A fork has an id of its own and nothing to collide
 *  with. */
async function forkSession(s) {
  try {
    const r = await call(`/api/session/${s.id}/fork`);
    setPendingSelect(r.session);
    toast('forked');
    // The branch moved on since the conversation, same as resume: worth saying,
    // not worth refusing over.
    if (r.warning) toast(r.warning, true);
  } catch (e) {
    toast(e.message, true);
  }
}

/** Name a session yourself, editing the rail row in place.
 *
 *  The input holds the name you gave it and shows the ai-title as a placeholder,
 *  so you can see what the row falls back to while you retype — the one thing the
 *  old `prompt()` box could not do. `Enter` commits, `Esc` cancels, and leaving
 *  the box commits too. Blank hands the row back to the ai-title.
 *
 *  The row is a `<button>`, so every event the input handles is stopped from
 *  bubbling: a click must not select the session, and a keystroke must not reach
 *  the app's keyboard map. */
function renameSession(s) {
  const rail = $('rail');
  const span = rail.querySelector(`[data-id="${s.id}"] .sess-name`);
  // The menu action can outlive the row it was opened on; if a re-render dropped
  // it, there is nothing to edit in place.
  if (!span) return;

  editingName = s.id;
  const input = document.createElement('input');
  input.className = 'sess-rename';
  input.value = s.name || '';
  input.placeholder = s.title || '';
  span.replaceWith(input);
  input.focus();
  input.select();

  let done = false;
  const finish = async (commit) => {
    if (done) return;             // blur fires alongside Enter; settle once.
    done = true;
    const given = input.value.trim();
    editingName = null;           // let the rail rebuild again before the await.
    if (commit && given !== (s.name || '')) {
      try {
        await call(`/api/session/${s.id}/rename`, { name: given });
      } catch (e) {
        toast(e.message, true);
      }
    }
    renderRail();                 // put the row back, whichever way it ended.
  };

  input.onkeydown = (e) => {
    if (e.key === 'Enter') { e.preventDefault(); finish(true); }
    else if (e.key === 'Escape') { e.preventDefault(); finish(false); }
    e.stopPropagation();
  };
  input.onblur = () => finish(true);
  input.onclick = (e) => e.stopPropagation();
  input.onpointerdown = (e) => e.stopPropagation();
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
/** Sessions a nudge would reach: parked at a prompt, and not mid-question.
 *
 *  Wider than `isWaiting`, on purpose. A session that has only just resumed is
 *  `ready`, which `wants_attention` excludes because an idle agent is not
 *  something to shout about — but it is exactly the one you want to send on. */
const isNudgeable = (s) =>
  // `ready` alone: resumed mid-conversation and not prompted since. A finished
  // turn is not paused mid-work, it is done, and telling it to continue would
  // invent the next thing for you.
  s.alive && s.state.state === 'your_turn' && s.state.reason === 'ready'
  // A session with no conversation behind it has nothing to continue, and would
  // read the word as its opening instruction. The daemon skips those too, so the
  // count here is what pressing the button actually does.
  && s.has_transcript
  // And it has to have been cut off mid-turn. `ready` alone cannot tell that
  // from a conversation that had finished before the restart — they come back
  // at the same empty prompt — so the bar was calling finished work "paused".
  && s.interrupted;

function renderWaitbar() {
  const waiting = snap.sessions.filter(isWaiting);
  const ready = snap.sessions.filter(isNudgeable);
  const bar = $('waitbar');
  if (!waiting.length && ready.length < 2) {
    bar.className = 'waitbar';
    return;
  }
  bar.replaceChildren();

  if (waiting.length) {
    const longest = waiting.reduce(
      (a, b) => ((a.waiting_ms ?? 0) >= (b.waiting_ms ?? 0) ? a : b));
    bar.className = 'waitbar on';
    /* "need you", not "waiting". The count is `wants_attention` — any `your_turn`
       but `ready`, plus a red build — so it covers a finished turn as well as a
       permission prompt, and the rows name those separately. "Waiting" promised
       somebody was blocked, and reading "2 waiting" over a rail with one obviously
       blocked row is the bar arguing with the list under it.

       Two nodes, because only the second half moves: the count changes with a
       snapshot, the duration changes every second. */
    bar.appendChild(el('span', null, `${waiting.length} need you · longest `));
    bar.appendChild(clock(null, longest.waiting_ms ?? 0));
    bar.title = `Jump to the one that has needed you longest · ${MOD_LABEL} Space`;
    bar.onclick = () => setSelected(longest.id);
  } else {
    /* Nobody is asking for you; a restart has just put several agents back at an
       empty prompt. Quieter than the waiting bar, because this is an offer rather
       than a queue: the whole point of `ready` not counting as attention. */
    bar.className = 'waitbar on calm';
    bar.appendChild(el('span', null,
      `${ready.length} session${ready.length === 1 ? '' : 's'} paused mid-work`));
    bar.onclick = () => setSelected(ready[0].id);
  }

  /* One poke for the lot. Typing the same word into each of them is the tax on
     auto-resume being worth having. */
  if (ready.length > 1) {
    const all = el('button', 'waitall', 'continue');
    all.title = 'Type "continue" into every session paused mid-work';
    all.onclick = (ev) => { ev.stopPropagation(); nudgeAll(); };
    bar.appendChild(all);
  }
}

/** Send them all on. */
async function nudgeAll() {
  try {
    const r = await call('/api/sessions/nudge');
    const n = (r.nudged || []).length;
    toast(n ? `nudged ${n}` : 'nothing to nudge');
    // Named, not silently skipped: a permission prompt or a question takes a
    // keystroke as its answer, so typing into one would be answering for you.
    const held = r.held || [];
    if (held.length) {
      toast(`${held.join(', ')} ${held.length === 1 ? 'is' : 'are'} waiting on an answer from you`, true);
    }
  } catch (e) {
    toast(e.message, true);
  }
}

export { renderRail as render, tick, railName as rowName, closeSession };
