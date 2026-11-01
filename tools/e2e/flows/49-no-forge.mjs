// A checkout nobody has pushed anywhere: no `origin`, no forge, and the base is
// its own `main`.
//
// Every other flow has an `origin`, so a remote-less repo was reachable only by
// unit tests that each saw one half. This is the round trip: a worktree cuts from
// the local base, a rebase takes in what main gained without claiming a fetch
// failed, and a swap refuses to hand the base itself to a worktree — whose
// commits would then move the base.

import assert from 'node:assert/strict'
import { branchOf, git } from '../harness.mjs'

export const name = 'a checkout with no remote cuts, rebases and keeps its base'

export const options = { noRemote: true }

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'local' })
  await t.settled(session)
  const dir = t.worktreePath('local')
  assert.equal(branchOf(dir), 'worktree-local', 'a worktree of its own, cut from main')

  // Main gains a commit the worktree does not have yet.
  git(t.repo, ['commit', '-q', '--allow-empty', '-m', 'main moved on'])
  const tip = git(t.repo, ['rev-parse', 'HEAD'])

  const rebased = await t.api('POST', '/api/workspace/local/rebase')
  assert.equal(rebased.rebased, 'local')
  assert.equal(rebased.warning ?? null, null, `a local base has nothing to fetch: ${rebased.warning}`)
  git(dir, ['merge-base', '--is-ancestor', tip, 'HEAD'])

  await assert.rejects(
    () => t.api('POST', '/api/workspace/local/swap-main'),
    /no worktree may hold it/,
  )
  assert.equal(branchOf(t.repo), 'main', 'the refused swap moved main')
  assert.equal(branchOf(dir), 'worktree-local', 'the refused swap moved the worktree')

  // And the forge is simply off, not an error anything else trips over.
  const state = await t.state()
  assert.equal(state.repos?.upstream ?? null, null)
}
