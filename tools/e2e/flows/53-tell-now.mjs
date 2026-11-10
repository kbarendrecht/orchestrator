// "Tell them now" on the Conflicts card types each side's note in at once, and
// the next prompt then has nothing left to say.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { until } from '../harness.mjs'

export const name = 'tell both sides of a conflict now, not on their next prompt'

export async function run(t) {
  const made = []
  for (const name of ['one', 'two']) {
    const { session } = await t.api('POST', '/api/worktree', { name })
    await t.settled(session)
    made.push(await t.session(session))
  }
  const [one, two] = made
  for (const s of made) {
    fs.writeFileSync(path.join(s.cwd, 'README.md'), `changed in ${s.workspace}\n`)
    await t.api('POST', `/api/workspace/${s.workspace}/reconcile`)
  }
  const overlaps = async (ws) => (await t.state()).workspaces.find((w) => w.id === ws)?.overlaps ?? []
  assert.deepEqual((await overlaps(one.workspace)).map((o) => o.told), [false], 'nobody is told yet')

  const r = await t.api('POST', `/api/session/${one.id}/tell-overlaps`)
  assert.equal(r.told, 2, `both sides were typed into: ${JSON.stringify(r)}`)
  await until('both notes to reach the agents', async () => {
    const log = t.agentLog()
    return log.includes(`${one.id.slice(0, 8)} turn: orchd:`) && log.includes(`${two.id.slice(0, 8)} turn: orchd:`)
  })
  assert.deepEqual((await overlaps(one.workspace)).map((o) => o.told), [true], 'the card can say they were told')
  assert.deepEqual((await overlaps(two.workspace)).map((o) => o.told), [true], 'on both sides')

  await t.api('POST', '/hooks/stop', { session_id: one.id, hook_event_name: 'Stop' })
  const next = await t.api('POST', '/hooks/user-prompt-submit', {
    session_id: one.id, hook_event_name: 'UserPromptSubmit', prompt: 'go on',
  })
  assert.equal(next?.hookSpecificOutput, undefined, 'and the next prompt does not say it again')
}
