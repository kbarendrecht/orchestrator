#!/usr/bin/env node
// Run the end-to-end flows.
//
//   mise run e2e                 every flow
//   mise run e2e -- swap         the ones whose name contains "swap"
//   mise run e2e -- --keep       leave each sandbox behind to read
//
// Each flow gets its own sandbox and its own daemon, so a flow that wedges one
// cannot reach the next. They run one at a time: the point is a readable failure,
// and six daemons racing for CPU makes the timeouts the flaky part.
//
// A flow is a module exporting `run(sandbox)`, and optionally `options` for the
// sandbox it wants. The runner owns creating and stopping it, so a flow that
// throws still leaves no daemon behind.
//
// A flow may also export `pending`, a string saying what it is waiting for. It is
// then listed and not run, and counts in neither total. That exists because a flow
// written before the thing it tests is the cheapest specification there is, and the
// alternative was leaving it red: the pre-commit hook runs this suite every fifth
// qualifying commit and a failure does not reset the counter, so one permanently
// red flow blocks every fifth commit and teaches everybody `--no-verify`. The
// string is required rather than a bare `true` so the listing says *why*, and a
// flow nobody can explain is a flow to delete.

import fs from 'node:fs'
import path from 'node:path'
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { repoRoot, sandbox } from './harness.mjs'

const here = path.dirname(fileURLToPath(import.meta.url))
const args = process.argv.slice(2)
const keep = args.includes('--keep')
const filters = args.filter((a) => !a.startsWith('--'))

// Built here rather than assumed: the flows drive the binary, and a stale one is
// the failure that wastes the most time, because everything still runs.
//
// **Both binaries.** `orchd` is the daemon, and `orch` is what the hooks and the
// agent call — flow 14 runs `orch guard push` directly. This built only the daemon,
// so a change to the guard's own half was tested against whatever `orch` happened
// to be on disk: the grant flow passed while refusing every command, because a
// stale `orch` read a reply shape the daemon no longer sends.
const build = spawnSync('cargo', ['build', '--bin', 'orchd', '--bin', 'orch'], {
  cwd: repoRoot,
  stdio: 'inherit',
})
if (build.status !== 0) process.exit(build.status ?? 1)

const files = fs.readdirSync(path.join(here, 'flows'))
  .filter((f) => f.endsWith('.mjs'))
  .sort()

let pass = 0
const failures = []
const pending = []

for (const file of files) {
  const mod = await import(path.join(here, 'flows', file))
  const name = mod.name ?? file.replace(/\.mjs$/, '')
  if (filters.length && !filters.some((f) => name.includes(f))) continue

  // Named explicitly rather than filtered out silently, so a flow cannot sit
  // pending for a year without anybody reading the reason.
  if (mod.pending) {
    pending.push(name)
    console.log(`  ${name} … pending (${mod.pending})`)
    continue
  }

  const started = Date.now()
  process.stdout.write(`  ${name} … `)
  let t
  let failed = false
  try {
    t = await sandbox(mod.options ?? {})
    await mod.run(t)
    console.log(`ok (${Date.now() - started}ms)`)
    pass++
  } catch (e) {
    failed = true
    failures.push(name)
    console.log('FAILED')
    console.log(`    ${String(e.message ?? e).split('\n').join('\n    ')}`)
  } finally {
    if (t) {
      await t.stop()
      // The sandbox is the evidence: its daemon log and its git state are the
      // only account of what happened, so a failure keeps it.
      if (failed || keep) console.log(`    sandbox: ${t.root}`)
      else t.cleanup()
    }
  }
}

const held = pending.length ? `, ${pending.length} pending` : ''
console.log(`\n${pass} passed, ${failures.length} failed${held}`)
if (failures.length) process.exit(1)
