// A restart, which is the only thing that puts the durable half under test.
//
// Every session record round-trips through `sessions.json` here — `restore`,
// `prune_ghosts`, `auto_resume`, `first_per_workspace` — and a fault in any of
// them is invisible until the next launch, when the rail comes back wrong or
// carries a row nothing can reach. Nothing else in the suite restarts anything.
//
// It is also the only honest test of `Session::resumable()`, which is computed
// during `restore` from fields that only exist on the record.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import { until } from '../harness.mjs'

export const name = 'restart, restore and auto-resume'
export const options = { autoResume: true }

export async function run(t) {
  // A real conversation in a worktree, and another in main.
  const { session: tree } = await t.api('POST', '/api/worktree', { name: 'invoice' })
  await t.settled(tree)
  const { session: main } = await t.api('POST', '/api/session', { workspace: 'main' })
  await t.settled(main)

  // A name you typed, which auto-resume rebuilds the record without: the resume
  // path reset every field `Session::new` defaults, and the title self-heals from
  // the transcript while the name had nothing to restore it.
  await t.api('POST', `/api/session/${tree}/rename`, { name: 'the invoice one' })
  assert.equal((await t.session(tree)).name, 'the invoice one')

  // And a pane nobody typed into, in a worktree of its own. It is the case the
  // whole `had_a_turn` distinction exists for: a headers-only transcript that
  // `--resume` opens and exits.
  t.setTurns(0)
  const { session: empty } = await t.api('POST', '/api/worktree', { name: 'ghost' })
  await until('the turnless pane to exist', async () => await t.session(empty))
  assert.equal((await t.session(empty)).has_transcript, false)
  t.setTurns(1)

  await t.restart()

  // Both real conversations came back, under their own ids and in their own
  // workspaces. The id surviving is what makes a resume a continuation rather
  // than a new row beside the old one.
  for (const [id, where] of [[tree, 'invoice'], [main, 'main']]) {
    const s = await until(`${where} to be restored`, async () => await t.session(id))
    assert.equal(s.workspace, where)
    assert.equal(s.has_transcript, true, `${where} lost its conversation across the restart`)
  }

  // The typed name came back with the record. Auto-resume respawns through the
  // resume path, so this is the field that used to revert to the workspace default.
  assert.equal(
    (await t.session(tree)).name,
    'the invoice one',
    'the rename did not survive the restart',
  )

  // `auto_resume` relaunched them, so they are live again rather than archived
  // rows waiting to be clicked.
  await t.settled(tree)
  await t.settled(main)
  assert.equal((await t.session(main)).alive, true)

  // Main's claim is re-taken by the restored session, not left empty. A live
  // agent in main with no occupant recorded is what `switch_main_to_pr` reads
  // before deciding it may move the checkout.
  await until('main to record its restored occupant', async () =>
    (await t.workspace('main')).occupant === main)

  // The turnless pane is gone entirely — not archived, not a row you cannot
  // reach. `prune_ghosts` drops it and removes the header file behind it.
  assert.equal(await t.session(empty), undefined, 'a turnless session survived the restart')
  const ghostFiles = fs.existsSync(`${t.home}/.claude/projects`)
    ? fs.readdirSync(`${t.home}/.claude/projects`).flatMap((d) =>
      fs.readdirSync(`${t.home}/.claude/projects/${d}`))
    : []
  assert.ok(
    !ghostFiles.includes(`${empty}.jsonl`),
    'the turnless transcript was left on disk for the next sweep to find',
  )

  // One live session per workspace still holds after a cold start: `auto_resume`
  // applies the same rule the API does, so a `sessions.json` with two records for
  // one worktree cannot bring both back.
  const perWorkspace = {}
  for (const s of (await t.state()).sessions.filter((s) => s.alive)) {
    perWorkspace[s.workspace] = (perWorkspace[s.workspace] ?? 0) + 1
  }
  for (const [ws, n] of Object.entries(perWorkspace)) {
    assert.equal(n, 1, `${ws} came back with ${n} live sessions`)
  }
}
