#!/usr/bin/env node
// Does the app somebody installs actually work? Driven end to end, on the built
// binary, with no window automation.
//
//   node tools/app-check.mjs <path to the app binary>
//   node tools/app-check.mjs <path> --shots <dir>    also capture the screen (macOS)
//
// **The gap this fills is the one CLAUDE.md admits to**: "Anything only a Mac can
// check … this is the one place a written rule is still the mechanism." Two shipped
// faults landed in it within a week. #16 packed no `orchd`, so the app could not
// start at all; #17 seeded a checkout's state directory with one file, so the rail
// opened empty while 28 session records sat on disk. Every existing gate was green
// for both, because none of them runs the thing.
//
// So this does, and it asserts the two properties those faults broke:
//
//   1. the app starts, spawns a checkout's daemon, and serves its page
//   2. a session created in it is **still there after a restart**
//
// **No clicking, deliberately.** A synthetic click is available on a macOS runner
// and it works — a coordinate click opened the native folder picker — but it is a
// *coordinate*, and a layout change moves the target while the failure reads as
// "the click did nothing". The app already serves an authenticated HTTP API, which
// is the same surface the SPA uses, so every assertion here is deterministic. What
// clicking would add is the window chrome and the native dialogs, and neither is
// what #16 or #17 broke.
//
// **Screenshots are evidence, never an assertion.** `mise run page-check` says why:
// a pixel baseline polices the commits that are *supposed* to change the page, and
// the churn teaches people to update it blind. These are uploaded so a failure can
// be looked at, and nothing compares them.

import { execFile, spawn } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))

const appBin = process.argv[2]
const shotsAt = process.argv.includes('--shots')
  ? process.argv[process.argv.indexOf('--shots') + 1]
  : null

if (!appBin || !fs.existsSync(appBin)) {
  console.error(`usage: app-check.mjs <path to the app binary> [--shots <dir>]`)
  process.exit(2)
}

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'orchd-app-check-'))
/* **`HOME` is left alone, unlike `tools/e2e/harness.mjs`.** Relocating it would
   keep the agent's transcripts out of the real one, but it also relocates mise's
   state — and `env_source` defaults to `Mise`, so every spawn would ask a mise that
   cannot resolve its own shims. The harness escapes that by writing
   `env_source: 'none'` into the config it owns; this drives the *app*, which writes
   its own config for a checkout it has just been handed, so there is no such file
   to pre-empt. Cleaning up afterwards is the smaller compromise, and the slug is
   knowable because Claude Code derives it from the cwd. */
const PROJECTS = path.join(os.homedir(), '.claude', 'projects')
const SANDBOXES = /orchd-app-check-\w+/

/** Transcript directories this check has produced, for a sandbox that is gone.
 *
 *  **Swept on the way in rather than only on the way out**, because the way out
 *  races: the agent can write its last line while the daemon is stopping, after the
 *  sweep has already run. A leftover is then outside the sandbox — which is kept on
 *  a failure deliberately — so it would litter the real home rather than help
 *  anybody. Keyed on the sandbox still existing, so a run in progress elsewhere is
 *  never touched. */
function sweepTranscripts() {
  if (!fs.existsSync(PROJECTS)) return
  for (const name of fs.readdirSync(PROJECTS)) {
    const tag = name.match(SANDBOXES)?.[0]
    if (!tag || fs.existsSync(path.join(os.tmpdir(), tag))) continue
    fs.rmSync(path.join(PROJECTS, name), { recursive: true, force: true })
  }
}
const cfg = path.join(root, 'cfg')
const checkout = path.join(root, 'checkout')
let failed = false
const ok = (what) => console.log(`  ok    ${what}`)
const bad = (what) => { console.log(`  FAIL  ${what}`); failed = true }

const run = (cmd, args, opts = {}) => new Promise((res) => {
  execFile(cmd, args, { encoding: 'utf8', ...opts }, (e, out, err) => res({ e, out, err }))
})

/** Wait for a condition rather than for the clock.
 *
 *  Every wait in this file is one of these, for `docs/e2e.md`'s reason: a start is
 *  a login shell and a git scan away from slow, so a fixed sleep trades flakiness
 *  for slowness and gets both. */
