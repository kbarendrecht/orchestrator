#!/usr/bin/env node
// A review queue for the README's recording, and nothing else.
//
// **This is demo data, and it is the only fabricated thing in any of the GIFs.**
// Everything else in them is real: real daemons, real git worktrees, a real
// Claude Code writing the diffs the changed-files pane shows. The review queue
// cannot be, and the reason is structural rather than lazy.
//
// The built-in queue asks GitHub for
// `repo:<owner>/<name> is:open is:pr review-requested:@me`. GitHub will not let
// you request a review from yourself, and the fixture's second identity is
// `github-actions[bot]`, which **cannot be a requested reviewer** — the same wall
// `docs/fixture-pr.md` records against `rerequest()`. So a genuinely populated
// queue needs a second human account, which a recording script cannot conjure.
//
// It is wired in by pointing that checkout's `reviews_command` at this file, which
// is an ordinary documented setting — see `docs/demo.md`. Nothing here ships in a
// release, no default reads it, and the repository it describes is the throwaway
// fixture the rest of the recording already uses.
//
// The shape is `docs/reviews-json.md`'s `Queue`, camelCase, on stdout.
//
// The ranks are spread on purpose, because the pane's colours are part of what the
// picture is showing: `prio` reads red, a request that names you reads amber, a
// team request stays grey, and `needsReReview` says re-requested. One row sits in
// `blocked` so the pane's second half is not empty either.
// A command-line script: printing is its output.
/* eslint-disable no-console */

const REPO = 'kbarendrecht/orchd-fixture';
const hours = (n) => new Date(Date.now() - n * 3600_000).toISOString();

/** One entry, with the fields the daemon actually reads defaulted sensibly. */
const entry = (pr, { reviewers = 0, blockers = [], needsReReview = false, ageDays, prio }) => ({
  pr: {
    url: `https://github.com/${REPO}/pull/${pr.number}`,
    isDraft: false,
    mergeable: blockers.includes('conflicts') ? 'CONFLICTING' : 'MERGEABLE',
    labels: [],
    reviews: [],
    requestedReviewers: [],
    requestedTeams: [],
    headOid: null,
    checks: blockers.includes('failing checks') ? 'FAILURE' : 'SUCCESS',
    ...pr,
  },
  reviewers,
  blockers,
  needsReReview,
  ageDays,
  prio,
});

const queue = {
  forLogin: 'you',
  total: 6,
  skipped: 1,
  actionable: [
    entry(
      { number: 31, title: 'Guard reserve() against a concurrent oversell', author: 'dana', createdAt: hours(5) },
      { prio: 1, ageDays: 0, reviewers: 0 },
    ),
    entry(
      { number: 29, title: 'Validate the sku argument in receive()', author: 'mo', createdAt: hours(26) },
      { prio: 2, ageDays: 1, reviewers: 1 },
    ),
    entry(
      { number: 24, title: 'Rework the low-stock threshold', author: 'lee', createdAt: hours(50) },
      { prio: 4, ageDays: 2, reviewers: 2, needsReReview: true },
    ),
    entry(
      { number: 22, title: 'Bump node to 22 in CI', author: 'sam', createdAt: hours(73) },
      { prio: 3, ageDays: 3, reviewers: 1 },
    ),
  ],
  blocked: [
    entry(
      { number: 18, title: 'Split the stock map per warehouse', author: 'dana', createdAt: hours(120) },
      { prio: 2, ageDays: 5, reviewers: 0, blockers: ['conflicts'] },
    ),
  ],
  ownBlocked: [],
};

console.log(JSON.stringify(queue));
