// Coming back to a conversation whose worktree is gone — and the trap next to it.
//
// Two arms, and they are different code. When the path is missing, `worktree::revive`
// rebuilds the tree from the recovery record before the session goes back into it.
// When the path is *there*, the rebuild is skipped entirely — and that is the arm
// that used to say nothing at all when the tree standing at the path was cut again
// for something else, so the conversation came back on code it had never run on.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'resume an archived session'

export async function run(t) {
  // --- the tree is gone, so it is rebuilt ---------------------------------

  const { session: first } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(first)
  const dir = t.worktreePath('invoice')

  await close(t, first)
  await t.api('POST', '/api/workspace/invoice/teardown')
  assert.equal(fs.existsSync(dir), false, 'teardown left the worktree behind')

  // Archived, and still offering a resume: it had a turn, and the recovery record
  // knows the branch and the commit to rebuild from.
  const archived = await t.session(first)
  assert.equal(archived.state.state, 'archived')
  assert.equal(archived.resumable, true)

  const back = await t.api('POST', `/api/session/${first}/resume`)
  assert.equal(back.warning ?? null, null, 'nothing moved, so nothing to warn about')
  assert.ok(fs.existsSync(dir), 'the worktree was not rebuilt')
  assert.equal(branchOf(dir), 'worktree-invoice', 'rebuilt on the wrong branch')
  await t.settled(first)
  assert.equal((await t.session(first)).workspace, 'invoice')

  // --- the tree is there, but it is not the same tree ----------------------

  const { session: second } = await t.api('POST', '/api/worktree', { name: 'billing' })
  await t.settled(second)
  const billing = t.worktreePath('billing')

  await close(t, second)
  await t.api('POST', '/api/workspace/billing/teardown')
  assert.equal(fs.existsSync(billing), false)

  // Cut again at the same path, on a different branch — what `ensure_pr_worktree`
  // does when you come back to a PR whose worktree you tore down.
  git(t.repo, ['branch', 'other', 'main'])
  git(t.repo, ['worktree', 'add', '-q', billing, 'other'])
  // The daemon only knows about a tree it created or adopted, and adoption happens
  // at boot — the same way the tree would be there in real life.
  await t.restart()
  await until('the re-cut worktree to be adopted', async () => await t.workspace('billing'))

  const drifted = await t.api('POST', `/api/session/${second}/resume`)
  assert.ok(drifted.warning, 'resuming into a tree cut again said nothing at all')
  for (const bit of ['billing', 'other', 'worktree-billing']) {
    assert.ok(
      drifted.warning.includes(bit),
      `the warning must name the tree and both branches, got: ${drifted.warning}`,
    )
  }
  // Said, not refused: the conversation still comes back, because the record is
  // its own history and cannot arbitrate what the tree is for now.
  await t.settled(second)
  assert.equal(branchOf(billing), 'other', 'the resume must not move the branch')
}

/** End a session and wait for the daemon to agree it is over — teardown's first
 *  preflight check is "no live session", and the kill returning is not that. */
async function close(t, id) {
  await t.api('POST', `/api/session/${id}/kill`)
  await until(`session ${id.slice(0, 8)} to stop being live`, async () =>
    (await t.session(id))?.alive === false)
}
