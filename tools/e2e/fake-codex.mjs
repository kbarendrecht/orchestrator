#!/usr/bin/env node
// A stand-in for the `codex` binary, for the end-to-end flows. `fake-claude.mjs`
// is its twin and says why a PATH lookup is all it takes.
//
// It honours what the daemon depends on from Codex, and no more:
//
//   1. The argv: `resume <id>` and `fork <id>` first, `-c key=value` overrides,
//      and the positionals behind `--`, the last of which is the first prompt.
//   2. **Trust.** Real Codex skips a hook it was not told to trust, silently, so a
//      daemon that got the hashes wrong would pass every flow while the rail never
//      moved. This one refuses to start instead, naming the hook. The hashes are
//      its own: `app-server` answers `hooks/list` with them, the way Codex does, so
//      the daemon has to ask rather than guess.
//   3. The hooks, fired from the `-c` overrides rather than hardcoded, with
//      Codex's own payload (its own session id, not the daemon's), and with
//      `SessionStart` arriving inside the first turn, which is the Codex habit the
//      daemon had to be built around.
//   4. A rollout under `$CODEX_HOME/sessions`, written from the first turn, and a
//      name in `session_index.jsonl` once that turn is done.
//   5. The skills: `developer_instructions` must name each one by a file that is
//      there.

import { spawnSync } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import path from 'node:path'
import process from 'node:process'
import readline from 'node:readline'

// --- the argv ----------------------------------------------------------------

const raw = process.argv.slice(2)
const verb = ['resume', 'fork', 'app-server'].includes(raw[0]) ? raw.shift() : null
/** @type {Record<string, unknown>} */
const config = {}
const positional = []
for (let i = 0; i < raw.length; i++) {
  if (raw[i] === '--') {
    positional.push(...raw.slice(i + 1))
    break
  }
  if (raw[i] === '-C') {
    // Real Codex stops at a directory picker when a resume or a fork starts
    // somewhere the conversation did not last run, unless this names it.
    const dir = raw[++i]
    if (fs.realpathSync(dir) !== fs.realpathSync(process.cwd())) {
      throw new Error(`fake-codex: -C ${dir} is not the directory it was started in`)
    }
    config.cd = dir
  } else if (raw[i] === '-c') {
    const kv = raw[++i]
    const eq = kv.indexOf('=')
    setPath(config, kv.slice(0, eq).split('.'), parseToml(kv.slice(eq + 1)))
  } else if (raw[i].startsWith('-')) {
    throw new Error(`fake-codex: unexpected option ${raw[i]}`)
  } else {
    positional.push(raw[i])
  }
}

/* Real Codex's `/new` stops at a worktree picker while its own worktrees are on,
   and a line the daemon types then is taken as the picker's answer. The daemon
   turns them off on every session it starts (`codex::flags`); `app-server` is the
   trust question and carries the hooks alone. */
if (verb !== 'app-server' && config.features?.worktrees !== false) {
  throw new Error('fake-codex: started without features.worktrees=false; real Codex would eat a typed line at the /new picker')
}

function setPath(obj, keys, v) {
  let o = obj
  for (const k of keys.slice(0, -1)) o = o[k] ??= {}
  o[keys.at(-1)] = v
}

/** Enough TOML for an inline value: strings, numbers, booleans, arrays and inline
 *  tables, with bare or quoted keys. What the daemon writes, and a refusal for
 *  anything else, so a value Codex could not read is not one this reads either. */
