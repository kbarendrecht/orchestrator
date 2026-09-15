// Saving the settings pane, which writes the config file the daemon is running on.
//
// The failure this guards against has happened, and it is not subtle: a write that
// replaces the file rather than merging into it costs every key the pane does not
// know about — the checkout, the port, the layout — and the app then reads the
// result as a first run and offers a folder picker for a project configured months
// ago. So the assertion that matters is about the keys nobody touched.
//
// The other half is that a save changes nothing about the daemon in front of you.
// It takes effect on the next start, and the snapshot has to keep saying what is
// actually true until then.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'

export const name = 'saving settings keeps the keys it does not know'

const config = (t) => JSON.parse(fs.readFileSync(path.join(t.cfg, 'config.json'), 'utf8'))

export async function run(t) {
  const before = config(t)
  assert.equal((await t.state()).several_in_main, false)

  const saved = await t.api('POST', '/api/config', {
    default_language: 'English',
    upstream_ref: before.upstream_ref,
    upstream_remote: before.upstream_remote,
    reviews_command: ['true'],
    main_processes: [],
    worktree_setup: [],
    worktree_retention_days: 21,
    allow_several_in_main: true,
  })
  assert.deepEqual(saved, { ok: true, restart_required: true })

  const after = config(t)
  assert.equal(after.worktree_retention_days, 21)
  assert.equal(after.allow_several_in_main, true)
  assert.equal(after.default_language, 'English')

  // The keys the pane has no control for. Each one is a way to lose a working
  // install to a save nobody thought was destructive.
  for (const key of ['main_checkout', 'port', 'worktrees_subdir', 'env_source', 'auto_resume']) {
    assert.deepEqual(after[key], before[key], `the save dropped ${key}`)
  }

  // Written, not applied: the daemon is still the one that started, and saying
  // otherwise in the snapshot would be the pane arguing with the rail.
  assert.equal((await t.state()).several_in_main, false, 'a save must not change the daemon')

  // And it is the file the next start reads.
  await t.restart()
  assert.equal((await t.state()).several_in_main, true)
}
