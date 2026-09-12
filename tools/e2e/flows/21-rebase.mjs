// The rail's rebase button, and the three states it has to tell apart.
//
// Everything interesting here is a real git outcome rather than a return value: a
// rebase that replays cleanly, one that stops on a conflict and leaves the tree
// mid-rebase, and the abort that puts it back. The route's own guards sit in front
// of all three, and each is the difference between a button and a way to lose
// work.
//
// The base moves for real. `origin` in the sandbox is a bare clone, so a commit
// pushed to it from main is exactly what a colleague's merge looks like — which is
// also the only way to prove the route fetches before it rebases, instead of
// answering from a ref that has not moved since the daemon started.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'rebase a worktree onto a base that moved'

/** A commit on the base, pushed from somebody else's clone.
 *
 *  Not from the sandbox's own checkout: `git push` moves the local
 *  `refs/remotes/origin/main` as a side effect, so pushing from here would leave
 *  the base already current and the route's fetch would be doing nothing. This
 *  clone is the colleague whose merge you have not seen yet, which is the only
 *  state in which "rebase" means anything. */
function advanceBase(t, file, body, message) {
  const at = path.join(t.root, 'colleague')
  if (!fs.existsSync(at)) {
    git(t.root, ['clone', '-q', path.join(t.root, 'origin.git'), 'colleague'])
    git(at, ['config', 'user.email', 'colleague@test'])
    git(at, ['config', 'user.name', 'colleague'])
  }
  git(at, ['pull', '-q', '--ff-only'])
  fs.writeFileSync(path.join(at, file), body)
  git(at, ['add', '-A'])
  git(at, ['commit', '-qm', message])
  git(at, ['push', '-q', 'origin', 'main'])
}

