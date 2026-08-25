// The review overlay: read a PR's threads, decide each one, then one batch of
// outward writes. The largest single feature in the SPA.

import { $, call, el, get, newShell, pending, snap, toast, setPendingSelect } from './core.js';
import * as Diff from './diff.js';
import { patchStats, hunkEl, fileListLabel, willWriteLabel } from './review-diff.js';


/* Replaces typing `/resolve <pr>` into a terminal pane. The agent reads every
   thread and proposes; you go through them and decide. Nothing is written until
   the final action.

   `design/review-overlay.html` is the spec for anything visual here.

   Local state is NOT derived from the snapshot. `render()` redraws from a full
   Snapshot on every websocket tick, and `Diff.state`/`Diff.edit` survive only
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
     where a newline belongs. `Diff.openEditor()` settled this. */
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
      /** @type {HTMLButtonElement} */ (send).disabled = !m.threads.every((x) => (manualState.comments[x.thread_id] || '').trim());
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
  if (Diff.state.open) Diff.close();
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
      if (r?.session) setPendingSelect(r.session);
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
    if (r.session) setPendingSelect(r.session);
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
  // A run that ended says so first: without it, threads left `pending` read as
  // waiting their turn behind a session that is not there any more.
  if (run.ended) {
    foot.appendChild(el('div', 'note',
      `${run.ended}. ${left ? `${left} thread(s) never got an answer — the rows above are where it stopped.`
        : 'Every thread was accounted for.'}`));
  } else {
    foot.appendChild(el('div', 'note', left
      ? `${left} still moving. The buttons below are yours whenever you want them; `
        + 'nothing here fires on its own.'
      : 'Nothing is moving. What is on the branch is what the session finished.'));
  }
  // The commits are the run's whole output and nothing pushes them for you, so
  // this is said plainly rather than left to the push button to imply.
  if (run.unpushed) {
    foot.appendChild(el('div', 'note warn',
      `${run.unpushed} commit${run.unpushed === 1 ? '' : 's'} on this branch that the remote does not have. `
      + 'Replies are already out; the change they describe is not.'));
  }
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

// The public surface. Everything else above is private by construction now,
// which is the point: the rail reaches the overlay through these four or not
// at all.

export { reviewState as state, openReview as open, closeReview as close, reviewKey as key };
