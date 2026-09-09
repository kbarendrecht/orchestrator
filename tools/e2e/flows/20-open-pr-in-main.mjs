// Opening a PR in the main checkout, which moves main's branch under everything.
//
// The other direction from `19-pr-branch-in-main`: that one takes a branch *out*
// of main, this one puts one *in*. Both refusals are the whole safety story, and
// neither is visible from a unit test — a session already in main is somebody
// still using the checkout, and uncommitted work in main is work this would carry
// onto a branch it does not belong to.
//
// The same route serves the worktree, so the `place` switch is exercised here too:
// one arm moves main, the other cuts a tree and touches main not at all.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'open a PR in main'

const VIEWER = 'e2e-viewer'
const PR = 303
const HEAD = 'feature/open-me'

export const options = {
  repo: 'acme/monorepo',
  githubToken: 'e2e-token',
  pollSeconds: 30,
}

export async function run(t) {
  const base = branchOf(t.repo)
  // A real branch: the checkout is moved with `git switch`, so a PR naming one
  // that exists only in the canned answer fails at git rather than at the guard.
  git(t.repo, ['branch', HEAD])
  t.setPrs([{ number: PR, head_ref: HEAD }], VIEWER)
  await until('the poll to report the PR', async () =>
    (await t.pollPrs()).prs.some((p) => p.number === PR))

  // --- work in main is a refusal ----------------------------------------------

  fs.writeFileSync(path.join(t.repo, 'README.md'), '# half-finished\n')
  await assert.rejects(
    () => t.api('POST', `/api/pr/${PR}/open`, { where: 'main' }),
    /uncommitted changes/,
  )
  assert.equal(branchOf(t.repo), base, 'a refusal must not move the checkout')
  git(t.repo, ['checkout', '--', 'README.md'])

  // --- and then it moves ------------------------------------------------------

  const { session, workspace } = await t.api('POST', `/api/pr/${PR}/open`, { where: 'main' })
  assert.equal(workspace, 'main')
  assert.equal(branchOf(t.repo), HEAD)
  await t.settled(session)
  assert.equal((await t.workspace('main')).occupant, session)

  // A second one is refused by the checkout, not by the session count: moving main
  // under a live agent replaces every file it is looking at.
  await assert.rejects(
    () => t.api('POST', `/api/pr/${PR}/open`, { where: 'main' }),
    /already holds main/,
  )
  await assert.rejects(
    () => t.api('POST', `/api/pr/${PR}/open`, { where: 'sideways' }),
    /unknown place/,
  )

  // Closing it hands the checkout back, which is the pair this route relies on:
  // without it main would stand on a PR branch until somebody noticed.
  await t.api('POST', `/api/session/${session}/kill`)
  await until('main to go back to its base', async () => branchOf(t.repo) === base, {
    context: async () => `main is on ${branchOf(t.repo)}, wanted ${base}`,
  })

  // --- the other arm of the same route ----------------------------------------

  const opened = await t.api('POST', `/api/pr/${PR}/open`, { where: 'worktree' })
  assert.equal(opened.workspace, `pr-${PR}`)
  assert.equal(branchOf(t.worktreePath(`pr-${PR}`)), HEAD)
  assert.equal(branchOf(t.repo), base, 'a worktree open must leave main alone')
  await t.settled(opened.session)
}
