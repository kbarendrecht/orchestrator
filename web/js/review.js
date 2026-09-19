// The review overlay: read a PR's threads, decide each one, then one batch of
// outward writes. The largest single feature in the SPA.

import { $, call, compactAge, el, get, MOD_LABEL, reason, selected, setPendingSelect, setSelected, snap, toast, unchanged } from './core.js';
import * as Diff from './diff.js';
import { langFor, hlTokens, paintRanges } from './diff.js';
import { patchStats, hunkEl } from './review-diff.js';


/* Replaces typing `/resolve <pr>` into a terminal pane. The agent reads every
   thread and proposes; you go through them and decide. Nothing is written until
   the final action.

   `design/review-overlay.html` is the spec for anything visual here.

   Local state is NOT derived from the snapshot. `render()` redraws from a full
   Snapshot on every websocket tick, and `Diff.state`/`Diff.edit` survive only
   because `render()` never touches them; this follows that idiom, or every tick
   would reset the scroll position and drop focus out of a half-typed reply. */
/** The review payload from `GET /api/pr/:n/review`.
 *
 *  Composed from the generated types rather than described by hand: `threads` and
 *  `proposals` are `ts-rs` exports of the structs the route serialises, so a
 *  renamed field on the Rust side fails here. The route itself
 *  builds a `json!` object, which is the one part with no struct to generate
 *  from — the four fields below the composed ones are what that literal adds.
 *
 *  @typedef {{
 *    title: string,
 *    url: string,
 *    head_ref: string,
 *    base_ref: string,
 *    viewer: string,
 *    head_sha: string,
 *    answerable: number,
 *    threads: import('../repo').Thread[],
 *    proposals: import('../base').ProposalSet | null,
 *    checks: import('../repo').Checks,
 *    mergeable: string,
 *    tracker: boolean,
 *  }} ReviewData
 */

/** @type {{ open: boolean, pr: number | null, head: string | null,
 *           data: ReviewData | null, screen: string, i: number,
 *           picks: Record<string, number>, modes: Record<string, string>,
 *           skipped: Record<string, boolean>, drafts: Record<string, string>,
 *           notes: Record<string, string>, editing: Record<string, boolean>,
 *           busy: boolean,
 *           session: string | null, proposalsLoaded: boolean,
 *           decisionsSent: boolean }} */
const reviewState = {
  open: false,
  pr: null,
  head: null,          // the head sha the proposals were generated against
  data: null,          // the /review payload
  screen: 'card',      // reading | card | changing | final | report
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
  busy: false,
  /* The single-session flow. `session` is the review session's id once started;
     while it is set, the overlay is driven by that session's ask (read from the
     snapshot). Null means no review is running on this PR,
     which is left exactly as it was. */
  session: null,
  proposalsLoaded: false,   // fetched /review once, when the decision ask appeared
  decisionsSent: false,     // answered the decision ask; the change phase is running
};

/** One thread and the agent's proposal for it, the way `queue()` pairs them.
 *  Every card, box and count below takes one of these.
 *
 *  @typedef {{ t: import('../repo').Thread, p: import('../base').Proposal }} QueueItem
 */

const draftKey = (/** @type {string} */ id, /** @type {number} */ pos) => `${id} ${pos}`;

function commentAge(/** @type {string | null | undefined} */ iso) {
  const then = Date.parse(iso ?? '');
  if (!then) return '';
  return compactAge((Date.now() - then) / 36e5);
}

/** The threads triage proposed for, in the order the daemon sorted them —
 *  GitHub's Files-changed order, which is the view people review in. */
function queue() {
  const set = reviewState.data?.proposals?.proposals || [];
  const by = new Map(set.map((p) => [p.thread_id, p]));
  return (reviewState.data?.threads || [])
    .filter((t) => by.has(t.id))
    .map((t) => ({ t, p: /** @type {import('../base').Proposal} */ (by.get(t.id)) }));
}

/** Whether a position can be acted on at all.
 *
 *  Only one thing makes a position unavailable: a story with no tracker
 *  configured. It is hidden rather than offered-and-refused. */
const offered = (/** @type {import('../base').Position} */ pos) => pos.stance !== 'story' || !!reviewState.data?.tracker;

/** Which position is selected on a card: your pick, else the recommendation.
 *
 *  Falls through to the first position that is actually offered, because the
 *  recommendation is often the story one — that is the whole point of a story on a
 *  review summary — and with no tracker it is not on screen. Leaving the pick
 *  pointing at a hidden row would have Enter send a decision the daemon refuses. */
function pickOf(/** @type {QueueItem} */ item) {
  const own = reviewState.picks[item.t.id];
  const want = own === undefined ? item.p.recommend : own;
  if (offered(item.p.positions[want])) return want;
  const first = item.p.positions.findIndex(offered);
  return first < 0 ? want : first;
}

const positionOf = (/** @type {QueueItem} */ item) => item.p.positions[pickOf(item)];

/** The reply that would be posted: your wording if you typed one. */
function replyOf(/** @type {QueueItem} */ item) {
  const i = pickOf(item);
  const drafted = item.p.positions[i]?.reply ?? '';
  return reviewState.drafts[draftKey(item.t.id, i)] ?? drafted;
}

const isHandled = (/** @type {QueueItem} */ item) =>
  !reviewState.skipped[item.t.id] && reviewState.picks[item.t.id] !== undefined;

/** Decided: handled or skipped. One word for one idea — the strip, the counts,
 *  the overview and the final screen all ask this and used to spell it out. */
const isDecided = (/** @type {QueueItem} */ item) => isHandled(item) || !!reviewState.skipped[item.t.id];

