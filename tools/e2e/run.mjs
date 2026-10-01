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
const build = spawnSync('cargo', ['build', '-p', 'orchd-serve', '--bin', 'orchd', '--bin', 'orch'], {
  cwd: repoRoot,
  stdio: 'inherit',
})
/* **A spawn that never ran is not a build that failed, and it used to read as
   one.** `spawnSync` reports an unfindable command in `error` and leaves `status`
   null, so the line below exited 1 having printed nothing at all — `stdio:
   inherit` has no output to inherit from a process that does not exist. The
   pre-commit hook then said "an e2e flow failed" above an empty log, which is a
   sentence that sends you reading flows for a toolchain fault.
   `mise` carries `cargo` here, so the environment this bites in is the one with
   no shell profile: a git hook, a launcher, CI without the setup step. Hence the
   second line — the fix is `mise exec -- <command>`, and naming it is the
   difference between a minute and an hour. */
if (build.error) {
  console.error(`e2e: cannot run cargo — ${build.error.message}`)
  console.error('e2e: mise carries the toolchain; try `mise exec -- node tools/e2e/run.mjs`')
  process.exit(1)
}
if (build.status !== 0) process.exit(build.status ?? 1)

const files = fs.readdirSync(path.join(here, 'flows'))
  .filter((f) => f.endsWith('.mjs'))
  .sort()

let pass = 0
const failures = []

for (const file of files) {
  const mod = await import(path.join(here, 'flows', file))
  const name = mod.name ?? file.replace(/\.mjs$/, '')
  if (filters.length && !filters.some((f) => name.includes(f))) continue

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

console.log(`\n${pass} passed, ${failures.length} failed`)
if (failures.length) process.exit(1)
