// A worktree the daemon did not cut is adopted by its root, whatever folder the
// hook that reports it was sent from (#55).
//
// A session in main ran `git worktree add` itself and then worked in a subfolder.
// The `SessionStart` that compaction sent came from `<tree>/tests/mcp`, and that
// folder became the workspace: every diff ran git from there and found nothing,
// and a new session in the workspace started there too.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { git, until } from '../harness.mjs'

export const name = 'adopt a worktree by its root, not the folder a hook came from'

export async function run(t) {
  const { session } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(session)

  const tree = t.worktreePath('by-hand')
  git(t.repo, ['worktree', 'add', '-q', '-b', 'by-hand', tree])
  const sub = path.join(tree, 'tests', 'mcp')
  fs.mkdirSync(sub, { recursive: true })

  await t.api('POST', '/hooks/session-start', {
    session_id: session, hook_event_name: 'SessionStart', source: 'compact', cwd: sub,
  })
  // A hook is answered before it is handled (`hooks::detach`), so wait for it.
  await until('the tree to be a workspace', async () => !!(await t.workspace('by-hand')))
  const ws = await t.workspace('by-hand')
  assert.equal(fs.realpathSync(ws.path), fs.realpathSync(tree), 'and its path is the root')
}
