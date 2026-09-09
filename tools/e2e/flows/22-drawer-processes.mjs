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
