#!/usr/bin/env node
// A stand-in for `gh`, so nothing in a sandbox can reach the real one.
//
// The daemon itself writes nothing to GitHub any more: the overlay review's
// replies, 👍s and re-requests went with it, and the review skill posts through
// the agent's own `gh`. What is left is the read side's `gh auth token` fallback
// and whatever an agent shells — and a real `gh` would reach the network with
// your own credential. So it is substituted by putting this earlier on PATH,
// exactly like the agent and like GitHub's read side.
//
// Each call is appended to `$ORCH_E2E_DIR/gh.jsonl` — argv and the parsed stdin —
// for reading by hand when a flow does something unexpected.
//
// Unlike the curl shim there is no pass-through.

import fs from 'node:fs'
import path from 'node:path'
import process from 'node:process'

const argv = process.argv.slice(2)

// Drained first, and before anything is written: a caller that passes a body over
// `--input -` and closes stdin would otherwise race an EPIPE.
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

// `gh api` answers JSON; anything else prints nothing and is read by its exit
// status alone.
process.stdout.write(argv[0] === 'api' ? '{}' : '')
