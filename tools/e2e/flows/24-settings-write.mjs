// Saving the settings pane, which writes the config file the daemon is running on.
//
// The failure this guards against has happened, and it is not subtle: a write that
// replaces the file rather than merging into it costs every key the pane does not
// know about — the checkout, the port, the layout — and the app then reads the
// result as a first run and offers a folder picker for a project configured months
// ago. So the assertion that matters is about the keys nobody touched.
//
// The other half is what a save does to the daemon in front of you. Everything but
// three fields applies at once, and the answer asks for a restart only when one of
// those three moved: the panel restarts on that answer and on nothing else, so a
// wrong `restart_required` either restarts for nothing or leaves a change unapplied.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'

export const name = 'saving settings keeps the keys it does not know'

const config = (t) => JSON.parse(fs.readFileSync(path.join(t.cfg, 'config.json'), 'utf8'))

export async function run(t) {
  const before = config(t)
  assert.equal((await t.state()).several_in_main, false)

  const settings = {
    default_language: 'English',
    upstream_ref: before.upstream_ref,
    upstream_remote: before.upstream_remote,
    reviews_command: ['true'],
    main_processes: [],
    worktree_init: ['git', 'fetch', '--prune'],
    worktree_setup: [],
    /* A note is prose the project wrote, so it is one string rather than an argv —
       and the half nobody set has to stay `null`. An empty string is a note, and
       `workspace_notes.for_main` would hand it to an arriving agent as one. */
    workspace_notes: { main: 'The dev stack runs here.', worktree: null },
    worktree_retention_days: 21,
    allow_several_in_main: true,
  }
  const saved = await t.api('POST', '/api/config', settings)
  assert.deepEqual(saved, { ok: true, restart_required: false },
    'none of these is fixed at start, so none of them needs a restart')

  const after = config(t)
  assert.equal(after.worktree_retention_days, 21)
  assert.equal(after.allow_several_in_main, true)
  assert.equal(after.default_language, 'English')
  assert.deepEqual(after.worktree_init, ['git', 'fetch', '--prune'])
  assert.deepEqual(after.workspace_notes, { main: 'The dev stack runs here.', worktree: null })

  // The keys the pane has no control for. Each one is a way to lose a working
  // install to a save nobody thought was destructive.
  for (const key of ['main_checkout', 'port', 'worktrees_subdir', 'env_source', 'auto_resume']) {
    assert.deepEqual(after[key], before[key], `the save dropped ${key}`)
  }

  // Applied, not only written: the snapshot says so now, and the pane reads back
  // what was saved rather than what the daemon started with.
  assert.equal((await t.state()).several_in_main, true, 'a save applies at once')
  assert.equal((await t.api('GET', '/api/config')).worktree_retention_days, 21)

  /* The upstream ref is one of the three, because the push guard's hook is built
     on it at start. Asked against what the daemon is *running*, so putting it back
     stops asking — a restart for a value that is already live would be a restart
     for nothing. */
  const moved = await t.api('POST', '/api/config', { ...settings, upstream_ref: 'upstream/elsewhere' })
  assert.equal(moved.restart_required, true, 'a new upstream ref needs a restart')
  const back = await t.api('POST', '/api/config', settings)
  assert.equal(back.restart_required, false, 'and putting it back does not')

  // And it is the file the next start reads.
  await t.restart()
  assert.equal((await t.state()).several_in_main, true)
}
