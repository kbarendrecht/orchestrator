// A Codex session, end to end: started, followed, named, forked, typed into and
// brought back after a restart, through the same routes a Claude Code one takes.
//
// Codex differs from Claude Code in four ways the daemon had to be built around,
// and each is asserted here rather than trusted: its hooks run only when trusted
// by hash (`fake-codex.mjs` refuses to start otherwise), its id is its own and
// learned from a hook, `SessionStart` arrives with the first prompt rather than
// before it, and a line the daemon types has to wait for the pane to settle.

import assert from 'node:assert/strict'
import crypto from 'node:crypto'
import fs from 'node:fs'
import path from 'node:path'
import { until } from '../harness.mjs'

export const name = 'a codex session starts, forks, is typed into, adopted, moved and resumed'
export const options = { codex: true, autoResume: true }

/** What the fake was last started with, for the daemon session `id`. */
const argvOf = (t, id) => JSON.parse(fs.readFileSync(path.join(t.root, 'codex-argv', id), 'utf8'))

export async function run(t) {
  // The probe runs off the boot path, so the second agent appears a moment in.
  const s0 = await until('the board to find both agents', async () => {
    const s = await t.state()
    return s.agents.length === 2 ? s : null
  })
  assert.deepEqual(s0.agents, ['claude', 'codex'])
  assert.equal(s0.default_agent, 'claude', 'nothing picked yet means Claude Code')

  // Picked, so remembered.
  const { session: cx } = await t.api('POST', '/api/worktree', { name: 'cx', agent: 'codex' })
  await t.settled(cx)
  await t.transcribed(cx)
  const s = await t.session(cx)
  assert.equal(s.agent, 'codex')
  assert.equal((await t.state()).default_agent, 'codex', 'a pick is what a plain press starts next')
  const first = argvOf(t, cx)
  assert.equal(first.verb, null, 'a new conversation has no verb')
  // The name Codex keeps in its own index, read back as the row's title.
  await until('the codex title to reach the row', async () =>
    (await t.session(cx))?.title === 'codex: e2e turn 1' || null)

  // A plain press now starts Codex, and Claude Code is still a pick away.
  const { session: plain } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(plain)
  assert.equal((await t.session(plain)).agent, 'codex')
  await t.api('POST', `/api/session/${plain}/kill`)

  // Unless the settings name one, which beats the last pick until unset.
  const settings = await t.api('GET', '/api/config')
  await t.api('POST', '/api/config', { ...settings, default_agent: 'claude' })
  assert.equal((await t.state()).default_agent, 'claude', 'the setting, not the last pick')
  const { session: fixed } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(fixed)
  assert.equal((await t.session(fixed)).agent, 'claude')
  await t.api('POST', `/api/session/${fixed}/kill`)
  await t.api('POST', '/api/config', { ...settings, default_agent: null })
  assert.equal((await t.state()).default_agent, 'codex', 'unset is the last pick again')

  // A fork continues Codex's own id, not the daemon's, and lands its own.
  const { session: fork } = await t.api('POST', `/api/session/${cx}/fork`)
  await t.settled(fork)
  const f = await t.session(fork)
  assert.equal(f.agent, 'codex', 'a fork is continued by the agent that holds it')
  assert.equal(f.forked_from, cx)
  const forked = argvOf(t, fork)
  assert.equal(forked.verb, 'fork')
  assert.equal(forked.continues, first.id, 'codex fork is given the id Codex chose')

  // A turnless pane leaves `starting` without a hook (Codex sends none before a
  // prompt), and a line the daemon types for it lands once the pane is still.
  t.setTurns(0)
  const child = await t.api('POST', `/api/session/${cx}/spawn`, {
    worktree: true,
    name: 'cx-helper',
    prompt: 'look at the invoice totals',
  })
  t.setTurns(1)
  const typed = await t.settled(child.session)
  assert.equal(typed.agent, 'codex', 'a session hands work on to its own kind')
  await until('the typed prompt to be the child\'s first turn', async () =>
    t.agentLog().includes('codex turn: look at the invoice totals') || null)
  await t.transcribed(child.session)

  t.setTurns(0)
  const { session: idle } = await t.api('POST', '/api/worktree', { name: 'cx-idle', agent: 'codex' })
  await t.settled(idle, ['your_turn'])
  assert.equal((await t.session(idle)).state.reason, 'ready', 'up, and waiting for you')
  t.setTurns(1)

  // A Codex conversation from a terminal in this checkout is listed beside the
  // daemon's own and adopted on Codex, while the daemon's own Codex sessions,
  // filed under the ids Codex chose, are not mistaken for outside ones.
  const outside = crypto.randomUUID()
  const day = path.join(t.home, '.codex/sessions/2026/10/11')
  fs.mkdirSync(day, { recursive: true })
  fs.writeFileSync(path.join(day, `rollout-2026-10-11T09-00-00-${outside}.jsonl`), [
    // Real-size: a first turn past the first 8KB went unseen, so this was never
    // listed and a move of it started a new conversation.
    { type: 'session_meta', payload: { id: outside, cwd: t.repo, base_instructions: 'x'.repeat(19 * 1024) } },
    { type: 'event_msg', payload: { type: 'task_started' } },
    { type: 'response_item', payload: { type: 'message', role: 'user', content: [{ type: 'input_text', text: 'hi' }] } },
  ].map((r) => JSON.stringify(r)).join('\n') + '\n')
  fs.appendFileSync(path.join(t.home, '.codex/session_index.jsonl'),
    `${JSON.stringify({ id: outside, thread_name: 'from a terminal' })}\n`)
  await t.api('POST', '/api/external/refresh')
  const listed = (await t.state()).external
  const ext = listed.find((c) => c.id === outside)
  assert.ok(ext, 'the outside Codex conversation is listed')
  assert.equal(ext.agent, 'codex')
  assert.equal(ext.title, 'from a terminal')
  assert.equal(ext.workspace, 'main')
  assert.ok(!listed.some((c) => c.id === first.id), "the daemon's own Codex session is not outside")
  const { session: adopted } = await t.api('POST', `/api/external/${outside}/resume`, { force: true })
  await t.settled(adopted)
  assert.equal((await t.session(adopted)).agent, 'codex', 'adopted on the agent it belongs to')
  assert.deepEqual(
    (({ verb, continues }) => ({ verb, continues }))(argvOf(t, adopted)),
    { verb: 'resume', continues: outside },
  )

  // Moved out of main, which is a kill and a resume in another directory: the
  // case Codex answers with a directory picker unless `-C` names the new one, and
  // the fake refuses a `-C` that is not where it was started.
  const moved = await t.api('POST', `/api/session/${adopted}/out-of-main`)
  assert.equal(moved.session.degraded, false, 'resumed, not forked')
  const there = await t.settled(moved.session.session)
  assert.equal(there.workspace, moved.workspace)
  assert.equal(argvOf(t, moved.session.session).continues, outside, 'the same Codex conversation')

  // And back after a restart, on `codex resume` with the id Codex chose.
  await t.restart()
  const back = await t.settled(cx)
  assert.equal(back.alive, true, 'auto-resume brought it back')
  assert.equal(back.agent, 'codex', 'and as what it was')
  const resumed = argvOf(t, cx)
  assert.equal(resumed.verb, 'resume')
  assert.equal(resumed.continues, first.id)
  assert.equal(resumed.id, first.id, 'the same conversation, not a new one')
}
