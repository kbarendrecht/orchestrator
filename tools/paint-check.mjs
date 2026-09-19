// Which pane rebuilds, how often, and what moved — against a *running* daemon.
//
//   mise run paint-check                 # sample for 60s
//   mise run paint-check -- --for 180    # sample for three minutes
//   mise run paint-check -- --port 7788  # another checkout's daemon
//
// Run it with agents working. Idle, every number here is zero and that is the
// whole point: a pane that rebuilds while nothing it draws has changed is the
// thing being hunted, and it only happens under load.
//
// **What this measures is snapshot churn, not paint cost.** It counts
// `core.unchanged` returning false and names where the signature moved. That
// comparison is the same in any engine, so driving Chrome here says something
// true about the app even though the app is WebKitGTK — see `docs/traps/spa.md`.
// What it does *not* measure is what the rebuild then costs to lay out, which is
// the engine's own and would need the real window.
//
// The daemon serves the page over plain loopback HTTP, so a normal browser can
// attach to the app you already have open. It reads and clicks nothing: the page
// is one more subscriber on the events socket, which is what `ws.rs` is built
// for. Chrome via playwright-core's `channel`, so nothing is downloaded.
import { chromium } from 'playwright-core';
import { execSync } from 'node:child_process';

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const i = args.indexOf(name);
  return i === -1 ? fallback : args[i + 1];
};

const port = flag('--port', process.env.ORCHD_PORT || '7777');
const seconds = Number(flag('--for', '60'));
const base = `http://127.0.0.1:${port}`;

// The token is embedded in the served page; read it back rather than making the
// caller paste it. Same trick as `shot.mjs`.
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

const browser = await chromium.launch({ channel: 'chrome', args: ['--no-sandbox'] });
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
page.on('pageerror', (e) => console.error('page error:', e.message));

// `observe`, for the reason `shot.mjs` gives: without it this browser's size
// would refit the session's pty and leave the real window painting at 1440x900.
await page.goto(`${base}/?token=${token}&observe=1`, { waitUntil: 'domcontentloaded' });
await page.waitForFunction(() => document.querySelectorAll('.rail-scroll *').length > 0,
  null, { timeout: 20000 }).catch(() => console.error('warning: rail never rendered'));

// Zero the counters *after* the first paint, so the rebuild every pane owes the
// page it just loaded is not counted as churn.
await page.evaluate(() => window.orchPaint(true));
process.stderr.write(`sampling ${seconds}s on ${base} — work the agents now\n`);
await page.waitForTimeout(seconds * 1000);

const stats = await page.evaluate(() => window.orchPaint());
await browser.close();

const rate = (n) => (n / seconds).toFixed(2);
const width = Math.max(4, ...stats.map((s) => s.name.length));
console.log(`\npane${' '.repeat(width - 4)}  rebuilds   per second  last moved at`);
for (const s of stats) {
  const paths = s.paths.length ? s.paths.join(', ') : '—';
  console.log(`${s.name.padEnd(width)}  ${String(s.rebuilds).padStart(8)}   ${rate(s.rebuilds).padStart(10)}  ${paths}`);
}
console.log(`
"last moved at" is where the signature differed on that pane's most recent
rebuild. Array indices collapse to [], so the leading [][] is the signature's own
list of inputs and its Nth element — what to read is the leaf. A leaf the pane
does not draw is a candidate for its drop list: see NOT_DRAWN in web/js/rail.js
and the paintSig comment in web/js/core.js.

"key order only" means the snapshot said the same thing twice and the pane was
torn down anyway, because a HashSet or HashMap came back in a different order.
That one is the daemon's to fix, not the pane's.
`);
