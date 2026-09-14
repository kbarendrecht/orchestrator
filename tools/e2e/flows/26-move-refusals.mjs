// Every way the daemon refuses to move main's checkout.
//
// **Moving and swapping is the part of this app that breaks most**, and until now
// only its happy paths were covered: flows 06, 07, 12, 17 and 18 all assert what
// happens when a move *works*. The refusals had no assertion anywhere — nine of
// them across `swap_with_main` and `move_out_of_main`, each written after something
// went wrong, none of them held by a test.
//
// They are cheap and they share a sandbox, so they are one flow rather than nine.
// `11-teardown-refusals` is the same shape for the same reason.
//
// The two that matter most, because both have an incident behind them in the
// source:
//
//   * **mid-turn.** "The swap replaces every file under it" — an agent working in
//     either tree would have its worktree changed underneath it. The same guard
//     exists three times (swap, move out, and `park_main`), so one regression
//     uncovers all three.
//   * **the `swapping` lock.** Its comment records a double click that "left a
//     session in main with its branch back in the worktree". Refused rather than
//     queued, deliberately: a queued second swap runs on what you saw before the
//     first one landed.
//
// Not covered here, and deliberately: `unknown workspace` for the swap route is the
// same `workspace_path` lookup flow 13 already pins on another route, and asserting
// it again buys a second copy of one `ok`.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { branchOf, git, gitMayFail, until } from '../harness.mjs'

export const name = 'moving main refuses what it must'

/** The message a refused call carried, whatever shape the route wrapped it in. */
const refusal = async (call) => {
  try {
    await call()
  } catch (e) {
    return String(e.message)
  }
  return null
}

export async function run(t) {
  const { session: inTree } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(inTree)
  const dir = t.worktreePath('invoice')
  const base = branchOf(t.repo)

  // --- the arguments that make no sense ---------------------------------------

  assert.match(
    await refusal(() => t.api('POST', '/api/workspace/main/swap-main')) ?? '',
    /swapped with itself/,
    'main swapping with main would exchange a checkout with itself',
  )

  assert.match(
    await refusal(() => t.api('POST', `/api/session/${inTree}/out-of-main`)) ?? '',
    /is not in main/,
    'a session already in a worktree has nothing to be moved out of',
  )

  const nobody = '00000000-0000-4000-8000-000000000000'
  assert.ok(
    await refusal(() => t.api('POST', `/api/session/${nobody}/out-of-main`)),
    'a session that does not exist cannot be moved',
  )

  // --- a tree that cannot switch at all ---------------------------------------
  //
  // A stopped rebase is the one git state a swap refuses outright: the tree cannot
  // check anything else out until it is finished or aborted. Uncommitted work is
  // *not* refused — it is carried — which is the distinction this pins.
  fs.writeFileSync(path.join(t.repo, 'README.md'), '# main\n')
  git(t.repo, ['commit', '-qam', 'main moves on'])
  git(dir, ['switch', '-qc', 'conflicting'])
  fs.writeFileSync(path.join(dir, 'README.md'), '# the worktree\n')
  git(dir, ['commit', '-qam', 'the worktree moves on'])
  /* Left stopped on purpose: a conflicting rebase exits non-zero and stays put.
     Confirmed the way `git::rebase_in_progress` confirms it — ask git for the git
     dir and look for its own marker — rather than by guessing a path. The first
     version of this guessed `.git/worktrees/<name>/rebase-merge`, which is right
     for the merge backend and for a worktree whose git name matches its workspace
     name, and it failed in the full suite while passing eight times alone. A
     fixture check that is a *different* rule from the daemon's is a second thing to
     be wrong. */
  const rebase = gitMayFail(dir, ['rebase', base])
  assert.notEqual(rebase.status, 0,
    `the fixture needed a rebase that conflicts: ${rebase.stdout}${rebase.stderr}`)
  const gitDir = git(dir, ['rev-parse', '--path-format=absolute', '--git-dir'])
  assert.ok(
    fs.existsSync(path.join(gitDir, 'rebase-merge'))
      || fs.existsSync(path.join(gitDir, 'rebase-apply')),
    `the rebase did not stop where the daemon looks: ${gitDir}`,
  )

  assert.match(
    await refusal(() => t.api('POST', '/api/workspace/invoice/swap-main')) ?? '',
    /rebase stopped part-way/,
    'a tree mid-rebase cannot take another branch',
  )
  git(dir, ['rebase', '--abort'])

  // --- an agent that is working ------------------------------------------------
  //
  // The refusal with the worst failure mode: without it the swap replaces every
  // file under a running agent. `t.hold` keeps the turn open, because a real one
  // here is 60ms and there is no window to catch otherwise.
  t.hold(true)
  try {
    await t.api('POST', `/api/session/${inTree}/tell`, { text: 'think about this' })
    await until('the agent to be working', async () =>
      (await t.session(inTree))?.state.state === 'working')

    assert.match(
      await refusal(() => t.api('POST', '/api/workspace/invoice/swap-main')) ?? '',
      /mid-turn/,
      'a swap must not pull the files out from under a working agent',
    )

    // And the same guard on the other route, which is its own copy of the check.
    const { session: inMain } = await t.api('POST', '/api/session', { workspace: 'main' })
    await until('the session in main to be working', async () =>
      (await t.session(inMain))?.state.state === 'working')
    assert.match(
      await refusal(() => t.api('POST', `/api/session/${inMain}/out-of-main`)) ?? '',
      /mid-turn/,
      'moving out of main must not pull the files out either',
    )
  } finally {
    // In a `finally`, or a failed assertion above leaves every later flow's agent
    // holding its first turn — the sandbox is per flow, but the mistake is easy.
    t.hold(false)
  }

  // Nothing moved through any of that, which is the other half of a refusal.
  assert.equal(branchOf(t.repo), base, 'a refusal must not move main')
  assert.equal(branchOf(dir), 'conflicting', 'nor the worktree')
}
