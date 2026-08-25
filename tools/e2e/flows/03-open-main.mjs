// Opening main: no worktree to cut, and main is exclusive.
//
// Main is where the managed processes and the dev stack live, so occupancy *is*
// the lease — there is no second mechanism. That exclusivity is the whole of what
// this flow is for.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import { until } from '../harness.mjs'

export const name = 'open main'

export async function run(t) {
  const { session } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(session)

  const s = await t.session(session)
  assert.equal(s.workspace, 'main', 'a main session belongs to no worktree')
  assert.equal(fs.realpathSync(s.cwd), fs.realpathSync(t.repo), 'cwd is the checkout')
  assert.equal(s.has_transcript, true, 'the turn did not register as a conversation')

  // The claim is recorded on the workspace, not inferred from the session list.
  const main = await t.workspace('main')
  assert.equal(main.occupant, session, 'main did not record its occupant')

  // A second one is refused by name rather than quietly stacked.
  await assert.rejects(
    () => t.api('POST', '/api/session', { workspace: 'main' }),
    /already has a live session/,
  )

  // Closing it hands the claim back, or main is held forever by a session that
  // is gone — the failure mode the stale-watcher guard exists for. The wait is
  // the point rather than politeness: the claim is released by the exit watcher,
  // not by the kill returning.
  await t.api('POST', `/api/session/${session}/kill`)
  await until('main to come free', async () => (await t.workspace('main')).occupant == null)

  const { session: second } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(second)
  assert.notEqual(second, session)
  assert.equal((await t.workspace('main')).occupant, second)
}
