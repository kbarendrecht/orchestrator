// The refusals that stand between a teardown and losing work.
//
// Teardown removes a checkout, so every one of its preflight checks is the last
// thing between you and work that exists nowhere else. A guard nothing exercises
// is one you find out about on the day it fails open, and the happy path — which
// flow 02 already drives on the delegated layout — proves nothing about them.
//
// This is also the daemon-cut layout's teardown, where the tree carries no
// `claude --worktree` lock and the removal is a plain one.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { git, until } from '../harness.mjs'

export const name = 'teardown refuses rather than lose work'

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(session)
  const dir = t.worktreePath('invoice')

  // 1. A live agent in it. Read off the process table rather than in-memory state,
  //    because a crashed daemon is exactly when a stale answer would delete a
  //    worktree with an agent still writing into it.
  await refuses(t, 'no live session')

  await t.api('POST', `/api/session/${session}/kill`)
  await until('the session to stop being live', async () =>
    (await t.session(session))?.alive === false)

  // 2. Uncommitted work. Untracked counts: it is the kind that exists in exactly
  //    one place, and a removal takes it with the directory.
  fs.writeFileSync(path.join(dir, 'scratch.txt'), 'not committed anywhere\n')
  await refuses(t, 'clean tree')
  fs.rmSync(path.join(dir, 'scratch.txt'))

  // 3. Commits with no counterpart on origin — the case the check exists for, since
  //    the branch dies with the worktree and takes them along.
  fs.writeFileSync(path.join(dir, 'work.txt'), 'real work\n')
  git(dir, ['add', '-A'])
  git(dir, ['commit', '-qm', 'work that exists nowhere else'])
  await refuses(t, 'nothing unpushed')

  // Push it and the objection goes away — the work is safe somewhere else now.
  git(dir, ['push', '-q', 'origin', 'worktree-invoice'])
  const pf = await t.api('GET', '/api/workspace/invoice/preflight')
  assert.deepEqual(
    pf.checks.filter((c) => !c.passed).map((c) => c.name),
    ['transcript copied', 'recovery record written'],
    'only the archive checks should still be outstanding',
  )

  // Those two are teardown's own prerequisite, so it archives first and then
  // removes — the one case where a failing check is not a refusal.
  await t.api('POST', '/api/workspace/invoice/teardown')
  assert.equal(fs.existsSync(dir), false, 'the worktree survived a clean teardown')
  assert.equal(await t.workspace('invoice'), undefined, 'the workspace record outlived its tree')

  const s = await t.session(session)
  assert.equal(s.state.state, 'archived')
  assert.equal(s.resumable, true, 'the archive must leave a way back')
}

/** Assert teardown refuses, and that the named check is the reason.
 *
 *  The check name, not just "it threw": a refusal for the wrong reason passes a
 *  bare `rejects` and hides that the guard under test never fired. */
async function refuses(t, check) {
  const pf = await t.api('GET', '/api/workspace/invoice/preflight')
  const found = pf.checks.find((c) => c.name === check)
  assert.ok(found, `no check named ${check}; preflight has ${pf.checks.map((c) => c.name)}`)
  assert.equal(found.passed, false, `${check} passed when it should have blocked`)

  await assert.rejects(
    () => t.api('POST', '/api/workspace/invoice/teardown'),
    (e) => {
      assert.match(e.message, new RegExp(check), `refused, but not for ${check}: ${e.message}`)
      return true
    },
  )
  assert.ok(fs.existsSync(t.worktreePath('invoice')), 'refused and removed it anyway')
}
