// Opening a worktree: the daemon cuts the tree, then opens a session in it.
//
// The layout here is the one where the *daemon* cuts the worktree —
// `worktrees_subdir` is not Claude Code's default — so the workspace is
// registered up front at a path the daemon already knows. That is the arm every
// PR worktree, resume and relocation goes through.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import { branchOf } from '../harness.mjs'

export const name = 'open a worktree (daemon-cut)'

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(session)

  // The tree is real, on its own branch, and the session is in it.
  const dir = t.worktreePath('invoice')
  assert.ok(fs.existsSync(dir), `no worktree at ${dir}`)
  assert.equal(branchOf(dir), 'worktree-invoice')

  const s = await t.session(session)
  assert.equal(s.workspace, 'invoice')
  assert.equal(fs.realpathSync(s.cwd), fs.realpathSync(dir))

  // A turn happened, so this is a conversation and not an empty pane — the bit
  // fork, resume, prune and the archive row all read.
  assert.equal(s.has_transcript, true, 'the turn did not register')
  assert.equal(s.alive, true)

  const ws = await t.workspace('invoice')
  assert.deepEqual(ws.branches, ['worktree-invoice'])

  // One live session per workspace, refused by name at the API edge rather than
  // discovered at the spawn.
  await assert.rejects(
    () => t.api('POST', '/api/session', { workspace: 'invoice' }),
    /already has a live session/,
  )

  /* A create with no name at all, which the rail's button allows and which used to
     produce `wt-<8 hex>`. The daemon invents one in Claude Code's own shape now
     (`src/names.rs`), and this is the only place that path is exercised end to
     end: the name has to reach a real directory and a real branch, so a word the
     spawn's own validator refuses would fail here rather than in front of a user. */
  const { session: unnamed } = await t.api('POST', '/api/worktree', {})
  const it = await t.session(unnamed)
  assert.match(it.workspace, /^[a-z]+-[a-z]+-[a-z]+$/, `not a generated name: ${it.workspace}`)
  assert.ok(fs.existsSync(t.worktreePath(it.workspace)), 'the named tree was not cut')
  assert.equal(branchOf(t.worktreePath(it.workspace)), `worktree-${it.workspace}`)
}
