// A file reached through a link out of the checkout is read, and has no blame.
//
// A checkout whose `.plan` links to a plans folder outside the repo opens those
// files read-only (#48). The viewer then asks who last changed the lines it drew,
// and blame still refused the link, so every plan opened with an error on screen.
// It is in no history of this repo, so the answer is nothing, not a refusal.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'

export const name = 'a plan behind a link out of the checkout opens without a blame error'

export async function run(t) {
  const plans = path.join(t.home, 'linked-plans')
  const link = path.join(t.repo, '.linked-plan')
  fs.mkdirSync(plans, { recursive: true })
  fs.writeFileSync(path.join(plans, 'plan.md'), '# plan\n\nline\n')
  fs.symlinkSync(plans, link)
  try {
    const q = (p) => new URLSearchParams({ workspace: 'main', path: p })
    const file = await t.api('GET', `/api/file?${q('.linked-plan/plan.md')}`)
    assert.equal(file.content, '# plan\n\nline\n', 'the plan is read through the link')
    assert.equal(file.read_only, true, 'and only read')
    const blame = await t.api('GET', `/api/blame?${q('.linked-plan/plan.md')}&from=1&to=3`)
    assert.deepEqual(blame, [], 'blame answers nothing rather than refusing')
    await assert.rejects(t.api('GET', `/api/blame?${q('../outside.md')}&from=1&to=3`),
      /outside|escape|\.\./, 'a path that climbs out is still refused')
  } finally {
    fs.rmSync(link, { force: true })
    fs.rmSync(plans, { recursive: true, force: true })
  }
}
