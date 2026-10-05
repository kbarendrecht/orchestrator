// The built-in review queue lists every open PR somebody else wrote, and says
// which ones asked for your review.
//
// It used to list only the asked ones (`review-requested:@me`), and on a repo
// where reviews are picked up rather than assigned that was two rows out of
// thirty. The pane's `asked` filter narrows it back, from the `requested` field
// each row carries, so that field is what this holds — the filter itself is a
// page-side `filter` over it.

import assert from 'node:assert/strict'
import { until } from '../harness.mjs'

export const name = 'the built-in review queue lists every PR and marks the asked ones'

export const options = {
  repo: 'acme/monorepo',
  githubToken: 'e2e-token',
  pollSeconds: 30,
  // Empty is the built-in queue; the suite's default `true` is a command.
  reviewsCommand: [],
}

export async function run(t) {
  t.setPrs([], 'e2e-viewer', {
    reviews: [
      { number: 201, title: 'asked of you', asked: 'me', created_at: '2026-03-01T00:00:00Z' },
      { number: 202, title: 'asked of your team', asked: 'team', created_at: '2026-02-01T00:00:00Z' },
      { number: 203, title: 'nobody asked, oldest of all', created_at: '2026-01-01T00:00:00Z' },
      { number: 204, title: 'nobody asked', created_at: '2026-04-01T00:00:00Z' },
    ],
  })
  await t.api('POST', '/api/reviews/refresh')
  const q = await until('the built-in queue to answer', async () => {
    const rv = (await t.state()).reviews
    return rv?.state === 'ok' && rv.actionable.length === 4 ? rv : null
  }, { context: async () => JSON.stringify((await t.state()).reviews) })

  // Requested first, oldest first within each: #203 is the oldest and still
  // comes after both requests, because nobody asked you about it.
  assert.deepEqual(q.actionable.map((r) => r.number), [202, 201, 203, 204])
  const by = (n) => q.actionable.find((r) => r.number === n)
  assert.equal(by(201).requested, true)
  assert.equal(by(201).prio, 2, 'named yourself is amber')
  assert.equal(by(202).requested, true)
  assert.equal(by(202).prio, 3, 'a team request is grey')
  assert.equal(by(203).requested, false)
  assert.equal(by(203).prio, 5, "nobody asked is the contract's other")
}
