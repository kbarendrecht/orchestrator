// Opening a worktree the other way: Claude Code cuts it, the daemon adopts it.
//
// With `worktrees_subdir` left at `.claude/worktrees` the daemon does not create
// the tree at all — it spawns `claude --worktree` from the main checkout and takes
// whatever cwd `SessionStart` reports. Worth its own flow because the workspace is
// registered at a different moment, and because it is the arm that leaves a git
// lock behind for teardown to clear.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'open a worktree (claude --worktree)'
export const options = { delegated: true }

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(session)

  const dir = t.worktreePath('invoice')
  assert.ok(fs.existsSync(dir), `claude --worktree cut nothing at ${dir}`)
  assert.equal(branchOf(dir), 'worktree-invoice')

  const s = await t.session(session)
  assert.equal(s.workspace, 'invoice')
  assert.equal(s.has_transcript, true)

  // The tree is locked, which is what `claude --worktree` really does — and what
  // makes a plain `git worktree remove` refuse forever once the session is gone.
  assert.match(
    git(t.repo, ['worktree', 'list', '--porcelain']),
    /^locked /m,
    'the delegated cut should leave a lock for teardown to deal with',
  )

  await t.api('POST', `/api/session/${session}/kill`)
  await until('the session to stop being live', async () =>
    (await t.session(session))?.alive === false)

  // Teardown archives first — that is its own preflight's prerequisite — and the
  // removal after it only works because the lock's owner is dead by now, which is
  // the retry `git::worktree_remove` exists for.
  await t.api('POST', '/api/workspace/invoice/teardown')
  assert.equal(fs.existsSync(dir), false, 'the worktree survived teardown')

  // Archived, and still a conversation you can come back to.
  const after = await t.session(session)
  assert.equal(after.state.state, 'archived')
  assert.equal(after.has_transcript, true)
}
