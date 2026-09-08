// The rail's default review verb: a pane pass over a PR's threads.
//
// `/orchd:handle-review` is what the `resolve` button starts, and it is the older
// shape deliberately — one agent in the PR's worktree with a person watching,
// rather than the triage-into-cards flow, because the cards are not good enough to
// be the only way through a review yet.
//
// Worth a flow because nothing else covers this path at all: it is the only spawn
// that goes through `spawn_command_session`, which had *no* caller but a test until
// now and whose prompt lookup could only ever bail. What a unit test cannot see is
// the part this asserts — a canned PR turning into a real worktree on the PR's real
// branch with the skill typed into the agent standing in it.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'handle a review in a pane'

const VIEWER = 'e2e-viewer'
const PR = 202
const HEAD = 'feature/reviewed'

export const options = {
  // Same reason as the fix-pr flow: without `repo` the daemon derives owner/name
  // from a local path and turns polling off, so `pr_from_poll` would never see it.
  repo: 'acme/monorepo',
  githubToken: 'e2e-token',
  pollSeconds: 30,
}

export async function run(t) {
  // A real branch: the worktree is cut with `git worktree add <path> <branch>`, so
  // a PR naming one that exists only in the canned answer fails at git rather than
  // at the interesting part.
  git(t.repo, ['branch', HEAD])
  t.setPrs([{ number: PR, head_ref: HEAD }], VIEWER)
  await until('the poll to report the canned PR', async () =>
    (await t.pollPrs()).prs.some((p) => p.number === PR))

  const { session } = await t.api('POST', `/api/pr/${PR}/handle-review`)
  await t.settled(session)

  // A worktree pinned to the PR's head branch, not a fresh one off the base: the
  // pass reads and amends *this* review's code.
  const s = await t.session(session)
  assert.equal(s.workspace, `pr-${PR}`)
  assert.ok(fs.existsSync(s.cwd), `no worktree at ${s.cwd}`)
  assert.equal(branchOf(s.cwd), HEAD)

  /* The typed first turn, which is the whole wiring: the command string, the
     `/orchd:` namespace, and the PR number. A skill directory that does not match
     the command answers `Unknown command` on this turn and nothing before it. */
  await until('the pane to be typed its instructions', async () =>
    t.agentLog().includes(`turn: /orchd:handle-review ${PR}`))

  /* An automation record carrying this command, which is how the rail colours it
     and how `branch_busy` and the PR guards recognise it — even though what you get
     is a pane you can take over. `spawn_run` is the seam, so it is a run by
     bookkeeping and a session by feel. */
  assert.equal(s.kind.kind, 'automation')
  assert.equal(s.kind.command, 'handle-review')

  // Pressing it again lands on the session already doing it rather than cutting a
  // second agent into the same tree.
  const again = await t.api('POST', `/api/pr/${PR}/handle-review`)
  assert.equal(again.session, session, 'a second press spawned a second agent')
}
