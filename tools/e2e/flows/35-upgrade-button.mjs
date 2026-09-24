// The update button: what it runs, and what it says afterwards.
//
// **The chain nobody could see end to end.** A press resolves the install, builds
// an argv, runs it bounded, and reports either nothing (success) or a tail. Each
// piece had a unit test and the chain had none — which is how two failures
// shipped inside a week: a cask told to run `mise up`, and an installer that
// exited 0 having done nothing while the bar offered a restart that changed
// nothing.
//
// **The agent's button rather than the app's**, and not for convenience: the
// app's nudge comes from the release poller, which a debug build does not start
// at all, so a sandbox can never have one. The agent's comes from `mise` — which
// is a PATH lookup, so the sandbox can answer it — and `start_upgrade` is one
// implementation for both subjects. What is asserted here is that implementation:
// the refusals, the claim, the run and the report.
//
// The decision *per install kind* is `update.rs`'s own table test, which is where
// five of the six kinds are reachable at all.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'
import { until } from '../harness.mjs'

export const name = 'the update button runs the installer and reports it'

/** A `mise` that answers the three questions the daemon asks it, and records
 *  every call so the flow can assert what was run rather than that something was.
 *
 *  `upgrade` fails while `mise-fail` exists, which is how the failure half of the
 *  report is driven without a second sandbox. */
const SHIM = (root) => `#!/usr/bin/env node
const fs = require('node:fs')
const argv = process.argv.slice(2)
fs.appendFileSync(${JSON.stringify(path.join('ROOT', 'mise.log')).replace('ROOT', root)}, argv.join(' ') + '\\n')
const [verb] = argv
if (verb === 'which') {
  // \`tool_of_install_path\` reads the tool out of mise's own layout.
  process.stdout.write('${root}/installs/claude-code/2.1.0/claude\\n')
} else if (verb === 'outdated') {
  process.stdout.write(JSON.stringify({ 'claude-code': { current: '2.1.0', latest: '2.2.0' } }))
} else if (verb === 'upgrade') {
  if (fs.existsSync('${root}/mise-fail')) {
    process.stderr.write('mise: no such version 2.2.0\\n')
    process.exit(1)
  }
}
`

export async function run(t) {
  fs.writeFileSync(path.join(t.bin, 'mise'), SHIM(t.root), { mode: 0o755 })
  const ran = () => (fs.existsSync(path.join(t.root, 'mise.log'))
    ? fs.readFileSync(path.join(t.root, 'mise.log'), 'utf8').trim().split('\n')
    : [])

  /* The check runs off a spawn as well as off the poller — starting a session is
     the moment the agent's version matters, because the agent nags about it in the
     pane itself. That is what makes the nudge reachable here at all. */
  const { session } = await t.api('POST', '/api/worktree', { name: 'upgrade' })
  await t.settled(session)

  const nudge = await until('the agent update to be noticed', async () => (await t.state()).agent_update)
  assert.equal(nudge.tool, 'claude-code')
  assert.equal(nudge.current, '2.1.0')
  assert.equal(nudge.latest, '2.2.0')

  // --- the press ----------------------------------------------------------

  const answer = await t.api('POST', '/api/agent/upgrade')
  assert.deepEqual(
    { from: answer.from, to: answer.to },
    { from: '2.1.0', to: '2.2.0' },
    'the route answers with the pair it is moving between',
  )

  const done = await until('the upgrade to finish', async () => {
    const run = (await t.state()).upgrade_run
    return run && !run.running ? run : null
  })
  assert.equal(done.tail, '', `a successful upgrade reports nothing, got ${JSON.stringify(done.tail)}`)
  assert.equal(done.to, '2.2.0')
  assert.ok(
    ran().includes('upgrade claude-code'),
    `the installer was not run as asked, calls: ${JSON.stringify(ran())}`,
  )

  // A second press while one is running is refused, and that is the whole reason
  // the run slot is taken under the lock that checked it.
  await t.api('POST', '/api/agent/upgrade')
  await assert.rejects(
    () => t.api('POST', '/api/agent/upgrade'),
    /already running|no agent update/,
    'two upgrades of one tool must not race over the same install directory',
  )
  await until('the second upgrade to finish', async () => {
    const run = (await t.state()).upgrade_run
    return run && !run.running
  })

  // --- and what it says when the installer refuses -------------------------

  fs.writeFileSync(path.join(t.root, 'mise-fail'), '')
  await t.api('POST', '/api/agent/upgrade/dismiss')
  await t.api('POST', '/api/agent/upgrade')
  const failed = await until('the failed upgrade to report', async () => {
    const run = (await t.state()).upgrade_run
    return run && !run.running ? run : null
  })
  /* The *end* of what the installer said, which is what a person needs: `mise`
     writes the reason to stderr and its stdout is progress, so a tail that took
     stdout would carry the noise and lose the answer.

     It carries the reason and not the command — `explain` only leads with the
     command for the one failure where the reason does not say what to do, which is
     `pkexec` with no authentication agent. What was run is in the button's own
     tooltip before the press, so the tail is the half that is not knowable
     otherwise. */
  assert.match(failed.tail, /no such version 2\.2\.0/, `the failure has to carry the reason: ${failed.tail}`)
  assert.ok(ran().filter((l) => l === 'upgrade claude-code').length >= 3,
    `each press runs the installer, calls: ${JSON.stringify(ran())}`)
}
