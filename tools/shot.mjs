// Screenshot the running SPA, headlessly, from a terminal.
//
// The UI is served over plain HTTP by the daemon, so a normal browser can drive
// it — no tauri-driver, no display. That makes it the one way to check a UI
// change when you cannot see the screen (working over SSH, or from a phone).
//
//   mise run shot                     # whole window
//   mise run shot -- '.drawer-head'   # just that element, tight crop
//   mise run shot -- '.drawer-head' --click '#rvhead'   # click first, then shoot
//   mise run shot -- '#keyhelp' --key '?'               # press first, then shoot
//
// `--key` exists because an overlay opened by a chord had no other way in: the
// legend has a close button and no open button, so `--click` could not reach it.
// Takes playwright key syntax, so 'Control+Shift+D' works as well as '?'.
//
// Chrome is used via playwright-core's `channel`, so nothing is downloaded.
import { chromium } from 'playwright-core';
import { mkdirSync } from 'node:fs';
import { execSync } from 'node:child_process';

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const i = args.indexOf(name);
  return i === -1 ? fallback : args[i + 1];
};

// The selector, if given, is always the first argument (`shot -- '.sel' --click …`).
const selector = args[0] && !args[0].startsWith('--') ? args[0] : null;
const port = flag('--port', process.env.ORCHD_PORT || '7777');
const outDir = flag('--out', 'target/shots');
const clickSel = flag('--click', null);
const pressKey = flag('--key', null);
const waitMs = Number(flag('--wait', '0'));
const base = `http://127.0.0.1:${port}`;

// The token is embedded in the served page; read it back rather than making the
// caller paste it.
let token;
try {
  const html = execSync(`curl -sS --max-time 5 ${base}/`, { encoding: 'utf8' });
  token = html.match(/token:\s*"([^"]+)"/)?.[1];
} catch {
  console.error(`no daemon on ${base} — start the app first`);
  process.exit(1);
}
if (!token) {
  console.error(`could not read the token from ${base}/ — is that really orchd?`);
  process.exit(1);
}

mkdirSync(outDir, { recursive: true });
const stamp = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
const name = (selector || 'window').replace(/[^a-z0-9]+/gi, '-').replace(/^-|-$/g, '');
const path = `${outDir}/${name}-${stamp}.png`;

const browser = await chromium.launch({ channel: 'chrome', args: ['--no-sandbox'] });
const page = await browser.newPage({
  viewport: { width: Number(flag('--width', '1440')), height: Number(flag('--height', '900')) },
  deviceScaleFactor: 2,
});
page.on('pageerror', (e) => console.error('page error:', e.message));

await page.goto(`${base}/?token=${token}`, { waitUntil: 'domcontentloaded' });

// The page renders on its first websocket snapshot. Poke the daemon so one is
// pushed now rather than waiting out a poll, then wait for the render.
await page.waitForTimeout(1200);
await page.evaluate(async () => {
  await fetch('/api/workspace/main/reconcile', {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-orch-token': window.__ORCH__.token },
    body: '{}',
  }).catch(() => {});
});
await page.waitForFunction(() => document.querySelectorAll('.rail-scroll *').length > 0,
  null, { timeout: 15000 }).catch(() => console.error('warning: rail never rendered'));

if (clickSel) {
  await page.click(clickSel, { timeout: 5000 });
  await page.waitForTimeout(250);
}
if (pressKey) {
  // On body, not a focused control: the app's handler is registered on `window`
  // with capture, and pressing into an input would be typing instead.
  await page.locator('body').press(pressKey);
  await page.waitForTimeout(250);
}
if (waitMs) await page.waitForTimeout(waitMs);

if (selector) {
  const el = page.locator(selector).first();
  await el.waitFor({ timeout: 10000 });
  await el.screenshot({ path });
} else {
  await page.screenshot({ path });
}
await browser.close();
console.log(path);
