// Forking a worktree session: same context, new direction, its own tree.
//
// A fork used to share the parent's checkout, which made "new direction" a lie —
// two agents editing one tree, and whichever wrote last decided what the other was
// looking at. So a fork cuts a worktree of its own, and `--resume` finds the
// conversation by id wherever it was recorded.

import assert from 'node:assert/strict'
import fs from 'node:fs'

export const name = 'fork a worktree session'

export async function run(t) {
  const { session: parent } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(parent)
  await t.transcribed(parent)

  const { session: fork } = await t.api('POST', `/api/session/${parent}/fork`)
  await t.settled(fork)

  const [p, f] = [await t.session(parent), await t.session(fork)]
  assert.notEqual(fork, parent, 'a fork is a second conversation, so a second id')
  assert.equal(f.forked_from, parent, 'without this two rows read the same')
  assert.equal(f.has_transcript, true, 'a fork opens on the replayed conversation')

  // Its own tree, and the parent is untouched in its own.
  assert.notEqual(f.workspace, p.workspace)
  assert.notEqual(fs.realpathSync(f.cwd), fs.realpathSync(p.cwd))
  assert.equal(p.alive, true, 'forking must not disturb the parent')

  // And the refusal that guards it: a pane never typed into has no conversation
  // to fork, so the API says so before a worktree is cut for a session that would
  // die instantly. The SPA greys the item too; this is the server-side half.
  t.setTurns(0)
  const { session: empty } = await t.api('POST', '/api/worktree', { name: 'empty' })
  await t.settled(empty, ['starting', 'your_turn'])
  assert.equal((await t.session(empty)).has_transcript, false)

  const before = (await t.state()).workspaces.length
  await assert.rejects(
    () => t.api('POST', `/api/session/${empty}/fork`),
    /no conversation yet/,
  )
  assert.equal(
    (await t.state()).workspaces.length,
    before,
    'the refusal must come before a worktree is cut, not after',
  )
}