function parseToml(src) {
  let i = 0
  const ws = () => { while (/\s/.test(src[i] ?? '')) i++ }
  const str = () => {
    let out = ''
    i++
    while (src[i] !== '"') {
      if (i >= src.length) throw new Error(`fake-codex: unterminated string in ${src}`)
      if (src[i] === '\\') {
        const e = src[++i]
        if (e === 'u') { out += String.fromCharCode(parseInt(src.slice(i + 1, i + 5), 16)); i += 5; continue }
        out += { n: '\n', t: '\t', r: '\r', '"': '"', '\\': '\\', b: '\b', f: '\f' }[e] ?? e
        i++
      } else out += src[i++]
    }
    i++
    return out
  }
  const key = () => {
    ws()
    if (src[i] === '"') return str()
    const m = /^[A-Za-z0-9_-]+/.exec(src.slice(i))
    if (!m) throw new Error(`fake-codex: bad key at ${src.slice(i, i + 20)}`)
    i += m[0].length
    return m[0]
  }
  const value = () => {
    ws()
    const c = src[i]
    if (c === '"') return str()
    if (c === '[') {
      i++
      const arr = []
      for (ws(); src[i] !== ']'; ws()) {
        arr.push(value())
        ws()
        if (src[i] === ',') i++
      }
      i++
      return arr
    }
    if (c === '{') {
      i++
      const obj = {}
      for (ws(); src[i] !== '}'; ws()) {
        const k = key()
        ws()
        if (src[i++] !== '=') throw new Error(`fake-codex: expected = in ${src}`)
        obj[k] = value()
        ws()
        if (src[i] === ',') i++
      }
      i++
      return obj
    }
    const m = /^(true|false|-?\d+)/.exec(src.slice(i))
    if (!m) throw new Error(`fake-codex: cannot parse ${src.slice(i, i + 30)}`)
    i += m[0].length
    return m[0] === 'true' ? true : m[0] === 'false' ? false : Number(m[0])
  }
  const v = value()
  ws()
  if (i !== src.length) throw new Error(`fake-codex: trailing input in ${src}`)
  return v
}

// --- the hooks and their trust -------------------------------------------------

const snake = (event) => event.replace(/([a-z])([A-Z])/g, '$1_$2').toLowerCase()

/** Every hook the overrides declare, with the key and hash Codex would give it. */
function declared() {
  const out = []
  for (const [event, groups] of Object.entries(config.hooks ?? {})) {
    if (event === 'state') continue
    groups.forEach((group, g) => (group.hooks ?? []).forEach((handler, h) => {
      out.push({
        event,
        matcher: group.matcher,
        handler,
        key: `/<session-flags>/config.toml:${snake(event)}:${g}:${h}`,
        hash: `sha256:${crypto.createHash('sha256').update(JSON.stringify(handler)).digest('hex')}`,
      })
    }))
  }
  return out
}

if (verb === 'app-server') {
  // Real `app-server` takes no `-C` and exits on one before answering anything,
  // which left every hook untrusted once the daemon started passing it.
  if (config.cd) {
    process.stderr.write("error: unexpected argument '-C' found\n")
    process.exit(2)
  }
  // The JSON-RPC the daemon speaks to learn the hashes. Answers on request and
  // stays up until its input ends, like the real one.
  const rl = readline.createInterface({ input: process.stdin })
  rl.on('line', (line) => {
    const m = JSON.parse(line)
    const reply = (result) => process.stdout.write(`${JSON.stringify({ id: m.id, result })}\n`)
    if (m.method === 'initialize') reply({ userAgent: 'fake-codex' })
    else if (m.method === 'hooks/list') {
      reply({ data: (m.params?.cwds ?? []).map((cwd) => ({
        cwd,
        hooks: declared().map((d) => ({ key: d.key, currentHash: d.hash, trustStatus: 'untrusted' })),
      })) })
    }
  })
  rl.on('close', () => process.exit(0))
} else {
  await session()
}

