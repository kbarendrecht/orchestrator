// The two buttons on a session's own row: rewind the conversation, and forget it.
//
// Rewind is a double-tap of Escape written into the pty, so what it *does* is
// Claude Code's business and only the refusals are the daemon's. Each one is a
// state where those two keystrokes would mean something other than "open the
// picker": cancelling a question, declining a permission, or landing in a session
// with no conversation to go back through.
//
// Delete is the other end of a row's life. It has to reach the record on disk as
// well as the one in memory, or the rail is rid of a session that comes back on
// the next start.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { until } from '../harness.mjs'

export const name = 'rewind refuses, and delete forgets'

const records = (t) =>
  JSON.parse(fs.readFileSync(path.join(t.cfg, 'sessions.json'), 'utf8'))

export async function run(t) {
  // --- a session that was never typed into ------------------------------------

  // The picker would open on nothing, which reads as a broken button rather than
  // as an empty conversation.
  t.setTurns(0)
  const { session: empty } = await t.api('POST', '/api/worktree', { name: 'quiet' })
  await t.settled(empty, ['your_turn', 'ready'])
  await assert.rejects(
    () => t.api('POST', `/api/session/${empty}/rewind`),
    /no conversation to rewind/,
  )

  // --- one that has a conversation --------------------------------------------

  t.setTurns(1)
  const { session } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(session)
  const { rewinding } = await t.api('POST', `/api/session/${session}/rewind`)
  assert.equal(rewinding, session)

  // --- and one that is over ----------------------------------------------------

  await t.api('POST', `/api/session/${session}/kill`)
  await until('the session to stop being live', async () =>
    (await t.session(session))?.alive === false)
  await assert.rejects(
    () => t.api('POST', `/api/session/${session}/rewind`),
    /only opens at the prompt/,
  )

  // Recorded before it is deleted, so the assertion below is about the delete and
  // not about a record that was never written.
  assert.ok(
    records(t).some((r) => r.id === session),
    'the killed session should still be on disk to be forgotten',
  )

  await t.api('POST', `/api/session/${session}/delete`)
  await until('the row to go', async () => (await t.session(session)) === undefined)
  assert.ok(
    !records(t).some((r) => r.id === session),
    'deleted in the rail and back on the next start',
  )
  // The other row is untouched: delete is one session, not a tidy-up.
  assert.ok(await t.session(empty), 'the quiet session went with it')
}
