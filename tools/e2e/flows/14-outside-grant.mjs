// The worktree grant: the guard refuses, the agent asks, you answer, it goes through.
//
// Worth a flow because no unit test can see this shape. The rule is a pure function
// in `src/guard.rs` and is tested there; what happens here spans four processes —
// `orch guard push` reading a `PreToolUse` payload, the daemon holding the grant on
// a session, the ask arriving in the same box every other question uses, and the
// answer releasing an agent that is blocked in a tool call.
//
// It drives `orch` directly rather than through the fake agent: the guard is a
// `command` hook, so the interesting inputs are its stdin payload and the session
// environment, both of which a flow can hand it exactly as Claude Code would.

import assert from 'node:assert/strict'
import path from 'node:path'
import { execFile, execFileSync } from 'node:child_process'
import { repoRoot, until } from '../harness.mjs'

export const name = 'a worktree asks to reach outside itself'

const ORCH = path.join(repoRoot, 'target/debug/orch')

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(session)
  const tree = t.worktreePath('invoice')
  // A second checkout, for the half of this flow that proves one yes is not every
  // yes. The bare clone the sandbox already made is a real directory outside the
  // main checkout, which is exactly the shape the grant must not reach.
  const elsewhere = path.join(t.root, 'origin.git')

  // What Claude Code gives a `command` hook: the session's own environment.
  const env = {
    PATH: process.env.PATH,
    ORCH_URL: `http://127.0.0.1:${t.port}`,
    ORCH_SESSION_ID: session,
    ORCH_ASK_TOKEN: t.askToken(session),
  }
  const guard = (command) => {
    const payload = JSON.stringify({ tool_name: 'Bash', tool_input: { command }, cwd: tree })
    try {
      execFileSync(ORCH, ['guard', 'push', '--base', 'main', '--main', t.repo],
        { input: payload, env, encoding: 'utf8', stdio: ['pipe', 'pipe', 'pipe'] })
      return { exit: 0, said: '' }
    } catch (e) {
      return { exit: e.status, said: String(e.stderr || '').trim() }
    }
  }

  // Refused by default, and the refusal has to carry the way out — an agent told
  // only "no" has nothing to do next.
  const aimed = `git -C ${t.repo} status`
  const before = guard(aimed)
  assert.equal(before.exit, 2, `the guard let ${aimed} through: ${before.said}`)
  assert.match(before.said, /orch outside/)
  // The push rules are a different rule on the same hook; neither grants the other.
  assert.equal(guard('git push --force').exit, 2)

  // The agent asks, and blocks in its tool call until somebody answers — which is
  // why this is started rather than awaited.
  const rx = (p) => new RegExp(p.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'))
  const askFor = (folder) => new Promise((resolve, reject) => {
    execFile(ORCH, ['outside', folder], { env, encoding: 'utf8' },
      (err, out) => (err ? reject(err) : resolve(out.trim())))
  })
  const question = () => until('the question to reach the session', async () => {
    const s = await t.session(session)
    return s && s.interaction && !s.interaction.answer ? s.interaction : null
  })
  const asking = askFor(t.repo)

  // It arrives as an ordinary interaction: the same field, the same box, the same
  // `your_turn`. A second permission mechanism beside that one is how two of them
  // come to disagree.
  const ask = await question()
  assert.match(ask.question, rx(t.repo))
  assert.deepEqual(ask.options.map((o) => o.value), ['outside-allow', 'outside-no'])
  assert.equal((await t.session(session)).state.state, 'your_turn')

  await t.api('POST', `/api/session/${session}/answer`, { ask: ask.id, answer: 'outside-allow' })
  assert.equal(await asking, 'allowed')

  // The same command, now allowed — and still only this rule.
  const after = guard(aimed)
  assert.equal(after.exit, 0, `still refused after the grant: ${after.said}`)
  assert.equal(guard('git push --force').exit, 2, 'the grant must not widen the push rules')

  // Remembered for that folder, so the same question is never asked twice.
  const again = execFileSync(ORCH, ['outside', t.repo], { env, encoding: 'utf8' }).trim()
  assert.equal(again, 'allowed')
  assert.equal((await t.session(session)).interaction.answer, 'outside-allow',
    'a second ask would have replaced the answered one')

  // --- and one yes is not every yes -----------------------------------------
  //
  // The half no unit test can see: a grant lives on the session in the daemon and
  // is read by a *different process* per git command, so "per folder" is only true
  // if the list makes that trip. It was a bool, and the first yes let the session
  // reach every checkout on the machine for the rest of the conversation.
  const other = `git -C ${elsewhere} status`
  const refused = guard(other)
  assert.equal(refused.exit, 2, `the grant on ${t.repo} reached ${elsewhere}`)
  assert.match(refused.said, rx(`orch outside ${elsewhere}`))

  // So it is asked about, on its own, and answered on its own.
  const asking2 = askFor(elsewhere)
  const ask2 = await question()
  assert.notEqual(ask2.id, ask.id, 'the answered question was handed back instead of a new one')
  assert.match(ask2.question, rx(elsewhere))
  await t.api('POST', `/api/session/${session}/answer`, { ask: ask2.id, answer: 'outside-allow' })
  assert.equal(await asking2, 'allowed')
  assert.equal(guard(other).exit, 0, `still refused after the grant on ${elsewhere}`)

  // Both stand, and neither widened the push rules.
  assert.equal(guard(aimed).exit, 0, 'the second grant dropped the first')
  assert.equal(guard('git push --force').exit, 2)
}
