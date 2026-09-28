// A PR route that moves main's branch out takes the stopped sessions about it along.
//
// Closing the last session in main already moves its branch out with it, so the
// PR routes only meet a stopped session in main when that park was skipped. A
// restart is the plain way there: shutting down never parks, since auto-resume
// would bring the session back expecting its branch. With auto-resume off here the
// session comes back stopped, main still holds the branch, and fix-pr has to move
// both — it used to move the branch and leave the conversation in main.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'a PR route takes the stopped sessions in main along'

const VIEWER = 'e2e-viewer'
const PR = 404
const HEAD = 'feature/left-in-main'
const OTHER = 405
const OTHER_HEAD = 'feature/opened-in-main'

export const options = {
  repo: 'acme/monorepo',
  githubToken: 'e2e-token',
  pollSeconds: 30,
}

export async function run(t) {
  const base = branchOf(t.repo)
  git(t.repo, ['switch', '-qc', HEAD])
  fs.writeFileSync(path.join(t.repo, 'README.md'), '# work left in main\n')

  const { session } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(session)
  await t.transcribed(session)

  await t.restart()
  assert.equal(branchOf(t.repo), HEAD, 'a shutdown must not park main')
  await until('the session to be back, stopped, in main', async () => {
    const s = await t.session(session)
    return s && s.workspace === 'main' && !s.alive
  })

  t.setPrs([{ number: PR, head_ref: HEAD }, { number: OTHER, head_ref: OTHER_HEAD }], VIEWER)
  await until('the poll to report the PR as pushable', async () =>
    (await t.pollPrs()).prs.find((p) => p.number === PR)?.head_pushable === true)

  const started = await until('fix-pr to start', async () => {
    try {
      return await t.api('POST', `/api/pr/${PR}/fix-pr`)
    } catch (e) {
      if (/swap is already (running|moving)/.test(String(e.message))) return null
      throw e
    }
  })
  const dir = t.worktreePath(`pr-${PR}`)
  assert.equal(branchOf(dir), HEAD)
  assert.equal(fs.readFileSync(path.join(dir, 'README.md'), 'utf8'), '# work left in main\n')
  await until('main to go back to its base', async () => branchOf(t.repo) === base)

  // The conversation about the branch followed it, beside the run.
  const s = await t.session(session)
  assert.equal(s.workspace, `pr-${PR}`, `the stopped session stayed in ${s.workspace}`)
  assert.equal(fs.realpathSync(s.cwd), fs.realpathSync(dir))
  assert.equal((await t.session(started.session)).workspace, `pr-${PR}`)

  // --- and "open in main" -------------------------------------------------------
  //
  // The same, on the route that switches main to another PR's branch rather than
  // moving this one into a worktree: the branch main is leaving goes out with the
  // stopped session about it, and main switches from base.
  const AGAIN = 'feature/second-left-in-main'
  git(t.repo, ['branch', OTHER_HEAD])
  git(t.repo, ['switch', '-qc', AGAIN])
  const { session: again } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(again)
  await t.transcribed(again)
  await t.restart()
  await until('the second session to be back, stopped, in main', async () => {
    const x = await t.session(again)
    return x && x.workspace === 'main' && !x.alive
  })

  // A restart starts with no poll, and the route answers from the poll.
  await until('the poll to report the second PR', async () =>
    (await t.pollPrs()).prs.some((p) => p.number === OTHER))
  const opened = await until('open in main to run', async () => {
    try {
      return await t.api('POST', `/api/pr/${OTHER}/open`, { where: 'main' })
    } catch (e) {
      if (/swap is already (running|moving)/.test(String(e.message))) return null
      throw e
    }
  })
  assert.equal(opened.workspace, 'main')
  assert.equal(branchOf(t.repo), OTHER_HEAD)
  const moved = (await t.session(again)).workspace
  assert.notEqual(moved, 'main', 'the stopped session stayed in main under another branch')
  assert.equal(branchOf(t.worktreePath(moved)), AGAIN)
}
