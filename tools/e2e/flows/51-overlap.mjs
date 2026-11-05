// Two workspaces that changed the same file are told so, on the board and to the
// agent.
//
// The unit tests in `overlap.rs` hold the comparison on two real trees. What only
// a real daemon can say is that it is *wired*: that a reconcile recomputes the
// pairs, that the snapshot carries them on both sides, and that the prompt hook
// answers with the note rather than being answered before it ran — which is what
// `hooks::detach` did to every observer route, and why the note had to move off it.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'

export const name = 'two workspaces that change one file are told so'

export async function run(t) {
  const made = []
  for (const name of ['left', 'right']) {
    const { session } = await t.api('POST', '/api/worktree', { name })
    await t.settled(session)
    made.push(await t.session(session))
  }
  const [left, right] = made

  // The same committed line in both, a new file at one path in both, and a file
  // only one of them touched.
  for (const s of made) {
    fs.writeFileSync(path.join(s.cwd, 'README.md'), `changed in ${s.workspace}\n`)
    fs.writeFileSync(path.join(s.cwd, 'both-new.txt'), `${s.workspace}\n`)
  }
  fs.writeFileSync(path.join(left.cwd, 'only-left.txt'), 'mine\n')

  for (const s of made) await t.api('POST', `/api/workspace/${s.workspace}/reconcile`)

  const overlaps = async (ws) =>
    (await t.state()).workspaces.find((w) => w.id === ws)?.overlaps ?? []
  const seen = await overlaps(left.workspace)
  assert.deepEqual(
    seen,
    [
      { path: 'README.md', with: right.workspace, lines: true },
      { path: 'both-new.txt', with: right.workspace, lines: true },
    ],
    `the left tree is told about both shared files and not its own: ${JSON.stringify(seen)}`,
  )
  assert.deepEqual(
    (await overlaps(right.workspace)).map((o) => [o.path, o.with]),
    [['README.md', left.workspace], ['both-new.txt', left.workspace]],
    'and the right tree about the same two, from its side',
  )
  assert.deepEqual(await overlaps('main'), [], 'main changed nothing and is told nothing')

  // The agent hears it with its next prompt, once.
  const prompt = () => t.api('POST', '/hooks/user-prompt-submit', {
    session_id: left.id, hook_event_name: 'UserPromptSubmit', prompt: 'go on',
  })
  const note = (await prompt())?.hookSpecificOutput?.additionalContext ?? ''
  assert.ok(
    note.includes('README.md') && note.includes(`\`${right.workspace}\``) && note.includes('same or adjacent lines'),
    `the prompt hook answers with the note: ${JSON.stringify(note)}`,
  )
  assert.equal((await prompt())?.hookSpecificOutput, undefined, 'and says it once')
}
