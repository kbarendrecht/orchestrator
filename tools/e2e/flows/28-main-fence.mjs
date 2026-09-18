// Main's own boundary: a session standing in the checkout is asked before it aims
// git at a worktree the daemon manages.
//
// The mirror of flow 14, and worth its own flow for the same reason: the rule is a
// pure function in `guard.rs` and is tested there, while the thing that can rot
// spans processes. The worktrees dir is *baked into the hook command* by
// `hooks::write_settings` from the repo's own `worktrees_subdir`, so a flag the
// daemon stops passing, or passes with the wrong path, reads as a fence that is
// silently not there — and every unit test still passes, because they hand the
// path in themselves.
//
// It drives `orch` directly rather than through the fake agent, like flow 14: the
// guard is a `command` hook, so its real inputs are the stdin payload and the
// session environment, and a flow can hand it both exactly as Claude Code would.
// **But the argv is the daemon's**, read back out of the `hooks.json` it wrote,
// not spelled here — a flow that passes `--worktrees` itself proves the rule and
// says nothing about whether the fence is ever switched on in a real session.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { execFile, execFileSync } from 'node:child_process'
import { repoRoot, until } from '../harness.mjs'

export const name = 'main asks before it reaches into a worktree'

const ORCH = path.join(repoRoot, 'target/debug/orch')

export async function run(t) {
  // Two trees, because one grant must not open the other — the same "a yes is one
  // folder" property flow 14 proves from the worktree side.
  const { session: first } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(first)
  const { session: second } = await t.api('POST', '/api/worktree', { name: 'payroll' })
  await t.settled(second)
  const invoice = t.worktreePath('invoice')
  const payroll = t.worktreePath('payroll')
  // Whichever layout the sandbox is in, this is what the daemon bakes into the
  // hook — read from the harness rather than spelled, since `.claude/worktrees` is
  // one repo's convention and the flag carries the configured path.
  const trees = path.dirname(invoice)

  // The session that does the reaching. It stands in main, so it has no worktree
  // to be held inside — only these trees to be held out of.
  const { session } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(session)

  const env = {
    PATH: process.env.PATH,
    ORCH_URL: `http://127.0.0.1:${t.port}`,
    ORCH_SESSION_ID: session,
    ORCH_ASK_TOKEN: t.askToken(session),
  }

  /* The flags the daemon really registered, taken from the settings file every
     session is spawned with. Quoted by `hooks::sh_quote`, so the words come back
     out the same way — which is also what keeps this honest about a config dir
     with a space in it. */
  const settings = JSON.parse(fs.readFileSync(path.join(t.cfg, 'hooks.json'), 'utf8'))
  const registered = settings.hooks.PreToolUse
    .map((e) => e.hooks[0].command)
    .find((c) => typeof c === 'string' && c.includes('guard push'))
  assert.ok(registered, 'the daemon registered no push guard at all')
  assert.ok(
    registered.includes(`--worktrees '${trees}'`),
    `the daemon must hand the guard the worktrees dir, got: ${registered}`,
  )
  const flags = registered.split(' guard push ')[1].split(' ').map((w) => w.replace(/^'|'$/g, ''))

  const guard = (command) => {
    const payload = JSON.stringify({ tool_name: 'Bash', tool_input: { command }, cwd: t.repo })
    try {
      execFileSync(ORCH, ['guard', 'push', ...flags],
        { input: payload, env, encoding: 'utf8', stdio: ['pipe', 'pipe', 'pipe'] })
      return { exit: 0, said: '' }
    } catch (e) {
      return { exit: e.status, said: String(e.stderr || '').trim() }
    }
  }
  const rx = (p) => new RegExp(p.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'))

  // Main's own work is its own business, and this is the half that must not have
  // become slower or louder: the fence points inwards, so everything above it in
  // the checkout goes through untouched.
  assert.equal(guard('git status').exit, 0)
  assert.equal(guard(`git -C ${t.repo} log -1`).exit, 0)

  // And the tree under it is not. The refusal carries the way out, because an
  // agent told only "no" has nothing to do next.
  const aimed = `git -C ${invoice} checkout -b topic`
  const before = guard(aimed)
  assert.equal(before.exit, 2, `the guard let ${aimed} through: ${before.said}`)
  assert.match(before.said, rx(`orch outside ${invoice}`))
  // A `cd` in the same tool call moves where the git runs, which is the spelling
  // that has no path on the git command at all.
  assert.equal(guard(`cd ${invoice} && git status`).exit, 2)
  // The push rules are a different rule on the same hook; neither grants the other.
  assert.equal(guard('git push --force').exit, 2)

  // The agent asks, and blocks in its tool call until somebody answers — which is
  // why this is started rather than awaited.
  const askFor = (folder) => new Promise((resolve, reject) => {
    execFile(ORCH, ['outside', folder], { env, encoding: 'utf8' },
      (err, out) => (err ? reject(err) : resolve(out.trim())))
  })
  const question = () => until('the question to reach the session', async () => {
    const s = await t.session(session)
    return s && s.interaction && !s.interaction.answer ? s.interaction : null
  })
  const asking = askFor(invoice)

  // The same box every other question uses: a second permission mechanism beside
  // that one is how two of them come to disagree.
  const ask = await question()
  assert.match(ask.question, rx(invoice))
  assert.deepEqual(ask.options.map((o) => o.value), ['outside-allow', 'outside-no'])
  assert.equal((await t.session(session)).state.state, 'your_turn')

  await t.api('POST', `/api/session/${session}/answer`, { ask: ask.id, answer: 'outside-allow' })
  assert.equal(await asking, 'allowed')

  const after = guard(aimed)
  assert.equal(after.exit, 0, `still refused after the grant: ${after.said}`)
  assert.equal(guard('git push --force').exit, 2, 'the grant must not widen the push rules')

  // One yes is one folder here too. A grant that covered "the worktrees" would
  // hand over every tree on the first question, which is the shape the worktree
  // side already had to be talked out of.
  const next = `git -C ${payroll} status`
  const refused = guard(next)
  assert.equal(refused.exit, 2, `the grant on ${invoice} reached ${payroll}`)
  assert.match(refused.said, rx(`orch outside ${payroll}`))

  const asking2 = askFor(payroll)
  const ask2 = await question()
  assert.notEqual(ask2.id, ask.id, 'the answered question was handed back instead of a new one')
  await t.api('POST', `/api/session/${session}/answer`, { ask: ask2.id, answer: 'outside-allow' })
  assert.equal(await asking2, 'allowed')
  assert.equal(guard(next).exit, 0, `still refused after the grant on ${payroll}`)
  assert.equal(guard(aimed).exit, 0, 'the second grant dropped the first')
}
