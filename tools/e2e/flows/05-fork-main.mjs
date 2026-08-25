// Forking main: the fork lands in a worktree, never beside its parent in main.
//
// Main is exclusive, so a fork that stayed there would have to evict the
// conversation it was forked from. It gets a tree of its own for the same reason
// a worktree fork does, and main keeps its occupant.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'

export const name = 'fork main into a worktree'

export async function run(t) {
  const { session: parent } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(parent)

  const { session: fork } = await t.api('POST', `/api/session/${parent}/fork`)
  await t.settled(fork)

  const f = await t.session(fork)
  assert.equal(f.forked_from, parent)
  assert.equal(f.has_transcript, true)

  // The point of the flow: not in main.
  assert.notEqual(f.workspace, 'main')
  assert.ok(
    fs.realpathSync(f.cwd).startsWith(fs.realpathSync(path.join(t.repo, '.worktrees'))),
    `the fork should be in a worktree, but its cwd is ${f.cwd}`,
  )

  // And main is still held by the conversation it was forked from, not handed to
  // the fork and not left free.
  assert.equal((await t.workspace('main')).occupant, parent)
  assert.equal((await t.session(parent)).alive, true)
}
