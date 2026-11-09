// What `orch new`, `orch rename` and `orch kill` do, from the daemon's side.
//
// The CLI's own tests cover the argv guard; nothing they can do proves the round
// trip, because the thing worth checking is a *worktree*: whether the spawn cut a
// real one on its own branch, and whether the undo takes it away again — or, in the
// case that matters more, leaves alone a tree the spawn did not create.
//
// That asymmetry is the whole safety property. Both spawns are discarded through
// the same route, and only one of them may end with a directory gone.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import { branchOf, until } from '../harness.mjs'

export const name = 'a session spawns another and undoes it'

export async function run(t) {
  const { session: parent } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(parent)
  const invoice = t.worktreePath('invoice')

  // 1. Naming a workspace that does not exist stays an error, and now says what
  //    the names are. This is the refusal that blocked the task: with nothing to
  //    create a tree, the only reachable shape was "beside me".
  await assert.rejects(
    () => t.api('POST', `/api/session/${parent}/spawn`, { workspace: 'dependabot-api' }),
    (e) => {
      assert.match(e.message, /unknown workspace dependabot-api/)
      assert.match(e.message, /known: .*invoice/, `it must list them: ${e.message}`)
      return true
    },
  )

  // Two names for one place is a request nobody can honour.
  await assert.rejects(
    () => t.api('POST', `/api/session/${parent}/spawn`, { workspace: 'invoice', worktree: true }),
    /not both/,
  )

  // 2. Your own tree is never shared: it holds you, and a second agent there
  //    shares your index and is dropped by the next restart.
  await assert.rejects(
    () => t.api('POST', `/api/session/${parent}/spawn`, { workspace: 'invoice' }),
    /invoice already has a live session/,
  )

  // A tree that exists and is free is a place you may name, and the undo must
  // leave it alone — the spawn did not cut it, so it is not the spawn's to remove.
  const { session: notesOwner } = await t.api('POST', '/api/worktree', { name: 'notes' })
  await t.settled(notesOwner)
  await t.api('POST', `/api/session/${notesOwner}/kill`)
  await until('the notes session to end', async () => !(await t.session(notesOwner))?.alive)
  const notes = t.worktreePath('notes')
  const beside = await t.api('POST', `/api/session/${parent}/spawn`, { workspace: 'notes', prompt: 'help me' })
  assert.equal(beside.workspace, 'notes')
  assert.equal(fs.realpathSync(beside.path), fs.realpathSync(notes))
  await t.settled(beside.session)

  // Asserted on the absence of *both* outcomes, because a teardown that was
  // attempted and refused would report `kept`, and "refused anyway" is a
  // different bug from "never tried".
  const undone = await t.api('POST', `/api/session/${parent}/spawned/${beside.session}/discard`)
  assert.equal(undone.removed, undefined, 'it removed a worktree it did not create')
  assert.equal(undone.kept, undefined, 'it should not have reached teardown at all')
  assert.equal(await t.session(beside.session), undefined, 'the record outlived the discard')
  assert.ok(fs.existsSync(notes), 'the named tree was removed')
  assert.ok(await t.workspace('notes'), 'the named tree lost its workspace record')

  // 3. A fresh tree, its own branch, its own index — the default, and what a name
  //    asks for. Two of these is "spawn two fixers, one PR each".
  // An effort the agent does not take is refused before anything is cut.
  await assert.rejects(
    () => t.api('POST', `/api/session/${parent}/spawn`, { name: 'fixer-a', effort: 'turbo' }),
    /takes an effort of low, medium, high, xhigh, max, not "turbo"/,
  )
  assert.ok(!fs.existsSync(t.worktreePath('fixer-a')), 'a refused spawn cut a tree anyway')
  const fixer = await t.api('POST', `/api/session/${parent}/spawn`, {
    name: 'fixer-a',
    prompt: 'fix the dependabot bump',
    model: 'haiku',
    effort: 'low',
  })
  assert.equal(fixer.workspace, 'fixer-a')
  const dir = t.worktreePath('fixer-a')
  assert.ok(fs.existsSync(dir), `no worktree at ${dir}`)
  assert.equal(branchOf(dir), 'worktree-fixer-a', 'its own branch, or it is not parallel work')
  assert.equal(fs.realpathSync(fixer.path), fs.realpathSync(dir), 'the reply must say where')
  assert.notEqual(fixer.path, beside.path, 'a fixer sharing the parent index is the bug')
  await t.settled(fixer.session)
  assert.match(t.agentLog(), new RegExp(`${fixer.session.slice(0, 8)} new in .* model haiku effort low`),
    'the agent was started on the model and effort asked for')

  // `orch rename`: the manager names the spawn once its work is clear, and the
  // name is what the snapshot's title now says — the rail and `orch ls` both
  // read that field.
  const named = await t.api('POST', `/api/session/${parent}/spawned/${fixer.session}/name`, {
    name: 'story 42: invoice totals',
  })
  assert.equal(named.name, 'story 42: invoice totals')
  assert.equal((await t.session(fixer.session)).title, 'story 42: invoice totals')
  // Your own row is the human's to name, by the same rule as the discard.
  await assert.rejects(
    () => t.api('POST', `/api/session/${parent}/spawned/${parent}/name`, { name: 'me' }),
    /is not a session you spawned/,
  )

  // 4. And the undo takes the tree with it, so a spawn you regret leaves no
  //    checkout on disk with nothing in the rail pointing at it.
  const killed = await t.api('POST', `/api/session/${parent}/spawned/${fixer.session}/discard`)
  assert.equal(killed.kept, undefined, `teardown refused: ${killed.kept}`)
  assert.equal(killed.removed, 'fixer-a')
  await until('the worktree to be gone', async () => !fs.existsSync(dir))
  assert.equal(await t.workspace('fixer-a'), undefined, 'the workspace record outlived its tree')
  assert.equal(await t.session(fixer.session), undefined)

  // 5. Only your own spawns. The parent is a session nobody spawned, which is
  //    every session a person opened — including the one they are sitting in.
  await assert.rejects(
    () => t.api('POST', `/api/session/${parent}/spawned/${parent}/discard`),
    /is not a session you spawned/,
  )
  assert.equal((await t.session(parent)).alive, true, 'it killed the caller')
}
