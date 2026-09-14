#!/usr/bin/env node
// What the rendered page must never show, asserted in a real browser.
//
//   mise run page-check
//   mise run page-check -- --mac    the macOS chrome, simulated
//   mise run page-check -- --keep   keep the sandbox to read
//
// **Deliberately not screenshots.** A pixel baseline would police the one thing
// this repo changes most — `renderer-check`'s neighbour commits are nearly all
// layout and copy — so every intentional change would arrive as a red build and
// a blind `--update-snapshots`, which is the same habit as `--no-verify`. And it
// would police *Chrome*, while the app ships WebKitGTK and WKWebView.
//
// So this asserts the faults that are text rather than pixels, and every one of
// them has happened here:
//
//   * an unresolved `MOD` placeholder. The legend's chords are substituted at
//     boot and its *descriptions* were not, which shipped once.
//   * `undefined` or `NaN` in something a person reads. The review header spent
//     months reading its fallback because the daemon never sent `title` — a
//     missing field shows up exactly this way.
//   * two sets of window buttons. The board and the first-run page used to draw
//     their own chrome separately, and macOS grew a native titlebar beside ours
//     (#11). One page draws it now, and this is what keeps that true.
//   * a link the page would navigate to that is not `http`. `safeHref` is the one
//     rule and every `href =` goes through it; asserted by calling it, because a
//     rendered page has no such link in it to look at — which is the point.
//   * anything thrown during boot, which `pageerror` catches for free.
//   * a `stack down` badge on a repo that has no stack. `docs/workspace-isolation.md`
//     records that orchd carries no container config at all and calls that the
//     portable default; the drawer contradicted it on every checkout with no
//     compose file.
//   * a boot preflight finding that never leaves the log. `machine::check` knows
//     at startup that `gh` is missing or that `reviews_command` is not there, and
//     the window used to show only the symptom — `unavailable`, `off` — with the
//     cause in a file a launcher-started app has no terminal for.
//
// **And one gesture, which is a second contract in the same file.** The rail's
// session drag is behaviour rather than text, so it does not belong under the
// heading above — it is here because the alternative is a second script booting a
// second daemon and a second Chrome for twenty lines of assertion, and a gate
// nobody runs is the thing this repo spends its checks avoiding.
//
// **What it holds, measured by breaking it.** Dropping `sessionOrder` from the
// rail's paint signature fails both drag lines; dropping the `localStorage` write
// fails the reload line. What it does **not** hold is the mid-drag render guard
// (`rowDrag` in `rail.js`): removing that still passes, because a synthetic drag
// is over in a few milliseconds and the snapshot that would rebuild the rail
// under the pointer never lands inside it. That failure is a hand on a mouse, and
// nothing here can see it.

import { chromium } from 'playwright-core'
import { sandbox } from './harness.mjs'

const asMac = process.argv.includes('--mac')
const keep = process.argv.includes('--keep')

let failed = false
const check = (ok, what) => {
  console.log(`  ${ok ? 'ok  ' : 'FAIL'}  ${what}`)
  if (!ok) failed = true
}

/* A `reviews_command` that is not there, so the boot preflight has something to
   find. Everything else here runs on a healthy sandbox; this one condition is
   deliberately broken, because the bar it raises is the assertion below. */
