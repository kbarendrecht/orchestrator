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

import { chromium } from 'playwright-core'
import { sandbox } from './harness.mjs'

const asMac = process.argv.includes('--mac')
const keep = process.argv.includes('--keep')

let failed = false
const check = (ok, what) => {
  console.log(`  ${ok ? 'ok  ' : 'FAIL'}  ${what}`)
  if (!ok) failed = true
}

const t = await sandbox({ turns: 1 })
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

  console.log(`\npage-check: ${failed ? 'FAILED' : 'ok'}`)
} finally {
  await browser?.close()
  await t.stop()
  if (failed || keep) console.log(`  sandbox: ${t.root}`)
}

process.exit(failed ? 1 : 0)
