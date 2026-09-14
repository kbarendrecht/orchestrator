// Swapping a worktree into a main that has no session of its own.
//
// What actually moves is what each tree has *checked out* — the directories stay
// exactly where they are, and neither is deleted. Worktrees live inside main, so
// one cannot become the primary checkout without containing its own parent, and
// git will not move the main worktree anyway.
//
// Three things travel and this flow pins all of them: the branch, the uncommitted
// tracked work, and the conversation. Untracked files do *not* travel, because
// `stash create` cannot carry them, and the response names the ones left behind.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'swap a worktree into empty main'

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(session)
  const dir = t.worktreePath('invoice')

  // Work in flight, of both kinds: a tracked edit, which travels with its branch,
  // and an untracked file, which cannot.
  fs.writeFileSync(path.join(dir, 'README.md'), '# edited in the worktree\n')
  fs.writeFileSync(path.join(dir, 'scratch.txt'), 'untracked\n')

  const r = await t.api('POST', '/api/workspace/invoice/swap-main')

  // The branches traded places.
  assert.equal(r.main, 'worktree-invoice')
  assert.equal(r.worktree, 'main')
  assert.equal(branchOf(t.repo), 'worktree-invoice')
  assert.equal(branchOf(dir), 'main')
  assert.equal(r.wip_error ?? null, null, 'the banked work should have re-applied')

  // Both directories are still there. "Swapping into main" is not a move.
  assert.ok(fs.existsSync(dir), 'the worktree must survive its own swap')
  assert.ok(fs.existsSync(t.repo))

  // The tracked edit went with its branch; the untracked file stayed and is named
  // rather than quietly not moving.
  assert.equal(
    fs.readFileSync(path.join(t.repo, 'README.md'), 'utf8'),
    '# edited in the worktree\n',
  )
  assert.deepEqual(r.untracked_left, ['scratch.txt'])
  assert.ok(fs.existsSync(path.join(dir, 'scratch.txt')))
  // Tracked only: the untracked file that stayed behind is exactly why the tree
  // is still "dirty" to a plain status, and it is not a leftover.
  assert.equal(
    git(dir, ['status', '--porcelain', '--untracked-files=no']),
    '',
    'the worktree kept a tracked change that should have travelled',
  )

  // The conversation followed its branch, and kept its id — one rail row moved,
  // rather than a forked sibling appearing.
  await until('the conversation to arrive in main', async () =>
    (await t.session(session))?.workspace === 'main')
  const s = await t.session(session)
  assert.equal(s.forked_from, null, 'a relocation is not a fork')
  assert.equal((await t.workspace('main')).occupant, session)
  /* **Intact, not merely recorded as moved.** A relocation is a kill and a
     `--resume` at the far end, so a record naming main is the cheapest half of the
     claim: the conversation has to still be resumable, and the pty has to have come
     back in the tree the record now names. `r.into_main.degraded` is the daemon's
     own report that the resume stayed up rather than falling back to a fork; the
     two below are the independent check. */
  assert.equal(r.into_main?.degraded, false, 'a fork is not the move that was promised')
  assert.equal(s.state.state, 'your_turn', 'the conversation did not come back idle')
  assert.equal(s.has_transcript, true, 'the conversation did not survive the move')
  assert.equal(fs.realpathSync(s.cwd), fs.realpathSync(t.repo), 'cwd did not follow')

  // Each tree gave a branch away, so neither may go on claiming it: `reconcile`
  // only adds, and a stale claim points a PR flow at the wrong tree.
  const ws = await t.workspace('invoice')
  assert.ok(!ws.branches.includes('worktree-invoice'), `invoice still claims ${ws.branches}`)

  // Pressing it again is the undo, which is what makes the menu item safe.
  const back = await t.api('POST', '/api/workspace/invoice/swap-main')
  assert.equal(back.main, 'main')
  assert.equal(back.worktree, 'worktree-invoice')
  assert.equal(branchOf(t.repo), 'main')
  assert.equal(
    fs.readFileSync(path.join(dir, 'README.md'), 'utf8'),
    '# edited in the worktree\n',
    'and the work comes home with it',
  )
}
