// The rail: what is running, what is waiting on you, and the PRs beside it.
// Twenty-four names, three out; the rest is how a row decides what it says.

import { $, byNewest, call, dotClass, duration, el, isArchived, isConversation, isWaiting, newSession, newWorktree, openMenu, pending, refreshButton, selected, sessionsOf, setSelected, sinceSnap, snap, stateClass, stateLabel, toast } from './core.js';
import * as Review from './review.js';
import * as Term from './term.js';

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
    ['resolve in ui [beta]', null, () => Review.open(p.number)],
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
    // How long since a poll actually landed. Live-ticked off the snapshot clock
    // like the rail's other ages, so a poller that is stuck without erroring
    // reads as stale rather than current. Hidden while a fetch is in flight.
    if (snap.pr_age_ms != null && !snap.pr_polling) {
      count.appendChild(el('span', 'prage', ` · ${duration(sinceSnap(snap.pr_age_ms))} ago`));
    }
  }
  head.appendChild(count);
  // The read token's source, only when it is the `gh auth token` fallback —
  // which carries write scopes orchd does not want (see TODO). Env/file are fine
  // and say nothing.
  if (snap.token_source === 'gh_cli') {
    const w = el('span', 'toksrc', '⚠');
    w.title = 'GitHub token is from `gh auth token` — broader (write) scopes than orchd needs. '
      + 'Set ORCHD_GITHUB_TOKEN or github_token_file to a read-only PAT.';
    head.appendChild(w);
  }
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
      b.onclick = (ev) => { ev.preventDefault(); ev.stopPropagation(); setSelected(auto.session); };
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
  const forked = forkBadge(s);
  if (forked) row.appendChild(forked);
  row.appendChild(el('span', 'sess-id', duration(sinceSnap(s.created_ms)) + ' ago'));
  btn.appendChild(row);

  if (!s.resumable) {
    // The transcript is readable, the conversation cannot be continued (§2).
    btn.appendChild(el('div', 'sess-sub', 'transcript only'));
  }
  btn.onclick = () => openArchived(s);
  btn.oncontextmenu = (ev) => openMenu(ev, [
    // Not gated on `resumable` the way opening it is: a fork cuts its own
    // worktree, so a conversation whose branch is gone can still be branched off.
    ['Fork session', null, s.has_transcript ? () => forkSession(s) : null],
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
    Term.close(`session:${r.session}`);
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

  const row = el('div', 'sess-row');
  row.appendChild(el('span', 'dot ' + dotClass(s)));
  row.appendChild(el('span', 'sess-name' + (pending(s) ? ' pending' : ''), railName(s, w)));
  const forked = forkBadge(s);
  if (forked) row.appendChild(forked);
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
  btn.onclick = () => setSelected(s.id);
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
    bar.appendChild(el('span', null,
      `${waiting.length} waiting · longest ${duration(sinceSnap(longest.waiting_ms) ?? 0)}`));
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

export { renderRail as render, railName as rowName, closeSession };