/** `renovate.json5:161 · bob`, matching what the daemon's report uses. */
function threadLabel(/** @type {import('../repo').Thread} */ t) {
  const who = t.comments?.[0]?.author || 'ghost';
  const line = t.line ?? t.original_line;
  if (!t.path) return `review summary · ${who}`;
  return line ? `${t.path}:${line} · ${who}` : `${t.path} · ${who}`;
}

/* ---------- chrome shared by every screen ---------- */

/** The header. `sub` is the right-hand half of the title line. */
function rvHead(/** @type {string} */ sub, /** @type {string | number | undefined} */ count) {
  const head = el('div', 'ov-head');
  const path = el('div', 'ov-path', `#${reviewState.pr} `);
  path.appendChild(el('span', null, `· ${sub}`));
  head.appendChild(path);
  head.appendChild(rvHealth());

  const nav = el('div', 'ov-nav');
  nav.appendChild(rvSteps());
  if (count) nav.appendChild(el('span', 'ov-count', String(count)));
  const esc = el('button', 'head-btn', 'esc');
  esc.onclick = () => closeReview();
  nav.appendChild(esc);
  head.appendChild(nav);
  return head;
}

/** Where you are in the run — three stages the five screens fold into, so the
 *  header answers "how far in, and what is left" without another screen. The
 *  per-thread strip tracks the threads; this tracks the flow. */
function rvSteps() {
  const stageOf = {
    // `reading` and `changing` are the two screens the session owns and you only
    // watch. Without them the lookup missed, `indexOf` answered -1 and the trail
    // went blank on the two screens where "how far in am I" is the only question
    // you have.
    reading: 'read',
    card: 'answer',
    final: 'send', changing: 'send', report: 'send',
  };
  const order = ['read', 'answer', 'send'];
  const cur = order.indexOf(stageOf[/** @type {keyof typeof stageOf} */ (reviewState.screen)] || 'found');
  const wrap = el('div', 'rvsteps');
  order.forEach((name, i) => {
    wrap.appendChild(el('span', 's' + (i < cur ? ' done' : i === cur ? ' on' : ''), name));
  });
  return wrap;
}

/** Branch health — CI colour and a develop conflict.
 *
 *  Information, never a gate: neither touches the apply/push machinery, which
 *  works inside the branch's own history. It used to carry a `fix` button as well,
 *  on the two pre-decision screens only — the two flows are mutually exclusive, so
 *  offering it from an active card would just be refused. Both screens are gone and
 *  the rail's PR row has the same verb, so this reports and nothing more. */
function rvHealth() {
  const d = reviewState.data;
  const wrap = el('div', 'health');
  const said = [];
  if (d?.checks === 'failing') { wrap.classList.add('bad'); said.push('checks failing'); }
  else if (d?.checks === 'passing') { wrap.classList.add('ok'); said.push('checks passing'); }
  else if (d?.checks === 'pending') { wrap.classList.add('pending'); said.push('checks running'); }
  // `conflicts with develop` named one repo's base branch on every repo. The PR
  // carries its own, which is also the right answer for a stacked PR whose base is
  // another PR's head.
  if (d?.mergeable === 'CONFLICTING') said.push(`conflicts with ${d.base_ref || 'its base'}`);
  if (!said.length) return wrap;

  wrap.appendChild(el('span', 'hdot'));
  said.forEach((s, i) => {
    if (i) wrap.appendChild(el('span', 'sep', '·'));
    const n = el('span', 's', s);
    if (i) n.style.color = 'var(--dim)';
    wrap.appendChild(n);
  });
  return wrap;
}

/** One bar per thread, filled by outcome. Bars read as progress through a
 *  queue; dots would read as status lights and invite a colour per verdict,
 *  which is not what varies. */
function rvStrip(/** @type {number | null} */ cur) {
  const strip = el('div', 'strip');
  const q = queue();
  q.forEach((item, i) => {
    let cls = 'pip';
    if (reviewState.skipped[item.t.id]) cls += ' skip';
    else if (reviewState.picks[item.t.id] !== undefined) cls += ' staged';
    if (i === cur) cls += ' cur';
    strip.appendChild(el('span', cls));
  });

  const done = q.filter(isDecided).length;
  let left;
  // On a card the strip is a position; everywhere else it is progress, and it says
  // "decided" because that is the one word this flow counts in.
  if (cur !== null && cur !== undefined) left = `${cur + 1} / ${q.length}`;
  else if (!done) left = 'none decided';
  else left = `${done} / ${q.length} decided`;
  strip.appendChild(el('span', 'left', left));
  return strip;
}

/** What the header's count says. */
/** How far through the threads you are.
 *
 *  One word for one idea: *decided*. This used to say "staged", which is git's word
 *  for something else entirely, and the card hint said "nothing chosen yet" for the
 *  same count in a different vocabulary — three names between them for the number
 *  the tally, the strip and the bar are all showing. */
function decidedCount() {
  const q = queue();
  const decided = q.filter(isDecided).length;
  const skipped = q.filter((x) => reviewState.skipped[x.t.id]).length;
  const of = `${decided} of ${q.length} decided`;
  return skipped ? `${of} · ${skipped} skipped` : of;
}

function rvActs(/** @type {(HTMLElement | null)[]} */ buttons, /** @type {string | undefined} */ hint) {
  const bar = el('div', 'acts');
  for (const b of buttons) if (b) bar.appendChild(b);
  if (hint) bar.appendChild(el('span', 'hint', hint));
  return bar;
}

function actBtn(/** @type {string} */ label, /** @type {string | null} */ cls, /** @type {() => unknown} */ onclick, /** @type {boolean | undefined} */ disabled) {
  const b = el('button', 'act' + (cls ? ' ' + cls : ''), label);
  b.disabled = !!disabled || reviewState.busy;
  b.onclick = onclick;
  return b;
}

