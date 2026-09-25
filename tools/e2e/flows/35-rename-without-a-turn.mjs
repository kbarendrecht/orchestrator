// A `/rename` reaching the rail with no turn behind it.
//
// The hooks read a session's title on `SessionStart` and `Stop`, and `/rename` is
// a local command: no turn runs, so neither fires. Claude Code writes a
// `custom-title` record within a second, and the rail used to show it only after
// the next turn ended. `start_title_poller` is what reads it now, and a missing
// `start_…` call compiles perfectly, which is why this is a flow and not only the
// unit test on `store::ai_title`.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { until } from '../harness.mjs'

export const name = 'a rename reaches the rail without a turn'

/** Claude Code keys its transcript dir by cwd, slugging every `/` *and* `.`:
 *  `config::transcript_slug`, and the fake agent spells it the same way. */
const transcriptDir = (home, cwd) =>
  path.join(home, '.claude/projects', cwd.replaceAll(/[/.]/g, '-'))

export async function run(t) {
  const { session } = await t.api('POST', '/api/worktree', { name: 'atlas' })
  await t.settled(session)
  await t.transcribed(session)
  assert.notEqual((await t.session(session)).title, 'the name I typed')

  // Idle at its prompt, so nothing below can arrive over a hook.
  const file = path.join(transcriptDir(t.home, t.worktreePath('atlas')), `${session}.jsonl`)
  fs.appendFileSync(
    file,
    `${JSON.stringify({ type: 'custom-title', customTitle: 'the name I typed', sessionId: session })}\n`,
  )

  await until('the rename to reach the snapshot', async () =>
    (await t.session(session))?.title === 'the name I typed')
}