async function until(what, fn, seconds = 60) {
  const deadline = Date.now() + seconds * 1000
  for (;;) {
    const got = await fn()
    if (got) return got
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`)
    await new Promise((r) => setTimeout(r, 250))
  }
}

const shot = async (name) => {
  if (!shotsAt || process.platform !== 'darwin') return
  fs.mkdirSync(shotsAt, { recursive: true })
  await run('screencapture', ['-x', path.join(shotsAt, `${name}.png`)])
}

// --- the app ----------------------------------------------------------------

/** One launch of the app, with its own stdout to read the port back out of.
 *
 *  The port comes from stdout rather than from `orchd.log`, which is rotated per
 *  start — so a second launch's line would have to be told apart from the first's
 *  in a file that has just moved. */
let child = null
let outFile = null

/** A stand-in `claude`, written here rather than asked of the caller.
 *
 *  **`node` by absolute path, not by PATH.** A session's environment is not this
 *  process's: `env_source` defaults to `Mise`, so the daemon asks mise for the
 *  worktree's environment and the agent starts under whatever PATH that composes.
 *  On the machine this was written on, `node` then resolved to a build old enough
 *  to lack `String.replaceAll`, and the agent died on its first line — which the
 *  daemon correctly reported as a session closed before its first turn, with no
 *  hint that the cause was a node three major versions back. `process.execPath` is
 *  the node already running this file, so the agent cannot be handed a different
 *  one. */
function writeAgentShim() {
  const bin = path.join(root, 'bin')
  fs.mkdirSync(bin, { recursive: true })
  const shim = path.join(bin, 'claude')
  const agent = path.join(here, 'e2e', 'fake-claude.mjs')
  // `exec`, so the daemon's kill reaches node itself rather than a shell holding it.
  fs.writeFileSync(shim, `#!/bin/sh\nexec ${JSON.stringify(process.execPath)} ${JSON.stringify(agent)} "$@"\n`)
  fs.chmodSync(shim, 0o755)
  return bin
}

async function launch(tag) {
  outFile = path.join(root, `${tag}.out`)
  const out = fs.openSync(outFile, 'a')
  child = spawn(appBin, [], {
    stdio: ['ignore', out, out],
    env: {
      ...process.env,
      PATH: `${agentBin}${path.delimiter}${process.env.PATH}`,
      ORCHD_CONFIG_DIR: cfg,
      // Skips `adopt_login_path`'s shell round trip *and* keeps this process's
      // PATH — which is what carries the stand-in agent.
      ORCHD_ADOPTED_LOGIN_PATH: '1',
    },
  })
  const port = await until(`${tag}: the page`, () => {
    const said = fs.existsSync(outFile) ? fs.readFileSync(outFile, 'utf8') : ''
    if (/could not open|would start/.test(said)) {
      throw new Error(`the app refused to start:\n${said}`)
    }
    return said.match(/the host is serving the page.*?port.*?(\d{4,5})/)?.[1]
  })
  // `GET /` is the one route with no token, and the page carries it — the same
  // read `tools/shot.mjs` makes rather than a second way of getting one.
  const base = `http://127.0.0.1:${port}`
  const page = await (await fetch(`${base}/`)).text()
  const token = page.match(/token:\s*"([^"]+)"/)?.[1]
  if (!token) throw new Error('the page went out without a token')
  return { base, token }
}

async function stop() {
  if (!child) return
  const pid = child.pid
  const gone = new Promise((r) => child.once('exit', r))
  child.kill('SIGTERM')
  await Promise.race([gone, new Promise((r) => setTimeout(r, 20000))])
  try { process.kill(pid, 'SIGKILL') } catch { /* already gone */ }
  child = null
}

