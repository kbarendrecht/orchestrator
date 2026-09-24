// The drawer's two kinds of process: a shell you opened, and one the repo declares.
//
// They are the same tab to look at and different things underneath. A shell is a
// pty and nothing else. A declared process comes from config, so it exists as a
// *name* before it exists as a process — which is why `stopped_processes` is in
// the snapshot at all: the restart button is drawn on a tab, and a process with
// `autostart: false` had no tab to draw it on.
//
// The half worth driving end to end is the stop. Declared processes belong to
// whoever was working in that workspace, so they are stopped when the last session
// leaves — a rule that lives in the exit watcher and answers to nothing a caller
// can see.

import assert from 'node:assert/strict'
import { until } from '../harness.mjs'

export const name = 'a shell and a declared process in the drawer'

export const options = {
  processes: [
    {
      name: 'watch',
      // Long-lived and silent: the flow is about the tab and the stop, and a
      // process that prints would only race the ring buffer.
      command: ['sh', '-c', 'while :; do sleep 1; done'],
      autostart: false,
    },
  ],
}

const names = async (t) => (await t.workspace('main')).processes.map((p) => p.name).sort()

export async function run(t) {
  // Declared and not running: a name in the snapshot with no process behind it.
  const idle = await t.workspace('main')
  assert.deepEqual(idle.stopped_processes, ['watch'])
  assert.deepEqual(idle.processes, [])

  const { session } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(session)

  // --- the two tabs -------------------------------------------------------------

  await t.api('POST', '/api/workspace/main/process/watch/restart')
  await until('the declared process to appear', async () =>
    (await names(t)).includes('watch'))
  assert.deepEqual(
    (await t.workspace('main')).stopped_processes,
    [],
    'a running process must not also be offered as a stopped one',
  )

  const { process: shell } = await t.api('POST', '/api/workspace/main/shell')
  await until('the shell to appear', async () =>
    (await t.workspace('main')).processes.some((p) => p.id === shell))
  assert.deepEqual(await names(t), ['shell', 'watch'])

  /* --- `exit` is not a dropped connection (#32) ----------------------------------

     The pane reconnects with backoff when its socket drops, because a dropped one
     used to eat every keystroke under a blinking cursor (#7). A shell somebody
     typed `exit` into closes that same socket — so without a reason on the wire the
     pane retried for the life of the page, against a pty the daemon had already
     reaped. `ws.rs` sends `PTY_EXITED` (4000) for the process ending and nothing
     for a blip, and `term.js` stops only on that code.

     Driven through a real socket rather than asserted in the page, because the
     close code is the contract between the two and neither side can prove it
     alone. */
  const token = t.log().match(/token=([a-z0-9]+)/)?.[1]
  assert.ok(token, 'the daemon never printed its token')
  const closed = await new Promise((resolve, reject) => {
    const sock = new WebSocket(
      `ws://127.0.0.1:${t.port}/ws/pty?token=${token}&target=proc:${shell}`)
    const fail = setTimeout(() => reject(new Error('the shell never closed its socket')), 15_000)
    sock.onopen = () => sock.send('exit\n')
    sock.onclose = (ev) => { clearTimeout(fail); resolve({ code: ev.code, reason: ev.reason }) }
    sock.onerror = () => { clearTimeout(fail); reject(new Error('the pty socket errored')) }
  })
  assert.equal(closed.code, 4000, `a shell that exited must say so, got ${JSON.stringify(closed)}`)
  assert.equal(closed.reason, 'process exited')

  await until('the shell to be reaped', async () => {
    const p = (await t.workspace('main')).processes.find((x) => x.id === shell)
    return !p || !p.alive
  })

  // Closing a tab is a stop, not a hide: the × has to reach the process, or the
  // next one starts beside it.
  await t.api('POST', `/api/process/${shell}/close`)
  await until('the shell tab to go', async () =>
    !(await t.workspace('main')).processes.some((p) => p.id === shell))
  assert.deepEqual(await names(t), ['watch'])

  // --- and they belong to the session --------------------------------------------

  await t.api('POST', `/api/session/${session}/kill`)
  const dead = await until('the declared process to stop with the last session', async () => {
    const p = (await t.workspace('main')).processes.find((x) => x.name === 'watch')
    return p && !p.alive ? p : null
  }, {
    context: async () => `watch is ${JSON.stringify(
      (await t.workspace('main')).processes.find((x) => x.name === 'watch'),
    )}`,
  })
  assert.notEqual(dead.exit_code, null, 'stopped, but nothing reaped it')

  /* Stopped, and still a tab. A managed process that died keeps its row so the
     restart button has something to be drawn on — which is also why it must not
     turn up in `stopped_processes` at the same time, where it would be a second
     way to start the one process. */
  assert.deepEqual(await names(t), ['watch'])
  assert.deepEqual((await t.workspace('main')).stopped_processes, [])
}
