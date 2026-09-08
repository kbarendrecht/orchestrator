// Handing a process pane's output to the session beside it.
//
// The drawer's `send to session` is a paste with guards: the daemon decides
// *when* a keystroke means "a prompt" rather than a submit, consent to a
// permission dialog, or an answer to a question. The guards are unit-tested
// against a hand-set state; what only a flow can say is that the text becomes a
// real **turn** in a real agent — the pty write, the 500ms gap before the return,
// and Claude Code's prompt box all being what they are.
//
// It drives the route rather than the SPA, which is where the flows stop: the menu
// that calls this is `paneMenu` in `web/app.js` and no flow sees the page.

import assert from 'node:assert/strict'
import { until } from '../harness.mjs'

export const name = 'tell a session what a pane shows'

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  // At its prompt, which is the one state the text may land in.
  await t.settled(session)

  // What the drawer sends: the pane's own label, then what the pane showed. The
  // label is the repo's config name — nothing here knows what the process is.
  const text = 'ng-watch says:\n\nsrc/app.ts:12:5 - error TS2345: nope'
  const { told } = await t.api('POST', `/api/session/${session}/tell`, { text })
  assert.equal(told, true)

  // A turn, not a stray line: the fake agent logs what it was actually prompted
  // with, so this is the only view of the round trip a flow has.
  await until('the text to arrive as a turn', async () =>
    t.agentLog().includes('error TS2345'))

  // --- and the refusals that make it safe to offer -------------------------

  // Nothing to send is not a turn.
  await assert.rejects(() => t.api('POST', `/api/session/${session}/tell`, { text: '   ' }),
    /nothing to send/)

  // A prompt is a line somebody reads, not a log. The cap is what stops a ring
  // buffer being pasted into an agent's context.
  await assert.rejects(
    () => t.api('POST', `/api/session/${session}/tell`, { text: 'x'.repeat(9 * 1024) }),
    /not the whole buffer/,
  )

  // And a session that is gone has no pty to type into, which is the state a
  // worktree left open all day ends in.
  await t.api('POST', `/api/session/${session}/kill`)
  await until('the session to settle', async () => {
    const s = await t.session(session)
    return s && !s.alive
  })
  await assert.rejects(() => t.api('POST', `/api/session/${session}/tell`, { text: 'hello' }),
    /resume it first/)
}
