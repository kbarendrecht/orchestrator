// Firing a PR flow while main is standing on that PR's own branch.
//
// Opening a PR is **not** a read-only flow with respect to main, deliberately.
// `park_main` will not carry uncommitted work, so a branch with work on it sits in
// main for days, and every PR flow for it is impossible while it does — git allows
// one checkout per branch, so there is no tree to cut. `ensure_pr_worktree` moves
// the branch *and* the work into the tree it was about to create anyway and puts
// main back on base. The one thing still refused is a live session in main, which
// is somebody still using the checkout.
//
// The forge is canned the way flow 08 cans it: a `curl` shim, because
// `forge::github::graphql` is the daemon's only route out.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'a PR whose branch is sitting in main'

const VIEWER = 'e2e-viewer'
const PR = 202
const HEAD = 'feature/in-main'

export const options = {
  repo: 'acme/monorepo',
  githubToken: 'e2e-token',
  pollSeconds: 30,
}

export async function run(t) {
  const base = branchOf(t.repo)
  const readme = path.join(t.repo, 'README.md')

  // Main on the PR's branch with uncommitted work on it: the state a session in
  // main leaves behind, and the one `park_main` correctly refuses to clean up.
  git(t.repo, ['switch', '-qc', HEAD])
  fs.writeFileSync(readme, '# work left in main\n')

  t.setPrs([{ number: PR, head_ref: HEAD }], VIEWER)
  await until('the poll to report the PR as pushable', async () =>
    (await t.pollPrs()).prs.find((p) => p.number === PR)?.head_pushable === true)

  // --- a live session in main is the refusal ----------------------------------

  const { session } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(session)
  await assert.rejects(
    () => t.api('POST', `/api/pr/${PR}/fix-pr`),
    /still open there/,
  )
  assert.ok(!fs.existsSync(t.worktreePath(`pr-${PR}`)), 'a refusal must not cut a worktree')
  assert.equal(branchOf(t.repo), HEAD, 'a refusal must not move main either')

  // Closing it is not enough on its own: the park that follows leaves a dirty main
  // exactly where it is, which is why the branch is still here to be moved.
  await t.api('POST', `/api/session/${session}/kill`)
  await until('main to come free', async () => (await t.workspace('main')).occupant == null)
  assert.equal(branchOf(t.repo), HEAD)

  // --- and then the branch moves out, work and all ----------------------------

  const { session: run } = await t.api('POST', `/api/pr/${PR}/fix-pr`)
  const dir = t.worktreePath(`pr-${PR}`)
  assert.equal(branchOf(dir), HEAD)
  assert.equal(
    fs.readFileSync(path.join(dir, 'README.md'), 'utf8'),
    '# work left in main\n',
    'the uncommitted work stayed behind in main',
  )

  // Main went back to base, and gave the work away rather than keeping a copy.
  await until('main to go back to its base', async () => branchOf(t.repo) === base, {
    context: async () => `main is on ${branchOf(t.repo)}, wanted ${base}`,
  })
  assert.equal(
    git(t.repo, ['status', '--porcelain', '--untracked-files=no']),
    '',
    'main kept tracked work that travelled',
  )

  // Main gave the branch away, so it must stop claiming it: `reconcile` only adds,
  // and two workspaces claiming one branch is what the PR lookup reads.
  const main = await t.workspace('main')
  assert.ok(!main.branches.includes(HEAD), `main still claims ${main.branches}`)
  assert.equal((await t.session(run)).workspace, `pr-${PR}`)
}
