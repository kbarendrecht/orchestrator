// An agent upgrade under running sessions, and the bar's one button after it.
//
// The installer is simulated the way Claude Code's own native one works: the
// `claude` on PATH becomes a symlink to a new file, and the old file is left where
// it was. That is the case a `stat` cannot see — the old build still exists — so
// it is the one only `update::mark_stale`'s `Resolve` look can catch, by resolving
// `claude` again the way a spawn would. mise's case, where the old file is deleted,
// is the unit test beside `mark_stale`.
//
// **The assertion with power is the session left alone.** A restart that took
// every session would pass the rest of this; the one opened after the upgrade is
// already on the new build, and respawning it would cost its scrollback for
// nothing.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { until } from '../harness.mjs'

export const name = 'after an upgrade, restart only the sessions it left behind'

const spawns = (t, id) =>
  t.agentLog().split('\n').filter((l) => l.startsWith(id.slice(0, 8)) && l.includes('recorded after')).length

export async function run(t) {
  const { session: a } = await t.api('POST', '/api/worktree', { name: 'first' })
  await t.settled(a)
  const { session: b } = await t.api('POST', '/api/worktree', { name: 'second' })
  await t.settled(b)
  for (const id of [a, b]) assert.equal((await t.session(id)).agent_stale, false, 'current before the upgrade')

  // The upgrade: a new build beside the old one, and the name on PATH repointed.
  const shim = path.join(t.bin, 'claude')
  const next = path.join(t.bin, 'claude-next')
  fs.copyFileSync(shim, next)
  fs.chmodSync(next, 0o755)
  fs.rmSync(shim)
  fs.symlinkSync(next, shim)

  // A new session is what makes the daemon look again, the way it does for real:
  // every spawn re-checks the agent. It is also the one on the new build.
  t.setTurns(0)
  const { session: fresh } = await t.api('POST', '/api/worktree', { name: 'fresh' })
  await t.settled(fresh)

  await until('the two older sessions to be marked stale', async () =>
    (await t.session(a))?.agent_stale && (await t.session(b))?.agent_stale)
  assert.equal((await t.session(fresh)).agent_stale, false, 'the new session is on the new build')

  const before = { a: spawns(t, a), b: spawns(t, b), fresh: spawns(t, fresh) }
  const asked = await t.api('POST', '/api/sessions/restart', { stale: true })
  assert.deepEqual(asked, { queued: 2, now: 2 }, 'the two on the old build, both at their prompt')

  for (const [id, where] of [[a, 'first'], [b, 'second']]) {
    await until(`${where} to be respawned`, async () => spawns(t, id) > before[id === a ? 'a' : 'b'])
    await t.settled(id)
    const s = await t.session(id)
    assert.equal(s.workspace, where, 'it came back where it was')
    assert.equal(s.agent_stale, false, 'and it is on the new build now')
    assert.equal(s.has_transcript, true, 'with its conversation')
  }
  await new Promise((r) => setTimeout(r, 500))
  assert.equal(spawns(t, fresh), before.fresh, 'the session already on the new build was left alone')
}
