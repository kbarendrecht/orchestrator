// Record the README video: the splash and captions rendered, the take recorded and
// exported by Argo, then the captions laid over the export.
//
//   mise run video -- --port <host page port>                 # target/video/orchd.mp4
//   mise run video -- --port <port> --codex                   # the new session is Codex
//   mise run video -- --port <port> --out somewhere.mp4
//   mise run video -- --port <port> --captions-only           # re-caption the last take
//
// **It drives real agents**: one new session takes a turn, and `Handle` starts a
// review run on the fixture PR. So it is a tool you point at a prepared host, not a
// gate, and `docs/demo.md` says what to prepare.
//
// **Captions are laid on after the export, not drawn by Argo.** Its overlays are a
// fixed grey box, and a caption that is a separate layer can be reworded without a
// new take: `--captions-only` re-runs this last step on the take already on disk.
import { chromium } from 'playwright';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, '../..');
const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const i = args.indexOf(name);
  return i === -1 ? fallback : args[i + 1];
};
const has = (name) => args.includes(name);
const die = (msg) => {
  console.error(msg);
  process.exit(1);
};

const port = flag('--port', null) ?? die('--port <the host page port> is required; docs/demo.md says how to read it');
const out = resolve(root, flag('--out', 'target/video/orchd.mp4'));
const scratch = join(here, '.argo');
mkdirSync(scratch, { recursive: true });
mkdirSync(dirname(out), { recursive: true });

/** Run a command to completion, output passed through, and stop on a failure. */
function run(cmd, argv, opts = {}) {
  const r = spawnSync(cmd, argv, { stdio: 'inherit', cwd: here, ...opts });
  if (r.status !== 0) die(`${cmd} ${argv.join(' ')} failed (${r.status ?? r.signal})`);
}

/* **ffmpeg 5.1 or newer**, because Argo's stitcher passes `-fps_mode`, and 4.4 —
   what Ubuntu 22.04 ships — refuses it with "Unrecognized option". Checked here
   rather than discovered at the end of a take that already spent its tokens. */
const ff = spawnSync('ffmpeg', ['-version'], { encoding: 'utf8' });
const major = Number(ff.stdout?.match(/ffmpeg version n?(\d+)/)?.[1] ?? 0);
if (major < 5) {
  die(`ffmpeg ${major || 'not found'} is too old for Argo; 5.1 or newer is needed. `
    + '`mise run video` carries one (conda:ffmpeg); run it through that.');
}

const base = `http://127.0.0.1:${port}`;
const page = await fetch(`${base}/`, { signal: AbortSignal.timeout(5000) }).then((r) => r.text())
  .catch(() => die(`no host on ${base}; start one with --host (docs/demo.md)`));
const token = page.match(/token:\s*"([^"]+)"/)?.[1] ?? die(`no token in ${base}/; is that a host?`);
const { checkouts } = await fetch(`${base}/api/host/checkouts`, { headers: { 'x-orch-token': token } })
  .then((r) => r.json());
if (!Array.isArray(checkouts) || checkouts.length < 2) {
  die(`the host on ${base} has ${checkouts?.length ?? 0} checkout(s); the video wants two (docs/demo.md)`);
}

/* **The splash, from the desktop app's own source.** It is a `data:` page the app
   builds in `splash_html()`, and the board's landing starts from its last frame,
   so a copy kept here would drift from the real one the first time either moved.
   The template is lifted out of the Rust and filled the way `format!` would. */
function splash() {
  const src = readFileSync(join(root, 'desktop/src/main.rs'), 'utf8');
  const body = src.slice(src.indexOf('fn splash_html()'));
  const open = body.indexOf('r##"');
  const close = body.indexOf('"##');
  if (open < 0 || close < open) die('could not find splash_html() in desktop/src/main.rs; did it move?');
  const wordmark = readFileSync(join(root, 'web/wordmark.svg'), 'utf8').trim();
  const letters = Array.from({ length: wordmark.split('<path').length - 1 }, (_, i) =>
    `.wordmark path:nth-child(${i + 1}){animation-delay:${760 + 32 * i}ms}`).join('');
  const version = readFileSync(join(root, 'Cargo.toml'), 'utf8').match(/^version\s*=\s*"([^"]+)"/m)?.[1] ?? '';
  return body.slice(open + 4, close)
    .replace('{letters}', '\0L').replace('{version}', '\0V').replace('{wordmark}', '\0W')
    .replaceAll('{{', '{').replaceAll('}}', '}')
    .replace('\0L', letters).replace('\0V', version).replace('\0W', wordmark);
}

