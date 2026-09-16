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

  console.log(`\npage-check: ${failed ? 'FAILED' : 'ok'}`)
} finally {
  await browser?.close()
  await t.stop()
  if (failed || keep) console.log(`  sandbox: ${t.root}`)
}

process.exit(failed ? 1 : 0)
