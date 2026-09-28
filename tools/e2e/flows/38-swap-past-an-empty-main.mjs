// Swapping while main holds a session that never had a turn.
//
// That session has no conversation, so it was never picked to go out, and main
// holds one session: the arrival was refused with "main is occupied" after git had
// already moved the branches and the arriving session had been killed on its way
// in. So every live session in main goes out now, and one with nothing to resume
// is started fresh in the worktree rather than resumed.

import assert from 'node:assert/strict'
import { branchOf, until } from '../harness.mjs'

export const name = 'swap past an empty session in main'

export async function run(t) {
  t.setTurns(0)
  const { session: empty } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(empty)
  t.setTurns(1)
  const { session: inTree } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(inTree)
  await t.transcribed(inTree)
  const dir = t.worktreePath('invoice')

  const r = await t.api('POST', '/api/workspace/invoice/swap-main', { session: inTree })
  assert.equal(branchOf(t.repo), 'worktree-invoice')
  assert.equal(branchOf(dir), 'main')

  // Off the response first, so a carry that failed says why instead of timing out.
  assert.equal(r.into_main?.error ?? null, null, `main got ${JSON.stringify(r.into_main)}`)
  assert.equal(r.into_main?.session, inTree, 'the pressed session is the one that arrives')
  assert.equal(r.moved_out.length, 1, `out of main: ${JSON.stringify(r.moved_out)}`)
  assert.equal(r.moved_out[0].error ?? null, null, `invoice got ${JSON.stringify(r.moved_out)}`)
  // A fresh start, not a resume: there was nothing to resume.
  const started = r.moved_out[0].session

  await until('both to land', async () => {
    const s = await t.state()
    const arrived = s.sessions.find((x) => x.id === inTree)
    const out = s.sessions.find((x) => x.id === started)
    const main = s.workspaces.find((w) => w.id === 'main')
    return arrived?.workspace === 'main' && arrived.alive
      && out?.workspace === 'invoice' && out.alive
      && main?.occupant === inTree
  })
  const liveInMain = (await t.state()).sessions.filter((x) => x.workspace === 'main' && x.alive)
  assert.deepEqual(liveInMain.map((x) => x.id), [inTree], 'main holds only the arrival')
}