/** Each caption as a transparent PNG, drawn in the app's own look: its mono face,
 *  the amber `>>` of the mark, a dark panel. */
async function renderCaptions(captions) {
  const b = await chromium.launch();
  const p = await b.newPage({ viewport: { width: 1440, height: 200 } });
  for (const [scene, { text }] of Object.entries(captions)) {
    const safe = text.replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' })[c]);
    await p.setContent(`<!doctype html><html><head>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@500;600&display=swap">
<style>html,body{margin:0;background:transparent}
.cap{display:inline-flex;align-items:center;gap:14px;padding:13px 22px 13px 18px;margin:8px;
  background:rgba(13,13,15,.9);border:1px solid rgba(255,255,255,.1);border-radius:10px;
  font:500 23px/1.2 "JetBrains Mono",ui-monospace,monospace;color:#ecebe8;white-space:nowrap}
.cap b{color:#f2a33a;font-weight:600}</style></head>
<body><div class="cap"><b>&gt;&gt;</b><span>${safe}</span></div></body></html>`);
    await p.evaluate(() => document.fonts.ready);
    await p.locator('.cap').screenshot({ path: join(scratch, `cap-${scene}.png`), omitBackground: true });
  }
  await b.close();
}

const captions = JSON.parse(readFileSync(join(here, 'captions.json'), 'utf8'));
await renderCaptions(captions);
const take = join(here, 'videos/readme.mp4');

if (!has('--captions-only')) {
  writeFileSync(join(scratch, 'splash.html'), splash());
  const env = {
    ...process.env,
    ORCHD_VIDEO_BASE: base,
    HOST_TOKEN: token,
    SPLASH: join(scratch, 'splash.html'),
    KILL_CHECKOUT: checkouts[0].path,
    CODEX: has('--codex') ? '1' : '0',
    // Both are about the checkouts `docs/demo.md` prepares: the reply the seeded
    // fixture sessions end on, and a task the clone of this repo can take.
    PATH_TEXT: flag('--path-text', 'src/inventory.js:'),
    WORK_TASK: flag('--task', 'add a Paper Dim preset to web/js/theme.js. no tests.'),
  };
  run('npx', ['argo', 'record', 'readme'], { env });
  run('npx', ['argo', 'export', 'readme'], { env });
}
if (!existsSync(take)) die(`no take at ${take}; record one first`);

/* **Caption times come from the take's own chapters**, which Argo writes from the
   scene marks — so a scene that ran long moves its caption with it, where times
   typed in by hand would drift on every take. */
const probe = spawnSync('ffprobe', ['-v', 'error', '-show_chapters', '-show_format', '-of', 'json', take],
  { encoding: 'utf8' });
const { chapters, format } = JSON.parse(probe.stdout);
const duration = Number(format.duration);
const HOLD = 2;
const inputs = ['-i', take];
let filter = `[0:v]tpad=stop_mode=clone:stop_duration=${HOLD}[v0]`;
let last = 'v0';
let n = 1;
for (const ch of chapters) {
  const scene = ch.tags?.title;
  const cap = captions[scene];
  if (!cap) continue;
  const from = Number(ch.start_time) + 0.25;
  const until = (Number(ch.end_time) >= duration - 0.05 ? duration + HOLD : Number(ch.end_time)) - 0.25;
  const len = until - from;
  const y = cap.at === 'bottom' ? 'H-h-102' : '20';
  inputs.push('-loop', '1', '-t', len.toFixed(2), '-framerate', '30', '-i', join(scratch, `cap-${scene}.png`));
  filter += `;[${n}:v]format=rgba,fade=in:st=0:d=0.3:alpha=1,fade=out:st=${(len - 0.3).toFixed(2)}:d=0.3:alpha=1,`
    + `setpts=PTS-STARTPTS+${from.toFixed(2)}/TB[c${n}];[${last}][c${n}]overlay=x=(W-w)/2:y=${y}:eof_action=pass[v${n}]`;
  last = `v${n}`;
  n += 1;
}
run('ffmpeg', ['-v', 'error', '-y', ...inputs, '-filter_complex', filter, '-map', `[${last}]`, '-an',
  '-map_chapters', '-1', '-map_metadata', '-1', '-c:v', 'libx264', '-preset', 'slow', '-crf', '21',
  '-pix_fmt', 'yuv420p', '-movflags', '+faststart', '-r', '30', out]);
console.log(`${out}: ${(duration + HOLD).toFixed(1)} s, ${chapters.length} scenes`);
