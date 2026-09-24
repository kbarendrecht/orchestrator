// Quitting while auto-resume is still working through its list (#33).
//
// **The window is the bug.** `auto_resume` spawns one session at a time with a
// stagger, so for the first seconds of a start most of the set has no process —
// and `was_live` is read off live state. A shutdown in that window wrote
// `was_live: false` for every session it had not reached, which is the flag the
// *next* start filters on. Quit once in the window and the rail opens with fewer
// sessions; an in-app update restarts the daemon three times in thirty seconds,
// and it opened with none. Nothing else was lost — the records and the
// transcripts were intact — which is why it reads as "my sessions are gone" with
// no error anywhere.
//
// Driven here rather than unit-tested because the fault only exists across two
// starts: what the first writes at shutdown is exactly what the second reads.
// `09-restart.mjs` restarts a *settled* daemon and therefore never enters the
// window at all.

import assert from 'node:assert/strict'
import { until } from '../harness.mjs'

export const name = 'quit while auto-resume is still going'

export const options = { autoResume: true }

/** How many sessions are live right now.
 *
 *  Asked of the API rather than read out of the log: the sandbox runs the daemon
 *  at `warn`, so `auto-resumed` — an INFO line — is never written, and a flow that
 *  waited for it would wait forever while everything worked.
 */
const liveNow = async (t) => (await t.state()).sessions.filter((s) => s.alive).map((s) => s.id)

export async function run(t) {
  /* Three worktrees, because the assertion is about the ones auto-resume has
     *not* reached: with one there is no window to quit inside. Their own trees,
     since `first_per_workspace` keeps one session per workspace. */
  const names = ['invoice', 'billing', 'ledger']
  const sessions = []
  for (const name of names) {
    const { session } = await t.api('POST', '/api/worktree', { name })
    await t.settled(session)
    sessions.push(session)
  }

  // --- quit between two spawns --------------------------------------------

  await t.restart()
  /* The first one back is the signal, not a clock: the stagger is 1200ms and a
     sleep would be a guess at both ends — too short and nothing has been resumed,
     too long and everything has. The condition is "some but not all", which is the
     window this flow exists to quit inside. */
  const mid = await until('the resume to reach its first session', async () => {
    const live = await liveNow(t)
    return live.length > 0 && live.length < names.length ? live : null
  })
  await t.stop()
  assert.ok(mid.length < names.length, `quit with ${mid.length} of ${names.length} back`)

  // --- and the rest still come back ---------------------------------------

  await t.restart()
  /* Every one of them, which is the whole point: before the fix the sessions the
     stopped daemon had not reached were written `was_live: false` and no later
     start ever offered them again. */
  const back = await until('every session to be live again', async () => {
    const live = await liveNow(t)
    return sessions.every((id) => live.includes(id)) ? live : null
  }, { timeout: 30_000 })

  for (const id of sessions) {
    assert.ok(back.includes(id), `${id.slice(0, 8)} did not come back`)
  }
}
