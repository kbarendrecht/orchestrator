// Two workspaces that changed the same file are told so, on the board and to the
// agent.
//
// The unit tests in `overlap.rs` hold the comparison on two real trees. What only
// a real daemon can say is that it is *wired*: that a reconcile recomputes the
// pairs, that the snapshot carries them on both sides, and that the prompt hook
// answers with the note rather than being answered before it ran — which is what
// `hooks::detach` did to every observer route, and why the note had to move off it.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { git, until } from '../harness.mjs'

export const name = 'two workspaces that change one file are told so'

export async function run(t) {
  const made = []
  for (const name of ['left', 'right']) {
    const { session } = await t.api('POST', '/api/worktree', { name })
    await t.settled(session)
    made.push(await t.session(session))
  }
  const [left, right] = made

  // The same committed line in both, a new file at one path in both, and a file
  // only one of them touched.
  for (const s of made) {
    fs.writeFileSync(path.join(s.cwd, 'README.md'), `changed in ${s.workspace}\n`)
    fs.writeFileSync(path.join(s.cwd, 'both-new.txt'), `${s.workspace}\n`)
  }
  fs.writeFileSync(path.join(left.cwd, 'only-left.txt'), 'mine\n')

  for (const s of made) await t.api('POST', `/api/workspace/${s.workspace}/reconcile`)

  const overlaps = async (ws) =>
    (await t.state()).workspaces.find((w) => w.id === ws)?.overlaps ?? []
  const seen = await overlaps(left.workspace)
  assert.deepEqual(
    seen,
    [
      { path: 'README.md', with: right.workspace, conflict: true, plan: null, told: false },
      { path: 'both-new.txt', with: right.workspace, conflict: true, plan: null, told: false },
    ],
    `the left tree is told about both shared files and not its own: ${JSON.stringify(seen)}`,
  )
  assert.deepEqual(
    (await overlaps(right.workspace)).map((o) => [o.path, o.with]),
    [['README.md', left.workspace], ['both-new.txt', left.workspace]],
    'and the right tree about the same two, from its side',
  )
  assert.deepEqual(await overlaps('main'), [], 'main changed nothing and is told nothing')

  // **The first to reach a prompt leads**: the brief to settle it comes with
  // that prompt, once, and nothing is pressed for it.
  const promptAs = (id, text = 'go on') => t.api('POST', '/hooks/user-prompt-submit', {
    session_id: id, hook_event_name: 'UserPromptSubmit', prompt: text,
  })
  const note = (await promptAs(left.id))?.hookSpecificOutput?.additionalContext ?? ''
  assert.ok(
    note.includes('README.md') && note.includes(`\`${right.workspace}\``) && note.includes('you lead'),
    `the lead's prompt carries the brief: ${JSON.stringify(note)}`,
  )
  assert.equal((await promptAs(left.id))?.hookSpecificOutput, undefined, 'and says it once')
  // The other side follows rather than opening a negotiation of its own.
  const follow = (await promptAs(right.id))?.hookSpecificOutput?.additionalContext ?? ''
  assert.ok(follow.includes('leads') && follow.includes(left.id) && !follow.includes('you lead'),
    `the other side is told who leads: ${JSON.stringify(follow)}`)
  await t.api('POST', '/hooks/stop', { session_id: right.id, hook_event_name: 'Stop' })

  // One agent can tell the other, as a turn typed in once the other is idle.
  const tell = (from, to, text) => t.api('POST', `/api/session/${from}/message`, { to, text })
  await t.settled(right.id)
  const told = await tell(left.id, right.id, 'I will take README.md, can you leave it?')
  assert.equal(told.delivered, true, 'the right session was idle, so it went in at once')
  await until('the message to reach the right agent', async () =>
    t.agentLog().includes(`${right.id.slice(0, 8)} turn: orchd: a message from session`))

  // **A state the daemon guesses wrong**: an escaped question leaves it saying
  // "asked a question" with no `Stop` to come. The message waits, the snapshot says
  // so, and the person can send it.
  await t.settled(right.id)
  // During a turn, as a real question comes: a prompt, then the question.
  await promptAs(right.id, 'go')
  await t.api('POST', '/hooks/notification/agent_needs_input', { session_id: right.id, hook_event_name: 'Notification' })
  assert.equal((await t.session(right.id)).state.reason, 'asked_a_question')
  const stuck = await tell(left.id, right.id, 'are you there?')
  assert.equal(stuck.delivered, false, 'a session asking a question got typed into')
  assert.equal((await t.session(right.id)).inbox, 1, 'the waiting message is in the snapshot')
  await t.api('POST', `/api/session/${right.id}/deliver-inbox`)
  await until('the message sent by hand to land', async () => (await t.session(right.id)).inbox === 0)

  // Three each way, then the person decides.
  await t.settled(right.id)
  await tell(left.id, right.id, 'third')
  await assert.rejects(() => tell(left.id, right.id, 'fourth'), /ask the user with `orch ask`/)

  // A session mid-turn is not typed into: the message waits for its `Stop`.
  await promptAs(left.id, 'busy')
  assert.equal((await t.session(left.id)).state.state, 'working')
  const before = t.agentLog().split(`${left.id.slice(0, 8)} turn: orchd: a message`).length
  const queued = await tell(right.id, left.id, 'fine, README.md is yours')
  assert.equal(queued.delivered, false, 'a working session got typed into')
  await t.api('POST', '/hooks/stop', { session_id: left.id, hook_event_name: 'Stop' })
  await until('the queued message to land once the turn ended', async () =>
    t.agentLog().split(`${left.id.slice(0, 8)} turn: orchd: a message`).length > before)
  await t.settled(left.id)

  // It ends in an `orch ask`, and the answer is the plan the card shows.
  const asked = await t.api('POST', `/api/session/${left.id}/ask`, {
    question: 'How do we settle README.md?',
    options: [
      { value: 'overlap-merge-first', label: 'left merges first, right rebases' },
      { value: 'overlap-adapt', label: 'right adapts' },
    ],
  })
  const askId = asked.ask ?? asked.id ?? (await t.session(left.id)).interaction.id
  await t.api('POST', `/api/session/${left.id}/answer`, { ask: askId, answer: 'overlap-merge-first' })
  const plans = (await overlaps(right.workspace)).map((o) => o.plan)
  assert.ok(plans.length && plans.every((p) => p === 'left merges first, right rebases'),
    `both sides of the pair carry the plan: ${JSON.stringify(plans)}`)

  // **Before the conflict**: an edit to a file only the other side changed so far
  // is held once, naming who changed it, and the retry goes through.
  fs.writeFileSync(path.join(right.cwd, 'only-right.txt'), 'theirs\n')
  await t.api('POST', `/api/workspace/${right.workspace}/reconcile`)
  const edit = () => t.api('POST', '/hooks/pre-edit', {
    session_id: left.id, hook_event_name: 'PreToolUse', tool_name: 'Edit',
    tool_input: { file_path: path.join(left.cwd, 'only-right.txt') },
  })
  const held = (await edit())?.hookSpecificOutput ?? {}
  assert.equal(held.permissionDecision, 'deny', `the first edit is held: ${JSON.stringify(held)}`)
  assert.ok(held.permissionDecisionReason.includes('only-right.txt') && held.permissionDecisionReason.includes(right.id),
    `and says who changed it: ${held.permissionDecisionReason}`)
  assert.equal((await edit())?.hookSpecificOutput, undefined, 'the retry goes through')

  // **Against upstream**: a colleague's merge to README.md, which both trees
  // changed, is a conflict the left tree is marked with and its agent told about.
  git(t.root, ['clone', '-q', path.join(t.root, 'origin.git'), 'colleague'])
  const colleague = path.join(t.root, 'colleague')
  fs.writeFileSync(path.join(colleague, 'README.md'), 'merged upstream\n')
  git(colleague, ['-c', 'user.email=c@c', '-c', 'user.name=c', 'commit', '-qam', 'upstream moved'])
  git(colleague, ['push', '-q', 'origin', 'main'])
  git(t.repo, ['fetch', '-q', 'origin'])
  await t.api('POST', `/api/workspace/${left.workspace}/reconcile`)
  const upstream = (await t.state()).workspaces.find((w) => w.id === left.workspace)?.upstream_conflicts
  assert.deepEqual(upstream, ['README.md'], `the left tree conflicts with origin/main: ${JSON.stringify(upstream)}`)
  const rebase = (await promptAs(left.id))?.hookSpecificOutput?.additionalContext ?? ''
  assert.ok(rebase.includes('README.md') && rebase.includes('Rebase onto origin/main'),
    `and its next prompt says to rebase: ${JSON.stringify(rebase)}`)
  await t.api('POST', '/hooks/stop', { session_id: left.id, hook_event_name: 'Stop' })
  await t.settled(left.id)

  // And only inside an overlap: main changed nothing.
  const { session: inMain } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(inMain)
  await assert.rejects(() => tell(left.id, inMain, 'hi'), /changed no file you changed/)

  // Once one side has no live session, neither side is marked.
  await t.api('POST', `/api/session/${right.id}/kill`)
  await until('the right session to stop being live', async () =>
    (await t.session(right.id))?.alive === false)
  assert.deepEqual(await overlaps(left.workspace), [], 'a stopped session is nobody to settle with')
  assert.deepEqual(await overlaps(right.workspace), [], 'and its own tree is not marked either')
}
