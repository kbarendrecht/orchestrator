#!/usr/bin/env node
// A stand-in for the `claude` binary, for the end-to-end flows.
//
// The daemon spawns its agent as `CommandBuilder::new("claude")` (`src/pty.rs`),
// a PATH lookup, so putting this earlier on PATH substitutes it with no change to
// the daemon. That is the whole reason these flows can run offline and
// deterministically: everything else in them is real — real git worktrees, real
// branch moves, the real HTTP API, the real hook wiring.
//
// It is not a mock of Claude Code. It honours the four things the daemon actually
// depends on, and nothing else:
//
//   1. The argv contract — `--session-id`, `--resume`, `--fork-session`,
//      `--settings`, `--plugin-dir`, and `--worktree` when the daemon delegates
//      the cut.
//   2. The hooks in the settings file the daemon wrote, fired at the right
//      moments. Read from that file rather than hardcoded, so a change to
//      `hooks::write_settings` reaches these flows instead of passing them by.
//   3. A transcript at the path Claude Code would use, because resume, fork and
//      the swap's carry all read it.
//   4. Staying alive on the pty until it is killed, which is how sessions end.
//
// Anything the daemon does not read, it does not do.

import { spawnSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import process from 'node:process'

const argv = process.argv.slice(2)
const flag = (name) => argv.includes(name)
const value = (name) => {
  const i = argv.indexOf(name)
  return i >= 0 ? argv[i + 1] : undefined
}

const settingsPath = value('--settings')
const resumeOf = value('--resume')
const forking = flag('--fork-session')
// `--resume <id>` alone continues that conversation under its own id; a fork
// carries `--session-id <new>` alongside it and is a second conversation. The
// daemon depends on both, so the id is picked the same way it picks it.
const sessionId = value('--session-id') ?? resumeOf
const home = process.env.HOME

// The vendored skills reach a session through `--plugin-dir`, and that flag is
// per *invocation*: a resume without it loses the skill the same id had a moment
// ago. So a spawn site that forgets it is the fault worth catching, and no unit
// test can see it — this is the only place every real spawn goes through.
// Real Claude Code ignores a directory that is not there, silently, which is why
// the layout is checked rather than the flag alone.
const pluginDir = value('--plugin-dir')
if (!pluginDir || !fs.existsSync(path.join(pluginDir, 'skills/orch/SKILL.md'))) {
  throw new Error(
    `spawned without a usable --plugin-dir (got ${pluginDir ?? 'nothing'}) — ` +
      'every site building a claude argv must push `config::session_flags()`',
  )
}

/** Claude Code keys its transcript dir by cwd, slugging every `/` *and* `.`. */
const transcriptDir = (cwd) =>
  path.join(home, '.claude/projects', cwd.replaceAll(/[/.]/g, '-'))

const git = (cwd, args) => {
  const r = spawnSync('git', args, { cwd, encoding: 'utf8' })
  if (r.status !== 0) throw new Error(`git ${args.join(' ')} failed: ${r.stderr}`)
  return r.stdout.trim()
}

// --- the worktree, when the daemon delegated the cut -------------------------
//
// With `.claude/worktrees` configured — Claude Code's own default — the daemon
// does not create the tree; it spawns `claude --worktree [name]` from the main
// checkout and adopts whatever cwd SessionStart reports. Cutting it here is what
// puts that adoption path (and `PENDING_WORKTREE`) under test.
let cwd = process.cwd()
if (flag('--worktree')) {
  const after = argv[argv.indexOf('--worktree') + 1]
  // A name only when one was asked for; otherwise invent one, which is the arm
  // where the daemon does not know the path until the hook reports it.
  const name = after && !after.startsWith('--') ? after : `wt-${sessionId.slice(0, 8)}`
  const main = cwd
  const target = path.join(main, '.claude/worktrees', name)
  git(main, ['worktree', 'add', '-q', '-b', `worktree-${name}`, target, 'HEAD'])
  // Claude Code locks every worktree it cuts, and the lock outlives the session
  // the daemon kills — which is exactly what `git::worktree_remove` has to clear
  // at teardown. Same reason string, so `stale_lock_pid` reads it the same way.
  git(main, ['worktree', 'lock', '--reason',
    `claude session ${name} (pid ${process.pid} start 0)`, target])
  process.chdir(target)
  cwd = target
}

// --- the transcript ----------------------------------------------------------

const dir = transcriptDir(cwd)
fs.mkdirSync(dir, { recursive: true })
const transcript = path.join(dir, `${sessionId}.jsonl`)

const append = (obj) => fs.appendFileSync(transcript, `${JSON.stringify(obj)}\n`)

if (forking && resumeOf) {
  // A fork replays the parent's conversation into a new id, so it has had a turn
  // from birth — the daemon records exactly that, and reading the file back has
  // to agree with it.
  for (const from of [path.join(dir, `${resumeOf}.jsonl`), findTranscript(resumeOf)]) {
    if (from && fs.existsSync(from)) {
      fs.copyFileSync(from, transcript)
      break
    }
  }
}
if (!fs.existsSync(transcript)) {
  // The headers-only file a session that was never typed into owns. `--resume`
  // opens it and exits, which is the case `had_a_turn` exists to tell apart, so
  // a turnless run must leave exactly this and no more.
  append({ type: 'summary', sessionId, cwd })
}

/** The parent's transcript may sit under another slug: a delegated session is
 *  filed under main's before it is adopted into the worktree. */
function findTranscript(id) {
  const root = path.join(home, '.claude/projects')
  if (!fs.existsSync(root)) return undefined
  for (const d of fs.readdirSync(root)) {
    const p = path.join(root, d, `${id}.jsonl`)
    if (fs.existsSync(p)) return p
  }
  return undefined
}

// --- the hooks ---------------------------------------------------------------
//
// Read out of the daemon's own settings file, never hardcoded. A hook that moves
// from http to command form, or changes its URL, then changes what these flows
// exercise — which is the point of driving them at all.

const settings = settingsPath && fs.existsSync(settingsPath)
  ? JSON.parse(fs.readFileSync(settingsPath, 'utf8'))
  : { hooks: {} }

const payload = () => JSON.stringify({
  session_id: sessionId,
  transcript_path: transcript,
  cwd,
  hook_event_name: 'E2E',
})

/** Fire every hook configured for an event, in the form the settings file says.
 *
 *  Awaited, unlike a real hook, and that is deliberate. Claude Code fires these
 *  and moves on, so in production `Stop` may overtake `UserPromptSubmit` and the
 *  daemon is built to tolerate it. A test that tolerated it too would be flaky
 *  rather than lenient: it showed up as a session reaching `your_turn` with
 *  `has_transcript` still false, because the turn that sets the bit had not
 *  landed yet. Ordering here is a property of the harness, not a claim about
 *  Claude Code.
 *
 *  Failures are still swallowed: a hook is an observer, and a dead daemon must
 *  not take the session down with it. */
async function fire(event) {
  for (const group of settings.hooks?.[event] ?? []) {
    for (const hook of group.hooks ?? []) {
      try {
        if (hook.type === 'command') {
          // `SessionStart` is a shell string on purpose — it is how that one hook
          // gets a pipe and a `|| true`. Running it through a shell is what makes
          // an unquoted config path split, so this is the arm that would catch it.
          spawnSync('sh', ['-c', hook.command], { input: payload(), encoding: 'utf8' })
        } else if (hook.type === 'http') {
          const headers = { 'content-type': 'application/json' }
          for (const [k, v] of Object.entries(hook.headers ?? {})) {
            // The daemon writes `$ORCH_SESSION_ID`, expanded from the session's
            // own environment — correlation is by header, never by cwd or pid.
            headers[k] = v.startsWith('$') ? (process.env[v.slice(1)] ?? '') : v
          }
          await fetch(hook.url, { method: 'POST', headers, body: payload() })
        }
      } catch { /* an observer that cannot observe is still not a failure */ }
    }
  }
}

// --- the session ------------------------------------------------------------

await fire('SessionStart')

/** A turn, as the daemon sees one: `UserPromptSubmit` enters `Working` and sets
 *  `had_a_turn`, `Stop` leaves it at `YourTurn`. */
async function turn(text) {
  note(`turn: ${text}`)
  await fire('UserPromptSubmit')
  append({ type: 'user', sessionId, message: { role: 'user', content: text } })
  await new Promise((r) => setTimeout(r, 60))
  append({ type: 'assistant', sessionId, message: { role: 'assistant', content: 'ok' } })
  await fire('Stop')
}

// How many turns to take unprompted, read from a file rather than the
// environment so a flow can change it between spawns without restarting the
// daemon the agent inherits its environment from. Zero is the turnless session
// the fork and resume guards are about.
const turnsFile = process.env.ORCH_E2E_DIR && path.join(process.env.ORCH_E2E_DIR, 'turns')
const autoTurns = turnsFile && fs.existsSync(turnsFile)
  ? Number(fs.readFileSync(turnsFile, 'utf8').trim())
  : 1

/** Wait until the daemon has a record of this session before speaking.
 *
 *  `spawn_session` inserts the record *after* spawning the pty, so an agent that
 *  fires `UserPromptSubmit` inside that window is a hook for a session the daemon
 *  has never heard of, and it is dropped. Real Claude Code takes human-scale
 *  seconds to reach a first prompt and never lands there; this one is ready in
 *  milliseconds, and the symptom was a session sitting at `your_turn` with
 *  `has_transcript` false — the turn that sets the bit had been thrown away while
 *  the `Stop` after it landed.
 *
 *  `sessions.json` is the acknowledgement rather than a sleep: the daemon writes it
 *  when the record changes, so seeing our own id there means the record exists. */
async function recorded() {
  const file = process.env.ORCH_E2E_DIR
    && path.join(process.env.ORCH_E2E_DIR, 'cfg/sessions.json')
  if (!file) return
  const started = Date.now()
  for (let i = 0; i < 200; i++) {
    try {
      if (fs.readFileSync(file, 'utf8').includes(sessionId)) {
        note(`recorded after ${Date.now() - started}ms`)
        return
      }
    } catch { /* not written yet */ }
    await new Promise((r) => setTimeout(r, 25))
  }
  note(`NEVER recorded after ${Date.now() - started}ms`)
}

/** A line in the sandbox's own log, so a flow that fails has the agent's side of
 *  the story next to the daemon's. */
function note(line) {
  if (!process.env.ORCH_E2E_DIR) return
  try {
    fs.appendFileSync(
      path.join(process.env.ORCH_E2E_DIR, 'agent.log'),
      `${sessionId.slice(0, 8)} ${line}\n`,
    )
  } catch { /* the log is a convenience, never a dependency */ }
}

const started = async () => {
  await recorded()
  for (let i = 0; i < autoTurns; i++) await turn(`e2e turn ${i + 1}`)
  // Typed input is a turn too, so a flow can drive one over the pty websocket.
  process.stdin.setEncoding('utf8')
  let buf = ''
  process.stdin.on('data', async (chunk) => {
    buf += chunk
    let nl
    while ((nl = buf.search(/[\r\n]/)) >= 0) {
      const line = buf.slice(0, nl).trim()
      buf = buf.slice(nl + 1)
      if (line) await turn(line)
    }
  })
}

void started()

// Alive until killed, because that is how the daemon ends a session, and
// `spawn_session_confirmed` waits out a grace window on the exit channel before
// its caller commits to anything.
setInterval(() => {}, 1 << 30)
