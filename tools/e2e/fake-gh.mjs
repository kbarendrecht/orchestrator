#!/usr/bin/env node
// A stand-in for `gh`, for the flows that need a review's writes to land.
//
// Every outward act of a review goes through `forge::github_write`, which shells
// `gh` from the main checkout — so, exactly like the agent and like GitHub's read
// side, it is substituted by putting this earlier on PATH and changing nothing in
// the daemon. What that leaves real is the part worth testing: the argv
// `github_write` builds, the REST path it aims at, and the body it puts on stdin
// footer and all.
//
// Each call is appended to `$ORCH_E2E_DIR/gh.jsonl` — argv and the parsed stdin —
// and a flow reads it with `t.ghCalls()`. Nothing is asserted here: what a write
// must look like belongs in the flow that asked for it, not in the shim.
//
// Unlike the curl shim there is no pass-through. Nothing else in a sandbox calls
// `gh`, and a real one would reach the network with a fixture token.

import fs from 'node:fs'
import path from 'node:path'
import process from 'node:process'

const argv = process.argv.slice(2)

// Drained first, and before anything is written: `github_write` passes the body
// over `--input -` and closes stdin, so exiting without reading races an EPIPE
// into what the daemon reports as a failed write.
let stdin = ''
if (argv.includes('--input')) {
  try { stdin = fs.readFileSync(0, 'utf8') } catch { /* no stdin is fine */ }
}

const dir = process.env.ORCH_E2E_DIR
if (dir) {
  let body = null
  try { body = stdin ? JSON.parse(stdin) : null } catch { body = stdin }
  fs.appendFileSync(path.join(dir, 'gh.jsonl'), `${JSON.stringify({ argv, body })}\n`)
}

// `gh api` answers are parsed as JSON by `Target::api`; `gh pr edit` prints
// nothing and is read by its exit status alone.
process.stdout.write(argv[0] === 'api' ? '{}' : '')
