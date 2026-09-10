// The host: several checkouts in one window, one daemon each.
//
// **Pending until Stage 3.** This flow is the specification for the architecture
// `multirepo.md` describes, written before it exists because that is the cheapest
// specification there is — and held out of the run rather than left red, because
// the pre-commit hook runs this suite every fifth qualifying commit and a failure
// does not reset the counter. See `run.mjs` on `pending`.
//
// What it asserts, in the order the stages deliver it:
//
//  1. Two checkouts, two daemons. Each answers on its own port, with its own token,
//     and the host lists both.
//  2. A daemon that dies is reported down and restarted **once**. A second death is
//     final and the row offers `reopen`. This is the one place the restart bound is
//     observable.
//  3. `close` is not a crash. Nothing restarts, and the other checkout's sessions
//     are untouched — the `stopping` flag is what makes those two different, and
//     without it a close resurrects months-old conversations through `auto_resume`.
//  4. Containment is refused in either direction, and the refusal names it.
//  5. One repository twice is refused, keyed on the repository the daemon would
//     poll rather than on the `Repos` pair.
//
// Two things this flow deliberately does not do. It never asserts on a *count* of
// child processes, because the hosting model is a decision the plan measures rather
// than a behaviour anybody depends on. And it never waits on a sleep: every step is
// a condition, because a daemon's start is a network fetch away from slow.

import assert from 'node:assert/strict'

export const name = 'a host runs one daemon per checkout'

export const pending = 'the host does not exist yet; Stage 3 removes this line'

export async function run(t) {
  // 1 — two checkouts, two daemons, one list.
  const second = await t.checkout('second')
  const added = await t.host('POST', '/api/host/checkout', { path: second })
  assert.equal(added.ok, true, 'adding a second checkout was refused')

  const listed = await t.until('the host lists both checkouts', async () => {
    const { checkouts } = await t.host('GET', '/api/host/checkouts')
    return checkouts.length === 2 && checkouts.every((c) => c.port && c.token) ? checkouts : null
  })
  for (const c of listed) {
    // The snapshot carries no checkout path of its own — the `main` workspace's
    // does, which is the daemon saying which tree it manages. Checked rather than
    // assumed, because two daemons answering one list is the whole point of this
    // step and a mixed-up port would otherwise pass.
    const state = await t.apiOn(c, 'GET', '/api/state')
    const main = state.workspaces.find((w) => w.id === 'main')
    assert.equal(main.path, c.path, 'a daemon answered for another checkout')
  }

  // 2 — a death is reported, restarted once, and then final.
  const victim = listed.find((c) => c.path === second)
  process.kill(victim.pid, 'SIGKILL')
  const restarted = await t.until('the host restarts the checkout once', async () => {
    const { checkouts } = await t.host('GET', '/api/host/checkouts')
    const row = checkouts.find((c) => c.path === second)
    return row?.live && row.pid !== victim.pid ? row : null
  })
  // A new process mints a new token, so the row's old one is dead — the reason the
  // host socket carries the list and the page substitution cannot be the only
  // channel.
  assert.notEqual(restarted.token, victim.token, 'a restarted daemon reused its token')

  process.kill(restarted.pid, 'SIGKILL')
  const down = await t.until('a second death is final', async () => {
    const { checkouts } = await t.host('GET', '/api/host/checkouts')
    const row = checkouts.find((c) => c.path === second)
    return row && !row.live ? row : null
  })
  assert.equal(down.reopen, true, 'a dead checkout offers no way back')

  // 3 — close is not a crash.
  const reopened = await t.host('POST', '/api/host/checkout/reopen', { path: second })
  assert.equal(reopened.ok, true)
  const live = await t.until('the reopened checkout is up', async () => {
    const { checkouts } = await t.host('GET', '/api/host/checkouts')
    return checkouts.find((c) => c.path === second && c.live) ?? null
  })

  const before = (await t.state()).sessions.length
  await t.host('POST', '/api/host/checkout/close', { path: second })
  await t.until('the checkout is gone from the list', async () => {
    const { checkouts } = await t.host('GET', '/api/host/checkouts')
    return checkouts.every((c) => c.path !== second) ? true : null
  })
  // The assertion the `stopping` flag exists for: a close that reads as a crash
  // restarts the daemon, and a restart runs `auto_resume`.
  assert.equal(
    await t.dead(live.pid),
    true,
    'the daemon was restarted after a close, so a close reads as a crash',
  )
  assert.equal((await t.state()).sessions.length, before, 'closing one checkout disturbed another')

  // 4 — containment, both ways.
  const inside = await t.until('the worktree exists', async () => t.worktreePath('main'))
  const nested = await t.host('POST', '/api/host/checkout', { path: inside })
  assert.equal(nested.ok, false)
  assert.match(nested.error, /inside/i, 'the refusal did not name containment')

  const parent = await t.host('POST', '/api/host/checkout', { path: t.root })
  assert.equal(parent.ok, false, 'a parent of an open checkout was accepted')
  assert.match(parent.error, /contains|inside/i)

  // 5 — one repository twice.
  const sibling = await t.checkout('sibling', { origin: t.originOf(t.repo) })
  const dup = await t.host('POST', '/api/host/checkout', { path: sibling })
  assert.equal(dup.ok, false, 'two checkouts of one repository were accepted')
  assert.match(dup.error, /repository/i, 'the refusal did not name the repository')
}
