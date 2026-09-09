// Main reclaiming its base branch from the worktree that is holding it.
//
// A swap is what hands the base out: main rests on it, a worktree swaps in, and
// the tree leaves holding base. Nothing can put main back until it comes home —
// git allows one checkout per branch, so "main returns to base" is not refused but
// *impossible*, and it surfaced days later as `fatal: 'main' is already used by
// worktree at …` from four calls deep. `park_main` takes it back at the moment it
// is needed, renaming the tree's branch at the commit it already has, and never
// from a tree somebody is working in.
//
// Both halves are invisible from the API: one is a rename in a directory nobody
// asked about, the other is a decision not to make it.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'park reclaims the base a worktree holds'

export async function run(t) {
  const base = branchOf(t.repo)

  // --- the reclaim -----------------------------------------------------------

  const { session } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(session)
  const dir = t.worktreePath('invoice')

  await t.api('POST', '/api/workspace/invoice/swap-main')
  assert.equal(branchOf(t.repo), 'worktree-invoice')
  assert.equal(branchOf(dir), base, 'the swap is what hands the base out')
  const held = git(dir, ['rev-parse', 'HEAD'])
  const readme = fs.readFileSync(path.join(dir, 'README.md'), 'utf8')

  // The conversation went with its branch, so the tree it left is empty and the
  // session to close is the one now in main.
  await until('the conversation to arrive in main', async () =>
    (await t.session(session))?.workspace === 'main')
  await t.api('POST', `/api/session/${session}/kill`)
  await until('main to take its base back', async () => branchOf(t.repo) === base, {
    context: async () => `main is on ${branchOf(t.repo)}, the tree on ${branchOf(dir)}`,
  })

  /* Only the name changed. `release_branch` cuts at the commit the tree already
     has, so every file and every commit stays where it is — and the name is a
     fresh one, because `worktree-invoice` is the branch main is coming off. */
  assert.equal(branchOf(dir), 'worktree-invoice-2')
  assert.equal(git(dir, ['rev-parse', 'HEAD']), held, 'the tree moved commits')
  assert.equal(fs.readFileSync(path.join(dir, 'README.md'), 'utf8'), readme)

  // And it stopped claiming the base. `reconcile` only ever adds to this set, so a
  // claim left behind points every PR flow for that branch at the wrong tree.
  const ws = await t.workspace('invoice')
  assert.ok(!ws.branches.includes(base), `invoice still claims ${ws.branches}`)

  // --- never from a tree somebody is in ---------------------------------------

  const { session: second } = await t.api('POST', '/api/worktree', { name: 'billing' })
  await t.settled(second)
  const bdir = t.worktreePath('billing')
  await t.api('POST', '/api/workspace/billing/swap-main')
  assert.equal(branchOf(bdir), base, 'the second swap hands the base out again')
  await until('the second conversation to arrive in main', async () =>
    (await t.session(second))?.workspace === 'main')

  // Somebody working in the tree that holds the base. The content would not move,
  // but a branch renamed under a live agent is a surprise the log cannot undo.
  const { session: inTree } = await t.api('POST', '/api/session', { workspace: 'billing' })
  await t.settled(inTree)

  await t.api('POST', `/api/session/${second}/kill`)
  // Waited on the log rather than on a state change, because the outcome under
  // test is that nothing happens: the warning is the only thing that says the
  // decision was taken at all.
  await until('the reclaim to say why it did not happen', async () =>
    t.log().includes('and a session is live there'))
  assert.equal(branchOf(t.repo), 'worktree-billing', 'main moved without taking its base')
  assert.equal(branchOf(bdir), base, 'the branch moved under a live session')
}
