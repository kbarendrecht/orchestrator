// The review overlay: read a PR's threads, decide each one, then one batch of
// outward writes. The largest single feature in the SPA.

import { $, call, el, get, MOD_LABEL, newShell, pending, setSelected, snap, toast, setPendingSelect } from './core.js';
import * as Diff from './diff.js';
import { langFor, hlTokens } from './diff.js';
import { patchStats, hunkEl, fileListLabel } from './review-diff.js';


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
  /* thread_id -> what the session should do, for a free-text answer. Keyed per
     thread rather than per option: it describes the work, not a wording. Goes out
     as `note`, and is never posted to the thread. */
  notes: {},
  /* thread_id -> true while its reply is open for editing on the overview. The
     overview shows a line by default; the box appears only when you ask, so the
     list is not a wall of textareas. */
  editing: {},
  report: null,
  busy: false,
  /* The single-session flow. `session` is the review session's id once started;
     while it is set, the overlay is driven by that session's ask (read from the
     snapshot) rather than the daemon batch. Null means the old triage+batch path,
     which is left exactly as it was. */
  session: null,
  proposalsLoaded: false,   // fetched /review once, when the decision ask appeared
  decisionsSent: false,     // answered the decision ask; the change phase is running
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

  const big = el('div', 'big', 'Not read for ');
  const sha = el('span', 'm', (d.head_sha || '').slice(0, 7));
  sha.style.fontSize = '13px';
  big.appendChild(sha);
  big.appendChild(document.createTextNode(' yet.'));
  mid.appendChild(big);

  mid.appendChild(el('p', null,
    'One session reads the code at each thread and works out how it could be answered. ' +
    'The read changes nothing — not a file, not a commit, not a comment. You decide thread ' +
    'by thread, then it makes the changes you picked and posts.'));

  const row = el('div');
  row.style.cssText = 'display:flex;gap:8px;margin-top:4px';
  // A session this window cannot drive is still a session doing the work: it is
  // past the decisions and this SPA does not hold them, so the only honest offer
  // is its pane. Starting a second one here would abandon it mid-ask, and a fresh
  // spawn drops the proposals the first is acting on.
  const busy = liveReviewSession(reviewState.pr);
  if (busy) {
    mid.appendChild(el('p', null,
      'A session is already answering this PR, further along than this window can pick up. '
      + 'Watch it in its pane; it posts nothing without asking.'));
    row.appendChild(headBtn('go to its pane', 'go', () => { closeReview(); setSelected(busy.id); }));
  } else {
    // The single-session flow: one session reads, then makes the changes you pick and
    // posts, staying open the whole time. The overlay does not close and hand you a
    // pane — it stays put and advances itself when the session has read the threads.
    row.appendChild(headBtn('read the threads', 'go', () => startReviewSession()));
  }
  if (d.url) {
    const gh = headBtn('open on github', null, () => window.open(d.url, '_blank', 'noreferrer'));
    row.appendChild(gh);
  }
  mid.appendChild(row);

  if (n === 0) {
    mid.appendChild(el('p', null,
      'Nothing is awaiting an answer right now, so the session would have nothing to read.'));
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
    ['Straightforward', 'they are right — one keystroke each',
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
  const p = el('p', null, 'The read changed nothing. Nothing is written or posted until you send your picks.');
  p.style.cssText = 'color:var(--dim);font-size:12px';
  note.appendChild(p);
  body.appendChild(note);
  root.appendChild(body);

  const handled = q.filter((x) => isHandled(x) || reviewState.skipped[x.t.id]).length;
  root.appendChild(rvActs([
    actBtn(handled ? `back to thread ${reviewState.i + 1} of ${q.length}` : `start · thread 1 of ${q.length}`,
      'pri', () => { reviewState.screen = 'card'; renderReview(); }),
    handled === q.length && q.length
      ? actBtn('review & send', null, () => { reviewState.screen = 'final'; renderReview(); })
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
  bar.appendChild(el('span', null, who.join(', ') + ' — arrived after the session read the threads'));
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
  // The thread's path is the only thing that can name a language here: a GitHub
  // diff hunk carries no `diff --git` header to read one from.
  if (hunk) top.appendChild(hunkEl(hunk, true, t.path));

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
    for (const part of commentParts(c.body, t.path)) cmt.appendChild(part);
    chain.appendChild(cmt);
  }
  top.appendChild(chain);
  body.appendChild(top);

  // -- the agent's read, one line with the rest behind a disclosure
  body.appendChild(rvRead(p));

  // -- the ways to answer this thread, one flat list; the selected option's reply
  //    is edited in the box below it, prefilled from what the read already drafted.
  body.appendChild(rvOptions(item));
  const reply = rvCardReply(item);
  if (reply) body.appendChild(reply);
  root.appendChild(body);

  const hint = `thread ${reviewState.i + 1} of ${q.length} · ` +
    (q.some(isHandled) ? stagedCount() : 'nothing chosen yet');
  root.appendChild(rvActs([
    actBtn('accept · ⏎', 'warm', () => acceptCard()),
    // Lit when this thread is skipped: with no Skip row in the list, the button is
    // the only thing that can say the card was answered by passing it over.
    actBtn('skip · s', reviewState.skipped[t.id] ? 'on' : null, () => skipCard()),
    reviewState.i > 0 ? actBtn('back', null, () => moveCard(-1)) : null,
  ], hint));
}

/** A comment body, split into prose and GitHub `suggestion` blocks.
 *
 *  A suggestion is a fenced block whose info string is `suggestion`, and its
 *  content is the reviewer's proposed replacement for the lines the thread is
 *  anchored to. Rendered as code rather than left as prose because the fence
 *  markers and the body were showing verbatim — the most common review comment
 *  there is, reading as the one thing on the card that had not been formatted.
 *
 *  **Display only.** Nothing here applies it, and that is deliberate: this
 *  codebase's stated position is that a suggestion is a claim to be verified, not
 *  an instruction to be executed (`commands/triage.md`). Showing it clearly is
 *  what lets you judge that; the Apply button is GitHub's, not ours.
 *
 *  Anything unterminated is left as prose, so a comment merely *discussing* a
 *  fence does not swallow the rest of itself. */
function commentParts(body, path) {
  const text = body || '';
  const out = [];
  // ```suggestion … ``` — the fence may carry trailing spaces, and GitHub allows
  // a longer run of backticks, which is why the closer is matched loosely.
  const re = /^[ \t]*```+[ \t]*suggestion[ \t]*\r?\n([\s\S]*?)^[ \t]*```+[ \t]*$/gm;
  let at = 0;
  let m;
  while ((m = re.exec(text)) !== null) {
    const before = text.slice(at, m.index).trim();
    if (before) out.push(el('p', null, before));
    out.push(suggestionEl(m[1].replace(/\r?\n$/, ''), path));
    at = re.lastIndex;
  }
  const rest = text.slice(at).trim();
  // The whole body when there was no suggestion at all, which is the usual case.
  if (rest || !out.length) out.push(el('p', null, rest));
  return out;
}

/** A reviewer's suggested replacement, as code they proposed rather than a diff:
 *  GitHub gives the replacement text only, so the lines it *removes* are not in
 *  the comment — showing them as additions against nothing would be inventing a
 *  diff. Labelled instead, and syntax-coloured from the thread's own path. */
function suggestionEl(code, path) {
  const box = el('div', 'suggestion');
  box.appendChild(el('div', 'sghead', 'suggested change'));
  const body = el('div', 'sgbody');
  const lang = langFor(path);
  for (const line of code.split('\n')) {
    const row = el('div', 'sgline');
    // A blank line still needs something in it, or the row collapses.
    row.appendChild(hlLine(line || ' ', lang));
    body.appendChild(row);
  }
  box.appendChild(body);
  return box;
}

/** One line of code, syntax-coloured with the viewer's palette. The same
 *  flattening `review-diff`'s rows use; kept here rather than exported from there
 *  because that module is about *diff* text and this is not a diff. */
function hlLine(text, lang) {
  const s = el('span', 'sgcode');
  const ranges = lang ? hlTokens(text, lang) : [];
  if (!ranges.length) { s.textContent = text; return s; }
  let at = 0;
  for (const r of ranges) {
    if (r.s > at) s.appendChild(document.createTextNode(text.slice(at, r.s)));
    s.appendChild(el('span', 'tok-' + r.cls, text.slice(r.s, r.e)));
    at = r.e;
  }
  if (at < text.length) s.appendChild(document.createTextNode(text.slice(at)));
  return s;
}

/** The read, in full.
 *
 *  It was collapsed to its opening sentence for a while, to answer the "wall of
 *  text" the first drive complained about. Shown whole again: the fold cost a
 *  click on every card to see the one thing the agent actually concluded, which is
 *  worse than the length it was hiding. The prompt keeps the real fix — it tells
 *  the agent the read must be terse. */
function rvRead(p) {
  const sec = el('div', 'sec');
  const read = el('div', 'read');
  read.appendChild(el('div', 'eyebrow', 'the read'));
  read.appendChild(el('p', null, (p.read || '').trim()));
  sec.appendChild(read);
  return sec;
}

/** A short preview of the reply a reply/story option would post, drafted or as
 *  edited on the overview. Empty when there is nothing written yet. */
function replyPreview(item, i) {
  const pos = item.p.positions[i];
  const r = (reviewState.drafts[draftKey(item.t.id, i)] ?? pos.reply ?? '').trim();
  if (!r) return '';
  return r.length > 120 ? r.slice(0, 120).trimEnd() + '…' : r;
}

/** The flat list of ways to answer this thread: one row per offered position, in
 *  the order triage handed them (it leads with `agree` where the reviewer is
 *  simply right), then Skip. No stance segment, no alts sub-row, no inline editor —
 *  picking a row stages it, and the words are edited on the overview. */
function rvOptions(item) {
  const sec = el('div', 'sec');
  const list = el('div', 'opts');
  const chosen = reviewState.skipped[item.t.id] ? -1 : pickOf(item);

  item.p.positions.forEach((pos, i) => {
    if (!offered(pos)) return;
    const b = el('button', 'opt' + (i === chosen ? ' on' : ''));
    const head = el('div', 'ohead');
    if (pos.stance === 'agree') head.appendChild(el('span', 'tag agree', '👍'));
    else if (pos.stance === 'story') head.appendChild(el('span', 'tag story', 'story'));
    head.appendChild(el('span', 'olabel', pos.label));
    if (i === item.p.recommend) head.appendChild(el('span', 'tag rec', 'recommended'));
    b.appendChild(head);
    // The descriptor, not the reply: the reply lives in the box under the list now.
    const sub = pos.stance === 'agree' ? 'Apply, thumbs up' : (pos.sub || 'your own words');
    b.appendChild(el('div', 'osub', sub));
    b.onclick = () => {
      reviewState.picks[item.t.id] = i;
      delete reviewState.skipped[item.t.id];
      renderReview();
    };
    list.appendChild(b);
  });

  /* Skip is deliberately *not* a row here. It is a way past the card rather than
     a way of answering it, and as a peer of the real answers it read as one. It
     lives on the action bar, where the other ways out of a card are. */
  sec.appendChild(list);
  return sec;
}

/** The daemon's appended free-text option ("Something else"): a reply stance with
 *  no drafted words. Identified by shape, not label, so a rename cannot break it. */
const isFreeText = (pos) => pos.stance === 'reply' && !((pos.reply || '').trim());

/** The reply box, shared by the card and the overview's edit toggle. Prefilled
 *  from the draft the read already produced — instant, nothing waits on the agent —
 *  and written back on input with no re-render, so typing stays smooth and the
 *  cursor never jumps. A textarea, not contenteditable: the text goes to GitHub as
 *  plain markdown, so rich paste is liability and browsers insert <div>/<br> where
 *  a newline belongs. `Diff.openEditor()` settled this. */
function replyBox(item) {
  const i = pickOf(item);
  const pos = item.p.positions[i];
  const wrap = el('div', 'replyedit');
  const box = el('textarea', 'box');
  box.setAttribute('aria-label', `Reply for ${threadLabel(item.t)}`);
  box.value = replyOf(item);
  if (isFreeText(pos)) box.placeholder = 'What the reviewer will read on the thread.';
  box.oninput = () => {
    // Straight to the draft, no re-render: the words already exist, so a repaint
    // would buy nothing and cost the cursor its place mid-sentence.
    reviewState.drafts[draftKey(item.t.id, i)] = box.value;
    rvFootState(wrap, item, pos, i);   // footer only — never the whole card
  };
  wrap.appendChild(box);
  wrap.appendChild(el('div', 'foot'));
  rvFootState(wrap, item, pos, i);
  return wrap;
}

/** What the session should *do* — the other half of a free-text answer, and a
 *  different thing from the reply: this one is never posted. It rides the wire as
 *  `note`, which `commands/review-session.md` already reads as "the human's own
 *  instruction; follow it". Keyed per thread, not per option, because it describes
 *  the thread's work rather than one wording of it. */
function instructionBox(item) {
  const wrap = el('div', 'replyedit');
  const box = el('textarea', 'box');
  box.setAttribute('aria-label', `Instructions for ${threadLabel(item.t)}`);
  box.value = reviewState.notes[item.t.id] ?? '';
  box.placeholder = 'What the session should do';
  // No re-render, same as the reply box: this text has no footer to repaint.
  box.oninput = () => { reviewState.notes[item.t.id] = box.value; };
  wrap.appendChild(box);
  return wrap;
}

/** The boxes a thread's answer needs, shared by the card and the overview's edit
 *  toggle so both surfaces offer exactly the same thing. A free-text answer gets
 *  two: what to do, and what to say. They are separate because they go to
 *  different readers — the instruction to the agent, the reply to the reviewer —
 *  and one box for both meant the reviewer read your instructions. */
function answerBoxes(item) {
  const wrap = el('div', 'answerboxes');
  if (isFreeText(positionOf(item))) {
    wrap.appendChild(el('div', 'boxlab', 'instructions for the session'));
    wrap.appendChild(instructionBox(item));
    wrap.appendChild(el('div', 'boxlab', 'reply to the reviewer'));
  }
  wrap.appendChild(replyBox(item));
  return wrap;
}

/** The selected option's answer, edited on the card. Absent for agree/skip, which
 *  post no words. Shares `reviewState.drafts`/`notes` with the overview's edit
 *  box, so text typed on either surface shows on the other. */
function rvCardReply(item) {
  if (reviewState.skipped[item.t.id]) return null;
  const pos = positionOf(item);
  if (!['reply', 'story'].includes(pos.stance)) return null;

  const sec = el('div', 'sec');
  sec.appendChild(el('div', 'eyebrow', isFreeText(pos) ? 'your answer' : 'reply · edit freely'));
  if (pos.stance === 'story') {
    sec.appendChild(el('div', 'storynote',
      `Files “${pos.story?.title || 'a story'}”, then replies with its id.`));
  }
  sec.appendChild(answerBoxes(item));
  return sec;
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
    said.appendChild(document.createTextNode(
      ' becomes a link to the story once it exists · (via orchestrator) is appended'));
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

/* ---------- screen 5: the overview, where the replies are edited and sent ---------- */

/** Whether a thread's pick posts words the human has to write. `agree` posts a
 *  thumbs up and no words, `skip` posts nothing; both need no reply. */
const needsWords = (item) =>
  isHandled(item) && ['reply', 'story'].includes(positionOf(item).stance);

/** The overview: every thread's answer in one list, with the drafted replies
 *  listed and editable here rather than one card at a time. Nothing has left the
 *  machine yet — the session applies the picks and posts only on your go. */
function rvFinal(root) {
  const q = queue();
  root.appendChild(rvHead('review & send', stagedCount()));
  root.appendChild(rvStrip(null));

  const body = el('div', 'body');
  const out = outward(q);

  /* One list, not three. The threads, the commit and the re-requests used to sit in
     separate sections with a summary panel under them, so the same batch was
     described twice and read four times. Everything that will be done is now one
     sequence of rows under one heading, with the tally above it. */
  body.appendChild(rvWillDo(out));

  const plan = el('div', 'sec plan');
  for (const item of q) plan.appendChild(rvOverviewRow(item));
  // The two things the batch does that belong to no single thread.
  const commit = rvCommitRow(out);
  if (commit) plan.appendChild(commit);
  for (const row of rvRerequestRows(q)) plan.appendChild(row);
  body.appendChild(plan);
  root.appendChild(body);

  const decided = q.every((x) => isHandled(x) || reviewState.skipped[x.t.id]);
  root.appendChild(rvActs([
    // A blank reply is not disabled here — that would need a live repaint on every
    // keystroke, which drops focus out of the box. `submitDecisions` refuses it.
    actBtn('send to the session', 'warm', () => submitDecisions(), !decided),
    actBtn('back', null, () => { reviewState.screen = 'card'; renderReview(); }),
  ], 'this is the last word: the session applies your picks, pushes, and posts these replies'));
}

/** One thread's row on the overview: what it will do, and its reply as a line with
 *  an `edit` toggle — not a textarea by default, which read as clutter. `agree`/
 *  `skip` are static; a reply/story shows the drafted words and opens a box on ask. */
function rvOverviewRow(item) {
  const pos = positionOf(item);
  const skipped = reviewState.skipped[item.t.id];
  const row = el('div', 'stage-row');

  /* Every act the thread causes, not just its headline one: a story also posts the
     reply carrying its id, and agreeing both changes code and reacts. One badge
     each hid half of what pressing send would do. */
  let badges;
  if (skipped || !isHandled(item)) {
    badges = [['skip', skipped ? 'skipped' : 'not handled']];
    row.classList.add('off');   // not answered, so not at full volume
  } else if (pos.stance === 'story') {
    badges = [['story', 'story'], ['reply', 'reply']];
  } else if (pos.stance === 'agree') {
    // `apply`, not `thumbs up`: the reaction is the smaller half of what this does.
    badges = [['apply', 'apply'], ['thumb', '👍']];
  } else {
    badges = [['reply', 'reply']];
  }

  /* The verdict rides on the same line as the thread it belongs to, as chips.
     They used to sit in a fixed column at the far left, which put a hand's width of
     nothing between the word and the thing it described — and hyphenated any
     label longer than the column. */
  const c = el('span', 'c');
  const head = el('div', 'threadhead');
  const kg = el('span', 'kgroup');
  for (const [kind, word] of badges) kg.appendChild(el('span', 'k ' + kind, word));
  head.appendChild(kg);
  head.appendChild(el('span', 'p', threadLabel(item.t)));
  c.appendChild(head);
  if (skipped || !isHandled(item)) {
    /* All three consequences stated, because "leaves it open" alone reads as
       harmless. */
    c.appendChild(el('span', 't',
      `Not handled. Stays open, nothing written, ${item.t.comments?.[0]?.author || 'they'} not re-requested.`));
  } else if (pos.stance === 'agree') {
    c.appendChild(el('span', 't', 'Makes the change, then 👍 — no written reply.'));
  } else {
    if (pos.stance === 'story') {
      c.appendChild(el('span', 't', `Files “${pos.story?.title || 'a story'}”, then replies with its id.`));
    }
    if (reviewState.editing[item.t.id]) {
      c.appendChild(answerBoxes(item));
      const done = el('button', 'linkbtn', 'done');
      done.onclick = () => { delete reviewState.editing[item.t.id]; renderReview(); };
      c.appendChild(done);
    } else {
      // A line by default, edit on demand — the box the card already offers.
      const preview = replyPreview(item, pickOf(item));
      // The instruction is not posted, so it is shown as a separate line rather
      // than quoted: seeing it beside the reply is how you catch the two swapped.
      const note = (reviewState.notes[item.t.id] || '').trim();
      if (note) {
        c.appendChild(el('span', 't instr',
          `Session: ${note.length > 90 ? note.slice(0, 90).trimEnd() + '…' : note}`));
      }
      const lineWrap = el('div', 'replyline');
      lineWrap.appendChild(el('span', 'q', preview ? `“${preview}”` : 'no reply written yet'));
      const edit = el('button', 'linkbtn', 'edit');
      edit.onclick = () => { reviewState.editing[item.t.id] = true; renderReview(); };
      lineWrap.appendChild(edit);
      c.appendChild(lineWrap);
    }
  }
  row.appendChild(c);
  return row;
}

/** The heading and the tally: every outward act as a count, before the list that
 *  spells them out. Badges rather than a sentence, because the question here is
 *  "how much of what", and a number you can read at a glance is the answer.
 *
 *  The irreversibility is stated once, quietly, under them. It used to be a framed
 *  panel of its own; a warning repeated in its own box on every send is one the eye
 *  learns to jump, and it was describing the same batch the list already showed. */
function rvWillDo(out) {
  const sec = el('div', 'sec willdo');
  sec.appendChild(el('div', 'eyebrow', 'what will be done'));
  const row = el('div', 'tallies');
  const add = (kind, n, one, many) => {
    if (!n) return;
    row.appendChild(el('span', 'tally-b ' + kind, `${n} ${n === 1 ? one : many}`));
  };
  // Commit first: it is the one that rewrites something that already exists.
  if (out.push !== 'no') {
    // "maybe" in words, never a `?`: the uncertainty is real and worth stating,
    // but a glyph makes the badge look like it is asking you something.
    row.appendChild(el('span', 'tally-b commit' + (out.push === 'may' ? ' maybe' : ''),
      out.push === 'will' ? '1 commit' : '1 commit, maybe'));
  }
  add('reply', out.replies, 'reply', 'replies');
  add('thumb', out.thumbs, 'thumbs up', 'thumbs up');
  add('req', out.rerequests, 're-request', 're-requests');
  add('story', out.stories, 'story', 'stories');
  if (!row.children.length) {
    row.appendChild(el('span', 'tally-b none', 'nothing'));
  }
  sec.appendChild(row);

  const note = el('div', 'willnote');
  if (row.querySelector('.none')) {
    note.textContent = 'Every thread was skipped, so nothing is written, pushed or posted.';
  } else {
    note.textContent = out.push === 'no'
      ? 'Comments are public and cannot be unsent.'
      : 'The branch head is rewritten and comments are public. None of it can be undone.';
  }
  sec.appendChild(note);
  return sec;
}

/** The commit, as a row in the same list as the threads.
 *
 *  It belongs to no single thread — one push carries all of them — but leaving it
 *  out of the list was worse: `outward().commits` reads position patches, which the
 *  session flow never has, so the most destructive act in the batch was the one
 *  thing the screen never mentioned. The certainty is graded rather than guessed. */
function rvCommitRow(out) {
  if (out.push === 'no') return null;
  const branch = `origin/${reviewState.data.head_ref || 'this branch'}`;
  const row = el('div', 'stage-row');
  const c = el('span', 'c');
  const head = el('div', 'threadhead');
  /* The three acts one push is made of, named separately because they fail and
     matter separately: writing code, folding it into the commits that own it, and
     rewriting the published branch. No hedging glyph on the badges — the certainty
     belongs in the sentence, where it can be said in words. */
  const kg = el('span', 'kgroup');
  for (const [kind, word] of [['code', 'code'], ['commit', 'commit'], ['push', 'push']]) {
    kg.appendChild(el('span', 'k ' + kind, word));
  }
  head.appendChild(kg);
  head.appendChild(el('span', 'p', branch));
  c.appendChild(head);
  c.appendChild(el('span', 't', out.push === 'will'
    ? 'Amends the commits that own the changed lines, then force-pushes. The branch '
      + 'head is rewritten for everyone who has it.'
    : `Amends and force-pushes only if the session changes code while answering `
      + `${out.pushThreads === 1 ? 'this thread' : `these ${out.pushThreads} threads`}.`));
  row.appendChild(c);
  return row;
}

/** One row per reviewer who gets re-requested, or is held back from it.
 *
 *  Per reviewer, not per PR: one whose every thread is addressed is re-requested
 *  even while another's are still open. The daemon recomputes this from a fresh
 *  fetch at post time; this is the same rule, shown early. */
function rvRerequestRows(q) {
  const viewer = reviewState.data.viewer;
  const mine = new Map();   // login -> { open: [labels] }
  for (const t of reviewState.data.threads || []) {
    if (!t.answerable) continue;
    const who = t.comments?.[0]?.author;
    if (!who || who === viewer) continue;
    const entry = mine.get(who) || { open: [] };
    const item = q.find((x) => x.t.id === t.id);
    if (!item || !isHandled(item)) {
      const line = t.line ?? t.original_line;
      entry.open.push(t.path ? (line ? `${t.path}:${line}` : t.path) : 'the review summary');
    }
    mine.set(who, entry);
  }

  const rows = [];
  for (const [who, { open }] of [...mine].sort()) {
    const row = el('div', 'stage-row' + (open.length ? ' off' : ''));
    const c = el('span', 'c');
    const head = el('div', 'threadhead');
    const kg = el('span', 'kgroup');
    kg.appendChild(el('span', 'k ' + (open.length ? 'skip' : 'req'),
      open.length ? 'no re-request' : 're-request'));
    head.appendChild(kg);
    head.appendChild(el('span', 'p', who));
    c.appendChild(head);
    c.appendChild(el('span', 't', open.length
      ? `Held back by ${open[0]}, which you did not handle.`
      : 'Every thread of theirs is addressed, so they are asked to look again.'));
    row.appendChild(c);
    rows.push(row);
  }
  return rows;
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

  /* How sure we are that a force-push happens.
     `commits` was derived from position patches, and the session flow's positions
     carry none — so the panel silently stopped reporting the most destructive
     thing in the batch. Rather than invent a diff we do not have, the certainty is
     graded and said in words: `agree` now means "make the change, then 👍", and a
     note is an instruction to change something, so either proves work. A plain
     reply might be prose, so it only earns `may`. Never `no` while the agent owns a
     thread — under-reporting a force-push is the bad direction to be wrong in. */
  const coding = handled.filter((x) => modeOf(x) === 'agent' &&
    positionOf(x).stance !== 'story');
  const push = !coding.length ? 'no'
    : coding.some((x) => positionOf(x).stance === 'agree' ||
        (reviewState.notes[x.t.id] || '').trim()) ? 'will' : 'may';

  return {
    files,
    commits: files.length ? 1 : 0,
    push, pushThreads: coding.length,
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
  if (!reviewState.open) return;

  // The single-session flow drives its own screens off the session's ask, not the
  // batch ladder — and it renders before /review has ever been fetched (the read
  // phase), so it does not fall through the `!data` guard the batch path needs.
  if (reviewState.session) return renderSessionReview(root);

  if (!reviewState.data) return;

  if (reviewState.report?.manual) reviewState.screen = 'manual';
  else if (reviewState.report) reviewState.screen = 'report';
  else if (reviewState.data.gate) reviewState.screen = 'gate';
  else if (!reviewState.data.proposals) reviewState.screen = 'intake';
  // Proposals are in and the tree is writable, but the screen is still sitting on
  // a pre-decision default: `intake` because triage had not run when the overlay
  // opened (and reopening the same PR does not reset it), or `gate` from before it
  // cleared. Nothing else advances off those, so triage produced cards the overlay
  // never showed. A screen the user navigated into — card, final, run — is left.
  else if (reviewState.screen === 'intake' || reviewState.screen === 'gate') {
    reviewState.screen = 'overview';
  }

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

/* ---------- the single-session flow ---------- */

/** The review session's pending, unanswered ask — read from the snapshot, so the
 *  overlay reacts to it on the same websocket tick everything else does. */
function sessionAsk() {
  const s = (snap.sessions || []).find((x) => x.id === reviewState.session);
  const i = s && s.interaction && !s.interaction.answer ? s.interaction : null;
  return i && i.options ? i : null;
}
const askHasValue = (ask, v) => !!ask && ask.options.some((o) => o.value === v);

/** Start one session that reads, then makes the changes you pick and posts. Unlike
 *  triage it does not close the overlay: it stays open and advances itself when the
 *  session has read the threads (the decision ask is how we know it has). */
async function startReviewSession() {
  if (reviewState.busy) return;
  reviewState.busy = true;
  reviewState.screen = 'reading';
  try {
    const r = await call(`/api/pr/${reviewState.pr}/review-session`);
    reviewState.session = r.session;
    // Put the rail and the pane behind the overlay on the session that is about
    // to ask for permissions, so closing the overlay lands on it instead of on
    // whatever you happened to be looking at when you started the review.
    setPendingSelect(r.session);
    reviewState.proposalsLoaded = false;
    reviewState.decisionsSent = false;
    toast('reading the threads…');
  } catch (e) {
    toast(e.message, true);
    reviewState.screen = 'intake';
  }
  reviewState.busy = false;
  renderReview();
}

/** Driven every websocket tick (from app.js). Watches the session's ask and moves
 *  the overlay between phases — the ask is the whole signal, so there is no polling
 *  of `/review` and no second source of truth. */
function reviewTick() {
  // Not gated on the overlay being open. The bar reports the phase from wherever you
  // are, so the phase has to go on advancing while you are looking at another
  // session — otherwise coming back would show you the screen you left rather than
  // the one the review has reached.
  if (!reviewState.session) return;
  const ask = sessionAsk();

  // The decision ask appears only after the session has posted its proposals, so it
  // is the proof they are ready. Fetch them once, then show the cards.
  if (askHasValue(ask, 'decisions') && !reviewState.proposalsLoaded) {
    reviewState.proposalsLoaded = true;
    reviewState.screen = 'overview';
    loadReview(reviewState.pr);
    return;
  }
  // The session ended.
  const s = (snap.sessions || []).find((x) => x.id === reviewState.session);
  if (s && s.alive) return;

  // It got as far as a phase we were driving, so there is a result to report.
  if (reviewState.decisionsSent) {
    if (reviewState.screen !== 'report') {
      reviewState.screen = 'report';
      renderReview();
    }
    return;
  }

  /* And otherwise the review is simply over — closed, killed, or it fell over while
     reading — with nothing decided and nothing to come back to. Let go of it, or the
     bar goes on announcing a session that no longer exists over every pane in the
     app, which is exactly how it looked: a review "reading the threads" forever,
     everywhere, for a conversation that had been closed. The overlay, if it is open,
     falls back to this PR's intake so it can be started again. */
  reviewState.session = null;
  reviewState.proposalsLoaded = false;
  reviewState.screen = 'intake';
  renderReview();
}

/** Route the session flow's own screens. */
function renderSessionReview(root) {
  if (reviewState.screen === 'reading') return rvReading(root);
  if (reviewState.screen === 'report') return rvSessionReport(root);
  if (reviewState.decisionsSent) return rvChanging(root);
  if (!reviewState.data || !reviewState.data.proposals) return rvReading(root);
  ({ overview: rvOverview, card: rvCard, final: rvFinal })[
    ['overview', 'card', 'final'].includes(reviewState.screen) ? reviewState.screen : 'overview'
  ](root);
}

/** The read phase: the session is reading, nothing to decide yet. */
function rvReading(root) {
  root.appendChild(rvHead(reviewState.data?.title || 'review'));
  const mid = el('div', 'mid');
  mid.appendChild(el('div', 'eyebrow', 'the session is reading the threads'));
  mid.appendChild(el('div', 'big', 'Reading…'));
  mid.appendChild(el('p', null,
    'One session reads the code at each thread and works out how it could be answered. '
    + 'It changes nothing yet. The cards open here the moment it is done — you do not '
    + 'reopen anything. Answer any permission prompts in the session’s pane.'));
  // The same way out `rvChanging` has: the reading phase is the one that asks for
  // permissions, so the pane it names has to be one gesture away.
  const row = el('div');
  row.style.cssText = 'display:flex;gap:8px;margin-top:6px';
  row.appendChild(headBtn('go to the pane', 'go', () => { closeReview(); setSelected(reviewState.session); }));
  mid.appendChild(row);
  root.appendChild(mid);
}

/** Between the decision submit and the post-go ask: the session is writing code. */
function rvChanging(root) {
  root.appendChild(rvHead(reviewState.data?.title || 'review'));
  const mid = el('div', 'mid');
  mid.appendChild(el('div', 'eyebrow', 'the session is making the changes you picked'));
  mid.appendChild(el('div', 'big', 'Applying…'));
  mid.appendChild(el('p', null,
    'It writes the code for each solution you chose, runs the repo’s checks, amends '
    + 'the owning commit and pushes, then posts your replies. Answer any permission '
    + 'prompts in the session’s pane. Nothing more is asked of you.'));
  const row = el('div');
  row.style.cssText = 'display:flex;gap:8px;margin-top:6px';
  row.appendChild(headBtn('go to the pane', 'go', () => { closeReview(); setSelected(reviewState.session); }));
  mid.appendChild(row);
  root.appendChild(mid);
}


/** Nothing more to do: the session finished. */
function rvSessionReport(root) {
  root.appendChild(rvHead('done'));
  const mid = el('div', 'mid');
  mid.appendChild(el('div', 'big', 'Posted.'));
  mid.appendChild(el('p', null, 'The session answered the threads and finished. Read its pane for the detail of what it changed and posted.'));
  root.appendChild(mid);
  root.appendChild(rvActs([actBtn('done', 'pri', () => finishReview())]));
}

/** Put the whole review away, rather than just the overlay.
 *
 *  `closeReview` means "I am looking at something else" — the bar goes on reporting
 *  and selecting the session brings the cards back. That is wrong for a review that
 *  has finished: there is nothing to come back to, and a bar reporting `posted`
 *  forever is the furniture this codebase keeps deleting. */
function finishReview() {
  reviewState.session = null;
  reviewState.pr = null;
  reviewState.data = null;
  reviewState.decisionsSent = false;
  closeReview();
}

/** The decision set the overlay hands back over the ask channel. One per thread:
 *  the stance, which solution the human picked, and the reply as they edited it. */
function decisionSet() {
  return queue().map((item) => {
    if (reviewState.skipped[item.t.id]) return { thread_id: item.t.id, stance: 'skip' };
    const pos = positionOf(item);
    const d = {
      thread_id: item.t.id,
      stance: pos.stance,
      solution: pos.label,
      reply: pos.stance === 'agree' ? '' : replyOf(item),
    };
    // `note` is what to do, and the prompt reads it as an instruction to follow.
    // Only sent when you actually wrote one, since its presence is the signal.
    const note = (reviewState.notes[item.t.id] || '').trim();
    if (note) d.note = note;
    return d;
  });
}

/** Answer the review session's pending ask, carrying the JSON in the free-text
 *  field the option opened. */
async function answerSession(value, payload) {
  const ask = sessionAsk();
  if (!ask) { toast('the session is not waiting on anything just now', true); return false; }
  try {
    await call(`/api/session/${reviewState.session}/answer`, {
      ask: ask.id, answer: value, text: JSON.stringify(payload),
    });
    return true;
  } catch (e) {
    toast(e.message, true);
    return false;
  }
}

/** Send the picks to the waiting session; it moves to the change phase. */
async function submitDecisions() {
  if (reviewState.busy) return;
  // A reply/story pick with an empty box would post a blank comment, which cannot
  // be unsent — the daemon refuses it too. Caught here rather than by disabling
  // the button, so typing does not force a focus-dropping repaint per keystroke.
  const blank = queue().filter((x) => needsWords(x) && !replyOf(x).trim());
  if (blank.length) {
    return toast(`write a reply for ${blank.map((x) => threadLabel(x.t)).join(', ')}`, true);
  }
  // A free-text answer needs its instruction too: the reply says what the reviewer
  // reads, and without the note the session is told nothing about what to do.
  const noInstr = queue().filter((x) =>
    isHandled(x) && isFreeText(positionOf(x)) && !(reviewState.notes[x.t.id] || '').trim());
  if (noInstr.length) {
    return toast(
      `write instructions for ${noInstr.map((x) => threadLabel(x.t)).join(', ')}`, true);
  }
  reviewState.busy = true;
  const ok = await answerSession('decisions', { decisions: decisionSet() });
  reviewState.busy = false;
  if (!ok) return;
  reviewState.decisionsSent = true;
  reviewState.screen = 'changing';
  toast('sent — the session is applying your picks');
  renderReview();
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
      reviewState.notes = {};
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

/** The live session answering this PR, whichever window started it. */
function liveReviewSession(pr) {
  return (snap.sessions || []).find((x) => x.alive
    && x.kind.kind === 'automation' && x.kind.command === 'review' && x.kind.pr === pr) || null;
}

/** Can this window pick up that session where it stands?
 *
 *  Only before it has been handed a decision set, because the decisions live in
 *  this SPA and nowhere else: adopting a session mid-change would face the post
 *  screen with empty picks, and post the recommended reply for every thread —
 *  including the ones the other window skipped. So: no ask yet (still reading),
 *  or the decision ask still open. Anything later is watched from its pane. */
function adoptable(s) {
  if (!s) return false;
  const i = s.interaction;
  return !i || (!i.answer && i.options.some((o) => o.value === 'decisions'));
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
    reviewState.notes = {};
    reviewState.editing = {};
    reviewState.report = null;
    reviewState.head = null;
    reviewState.i = 0;
    reviewState.screen = 'intake';
    reviewState.data = null;
    reviewState.session = null;
    reviewState.proposalsLoaded = false;
    reviewState.decisionsSent = false;
    // A session for this PR may already be running: the overlay was closed and
    // another PR opened in between, or the page reloaded. Adopt it rather than
    // leaving it mid-ask with nothing able to answer it. Its phase comes from the
    // ask, so `tick` sorts out which screen this is.
    const live = liveReviewSession(pr);
    if (adoptable(live)) {
      reviewState.session = live.id;
      reviewState.screen = 'reading';
    }
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

/** DEV-ONLY. Render the flat overview/card/final against canned data, with no
 *  session and no daemon fetch — so the flattened UI can be clicked while the
 *  GitHub fixture is blocked on CI. Reached only from `/review-preview`; nothing
 *  in the app calls it, and `send` is inert because there is no session to answer. */
export function preview(data) {
  reviewState.picks = {};
  reviewState.skipped = {};
  reviewState.drafts = {};
  reviewState.notes = {};
  reviewState.editing = {};
  reviewState.report = null;
  reviewState.head = data.proposals?.base_sha || null;
  reviewState.i = 0;
  reviewState.session = null;
  reviewState.proposalsLoaded = false;
  reviewState.decisionsSent = false;
  reviewState.pr = data.pr_number ?? 0;
  reviewState.data = data;
  reviewState.open = true;
  reviewState.screen = 'overview';
  $('rvoverlay').classList.add('on');
  renderReview();
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
  // The words are edited on the overview, so an empty reply is not blocked here —
  // the send button on the overview is where a blank reply is refused.
  reviewState.picks[item.t.id] = pickOf(item);
  delete reviewState.skipped[item.t.id];
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
      if (n) toast(`re-requested ${r.rerequested.join(', ')}`);
      // Why nobody, not just that there was nobody: before, a reviewer held back by
      // a thread of their own read the same as a PR with no reviewers at all.
      for (const h of r.held_back || []) toast(h);
      if (!n && !(r.held_back || []).length) toast('nobody to re-request yet');
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

/* ---------- the bar the overlay leaves behind ---------- */

/** What the review is doing, for someone who is not looking at it.
 *
 *  Returns `null` when there is nothing to say. The tone is the flow's own: `work`
 *  is the agent busy, `attn` is it waiting on you, `ok` is finished — the same three
 *  the rail uses, so a review reads like every other row in the app. */
function barState() {
  if (!reviewState.session) return null;
  const s = (snap.sessions || []).find((x) => x.id === reviewState.session);
  // Belt to the tick's braces: a bar for a session that is not there is the one
  // thing this must never draw, whatever order the snapshot and the render ran in.
  if (!s && !reviewState.decisionsSent) return null;
  const q = reviewState.data?.proposals ? queue() : [];
  if (reviewState.screen === 'report' || (s && !s.alive && reviewState.decisionsSent)) {
    return { tone: 'ok', what: `posted · ${q.length || ''} answered`.replace('·  ', '· ') };
  }
  // Nothing has been sent yet and the cards are up: it is your turn, and how many
  // threads are left is the only number worth carrying out here.
  if (!reviewState.decisionsSent && q.length) {
    const left = q.filter((x) => !isHandled(x) && !reviewState.skipped[x.t.id]).length;
    return left
      ? { tone: 'attn', what: `${left} of ${q.length} threads waiting on you` }
      : { tone: 'attn', what: `${q.length} threads decided · not sent yet` };
  }
  if (reviewState.decisionsSent) return { tone: 'work', what: 'writing the code' };
  return { tone: 'work', what: 'reading the threads' };
}

/** Draw the bar, or take it away.
 *
 *  Only while the overlay is *closed*: open, the cards are the report. Rendered from
 *  `app.js`'s tick like every other pane, and into a host that lives outside
 *  `#rvoverlay` — `renderReview` replaces that element's children on every snapshot
 *  and would tear a live node out from under itself once a second. */
function renderBar() {
  const host = $('rvbar');
  const st = reviewState.open ? null : barState();
  if (!st) { host.hidden = true; host.replaceChildren(); return; }

  host.replaceChildren();
  host.className = `rvbar ${st.tone}`;
  host.appendChild(el('span', 'dot'));
  host.appendChild(el('span', 'k', `review · pr ${reviewState.pr}`));
  host.appendChild(el('span', 'what', st.what));
  const go = el('button', 'go', `open · ${MOD_LABEL}\u21e7R`);
  go.onclick = () => openReview(reviewState.pr);
  host.appendChild(go);
  host.hidden = false;
}

// The public surface. Everything else above is private by construction now,
// which is the point: the rail reaches the overlay through these four or not
// at all.

export {
  reviewState as state, openReview as open, closeReview as close,
  reviewKey as key, reviewTick as tick, renderBar as bar,
};
