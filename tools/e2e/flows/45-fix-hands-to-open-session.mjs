// `fix` on a PR whose branch already has a session open asks that session.
//
// The run used to refuse ("fix-pr would fight it"), so the rail hid the button
// whenever a session was open, and the PR you were working on was the one you
// could not press it for. Now the press types `/orchd:fix-pr <n>` into that
// session as your turn, and no unattended run is recorded: nothing new started.

import assert from 'node:assert/strict'
import { git, until } from '../harness.mjs'

export const name = 'fix hands the job to a session already open on the PR'

const VIEWER = 'e2e-viewer'
const PR = 303
const HEAD = 'feature/open-and-red'

export const options = {
  repo: 'acme/monorepo',
  githubToken: 'e2e-token',
  pollSeconds: 30,
}

export async function run(t) {
  git(t.repo, ['branch', HEAD])
  // Failing checks, so `fix` is a button the row offers; pushable by default.
  t.setPrs([{ number: PR, head_ref: HEAD, checks: 'FAILURE' }], VIEWER)
  await until('the poll to report the canned PR', async () =>
    (await t.pollPrs()).prs.some((p) => p.number === PR))

  // An ordinary conversation on the PR's branch: what the `session` chip goes to.
  const { session } = await t.api('POST', `/api/pr/${PR}/open`, { where: 'worktree' })
  await t.settled(session)

  const r = await t.api('POST', `/api/pr/${PR}/fix-pr`)
  assert.equal(r.session, session, 'the press goes to the session already there')
  // A second press straight after is refused: the session is working on the
  // first, and a double click must not type the command twice.
  const again = await t.api('POST', `/api/pr/${PR}/fix-pr`).then(() => null, (e) => String(e))
  assert.ok(again && /mid-turn|working/.test(again), `a second press is refused, got ${again}`)
  await until('the session to be typed the fix', async () =>
    t.agentLog().includes(`turn: /orchd:fix-pr ${PR}`))
  await new Promise((r) => setTimeout(r, 1000))
  const typed = t.agentLog().split(`turn: /orchd:fix-pr ${PR}`).length - 1
  assert.equal(typed, 1, 'the command reached the session once')

  // No run was recorded: the session you were in took the job, nothing unattended
  // started, so nothing may claim a run is going.
  const auto = (await t.state()).automation?.[PR]
  assert.ok(!auto || auto.state !== 'running', `no fix run is recorded, got ${JSON.stringify(auto)}`)
}