/** A button with its chord in the tooltip. Separate from [`actBtn`] because only
 *  a couple of these have one, and a parameter nobody passes reads as noise. */
function withTitle(/** @type {HTMLElement} */ button, /** @type {string} */ title) {
  button.title = title;
  return button;
}

function headBtn(/** @type {string} */ label, /** @type {string | null} */ cls, /** @type {() => unknown} */ onclick) {
  const b = el('button', 'head-btn' + (cls ? ' ' + cls : ''), label);
  b.disabled = reviewState.busy;
  b.onclick = onclick;
  return b;
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
  if (!fresh.length || !reviewState.data?.proposals) return document.createComment('no new threads');

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
function rvCard(/** @type {HTMLElement} */ root) {
  const q = queue();
  const item = q[reviewState.i];
  if (!item) { reviewState.screen = 'final'; return rvFinal(root); }
  const { t, p } = item;

  root.appendChild(rvHead(`thread ${reviewState.i + 1} of ${q.length}`, decidedCount()));
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
    const mine = c.author === reviewState.data?.viewer;
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

  /* -- the agent's read, *above* the evidence rather than below it.
     It is the card's verdict: the one line saying whether the reviewer is right,
     and the only place the agent can say they are not — the positions can offer the
     other side but an `.osub` is a single ellipsised line and cannot argue. Fourth
     in reading order it sat under two blocks of unbounded length, so on a long
     thread the fast path was anchor, Enter, verdict never seen. Conclusion first;
     the hunk and the chain are reference and read as such underneath it. */
  body.appendChild(rvRead(p));
  body.appendChild(top);

  // -- the ways to answer this thread, one flat list; the selected option's reply
  //    is edited in the box below it, prefilled from what the read already drafted.
  body.appendChild(rvOptions(item));
  const reply = rvCardReply(item);
  if (reply) body.appendChild(reply);
  root.appendChild(body);

  // The header already carries "thread N of q" and the decided count, so the hint
  // is free to teach the two keys that have no button of their own. Enter and s
  // ride the accept and skip buttons.
  const hint = 'j / k to move · 1–9 to pick';
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
function commentParts(/** @type {string} */ body, /** @type {string | null} */ path) {
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
function suggestionEl(/** @type {string} */ code, /** @type {string | null} */ path) {
  const box = el('div', 'suggestion');
  box.appendChild(el('div', 'sghead', 'suggested change'));
  const body = el('div', 'sgbody');
  const lang = langFor(path ?? '');
  for (const line of code.split('\n')) {
    const row = el('div', 'sgline');
    // A blank line still needs something in it, or the row collapses.
    row.appendChild(hlLine(line || ' ', lang));
    body.appendChild(row);
  }
  box.appendChild(body);
  return box;
}

/** One line of code, syntax-coloured with the viewer's palette. */
function hlLine(/** @type {string} */ text, /** @type {string | null} */ lang) {
  return paintRanges(el('span', 'sgcode'), text, lang ? hlTokens(text, lang) : []);
}

/** Whether the reads are expanded, for the whole session. Collapsed by default;
 *  opening one opens them all, and they stay open until you close one or relaunch. */
let readsOpen = false;

/** The read, collapsed by default.
 *
 *  History, because this reverses an earlier decision: the read was once collapsed to
 *  its first sentence for *clutter*, and that was reverted — the fold cost a click on
 *  every card to see the one thing the agent concluded, worse than the length it hid.
 *  This collapse is for a different reason: *anchoring*. The read is a confident verdict
 *  shown first, and stating the agent's conclusion up front pulls the human toward it
 *  before they have read the comment and the code themselves. Folded, the reviewer and
 *  the diff lead, and the read is context you pull in rather than a headline pushed at
 *  you — a little more in the human's hands.
 *
 *  The per-card-click objection that killed the first fold is answered by making the
 *  toggle *sticky for the session*: open one and they are all open. It resets to
 *  collapsed next launch, so each session starts with the human forming their own view.
 *  The prompt still keeps the read terse. */
function rvRead(/** @type {import('../base').Proposal} */ p) {
  const sec = el('div', 'sec');
  if (!readsOpen) {
    const tog = el('button', 'readtog');
    tog.setAttribute('aria-expanded', 'false');
    tog.setAttribute('aria-label', 'Show the agent’s read');
    tog.appendChild(el('span', 'car', '▸'));
    tog.appendChild(el('span', 'q', 'some context'));
    tog.appendChild(el('span', 'hint', 'tap to show'));
    tog.onclick = () => { readsOpen = true; renderReview(); };
    sec.appendChild(tog);
    return sec;
  }
  const read = el('div', 'read');
  const head = el('button', 'readhead');
  head.setAttribute('aria-expanded', 'true');
  head.setAttribute('aria-label', 'Hide the agent’s read');
  head.appendChild(el('span', 'car', '▾'));
  // The same words as the collapsed toggle, so opening only flips the caret — and it
  // does not restate "is the reviewer right?", the verdict framing that primed the
  // human toward the read before they had formed their own view.
  head.appendChild(el('span', 'eyebrow', 'some context'));
  head.onclick = () => { readsOpen = false; renderReview(); };
  read.appendChild(head);
  read.appendChild(el('p', null, (p.read || '').trim()));
  sec.appendChild(read);
  return sec;
}

/** A short preview of the reply a reply/story option would post, drafted or as
 *  edited on the overview. Empty when there is nothing written yet. */
function replyPreview(/** @type {QueueItem} */ item, /** @type {number} */ i) {
  const pos = item.p.positions[i];
  const r = (reviewState.drafts[draftKey(item.t.id, i)] ?? pos.reply ?? '').trim();
  if (!r) return '';
  return r.length > 120 ? r.slice(0, 120).trimEnd() + '…' : r;
}

/** The flat list of ways to answer this thread: one row per offered position, in
 *  the order triage handed them (it leads with `agree` where the reviewer is
 *  simply right), then Skip. No stance segment, no alts sub-row, no inline editor —
 *  picking a row stages it, and the words are edited on the overview. */
function rvOptions(/** @type {QueueItem} */ item) {
  const sec = el('div', 'sec');
  const list = el('div', 'opts');
  const chosen = reviewState.skipped[item.t.id] ? -1 : pickOf(item);

  item.p.positions.forEach((/** @type {import('../base').Position} */ pos, /** @type {number} */ i) => {
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
    /* The other side, in a line. Triage is *required* to draft the case against its own
       read — not a strawman (commands/triage.md) — and that argument lands in the
       position's `reply`. But a reply only shows once you select its row, so the reason to
       disagree was invisible at the moment you decide, and the ellipsised `sub` above
       "cannot argue". Surface the gist on every non-recommended reply/story position: the
       recommended side is already argued in the read above, and agree/"Something else"
       carry no drafted words, so those stay quiet. One line, from data, no extra model call. */
    if (i !== item.p.recommend) {
      const why = replyPreview(item, i);
      if (why) b.appendChild(el('div', 'owhy', why));
    }
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
const isFreeText = (/** @type {import('../base').Position} */ pos) => pos.stance === 'reply' && !((pos.reply || '').trim());

/** The reply box, shared by the card and the overview's edit toggle. Prefilled
 *  from the draft the read already produced — instant, nothing waits on the agent —
 *  and written back on input with no re-render, so typing stays smooth and the
 *  cursor never jumps. A textarea, not contenteditable: the text goes to GitHub as
 *  plain markdown, so rich paste is liability and browsers insert <div>/<br> where
 *  a newline belongs. `Diff.openEditor()` settled this. */
function replyBox(/** @type {QueueItem} */ item) {
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
function instructionBox(/** @type {QueueItem} */ item) {
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
function answerBoxes(/** @type {QueueItem} */ item) {
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
function rvCardReply(/** @type {QueueItem} */ item) {
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
function rvFootState(/** @type {HTMLElement} */ bodyEl, /** @type {QueueItem} */ item, /** @type {import('../base').Position} */ pos, /** @type {number} */ i) {
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
const needsWords = (/** @type {QueueItem} */ item) =>
  isHandled(item) && ['reply', 'story'].includes(positionOf(item).stance);

/** The overview: every thread's answer in one list, with the drafted replies
 *  listed and editable here rather than one card at a time. Nothing has left the
 *  machine yet — the session applies the picks and posts only on your go. */
function rvFinal(/** @type {HTMLElement} */ root) {
  const q = queue();
  root.appendChild(rvHead('review & send', decidedCount()));
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

  const decided = q.every(isDecided);
  /* What actually leaves the machine, counted rather than described. This is the
     only irreversible control in the flow — one press writes code *and* speaks to a
     reviewer — and it used to be labelled `send to the session`, which is true of the
     plumbing and silent about GitHub. A button is read on its own, so it names the
     outcome; the hint carries the numbers, which are what change the decision. */
  const said = saidCounts(out);
  root.appendChild(rvActs([
    // A blank reply is not disabled here — that would need a live repaint on every
    // keystroke, which drops focus out of the box. `submitDecisions` refuses it.
    // The chord presses this very button (`app.js` clicks `.acts .act.warm`), so
    // the tooltip names it here rather than the map naming it somewhere else.
    withTitle(
      actBtn('apply, push and post', 'warm', () => submitDecisions(), !decided),
      `Send the batch · ${MOD_LABEL} \u23ce`,
    ),
    actBtn('back', null, () => { reviewState.screen = 'card'; renderReview(); }),
  ], said
    ? `${said} go to the PR · nothing here can be unsent`
    : 'nothing to say to a reviewer · this only applies and pushes'));
}

/** One thread's row on the overview: what it will do, and its reply as a line with
 *  an `edit` toggle — not a textarea by default, which read as clutter. `agree`/
 *  `skip` are static; a reply/story shows the drafted words and opens a box on ask. */
function rvOverviewRow(/** @type {QueueItem} */ item) {
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
function rvWillDo(/** @type {ReturnType<typeof outward>} */ out) {
  const sec = el('div', 'sec willdo');
  sec.appendChild(el('div', 'eyebrow', 'what will be done'));
  const row = el('div', 'tallies');
  const add = (/** @type {string} */ kind, /** @type {number} */ n, /** @type {string} */ one, /** @type {string} */ many) => {
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
function rvCommitRow(/** @type {ReturnType<typeof outward>} */ out) {
  if (out.push === 'no') return null;
  const branch = `origin/${reviewState.data?.head_ref || 'this branch'}`;
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
function rvRerequestRows(/** @type {QueueItem[]} */ q) {
  const viewer = reviewState.data?.viewer;
  const mine = new Map();   // login -> { open: [labels] }
  for (const t of reviewState.data?.threads || []) {
    if (!t.answerable) continue;
    const who = t.comments?.[0]?.author;
    if (!who || who === viewer) continue;
    const entry = mine.get(who) || { open: [] };
    const item = q.find((/** @type {QueueItem} */ x) => x.t.id === t.id);
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


/** `2 replies and 1 👍`: the outward writes in words, or '' for none. */
function saidCounts(/** @type {ReturnType<typeof outward>} */ out) {
  return [
    out.replies && `${out.replies} ${out.replies === 1 ? 'reply' : 'replies'}`,
    out.thumbs && `${out.thumbs} 👍`,
    out.stories && `${out.stories} ${out.stories === 1 ? 'story' : 'stories'}`,
  ].filter(Boolean).join(' and ');
}

/** Everything the batch would do, counted. */
function outward(/** @type {QueueItem[]} */ q) {
  const handled = q.filter(isHandled);
  /** @type {{ path: string, added: number, deleted: number }[]} */
  const files = [];
  for (const item of handled) {
    if (!positionOf(item).patch) continue;
    for (const f of patchStats(positionOf(item).patch)) {
      const seen = files.find((x) => x.path === f.path);
      if (seen) { seen.added += f.added; seen.deleted += f.deleted; }
      else files.push({ ...f });
    }
  }
  const replies = handled.filter((/** @type {QueueItem} */ x) => replyOf(x).trim() &&
    ['reply', 'story'].includes(positionOf(x).stance)).length;
  // Counted apart from the GitHub writes: a story goes to a different system, and
  // it is the one thing in the batch that is not re-derivable from the PR.
  const stories = handled.filter((/** @type {QueueItem} */ x) => positionOf(x).stance === 'story').length;
  const thumbs = handled.filter((/** @type {QueueItem} */ x) => positionOf(x).stance === 'agree').length;

  /* How sure we are that a force-push happens.
     `commits` was derived from position patches, and the session flow's positions
     carry none — so the panel silently stopped reporting the most destructive
     thing in the batch. Rather than invent a diff we do not have, the certainty is
     graded and said in words: `agree` now means "make the change, then 👍", and a
     note is an instruction to change something, so either proves work. A plain
     reply might be prose, so it only earns `may`. Never `no` while the agent owns a
     thread — under-reporting a force-push is the bad direction to be wrong in. */
  // Every handled thread but a story: the agent owns all of them now that a thread
  // cannot be taken over by hand. `manual` was the batch's third decision and went
  // with it.
  const coding = handled.filter((/** @type {QueueItem} */ x) => positionOf(x).stance !== 'story');
  const push = !coding.length ? 'no'
    : coding.some((/** @type {QueueItem} */ x) => positionOf(x).stance === 'agree' ||
        (reviewState.notes[x.t.id] || '').trim()) ? 'will' : 'may';

  return {
    files,
    commits: files.length ? 1 : 0,
    push, pushThreads: coding.length,
    stories,
    replies, thumbs,
    total: replies + thumbs,
  };
}

function renderReview() {
  const root = $('rvoverlay');
  root.replaceChildren();
  if (!reviewState.open) return;

  /* **No session means no overlay at all**, and that is the whole of the routing
     now. There used to be a ladder here: an intake screen offering to start the
     read, and a worktree gate in front of it. Both were the batch's furniture —
     the rail's review verb starts the session itself, and the daemon refuses a
     dirty start with `Gate::say()` in the toast — so the two ways in
     (`app.js`'s checkpoint answer and `MOD⇧R`) both already hold a session. An
     overlay with none is a state nothing can reach, and drawing nothing is what it
     deserves rather than a screen kept alive for it. */
  if (!reviewState.session) return;
  renderSessionReview(root);
}

/* ---------- the single-session flow ---------- */

/** The review session's pending, unanswered ask — read from the snapshot, so the
 *  overlay reacts to it on the same websocket tick everything else does. */
function sessionAsk() {
  const s = (snap.sessions || []).find((x) => x.id === reviewState.session);
  const i = s && s.interaction && !s.interaction.answer ? s.interaction : null;
  return i && i.options ? i : null;
}
const askHasValue = (/** @type {import('../snapshot').Interaction | null | undefined} */ ask, /** @type {string} */ v) => !!ask && ask.options.some((/** @type {import('../snapshot').InteractionOption} */ o) => o.value === v);

/** Start the overlay session on a PR, from wherever you are.
 *
 *  **The rail's second review item comes here now.** It used to start the headless
 *  triage pass, whose proposals a *later* run carried out — and this flow was
 *  reachable only from the overlay's own intake screen, which is a screen the
 *  triage flow owns. Two flows, one of them reachable only through the other's
 *  furniture. This is the same spawn either way; the rail passes the PR because it
 *  has one and the overlay does not need to.
 *
 *  The overlay is deliberately not opened. The read takes minutes of somebody
 *  else's work, and a full screen saying so is a window spent on one sentence: the
 *  bar carries it, and `MOD⇧R` is how you go to the cards once it says they are
 *  there.
 *
 *  @param {number | null} number
 *  @param {HTMLButtonElement | null} [btn]
 */
export async function startSession(number, btn) {
  if (reviewState.busy || number === null) return;
  reviewState.busy = true;
  if (btn) btn.disabled = true;
  // Restored on a refusal rather than forced to `intake`: this is reached from the
  // rail as well now, where there is no intake screen behind it and leaving the
  // overlay on `reading` with no session would greet the next `MOD⇧R` with a
  // progress screen for a read that never started.
  const was = reviewState.screen;
  reviewState.screen = 'reading';
  try {
    const r = await call(`/api/pr/${number}/review-session`);
    adoptTriage(number, r.session);
    // Put the rail and the pane behind the overlay on the session that is about to
    // ask for permissions, so closing the overlay lands on it rather than on
    // whatever you happened to be looking at when you started the review.
    setPendingSelect(r.session);
    toast(`reading #${number}`);
  } catch (e) {
    toast(reason(e), true);
    reviewState.screen = was;
  } finally {
    if (btn) btn.disabled = false;
    reviewState.busy = false;
  }
  renderReview();
}

/** Take up a triage pass the rail just started, without opening the overlay.
 *
 *  The bar draws from `reviewState`, so something has to put the PR and the
 *  session there. `reviewTick` does it on its own for a pass already running when
 *  the page loaded; this is the same two fields, set at the moment the run starts,
 *  so the bar is up before the first snapshot carrying it arrives. */
function adoptTriage(/** @type {number | null} */ pr, /** @type {string} */ session) {
  reviewState.pr = pr;
  reviewState.session = session;
  reviewState.screen = 'reading';
  reviewState.proposalsLoaded = false;
  reviewState.decisionsSent = false;
  reviewState.data = null;
}

/** Driven every websocket tick (from app.js). Watches the session's ask and moves
 *  the overlay between phases — the ask is the whole signal, so there is no polling
 *  of `/review` and no second source of truth. */
function reviewTick() {
  // Not gated on the overlay being open. The bar reports the phase from wherever you
  // are, so the phase has to go on advancing while you are looking at another
  // session — otherwise coming back would show you the screen you left rather than
  // the one the review has reached.
  /* **A restart forgets the review; the daemon does not.** `reviewState` lives in
     the page, so restarting the app left a resumed and still-working review with no
     bar, no `MOD⇧R`, and no way back but hunting for its session in the rail.
     `auto_resume` brings that session back with its `Pass` intact, and the pass is
     the whole record needed to pick the thread up: command `review`, with the PR
     on it.

     Only the session you are looking at. That is the one the bar would draw for
     anyway — `renderBar` refuses to caption another session's pane — and it is the
     one question with a single answer when two reviews are in flight. */
  if (!reviewState.session && selected) {
    const s = (snap.sessions || []).find((x) => x.id === selected);
    const k = s && s.alive ? s.pass : null;
    if (s && k && k.command === 'review') {
      reviewState.pr = k.pr;
      reviewState.session = s.id;
      reviewState.proposalsLoaded = false;
      reviewState.screen = 'reading';
    }
  }
  if (!reviewState.session) return;
  const ask = sessionAsk();

  // The decision ask appears only after the session has posted its proposals, so it
  // is the proof they are ready. Fetch them once, then show the cards.
  //
  // **Card 1, not a tally of what it found.** That tally was a screen you read once
  // and pressed through, and it described the same queue the strip above every card
  // already draws. The first thing to decide is the first thread.
  if (askHasValue(ask, 'decisions') && !reviewState.proposalsLoaded) {
    reviewState.proposalsLoaded = true;
    reviewState.screen = 'card';
    void loadReview(reviewState.pr);
    return;
  }
  // The session ended.
  const s = (snap.sessions || []).find((x) => x.id === reviewState.session);
  if (s && s.alive) return;

  // Handed the checks on. The review's last act is `/handoff`, which ends the
  // session and lets its exit start a `fix-pr` run — so the work is somewhere else
  // now, and this overlay is the wrong frame for it. Give way to the flow that owns
  // watching a PR: close, and land on the run's pane the way the `fix` button does.
  // Not a screen of our own reporting on someone else's run, which is how you end up
  // with two places to look and one of them a version behind.
  if (s && s.handed_off) {
    const run = (snap.automation || {})[String(reviewState.pr)];
    // The run does not exist yet — the daemon is still cutting its worktree. Hold
    // the screen we are on rather than showing the report we are about to replace.
    if (!run || run.state !== 'running' || run.session === reviewState.session) return;
    const pr = reviewState.pr;
    // Only if you were watching this. A review you left running in the background
    // finishing is not a reason to take the centre pane away from whatever you went
    // off to do — the rail's `fixing` chip is how that one reaches you.
    const watching = reviewState.open || selected === reviewState.session;
    finishReview();
    if (watching) setPendingSelect(run.session);
    toast(`review done · fix-pr is watching #${pr}`);
    return;
  }

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
     goes with it: every screen it has left belongs to a session, so there is nothing
     to fall back to and an empty frame is not an answer. */
  reviewState.session = null;
  reviewState.proposalsLoaded = false;
  reviewState.screen = 'card';
  closeReview();
}

/** Route the session flow's own screens. */
function renderSessionReview(/** @type {HTMLElement} */ root) {
  if (reviewState.screen === 'reading') return rvReading(root);
  if (reviewState.screen === 'report') return rvSessionReport(root);
  if (reviewState.decisionsSent) return rvChanging(root);
  if (!reviewState.data || !reviewState.data?.proposals) return rvReading(root);
  (/** @type {Record<string, (root: HTMLElement) => void>} */
    ({ card: rvCard, final: rvFinal }))[
    reviewState.screen === 'final' ? 'final' : 'card'
  ](root);
}

/** A phase the session owns and you only watch: what it is doing, and the one
 *  way out. Both such phases ask for permissions in the pane, so the pane has to
 *  be one gesture away. */
function waitScreen(/** @type {HTMLElement} */ root, /** @type {string} */ eyebrow, /** @type {string} */ big, /** @type {string | null} */ para) {
  root.appendChild(rvHead(reviewState.data?.title || 'review'));
  const mid = el('div', 'mid');
  mid.appendChild(el('div', 'eyebrow', eyebrow));
  mid.appendChild(el('div', 'big', big));
  if (para !== null) mid.appendChild(el('p', null, para));
  const row = el('div');
  row.style.cssText = 'display:flex;gap:8px;margin-top:6px';
  row.appendChild(headBtn('go to the pane', 'go', () => { closeReview(); setSelected(reviewState.session); }));
  mid.appendChild(row);
  root.appendChild(mid);
}

/** The read phase: the session is reading, nothing to decide yet.
 *
 *  **Not reachable from a triage pass**, which is the flow the rail's `resolve`
 *  starts: `busyOnItsOwn` shuts every door into the overlay while one is running,
 *  because a full window saying somebody else is working is worse than the one
 *  line the bar already carries, and it covers the pane the agent asks its own
 *  questions in. This is the overlay session's flow (`commands/review-session.md`),
 *  which stays open across its own read phase and has nowhere else to say so. */
function rvReading(/** @type {HTMLElement} */ root) {
  waitScreen(root, 'the session is reading the threads', 'Reading…',
    'The cards open here when it is done. Permission prompts appear in the session’s pane.');
}

/** Between the decision submit and the post-go ask: the session is writing code. */
function rvChanging(/** @type {HTMLElement} */ root) {
  waitScreen(root, 'the session is making the changes you picked', 'Applying…',
    'It writes the code for each solution you chose, runs the repo’s checks, amends '
    + 'the owning commit and pushes, then posts your replies. Answer any permission '
    + 'prompts in the session’s pane. Nothing more is asked of you.');
}


/** Nothing more to do: the session finished. */
function rvSessionReport(/** @type {HTMLElement} */ root) {
  root.appendChild(rvHead('done'));
  const q = queue();
  const out = outward(q);
  const skipped = q.filter((x) => reviewState.skipped[x.t.id]).length;

  const mid = el('div', 'mid');
  /* What you authorised, counted — not "read its pane for the detail", which sent
     you to a terminal to find out what the app had just done on your behalf. Phrased
     as what was sent rather than what GitHub accepted, because the overlay watched
     the session end and did not watch the API answer: claiming a result it cannot
     see is the one thing a success message must not do. */
  const said = saidCounts(out);
  mid.appendChild(el('div', 'big', said ? `Sent ${said}.` : 'Finished.'));
  mid.appendChild(el('p', null,
    (skipped ? `${skipped} thread${skipped === 1 ? '' : 's'} skipped and still open. ` : '')
    + 'The commit and what it actually wrote are in the pane.'));
  root.appendChild(mid);
  root.appendChild(rvActs([actBtn('close', 'pri', () => finishReview())]));
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
    /** @type {{ thread_id: string, stance: string, solution: string, reply: string, note?: string }} */
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
async function answerSession(/** @type {string} */ value, /** @type {any} */ payload) {
  const ask = sessionAsk();
  if (!ask) { toast('the session is not waiting on anything just now', true); return false; }
  try {
    await call(`/api/session/${reviewState.session}/answer`, {
      ask: ask.id, answer: value, text: JSON.stringify(payload),
    });
    return true;
  } catch (e) {
    toast(reason(e), true);
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
  /* **The session that read them is the one that applies them**, and there is no
     longer a second pass to hand them to. This used to branch: a live session
     answered its ask, and a finished headless triage started a resolve run over the
     same picks instead. The triage pass and the run are both gone, so a set of
     decisions with nothing waiting for them is a review whose session ended — which
     is a sentence, not a fallback.

     The ask is the test rather than the session record, because it is the thing that
     is actually true at this moment. */
  if (!sessionAsk()) {
    return toast('the session that read these threads is gone — start the review again', true);
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
 *  it also carries the thread bodies, and it is deliberately explicit rather than a
 *  side effect of a tick. */
async function loadReview(/** @type {number | null} */ pr) {
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
      toast('the branch moved — decisions cleared, re-read the cards');
    }
    reviewState.head = base;

    renderReview();
  } catch (e) {
    toast(reason(e), true);
    if (!reviewState.data) closeReview();
  }
}

/** The live session answering this PR, whichever window started it. */
function liveReviewSession(/** @type {number | null} */ pr) {
  return (snap.sessions || []).find((x) => x.alive
    && x.pass && x.pass.command === 'review' && x.pass.pr === pr) || null;
}

/** Can this window pick up that session where it stands?
 *
 *  Only before it has been handed a decision set, because the decisions live in
 *  this SPA and nowhere else: adopting a session mid-change would face the post
 *  screen with empty picks, and post the recommended reply for every thread —
 *  including the ones the other window skipped. So: no ask yet (still reading),
 *  or the decision ask still open. Anything later is watched from its pane. */
function adoptable(/** @type {import('../snapshot').SessionView | null | undefined} */ s) {
  if (!s) return false;
  const i = s.interaction;
  return !i || (!i.answer && i.options.some((/** @type {import('../snapshot').InteractionOption} */ o) => o.value === 'decisions'));
}

async function openReview(/** @type {number | null} */ pr) {
  // Two overlays at the same z-index would stack; the diff viewer goes first.
  if (Diff.state.open) void Diff.close();
  if (reviewState.pr !== pr) {
    reviewState.picks = {};
    reviewState.skipped = {};
    reviewState.drafts = {};
    reviewState.notes = {};
    reviewState.editing = {};
    reviewState.head = null;
    reviewState.i = 0;
    reviewState.screen = 'card';
    reviewState.data = null;
    reviewState.session = null;
    reviewState.proposalsLoaded = false;
    reviewState.decisionsSent = false;
    // A session for this PR may already be running: the overlay was closed and
    // another PR opened in between, or the page reloaded. Adopt it rather than
    // leaving it mid-ask with nothing able to answer it. Its phase comes from the
    // ask, so `tick` sorts out which screen this is.
    const live = liveReviewSession(pr);
    if (live && adoptable(live)) {
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

/** DEV-ONLY. Render the card and the approval page against canned data, with no
 *  daemon fetch — so the UI can be clicked while the GitHub fixture is blocked on
 *  CI. Reached only from `/review-preview`; nothing in the app calls it.
 *
 *  **The session id is a lie, and it has to be.** Every screen left belongs to a
 *  session, so `renderReview` draws nothing without one — this used to set `null`
 *  and the preview page went blank the moment the intake screen was deleted. The
 *  id matches no session in the snapshot, which is exactly what keeps `send` inert:
 *  `submitDecisions` tests the live ask, finds none and refuses. */
export function preview(/** @type {any} */ data) {
  reviewState.picks = {};
  reviewState.skipped = {};
  reviewState.drafts = {};
  reviewState.notes = {};
  reviewState.editing = {};
  reviewState.head = data.proposals?.base_sha || null;
  reviewState.i = 0;
  reviewState.session = 'preview';
  reviewState.proposalsLoaded = false;
  reviewState.decisionsSent = false;
  reviewState.pr = data.pr_number ?? 0;
  reviewState.data = data;
  reviewState.open = true;
  reviewState.screen = 'card';
  $('rvoverlay').classList.add('on');
  renderReview();
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
    i > reviewState.i && !isDecided(x));
  if (next >= 0) reviewState.i = next;
  else if (q.every(isDecided)) reviewState.screen = 'final';
  else reviewState.i = Math.min(reviewState.i + 1, q.length - 1);
  renderReview();
}

/** j / k. Both ways on purpose: a later thread often changes what an earlier one
 *  deserves as an answer, and decisions stage rather than post, so revising one
 *  is free right up to the final action. */
function moveCard(/** @type {number} */ delta) {
  const q = queue();
  if (!q.length) return;
  reviewState.i = Math.max(0, Math.min(q.length - 1, reviewState.i + delta));
  reviewState.screen = 'card';
  renderReview();
}

function reviewKey(/** @type {KeyboardEvent} */ e) {
  if (e.key === 'Enter') {
    // A focused control owns Enter. Without this the global handler hijacks it to
    // accept the card, so a keyboard user who tabbed to an option button pressed
    // Enter and staged the *recommendation* instead of the row they were on. Let
    // the button activate natively; the re-render then drops focus back to body,
    // so the next Enter accepts as before.
    const on = document.activeElement;
    if (on && (on.tagName === 'BUTTON' || on.tagName === 'A')) return false;
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
    const left = q.filter((x) => !isDecided(x)).length;
    return left
      ? { tone: 'attn', what: `${left} of ${q.length} threads waiting on you` }
      : { tone: 'attn', what: `${q.length} threads decided · not sent yet` };
  }
  // The session is carrying out what you decided, and it says how far it has got
  // in its own pane rather than through the daemon.
  if (reviewState.decisionsSent) return { tone: 'work', what: 'applying · writing the code' };
  return { tone: 'work', what: 'review · reading the threads' };
}

/** Draw the bar, or take it away.
 *
 *  Two conditions, and both are about *where you are standing* rather than what the
 *  review is doing. Only while the overlay is *closed*: open, the cards are the
 *  report. And only on the review's own pane — the bar sits over `.termwrap`, which
 *  every session shares, so without this it captioned whichever pty you happened to
 *  be looking at with what a different session was up to.
 *
 *  Rendered from `app.js`'s tick like every other pane, and into a host that lives
 *  outside `#rvoverlay` — `renderReview` replaces that element's children on every
 *  snapshot and would tear a live node out from under itself once a second. */
const barDrawn = { sig: null, name: 'review-bar' };

function renderBar() {
  const host = $('rvbar');
  const mine = !!selected && selected === reviewState.session;
  const st = reviewState.open || !mine ? null : barState();
  /* **Rebuilt only when it would come out different**, like every other pane.
     This one had no guard, and the review it captions is exactly when the daemon
     pushes hardest: an agent taking a turn is ~7 snapshots a second, and each one
     replaced the bar's own children. `:hover` is re-targeted on every rebuild, so
     the `open` button strobed under the pointer, and a click whose mousedown and
     mouseup land on two different nodes is never delivered — the button that
     flickers and does not open. The class toggle below is idempotent, so skipping
     it costs nothing. */
  if (unchanged(barDrawn, [st, reviewState.pr])) return;
  // The pane is Claude's while the agent has the turn, so it reads as Claude's:
  // dimmed, with the bar at full strength over it. Only on `work` — the lift back
  // to normal is itself the signal that the turn came back to you.
  $('termwrap').classList.toggle('rvwork', !!st && st.tone === 'work');
  if (!st) { host.hidden = true; host.replaceChildren(); return; }

  host.replaceChildren();
  host.className = `rvbar ${st.tone}`;
  host.appendChild(el('span', 'dot'));
  host.appendChild(el('span', 'k', `REVIEW · PR ${reviewState.pr}`));
  host.appendChild(el('span', 'what', st.what));
  const go = el('button', 'go', `open · ${MOD_LABEL}\u21e7R`);
  go.onclick = () => openReview(reviewState.pr);
  host.appendChild(go);
  host.hidden = false;
}

/** What Ctrl+Enter does: press the final screen's send button. Through the button
 *  rather than straight to `sendBatch`, or the shortcut sends a batch the button
 *  itself refuses — which it did until now. Off the final screen, nothing. */
function sendFromKeyboard() {
  if (reviewState.screen !== 'final') return;
  const send = /** @type {HTMLButtonElement|null} */ (
    $('rvoverlay').querySelector('.acts .act.warm'));
  if (send && !send.disabled) send.click();
}

// The public surface. Everything else above is private by construction now,
// which is the point: the rail and the keymap reach the overlay through these
// or not at all.

export {
  reviewState as state, openReview as open, closeReview as close, adoptTriage as adopt,
  reviewKey as key, reviewTick as tick, renderBar as bar, sendFromKeyboard as send,
};
