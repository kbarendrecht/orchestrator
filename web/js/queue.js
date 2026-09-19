// The review queue pane.
//
// The first seam to become a real module: five names, one of which leaves. What
// it needs from elsewhere is now an import list rather than an assumption about
// what happens to be in scope.
import { $, activeCheckout, bandOf, call, caret, CHECKOUTS, CHROME, clock, compactAge, confirmBox, el, icon, keyActivate, QUEUE_MAX, refreshButton, safeHref, snap, toast, unchanged } from './core.js';

/** Above this many, the press asks first.
 *
 *  **Ask rather than cap.** A hard cap opens the first N and drops the rest with
 *  nothing said, which is the silent truncation this codebase keeps writing rules
 *  against. The question states the number before it happens, and the head already
 *  shows that number two elements to the left, so it is not a surprise.
 */
const ASK_ABOVE = 8;

/** Open every actionable review, one press.
 *
 *  **Grey, like the refresh beside it.** Nothing in this header is coloured, and
 *  amber means "needs you" everywhere else in this UI (§9) — a control wearing it
 *  would be claiming a state. The count is not on the glyph either: it is in the
 *  tooltip, the same way the refresh says "Refresh now", and the head spells it
 *  out in words two elements to the left.
 *
 *  **Absent in a browser tab**, and that is the whole of the popup-blocker answer.
 *  A tab returns before `app.js`'s external-link handler, so the rows open
 *  natively and this button would mean one `window.open` per row: the first lands
 *  inside the gesture and the browser drops the rest *without telling the page*.
 *  One press, one tab, four reviews lost. A button that cannot work is worse than
 *  no button, and ⌘-clicking the rows still does the job there.
 */
function openAllButton(/** @type {import('../repo').Review[]} */ rows) {
  // Drawn, not typed — the same reason the refresh glyph beside it is an SVG.
  const btn = icon('openall', 1.5, 'M6.2 2.5h7.3v7.3', 'M13.5 2.5 7 9', 'M11 9.6v3.9H2.5V5h3.9');
  btn.setAttribute('role', 'button');
  // Dead rather than gone on an empty queue, so the header keeps its shape between
  // polls instead of the refresh jumping sideways every time the last row clears.
  const none = !rows.length;
  btn.setAttribute('aria-disabled', String(none));
  const what = none ? 'No reviews to open' : `Open all ${rows.length} reviews`;
  btn.title = what;
  btn.setAttribute('aria-label', what);
  if (none) return btn;
  keyActivate(btn);
  btn.onclick = async (e) => {
    e.stopPropagation();               // the header's own click toggles the pane
    if (rows.length > ASK_ABOVE && !await confirmBox(
      `Open ${rows.length} reviews in your browser?`, { ok: 'Open', danger: false },
    )) return;
    try {
      // One call, not one per row: `api::open_urls` says why the count has to come
      // back from one place.
      const r = await call('/api/open-all', { urls: rows.map((x) => x.url) });
      if (r.opened < rows.length) toast(`opened ${r.opened} of ${rows.length}`, true);
    } catch (err) {
      toast(err instanceof Error ? err.message : String(err), true);
    }
  };
  return btn;
}

let showReviews = true;
let showBlockedReviews = false;

/* **The pane is rebuilt only when it would come out different.**
 *
 * The daemon pushes a whole snapshot on every state change — `notify` is called
 * from about seventy places, and three running sessions measured at ~7 pushes a
 * second — and `render()` calls this on each one. `replaceChildren` then destroys
 * the row under the pointer seven times a second: `:hover` is re-targeted on
 * every rebuild so the highlight strobes, and a click whose mousedown and mouseup
 * land on two different elements is never delivered, which is the review row that
 * does not open when you click it. The rail's waiting clock was moved off
 * `Rail.render()` for this same reason, and this is the same fault one pane over. */
const headDrawn = { sig: null, name: 'review-head' };
const listDrawn = { sig: null, name: 'review-list' };

/* **The head and the list are guarded apart, because a refresh only moves the
   head.** A poll flips `reviews_polling` on and then off again, and the counter
   it finishes on is what stops the spinner — three pushes that change the head
   and, when the queue came back the same, nothing at all in the list. Guarded
   together, each of those took the row out from under the pointer, which is a
   review link that does not open *while the queue is reloading*, exactly when you
   are most likely to be reaching for one. */

