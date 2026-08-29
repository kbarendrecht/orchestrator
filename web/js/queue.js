// The review queue pane.
//
// The first seam to become a real module: five names, one of which leaves. What
// it needs from elsewhere is now an import list rather than an assumption about
// what happens to be in scope.
import { $, el, snap, refreshButton, duration, sinceSnap } from './core.js';

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
  head.setAttribute('aria-expanded', String(showReviews));
  head.appendChild(el('span', 'caretr', '\u203a'));
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
  // How fresh the queue is, the same line the PR pane shows. Hidden mid-poll so
  // it does not flicker to "0s ago" and back.
  if (snap.reviews_age_ms != null && !snap.reviews_polling) {
    count.appendChild(el('span', 'prage', ` · ${duration(sinceSnap(snap.reviews_age_ms))} ago`));
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
     * somebody's release waiting on you. */
    const dot = r.prio <= 1 ? ' prio' : r.needs_re_review ? ' blocked' : '';
    a.appendChild(el('span', 'dot' + dot));
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

export { renderReviews as render };
