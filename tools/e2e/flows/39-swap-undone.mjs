// A swap whose arriving session will not start puts everything back.
//
// Git and a starting `claude` cannot share a lock, so a session that dies at the far
// end is found out after the branches have moved. That used to be a partial success:
// the branches swapped, the conversation stopped in the tree its branch had left.
// Now it is undone: the branches swap back, the session that did arrive goes home,
// and the one that failed is resumed where it was.
//
// Main's session goes out first and is let through; the worktree's resume into main
// and its fork fallback both die, so the undo has an arrived session to send back
// as well as a failed one to resume.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { branchOf, until } from '../harness.mjs'

export const name = 'a swap that cannot finish is undone'

// `debug` for the daemon's own crates, because this flow fails only on CI runners
// (macos-14 on 2026-10-06, ubuntu-22.04 on 2026-10-13, never locally in 8 of 8),
// and those keep no sandbox. What the undo did is the evidence, and at the suite's
// `warn` most of it is never written.
export const options = {
  log: 'warn,orchd=debug,orchd_serve=debug,orchd_base=debug,orchd_repo=debug',
}

export async function run(t) {
  const { session: inMain } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(inMain)
  await t.transcribed(inMain)
  const { session: inTree } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(inTree)
  await t.transcribed(inTree)
  const dir = t.worktreePath('invoice')
  const base = branchOf(t.repo)

  fs.writeFileSync(path.join(t.repo, 'README.md'), '# main was editing this\n')
  fs.writeFileSync(path.join(dir, 'README.md'), '# the worktree was editing this\n')
  // A new file travels with the branch, so the undo has to bring it back too.
  fs.mkdirSync(path.join(dir, 'made'), { recursive: true })
  fs.writeFileSync(path.join(dir, 'made/new.txt'), 'the worktree made this\n')

  // Where this flow's own story starts in the two logs, for the timeout below.
  const logFrom = t.log().length
  const agentFrom = t.agentLog().length

  t.dieOnResume(1, 2)
  let refused
  try {
    await t.api('POST', '/api/workspace/invoice/swap-main', { session: inTree })
  } catch (e) {
    refused = e
  }
  assert.ok(refused, 'the swap reported success while a session could not arrive')
  assert.match(refused.message, /swap failed, nothing moved/)
  assert.equal(refused.status, 409, 'an undone swap is a try-again, not a never')

  /* **Both intended deaths, and no more.** This flow decides how a swap fails by
     counting resumes, and the undo's own two resumes then have to find the counter
     empty: one death left over kills a session on its way home, and the only
     symptom is the wait below never coming true — which is how this flow failed on
     a macOS runner, saying nothing about why. Read here, so that a miscount names
     itself instead of looking like a slow machine. */
  const deaths = fs.readFileSync(path.join(t.root, 'die-on-resume'), 'utf8').trim()
  assert.equal(deaths, '0,0', 'the swap used both deaths and left none for the undo')

  // The branches and each tree's own edits are back.
  assert.equal(branchOf(t.repo), base)
  assert.equal(branchOf(dir), 'worktree-invoice')
  assert.equal(fs.readFileSync(path.join(t.repo, 'README.md'), 'utf8'), '# main was editing this\n')
  assert.equal(fs.readFileSync(path.join(dir, 'README.md'), 'utf8'), '# the worktree was editing this\n')
  assert.equal(fs.readFileSync(path.join(dir, 'made/new.txt'), 'utf8'), 'the worktree made this\n',
    'the new file came home with its branch')
  assert.ok(!fs.existsSync(path.join(t.repo, 'made/new.txt')), 'and is not left behind in main')

  // Both conversations are home, live, under their own ids, and main is held again.
  await until('both sessions to be home', async () => {
    const s = await t.state()
    const a = s.sessions.find((x) => x.id === inMain)
    const b = s.sessions.find((x) => x.id === inTree)
    const main = s.workspaces.find((w) => w.id === 'main')
    return a?.workspace === 'main' && a.alive
      && b?.workspace === 'invoice' && b.alive
      && main?.occupant === inMain
  }, {
    /* Which of the three was false. A timeout that only names what it wanted is
       the least useful failure here: the daemon is gone by the time anyone reads
       the log, and a session that would not come home looks exactly like one that
       was merely slow. */
    context: async () => {
      const s = await t.state()
      const where = (id) => {
        const x = s.sessions.find((y) => y.id === id)
        return x ? `${x.workspace}/${x.alive ? 'alive' : 'stopped'}` : 'no record'
      }
      const main = s.workspaces.find((w) => w.id === 'main')
      /* **And both sides of the undo, since the swap.** The state above says which
         session did not come home; it cannot say why, and on a runner nothing of
         the sandbox survives the job. So the daemon's log and the agent's notes
         go into the failure itself. The tail only: a whole debug log is noise. */
      // The daemon colours its log even into a file; a failure is read as text.
      const plain = (text) => text.replace(/\x1b\[[0-9;]*m/g, '')
      const tail = (text, n) => plain(text).trimEnd().split('\n').slice(-n).join('\n')
      const agent = t.agentLog().slice(agentFrom)
      return `main's ${where(inMain)}, the worktree's ${where(inTree)}, `
        + `main held by ${main?.occupant?.slice(0, 8) ?? 'nobody'} `
        + `(main's session is ${inMain.slice(0, 8)}, the worktree's ${inTree.slice(0, 8)})`
        + `\n--- the agents, since the swap ---\n${tail(agent, 40) || '(nothing)'}`
        + `\n--- the daemon, since the swap ---\n${tail(t.log().slice(logFrom), 120) || '(nothing)'}`
    },
  })
  const s = await t.state()
  for (const id of [inMain, inTree]) {
    const x = s.sessions.find((y) => y.id === id)
    assert.equal(x.cwd, id === inMain ? t.repo : dir, `${id.slice(0, 8)} is back in its own tree`)
  }
  // The fork that was tried and died is a stopped record about the worktree's
  // branch, so it went home with the session it forked from.
  for (const fork of s.sessions.filter((x) => x.forked_from === inTree)) {
    assert.equal(fork.workspace, 'invoice', 'a dead fork stayed behind in main')
  }
  assert.deepEqual(
    s.sessions.filter((x) => x.workspace === 'main' && x.alive).map((x) => x.id),
    [inMain],
  )
}
