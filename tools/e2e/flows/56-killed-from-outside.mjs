// An agent killed by a signal orchd did not send stays a row, says why, and comes
// back only when you resume it.
//
// The usual sender is memory pressure: systemd-oomd or the kernel's OOM killer. So
// the row must not go to the archive, where nobody looks, and must not come back by
// itself on the next start, into the same pressure that killed it.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { until } from '../harness.mjs'

export const name = 'an agent killed from outside stays stopped until you resume it'

export async function run(t) {
  const { session: id } = await t.api('POST', '/api/worktree', { name: 'heavy' })
  await t.settled(id)

  await killFromOutside(t, id)
  assert.equal((await t.session(id)).killed, 'outside', 'an outside kill was not told apart')

  // A restart keeps it stopped: not auto-resumed, and still owed the resume.
  await t.restart()
  const restored = await until('the stopped session to be restored', async () => await t.session(id))
  assert.equal(restored.killed, 'outside', 'the restart forgot why it stopped')
  assert.equal(restored.alive, false, 'auto-resume brought back a session that was killed')

  // Resume is what clears it.
  await t.api('POST', `/api/session/${id}/resume`)
  await t.settled(id)
  assert.equal((await t.session(id)).killed, null, 'a resumed session still reads as stopped')

  // Closing a stopped row puts it in the archive instead.
  await killFromOutside(t, id)
  await t.api('POST', `/api/session/${id}/kill`)
  await until('the closed row to be archived', async () => (await t.session(id))?.killed === null)
}

/** `SIGKILL` the agent the way oomd would: from outside, with no warning. */
async function killFromOutside(t, id) {
  const pid = await until(`session ${id.slice(0, 8)}'s pid on its record`, () => {
    const records = JSON.parse(fs.readFileSync(path.join(t.cfg, 'sessions.json'), 'utf8'))
    return records.find((r) => r.id === id)?.pid
  })
  process.kill(pid, 'SIGKILL')
  await until(`session ${id.slice(0, 8)} to stop being live`, async () =>
    (await t.session(id))?.alive === false)
}
