// A PR merges, and the session row says so rather than losing it.
//
// **The poll cannot see this on its own.** It asks GitHub for
// `is:pr is:open author:@me`, so a PR that merges or closes simply stops being in
// the answer — and a row that read `#41 open` would fall back to the session's own
// state with nothing ever saying which of the two happened. Merged is the one
// worth saying: it is the moment the worktree can go.
//
// So the daemon asks, once per disappearance, and this drives all three answers:
// merged, closed, and the one that must *not* record anything — a PR GitHub still
// calls open, which is what a briefly wrong search looks like.
import assert from 'node:assert/strict'

export const name = 'a PR merges, and the row says so'

export const options = {
  // Without `repo` the daemon derives owner/name from the origin remote, which the
  // sandbox has not got. Same reason flow 08 pins it.
  repo: 'acme/monorepo',
  githubToken: 'e2e',
}

/** The ended-PR list, as the snapshot carries it. */
const ended = async (t) => (await t.state()).prs_ended ?? []

export async function run(t) {
  /* **A worktree on the PR's head branch, because that is what bounds the whole
     mechanism.** The daemon keeps asking what became of a vanished PR only while a
     workspace still holds its branch — a PR with no worktree left has no row to
     tell, so there is nothing to find out. Without this the queue empties on the
     poll that fills it and the flow fails for the right reason. */
  const HEAD = 'worktree-merge-me'
  const { session } = await t.api('POST', '/api/worktree', { name: 'merge-me' })
  await t.settled(session)
  assert.equal(
    (await t.state()).workspaces.find((w) => w.id === 'merge-me')?.branch, HEAD,
    'the worktree has to be on the branch the canned PR names',
  )
  t.setPrs([{ number: 4100, head_ref: HEAD, checks: 'SUCCESS' }], 'e2e-viewer')
  await t.pollPrs()
  assert.ok(
    (await t.state()).prs.some((p) => p.number === 4100),
    'the open poll has to see it before it can notice it leaving',
  )
  assert.deepEqual(await ended(t), [], 'nothing has ended yet')

  /* --- gone, and still open to GitHub ---------------------------------------- */

  /* The search can be briefly wrong — an author change, an index lag — and
     recording an outcome from the disappearance alone would mark a live PR as
     ended. The daemon asks, GitHub says `OPEN`, and nothing is written. */
  t.setPrs([], 'e2e-viewer', { ended: { 4100: 'OPEN' } })
  await t.pollPrs()
  assert.deepEqual(
    await ended(t), [],
    'a PR the forge still calls open must not be recorded as ended',
  )

  /* --- merged ---------------------------------------------------------------- */

  t.setPrs([], 'e2e-viewer', { ended: { 4100: 'MERGED' }, heads: { 4100: HEAD } })
  await t.pollPrs()
  const after = await ended(t)
  assert.equal(after.length, 1, `one ended PR, got ${JSON.stringify(after)}`)
  assert.equal(after[0].number, 4100)
  assert.equal(after[0].outcome, 'merged')
  assert.equal(after[0].head_ref, HEAD, 'the head ref is what ties it back to a workspace')
  assert.equal(
    after[0].workspace, 'merge-me',
    'and the snapshot resolves it to the workspace, the way it does an open PR',
  )

  /* --- reopened -------------------------------------------------------------- */

  /* A number can come back: closed, reopened, merged. The open list is the true
     answer while it is in it, so the ended entry has to go — otherwise the row
     would keep saying `merged` about a PR that is open again, and both lists would
     claim it. */
  t.setPrs([{ number: 4100, head_ref: HEAD }], 'e2e-viewer', { ended: { 4100: 'MERGED' } })
  await t.pollPrs()
  assert.deepEqual(
    await ended(t), [],
    'a PR back in the open list is not an ended one',
  )

  /* --- closed, and it replaces rather than duplicates ------------------------ */

  t.setPrs([], 'e2e-viewer', { ended: { 4100: 'CLOSED' }, heads: { 4100: HEAD } })
  await t.pollPrs()
  const last = await ended(t)
  assert.equal(last.length, 1, `still one entry per number, got ${JSON.stringify(last)}`)
  assert.equal(last[0].outcome, 'closed', 'the last answer is the true one')
}
