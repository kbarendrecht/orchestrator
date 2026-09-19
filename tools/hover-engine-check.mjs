#!/usr/bin/env node
// Does a hovered row survive being rebuilt under a stationary pointer?
//
//   mise run hover-check
//
// **The question this answers.** `renderRail` rebuilds the whole rail with
// `replaceChildren` whenever any session's state moves, which destroys the row the
// pointer is on. Chrome re-resolves `:hover` onto the replacement immediately, so
// the only symptom there is `.sess`'s 120ms background fade restarting. The app is
// WebKitGTK, and if that engine waits for the next mousemove instead, the same
// rebuild rate reads as the highlight vanishing rather than flickering — a
// different fault with a different fix, and not one Chrome can be asked about.
//
// So this asks both engines the same question on the same page. playwright's
// `webkit` on Linux is the GTK port — the one the app actually ships in. That is
// the opposite of the caveat in `renderer.mjs`: there the GTK port is the *wrong*
// engine because the fault is WKWebView compositing a texture on an Apple GPU;
// here the engine under test and the engine in the app are the same port, and the
// behaviour asked about is CSS hover resolution rather than anything on a GPU.
//
// It is a standalone page, not the SPA: the SPA needs a daemon and sessions to
// rebuild anything, and what is being measured is the engine's rule, not orchd's.
// The page reproduces exactly what the rail does — a list replaced wholesale on a
// timer, rows carrying a `:hover` background with the same 120ms transition.
import { chromium, webkit } from 'playwright-core';

const HZ = Number(process.argv.includes('--hz')
  ? process.argv[process.argv.indexOf('--hz') + 1] : 7);
const SECONDS = Number(process.argv.includes('--for')
  ? process.argv[process.argv.indexOf('--for') + 1] : 8);

// `.sess` copied from web/app.css, down to the transition, because the transition
// is half of what is being measured.
const PAGE = `<!doctype html><meta charset=utf-8><style>
  body { margin: 0; background: #1B1B1B; }
  #rail { width: 260px; }
  .sess {
    width: 100%; display: block; padding: 9px 15px; text-align: left;
    background: transparent; color: #C9C9C9; border: 0;
    border-left: 2px solid transparent; transition: background .12s;
  }
  .sess:hover { background: #2E2E2E; }
</style><div id=rail></div><script>
  const rail = document.getElementById('rail');
  let builds = 0;
  function build() {
    builds += 1;
    const rows = [];
    for (let i = 0; i < 8; i += 1) {
      const b = document.createElement('button');
      b.className = 'sess';
      b.dataset.id = 'row-' + i;
      b.textContent = 'session ' + i + '  (' + builds + ')';
      rows.push(b);
    }
    rail.replaceChildren(...rows);
  }
  build();
  window.__start = (hz) => { window.__t = setInterval(build, 1000 / hz); };
  window.__probe = (ms) => new Promise((done) => {
    let held = 0, painted = 0, seen = 0;
    const poll = setInterval(() => {
      seen += 1;
      const on = document.querySelector('.sess:hover');
      if (!on) return;
      held += 1;
      // The highlight only counts once it has actually reached the hover colour.
      if (getComputedStyle(on).backgroundColor === 'rgb(46, 46, 46)') painted += 1;
    }, 25);
    setTimeout(() => { clearInterval(poll); done({ held, painted, seen, builds }); }, ms);
  });
</script>`;

// Chrome by `channel`, like `shot.mjs` and the e2e scripts: playwright drives a
// browser it does not install, and the chromium build is not on disk. WebKit is,
// because that one it cannot borrow from the system.
async function ask(name, type, opts = {}) {
  const browser = await type.launch(opts);
  const page = await browser.newPage({ viewport: { width: 400, height: 400 } });
  await page.setContent(PAGE);
  // Park the pointer in the middle of a row and never move it again. That is the
  // whole experiment: a mousemove would let any engine re-resolve :hover.
  const box = await page.locator('.sess').nth(3).boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.waitForTimeout(400);

  const still = await page.evaluate(() => !!document.querySelector('.sess:hover'));
  await page.evaluate((hz) => window.__start(hz), HZ);
  const m = await page.evaluate((ms) => window.__probe(ms), SECONDS * 1000);
  await browser.close();

  const pct = (n) => `${((n / m.seen) * 100).toFixed(0)}%`;
  console.log(`${name.padEnd(9)} hover before rebuilds: ${still ? 'yes' : 'no'}   `
    + `during: held ${pct(m.held)}  highlight painted ${pct(m.painted)}   `
    + `(${m.builds} rebuilds, ${m.seen} samples)`);
  return { held: m.held / m.seen, painted: m.painted / m.seen };
}

console.log(`rebuilding a hovered list at ${HZ}Hz for ${SECONDS}s, pointer parked\n`);
const c = await ask('chromium', chromium, { channel: 'chrome', args: ['--no-sandbox'] });
// **A webkit that will not launch has to say so, not answer.** playwright drives
// a browser it does not install (see CLAUDE.md on the one dependency `[tools]`
// cannot carry), so the build may be absent or missing system libraries — and a
// silent fall back to chromium's answer would be this script reporting that the
// engines agree when it never asked one of them.
let w;
try {
  /* **Headed, and that is not a detail.** `pw_run.sh` picks the bundle by flag:
     `--headless` runs the *WPE* port, headed runs the *GTK* one — and WebKitGTK is
     what the app ships in. Both are asked, because if they agree the cheap one can
     be the gate later, and if they disagree only the GTK answer is about orchd. */
  await ask('webkit/wpe', webkit, { headless: true });
  w = await ask('webkit/gtk', webkit, { headless: false });
} catch (e) {
  console.error(`\nwebkit could not run, so this proves nothing about the app:\n  ${String(e.message).split('\n')[0]}`);
  console.error('  npx --prefix tools playwright-core install webkit');
  process.exit(1);
}

console.log(`
"held" is how often :hover still resolved to some row; "painted" is how often the
highlight had actually reached its colour. An engine that keeps hover but loses
the paint is showing you a 120ms fade restarting — the fix is to stop replacing
the row. An engine that loses hover itself is showing you nothing at all until you
move the mouse, which is the worse of the two and the same fix.`);

if (w.held < c.held - 0.1) {
  console.log('\nwebkit loses hover where chromium does not: the app sees the worse fault.');
}