const t = await sandbox({ turns: 1, reviewsCommand: ['/nonexistent-orchd-probe'] })
let browser
try {
  const { session } = await t.api('POST', '/api/worktree', { name: 'page' })
  await t.settled(session)

  browser = await chromium.launch({ channel: 'chrome', args: ['--no-sandbox'] })
  const page = await browser.newPage({ viewport: { width: 1400, height: 900 } })
  page.on('pageerror', (e) => {
    console.error('  page error:', e.message)
    failed = true
  })

  if (asMac) {
    // The same amend `renderer.mjs` uses, and for the same reason: `platform`
    // and `chrome` are values the daemon substitutes into the page, so setting
    // them exercises the real branch rather than a stand-in for it.
    await page.addInitScript(() => {
      let held
      Object.defineProperty(window, '__ORCH__', {
        configurable: true,
        get: () => held,
        set: (v) => { held = { ...v, platform: 'macos', chrome: 'overlay' } },
      })
    })
  }

  await page.goto(`http://127.0.0.1:${t.port}/`, { waitUntil: 'domcontentloaded' })
  // `body.ready` is the page's own "I have a snapshot and I have drawn it".
  // Asserting on text before that would read an empty document and pass.
  await page.waitForFunction(() => document.body.classList.contains('ready'), null, { timeout: 15_000 })
  // The legend is where the placeholders live, and it is hidden until asked for.
  await page.keyboard.press('Control+Shift+?')
  await page.waitForSelector('#keyhelp:not([hidden])', { timeout: 5000 })

  const seen = await page.evaluate(() => {
    const text = []
    const walk = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT)
    for (let n = walk.nextNode(); n; n = walk.nextNode()) {
      const el = n.parentElement
      if (!el || el.closest('[hidden]') || el.closest('.termhost')) continue
      const s = (n.textContent || '').trim()
      if (s) text.push(s)
    }
    return {
      text,
      wctl: document.querySelectorAll('.wctl').length,
      buttons: document.querySelectorAll('.wctl .wctl-btn').length,
    }
  })

  // Word boundaries: `MOD` is a placeholder, and a description mentioning a
  // modifier by name ("Modifier") must not read as one.
  const placeholder = seen.text.filter((s) => /\bMOD\b/.test(s))
  check(placeholder.length === 0, `no unresolved MOD placeholder${placeholder.length ? `: ${placeholder[0]}` : ''}`)

  // `undefined` as a *word*: a path or a label may legitimately contain the
  // letters, and `NaN` is checked the same way.
  const broken = seen.text.filter((s) => /\b(undefined|NaN)\b/.test(s))
  check(broken.length === 0, `nothing renders undefined or NaN${broken.length ? `: ${broken[0]}` : ''}`)

  check(seen.wctl === 1, `one window-button group${seen.wctl === 1 ? '' : `, found ${seen.wctl}`}`)
  check(seen.buttons === 3, `three window buttons${seen.buttons === 3 ? '' : `, found ${seen.buttons}`}`)

  /* The href rule, in the page's own module rather than a copy of it here. A PR's
     URL comes from GitHub, a review row's from whatever `reviews_command` prints,
     a story's from an agent reading third-party comments — and `javascript:` in
     one of them is a script running with the page's token on a click that looks
     like a link. The refused shapes are the ones a prefix test gets wrong. */
  const hrefs = await page.evaluate(async () => {
    const { safeHref } = await import('/js/core.js')
    return {
      https: safeHref('https://github.com/acme/mono/pull/7'),
      http: safeHref('http://127.0.0.1:9/x'),
      script: safeHref('javascript:fetch("/api/state")'),
      spaced: safeHref('  javascript:alert(1)'),
      cased: safeHref('JavaScript:alert(1)'),
      data: safeHref('data:text/html,<script>1</script>'),
      empty: safeHref(''),
      relative: safeHref('/review-preview'),
    }
  })
  check(hrefs.https.startsWith('https://github.com/'), 'an https URL is left alone')
  check(hrefs.http.startsWith('http://'), 'so is plain http')
  check(hrefs.relative.startsWith(`http://127.0.0.1:${t.port}/`), 'a relative URL resolves against the page')
  const refused = ['script', 'spaced', 'cased', 'data', 'empty']
  const got = refused.filter((k) => hrefs[k] !== '#')
  check(got.length === 0, `nothing but http reaches an href${got.length ? `: ${got.join(', ')} did` : ''}`)


  /* --- what the boot preflight found ----------------------------------------- */

  /* Every one of these used to be a `tracing::warn!` and nothing else, so the
     window showed the symptom — a PR pane reading `unavailable`, a queue reading
     `off` — and the cause lived in a log that a launcher-started app has no
     terminal for. Asserted here rather than in Rust because the failure is the
     *journey*: a field dropped from the snapshot, or a bar nothing calls, both
     compile. */
  const mbar = await page.$eval('#machinebar', (b) => ({ hidden: b.hidden, text: b.textContent }))
  check(!mbar.hidden, 'the boot preflight reaches the window')
  check(mbar.text.includes('/nonexistent-orchd-probe'), 'and the bar names what is missing')
  /* Dismissed before the drag below, because it is `position: fixed` over the
     board and a bar left open is one more thing between a synthetic pointer and
     the row it is aiming at. That it *stays* dismissed under the snapshots that
     keep arriving is the other half worth holding. */
  await page.click('#machinex')
  await page.waitForTimeout(1200)
  check(await page.$eval('#machinebar', (b) => b.hidden), 'and a dismissed bar stays dismissed')

  /* --- a repo with no stack says nothing about one ---------------------------- */

  /* The sandbox carries no compose file, which is the shape of nearly every repo
     that is not the one orchd was written against. The drawer used to draw a red
     dot and the words `stack down` on it, permanently — a feature of one repo
     drawn as a fault on every other. Asserted on the *rendered* header, because
     the daemon's `null` was always available and it was the SPA that read it as
     "down". */
  const stack = await page.$eval('#dcwd', (d) => d.textContent.trim())
  check(stack === '', `a checkout with no compose file says nothing about a stack${stack ? `, got "${stack}"` : ''}`)

  /* --- the rail's session drag ---------------------------------------------- */

  /* Two more worktrees, made here rather than up front so the text assertions
     above run on the page they were written for. Three rows is the fewest that
     tells a reorder from a swap of the pair. */
  for (const name of ['drag-b', 'drag-c']) {
    const { session } = await t.api('POST', '/api/worktree', { name })
    await t.settled(session)
  }
  const rows = page.locator('#rail .sess[data-id]')
  await page.waitForFunction(() => document.querySelectorAll('#rail .sess[data-id]').length >= 3,
    null, { timeout: 15_000 })

  const railNames = () => page.$$eval('#rail .sess[data-id] .sess-name', (ns) => ns.map((n) => n.textContent))
  const before = await railNames()
  const last = before[before.length - 1]
  await rows.nth(await rows.count() - 1).dragTo(rows.nth(0))
  /* The drop writes the order and renders from it; nothing here waits on the
     daemon, so this is the render rather than a round trip. Polled rather than
     slept, because a snapshot arrives every second and the failure this guards is
     one of them rebuilding the list back. */
  const moved = await page.waitForFunction(
    (want) => document.querySelector('#rail .sess[data-id] .sess-name')?.textContent === want,
    last, { timeout: 5000 },
  ).then(() => true).catch(() => false)
  check(moved, 'a dragged session row lands where it was dropped')
  const after = await railNames()
  check(after.length === before.length, 'the drag loses no row')
  check([...after].sort().join('|') === [...before].sort().join('|'), 'and invents none')

  /* The order is yours, so it has to outlive the page. A reload plus the snapshots
     that land after it is the whole failure mode: the list was right until the
     daemon spoke. */
  await page.reload({ waitUntil: 'domcontentloaded' })
  await page.waitForFunction(() => document.body.classList.contains('ready'), null, { timeout: 15_000 })
  await page.waitForTimeout(2000)
  check((await railNames()).join('|') === after.join('|'), 'the order survives a reload and the snapshots after it')

  console.log(`\npage-check: ${failed ? 'FAILED' : 'ok'}`)
} finally {
  await browser?.close()
  await t.stop()
  if (failed || keep) console.log(`  sandbox: ${t.root}`)
}

process.exit(failed ? 1 : 0)
