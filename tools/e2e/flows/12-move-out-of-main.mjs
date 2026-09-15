// Moving a session out of main, in both of the shapes main can be in.
//
// Here because it moves *uncommitted work between trees* and nothing else covers
// the live path: the git half is unit-tested against a real repo, but cutting the
// worktree, parking main, releasing main's claim and relocating the conversation
// only happen together against a real daemon. The two halves are genuinely
// different code paths — main on base has no branch to hand over, so one cuts a
// branch and leaves main where it is, and the other hands its branch over and
// returns main to base.
import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'move a session out of main'

export async function run(t) {
  const { session } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(session)
  const base = branchOf(t.repo)
  console.log('    main is on', base, '(its base) with a session in it')

  fs.writeFileSync(path.join(t.repo, 'README.md'), '# started in main\n')
  fs.writeFileSync(path.join(t.repo, 'scratch.txt'), 'untracked\n')

  const r = await t.api('POST', `/api/session/${session}/out-of-main`)
  console.log('    ->', JSON.stringify(r))
  assert.equal(r.created, true, 'main was on base, so a branch had to be cut')

  const dir = t.worktreePath(r.workspace)
  assert.equal(branchOf(dir), r.branch)
  assert.equal(branchOf(t.repo), base, 'main should not have moved at all')
  assert.equal(r.wip_error ?? null, null)
  assert.equal(fs.readFileSync(path.join(dir, 'README.md'), 'utf8'), '# started in main\n',
    'the work did not travel')
  assert.equal(git(t.repo, ['status', '--porcelain', '--untracked-files=no']), '',
    'main kept tracked work it should have handed over')
  assert.ok(fs.existsSync(path.join(t.repo, 'scratch.txt')), 'the untracked file stays')
  /* **And the answer says so.** `stash create` cannot carry untracked files, so
     they stay in main on base, where they are indistinguishable from base's own —
     and the route computed that list and only logged it. The confirm box was the
     one place the product ever said it, so when the box went the fact went with it.
     The swap beside this already answers with `untracked_left`; so does this now. */
  assert.deepEqual(r.untracked_left, ['scratch.txt'],
    `the move must name what it left behind: ${JSON.stringify(r.untracked_left)}`)

  await until('the conversation to arrive in the worktree', async () =>
    (await t.session(session))?.workspace === r.workspace)
  const s = await t.session(session)
  assert.equal(fs.realpathSync(s.cwd), fs.realpathSync(dir), 'cwd did not follow')
  // The move is a kill and a `--resume`, so the conversation has to have come back
  // and be resumable — a record naming the new workspace is the cheapest half.
  assert.equal(s.state.state, 'your_turn', 'the conversation did not come back idle')
  assert.equal(s.has_transcript, true, 'the conversation did not survive the move')
  await until('main to come free', async () => (await t.workspace('main')).occupant == null)
  console.log('    conversation', session.slice(0, 8), 'is in', s.workspace, '· main released')

  // And the other half: a session in main that is on a branch of its own.
  const { session: second } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(second)
  git(t.repo, ['switch', '-qc', 'feature/real-work'])
  fs.writeFileSync(path.join(t.repo, 'README.md'), '# on a branch\n')
  const r2 = await t.api('POST', `/api/session/${second}/out-of-main`)
  console.log('    ->', JSON.stringify(r2))
  assert.equal(r2.created, false, 'this branch was handed over, not cut')
  assert.equal(r2.branch, 'feature/real-work')
  assert.equal(r2.workspace, 'real-work', 'the tree is named after the branch leaf')
  assert.equal(branchOf(t.repo), base, 'main should be back on base')
  assert.equal(branchOf(t.worktreePath('real-work')), 'feature/real-work')
  assert.equal(
    fs.readFileSync(path.join(t.worktreePath('real-work'), 'README.md'), 'utf8'),
    '# on a branch\n')
  // And the same of the second conversation: the two arms are different code paths,
  // so asserting one of them says nothing about the other.
  await until('the second conversation to arrive', async () =>
    (await t.session(second))?.workspace === r2.workspace)
  const s2 = await t.session(second)
  assert.equal(fs.realpathSync(s2.cwd), fs.realpathSync(t.worktreePath('real-work')))
  assert.equal(s2.state.state, 'your_turn')
  assert.equal(s2.has_transcript, true)
}
