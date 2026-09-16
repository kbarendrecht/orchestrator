// The spare pool: an unnamed create is handed a worktree that was already cut.
//
// **Asserted by identity, never by timing.** The saving is 4.4 seconds on the
// monorepo and nothing on a fixture repo of three files, so a flow that timed the
// create would assert on the machine rather than on the daemon — which is the
// shape `docs/traps/e2e.md` warns about. `snapshot.spare` names the tree the pool
// is holding, so the claim is provable: the session lands in *that* workspace, and
// the pool has moved on to another.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import { branchOf, until } from '../harness.mjs'

export const name = 'an unnamed create is handed the spare'

/** Wait for the pool to hold a tree, which is cut in the background at boot. */
const pooled = (t) =>
  until('the spare pool to fill', async () => {
    const s = await t.state()
    return s.spare?.length ? s.spare[0] : null
  })

export async function run(t) {
  const spare = await pooled(t)

  // It is a real worktree on its own branch, and nothing is in it.
  const dir = t.worktreePath(spare)
  assert.ok(fs.existsSync(dir), `the pool named ${spare} but cut no tree`)
  assert.equal(branchOf(dir), `worktree-${spare}`)
  const before = await t.state()
  assert.equal(
    before.sessions.filter((s) => s.workspace === spare).length,
    0,
    'a spare has no session',
  )

  // The claim: an unnamed create lands in exactly that workspace.
  const { session } = await t.api('POST', '/api/worktree', {})
  await t.settled(session)
  const s = await t.session(session)
  assert.equal(s.workspace, spare, 'the create did not take the spare')
  assert.equal(fs.realpathSync(s.cwd), fs.realpathSync(dir))

  // And the pool refills with a different tree, so the next create is fast too.
  const next = await until('the pool to refill', async () => {
    const now = await t.state()
    return now.spare?.length && now.spare[0] !== spare ? now.spare[0] : null
  })
  assert.ok(fs.existsSync(t.worktreePath(next)), 'the refill cut no tree')

  /* A *named* create never takes the spare: the directory name is the workspace
     name, so there is nothing to hand over. The pool must be untouched by it. */
  const { session: named } = await t.api('POST', '/api/worktree', { name: 'ledger' })
  await t.settled(named)
  const after = await t.state()
  assert.equal(after.spare[0], next, 'a named create must not consume the spare')
}