async function session() {
  if (verb && !config.cd) {
    throw new Error('fake-codex: a resume or a fork without -C stops at the directory picker')
  }
  const hooks = declared()
  const state = config.hooks?.state ?? {}
  for (const d of hooks) {
    if (state[d.key]?.trusted_hash !== d.hash) {
      throw new Error(
        `fake-codex: the ${d.event} hook is not trusted (key ${d.key}); real Codex ` +
          'would skip it in silence; `codex::flags` must hand back the hashes `hooks/list` gave',
      )
    }
  }
  if (hooks.length === 0) throw new Error('fake-codex: spawned with no hooks at all')

  const instructions = String(config.developer_instructions ?? '')
  const orchSkill = /- orchd:orch \(([^)]+)\)/.exec(instructions)
  if (!orchSkill || !fs.existsSync(orchSkill[1])) {
    throw new Error(`fake-codex: developer_instructions name no orch skill file (got ${orchSkill?.[1] ?? 'nothing'})`)
  }

  const codexHome = process.env.CODEX_HOME || path.join(process.env.HOME, '.codex')
  const daemonId = process.env.ORCH_SESSION_ID ?? 'unknown'
  const continues = verb ? positional.shift() : undefined
  const prompt = positional.shift()

  // The same knob as the Claude fake: a resume that will not stay up.
  const dieFile = process.env.ORCH_E2E_DIR && path.join(process.env.ORCH_E2E_DIR, 'die-on-resume')
  if (verb && dieFile && fs.existsSync(dieFile)) {
    const [skip, die] = fs.readFileSync(dieFile, 'utf8').trim().split(',').map(Number)
    if (skip > 0) fs.writeFileSync(dieFile, `${skip - 1},${die}`)
    else if (die > 0) {
      fs.writeFileSync(dieFile, `0,${die - 1}`)
      process.exit(1)
    }
  }

  const sessionsRoot = path.join(codexHome, 'sessions')
  const findRollout = (id) => {
    if (!fs.existsSync(sessionsRoot)) return undefined
    for (const f of fs.readdirSync(sessionsRoot, { recursive: true })) {
      if (String(f).endsWith(`-${id}.jsonl`)) return path.join(sessionsRoot, String(f))
    }
    return undefined
  }
  if (verb === 'resume' && !findRollout(continues)) {
    // Codex answers an unknown id with an error and an exit, which is the case
    // the daemon's `had_a_turn` exists to avoid ever asking for.
    process.stderr.write(`No saved session found with ID ${continues}\n`)
    process.exit(1)
  }
  const id = verb === 'resume' ? continues : crypto.randomUUID()
  const day = path.join(sessionsRoot, '2026', '10', '12')
  const rollout = findRollout(id) ?? path.join(day, `rollout-2026-10-12T00-00-00-${id}.jsonl`)
  if (verb === 'fork') {
    const parent = findRollout(continues)
    if (!parent) {
      process.stderr.write(`No saved session found with ID ${continues}\n`)
      process.exit(1)
    }
    fs.mkdirSync(day, { recursive: true })
    fs.copyFileSync(parent, rollout)
  }
  const append = (obj) => {
    fs.mkdirSync(path.dirname(rollout), { recursive: true })
    fs.appendFileSync(rollout, `${JSON.stringify(obj)}\n`)
  }

  const note = (line) => {
    if (!process.env.ORCH_E2E_DIR) return
    try {
      fs.appendFileSync(path.join(process.env.ORCH_E2E_DIR, 'agent.log'), `${daemonId.slice(0, 8)} codex ${line}\n`)
    } catch { /* a convenience */ }
  }

  /** Fire every trusted hook for `event`, matched against `tool` when it has one. */
  function fire(event, extra = {}, tool) {
    const payload = JSON.stringify({
      session_id: id,
      transcript_path: rollout,
      cwd: process.cwd(),
      hook_event_name: event,
      model: 'fake',
      permission_mode: 'default',
      ...extra,
    })
    const outs = []
    for (const d of hooks.filter((h) => h.event === event)) {
      if (d.matcher && !(tool && new RegExp(`^(${d.matcher})$`).test(tool))) continue
      const r = spawnSync('sh', ['-c', d.handler.command], { input: payload, encoding: 'utf8' })
      if (r.stdout.trim()) outs.push(r.stdout.trim())
    }
    return outs
  }

  // Drawn at once, then still: what `spawn::watch_ready` waits for.
  process.stdout.write('fake codex\r\n> ')

  let first = true
  async function turn(text) {
    note(`turn: ${text}`)
    const turnId = crypto.randomUUID()
    if (first) {
      fire('SessionStart', { source: verb === 'resume' ? 'resume' : 'startup' })
    }
    fire('UserPromptSubmit', { turn_id: turnId, prompt: text })
    // As long as a real one, whose base instructions alone are ~19KB: the
    // daemon looked for the first turn in the first 8KB, found none behind it,
    // and every restart started a new conversation.
    append({ type: 'session_meta', payload: { id, cwd: process.cwd(), base_instructions: 'x'.repeat(19 * 1024) } })
    append({ type: 'event_msg', payload: { type: 'task_started', turn_id: turnId } })
    append({ type: 'response_item', payload: { type: 'message', role: 'user', content: [{ type: 'input_text', text }] } })
    // A tool call, so the pre-edit route and the push guard are exercised through
    // `orch hook` and the guard's own argv.
    const said = fire('PreToolUse', { turn_id: turnId, tool_name: 'Bash', tool_input: { command: 'true' } }, 'Bash')
    if (said.length) note(`pre-tool-use said: ${said.join(' | ')}`)
    fire('PostToolUse', { turn_id: turnId, tool_name: 'Bash', tool_input: { command: 'true' }, tool_response: '' }, 'Bash')
    await held()
    await new Promise((r) => setTimeout(r, 60))
    append({ type: 'response_item', payload: { type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'ok' }] } })
    fire('Stop', { turn_id: turnId, stop_hook_active: false, last_assistant_message: 'ok' })
    if (first) {
      fs.mkdirSync(codexHome, { recursive: true })
      fs.appendFileSync(path.join(codexHome, 'session_index.jsonl'),
        `${JSON.stringify({ id, thread_name: `codex: ${text}`.slice(0, 60), updated_at: new Date().toISOString() })}\n`)
    }
    first = false
  }

  async function held() {
    const file = process.env.ORCH_E2E_DIR && path.join(process.env.ORCH_E2E_DIR, 'hold')
    if (!file || !fs.existsSync(file)) return
    for (let i = 0; i < 600 && fs.existsSync(file); i++) {
      await new Promise((r) => setTimeout(r, 50))
    }
  }

  async function recorded() {
    const file = process.env.ORCH_E2E_DIR && path.join(process.env.ORCH_E2E_DIR, 'cfg/sessions.json')
    if (!file) return
    for (let i = 0; i < 200; i++) {
      try {
        if (fs.readFileSync(file, 'utf8').includes(daemonId)) return
      } catch { /* not written yet */ }
      await new Promise((r) => setTimeout(r, 25))
    }
  }

  const turnsFile = process.env.ORCH_E2E_DIR && path.join(process.env.ORCH_E2E_DIR, 'turns')
  const autoTurns = turnsFile && fs.existsSync(turnsFile) ? Number(fs.readFileSync(turnsFile, 'utf8').trim()) : 1

  // Record the argv a flow may want to assert on: which verb, which id it was
  // told to continue, and the first prompt.
  if (process.env.ORCH_E2E_DIR) {
    fs.mkdirSync(path.join(process.env.ORCH_E2E_DIR, 'codex-argv'), { recursive: true })
    fs.writeFileSync(path.join(process.env.ORCH_E2E_DIR, 'codex-argv', daemonId),
      JSON.stringify({ verb, continues, prompt, id }))
  }

  await recorded()
  // Unprompted turns on every spawn, resumes and forks included, the way the
  // Claude fake takes them: the harness's `settled` counts on it.
  if (prompt) await turn(prompt)
  else for (let i = 0; i < autoTurns; i++) await turn(`e2e turn ${i + 1}`)

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
  setInterval(() => {}, 1 << 30)
}
