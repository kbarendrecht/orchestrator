// Restart sessions in place, so they run the `claude` installed now.
//
// The unit tests hold the queue and the "safe to interrupt" rule; this is the
// half only a real respawn can show. A restart must come back under the same id,
// in the same workspace, with its conversation — a restart that forked or lost
// the transcript would look fine in every unit test and cost you the session.
//
// **And a working agent must not be touched until its turn ends.** That is the
// assertion with power: a restart that ignored the state would still pass the
// idle half, and it would throw away the turn it interrupted.

import assert from 'node:assert/strict'
import { until } from '../harness.mjs'

export const name = 'restart sessions in place, each when its turn ends'

/** How many times the agent for `id` has been spawned. The fake agent notes every
 *  spawn once the daemon has its record, so a respawn is one more line. */
const spawns = (t, id) =>
  t.agentLog().split('\n').filter((l) => l.startsWith(id.slice(0, 8)) && l.includes('recorded after')).length

export async function run(t) {
  const { session: idle } = await t.api('POST', '/api/worktree', { name: 'idle' })
  await t.settled(idle)
  const { session: busy } = await t.api('POST', '/api/worktree', { name: 'busy' })
  await t.settled(busy)
  await t.api('POST', `/api/session/${idle}/rename`, { name: 'keeps its name' })

  const before = { idle: spawns(t, idle), busy: spawns(t, busy) }
  // Where the idle one stands, to hold the respawn to: the agent takes a turn per
  // launch, so this is a finished turn with a clock already running.
  const rested = (await t.session(idle)).state
  assert.equal(rested.reason, 'turn_complete')

  t.hold(true)
  try {
    await t.api('POST', `/api/session/${busy}/tell`, { text: 'think about this' })
    await until('the busy agent to be working', async () =>
      (await t.session(busy))?.state.state === 'working')

    /* **A real `--resume` takes no turn, and this agent takes one per launch.**
       Left at one, the respawned session opens a turn the hold then keeps open,
       and it never reaches its prompt — which reads as a restart that hung. */
    t.setTurns(0)
    const asked = await t.api('POST', '/api/sessions/restart')
    assert.deepEqual(asked, { queued: 2, now: 1 }, 'both are queued, one can go now')

    // The idle one goes straight away, under its own id and with what it had.
    await until('the idle session to be respawned', async () => spawns(t, idle) > before.idle)
    await t.settled(idle)
    const back = await t.session(idle)
    assert.equal(back.workspace, 'idle', 'it came back in its own workspace')
    assert.equal(back.name, 'keeps its name', 'the name survived the respawn')
    assert.equal(back.has_transcript, true, 'the conversation survived the respawn')
    assert.equal(back.restart_queued, false, 'and it is out of the queue')
    /* **Not `ready`.** `SessionStart` opens a fresh record at `ready` with the
       clock at zero, and a restart used to take that too: a finished turn you had
       not looked at yet read as a session that owed you nothing. */
    assert.deepEqual(back.state, rested, 'it came back where it stood, clock and all')

    // The working one has not been touched, and says it is waiting.
    const waiting = await t.session(busy)
    assert.equal(waiting.state.state, 'working', 'a turn in flight is left alone')
    assert.equal(waiting.restart_queued, true, 'the row says a restart is coming')
    assert.equal(spawns(t, busy), before.busy, 'and it was not respawned mid-turn')
  } finally {
    t.hold(false)
  }

  // Its `Stop` is what lets it go.
  await until('the busy session to be respawned once its turn ended', async () =>
    spawns(t, busy) > before.busy)
  await t.settled(busy)
  const done = await t.session(busy)
  assert.equal(done.workspace, 'busy')
  assert.equal(done.restart_queued, false)
  assert.equal(done.state.reason, 'turn_complete', 'the turn it waited for is still unread')

  // One restart each, not one per snapshot: the watcher wakes on every notify, and
  // a flag that outlived its respawn would restart the session forever.
  await new Promise((r) => setTimeout(r, 500))
  assert.equal(spawns(t, idle), before.idle + 1, 'the idle session restarted exactly once')
  assert.equal(spawns(t, busy), before.busy + 1, 'the busy session restarted exactly once')

  // Asking by id for a session that is not running is a refusal, not a count of 0.
  await t.api('POST', `/api/session/${idle}/kill`)
  await until('the idle session to be gone', async () => {
    const s = await t.session(idle)
    return !s || !s.alive
  })
  let refused = ''
  try {
    await t.api('POST', `/api/session/${idle}/restart`)
  } catch (e) {
    refused = e.message
  }
  assert.match(refused, /not running/, 'restart on a closed session says to resume it')
}
