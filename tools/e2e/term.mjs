#!/usr/bin/env node
// The terminal, driven in a real browser against a real daemon.
//
//   mise run term-e2e            run it
//   mise run term-e2e -- --keep  leave the sandbox behind to read
//
// The rest of the e2e suite is headless and says nothing about the SPA on
// purpose (`docs/e2e.md`). This is the one flow that opens the page, because the
// pty websocket's lifetime lives only in the browser: a dropped socket that drops
// every keystroke in silence (#7) cannot be seen from Rust or from `curl`.
//
// It reuses the same sandbox as the headless flows — a real daemon, a real
// worktree, the fake `claude` on PATH — and adds a browser that opens a session's
// pane. The fake agent writes nothing to stdout, so the observable content is the
// pty's own echo of what is typed: type a line, the line discipline echoes it,
// the daemon rings it, and it comes back over the socket. That round trip is the
// thing being tested, so the assertions read the *bytes the pty socket delivered*
// rather than xterm's rendered glyphs — which keeps this independent of the
// renderer, and lets it run under Chrome (canvas) where the DOM holds no text.
//
// Chrome, via playwright-core's channel, the same as `shot.mjs`: the socket
// lifecycle is engine-agnostic, and no WebKit build is required on disk. The
// WebKitGTK-only concerns are rendering (the DOM-renderer garble), not this.

import assert from 'node:assert/strict'
import { chromium } from 'playwright-core'
import { sandbox } from './harness.mjs'

const keep = process.argv.includes('--keep')

// Runs before any page script (addInitScript), so it wraps the WebSocket every pty
// pane opens. It records each pty socket and accumulates the text every one of them
// delivers, across reconnects, so a test can assert an echo arrived and can close a
// socket to simulate a drop. The events socket passes straight through.
const OBSERVE_SOCKETS = () => {
  const Real = window.WebSocket
  const dec = new TextDecoder()
  window.__pty__ = { socks: [], text: '' }
  function Wrapped(url, protocols) {
    const ws = protocols === undefined ? new Real(url) : new Real(url, protocols)
    if (String(url).includes('/ws/pty')) {
      window.__pty__.socks.push(ws)
      ws.addEventListener('message', (ev) => {
        if (ev.data instanceof ArrayBuffer) window.__pty__.text += dec.decode(ev.data)
      })
    }
    return ws
  }
  // term.js reads `WebSocket.OPEN`, so the constants have to survive the wrap.
  for (const k of ['CONNECTING', 'OPEN', 'CLOSING', 'CLOSED']) Wrapped[k] = Real[k]
  Wrapped.prototype = Real.prototype
  window.WebSocket = Wrapped
}

/** Wait until the accumulated pty text contains `needle`. */
async function waitForEcho(page, needle, note) {
  await page.waitForFunction(
    (s) => (window.__pty__?.text ?? '').includes(s),
    needle,
    { timeout: 10_000 },
  ).catch(() => { throw new Error(`never saw "${needle}" over the pty socket (${note})`) })
}

/** Count of pty sockets ever opened on this page. A reconnect is one more. */
const socketCount = (page) => page.evaluate(() => window.__pty__.socks.length)

async function main() {
  const t = await sandbox({ turns: 1 })
  let browser
  let failed = false
  try {
    // A daemon-cut worktree with a live session, exactly as flow 01 opens one.
    const { session } = await t.api('POST', '/api/worktree', { name: 'term' })
    await t.settled(session)

    browser = await chromium.launch({ channel: 'chrome', args: ['--no-sandbox'] })
    const page = await browser.newPage({ viewport: { width: 1200, height: 800 } })
    page.on('pageerror', (e) => console.error('    page error:', e.message))
    await page.addInitScript(OBSERVE_SOCKETS)
    // GET / is not token-gated and embeds the daemon's own token, so no query is
    // needed — the SPA comes up fully authenticated.
    await page.goto(`http://127.0.0.1:${t.port}/`, { waitUntil: 'domcontentloaded' })

    // Selecting the session is what opens its pane, so its terminal (and its pty
    // socket) only exist after this click.
    const row = page.locator(`[data-id="${session}"]`).first()
    await row.waitFor({ timeout: 15_000 })
    await row.click()

    // --- 1. attach and round-trip -------------------------------------------
    // A pty socket opens and reaches OPEN, then a typed line echoes back. This is
    // the whole happy path, and the guard against the connect() refactor breaking
    // ordinary typing.
    await page.waitForFunction(
      () => window.__pty__.socks.some((s) => s.readyState === WebSocket.OPEN),
      null, { timeout: 15_000 },
    ).catch(() => { throw new Error('the pty socket never opened') })

    // xterm's input is an off-screen helper textarea, so it is never "visible" to
    // Playwright — wait for it attached and focus it, and click the host too, which
    // is how a person hands the pane the keyboard.
    const host = page.locator('#termwrap .termhost:not([hidden])').first()
    const textarea = host.locator('.xterm-helper-textarea')
    await textarea.waitFor({ state: 'attached', timeout: 10_000 })
    const focusTerm = async () => { await host.click(); await textarea.focus() }
    await focusTerm()
    await page.keyboard.type('echo-one\r')
    await waitForEcho(page, 'echo-one', 'first attach')

    // --- 2. a dropped socket heals on its own -------------------------------
    // Close it the way sleep or a network blip does, and the pane must reconnect
    // rather than going deaf forever. The marker shows while it is down.
    const before = await socketCount(page)
    await page.evaluate(() => window.__pty__.socks.at(-1).close())
    await page.waitForSelector('#termwrap .termhost.detached', { timeout: 5_000 })
      .catch(() => { throw new Error('a dropped pane showed no reconnecting marker') })
    await page.waitForFunction(
      (n) => window.__pty__.socks.length > n
        && window.__pty__.socks.at(-1).readyState === WebSocket.OPEN,
      before, { timeout: 15_000 },
    ).catch(() => { throw new Error('the pane never reconnected after the socket dropped') })
    await page.waitForFunction(
      () => !document.querySelector('#termwrap .termhost.detached'),
      null, { timeout: 5_000 },
    ).catch(() => { throw new Error('the reconnecting marker never cleared') })

    await focusTerm()
    await page.keyboard.type('echo-two\r')
    await waitForEcho(page, 'echo-two', 'after reconnect')

    // --- 3. keystrokes typed while down are not lost ------------------------
    // The worst outcome in #7: a blinking cursor over a deaf pane. Close the
    // socket and type immediately, before the ~600ms reconnect — sendInput banks
    // it, and the flush on reattach must deliver it.
    const before2 = await socketCount(page)
    await page.evaluate(() => window.__pty__.socks.at(-1).close())
    await focusTerm()
    await page.keyboard.type('echo-banked\r') // typed while the socket is closed
    await page.waitForFunction(
      (n) => window.__pty__.socks.length > n
        && window.__pty__.socks.at(-1).readyState === WebSocket.OPEN,
      before2, { timeout: 15_000 },
    ).catch(() => { throw new Error('the pane never reconnected the second time') })
    await waitForEcho(page, 'echo-banked', 'banked while down')

    console.log('  terminal e2e: ok — attach, round-trip, reconnect, banked input')
    assert.ok(true)
  } catch (e) {
    failed = true
    console.error('  terminal e2e: FAILED\n   ', String(e.message ?? e))
  } finally {
    if (browser) await browser.close()
    await t.stop()
    if (failed || keep) console.log(`    sandbox: ${t.root}`)
    else t.cleanup()
  }
  process.exit(failed ? 1 : 0)
}

await main()
