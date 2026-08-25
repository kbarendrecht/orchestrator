// Firing `fix-pr` at a PR, and being refused by the guard table.
//
// This is the one flow with a forge in it. GitHub is substituted the same way the
// agent is — a shim earlier on PATH, because `curl` is the daemon's only route out
// (`forge::github::graphql`) — so the argv, the header-on-stdin dance, the JSON
// parser and every guard are the real ones, and only the network is canned.
//
// What it is here to catch is the part no unit test covers: the guards are
// evaluated against `inner.viewer` and `inner.prs`, which only exist after a poll
// has actually landed, and a `Go` verdict then has to turn into a real worktree on
// the PR's real branch with a real agent in it. `evaluate()` being right about a
// hand-built `GuardInput` says nothing about either half.
//
// `POST /api/pr/:n/fix-pr` starts a run immediately — no confirmation — so the
// refusals are the whole safety story, and two of them are asserted for real.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'fix a PR'

const VIEWER = 'e2e-viewer'
const PR = 101
const HEAD = 'feature/fix-me'

export const options = {
  // Without `repo` the daemon derives owner/name from the origin remote — a local
  // path here — and turns polling off, so nothing downstream would ever run. The
  // token only has to be non-empty: it short-circuits the ladder before `gh`, and
  // the shim never looks at it.
  repo: 'acme/monorepo',
  githubToken: 'e2e-token',
  pollSeconds: 30,
}

export async function run(t) {
  // A real branch, because the worktree the run needs is cut with `git worktree
  // add <path> <branch>`. A PR naming a branch that exists only in the canned
  // answer would fail at git, several steps after the interesting part.
  git(t.repo, ['branch', HEAD])

  // --- the authorship guard, for real ---------------------------------------
  //
  // A run rebases and force-pushes, so a head repo that is not yours is the one
  // refusal that protects somebody else's branch. It is asserted first because it
  // must refuse *before* anything is created.
  t.setPrs([{ number: PR, head_ref: HEAD, head_owner: 'someone-else' }], VIEWER)
  const seen = await t.pollPrs()
  const pr = seen.prs.find((p) => p.number === PR)
  assert.ok(pr, `the poll saw ${JSON.stringify(seen.prs)} — a mis-shaped canned node reads as no PRs`)
  assert.equal(pr.head_ref, HEAD)
  assert.equal(seen.pr_error, null)

  await assert.rejects(
    () => t.api('POST', `/api/pr/${PR}/fix-pr`),
    /not your fork/,
  )
  // A refused run leaves nothing behind. The worktree used to be cut before the
  // guard ran, which is exactly the shape this pins down.
  assert.ok(!fs.existsSync(t.worktreePath(`pr-${PR}`)), 'a refusal must not cut a worktree')

  // --- the run ---------------------------------------------------------------

  t.setPrs([{ number: PR, head_ref: HEAD }], VIEWER)
  // A poll per attempt, not one poll and then waiting: a fetch already in flight
  // when the file changed answers from the old copy, and with a 30s period the
  // next one of its own accord is far past any sane timeout.
  await until('the poll to report the PR as yours', async () =>
    (await t.pollPrs()).prs.find((p) => p.number === PR)?.head_owner === VIEWER)

  const { session } = await t.api('POST', `/api/pr/${PR}/fix-pr`)

  // The worktree is pinned to the PR's head branch, not cut fresh from the base:
  // §8 asks for a worktree "pinned to that PR's head branch", and `--worktree`
  // would have given a new branch off upstream instead.
  const dir = t.worktreePath(`pr-${PR}`)
  assert.ok(fs.existsSync(dir), `no worktree at ${dir}`)
  assert.equal(branchOf(dir), HEAD)

  const ws = await t.workspace(`pr-${PR}`)
  assert.ok(ws, 'the worktree must be registered, or nothing can find the run')
  assert.equal(ws.path, dir)
  assert.ok(ws.branches.includes(HEAD), `pr-${PR} claims ${ws.branches}`)

  // Automation, not interactive: the kind is what the rail reads to keep the run
  // out of the ordinary session list, and what `watch_session_exit` reads to send
  // the verdict to `fix_pr::settle`.
  const s = await t.settled(session)
  assert.equal(s.workspace, `pr-${PR}`)
  assert.deepEqual(s.kind, { kind: 'automation', pr: PR, command: 'fix-pr' })

  // Recorded, and recorded against this session. The write is what survives a
  // restart, and it is the whole of the one-run-per-PR cap.
  const auto = (await t.state()).automation[String(PR)]
  assert.equal(auto?.state, 'running')
  assert.equal(auto.session, session)

  // --- the second-run guard, for real ---------------------------------------
  //
  // Same button, twice. Nothing in the API asks whether you meant it, so this is
  // the refusal that stops two agents rebasing one branch.
  await assert.rejects(
    () => t.api('POST', `/api/pr/${PR}/fix-pr`),
    /already has a fix-pr session running/,
  )
}
