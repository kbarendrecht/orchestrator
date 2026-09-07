// Opening a worktree at Claude Code's own layout, which the daemon cuts too.
//
// `worktrees_subdir` left at `.claude/worktrees` used to change who created the
// tree: the daemon spawned `claude --worktree` from the main checkout and adopted
// whatever cwd `SessionStart` reported. It does not any more — `spawn_worktree_session`
// says why, and the short version is that arm pinned worktree isolation into the
// transcript, which refuses writes as well as git. So this flow pins the layout
// making no difference to who cuts, and the lock the old arm left behind being gone.
//
// It also drives the stale-lock retry, on a lock put there by hand: that is now the
// only way to reach it, and it is still reachable in the wild by a repo that locks
// its own trees.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'open a worktree (Claude Code layout)'
export const options = { delegated: true }

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(session)

  const dir = t.worktreePath('invoice')
  assert.ok(fs.existsSync(dir), `nothing was cut at ${dir}`)
  assert.equal(branchOf(dir), 'worktree-invoice')

  const s = await t.session(session)
  assert.equal(s.workspace, 'invoice')
  assert.equal(s.has_transcript, true)

  // The daemon cut it, so nothing locked it. `claude --worktree` did, and that is
  // what made a plain `git worktree remove` refuse forever once its session was gone.
  assert.doesNotMatch(
    git(t.repo, ['worktree', 'list', '--porcelain']),
    /^locked /m,
    'a daemon-cut tree should carry no lock',
  )

  /* Stand in for a repo that locks its own trees, so teardown still has to clear a
     lock whose owner is dead. The reason has to carry a pid: `stale_lock_pid` only
     clears a lock it can prove is orphaned, and a lock with no pid in it is left to
     refuse on purpose. Pid 1 is init and alive, so this uses a pid that cannot be —
     one past the maximum. */
  const deadPid = Number(fs.readFileSync('/proc/sys/kernel/pid_max', 'utf8').trim()) + 1
  git(t.repo, ['worktree', 'lock', '--reason', `claude code session (pid ${deadPid})`, dir])

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
