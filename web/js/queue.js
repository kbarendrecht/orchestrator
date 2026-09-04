// The review queue pane.
//
// The first seam to become a real module: five names, one of which leaves. What
// it needs from elsewhere is now an import list rather than an assumption about
// what happens to be in scope.
import { $, caret, compactAge, el, snap, refreshButton, duration, sinceSnap } from './core.js';

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
 * `Rail.render()` for this same reason, and this is the same fault one pane over.
 *
 * The signature deliberately leaves out the "· 3s ago" text, which is the one
 * thing here that really does change every second. Rebuilding the pane for it
 * would put the strobe back at 1Hz, so it is written into the span in place. */
let renderedSig = null;
/** The `· 3s ago` span while it is on screen, so the clock can tick without a
 *  rebuild. Null when the head does not show one. */
let ageSpan = null;

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
  const rv = snap.reviews;

  // How fresh the queue is. Computed up here because it is the one string that
  // changes without the pane changing, so it is also the one the signature omits.
  const ageText = rv && rv.state === 'ok' && snap.reviews_age_ms != null && !snap.reviews_polling
    ? ` · ${duration(sinceSnap(snap.reviews_age_ms))} ago`
    : null;
  /* `snap.reviews_poll` earns its place: `refreshButton` clears the spinner it
     started when that counter moves, and it can only do that when it is built. */
  const sig = JSON.stringify([showReviews, showBlockedReviews, rv,
    snap.reviews_poll ?? 0, !!snap.reviews_polling, ageText != null]);
  block.classList.toggle('closed', !showReviews);
  if (sig === renderedSig) {
    if (ageSpan) ageSpan.textContent = ageText || '';
    return;
  }
  renderedSig = sig;
  ageSpan = null;
  head.replaceChildren();
  list.replaceChildren();
  head.setAttribute('aria-expanded', String(showReviews));
  head.appendChild(caret());
  head.appendChild(el('span', 'eyebrow', 'Review queue'));
  const count = el('span', 'rvcount');

  const refresh = refreshButton('review', snap.reviews_poll ?? 0, '/api/reviews/refresh',
    snap.reviews_polling);

  if (!rv || rv.state !== 'ok') {
    // Never an empty queue: a broken command reads as broken (§6b). Startup and
    // "no such command here" are not broken, so they each say so differently.
    const pending = !rv || rv.state === 'pending';
    const off = rv && rv.state === 'off';
    // Only a real fault gets the red `f`; pending and off are neutral.
    const label = pending ? 'polling…' : off ? 'off' : 'unavailable';
    count.appendChild(el('span', pending || off ? null : 'f', label));
    head.appendChild(count);
    head.appendChild(refresh);
    // `reason` belongs to the degraded variant alone; the others simply have none.
    const why = rv && 'reason' in rv ? rv.reason : '';
    head.title = why;
    list.appendChild(el('div', 'fempty', pending
      ? 'waiting for the first poll'
      : off
        ? 'no review queue configured\nset `reviews_command` in config.json'
        : `reviews unavailable\n${why.slice(0, 160)}`));
    head.onclick = () => { showReviews = !showReviews; renderReviews(); };
    return;
  }

  const rows = rv.actionable || [];
  const blocked = rv.blocked || [];
  count.appendChild(el('span', rows.length ? 'n' : null,
    rows.length ? `${rows.length} waiting` : 'clear'));
  // The same line the PR pane shows. Hidden mid-poll so it does not flicker to
  // "0s ago" and back. Kept, so the clock ticks without rebuilding the pane.
  if (ageText != null) {
    ageSpan = el('span', 'prage', ageText);
    count.appendChild(ageSpan);
  }
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
    const dot = r.prio <= 1 ? ' prio' : blocked ? ' bad' : r.needs_re_review ? ' blocked' : '';
    a.appendChild(el('span', 'dot' + dot, null, blocked ? r.blockers.join(', ') : undefined));
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

  for (const r of rows) list.appendChild(rowFor(r, false));
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
    if (showBlockedReviews) for (const r of blocked) list.appendChild(rowFor(r, true));
  }
}

export { renderReviews as render };
