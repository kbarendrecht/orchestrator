// The file pane's HTML preview, as the route a sandboxed frame reads.
//
// The frame runs the page's own scripts, so what this route serves is what those
// scripts can send anywhere. `preview.rs` holds the rules as a unit test; this is
// the half only a real daemon shows: the route is registered, the guard lets an
// opaque origin through on `/preview/` and nowhere else, and the headers that make
// a preview URL sandboxed on its own are really on the wire.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'

export const name = 'an html preview and an image serve their file and nothing they must not'

export async function run(t) {
  const site = path.join(t.repo, 'site')
  fs.mkdirSync(site, { recursive: true })
  fs.writeFileSync(path.join(site, 'index.html'),
    '<link rel="stylesheet" href="app.css"><script type="module" src="app.js"></script><p>hi</p>')
  fs.writeFileSync(path.join(site, 'app.css'), 'p { color: red }')
  fs.writeFileSync(path.join(site, 'app.js'), 'document.title = "ran"')
  fs.writeFileSync(path.join(t.repo, '.env'), 'SECRET=1')
  fs.writeFileSync(path.join(t.repo, 'secrets.json'), '{"key":1}')
  fs.appendFileSync(path.join(t.repo, '.gitignore'), '\nsecrets.json\n')

  const base = `http://127.0.0.1:${t.port}`
  const { token } = await t.api('POST', '/api/preview', { workspace: 'main', path: 'site/index.html' })
  const again = await t.api('POST', '/api/preview', { workspace: 'main', path: 'site/index.html' })
  assert.equal(again.token, token, 'one token per page')

  /* As the frame asks: no app token, and the opaque origin a sandbox sends. A
     module script is fetched in CORS mode, so this is the request that would
     break every modern page if the guard refused it. */
  const asFrame = (rel) => fetch(`${base}/preview/${token}/${rel}`, { headers: { origin: 'null' } })

  const page = await asFrame('site/index.html')
  assert.equal(page.status, 200)
  assert.match(page.headers.get('content-type') ?? '', /^text\/html/)
  assert.equal(page.headers.get('content-security-policy'), 'sandbox allow-scripts',
    'a preview URL opened in a tab of its own is sandboxed too')
  assert.equal(page.headers.get('access-control-allow-origin'), '*')

  const js = await asFrame('site/app.js')
  assert.equal(js.status, 200)
  assert.match(js.headers.get('content-type') ?? '', /^text\/javascript/)
  assert.equal((await asFrame('site/app.css')).status, 200)

  // What the page's scripts must not be able to read.
  for (const rel of ['.env', 'site/../.env', '.git/config', 'secrets.json']) {
    assert.equal((await asFrame(rel)).status, 404, `${rel} is refused`)
  }
  assert.equal((await fetch(`${base}/preview/not-a-token/site/index.html`)).status, 404)

  // And the exemption is the preview route's alone: the same origin on the API
  // is refused, which is what keeps the frame's `fetch` away from everything else.
  const api = await fetch(`${base}/api/state`, { headers: { origin: 'null' } })
  assert.equal(api.status, 403, 'an opaque origin still cannot read the API')

  /* The file pane's images, on a route of their own. The page is trusted, so the
     preview's file rules are not these; what this must never do is serve a
     workspace's HTML from the daemon's origin, where it would run as the app. */
  fs.writeFileSync(path.join(t.repo, 'shot.png'), Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==', 'base64'))
  const image = (rel) => fetch(`${base}/api/file/image?workspace=main&path=${encodeURIComponent(rel)}`)
  const png = await image('shot.png')
  assert.equal(png.status, 200)
  assert.equal(png.headers.get('content-type'), 'image/png')
  assert.match(png.headers.get('content-security-policy') ?? '', /sandbox/)
  assert.equal((await image('site/index.html')).status, 404, 'an image route serves no html')
  assert.equal((await image('../outside.png')).status, 404, 'nor anything outside the workspace')

  // Only a page mints a token.
  await assert.rejects(
    () => t.api('POST', '/api/preview', { workspace: 'main', path: 'secrets.json' }),
    /only an .html file/,
  )
}
