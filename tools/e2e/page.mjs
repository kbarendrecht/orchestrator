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
//   * two panes of chrome on a checkout with no forge. `unavailable` beside `off`,
//     each honest on its own, summing to a window that reads as a broken install.
//   * a confirm box in front of a reversible action, or none in front of a lossy
//     one. The rule lives at `confirmBox` in `core.js`; this is what holds it.
//   * a `stack down` badge on a repo that has no stack. `docs/workspace-isolation.md`
//     records that orchd carries no container config at all and calls that the
//     portable default; the drawer contradicted it on every checkout with no
//     compose file.
//   * a pane header clipping its own label. `Changes` read as `Chang…` at the
//     default width, which is a layout fault that shows up as missing words.
//   * a column of clipped paths in the search index. The line and the path were
//     both shrinkable, so flex squeezed the path's *box* while the rigid text in
//     it kept its size and ran off the pane. It needed a real monorepo to see —
//     every path in this sandbox is short — so the sandbox grows two files for it.
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

import fs from 'node:fs'
import path from 'node:path'

import { chromium } from 'playwright-core'
import { sandbox, until } from './harness.mjs'

const asMac = process.argv.includes('--mac')
/* **The modifier the *page* is waiting for, not the one this machine has.**
   `--mac` tells the page it is macOS, so `IS_MAC` is true and `appMod`/`modheld`
   want ⌘ — while `process.platform` on the runner still says linux. Reading the
   runner meant the simulated run pressed `Control` at a page waiting for `Meta`,
   and every modifier-click assertion would have failed for a reason that has
   nothing to do with the code. It never did fail only because nothing ran this
   file with `--mac`; the Linux job does now. */
const MOD_KEY = asMac || process.platform === 'darwin' ? 'Meta' : 'Control'
/** An app chord, spelled for whichever modifier the page is waiting for.
 *
 *  `appMod` is ⌘ on macOS and Ctrl elsewhere — one rule, two spellings — so a
 *  test that hardcodes `Control+Shift+F` is testing one of the two branches and
 *  silently skipping the other. */
const chord = (/** @type {string} */ rest) => `${MOD_KEY}+${rest}`
const keep = process.argv.includes('--keep')

let failed = false
const check = (ok, what) => {
  console.log(`  ${ok ? 'ok  ' : 'FAIL'}  ${what}`)
  if (!ok) failed = true
}

/* A `reviews_command` that is not there, so the boot preflight has something to
   find. Everything else here runs on a healthy sandbox; this one condition is
   deliberately broken, because the bar it raises is the assertion below. */
/* `ws` at info for the restart step at the end, which counts the daemon's own
   `pty client attached` lines; everything else stays at the suite's `warn`. */
