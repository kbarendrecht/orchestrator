// Swapping when both trees hold a conversation: everything crosses.
//
// The asymmetric version — moving only the worktree's session in — left main's own
// conversation staring at a tree whose every file had changed under it. So both
// move, both keep their ids, and the order is load-bearing: main holds one session
// at a time, so the outgoing one has to vacate before the arrival is let in, or the
// arrival is refused with "main is occupied by …".

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { branchOf, until } from '../harness.mjs'

export const name = 'swap a worktree with an occupied main'

export async function run(t) {
  const { session: inMain } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(inMain)
  const { session: inTree } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(inTree)
  const dir = t.worktreePath('invoice')

  // Uncommitted work on both sides, so the crosswise re-apply runs in both
  // directions rather than only the one anybody would try by hand.
  fs.writeFileSync(path.join(t.repo, 'README.md'), '# main was editing this\n')
  fs.writeFileSync(path.join(dir, 'README.md'), '# the worktree was editing this\n')

  const r = await t.api('POST', '/api/workspace/invoice/swap-main')
  assert.equal(r.wip_error ?? null, null)
  assert.equal(branchOf(t.repo), 'worktree-invoice')
  assert.equal(branchOf(dir), 'main')

  // Each tree's work followed its own branch, crosswise.
  assert.equal(
    fs.readFileSync(path.join(t.repo, 'README.md'), 'utf8'),
    '# the worktree was editing this\n',
  )
  assert.equal(
    fs.readFileSync(path.join(dir, 'README.md'), 'utf8'),
    '# main was editing this\n',
  )

  // Asserted off the response before the state, so a conversation that did not
  // travel says why — `pick_to_carry` finding nobody, or a resume that would not
  // stay up and fell back to a fork — instead of timing out on a wait.
  assert.equal(r.into_main?.error ?? null, null)
  assert.equal(r.into_worktree?.error ?? null, null)
  assert.equal(r.into_main?.session, inTree, `main got ${JSON.stringify(r.into_main)}`)
  assert.equal(r.into_worktree?.session, inMain, `invoice got ${JSON.stringify(r.into_worktree)}`)
  assert.equal(r.into_main?.degraded, false, 'a fork is not the move that was promised')
  assert.equal(r.into_worktree?.degraded, false)

  // Both conversations moved, both kept their ids.
  //
  // The claim is waited on rather than read once: a relocation is a kill and a
  // resume at the far end, so the record naming its new workspace lands before
  // `claim_main` has run. Reading straight after the POST saw `occupant: null` and
  // called it a lost claim.
  await until('both conversations to land', async () => {
    const a = await t.session(inTree)
    const b = await t.session(inMain)
    const main = await t.workspace('main')
    return a?.workspace === 'main' && b?.workspace === 'invoice' && main.occupant === inTree
  }, {
    context: async () => {
      const s = await t.state()
      const rows = s.sessions.map((x) => `${x.id.slice(0, 8)}@${x.workspace}:${x.state.state}`)
      const occ = s.workspaces.find((w) => w.id === 'main')?.occupant
      return `${rows.join(' ')} occupant=${occ?.slice(0, 8)} `
        + `(wanted ${inTree.slice(0, 8)} in main, ${inMain.slice(0, 8)} in invoice)`
    },
  })
  for (const [id, where] of [[inTree, 'main'], [inMain, 'invoice']]) {
    const s = await t.session(id)
    assert.equal(s.workspace, where)
    assert.equal(s.forked_from, null, 'a relocation keeps its id, so it is not a fork')
  }

  // Exactly one occupant in main throughout, and it is the arrival.
  assert.equal((await t.workspace('main')).occupant, inTree)
  const live = (await t.state()).sessions.filter((s) => s.workspace === 'main' && s.alive)
  assert.equal(live.length, 1, `main ended up with ${live.length} live sessions`)

  // A session mid-turn in either tree is the one refusal, because the swap
  // replaces every file under it. Idle is fine — that is the normal place to
  // press this from, and both of these are idle, which is why the swap above ran.
  assert.equal((await t.session(inTree)).state.state, 'your_turn')
}
