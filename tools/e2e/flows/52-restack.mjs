// A branch built on another follows it by itself when nobody could lose anything.
//
// `git.rs` holds the rebase itself on a real repo. What only a running daemon can
// say is that it is *wired*: that a reconcile which finds the lower branch moved
// rebases an idle, clean, never-pushed upper branch, and that the agent hears
// about it on its next prompt rather than finding its commit ids changed.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { git, until } from '../harness.mjs'

export const name = 'an idle branch on a stack follows the branch under it'

export async function run(t) {
  const made = []
  for (const name of ['lower', 'upper']) {
    const { session } = await t.api('POST', '/api/worktree', { name })
    await t.settled(session)
    made.push(await t.session(session))
  }
  const [lower, upper] = made
  const commit = (cwd, file, body, msg) => {
    fs.writeFileSync(path.join(cwd, file), body)
    git(cwd, ['add', '-A'])
    git(cwd, ['-c', 'user.email=e@e', '-c', 'user.name=e', 'commit', '-qm', msg])
  }
  const lowerBranch = git(lower.cwd, ['symbolic-ref', '--short', 'HEAD']).trim()
  commit(lower.cwd, 'shared.txt', 'lower 1\n', 'lower 1')

  // Cut a second later than the lower branch, so its reflog says which is on top.
  await new Promise((r) => setTimeout(r, 1100))
  git(upper.cwd, ['checkout', '-q', '-b', 'stacked-up', lowerBranch])
  commit(upper.cwd, 'mine.txt', 'upper\n', 'upper')
  for (const s of made) await t.api('POST', `/api/workspace/${s.workspace}/reconcile`)
  await until('the stack to be seen', async () =>
    (await t.state()).workspaces.find((w) => w.id === upper.workspace)?.stacked_on?.on === lower.workspace)

  // The branch under it moves on.
  commit(lower.cwd, 'shared.txt', 'lower 2\n', 'lower 2')
  const tip = git(lower.cwd, ['rev-parse', 'HEAD']).trim()
  await t.api('POST', `/api/workspace/${lower.workspace}/reconcile`)
  await until('the upper branch to be restacked onto it', async () =>
    git(upper.cwd, ['rev-list', '--count', `HEAD..${tip}`]).trim() === '0')
  assert.equal(fs.readFileSync(path.join(upper.cwd, 'mine.txt'), 'utf8'), 'upper\n', 'its own work is still there')
  assert.equal(git(upper.cwd, ['status', '--porcelain']).trim(), '', 'and the tree is clean, not mid-rebase')

  const note = (await t.api('POST', '/hooks/user-prompt-submit', {
    session_id: upper.id, hook_event_name: 'UserPromptSubmit', prompt: 'go on',
  }))?.hookSpecificOutput?.additionalContext ?? ''
  assert.ok(note.includes('orchd rebased your branch') && note.includes(lowerBranch),
    `its next prompt says it was restacked: ${JSON.stringify(note)}`)
  // That prompt started a turn; end it, or orchd rightly leaves a busy tree alone.
  await t.api('POST', '/hooks/stop', { session_id: upper.id, hook_event_name: 'Stop' })
  await t.settled(upper.id)

  // **Pushed is asked about**, and only once the lower branch pushed: its
  // unpushed commits are in no PR and must not show up in the upper one.
  git(upper.cwd, ['push', '-q', 'origin', 'stacked-up'])
  const before = git(upper.cwd, ['rev-parse', 'HEAD']).trim()
  commit(lower.cwd, 'shared.txt', 'lower 3\n', 'lower 3')
  await t.api('POST', `/api/workspace/${lower.workspace}/reconcile`)
  await until('the stack to see the new commit', async () =>
    (await t.state()).workspaces.find((w) => w.id === upper.workspace)?.stacked_on?.behind === 1)
  await t.api('POST', `/api/workspace/${upper.workspace}/reconcile`)
  assert.equal(git(upper.cwd, ['rev-parse', 'HEAD']).trim(), before, 'a pushed branch is not rebased by itself')
  assert.equal((await t.session(upper.id)).interaction, null, 'nor asked about while the lower one is unpushed')

  git(lower.cwd, ['push', '-q', 'origin', lowerBranch])
  const pushedTip = git(lower.cwd, ['rev-parse', 'HEAD']).trim()
  await t.api('POST', `/api/workspace/${upper.workspace}/reconcile`)
  const asked = await until('the question on the upper session', async () => {
    const i = (await t.session(upper.id)).interaction
    return i && !i.answer ? i : null
  })
  assert.ok(asked.question.includes('force-push'), `it says what a yes does: ${asked.question}`)
  assert.equal((await t.state()).workspaces.find((w) => w.id === upper.workspace)?.stacked_on?.asking, true,
    'and the stack card can say so')
  await t.api('POST', `/api/session/${upper.id}/answer`, { ask: asked.id, answer: 'restack-push' })
  await until('the restack to land on origin', async () =>
    git(upper.cwd, ['rev-list', '--count', `HEAD..${pushedTip}`]).trim() === '0'
      && git(upper.cwd, ['ls-remote', 'origin', 'refs/heads/stacked-up']).split(/\s/)[0]
        === git(upper.cwd, ['rev-parse', 'HEAD']).trim())
  const told = (await t.api('POST', '/hooks/user-prompt-submit', {
    session_id: upper.id, hook_event_name: 'UserPromptSubmit', prompt: 'go on',
  }))?.hookSpecificOutput?.additionalContext ?? ''
  assert.ok(told.includes('force-pushed'), `its next prompt says so: ${JSON.stringify(told)}`)
}
