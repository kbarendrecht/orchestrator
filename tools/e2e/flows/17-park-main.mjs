// Main going back to its base branch when the last session in it closes.
//
// The move happens in the exit watcher rather than in the request that killed the
// session, so nothing the caller reads says whether it ran — which is what makes
// this a flow rather than a unit test. Both halves are load-bearing. A branch left
// standing in main makes every PR flow for it impossible, because git allows one
// checkout per branch and the worktree those flows need cannot be cut. And a
// *dirty* main must keep its branch: carrying somebody's uncommitted work onto
// another branch is not the daemon's to do, and the skip is silent, so the only
// way to see it is from outside.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'main parks when the last session leaves'

export async function run(t) {
  const base = branchOf(t.repo)
  const readme = path.join(t.repo, 'README.md')

  // --- a dirty main keeps its branch -----------------------------------------

  const { session } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(session)
  git(t.repo, ['switch', '-qc', 'feature/parked'])
  fs.writeFileSync(readme, '# uncommitted in main\n')

  await t.api('POST', `/api/session/${session}/kill`)
  await until('main to come free', async () => (await t.workspace('main')).occupant == null)
  /* Read here and again once the next session is in. The claim is handed back a
     few lines *before* the park decision in the same watcher, so one look proves
     nothing on its own — but a park that ignored the dirty tree has to show up in
     one of the two, and the second one is taken after a whole request round trip. */
  assert.equal(branchOf(t.repo), 'feature/parked', 'a dirty main must keep its branch')
  assert.equal(fs.readFileSync(readme, 'utf8'), '# uncommitted in main\n', 'and its work')

  // --- a clean one goes home --------------------------------------------------

  git(t.repo, ['commit', '-qam', 'work in main'])
  const { session: second } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(second)
  assert.equal(branchOf(t.repo), 'feature/parked', 'the dirty main was parked after all')

  await t.api('POST', `/api/session/${second}/kill`)
  await until('main to go back to its base', async () => branchOf(t.repo) === base, {
    context: async () => `main is on ${branchOf(t.repo)}, wanted ${base}`,
  })

  // Parked, not lost: the branch still has the commit, and `move_branch_out` is how
  // it gets a tree of its own if you want one.
  assert.equal(git(t.repo, ['log', '-1', '--format=%s', 'feature/parked']), 'work in main')
  assert.equal(git(t.repo, ['status', '--porcelain']), '', 'main came home with work on it')
}