/** Every bank in the repo, straight from git rather than from the snapshot. */
const banks = (t) => git(t.repo, ['for-each-ref', '--format=%(refname)', 'refs/orchd/wip'])

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(session)
  const dir = t.worktreePath('invoice')

  /* Every call below is refused while a session in this workspace is mid-turn,
     which is right: rebasing under a working agent would fight it. This flow
     settled its session once at the top and then assumed it stayed settled, and
     under a full run it does not — about one run in six failed here with
     `<id> is working here`, because the agent's hooks land on the daemon's clock
     and not on this flow's. So idleness is a *condition* before each call rather
     than an assumption, which is the rule every other wait in this suite follows. */
  const act = async (path, body) => {
    await t.settled(session)
    return t.api('POST', `/api/workspace/invoice/${path}`, body)
  }

  // The worktree's own commit, and a base that has moved past it since.
  fs.writeFileSync(path.join(dir, 'feature.txt'), 'mine\n')
  git(dir, ['add', '-A'])
  git(dir, ['commit', '-qm', 'the work in the worktree'])
  advanceBase(t, 'base.txt', 'theirs\n', 'a colleague merged this')

  // --- the clean replay --------------------------------------------------------

  const done = await act('rebase')
  assert.equal(done.warning ?? null, null, 'the fetch failed, so the base may be stale')
  assert.ok(fs.existsSync(path.join(dir, 'base.txt')), 'the base commit did not arrive')
  assert.deepEqual(
    git(dir, ['log', '-2', '--format=%s']).split('\n'),
    ['the work in the worktree', 'a colleague merged this'],
    'the work must replay on top of the base, not the other way round',
  )

  // --- uncommitted work rides along --------------------------------------------
  //
  // It used to be a refusal. The button banks the work, rebases, and puts it back,
  // and the whole of that is invisible from the response unless it goes wrong — so
  // what is asserted is the tree afterwards and the absence of a leftover ref.

  fs.writeFileSync(path.join(dir, 'notes.md'), 'still writing this\n')
  git(dir, ['add', 'notes.md'])
  fs.writeFileSync(path.join(dir, 'feature.txt'), 'mine, edited\n')
  advanceBase(t, 'other.txt', 'theirs again\n', 'a second colleague commit')

  const carried = await act('rebase')
  assert.equal(carried.wip, 'reapplied', `the work did not come back: ${JSON.stringify(carried)}`)
  assert.equal(carried.banked_files, 2)
  assert.equal(fs.readFileSync(path.join(dir, 'feature.txt'), 'utf8'), 'mine, edited\n')
  assert.equal(
    git(dir, ['status', '--porcelain', 'notes.md']),
    'A  notes.md',
    'the index came back with the working tree',
  )
  assert.ok(fs.existsSync(path.join(dir, 'other.txt')), 'and the base arrived')
  assert.equal(banks(t), '', 'a bank that went back leaves no ref')
  assert.equal((await t.workspace('invoice')).banked, null)

  git(dir, ['reset', '-q'])
  fs.rmSync(path.join(dir, 'notes.md'))
  git(dir, ['checkout', '--', 'feature.txt'])

  // --- and when it cannot go back, it is kept -----------------------------------

  /* The uncommitted edit and the base land on the same file, while the worktree's
     own commits are somewhere else entirely — so the rebase replays cleanly and it
     is the *bank* that cannot come back. That is the outcome with nowhere else to
     put the work, and the ref is what keeps it. */
  fs.writeFileSync(path.join(dir, 'README.md'), '# my uncommitted line\n')
  advanceBase(t, 'README.md', '# their line\n', 'they took the readme')

  const kept = await act('rebase')
  assert.equal(kept.wip, 'conflicted', `wanted a kept bank: ${JSON.stringify(kept)}`)
  assert.match(banks(t), /refs\/orchd\/wip\/invoice/)
  const view = await t.workspace('invoice')
  assert.equal(view.banked?.files, 1, `the pane must know: ${JSON.stringify(view.banked)}`)
  assert.equal(view.banked?.at, 'refs/orchd/wip/invoice')
  assert.ok(
    git(dir, ['diff', '--name-only', '--diff-filter=U']).includes('README.md'),
    'both sides belong in the tree, which is where they can be resolved',
  )

  // Neither button pretends the conflict is not there: git cannot apply anything
  // over unmerged paths, and pressing rebase again would have to bank one.
  await assert.rejects(
    () => act('wip/restore'),
    /settle the conflict/,
  )
  await assert.rejects(
    () => act('rebase'),
    /still has conflicts/,
  )

  /* The hand-off, which is a *tell* and not a spawn: the pane belongs to a session,
     so the button gives that session the conflict as an ordinary user turn. The
     fake agent logs whatever is typed at it, which is the only view of a turn a
     flow has. */
  const handed = await act('wip/resolve')
  assert.equal(handed.told, session)
  await until('the session to be told about the conflict', async () =>
    t.agentLog().includes('refs/orchd/wip/invoice'))
  // It lands as a user turn, so the agent is now working — and everything below
  // this line is refused while it is. Which is the right refusal: a rebase under a
  // live turn is the one thing that guard exists for.
  await t.settled(session)

  // Discard is the only verb here git cannot undo, and it is the one that clears
  // the strip when you have taken what you wanted out of the conflict.
  await act('wip/discard')
  assert.equal(banks(t), '', 'the ref outlived its discard')
  assert.equal((await t.workspace('invoice')).banked, null)
  // Settled the way a person would, so the section below starts from a clean tree.
  git(dir, ['checkout', '-f', 'HEAD', '--', 'README.md'])
  assert.equal(git(dir, ['status', '--porcelain']), '')

  // --- a conflict stops, and says so --------------------------------------------

  fs.writeFileSync(path.join(dir, 'README.md'), '# the worktree says this\n')
  git(dir, ['commit', '-qam', 'my line'])
  advanceBase(t, 'README.md', '# main says this\n', 'their line')
  // Uncommitted work as well, somewhere the conflict is not: a stopped rebase owns
  // the tree, so the bank has to wait for it rather than being put back into it.
  fs.writeFileSync(path.join(dir, 'feature.txt'), 'mine, still unsaved\n')

  await assert.rejects(
    () => act('rebase'),
    /stopped on conflicts.*banked at refs\/orchd\/wip\/invoice/s,
  )
  assert.equal((await t.workspace('invoice')).banked?.files, 1, 'the bank must wait, and show')
  // The tree is left mid-rebase on purpose: the conflict is resolved in a session,
  // which is only possible if the rebase is still standing there.
  await until('the pane to report the tree as mid-rebase', async () =>
    (await t.workspace('invoice')).rebasing === true)
  await assert.rejects(
    () => act('rebase'),
    /already stopped part-way/,
  )

  // --- and the abort puts it back ------------------------------------------------

  const undone = await act('rebase/abort')
  await until('the pane to report the rebase gone', async () =>
    (await t.workspace('invoice')).rebasing === false)
  assert.equal(branchOf(dir), 'worktree-invoice')
  assert.equal(fs.readFileSync(path.join(dir, 'README.md'), 'utf8'), '# the worktree says this\n')
  assert.equal(git(dir, ['log', '-1', '--format=%s']), 'my line', 'the abort lost a commit')

  // An abort is the undo of the press, so the work it banked comes home with it —
  // otherwise the tree is back where it started and your edits are not.
  assert.equal(undone.wip, 'reapplied', `the abort left the bank: ${JSON.stringify(undone)}`)
  assert.equal(
    fs.readFileSync(path.join(dir, 'feature.txt'), 'utf8'),
    'mine, still unsaved\n',
    'the abort did not put the uncommitted work back',
  )
  assert.equal(banks(t), '', 'and it dropped the ref')
  assert.equal((await t.workspace('invoice')).banked, null)
}
