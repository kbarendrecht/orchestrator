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
  assert.ok(note.includes('orchd replayed your own commits') && note.includes(lowerBranch),
    `its next prompt says it was restacked: ${JSON.stringify(note)}`)
  // That prompt started a turn; end it, or orchd rightly leaves a busy tree alone.
  await t.api('POST', '/hooks/stop', { session_id: upper.id, hook_event_name: 'Stop' })
  await t.settled(upper.id)

  // **The base moves on, and nothing moves by itself**: a colleague's merge that
  // touches neither branch. The card's "Rebase the stack" moves it, bottom first.
  git(t.root, ['clone', '-q', path.join(t.root, 'origin.git'), 'colleague'])
  const colleague = path.join(t.root, 'colleague')
  fs.writeFileSync(path.join(colleague, 'elsewhere.txt'), 'merged upstream\n')
  git(colleague, ['add', '-A'])
  git(colleague, ['-c', 'user.email=c@c', '-c', 'user.name=c', 'commit', '-qm', 'upstream moved'])
  git(colleague, ['push', '-q', 'origin', 'main'])
  git(t.repo, ['fetch', '-q', 'origin'])
  const base = git(t.repo, ['rev-parse', 'origin/main']).trim()
  await t.api('POST', `/api/workspace/${lower.workspace}/reconcile`)
  assert.notEqual(git(lower.cwd, ['rev-list', '--count', `HEAD..${base}`]).trim(), '0', 'not rebased by itself')
  await t.api('POST', `/api/workspace/${upper.workspace}/rebase-stack`)
  await until('the stack to be rebased onto the moved base, bottom first', async () =>
    git(lower.cwd, ['rev-list', '--count', `HEAD..${base}`]).trim() === '0'
      && git(upper.cwd, ['rev-list', '--count', `HEAD..${git(lower.cwd, ['rev-parse', 'HEAD']).trim()}`]).trim() === '0')
  assert.ok(fs.existsSync(path.join(upper.cwd, 'elsewhere.txt')), 'the upper branch has the new base too')
  const rebased = (await t.api('POST', '/hooks/user-prompt-submit', {
    session_id: upper.id, hook_event_name: 'UserPromptSubmit', prompt: 'go on',
  }))?.hookSpecificOutput?.additionalContext ?? ''
  assert.ok(rebased.includes('your stack was rebased'), `and its agent is told: ${JSON.stringify(rebased)}`)
  await t.api('POST', '/hooks/stop', { session_id: upper.id, hook_event_name: 'Stop' })
  await t.settled(upper.id)

  // **A rewrite of a branch others are built on is said before it lands**: the
  // lower branch amends and force-pushes, and its push hears what that leaves the
  // branch above with.
  git(lower.cwd, ['push', '-q', 'origin', lowerBranch])
  fs.writeFileSync(path.join(lower.cwd, 'shared.txt'), 'lower 2, amended\n')
  git(lower.cwd, ['add', '-A'])
  git(lower.cwd, ['-c', 'user.email=e@e', '-c', 'user.name=e', 'commit', '-q', '--amend', '--no-edit'])
  const lowerPush = () => t.api('POST', '/hooks/pre-edit', {
    session_id: lower.id, hook_event_name: 'PreToolUse', tool_name: 'Bash',
    tool_input: { command: `git push --force-with-lease origin ${lowerBranch}` },
  })
  const rewrite = (await lowerPush())?.hookSpecificOutput?.permissionDecisionReason ?? ''
  assert.ok(rewrite.includes('this push rewrites') && rewrite.includes('stacked-up'),
    `the lower branch hears what its rewrite does above it: ${rewrite}`)
  assert.equal((await lowerPush())?.hookSpecificOutput, undefined, 'the retry goes through')
  git(lower.cwd, ['push', '-q', '--force-with-lease', 'origin', lowerBranch])
  await t.api('POST', `/api/workspace/${lower.workspace}/reconcile`)
  await t.api('POST', `/api/workspace/${upper.workspace}/reconcile`)

  // **Pushed is never done unasked, and nothing is asked**: the agent hears
  // the restack at its next push, with the command, and the card can do it.
  git(upper.cwd, ['push', '-q', 'origin', 'stacked-up'])
  const before = git(upper.cwd, ['rev-parse', 'HEAD']).trim()
  commit(lower.cwd, 'shared.txt', 'lower 3\n', 'lower 3')
  git(lower.cwd, ['push', '-q', 'origin', lowerBranch])
  const pushedTip = git(lower.cwd, ['rev-parse', 'HEAD']).trim()
  await t.api('POST', `/api/workspace/${lower.workspace}/reconcile`)
  await until('the stack to see the new commit', async () =>
    ((await t.state()).workspaces.find((w) => w.id === upper.workspace)?.stacked_on?.behind ?? 0) >= 1)
  await t.api('POST', `/api/workspace/${upper.workspace}/reconcile`)
  assert.equal(git(upper.cwd, ['rev-parse', 'HEAD']).trim(), before, 'a pushed branch is not rebased by itself')
  assert.equal((await t.session(upper.id)).interaction, null, 'and nobody is asked')

  const push = () => t.api('POST', '/hooks/pre-edit', {
    session_id: upper.id, hook_event_name: 'PreToolUse', tool_name: 'Bash',
    tool_input: { command: 'git push --force-with-lease' },
  })
  const held = (await push())?.hookSpecificOutput ?? {}
  assert.equal(held.permissionDecision, 'deny', `its push is held once: ${JSON.stringify(held)}`)
  assert.ok(held.permissionDecisionReason.includes('Restack first: `git rebase'),
    `with the command: ${held.permissionDecisionReason}`)
  assert.equal((await push())?.hookSpecificOutput, undefined, 'the retry goes through')

  // The card's "Restack it now": a press, so a pushed branch goes up with a lease.
  await t.api('POST', `/api/workspace/${upper.workspace}/restack`)
  assert.equal(git(upper.cwd, ['rev-list', '--count', `HEAD..${pushedTip}`]).trim(), '0', 'restacked onto what it pushed')
  assert.equal(git(upper.cwd, ['ls-remote', 'origin', 'refs/heads/stacked-up']).split(/\s/)[0],
    git(upper.cwd, ['rev-parse', 'HEAD']).trim(), 'and on origin')
}
