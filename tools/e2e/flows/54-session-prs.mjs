// A session's linked PRs, edited by hand: the app's route and `orch pr`'s.
//
// The poll links a PR to every session that held its branch, so the edit that
// matters is the one the poll would undo: a PR taken off has to stay off.
// `Inner::edit_session_prs`'s own test holds that against the poll; this holds
// the two routes, the snapshot the rail reads, and the record a restart reads.

import assert from 'node:assert/strict'

export const name = 'a session\'s linked PRs can be added and cleared, and stay that way'

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'linked' })
  await t.settled(session)
  const numbers = async () => (await t.session(session)).prs.map((p) => p.number)

  // The app's route adds by number, on no branch of the session's.
  const added = await t.api('POST', `/api/session/${session}/prs`, { add: [7, 3] })
  assert.deepEqual(added.prs.map((p) => p.number), [3, 7], 'added, in number order')
  assert.deepEqual(await numbers(), [3, 7], 'and the snapshot says so')

  // `orch pr`'s route, with nothing to change, lists.
  const listed = await t.api('POST', `/api/session/${session}/link-prs`, {})
  assert.deepEqual(listed.prs.map((p) => p.number), [3, 7])

  // Removing one, then clearing the rest.
  await t.api('POST', `/api/session/${session}/link-prs`, { remove: [3] })
  assert.deepEqual(await numbers(), [7])
  await t.api('POST', `/api/session/${session}/prs`, { clear: true })
  assert.deepEqual(await numbers(), [], 'cleared')

  await assert.rejects(
    () => t.api('POST', `/api/session/${session}/prs`, { add: [0] }),
    /starts at 1/,
  )

  /* `orch pr <id>` reaches a session this one spawned, and no other: the rule
     `orch rename` and `orch kill` hold. */
  const spawned = await t.api('POST', `/api/session/${session}/spawn`, { worktree: true })
  await t.settled(spawned.session)
  const theirs = await t.api('POST', `/api/session/${session}/link-prs`, { session: spawned.session, add: [4] })
  assert.deepEqual(theirs.prs.map((p) => p.number), [4], 'its spawn\'s PRs')
  const { session: stranger } = await t.api('POST', '/api/worktree', { name: 'stranger' })
  await t.settled(stranger)
  await assert.rejects(
    () => t.api('POST', `/api/session/${session}/link-prs`, { session: stranger, add: [4] }),
    /is not a session you spawned/,
  )

  // Across a restart the cleared list stays cleared, and an added one stays on.
  await t.api('POST', `/api/session/${session}/prs`, { add: [9] })
  await t.restart()
  assert.deepEqual(await numbers(), [9], 'the record kept the edit')
}