const api = async (base, origin, token, method, route, body) => {
  const res = await fetch(base + route, {
    method,
    headers: {
      'x-orch-token': token,
      origin,
      ...(body ? { 'content-type': 'application/json' } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  })
  const text = await res.text()
  let json
  try { json = JSON.parse(text) } catch { json = { raw: text } }
  return { status: res.status, json }
}

/** The checkout's own daemon: which port it ended up on, and its own token. */
async function childOf(host) {
  const rows = await api(host.base, host.base, host.token, 'GET', '/api/host/checkouts')
  const row = (rows.json.checkouts ?? []).find((c) => c.path === checkout)
    ?? (rows.json.checkouts ?? [])[0]
  if (!row?.port) throw new Error(`no checkout row: ${JSON.stringify(rows.json)}`)
  return { base: `http://127.0.0.1:${row.port}`, token: row.token, row }
}

// --- the run ----------------------------------------------------------------

const agentBin = writeAgentShim()
sweepTranscripts()

try {
  fs.mkdirSync(cfg, { recursive: true })
  fs.mkdirSync(checkout, { recursive: true })
  /* One commit and a **real** `origin`, which is a local bare clone. The daemon
     cuts a worktree from the upstream base ref, so a repo with no remote refuses
     the very first session with "base ref origin/HEAD does not resolve" — which is
     a precondition of the fixture rather than a fault in the app, and is exactly
     what the first local run of this found. `tools/e2e/harness.mjs` builds its
     repo the same way, for the same reason. */
  const git = (at, ...a) => run('git', ['-C', at, ...a])
  await git(checkout, 'init', '-q', '-b', 'main')
  await git(checkout, 'config', 'user.email', 'app-check@test')
  await git(checkout, 'config', 'user.name', 'app-check')
  fs.writeFileSync(path.join(checkout, 'README.md'), '# app-check\n')
  await git(checkout, 'add', '-A')
  await git(checkout, 'commit', '-qm', 'base')
  const origin = path.join(root, 'origin.git')
  await run('git', ['clone', '-q', '--bare', checkout, origin])
  await git(checkout, 'remote', 'add', 'origin', origin)
  await git(checkout, 'fetch', '-q', 'origin')

  // --- 1. it starts ---------------------------------------------------------
  console.log('\n1. the app starts and serves its page')
  let host = await launch('first')
  ok(`serving on ${host.base}`)
  await shot('1-opened')

  // --- 2. a checkout, and a daemon for it -----------------------------------
  console.log('\n2. a checkout gets its own daemon')
  const added = await api(host.base, host.base, host.token, 'POST', '/api/host/checkout',
    { path: checkout })
  if (added.status !== 200) bad(`adding the checkout: ${JSON.stringify(added.json)}`)
  else ok(`added: ${added.json.result?.added}`)

  let kid = await until('the checkout to have a daemon', async () => {
    try { return await childOf(host) } catch { return null }
  })
  ok(`its daemon is on ${kid.base}`)

  // --- 3. a session ---------------------------------------------------------
  console.log('\n3. a session, from the stand-in agent on PATH')
  const made = await api(kid.base, host.base, kid.token, 'POST', '/api/worktree',
    { name: 'app-check' })
  if (made.status !== 200) bad(`creating a session: ${JSON.stringify(made.json)}`)
  const session = made.json.session
  if (!session) throw new Error(`no session came back: ${JSON.stringify(made.json)}`)
  ok(`session ${session.slice(0, 8)} in workspace app-check`)

  /* **Wait for the conversation to exist, not merely the record.** A session
     closed before its first turn is *forgotten* on purpose — `spawn` logs "closed
     before its first turn" and `restore` then drops it as having "no conversation
     to return to". So a restart timed a second too early destroys the very thing
     this is about to assert, and the failure reads exactly like the bug.

     That is not hypothetical: the first run of this file did it, and the harness
     already carries the same warning for the same reason. `has_transcript` is the
     honest condition, because it is what "there is a conversation to return to"
     actually means. */
  await until('the session to have a conversation', async () => {
    const s = await api(kid.base, host.base, kid.token, 'GET', '/api/state')
    return (s.json.sessions ?? []).some((x) => x.id === session && x.has_transcript)
  })
  ok('it has taken a turn and is on disk')
  await shot('2-session')

  // --- 4. the restart -------------------------------------------------------
  //
  // The property #17 broke. Nothing is re-added here: the host remembers its
  // checkouts in `host.json` and opens them again itself, which is the same path
  // that reported "none of the 1 remembered checkouts would start" in #16.
  console.log('\n4. close it, open it again, and look for the session')
  await stop()
  ok('closed')

  host = await launch('second')
  ok(`serving again on ${host.base}`)

  kid = await until('the remembered checkout to come back', async () => {
    try { return await childOf(host) } catch { return null }
  })
  ok(`its daemon is back on ${kid.base}`)

  const after = await until('the state to be readable', async () => {
    const s = await api(kid.base, host.base, kid.token, 'GET', '/api/state')
    return s.status === 200 ? s.json : null
  })
  const found = (after.sessions ?? []).find((x) => x.id === session)
  if (found) ok(`the session survived: ${session.slice(0, 8)} in ${found.workspace}`)
  else bad(`the session is gone. state held: ${JSON.stringify(
    (after.sessions ?? []).map((x) => ({ id: x.id?.slice(0, 8), workspace: x.workspace })))}`)
  await shot('3-reopened')
} catch (e) {
  bad(String(e?.message ?? e))
  if (outFile && fs.existsSync(outFile)) {
    console.log('--- the app said ---')
    console.log(fs.readFileSync(outFile, 'utf8').split('\n').slice(-30).join('\n'))
  }
} finally {
  await stop()
  console.log(`\napp-check: ${failed ? 'FAILED' : 'ok'}`)
  if (failed) console.log(`  sandbox kept: ${root}`)
  else fs.rmSync(root, { recursive: true, force: true })
  // After the sandbox is gone, so this run's own transcripts qualify too.
  sweepTranscripts()
}

process.exit(failed ? 1 : 0)