const t = await sandbox({
  turns: 1,
  reviewsCommand: ['/nonexistent-orchd-probe'],
  log: 'warn,orchd_serve::ws=info',
})
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
        /* `mac`, which is the word the daemon sends and the word `IS_MAC` reads
           (`host.rs` and `core.js`). It said `macos` here, so this mode has never
           actually made the page believe it was macOS — every assertion under
           `--mac` was running the Linux branch, quietly. `renderer.mjs` had it
           right, which is why nothing noticed. */
        set: (v) => { held = { ...v, platform: 'mac', chrome: 'overlay' } },
      })
    })
  }

  await page.goto(`http://127.0.0.1:${t.port}/`, { waitUntil: 'domcontentloaded' })
  // `body.ready` is the page's own "I have a snapshot and I have drawn it".
  // Asserting on text before that would read an empty document and pass.
  await page.waitForFunction(() => document.body.classList.contains('ready'), null, { timeout: 15_000 })
  // The legend is where the placeholders live, and it is hidden until asked for.
  await page.keyboard.press(chord('Shift+?'))
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

  /* **A pane header that does not fit its own pane.** `Changes` came back as
     `Chang…` at the default 296px, because the title and the `since <sha>` beside
     it could both shrink and the longer string won. It is a layout fault, but it
     is a *text* one at heart — the word a person reads is not there — and
     `scrollWidth > clientWidth` is what the ellipsis actually is, so it needs no
     pixel baseline. Every element carrying its own label in a fixed-width pane
     header, so a third one added later is covered without another line here. */
  const clipped = await page.evaluate(() => {
    /* The strings the pane really draws, put there rather than waited for: the
       sandbox workspace has no merge base, so `#filesbase` is empty and the title
       it competes with has nothing to lose to. `diff.js` writes exactly these two
       — `Changeset` is the longer title, and a base is a 7-character short sha. */
    const title = document.getElementById('filestitle')
    const base = document.getElementById('filesbase')
    const was = [title.textContent, base.textContent]
    title.textContent = 'Changeset'
    base.textContent = 'since 1a2b3c4'
    /* **Under the app's own chrome, which is the case that clipped.** A browser tab
       draws no window buttons, so `.top-r` has ~100px this checks nothing about;
       `custom` is what the Linux app runs and what reveals `.wctl`. Set here rather
       than in a second browser launch, because it is a CSS branch and the rules it
       turns on (`.wctl{display:flex}`, `.top-r{padding-right:0}`) are the whole of
       the difference. */
    const chrome = document.body.dataset.chrome
    document.body.dataset.chrome = 'custom'
    const out = []
    for (const el of document.querySelectorAll('.top-r .eyebrow, .top-r .ctx-btn, .rvhead .eyebrow')) {
      const t = (el.textContent || '').trim()
      // Overflow by a subpixel is the browser rounding, not an ellipsis.
      if (t && el.scrollWidth > el.clientWidth + 1) out.push(`${el.id || el.className}: ${t}`)
    }
    ;[title.textContent, base.textContent] = was
    if (chrome === undefined) delete document.body.dataset.chrome
    else document.body.dataset.chrome = chrome
    return out
  })
  check(clipped.length === 0, `no header label is clipped${clipped.length ? `: ${clipped[0]}` : ''}`)

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

  /* --- the settings pane's two halves stay apart ------------------------------- */

  /* **Both directions, because this has broken both ways.** A config field added to
     the markup and not to the dirty list lost your typing in silence; delegating to
     the whole pane instead then made a *theme* change mark the config unsaved,
     which stops `loadConfigInto` re-reading for the rest of the page's life — so
     the next open shows another checkout's values and Save writes them here.
     `[data-config]` is the line between them, and this is what holds it. */
  await page.click('#gearbtn')
  await page.waitForSelector('#settings:not([hidden])', { timeout: 5000 })
  const dirtyAfter = async (/** @type {string} */ sel, /** @type {string} */ value) => {
    await page.fill(sel, value)
    await page.waitForTimeout(150)
    return page.$eval('#setdiscard', (b) => !b.hidden)
  }
  check(await dirtyAfter('#setnotemain', 'the dev stack runs here') === true,
    'a config field marks the pane unsaved')
  await page.click('#setdiscard')
  await page.waitForTimeout(300)
  check(await page.$eval('#setdiscard', (b) => b.hidden), 'and Discard clears it')
  // A theme control is this browser's and applies at once, so it is not a draft.
  await page.selectOption('#thpreset', { index: 1 }).catch(() => {})
  await page.waitForTimeout(300)
  check(await page.$eval('#setdiscard', (b) => b.hidden),
    'changing the theme does not mark the config unsaved')
  await page.click('#setclose')
  await page.waitForTimeout(300)

  /* --- a checkout with no forge draws one line, not two panes ------------------ */

  /* The sandbox's `origin` is a local clone, so GitHub has never heard of it —
     which is the shape of a fresh install. The PR pane used to read `unavailable`
     and the review queue `off` beside it: two headers, two counts, two refresh
     buttons and two carets, every label honest and the sum looking broken. */
  /* **Computed style, not the `hidden` property.** `.rvblock` carries an author
     `display:flex`, which beats the UA's `[hidden]{display:none}` — so a pane given
     `el.hidden = true` was laid out and on screen while the property read `true`,
     and the first version of this check passed over it. Whether a thing is *drawn*
     is the only question worth asking of a page. */
  const forgeless = await page.evaluate(() => {
    const rv = document.querySelector('#rvblock')
    return {
      pr: (document.querySelector('#prpane')?.textContent || '').trim(),
      rvShown: !!rv && getComputedStyle(rv).display !== 'none',
      rvText: (rv?.textContent || '').trim().replace(/\s+/g, ' '),
    }
  })
  check(/No GitHub remote here/.test(forgeless.pr),
    `a forgeless checkout says it once${forgeless.pr ? `, got "${forgeless.pr.slice(0, 60)}"` : ''}`)
  /* **And the review queue is still there, which is the point.** This sandbox has
     no forge *and* a `reviews_command`, which is a real arrangement — a repo that
     ranks its own reviews on a remote GitHub has never heard of. The queue was
     hidden on `repos.upstream` for one commit, so those rows were deleted from the
     window while the daemon went on fetching them. The pane answers to
     `reviews.state`: `off` is the daemon's own "no command and no repo", and a
     command that will not run is `degraded` and must read as broken. */
  /* **And `hidden` must actually hide it**, which this sandbox cannot reach on its
     own: `off` needs no command *and* no repo, and the preflight assertion above
     wants a command that is broken rather than absent. So the rule is asserted
     directly — an author `display` with no `[hidden]` companion is the trap
     `app.css` names in four other places, and it had this pane on screen behind a
     `hidden` that did nothing. */
  const hides = await page.evaluate(() => {
    const rv = document.querySelector('#rvblock')
    if (!rv) return null
    const was = rv.hidden
    rv.hidden = true
    const gone = getComputedStyle(rv).display === 'none'
    rv.hidden = was
    return gone
  })
  check(hides === true, 'a hidden review block is actually not displayed')

  check(forgeless.rvShown && /unavailable/.test(forgeless.rvText),
    `a configured review command is drawn whatever the forge says, got "${forgeless.rvText.slice(0, 60)}"`)

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
  // Sorted joins settle both halves: unequal lengths cannot produce equal joins.
  check([...after].sort().join('|') === [...before].sort().join('|'),
    'the drag neither loses a row nor invents one')

  /* The order is yours, so it has to outlive the page. A reload plus the snapshots
     that land after it is the whole failure mode: the list was right until the
     daemon spoke. */
  await page.reload({ waitUntil: 'domcontentloaded' })
  await page.waitForFunction(() => document.body.classList.contains('ready'), null, { timeout: 15_000 })
  await page.waitForTimeout(2000)
  check((await railNames()).join('|') === after.join('|'), 'the order survives a reload and the snapshots after it')

  /* --- the number the window chrome depends on --------------------------------- */

  /* **The top row's height is two copies of one number**, and the other is
     `TOP_ROW` in `desktop/src/main.rs`: macOS centres the traffic lights in a 28pt
     band of its own, so putting them on this row's centre line needs the row's
     height in Rust (#29). Nothing in a browser can see the lights — that half is
     `app-check` on macos-14 — so what is asserted here is the half a browser *can*
     see, which is the number the other half is derived from. */
  const topRow = await page.evaluate(
    () => getComputedStyle(document.querySelector('.app')).gridTemplateRows.split(' ')[0])
  check(topRow === '46px', `the top row is 46px, or TOP_ROW in main.rs is now wrong — got ${topRow}`)

  /* --- the close code the page stops retrying on (#32) -------------------------- */

  /* **Two copies of one number**, and the other is `PTY_EXITED` in
     `crates/orchd-serve/src/ws.rs`. The page reconnects with backoff when a pty
     socket drops, because a dropped one used to eat every keystroke under a
     blinking cursor (#7) — so a shell somebody typed `exit` into retried forever,
     against a pty the daemon had already reaped. The daemon says which of the two
     happened with a close code now, and the page stops only on that one.

     The behaviour is driven on the wire by
     `tools/e2e/flows/22-drawer-processes.mjs`, which ends a real shell and asserts
     the code it receives. What that flow cannot see is the page agreeing about the
     number, and a mismatch is silent: every close would read as a blip again. Read
     out of the served source, because the constant is module-private and exporting
     it for a test would be the wrong trade.

     What neither holds is the pane's own badge. Driving it needs a pane that is
     attached at the moment its pty dies, and by the end of a run the board has
     moved on — every attempt read whichever pane the drawer or the centre had
     fallen back to. Left to the two halves above, deliberately. */
  const termSrc = await page.evaluate(() => fetch('/js/term.js').then((r) => r.text()))
  check(/const PTY_EXITED = 4000\b/.test(termSrc),
    'term.js still stops its backoff on close code 4000')

  /* --- the add row, at every rail width --------------------------------------- */

  /* `+ worktree`, `+ main` and `archived` share one line under the list. They
     spent a commit on the project header instead, which costs no row — and three
     labels did not fit: at `COLS.rail.min`, 210px and one drag away, the last of
     them ran off the right edge, where `.rail`'s `overflow:hidden` swallows it
     silently. The row has the same arithmetic to answer, so it is measured here
     rather than reasoned about. */
  const { session: doomed } = await t.api('POST', '/api/worktree', { name: 'archive-me' })
  await t.settled(doomed)
  await t.api('POST', `/api/session/${doomed}/kill`)
  await page.waitForFunction(() => !!document.querySelector('#rail .ws-add .arctoggle'),
    null, { timeout: 15_000 })

  const addText = await page.$$eval('#rail .ws-add .addbtn', (b) => b.map((x) => x.textContent))
  check(addText.join('|') === '+ worktree|+ main',
    `the add row carries both verbs, got ${JSON.stringify(addText)}`)
  check(await page.locator('#rail .ws-title .addbtn').count() === 0,
    'and the project header carries none of them')

  const atWidth = (px) => page.evaluate((w) => {
    const root = document.documentElement
    const had = root.style.getPropertyValue('--rail')
    root.style.setProperty('--rail', `${w}px`)
    const rail = document.querySelector('#rail').getBoundingClientRect()
    const arc = document.querySelector('#rail .ws-add .arctoggle').getBoundingClientRect()
    const name = document.querySelector('#rail .ws-title .co-name').getBoundingClientRect()
    root.style.setProperty('--rail', had)
    return { spill: Math.round(arc.right - rail.right), arc: Math.round(arc.width), name: Math.round(name.width) }
  }, px)

  for (const w of [320, 210]) {
    const { spill, arc, name } = await atWidth(w)
    check(spill <= 0, `at ${w}px the archive stays inside the rail, overflowed by ${spill}px`)
    // Clipped to nothing is the same fault as pushed off the edge, one rule along.
    check(arc >= 40, `at ${w}px the archive keeps its word, got ${arc}px wide`)
    check(name >= 24, `at ${w}px the project name is readable, got ${name}px`)
  }

  /* --- the archive filters what is in it -------------------------------------- */

  /* The filter is inside the box the caret opens, and it answers in two waves:
     the names out of the snapshot, then whatever the daemon finds in the
     transcripts. Only the first is asserted here — the second is one `getOn` away
     and needs a transcript with words in it — but the count is the contract both
     waves write to, and a filter that matched nothing while claiming otherwise is
     the failure worth catching. */
  await page.click('#rail .ws-add .arctoggle')
  await page.waitForSelector('#rail .arcbox .arcq', { timeout: 10_000 })
  const arcRows = () => page.locator('#rail .arcbox .sess.arc').count()
  check(await arcRows() >= 1, 'the archive opens on the conversation that was killed')
  await page.fill('#rail .arcbox .arcq', 'zzzznothinglikethis')
  await page.waitForFunction(() => document.querySelectorAll('#rail .arcbox .sess.arc').length === 0,
    null, { timeout: 5000 })
  const counted = await page.textContent('#rail .arcbox .arcn')
  check(/^0 of [1-9]/.test(counted ?? ''),
    `and a query nothing matches says so against the total, got "${counted}"`)
  await page.fill('#rail .arcbox .arcq', '')
  await page.waitForFunction(() => document.querySelectorAll('#rail .arcbox .sess.arc').length >= 1,
    null, { timeout: 5000 })
  check(await page.textContent('#rail .arcbox .arcn') === '',
    'and clearing it puts the archive back with no count to explain')

  /* --- Backspace is not a way out of the board -------------------------------- */

  /* **The other half of #14, and the half that can be proven here.** A bare
     `Backspace` outside a text field is a navigation key: clicking a titlebar
     focuses something that is not editable, and one press took the window back to
     the splash it was launched on — no forward item, and a reload that reloads the
     splash. `boot_daemon` now replaces that entry rather than pushing the board on
     top of it, but that entry is WKWebView's and this browser has none, so the
     destination cannot be asserted anywhere but a Mac. The *trigger* can.

     `dispatchEvent` returns false when something called `preventDefault`, which is
     the whole question. Asked that way rather than with a second listener, because
     the app's own handler calls `stopPropagation` on a key it took — so a probe
     listening after it would never run, and the gate would pass by never firing. */
  const swallows = (sel) => page.evaluate((s) => {
    const el = document.querySelector(s)
    if (!el) return null
    return !el.dispatchEvent(
      new KeyboardEvent('keydown', { key: 'Backspace', bubbles: true, cancelable: true }))
  }, sel)

  check(await swallows('#rail') === true, 'Backspace outside a text field is refused')
  /* The two ways this cure could be worse than the disease, and both are one
     `closest` call away from each other. A terminal that cannot delete a character
     is the louder of them, which is why it is asserted on the real helper textarea
     xterm focuses rather than on a stand-in. */
  check(await swallows('#setlang') === false, 'Backspace still reaches a text field')
  check(await swallows('.xterm-helper-textarea') === false, 'Backspace still reaches the pty')

  /* --- the two bottom-docked panels do not sit on each other ------------------ */

  /* **#21 was a screenshot of the review bar drawn across the question's answer
     field.** Both dock at `bottom: var(--oq-clear)` — on purpose, so each clears
     the agent's input line — and the question grows upward from that edge, so
     whenever both are up the bar landed on it. `--oq-h` is the question's measured
     height now, written by a `ResizeObserver`, and the bar rises by it.

     Driven by unhiding the two elements rather than by getting a real session to
     ask something mid-review: the rule under test is layout, and a geometry
     assertion does not care which state machine produced the two boxes. What it
     does care about is that the observer really fires, which a hand-set variable
     would have faked.

     Asked as "the rectangles do not intersect", not "the bar is below": the bar
     rises *above* the question, because the two have different containing blocks
     and the offset carries it clear. Which side it ends up on is layout's business;
     not covering the answer field is the contract. Touching is allowed — demanding
     a gap would be a second, invented rule. */
  const overlap = await page.evaluate(async () => {
    const oq = document.getElementById('oq')
    const bar = document.getElementById('rvbar')
    if (!oq || !bar) return null
    oq.innerHTML = '<div class="oqh">needs your call</div><div class="oqq">'
      + 'a question long enough to be more than one line, so the bar has something to clear'
      + '</div>'
    oq.hidden = false
    bar.textContent = 'review · applying'
    bar.hidden = false
    // Two frames: one for the boxes to lay out, one for the observer's write to
    // land and the bar to be positioned from it.
    await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))
    const a = oq.getBoundingClientRect()
    const b = bar.getBoundingClientRect()
    const clear = b.bottom <= a.top || b.top >= a.bottom

    oq.hidden = true; oq.replaceChildren(); bar.hidden = true; bar.textContent = ''
    return { clear, oq: `${Math.round(a.top)}-${Math.round(a.bottom)}`, bar: `${Math.round(b.top)}-${Math.round(b.bottom)}` }
  })
  check(overlap?.clear === true,
    `the review bar clears the open question (question ${overlap?.oq}, bar ${overlap?.bar})`)

  /* --- a binding is its exact modifiers ---------------------------------------- */

  /* **#20: `Ctrl+Option+Cmd+←` is how macOS moves a window to the next display**,
     and the diff overlay's `Ctrl+←` claimed it because the branch tested `ctrlKey`
     alone — every chord that merely *contains* Ctrl matched. Asserted on the event
     rather than through the overlay, because the defect is the guard and not what
     the guard protects: an app that swallows a chord it does not implement is the
     whole of it. */
  /* **The overlay has to be open, or this gate asserts nothing.** The branch lives
     under `if (Diff.state.open)`, so on a board with no diff up both chords pass
     through untouched and a reverted fix still reads green — measured, that is
     exactly what happened to the first version of these two lines. Opening it
     through the module's own state rather than by clicking a file: the thing under
     test is the guard, and `stepChange` on an empty changeset is a no-op by its
     own first branch. */
  const withDiffOpen = async (init) => page.evaluate(async (d) => {
    const Diff = await import('/js/diff.js')
    const was = Diff.state.open
    Diff.state.open = true
    const took = !document.body.dispatchEvent(
      new KeyboardEvent('keydown', { ...d, bubbles: true, cancelable: true }))
    Diff.state.open = was
    return took
  }, init)

  /* The pair is the point. Without the first line the second proves only that
     *some* key went unclaimed, which a closed overlay also satisfies. */
  check(await withDiffOpen({ key: 'ArrowLeft', ctrlKey: true }) === true,
    'Ctrl+Left still steps the changeset')
  check(await withDiffOpen({ key: 'ArrowLeft', ctrlKey: true, altKey: true, metaKey: true }) === false,
    'Ctrl+Option+Cmd+Left is left to the window manager')

  /* --- the chord labels and the drawn icons are nodes, not markup ------------- */

  /* Both were `innerHTML` and are DOM calls now, which is what lets the SPA's
     no-HTML-sinks rule (`no-restricted-syntax`, `tools/eslint.config.mjs`) have no
     exceptions. Neither break is visible to `tsc` or to ESLint: a chord left
     reading `MOD Shift N`, a `kbd` eaten by the rewrite, and an icon that draws
     nothing all type-check and lint clean. The placeholder itself is already
     asserted above, on the rendered text. */
  const kbds = await page.$$eval('[data-mod] kbd', (ns) => ns.length)
  check(kbds > 0, `the chord rewrite kept the kbd children it runs over, got ${kbds}`)
  const drawn = await page.evaluate(async () => {
    const core = await import('/js/core.js')
    const c = core.caret()
    const svg = c.firstElementChild
    return svg?.namespaceURI === 'http://www.w3.org/2000/svg'
      && svg.tagName === 'svg' && svg.querySelectorAll('path').length === 1
  })
  check(drawn === true, 'a drawn icon is a real SVG node with its path')

  /* **Last, and deliberately so.** The move below puts a session into main,
     and the rail draws main's sessions and the worktrees' as two runs. The drag
     above reorders *within* a run and refuses a drop across them, so running
     this first left that drag aiming at the other side of the boundary. */
  /* --- a box is for destructive, and for nothing else ------------------------- */

  /* The rule is in `core.js` at `confirmBox`: a question is for work that cannot be
     got back, and loudness is answered with a toast instead. It was eight boxes and
     is four, so the drift this holds runs both ways — a reversible action growing a
     box, and one of the four losing one. Asserted through the menu rather than by
     counting call sites, because what matters is what a press actually does. */
  /* The real ids, and no `.catch`. Three selectors were guessed at here — `#dialog`
     and `.dialog` exist nowhere in the page — and a miss became `null`, which
     passes `!== true`: the half of this gate that guards against a box coming back
     could not fail. `#dlg` and `#ctxmenu` are what `index.html` actually has. */
  const rowMenu = async (label) => {
    await page.click('#rail .sess[data-id]', { button: 'right' })
    await page.waitForSelector('#ctxmenu:not([hidden]) button', { timeout: 5000 })
    for (const item of await page.$$('#ctxmenu button')) {
      if ((await item.textContent())?.trim() === label) return item
    }
    return null
  }
  // The app's own answer, not a guess at its markup.
  const asking = () => page.evaluate(async () => (await import('/js/core.js')).dialogOpen())

  /* #31: what the session's directory has checked out, for pasting into a
     terminal. The *workspace's* branch rather than the session's own field, which
     is what the conversation was about and stops being re-stamped once the session
     is archived. The item is drawn disabled when there is none, so `isEnabled` is
     the half that says the value arrived: a row whose workspace never reached the
     page would still offer the item. */
  const branch = await rowMenu('copy branch')
  check(!!branch, 'the row offers to copy its branch')
  check(await branch?.isEnabled() === true, 'and a worktree session has one to copy')
  await page.keyboard.press('Escape')

  const move = await rowMenu('move to main')
  check(!!move, 'the row offers a move')
  await move?.click()
  /* A condition, not a clock: the move is done when the rail says the session is in
     main. A fixed sleep here is the trap `docs/traps/e2e.md` names — it guesses at a
     branch swap plus a pty respawn on a loaded runner, and a gate that fails for
     timing teaches everybody `--no-verify`. */
  await page.waitForFunction(
    () => !!document.querySelector('#rail .sess[data-id] .sess-main'),
    null, { timeout: 30_000 },
  )
  check((await asking()) !== true, 'moving a branch to main asks nothing — it is reversible')

  const del = await rowMenu('delete')
  check(!!del, 'the row offers a delete')
  await del?.click()
  await page.waitForSelector('#dlg:not([hidden])', { timeout: 5000 })
  check((await asking()) === true, 'deleting a session still asks — the transcript does not come back')
  const said = await page.$eval('#dlg', (d) => d.textContent || '')
  check(/for good/.test(said), 'and the box says what goes for good')
  await page.keyboard.press('Escape')

  /* --- Shift-Shift opens the file search, and typing capitals does not --------- */

  /* **The refusal is the assertion with power here.** "Two Shift keydowns inside
     the window" also describes somebody typing `Shift A Shift B`, so a detector
     that only measured the interval would open the overlay mid-sentence. The guard
     is that any other key pressed while Shift is held disqualifies that tap, and
     this pair is what holds it: the gesture works, and the prose does not trigger
     it.

     Nothing here waits between the taps, deliberately: the window is `TAP` in
     `app.js` and this test is about the guard, not the number. A test that slept
     200ms would start failing the day somebody tightened it for a good reason.

     Driven with real `down`/`up` rather than synthetic events, because the guard
     turns on the keyup arriving between the two presses — a `dispatchEvent` of
     keydown alone would pass while the feature was broken. */
  await page.keyboard.press('Escape')
  const findUp = () => page.$eval('#fnoverlay', (o) => o.classList.contains('on')).catch(() => null)
  const tapShift = async () => { await page.keyboard.down('Shift'); await page.keyboard.up('Shift') }

  await page.click('#rail')
  await tapShift()
  await tapShift()
  await page.waitForTimeout(150)
  check(await findUp() === true, 'Shift Shift opens the file search')

  await page.keyboard.press('Escape')
  await page.waitForTimeout(100)
  check(await findUp() === false, 'and Escape closes it')

  // A capital, then another: two Shift taps with a letter inside each, which is
  // the shape the guard exists to refuse.
  await page.keyboard.down('Shift')
  await page.keyboard.press('KeyA')
  await page.keyboard.up('Shift')
  await page.keyboard.down('Shift')
  await page.keyboard.press('KeyB')
  await page.keyboard.up('Shift')
  await page.waitForTimeout(150)
  check(await findUp() === false, 'typing two capitals does not open it')

  /* --- and a dialog hands the keyboard back when it closes -------------------- */

  /* **The keystrokes after `Escape` went nowhere** (#27): the finder took focus
     from the pane, hiding it left focus on `document.body`, and typing then did
     nothing until the pane was clicked. `borrowFocus`/`returnFocus` in `core.js`
     is the fix, and this is the report's own steps.

     Focused through the textarea rather than by clicking the pane, for the reason
     the drawer's own `focusTerm` gives below: a click in the rows is how a path is
     opened now. */
  const focusedNow = () => page.evaluate(() => {
    const a = document.activeElement
    return a ? (a.className || a.id || a.tagName) : null
  })
  await page.$eval('#termwrap .termhost:not([hidden]) .xterm-helper-textarea',
    (t) => /** @type {HTMLTextAreaElement} */ (t).focus())
  const hadIt = await focusedNow()
  check(/xterm-helper-textarea/.test(hadIt ?? ''), `the pane has the keyboard, got ${hadIt}`)

  await tapShift()
  await tapShift()
  await page.waitForTimeout(150)
  check(await findUp() === true, 'Shift Shift opens the finder over the pane')
  await page.keyboard.press('Escape')
  await page.waitForTimeout(150)
  const gotBack = await focusedNow()
  check(gotBack === hadIt, `and closing it hands the keyboard back, got ${gotBack}`)

  /* --- a pane with nothing on it says so in the middle ------------------------ */

  /* **Where you look when a pane is blank is the middle of it**, not the top
     right corner — which is where this sat, reading as a decoration rather than
     as the answer. The stand-in agent prints nothing, so the centre pane is
     exactly the case: attached, empty, and waiting. */
  const badge = await page.$eval('#termwrap .termhost:not([hidden]) .term-badge', (b) => ({
    shown: !b.hidden,
    mid: b.classList.contains('mid'),
    says: b.textContent,
  })).catch(() => null)
  /* The contract rather than one state of it: whatever the pane is saying, the
     pill is centred unless there is text underneath it to cover. Two states have
     that text — `reconnecting…`, and `exited` since #32, which is what a pane says
     once its pty is gone for good. Asserted this way because which state a pane is
     in here depends on what the socket did a moment ago, and the rule does not.

     **That second state is also the proof the fix landed.** This line read
     `reconnecting…` before #32, forever, on a pane whose agent had finished. */
  check(
    !!badge && badge.shown && badge.mid === !/reconnecting|exited/.test(badge.says ?? ''),
    `an empty pane says so in the middle, got ${JSON.stringify(badge)}`,
  )

  /* And the rule itself, applied by hand: which state a pane is in here depends on
     what its socket did a moment ago, so the assertion above can only ever prove
     one branch. This proves the other — that `mid` puts the pill in the middle of
     the pane rather than merely naming the intention. */
  check(
    await page.$eval('#termwrap .termhost:not([hidden])', (host) => {
      const b = host.querySelector('.term-badge')
      if (!b) return false
      const had = b.classList.contains('mid')
      b.classList.add('mid')
      const pill = b.getBoundingClientRect()
      const pane = host.getBoundingClientRect()
      const off = Math.abs((pill.left + pill.width / 2) - (pane.left + pane.width / 2))
      if (!had) b.classList.remove('mid')
      return off < 3
    }),
    'and the centred rule really centres it',
  )

  /* --- the search answers, and the viewer shows the file it found ------------- */

  /* **The overlay is the viewer, so this is one assertion about both.** Opening it
     proves the chord; only typing into it proves the route, the index and the file
     underneath are wired to each other. The word is written into the worktree
     first and never committed, which also asks the question the daemon's own
     walk answers: an untracked file an agent wrote a moment ago has to be
     findable, because that is the normal state of everything here. */
  const tree = (await t.session(session)).cwd
  fs.writeFileSync(
    path.join(tree, 'haystack.txt'),
    'first line\nsecond line\nthe frobnicate word is here\nfourth line\n',
  )
  await page.keyboard.press(chord('Shift+KeyF'))
  await page.waitForSelector('#fnoverlay.on', { timeout: 5000 })
  await page.fill('#fnq', 'frobnicate')
  await page.waitForFunction(
    () => !!document.querySelector('#fnhits .fnhit'), null, { timeout: 5000 })

  /* **The row says what was found, and where.** The line is the left half because
     it is the answer; the path is the right half because a column of paths lines
     up and a column of snippets does not. Asserted as two elements rather than as
     the row's text, or a layout swap would read as a pass. */
  const snip = await page.$eval('#fnhits .fnhit .fnsnip', (r) => r.textContent)
  check(snip === 'the frobnicate word is here', `the index draws the line, got ${snip}`)
  check(
    await page.$eval('#fnhits .fnhit .fnsnip .tok-find', (m) => m.textContent)
      .catch(() => null) === 'frobnicate',
    'with the match marked in it, the same colour the viewer uses',
  )
  const at = await page.$eval('#fnhits .fnhit .fnat', (r) => r.textContent)
  check(at === 'haystack.txt:3', `and the path on the right, got ${at}`)
  check(
    await page.$eval('#fnhits .fnhit', (r) => {
      const s = r.querySelector('.fnsnip')?.getBoundingClientRect()
      const a = r.querySelector('.fnat')?.getBoundingClientRect()
      return !!s && !!a && s.left < a.left
    }),
    'and the path is to the right of the line, not merely after it in the markup',
  )

  /* **A long line must not push the path off the edge**, which is what the first
     cut of this row did: the line and the path were both shrinkable, so flex took
     the slack out of both, squeezing the path's *box* while the text inside it
     kept its size and spilled past the pane. It read as a column of clipped
     paths, and it needed a real monorepo to see — every path in this sandbox is
     short. So the sandbox grows one file that is long in both ways. */
  /* The match leads the line, because the snippet is a window around it: put the
     word at the end and the window is 50 characters long and proves nothing. */
  const wide = `frobnicate ${'padding words to make this line far too wide for any pane '.repeat(8)}\n`
  const deep = path.join(tree, 'a/deep/nested/directory/that/keeps/going')
  fs.mkdirSync(deep, { recursive: true })
  fs.writeFileSync(path.join(deep, 'AFileWithARatherLongNameIndeed.txt'), wide)
  /* **And one at the root, which is the case that actually spilled.** A deep path
     has a directory to give way, so it absorbs the squeeze and looks fine; a file
     with no directory at all leaves a name and a line number that cannot shrink,
     and those are what ran off the edge. Both are here because each misses the
     other's fault. */
  fs.writeFileSync(path.join(tree, 'AFileWithAnEquallyLongNameAtTheRoot.txt'), wide)
  await page.fill('#fnq', 'frobnicate')
  await page.waitForFunction(
    () => document.querySelectorAll('#fnhits .fnhit').length === 3, null, { timeout: 5000 })
  const spill = await page.$eval('#fnhits', (box) => {
    const right = box.getBoundingClientRect().right
    const past = [...box.querySelectorAll('.fnline, .fnbase')]
      .filter((e) => e.getBoundingClientRect().right > right + 0.5)
    return { sideways: box.scrollWidth > box.clientWidth, past: past.length }
  })
  check(
    !spill.sideways && spill.past === 0,
    `nothing spills past the index, got ${JSON.stringify(spill)}`,
  )
  // And the half that says the shrink went where it was meant to: the directory
  // is what gives way, never the name or the line number.
  const kept = await page.$$eval('#fnhits .fnhit', (rows) => rows.map((r) => {
    const cut = (sel) => {
      const e = r.querySelector(sel)
      return !!e && e.scrollWidth > e.getBoundingClientRect().width + 0.5
    }
    return { dir: cut('.fndir'), base: cut('.fnbase'), line: cut('.fnline') }
  }))
  check(
    kept.some((r) => r.dir) && kept.every((r) => !r.base && !r.line),
    `the directory truncates and the name does not, got ${JSON.stringify(kept)}`,
  )
  // Taken away again: everything below this is written about a search with one
  // answer in it, and a second hit moves the cursor off the file they assert on.
  fs.rmSync(path.join(tree, 'a'), { recursive: true, force: true })
  fs.rmSync(path.join(tree, 'AFileWithAnEquallyLongNameAtTheRoot.txt'), { force: true })
  await page.fill('#fnq', 'frobnicate')
  await page.waitForFunction(
    () => document.querySelectorAll('#fnhits .fnhit').length === 1, null, { timeout: 5000 })

  /* --- and the split between the two panes is draggable ----------------------- */

  /* A search that answers with forty rows and one that answers with two want
     different splits, so the index takes a third by default and moves from there.
     The reset is asserted beside the drag because it is the way back from a bad
     one, and because it is what leaves this page as the assertions below expect. */
  const indexHeight = () => page.$eval('#fnhits', (e) => e.getBoundingClientRect().height)
  const wasTall = await indexHeight()
  const grip = await page.$eval('#fnsplit', (e) => {
    const b = e.getBoundingClientRect()
    return { x: b.x + b.width / 2, y: b.y + b.height / 2 }
  })
  await page.mouse.move(grip.x, grip.y)
  await page.mouse.down()
  await page.mouse.move(grip.x, grip.y + 90)
  await page.mouse.up()
  const dragged = await indexHeight()
  check(
    dragged > wasTall + 60,
    `dragging the split makes the index taller, ${Math.round(wasTall)} to ${Math.round(dragged)}`,
  )
  await page.dblclick('#fnsplit')
  const reset = await indexHeight()
  check(
    Math.abs(reset - wasTall) < 2,
    `and a double-click puts it back, got ${Math.round(reset)} against ${Math.round(wasTall)}`,
  )

  /* --- the mode is a switch, not a caption ----------------------------------- */

  /* Both chords exist, and neither is on screen: `Shift Shift` and a three-key
     chord are not things a person finds by looking. The label names the question
     being asked now, and clicking it asks the other one. */
  check(
    await page.$eval('#fnmode', (m) => m.textContent) === 'find contents',
    'the mode label names the search, not its object',
  )
  // Clicked through the element, like every other button in this overlay: the
  // update pill floats over the middle of any overlay header.
  await page.$eval('#fnmode', (b) => b.click())
  await page.waitForFunction(
    () => document.getElementById('fnmode')?.textContent === 'find files',
    null, { timeout: 5000 })
  /* `README.md` rather than the file written above, and the reason is worth
     knowing: the path list is fetched once per workspace and kept, so a file
     created after the overlay first opened is not in it until the page reloads.
     That is `loadPaths`, not this test. */
  await page.fill('#fnq', 'readme')
  await page.waitForFunction(
    () => !!document.querySelector('#fnhits .fnhit'), null, { timeout: 5000 })
  const named = await page.$eval('#fnhits .fnhit', (r) => ({
    text: r.textContent, snips: r.querySelectorAll('.fnsnip').length,
  }))
  check(
    named.text === 'README.md' && named.snips === 0,
    `names mode is the path alone, got ${JSON.stringify(named)}`,
  )
  await page.$eval('#fnmode', (b) => b.click())
  await page.waitForFunction(
    () => document.getElementById('fnmode')?.textContent === 'find contents',
    null, { timeout: 5000 })
  await page.fill('#fnq', 'frobnicate')
  await page.waitForFunction(
    () => !!document.querySelector('#fnhits .fnhit .fnsnip'), null, { timeout: 5000 })

  // The file under it, at the line the hit named — and the match marked through
  // the same range machinery the word-diff paints with.
  await page.waitForFunction(
    () => !!document.querySelector('#fnsrc .fnrow.on'), null, { timeout: 5000 })
  const shown = await page.$eval('#fnsrc .fnrow.on', (r) => r.textContent)
  check(shown === 'the frobnicate word is here', `the viewer shows the matched line, got ${shown}`)
  check(
    await page.$eval('#fnsrc .fnrow.on .tok-find', (m) => m.textContent).catch(() => null) === 'frobnicate',
    'and the match itself is marked',
  )
  // The whole file is there to scroll, not only the matched line.
  const viewerRows = await page.$$eval('#fnsrc .fnrow', (rs) => rs.length)
  check(viewerRows === 5, `the viewer holds the file, not the hit, got ${viewerRows} rows`)
  /* --- and the viewer edits, with no base revision beside it ------------------ */

  /* **The half that only exists because `editor.js` was lifted out of the diff.**
     Its load used to read `diffState` directly, so a search result — a file with
     no changeset and usually no base — could not have opened it at all. Asserted
     by writing through it and reading the disk, because a buffer that looks saved
     and is not is the failure worth catching. */
  /* Clicked through the element rather than the pointer, and the reason is not
     this feature: `.updatebar` is a centred pill at `z-index:100`, so whenever the
     daemon has something to announce it floats over the middle of *any* overlay
     header — the diff's path sits under it too. The assertion here is about the
     buffer, not about hit-testing a button. */
  const press = (id) => page.$eval(id, (b) => b.click())
  await press('#fnedit')
  await page.waitForSelector('#fnsrc.editing .editarea', { timeout: 5000 })
  check(
    await page.$$eval('#fnsrc .editbase', (b) => b.length) === 0,
    'no base pane: a search result has no revision to sit beside',
  )
  await page.fill('#fnsrc .editarea', 'the frobnicate word moved\n')
  check(
    await page.$eval('#fnsave', (b) => b.textContent) === 'Save •',
    'typing marks the buffer dirty',
  )
  await press('#fnsave')
  await page.waitForFunction(
    () => document.getElementById('fnsave')?.textContent?.startsWith('Save') === true
      && !document.getElementById('fnsave')?.textContent?.includes('•'),
    null, { timeout: 5000 },
  )
  check(
    fs.readFileSync(path.join(tree, 'haystack.txt'), 'utf8') === 'the frobnicate word moved\n',
    'the write reached the workspace the read came from',
  )

  // Cancel goes back to the viewer, on the file as it now is rather than the copy
  // the band was built from.
  await press('#fnedit')
  await page.waitForFunction(
    () => !!document.querySelector('#fnsrc .fnrow'), null, { timeout: 5000 })
  check(
    await page.$eval('#fnsrc .fnrow', (r) => r.textContent) === 'the frobnicate word moved',
    'and the viewer comes back on the saved file, not the stale one',
  )
  await page.keyboard.press('Escape')

  /* --- modifier-click goes to a definition ------------------------------------ */

  /* **The branch is what is asserted, not the jump.** One hit moves the viewer;
     anything else has to stay a list, because a heuristic that guesses silently is
     worse than no jump at all. Both halves are driven here: a name defined once,
     and a name defined twice.

     Clicked for real, at the pointer, because the word is read off the caret API
     rather than off the clicked element — a token span is `run_blocking` sometimes
     and `::` just as often, and a test that passed the word in would prove none of
     that. */
  fs.writeFileSync(
    path.join(tree, 'lib.rs'),
    'fn caller() {\n    the_target(1);\n}\nfn the_target(n: u32) {}\nfn twice() {}\n',
  )
  fs.writeFileSync(path.join(tree, 'more.rs'), 'fn twice() {}\n')

  await press('#fnclose')
  await page.keyboard.press(chord('Shift+KeyF'))
  await page.waitForSelector('#fnoverlay.on', { timeout: 5000 })
  await page.fill('#fnq', 'the_target')
  await page.waitForFunction(
    () => document.querySelector('#fnhits .fnhit .fnat')?.textContent === 'lib.rs:2',
    null, { timeout: 5000 })
  // The index answers before the viewer paints, and the click is at the pointer —
  // so wait for the line to actually be on screen.
  await page.waitForFunction(
    () => document.querySelector('#fnsrc .fnrow.on')?.textContent?.includes('the_target') === true,
    null, { timeout: 5000 })

  // The call site is on screen; ⌘/Ctrl-click the name in it.
  const modClick = async (selector, word) => {
    const box = await page.evaluate(([sel, w]) => {
      const row = document.querySelector(sel)
      if (!row) return null
      const body = row.querySelector('s')
      const text = body?.textContent ?? ''
      const at = text.indexOf(w)
      if (at < 0) return null
      // The middle of the word, found by measuring the character range itself —
      // a column guess would land differently at another font size.
      const walk = document.createTreeWalker(body, NodeFilter.SHOW_TEXT)
      let seen = 0
      for (let t = walk.nextNode(); t; t = walk.nextNode()) {
        const len = (t.textContent ?? '').length
        if (seen + len > at) {
          const r = document.createRange()
          r.setStart(t, at - seen)
          r.setEnd(t, Math.min(len, at - seen + w.length))
          const b = r.getBoundingClientRect()
          return { x: b.x + b.width / 2, y: b.y + b.height / 2 }
        }
        seen += len
      }
      return null
    }, [selector, word])
    if (!box) return false
    /* The modifier is held around the click rather than passed to it: this
       playwright drops a `modifiers` list it does not know and the click then
       arrives bare — which looks exactly like a broken binding. Measured, not
       guessed: a probe listener saw `ctrlKey: false`. */
    await page.keyboard.down(MOD_KEY)
    await page.mouse.click(box.x, box.y)
    await page.keyboard.up(MOD_KEY)
    return true
  }

  check(await modClick('#fnsrc .fnrow.on', 'the_target'), 'the call site is on screen to click')
  await page.waitForFunction(
    () => document.getElementById('fnfoot')?.textContent?.startsWith('one definition') === true,
    null, { timeout: 5000 })
  check(
    await page.$eval('#fnhits .fnhit .fnat', (r) => r.textContent) === 'lib.rs:4',
    'one definition is a jump, and it lands on the definition, not the call',
  )
  check(
    await page.$eval('#fnq', (q) => q.value) === 'the_target',
    'and the query box carries the symbol, so the search is reproducible by hand',
  )

  /* --- and the mouse's back button undoes the jump ---------------------------- */

  /* **Dispatched, not pressed**: this playwright's mouse has left, right and
     middle only. So what is asserted is the handler and the trail — the answers
     that were on screen come back, rather than being searched for again. What it
     leaves unproven is the delivery, whether a webview hands button 3 to the page
     at all, and nothing available here can answer that. */
  await page.$eval('#fnoverlay', (o) => o.dispatchEvent(
    new MouseEvent('mousedown', { button: 3, bubbles: true, cancelable: true })))
  await page.waitForFunction(
    () => document.querySelectorAll('#fnhits .fnhit').length === 2,
    null, { timeout: 5000 })
  const returned = await page.$eval('#fnhits .fnhit .fnat', (r) => r.textContent)
  check(
    returned === 'lib.rs:2',
    `the back button returns to the search the jump was made from, got ${returned}`,
  )

  // Two definitions of one name: a list, never a guess.
  await page.fill('#fnq', 'twice')
  // On the row, not merely on a row: the previous jump left one selected, and a
  // wait that only asks whether `.on` exists is answered by the stale one.
  await page.waitForFunction(
    () => document.querySelector('#fnsrc .fnrow.on')?.textContent?.includes('twice') === true,
    null, { timeout: 5000 })
  check(await modClick('#fnsrc .fnrow.on', 'twice'), 'a name defined twice is on screen')
  await page.waitForFunction(
    () => document.getElementById('fnfoot')?.textContent?.includes('definitions of twice') === true,
    null, { timeout: 5000 })
  check(
    await page.$$eval('#fnhits .fnhit', (rs) => rs.length) === 2,
    'two definitions stay a list — the jump is refused',
  )
  await page.keyboard.press('Escape')


  /* --- and the diff's editor is unchanged by the lift ------------------------- */

  /* **The regression this step could quietly cause.** `editor.js` was the diff's
     right-hand pane; if the move broke it, nothing above would have said so —
     every assertion here is about the search viewer, which never had one. The
     difference between the two is the base pane, so that is what is asserted:
     the diff opens one, the search viewer does not. */
  // A tracked file, changed: the diff is a changeset, so it needs one to draw.
  fs.appendFileSync(path.join(tree, 'README.md'), 'a line the diff can show\n')
  await page.keyboard.press(chord('Shift+KeyD'))
  await page.waitForSelector('#overlay.on', { timeout: 5000 })
  await page.waitForFunction(
    () => !!document.querySelector('#diffbody .ln'), null, { timeout: 10_000 })
  await press('#ovedit')
  await page.waitForSelector('#diffbody.editing .editarea', { timeout: 5000 })
  check(
    await page.$$eval('#diffbody .editbase', (b) => b.length) === 1,
    "the diff's editor still shows the base revision beside the buffer",
  )
  await press('#ovedit')
  await page.waitForFunction(
    () => !document.getElementById('diffbody')?.classList.contains('editing'),
    null, { timeout: 5000 })
  check(
    await page.$eval('#ovsave', (b) => b.hidden) === true,
    'and cancelling puts the Save button away',
  )

  /* **And a modifier-click works in the diff, which is the return on one
     renderer.** `source.js` reads the word off the caret and neither viewer knows
     anything about the other; the diff draws `.ln` rows and the search viewer
     draws `.fnrow`, and the same handler reaches both. Asserted on the diff's own
     rows so a change that split the renderers again fails here. */
  check(await modClick('#diffbody .ln.add', 'diff'), 'a diff row is on screen to click')
  await page.waitForSelector('#fnoverlay.on', { timeout: 5000 })
  check(
    await page.$eval('#fnq', (q) => q.value) === 'diff',
    'a modifier-click in the diff asks about the word under it',
  )
  await page.keyboard.press('Escape')
  await page.keyboard.press('Escape')

  /* And the content half, on its own chord. `Control+Shift+F` rather than a plain
     `Control+F`, which is readline's forward-char and the pty's to keep. */
  await page.keyboard.press(chord('Shift+KeyF'))
  await page.waitForTimeout(150)
  check(await findUp() === true, 'Ctrl Shift F opens the search in contents mode')
  check(
    await page.$eval('#fnmode', (m) => m.textContent) === 'find contents',
    'and it says which mode it is in',
  )
  await page.keyboard.press('Escape')

  /* --- a path the agent printed opens the same viewer ------------------------- */

  /* **Last, and after a reload, because this one needs the renderer the app
     ships.** A browser tab gets xterm's WebGL renderer (`webglWanted` is
     `CHROME === 'none'`), and a canvas has no text to measure or click — the
     terminal is invisible to everything this file does. The app's window is not a
     tab, so it draws into the DOM; telling the page it has the app's chrome is
     what turns that on, and it is the same amend `--mac` already makes for the
     same kind of reason. Everything above runs before it, unchanged.

     A shell rather than the agent pane, for the one property a test needs: its
     output is whatever this types into it. */
  /* **The mac amend has to survive this**, or the reload puts the page back on
     Linux while the rest of the run is pressing ⌘. Passed in rather than closed
     over: an init script is serialised into the page and cannot see `asMac`. */
  await page.addInitScript((mac) => {
    let held
    Object.defineProperty(window, '__ORCH__', {
      configurable: true,
      get: () => held,
      set: (v) => { held = { ...v, chrome: 'custom', ...(mac ? { platform: 'mac' } : {}) } },
    })
  }, asMac)
  await page.reload({ waitUntil: 'domcontentloaded' })
  await page.waitForFunction(() => document.body.classList.contains('ready'), null, { timeout: 15_000 })

  /* Selected by hand after the reload: the drawer lists the processes of the
     session you are looking at, and the page restores whichever selection it had
     remembered rather than the one this test cares about. */
  await page.click(`#rail .sess[data-id="${session}"]`)
  fs.writeFileSync(path.join(tree, 'deep.txt'), 'one\ntwo is the line\nthree\n')
  // The session's own workspace, not `main`: the drawer lists the processes of
  // the workspace you are looking at, and this test has been in a worktree since
  // its first line.
  const ws = (await t.state()).sessions.find((/** @type {any} */ x) => x.id === session).workspace
  const { process: shell } = await t.api('POST', `/api/workspace/${ws}/shell`)
  await page.waitForFunction(
    () => !!document.querySelector('#drawerbody .termhost:not([hidden]) .xterm-rows'),
    null, { timeout: 15_000 })
  /* **Focused through the textarea, not by clicking the pane.** A click is now how
     a path is opened, and the shell's prompt is a path — so clicking the middle of
     the terminal to give it the keyboard opens the prompt's directory and leaves a
     refusal toast floating over the rows this then wants to click. */
  const focusTerm = () => page.$eval(
    '#drawerbody .termhost:not([hidden]) .xterm-helper-textarea',
    (t) => /** @type {HTMLTextAreaElement} */ (t).focus())
  await focusTerm()
  /* The prompt, not a sleep: the pane exists as soon as the socket is up, and
     typing into a shell that has not started reading yet loses the line. */
  await page.waitForFunction(() => {
    const rows = document.querySelector('#drawerbody .termhost:not([hidden]) .xterm-rows')
    return !!rows && [...rows.children].some((r) => (r.textContent ?? '').trim().length > 0)
  }, null, { timeout: 15_000 })
  /* The printed path differs from the typed one (`de%s.txt` becomes `deep.txt`),
     so the row this finds is the shell's output and never the command line echoed
     above it. */
  await page.keyboard.type("printf 'at de%s.txt:2 here\\n' ep")
  await page.keyboard.press('Enter')
  /** Where a substring of a terminal row is on screen, measured off the text node
   *  rather than guessed from a column: the cell width moves with the font-size
   *  setting, which is a slider in this app.
   *
   *  **It waits for the row to stop moving, which a fixed delay cannot do.** The
   *  shell prints its next prompt a moment after the line, every row shifts up
   *  one, and a click at coordinates taken before that lands on the prompt —
   *  which is itself a path and opens the wrong file. Seen exactly that way, as a
   *  refusal naming the prompt. So the row is measured twice and only trusted
   *  when it has not moved between. */
  const pointAt = async (needle, from, to) => {
    const where = async () => page.evaluate((want) => {
      const rows = document.querySelector('#drawerbody .termhost:not([hidden]) .xterm-rows')
      const row = [...(rows?.children ?? [])].find((r) => (r.textContent ?? '').includes(want))
      return row ? Math.round(row.getBoundingClientRect().y) : null
    }, needle)
    for (let tries = 0; tries < 40; tries++) {
      const first = await where()
      await page.waitForTimeout(250)
      if (first !== null && first === await where()) break
    }
    return page.evaluate(([want, a, b]) => {
      const rows = document.querySelector('#drawerbody .termhost:not([hidden]) .xterm-rows')
      for (const row of rows?.children ?? []) {
        const at = (row.textContent ?? '').indexOf(want)
        if (at < 0) continue
        const walk = document.createTreeWalker(row, NodeFilter.SHOW_TEXT)
        let seen = 0
        for (let t = walk.nextNode(); t; t = walk.nextNode()) {
          const len = (t.textContent ?? '').length
          if (seen + len > at + a) {
            const r = document.createRange()
            r.setStart(t, at + a - seen)
            r.setEnd(t, Math.min(len, at + b - seen))
            const box = r.getBoundingClientRect()
            return { x: box.x + box.width / 2, y: box.y + box.height / 2 }
          }
          seen += len
        }
      }
      return null
    }, [needle, from, to])
  }

  /* Two points on the one line: the path, and a word beside it that is not one.
     Measured off the text nodes rather than guessed from a column, because the
     cell width moves with the font-size setting. */
  const cells = {
    path: await pointAt('at deep.txt:2 here', 3, 11),
    word: await pointAt('at deep.txt:2 here', 14, 18),
  }

  /* **The refusal first, because it is the half with the power.** Everything on
     this line is clickable text and only part of it is a path; a provider that
     offered the lot would pass the assertion below while turning every word in
     the scrollback into a link and stealing the click from the program. */
  await page.mouse.click(cells.word.x, cells.word.y)
  await page.waitForTimeout(400)
  check(
    await page.$$eval('#fvoverlay.on', (o) => o.length) === 0,
    'a click on an ordinary word does nothing — it is not a path',
  )
  /* The pointer leaves and the wait is past the double-click interval, or the
     click below lands as the second half of a double-click — which selects a word
     in a terminal and never reaches a link. */
  await page.mouse.move(cells.path.x, cells.path.y - 60)
  await page.waitForTimeout(600)
  await page.mouse.move(cells.path.x, cells.path.y)
  await page.waitForTimeout(150)
  await page.mouse.click(cells.path.x, cells.path.y)

  const opened = await page.waitForFunction(
    () => document.getElementById('fvpath')?.textContent === 'deep.txt',
    null, { timeout: 5000 }).then(() => true).catch(() => false)
  check(opened, 'a plain click on a printed path opens it in the viewer')
  check(
    await page.$eval('#fvsrc .fnrow.on', (r) => r.textContent) === 'two is the line',
    'at the line the path named, not the top of the file',
  )
  /* **The file pane, not the search overlay.** Opening one file used to take the
     finder over, which threw away whatever search was in it. */
  check(
    await page.$$eval('#fnoverlay.on', (o) => o.length) === 0,
    'and the finder is left alone — this is a pane of its own',
  )

  /* --- a bare name, a range, and two files with one name ---------------------- */

  /* **An agent prints a file name, not a path**, and joining that onto the pty's
     own directory names a file that is not there — which is what the viewer said,
     correctly and uselessly. The workspace's own file list is the answer, so this
     writes the file where nothing would find it by guessing and prints only its
     name. The range is the other half: `:2-4` is what Claude Code writes when it
     means a block, and marking one line of it would answer a question nobody
     asked. */
  fs.mkdirSync(path.join(tree, 'far/down/below'), { recursive: true })
  fs.writeFileSync(path.join(tree, 'far/down/below/Lonely.txt'), 'a\nb\nc\nd\ne\n')
  await page.keyboard.press('Escape')
  await focusTerm()
  await page.keyboard.type("printf 'see Lonel%s.txt:2-4 now\\n' y")
  await page.keyboard.press('Enter')
  const lonely = await pointAt('see Lonely.txt:2-4 now', 4, 14)
  await page.mouse.move(lonely.x, lonely.y)
  await page.waitForTimeout(150)
  await page.mouse.click(lonely.x, lonely.y)
  const foundDeep = await page.waitForFunction(
    () => document.getElementById('fvpath')?.textContent === 'far/down/below/Lonely.txt',
    null, { timeout: 5000 }).then(() => true).catch(() => false)
  check(foundDeep, 'a bare file name is looked up in the workspace, not joined to the cwd')
  check(
    await page.$$eval('#fvsrc .fnrow.on', (rs) => rs.length) === 3,
    'and a line range lights every row in it',
  )

  /* Two files with one name is the normal shape of a large repo, so it is a list
     to pick from rather than a guess or a refusal. */
  fs.mkdirSync(path.join(tree, 'other/place'), { recursive: true })
  fs.writeFileSync(path.join(tree, 'other/place/Lonely.txt'), 'x\ny\n')
  await page.keyboard.press('Escape')
  await focusTerm()
  await page.keyboard.type("printf 'and Lonel%s.txt again\\n' y")
  await page.keyboard.press('Enter')
  const twice = await pointAt('and Lonely.txt again', 4, 14)
  await page.mouse.move(twice.x, twice.y)
  await page.waitForTimeout(150)
  await page.mouse.click(twice.x, twice.y)
  const picked = await page.waitForFunction(
    () => [...document.querySelectorAll('#ctxmenu:not([hidden]) .ctxmenu-item')]
      .filter((b) => (b.textContent ?? '').endsWith('Lonely.txt')).length === 2,
    null, { timeout: 5000 }).then(() => true).catch(() => false)
  check(picked, 'two files with one name are a choice, not a guess')
  // And picking one opens it.
  await page.$$eval('#ctxmenu .ctxmenu-item', (bs) => bs[0].click())
  const chosen = await page.waitForFunction(
    () => document.getElementById('fvpath')?.textContent?.endsWith('Lonely.txt') === true,
    null, { timeout: 5000 }).then(() => true).catch(() => false)
  check(chosen, 'and the one you pick is the one that opens')

  /* --- and the right-click menu on the same path ------------------------------ */

  /* **Asserted as far as the menu and no further.** The two new items hand a path
     to `xdg-open`, and a gate that actually pressed them would open a file manager
     on whatever machine is running the suite. What is worth holding is that the
     menu appears for a path, carries the three answers, and does *not* appear for
     ordinary text — which is the same refusal the click has, through a different
     event. */
  await page.keyboard.press('Escape')
  const onPath = await pointAt('and Lonely.txt again', 4, 14)
  await page.mouse.click(onPath.x, onPath.y, { button: 'right' })
  await page.waitForSelector('#ctxmenu:not([hidden])', { timeout: 5000 })
  const items = await page.$$eval('#ctxmenu .ctxmenu-item', (bs) => bs.map((b) => b.textContent))
  check(
    items.length === 3 && items[1] === 'Open the folder' && /Finder|file manager/.test(items[2] ?? ''),
    `the menu offers the three answers, got ${JSON.stringify(items)}`,
  )
  await page.keyboard.press('Escape')
  const onWord = await pointAt('and Lonely.txt again', 0, 3)
  await page.mouse.click(onWord.x, onWord.y, { button: 'right' })
  await page.waitForTimeout(400)
  /* The pane has a menu of its own — "send the last lines to the session" — so
     what is asserted is that ordinary output gets *that* one rather than these
     items, not that a right-click does nothing. */
  const plain = await page.$$eval('#ctxmenu .ctxmenu-item', (bs) => bs.map((b) => b.textContent))
  check(
    !plain.some((t) => t === 'Open the folder'),
    `ordinary output gets the pane's own menu, got ${JSON.stringify(plain)}`,
  )

  /* --- and a markdown file opens rendered ------------------------------------ */

  /* **The one thing the parser's own test cannot see**: that the tree reaches the
     page as nodes. Asserted on the elements rather than on text, because the
     whole point of the mode is the shape — a heading is an `h2`, a fence is a
     `pre`, and a link is an anchor with a safe href on it. */
  fs.writeFileSync(
    path.join(tree, 'note.md'),
    '# Title\n\nsome **bold** prose with `code` in it.\n\n'
    + '- one\n- two\n\n```js\nconst x = 1\n```\n\n[home](https://example.com)\n\n'
    /* **Long on purpose.** The band is only rebuilt once the viewport passes its
       margins, so a short note scrolls without ever reaching the code that used
       to replace the rendered page with rows. The file this was reported on is a
       long one. */
    /* A quote of two blocks and a nested list: both were painter bugs that the
       parser's own test cannot see — the quote dropped every other child to a
       live-collection walk, and every indented item rendered flat. */
    + '> first quoted paragraph\n>\n> second quoted paragraph\n\n'
    + '- top\n  - nested\n    - deeper\n\n'
    + '## More\n\nprose that goes on.\n\n'.repeat(60),
  )
  await page.keyboard.press('Escape')
  await focusTerm()
  await page.keyboard.type("printf 'read not%s.md now\\n' e")
  await page.keyboard.press('Enter')
  const mdAt = await pointAt('read note.md now', 5, 12)
  await page.mouse.move(mdAt.x, mdAt.y)
  await page.waitForTimeout(150)
  await page.mouse.click(mdAt.x, mdAt.y)
  const rendered = await page.waitForFunction(
    () => !!document.querySelector('#fvsrc .md .md-h1'), null, { timeout: 5000 })
    .then(() => true).catch(() => false)
  check(rendered, 'a markdown file opens rendered, not as lines')
  const shape = await page.$eval('#fvsrc .md', (m) => ({
    h1: m.querySelector('.md-h1')?.textContent,
    bold: !!m.querySelector('strong'),
    tick: m.querySelector('.md-tick')?.textContent,
    items: m.querySelectorAll('.md-li').length,
    code: m.querySelector('.md-code')?.textContent,
    href: m.querySelector('.md-link')?.getAttribute('href'),
    rows: m.querySelectorAll('.fnrow').length,
  }))
  check(
    shape.h1 === 'Title' && shape.bold && shape.tick === 'code' && shape.items >= 2
      && shape.code === 'const x = 1' && shape.href === 'https://example.com/' && shape.rows === 0,
    `the blocks arrive as elements, got ${JSON.stringify(shape)}`,
  )
  /* The two the painter got wrong, asserted on the shape rather than the text:
     a quote keeps both of its paragraphs, and an indented item is a list inside
     a list rather than a sibling. */
  const nesting = await page.$eval('#fvsrc .md', (m) => ({
    quoted: m.querySelectorAll('.md-quote .md-p').length,
    deep: m.querySelectorAll('.md-list .md-list .md-list .md-li').length,
  }))
  check(
    nesting.quoted === 2 && nesting.deep === 1,
    `a quote keeps every block and a list nests, got ${JSON.stringify(nesting)}`,
  )
  /* **Scrolling must not change the mode**, which is what it did: the band is
     rebuilt as the pane scrolls, and a rebuild over the markdown page replaced it
     with rows. Reported as "scrolling toggled it back to source", and it is the
     one assertion here that fails on the code as it was. */
  await page.$eval('#fvsrc', (m) => { m.scrollTop = m.scrollHeight })
  await page.waitForTimeout(300)
  check(
    await page.$$eval('#fvsrc .md', (m) => m.length) === 1
      && await page.$$eval('#fvsrc .fnrow', (r) => r.length) === 0,
    'and scrolling it leaves it rendered',
  )

  // And the toggle puts the source back, at which point it is lines again.
  await page.$eval('#fvmode', (b) => b.click())
  check(
    await page.$$eval('#fvsrc .fnrow', (rs) => rs.length) > 0
      && await page.$$eval('#fvsrc .md', (m) => m.length) === 0,
    'and the toggle puts the file back as it is written',
  )
  await page.keyboard.press('Escape')

  /* --- and an html file runs in a sandbox ------------------------------------ */

  /* **What only a browser can say**: that the page's own scripts run, that its
     relative CSS and module script load through the preview route, and that the
     same scripts can reach neither this page's token nor the daemon's API. The
     route's rules are `preview.rs`'s test and flow 36; this is the frame. The
     script writes what it got into its own title, which is the one thing a
     cross-origin frame hands back without being asked to. */
  fs.writeFileSync(path.join(tree, 'demo.html'),
    '<link rel="stylesheet" href="demo.css"><p id="p">hi</p><script type="module" src="demo.js"></script>')
  fs.writeFileSync(path.join(tree, 'demo.css'), '#p { color: rgb(255, 0, 0) }')
  fs.writeFileSync(path.join(tree, 'demo.js'), [
    'const out = { ran: true }',
    "try { out.token = typeof parent.__ORCH__ } catch { out.token = 'blocked' }",
    "try { out.api = (await fetch(location.origin + '/api/state')).status } catch { out.api = 'blocked' }",
    "out.css = getComputedStyle(document.getElementById('p')).color",
    'document.title = JSON.stringify(out)',
  ].join('\n'))
  await focusTerm()
  await page.keyboard.type("printf 'see dem%s.html now\\n' o")
  await page.keyboard.press('Enter')
  const htmlAt = await pointAt('see demo.html now', 4, 13)
  await page.mouse.move(htmlAt.x, htmlAt.y)
  await page.waitForTimeout(150)
  await page.mouse.click(htmlAt.x, htmlAt.y)
  await page.waitForFunction(
    () => document.getElementById('fvpath')?.textContent === 'demo.html', null, { timeout: 5000 })
  /* The mode is remembered across files and the step above left it on source, so
     the button is what says which mode this is: it offers the one not showing. */
  check(
    await page.$eval('#fvmode', (b) => b.textContent) === 'Preview',
    'the toggle names the preview for an html file',
  )
  await page.$eval('#fvmode', (b) => b.click())
  const framed = await page.waitForSelector('#fvsrc iframe.preview', { timeout: 5000 })
    .then(() => true).catch(() => false)
  check(framed, 'an html file opens as a preview, not as lines')
  check(
    await page.$eval('#fvsrc iframe.preview', (f) => f.getAttribute('sandbox')).catch(() => null)
      === 'allow-scripts',
    'and the frame is sandboxed with scripts and nothing else',
  )
  /** @type {any} */
  let ran = null
  for (let i = 0; i < 50 && !ran; i++) {
    const frame = page.frames().find((f) => f.url().includes('/preview/'))
    const title = frame ? await frame.evaluate(() => document.title).catch(() => '') : ''
    try { ran = JSON.parse(title) } catch { await page.waitForTimeout(100) }
  }
  check(ran?.ran === true, `the page's module script runs, got ${JSON.stringify(ran)}`)
  check(ran?.css === 'rgb(255, 0, 0)', `its relative stylesheet loads, got ${ran?.css}`)
  check(ran?.token === 'blocked', `its script cannot reach this page's token, got ${ran?.token}`)
  check(ran != null && ran.api !== 200, `nor read the daemon's API, got ${ran?.api}`)
  await page.keyboard.press('Escape')

  /* --- and the finder hands a file to that pane ------------------------------ */

  /* **The index is for finding and the file pane is for reading.** Below an index
     the file gets two thirds of the height and no markdown mode, so the finder
     needs a way out to the pane that has both — on `Enter`, which the overlay
     contract gives to whatever is open, and on a button for the people who do not
     know that. */
  await page.keyboard.press(chord('Shift+KeyF'))
  await page.waitForSelector('#fnoverlay.on', { timeout: 5000 })
  await page.fill('#fnq', 'frobnicate')
  await page.waitForFunction(
    () => !!document.querySelector('#fnhits .fnhit .fnsnip'), null, { timeout: 5000 })
  await page.$eval('#fnopen', (b) => b.click())
  const handed = await page.waitForFunction(
    () => document.getElementById('fvpath')?.textContent === 'haystack.txt',
    null, { timeout: 5000 }).then(() => true).catch(() => false)
  check(handed, 'the finder hands its file to the file pane')
  /* The line this file now holds: the editor section above wrote through it, and
     asserting the original text here would be asserting a stale read. */
  const onLine = await page.$eval('#fvsrc .fnrow.on', (r) => r.textContent)
  check(onLine?.includes('frobnicate') === true, `at the line the index was on, got ${onLine}`)
  /* And closing it leaves the search where it was, which is the whole reason
     these are two overlays. */
  await page.keyboard.press('Escape')
  check(
    await page.$$eval('#fnoverlay.on', (o) => o.length) === 1
      && await page.$eval('#fnq', (q) => q.value) === 'frobnicate',
    'and closing it puts you back in the search you had',
  )
  await page.keyboard.press('Escape')

  await t.api('POST', `/api/process/${shell}/close`)

  /* --- a restart puts the pane back on the new process (#35) ----------------- */

  /* **The one half the restart flows cannot see**, because they drive the API: a
     respawn keeps the id, the old pty's socket closes as `exited`, and the pane
     used to stop there — on the old scrollback, with the new `claude --resume`
     running and nothing showing it. The stand-in agent prints nothing, so the
     proof is the daemon's own line for each socket it attaches, and a pane that
     did not come back is one that never asks again. */
  const { session: again } = await t.api('POST', '/api/worktree', { name: 'restart-me' })
  await t.settled(again)
  await page.click(`#rail .sess[data-id="${again}"]`)
  const attaches = () => t.log().split('\n')
    .filter((l) => l.includes('pty client attached') && l.includes(`session:${again}`)).length
  await until('the pane to attach', async () => attaches() >= 1)
  const attachedBefore = attaches()
  await t.api('POST', `/api/session/${again}/restart`)
  const reattached = await until('the pane to attach to the new process',
    async () => attaches() > attachedBefore).then(() => true).catch(() => false)
  check(reattached, "a restarted session's pane attaches to the new process")
  const says = await page.$eval('#termwrap .termhost:not([hidden]) .term-badge',
    (b) => (b.hidden ? null : b.textContent)).catch(() => 'no pane')
  check(says !== 'exited', `and it no longer says it has exited, got ${says}`)

  console.log(`\npage-check: ${failed ? 'FAILED' : 'ok'}`)
} finally {
  await browser?.close()
  await t.stop()
  if (failed || keep) console.log(`  sandbox: ${t.root}`)
}

process.exit(failed ? 1 : 0)
