#!/usr/bin/env node
// Which renderer each terminal opens with, asserted rather than eyeballed.
//
//   mise run renderer-check          this platform, as the app sees it
//   mise run renderer-check -- --mac the macOS rule, simulated
//   mise run renderer-check -- --keep  keep the sandbox to read
//
// **What this proves, and what it cannot.** It proves the *decision*: that a Mac
// gives the agent pane WebGL and every drawer terminal the DOM renderer (one live
// context per window, the fix for #8), and that a browser tab still gives both the
// canvas. It cannot prove the corruption is gone, and nothing here can:
//
//   * the fault is WKWebView compositing a texture atlas wrong, so it needs the
//     real engine on a real Apple GPU — playwright's "webkit" on Linux is the
//     *GTK* port, a different engine with its own separate version of this bug;
//   * the Tauri window exposes no CDP surface, so nothing can drive it; and
//   * the buffer stays correct while the paint does not, so even a self-check
//     running *inside* the page would read back the right glyphs.
//
// So the pixels need a person on a Mac. What this removes is the other half of
// that question — "was the fix even active?" — which is otherwise indistinguishable
// from "the fix did not work", and which a screen recording answers badly.
//
// The simulation is honest about what it overrides: `platform` and `chrome` are
// both values the *daemon* substitutes into the page (told, not sniffed), so
// setting them exercises the real branch rather than a stand-in for it.

import { chromium } from 'playwright-core'
import { sandbox, until } from './harness.mjs'

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
  const { session } = await t.api('POST', '/api/worktree', { name: 'rend' })
  await t.settled(session)

  browser = await chromium.launch({ channel: 'chrome', args: ['--no-sandbox'] })
  const page = await browser.newPage({ viewport: { width: 1400, height: 900 } })
  page.on('pageerror', (e) => {
    console.error('  page error:', e.message)
    failed = true
  })

  if (asMac) {
    /* Rewriting the served HTML was the obvious way and the wrong one: a fulfilled
       response changes how Chrome classifies the document, and the page's own
       `ws://127.0.0.1` was then refused as a local-network request. So nothing is
       intercepted — `__ORCH__` is defined as an accessor before any page script
       runs, and the page's own assignment is amended as it lands.

       Both fields, or it is not a Mac *app*: `chrome` is `overlay` there, and
       leaving it `none` takes the browser-tab arm and exercises no macOS rule. */
    await page.addInitScript(() => {
      let held
      Object.defineProperty(window, '__ORCH__', {
        configurable: true,
        get: () => held,
        set: (o) => { held = { ...o, platform: 'mac', chrome: 'overlay' } },
      })
    })
  }

  await page.goto(`http://127.0.0.1:${t.port}/`, { waitUntil: 'domcontentloaded' })
  await page.waitForFunction(() => document.body.classList.contains('ready'), null, {
    timeout: 15_000,
  })

  // An agent pane and a drawer terminal, because the rule distinguishes them.
  await page.locator(`[data-id="${session}"]`).first().click()
  await page.waitForSelector('#termwrap .termhost:not([hidden])', { timeout: 10_000 })
  await page.click('#addshell')
  await page.waitForSelector('.drawer-body .termhost:not([hidden])', { timeout: 10_000 })

  /* Read back out of the *daemon's log*, not out of the page. That is the point:
     it is the same line a person on a Mac can paste, so this test and a real bug
     report are answering from one source. Needs the daemon at info — the harness
     runs it at `warn` unless told otherwise. */
  const log = await until(
    'the renderer lines to reach the daemon log',
    async () => {
      const l = t.log()
      return (l.match(/page: .*renderer=/g) || []).length >= 2 ? l : null
    },
    { timeout: 15_000, context: async () => 'no `page: … renderer=` lines yet' },
  )

  const lines = log
    .split('\n')
    .filter((l) => l.includes('page: ') && l.includes('renderer='))
    .map((l) => l.slice(l.indexOf('page: ')))
  for (const l of lines) console.log(`  log   ${l}`)

  const agent = lines.find((l) => l.includes(`session:${session}`))
  const shell = lines.find((l) => !l.includes(`session:${session}`))
  check(!!agent, 'the agent pane reported its renderer')
  check(!!shell, 'a drawer terminal reported its renderer')
  if (!agent || !shell) throw new Error('nothing to assert against')

  const engine = asMac ? 'wkwebview' : 'browser'
  check(agent.includes(`engine=${engine}`), `the engine reads ${engine}`)

  if (asMac) {
    // The #8 rule: one live WebGL context per window.
    check(agent.includes('renderer=webgl'), 'macOS: the agent pane keeps webgl')
    check(shell.includes('renderer=dom'), 'macOS: a drawer terminal falls back to dom')
  } else {
    // A browser tab is deliberately untouched by that rule.
    check(agent.includes('renderer=webgl'), 'browser: the agent pane keeps webgl')
    check(shell.includes('renderer=webgl'), 'browser: a drawer terminal keeps webgl')
  }
} catch (e) {
  failed = true
  console.error(`  FAIL  ${String(e.message ?? e)}`)
} finally {
  if (browser) await browser.close()
  await t.stop()
  if (failed || keep) console.log(`  sandbox: ${t.root}`)
  else t.cleanup()
}

console.log(failed ? '\nrenderer-check: FAILED' : '\nrenderer-check: ok')
process.exit(failed ? 1 : 0)
