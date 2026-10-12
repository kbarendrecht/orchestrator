// An image an agent saved in its own Claude Code scratchpad opens in the pane.
//
// The scratchpad is outside every workspace, so the workspace bound refused the
// path and it was never even underlined. `preview::scratchpad_image` serves that
// one folder for the session it belongs to, resolved first and checked after;
// this holds the route, and `preview::in_scratchpad`'s own test holds the shape.
// A text file there is served by `preview::scratchpad_file`, also by the
// `scratchpad/<file>` an agent writes in prose.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

export const name = 'an image in a session scratchpad is served, and nothing beside it'

// The smallest valid PNG: one transparent pixel.
const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=',
  'base64',
)

export async function run(t) {
  const { session: a } = await t.api('POST', '/api/worktree', { name: 'shots' })
  const { session: b } = await t.api('POST', '/api/worktree', { name: 'other' })
  await t.settled(a)
  await t.settled(b)
  const s = await t.session(a)
  const slug = s.cwd.replace(/[/.]/g, '-')
  const dir = path.join('/tmp', `claude-${process.getuid()}`, slug, a, 'scratchpad')
  fs.mkdirSync(dir, { recursive: true })
  const shot = path.join(dir, 'kanban.png')
  fs.writeFileSync(shot, PNG)
  const outside = path.join(os.tmpdir(), `orchd-e2e-not-a-scratchpad-${process.pid}.png`)
  fs.writeFileSync(outside, PNG)
  try {
    const get = (session, p) => fetch(`http://127.0.0.1:${t.port}/api/scratchpad/image?`
      + new URLSearchParams({ session, path: p }))
    const ok = await get(a, shot)
    assert.equal(ok.status, 200, 'its own scratchpad image is served')
    assert.equal(ok.headers.get('content-type'), 'image/png')
    assert.deepEqual(Buffer.from(await ok.arrayBuffer()), PNG)
    // The underline asks with HEAD before it draws a link, so HEAD has to answer.
    const head = await fetch(`http://127.0.0.1:${t.port}/api/scratchpad/image?`
      + new URLSearchParams({ session: a, path: shot }), { method: 'HEAD' })
    assert.equal(head.status, 200, 'HEAD answers for the underline')
    assert.equal((await get(b, shot)).status, 404, "another session's scratchpad is not")
    assert.equal((await get(a, outside)).status, 404, 'a file outside the scratchpad is not')

    fs.writeFileSync(path.join(dir, 'handoff.md'), '# handoff\n')
    const text = (session, p) => fetch(`http://127.0.0.1:${t.port}/api/scratchpad/file?`
      + new URLSearchParams({ session, path: p }))
    const long = await text(a, path.join(dir, 'handoff.md'))
    assert.equal(long.status, 200, 'a text file in its own scratchpad is served')
    assert.equal((await long.json()).content, '# handoff\n')
    const short = await text(a, 'scratchpad/handoff.md')
    assert.equal(short.status, 200, 'the scratchpad/<file> shorthand resolves to its own scratchpad')
    assert.equal((await short.json()).content, '# handoff\n')
    assert.equal((await text(b, 'scratchpad/handoff.md')).status, 404, "not in another session's")
    assert.equal((await text(a, 'scratchpad/../../../../../../etc/hostname')).status, 404, 'the shorthand cannot climb out')
    assert.equal((await text(a, shot)).status, 404, 'an image is not text')
  } finally {
    // The slug folder too: it is this flow's, and an empty one was left per run.
    fs.rmSync(path.join('/tmp', `claude-${process.getuid()}`, slug), { recursive: true, force: true })
    fs.rmSync(outside, { force: true })
  }
}
