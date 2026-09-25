// The review queue's "open all", in the queue's own order.
//
// The opener is spawned detached, so the daemon cannot see the browser; what it
// controls is the order the URLs leave in and the gap between them, and without a
// gap every one reached the browser inside a few milliseconds and the tabs came up
// in whatever order the openers finished. A stand-in opener on PATH records what
// it was handed and when, which is the whole of what the daemon promises.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { until } from '../harness.mjs'

export const name = 'open all opens the reviews in order, spaced apart'

export async function run(t) {
  const log = path.join(t.root, 'opened.log')
  /* `open` on macOS, `xdg-open` elsewhere: the first opener `open_external` finds.
     The clock is node's rather than `date +%s%3N`, which is GNU's and prints a
     literal `3N` on the Mac the flows also run on. */
  const record = `require('fs').appendFileSync(process.argv[1], Date.now() + ' ' + process.argv[2] + '\\n')`
  for (const opener of ['xdg-open', 'open']) {
    const shim = path.join(t.bin, opener)
    fs.writeFileSync(shim, `#!/bin/sh\nexec ${JSON.stringify(process.execPath)} -e ${JSON.stringify(record)} ${JSON.stringify(log)} "$1"\n`)
    fs.chmodSync(shim, 0o755)
  }

  const urls = [1, 2, 3, 4].map((n) => `https://example.com/pr/${n}`)
  const r = await t.api('POST', '/api/open-all', { urls })
  assert.equal(r.opened, 4)

  const lines = await until('every opener to have run', () => {
    const got = fs.existsSync(log) ? fs.readFileSync(log, 'utf8').trim().split('\n') : []
    return got.length === 4 ? got : null
  })
  const seen = lines.map((l) => ({ at: Number(l.split(' ')[0]), url: l.split(' ')[1] }))
  assert.deepEqual(seen.map((s) => s.url), urls, 'in the order the queue sent them')
  for (let i = 1; i < seen.length; i++) {
    const gap = seen[i].at - seen[i - 1].at
    assert.ok(gap >= 40, `hand-off ${i} came ${gap}ms after the one before it`)
  }
}