/** Why this row is in your queue, when there is a reason worth the width. */
function reviewReason(/** @type {import('../repo').Review} */ r) {
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
  const rv = snap.reviews;

  /* **Nothing at all when there is no queue to show, and the daemon decides that.**
     `ReviewState::Off` means no `reviews_command` *and* no GitHub repository — the
     one case where the pane would be chrome around a question nobody asked. It was
     `!snap.repos?.upstream` for one commit, which was wrong in a way the daemon
     could have said: `reviews::fetch` runs a configured command *before* it looks
     at the repo, so a checkout with its own queue and a non-GitHub remote had its
     rows deleted from the window while the daemon went on fetching them.

     The block is emptied before it is hidden, and its band cleared with it: two
     checkouts open and the pane would otherwise keep the other one's rows and the
     other one's colour behind a `hidden` that only stops it being read. */
  if (rv && rv.state === 'off') {
    // Once, not on every render. `render()` reaches this several times a second
    // while an agent works, and the six writes below change nothing after the
    // first — including two that null the paint guards this module exists for.
    if (!block.hidden) {
      head.replaceChildren();
      list.replaceChildren();
      block.classList.remove('rv-of-checkout');
      block.style.removeProperty('--band');
      headDrawn.sig = null;
      listDrawn.sig = null;
      block.hidden = true;
    }
    return;
  }
  block.hidden = false;

  block.classList.toggle('closed', !showReviews);
  /* The same band the PR pane wears, for the same reason it wears it: this pane
     answers for one checkout — the one you are in — and it sits below a scroller,
     so the coloured block that would have said which has scrolled away. The two
     panes are a pair (yours bottom-left, your colleagues' bottom-right) and one
     of them saying which checkout it means while the other does not is the pair
     disagreeing about a question they both answer.
     On the right edge rather than the left, which is the one thing that differs:
     each band runs down the outer edge of its own column. */
  const band = CHECKOUTS.length > 1 ? bandOf(activeCheckout().path) : null;
  block.classList.toggle('rv-of-checkout', !!band);
  if (band) block.style.setProperty('--band', `var(--co-${band})`);
  else block.style.removeProperty('--band');
  /* Whether there is an age to show, not what it says: the text itself is a
     `data-clock` node that `tick` rewrites in place once a second, and
     `paintSig` drops `reviews_age_ms` for exactly that reason.
     `snap.reviews_poll` earns its place: `refreshButton` clears the spinner it
     started when that counter moves, and it can only do that when it is built. */
  const hasAge = snap.reviews_age_ms != null && !snap.reviews_polling;
  const drawHead = !unchanged(headDrawn, [showReviews, rv,
    snap.reviews_poll ?? 0, !!snap.reviews_polling, hasAge]);
  const drawList = !unchanged(listDrawn, [showReviews, showBlockedReviews, rv]);
  if (!drawHead && !drawList) return;
  if (drawHead) head.replaceChildren();
  if (drawList) list.replaceChildren();
  head.setAttribute('aria-expanded', String(showReviews));
  if (drawHead) {
    head.appendChild(caret());
    head.appendChild(el('span', 'eyebrow', 'Review queue'));
  }
  const count = el('span', 'rvcount');

  const refresh = refreshButton('review', snap.reviews_poll ?? 0, '/api/reviews/refresh',
    snap.reviews_polling);

  if (!rv || rv.state !== 'ok') {
    /* Never an empty queue: a broken command reads as broken (§6b). Startup is not
       broken and says so differently.

       **`off` is not among these any more.** It used to draw a head and a line
       reading "no GitHub repository for this checkout"; the whole pane is now
       simply absent in that case — the block above returns before this — because a
       pane explaining that it does not apply is the chrome a fresh install reads as
       a fault. The compiler agrees: `rv.state` cannot be `'off'` here. */
    const pending = !rv || rv.state === 'pending';
    // Only a real fault gets the red `f`; pending is neutral.
    const label = pending ? 'polling…' : 'unavailable';
    // `reason` belongs to the degraded variant alone; the others simply have none.
    const why = rv && 'reason' in rv ? rv.reason : '';
    if (drawHead) {
      count.appendChild(el('span', pending ? null : 'f', label));
      head.appendChild(count);
      head.appendChild(refresh);
      head.title = why;
      head.onclick = () => { showReviews = !showReviews; renderReviews(); };
    }
    if (drawList) {
      list.appendChild(el('div', 'fempty', pending
        ? 'waiting for the first poll'
        : `reviews unavailable\n${why.slice(0, 160)}`));
    }
    return;
  }

  const rows = rv.actionable || [];
  const blocked = rv.blocked || [];
  if (drawHead) {
    /* The number alone. `waiting` named what the rows under it already are, and
       the dot that joins it to the age was doing the joining either way — so the
       word cost a third of the header's text and said nothing. `clear` stays,
       because a bare `0` is not an answer. */
    count.appendChild(el('span', rows.length ? 'n' : null,
      rows.length ? String(rows.length) : 'clear'));
    // The same line, and the same clock, the PR pane shows. Hidden mid-poll so it
    // does not flicker to "0s ago" and back.
    if (hasAge) count.appendChild(clock('prage', snap.reviews_age_ms, ' ago', ' · '));
    head.appendChild(count);
    if (CHROME !== 'none') head.appendChild(openAllButton(rows));
    head.appendChild(refresh);
    head.onclick = () => { showReviews = !showReviews; renderReviews(); };
  }
  if (!drawList) return;

  // The file-count column only earns its width once the source emits it.
  const anyFiles = [...rows, ...blocked].some((r) => r.changed_files != null);

  const rowFor = (/** @type {import('../repo').Review} */ r, /** @type {boolean} */ dim) => {
    // Rows are anchors, so ⌘-click and copy-link behave, and the browser
    // already holds the GitHub session (§6b).
    const a = el('a', 'rvrow' + (dim ? ' dim' : ''));
    // The conversation tab: what a reviewer needs first is the description and
    // what has already been said, not a wall of diff with none of the context.
    a.href = safeHref(r.url);
    a.target = '_blank';
    a.rel = 'noreferrer';
    /* Grey unless it is a re-review: the source only sets `needsReReview` for
     * rows in *your* queue, so it is the one thing here that is waiting on you
     * rather than on a colleague. Amber is the legend's "needs you" (§9).
     * It cannot tell a personal re-request from a team one — `prio` splits that
     * only for first requests.
     * A `prio` or `prio stopper` label outranks both: red, because that queue is
     * somebody's release waiting on you.
     *
     * **A blocker is red too, and it was grey.** `conflicts` or `failing checks`
     * came through as a word in the reason column beside a dot that looked
     * exactly like a healthy row's, so the one thing you can see from across the
     * pane said nothing. Same red as `prio` by its own name rather than by
     * reusing that class: they mean different things — one is urgent *for you*,
     * this one is broken and not yours — and a palette that ever splits them
     * should not have to find the call sites first. Which blocker it is stays in
     * the reason column and on the dot's own tooltip, because red cannot spell
     * "conflicts". */
    const blocked = r.blockers && r.blockers.length;
    /* **Amber is `prio === 2`: somebody named you.** The built-in queue only ever
       emits 2 or 3, so this is the whole of its colour — amber when the request
       carries your name, grey when it went to a team you happen to be in. That is
       the legend's "needs you" (§9) meaning what it says: a team request is
       waiting on the team.
       The two arms above it survive for a configured command, which may still
       emit the label ranks and a re-review; the built-in never does. */
    const dot = r.prio <= 1 ? ' prio'
      : blocked ? ' bad'
        : r.needs_re_review ? ' blocked'
          : r.prio === 2 ? ' attn' : '';
    a.appendChild(el('span', 'dot' + dot, undefined, blocked ? r.blockers.join(', ') : undefined));
    // Age, not the PR number: how long it has waited is what tells you to pick
    // it up. The whole row already links to the PR, so the number earns nothing.
    const age = el('span', 'num', compactAge(r.age_hours || 0));
    age.title = `#${r.number}`;
    a.appendChild(age);
    a.appendChild(el('span', 'ttl', r.title, r.title));
    a.appendChild(el('span', 'who', r.author, r.author));
    // File count stands in for review cost — 37 files is a different
    // commitment from 1 — but an empty column just steals width from the title.
    if (anyFiles) a.appendChild(el('span', 'fc', r.changed_files != null ? String(r.changed_files) : '·'));
    const why = reviewReason(r);
    if (why) a.appendChild(el('span', 'why', why));
    return a;
  };

  // Bounded like the PR pane's, and for the same reason `QUEUE_MAX` gives: the
  // head above still counts every one.
  for (const r of rows.slice(0, QUEUE_MAX)) list.appendChild(rowFor(r, false));
  if (!rows.length) list.appendChild(el('div', 'fempty', 'Nothing waiting on you.'));

  // Blocked on conflicts or red checks: waiting on their author, not on you.
  // Sunk rather than dropped, because sometimes you still want to look — but
  // folded, so they do not pad the queue you actually work from.
  if (blocked.length) {
    const t = el('button', 'arctoggle');
    t.setAttribute('aria-expanded', String(showBlockedReviews));
    t.appendChild(caret());
    t.appendChild(el('span', null, `${blocked.length} not reviewable`));
    t.title = blocked.map((r) => `#${r.number} — ${r.blockers.join(', ')}`).join('\n');
    t.onclick = () => { showBlockedReviews = !showBlockedReviews; renderReviews(); };
    list.appendChild(t);
    if (showBlockedReviews) {
      for (const r of blocked.slice(0, QUEUE_MAX)) list.appendChild(rowFor(r, true));
    }
  }
}

export { renderReviews as render };
