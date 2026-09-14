#!/usr/bin/env node
// Every flow, N times in a row, one flow at a time.
//
//   node tools/e2e/deflake.mjs            each flow 8 times
//   node tools/e2e/deflake.mjs --runs 3   fewer, when you only want a smell test
//   node tools/e2e/deflake.mjs --only 08  one flow, by file-name prefix
//
// **A flaky gate is worse than no gate**, because it is what teaches everybody
// `--no-verify`. `docs/traps/gates.md` records the bar this repo already set once —
// seven consecutive clean runs, 168 flow executions — before letting `check.yml`
// run the flows at all. This is that measurement, made repeatable.
//
// One flow at a time, and sequential: the suite runs that way, so flows racing each
// other for CPU would measure a load the gate never actually sees. What it does
// isolate is order — each run gets its own sandbox and its own daemon, so a flow
// that only passes after its predecessor has warmed something up is caught here.
//
// A failing run keeps its log; a passing one is thrown away, because two hundred
// transcripts of success are not evidence of anything.

import { execFileSync, spawnSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const repoRoot = path.resolve(here, '../..')

const argv = process.argv.slice(2)
const flag = (name, fallback) => {
  const i = argv.indexOf(name)
  return i === -1 ? fallback : argv[i + 1]
}
const runs = Number(flag('--runs', 8))
const only = flag('--only', null)
const out = flag('--out', path.join(repoRoot, 'target', 'deflake'))

fs.rmSync(out, { recursive: true, force: true })
fs.mkdirSync(out, { recursive: true })

const dir = path.join(here, 'flows')
const flows = []
for (const f of fs.readdirSync(dir).filter((x) => x.endsWith('.mjs')).sort()) {
  const mod = await import(path.join(dir, f))
  flows.push({ id: f.replace(/\.mjs$/, ''), name: mod.name ?? f })
}

// Built once here, so N runs do not each pay for cargo's freshness check. `run.mjs`
// builds too, and finding it already fresh is the cheap half of what it does.
execFileSync('cargo', ['build', '-p', 'orchd-serve', '--bins'],
  { cwd: repoRoot, stdio: 'inherit' })

let flaky = 0
const began = Date.now()
for (const { id, name } of flows) {
  if (only && !id.includes(only)) continue
  let passed = 0
  const failures = []
  for (let i = 1; i <= runs; i++) {
    const r = spawnSync('node', ['tools/e2e/run.mjs', name],
      { cwd: repoRoot, encoding: 'utf8' })
    const said = `${r.stdout ?? ''}${r.stderr ?? ''}`
    /* `run.mjs` exits 0 when its filter matched nothing, so a name that stops
       matching would otherwise read as a clean sweep of a flow that never ran —
       which is the one failure mode this tool must not have. */
    if (r.status === 0 && /^1 passed, 0 failed/m.test(said)) {
      passed++
    } else {
      failures.push(i)
      fs.writeFileSync(path.join(out, `${id}.run${i}.log`), said)
    }
  }
  const clean = passed === runs
  if (!clean) flaky++
  console.log(`  ${clean ? 'ok  ' : 'FLAKY'}  ${id.padEnd(30)} ${passed}/${runs}`
    + (clean ? '' : `  failed on ${failures.join(', ')}`))
}

const mins = ((Date.now() - began) / 60000).toFixed(1)
console.log(`\ndeflake: ${flaky ? `${flaky} flaky` : 'every flow clean'}`
  + ` · ${runs} runs each · ${mins} min`)
if (flaky) console.log(`  logs: ${out}`)
process.exit(flaky ? 1 : 0)
