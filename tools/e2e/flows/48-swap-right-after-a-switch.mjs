// Swapping straight after the agent switched branch: the conversation still goes.
//
// The swap picks who travels by `Session::branch`, and a record follows its tree's
// branch only when a reconcile runs — every two seconds, off the HEAD poller. A
// swap inside that gap read the branch the session started on, matched nobody, and
// moved the branch into main with its conversation left in the worktree. The swap
// reconciles both trees before it reads, and this is the window that proves it.

import assert from 'node:assert/strict'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'swap right after a branch switch carries the conversation'

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(session)
  await t.transcribed(session)
  const dir = t.worktreePath('invoice')

  // What an agent's `git checkout -b` does, and no wait after it.
  git(dir, ['switch', '-qc', 'feature/quick'])
  const r = await t.api('POST', '/api/workspace/invoice/swap-main', { session })
  assert.equal(branchOf(t.repo), 'feature/quick')
  assert.equal(r.into_main?.error ?? null, null, `main got ${JSON.stringify(r.into_main)}`)
  assert.equal(r.into_main?.session, session, 'the conversation stayed behind its branch')

  await until('the conversation to arrive in main', async () => {
    const s = await t.session(session)
    return s?.workspace === 'main' && s.alive && s.branch === 'feature/quick'
  })
}
