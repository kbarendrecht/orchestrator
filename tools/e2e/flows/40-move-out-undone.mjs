// A move out of main whose session will not start in the new tree puts everything
// back.
//
// The swap's rule, on the other gesture: the branch and its work go back into main,
// the tree cut for them is removed, and the session is resumed in main as it stood.
// Main is on a branch of its own here, so the undo has a handed-over branch to take
// back rather than one cut for the move.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'a move out of main that cannot finish is undone'

export async function run(t) {
  const { session } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(session)
  await t.transcribed(session)
  git(t.repo, ['switch', '-qc', 'feature/real-work'])
  fs.writeFileSync(path.join(t.repo, 'README.md'), '# on a branch\n')

  t.dieOnResume(0, 2)
  let refused
  try {
    await t.api('POST', `/api/session/${session}/out-of-main`)
  } catch (e) {
    refused = e
  }
  assert.ok(refused, 'the move reported success while its session could not arrive')
  assert.match(refused.message, /move failed, nothing moved/)
  assert.equal(refused.status, 409)

  assert.equal(branchOf(t.repo), 'feature/real-work', 'the branch is back in main')
  assert.equal(fs.readFileSync(path.join(t.repo, 'README.md'), 'utf8'), '# on a branch\n')
  assert.ok(!fs.existsSync(t.worktreePath('real-work')), 'the tree cut for the move is gone')

  await until('the session to be home', async () => {
    const s = await t.state()
    const x = s.sessions.find((y) => y.id === session)
    return x?.workspace === 'main' && x.alive
      && s.workspaces.find((w) => w.id === 'main')?.occupant === session
      && !s.workspaces.some((w) => w.id === 'real-work')
  })
}
