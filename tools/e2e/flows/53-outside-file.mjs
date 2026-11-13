// A file no workspace holds opens read-only, and a `~/` path is the daemon's to expand.
//
// An agent leaves a plan in `~/development/claude-plans/` and prints the path, and
// the pane refused it as outside the workspace. `preview::outside_file` is the
// bound: absolute or `~/`, a regular file. An HTML page gets a preview grant for
// that one file, so nothing beside it is served.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'

export const name = 'a file outside every workspace is shown read-only, and only that file'

export async function run(t) {
  const plans = path.join(t.home, 'plans')
  fs.mkdirSync(plans, { recursive: true })
  fs.writeFileSync(path.join(plans, 'plan.md'), '# plan\n')
  fs.writeFileSync(path.join(plans, 'plan.html'), '<p>plan</p>\n')
  fs.writeFileSync(path.join(plans, 'secret.txt'), 'not for the frame\n')
  try {
    const text = (p, method = 'GET') => fetch(`http://127.0.0.1:${t.port}/api/outside/file?`
      + new URLSearchParams({ path: p }), { method })
    const home = await text('~/plans/plan.md')
    assert.equal(home.status, 200, 'a ~/ path is expanded by the daemon')
    assert.equal((await home.json()).content, '# plan\n')
    assert.equal((await text(path.join(plans, 'plan.md'))).status, 200, 'and an absolute one is read')
    assert.equal((await text('~/plans/plan.md', 'HEAD')).status, 200, 'HEAD answers for the underline')
    assert.equal((await text('~/plans/gone.md')).status, 404, 'a missing file is not')
    assert.equal((await text('~/plans')).status, 404, 'a folder is not a file')
    assert.equal((await text('plans/plan.md')).status, 404, 'a relative path is relative to nothing')

    // The preview grant names one page, and the frame cannot read its neighbours.
    const { token } = await t.api('POST', '/api/preview', { path: '~/plans/plan.html', outside: true })
    const page = await fetch(`http://127.0.0.1:${t.port}/preview/${token}/plan.html`)
    assert.equal(page.status, 200, 'the page itself is served to the frame')
    assert.equal(await page.text(), '<p>plan</p>\n')
    assert.equal(page.headers.get('content-security-policy'), 'sandbox allow-scripts')
    const beside = await fetch(`http://127.0.0.1:${t.port}/preview/${token}/secret.txt`)
    assert.equal(beside.status, 404, 'a file beside it is not')
  } finally {
    fs.rmSync(plans, { recursive: true, force: true })
  }
}
