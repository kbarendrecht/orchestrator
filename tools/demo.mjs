// Record the README's demo GIF, headlessly, against a real daemon.
//
// The same trick `shot.mjs` uses: the UI is served over plain HTTP, so a normal
// browser can drive it with no display and no tauri-driver. This adds a camera.
//
//   mise run demo                            # docs/demo.gif, 1100px wide
//   mise run demo -- --out docs/review.gif   # somewhere else
//   mise run demo -- --keep-video            # keep the webm ffmpeg read
//
// **It drives a real `claude`, on purpose.** A GIF of the fake agent from
// `tools/e2e/` would show a working rail and an empty centre pane, because that
// stand-in honours the four things the daemon reads and paints nothing — see its
// header. What the picture is *for* is the agent working, so the agent has to be
// the real one. That makes this a tool you point at a prepared checkout rather
// than one that builds its own: `docs/demo.md` says what to prepare.
//
// Three things are load-bearing and easy to get wrong, each of them a take that
// had to be thrown away:
//
//   * The terminal is **fitted**, where `shot.mjs` refuses to fit it. See the
//     `observe` note below — unfitted, the scrollback re-wraps at a width the pty
//     never had and every line shears down the right edge.
//   * Typing goes to the **terminal**, not to an input. The centre pane is the
//     agent's pty under xterm, so `keyboard.type` is what a person does; there is
//     no prompt box to fill in — and it has to be slow enough for the TUI.
//   * Rows are clicked by **session id**, never by position: the rail sorts by
//     recency, so taking a turn re-orders the thing being iterated.
import { chromium } from 'playwright-core';
import { mkdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { execFileSync, execSync } from 'node:child_process';

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const i = args.indexOf(name);
  return i === -1 ? fallback : args[i + 1];
};
const has = (name) => args.includes(name);

const port = flag('--port', process.env.ORCHD_PORT || '7777');
const out = flag('--out', 'docs/demo.gif');
const width = Number(flag('--width', '1440'));
const height = Number(flag('--height', '900'));
// What the GIF is scaled to. GitHub lays a README image out at about 880 CSS
// pixels, so this is a little over 1x — sharp without paying for a 1440 frame.
const scale = Number(flag('--scale', '1100'));
// Ten is enough for a UI whose motion is a pane swapping and text arriving, and
// every frame above it is bytes a reader waits for.
const fps = Number(flag('--fps', '10'));
/* `w:h:x:y` in *captured* pixels, applied before the scale. The PR pane and the
   review queue live in two bottom corners and are perhaps a seventh of the frame
   between them, so a whole-window recording of them is mostly centre pane. A band
   across the bottom is the only framing in which both are legible at README width. */
const crop = flag('--crop', null);
// How long to give the agent's turn before moving on. A real model, so this is a
// ceiling rather than a wait: the poll below leaves as soon as the row settles.
const turnMs = Number(flag('--turn', '40000'));
const base = `http://127.0.0.1:${port}`;

let token;
try {
  const html = execSync(`curl -sS --max-time 5 ${base}/`, { encoding: 'utf8' });
  token = html.match(/token:\s*"([^"]+)"/)?.[1];
} catch {
  console.error(`no daemon on ${base} — start one first (see docs/demo.md)`);
  process.exit(1);
}
if (!token) {
  console.error(`could not read the token from ${base}/ — is that really orchd?`);
  process.exit(1);
}

const videoDir = 'target/demo';
rmSync(videoDir, { recursive: true, force: true });
mkdirSync(`${videoDir}/frames`, { recursive: true });

const browser = await chromium.launch({ channel: 'chrome', args: ['--no-sandbox'] });
// `deviceScaleFactor` stays at 1: a browser tab renders xterm through WebGL and
// its glyphs come out at the device scale rather than the CSS one, so a 2x
// recording has a terminal at twice the size of the UI around it. `shot.mjs`
// carries the same note.
const context = await browser.newContext({
  viewport: { width, height },
  deviceScaleFactor: 1,
});
const page = await context.newPage();
page.on('pageerror', (e) => console.error('page error:', e.message));

/* **Chrome's own screencast, not playwright's `recordVideo`.** That option shells
   out to a copy of ffmpeg under `~/.cache/ms-playwright`, which this repo does not
   have and will not fetch: `tools/` pins `playwright-core` precisely so nothing is
   downloaded, and Chrome itself is a system dependency. CDP gives the same frames
   playwright would have encoded, and the system ffmpeg encodes them below.

   The useful property is that Chrome emits a frame when something *changes*. A
   still hold costs no frames at all, so the timings collected here are real
   timings, and the concat list further down replays them rather than guessing a
   constant rate. */
const cdp = await context.newCDPSession(page);
const frames = [];
cdp.on('Page.screencastFrame', async ({ data, sessionId, metadata }) => {
  const file = `${videoDir}/frames/${String(frames.length).padStart(5, '0')}.jpg`;
  writeFileSync(file, Buffer.from(data, 'base64'));
  frames.push({ file: file.replace(`${videoDir}/`, ''), at: metadata.timestamp });
  // Chrome sends nothing further until each frame is acknowledged.
  await cdp.send('Page.screencastFrameAck', { sessionId }).catch(() => {});
});

/* **This one fits the terminal, where `shot.mjs` refuses to.** `observe=1` keeps
   the fit local so a screenshot cannot resize the session's pty — right for a tool
   you point at the checkout you are working in. It is wrong here: the pty then
   keeps whatever width it was last given and the pane re-wraps its scrollback at a
   different one, which recorded as text shearing down the right edge of every
   line. A demo is recorded against a throwaway checkout with no window open on it,
   so the fit costs nothing. `--observe` puts the guard back for anyone recording
   against a checkout they also have open. */
const observe = has('--observe') ? '&observe=1' : '';
await page.goto(`${base}/?token=${token}${observe}`, { waitUntil: 'domcontentloaded' });
await page.waitForTimeout(1200);
// The page renders on its first websocket snapshot; poke the daemon for one
// rather than waiting out a poll.
await page.evaluate(async () => {
  await fetch('/api/workspace/main/reconcile', {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-orch-token': window.__ORCH__.token },
    body: '{}',
  }).catch(() => {});
});
/* **One rail, several checkouts.** With more than one open, the rail wraps each
   checkout's rows in `.co-block[aria-label="<name>"]` — and with exactly one it
   draws no block at all, because a header naming the only checkout there is would
   be noise. So the scope is the block when `--checkout` names one, and the whole
   rail otherwise. */
const only = flag('--checkout', null);
const SCOPE = only ? `#rail .co-block[aria-label="${only}"]` : '#rail';
/** A live session row. Archived ones carry `.arc` and are not part of the demo. */
const LIVE = `${SCOPE} button.sess:not(.arc)[data-id]`;
await page.waitForFunction((sel) => document.querySelectorAll(sel).length >= 2, LIVE, {
  timeout: 20000,
});

/* **Held by session id, never by position.** The rail orders by recency, so every
   turn taken below re-sorts it — and a script clicking `nth(1)` therefore seeds one
   session twice and another not at all. That is not hypothetical: it is what put a
   half-typed draft from one pane into the composer of another in the third take. */
const ids = await page.$$eval(LIVE, (els) => els.map((e) => e.dataset.id));
const row = (id) => page.locator(`#rail button.sess:not(.arc)[data-id="${id}"]`);
const count = ids.length;
if (count < 2) {
  console.error(`only ${count} live session row(s) — the demo wants at least two`);
  process.exit(1);
}

/** Hold the frame, so a reader can register what just changed. */
const beat = (ms) => page.waitForTimeout(ms);

/** Type into the agent's pty, at a speed its TUI can keep up with.
 *
 *  **Not `fill`, and not fast.** The centre pane is a pty under xterm, so there is
 *  no input to fill — and Claude Code's TUI redraws its own composer, so keystrokes
 *  arriving faster than it repaints land out of order: at 42ms the first take
 *  recorded `ainionelsentence,iwhatpdoese…` on screen. This is a human speed
 *  because a human is what it is standing in for.
 */
async function ask(text) {
  await page.locator('#termwrap').click();
  await beat(400);
  /* Clear whatever the composer is holding first. A pane is durable — that is the
     product — so a draft from a previous run of this script is still sitting in
     it, and typing would append to that. `Ctrl+U` is the readline kill that
     Claude Code's composer honours. */
  await page.keyboard.press('Control+u');
  await beat(250);
  await page.keyboard.type(text, { delay: 85 });
  await beat(600);
  await page.keyboard.press('Enter');
}

/* The predicate runs in the *page*, not here, so it is a source string rather than
   a function this file could share: a closure passed to `waitForFunction` is
   serialised and evaluated in the browser, where nothing from this module exists. */
const BUSY = `/working|thinking/i.test(document.querySelector('#rail')?.textContent ?? '')`;

/** Wait for a turn to start, and then to finish.
 *
 *  **Both halves, because waiting only for the end returns immediately.** The rail
 *  flips to `working` when the hook lands, which is a moment *after* the Enter that
 *  caused it — so a lone "nothing is working" poll is satisfied by the gap before
 *  the turn has begun. That is not a theory: it ended the fifth take three seconds
 *  into a reply, on the word `thinking`.
 */
async function settle(ms) {
  await page.waitForFunction(BUSY, null, { timeout: 15000 }).catch(() => {
    console.error('warning: no turn ever started');
  });
  await page.waitForFunction(`!(${BUSY})`, null, { timeout: ms }).catch(() => {
    console.error('warning: a turn did not settle in time');
  });
}

/* The app's upgrade bar, which sits across the header. Dismissed rather than waited
   out: an agent version behind is a fact about the recording machine, not about the
   product.

   **Waited for, then clicked, then confirmed.** A bare click with a short timeout
   silently did nothing in the multi-checkout take — the bar is drawn from the
   snapshot, so it appears *after* the first websocket frame rather than with the
   page, and the click landed before it existed. The nag inside the pty is a
   different thing and cannot be clicked away: that one is Claude Code's own, and
   the fix for it is to have the agent the checkout resolves actually be current. */
const bar = page.locator('#agentx');
if (await bar.waitFor({ state: 'visible', timeout: 8000 }).then(() => true, () => false)) {
  await bar.click().catch(() => {});
  await bar.waitFor({ state: 'hidden', timeout: 3000 }).catch(() => {
    console.error('warning: the upgrade bar would not dismiss; it will be in the picture');
  });
}

/* **A pane with no conversation in it is the whole picture wasted**, and a fresh
   session has none — Claude Code draws an empty composer and nothing else, which
   is what the first take recorded. So every row gets a real turn before the
   camera starts. **Edits rather than questions**, because the changed-files pane
   is part of the picture and a worktree nobody has touched reads `Nothing changed
   in this worktree yet.` — three times over. `no tests` keeps each turn to one
   small diff; the agent will otherwise write a suite and the turn runs long. */
/* **Three different subjects, not three phrasings of one.** The rail labels a row
   with the session's own title, which Claude Code derives from the conversation —
   so seeds that rhyme produce three rows reading `available() return valu…`, and
   the picture stops showing that these are separate pieces of work. */
const seeds = [
  'add a reserve(sku, count) to src/inventory.js that refuses to go below zero. no tests.',
  'add a LOW_STOCK constant of 5 and an isLow(sku) helper to src/inventory.js. no tests.',
  'make receive() reject a sku that is not a non-empty string. no tests.',
];
const wanted = flag('--seeds', null);
if (wanted) seeds.splice(0, seeds.length, ...wanted.split('|'));
for (let i = has('--no-seed') ? -1 : Math.min(count, seeds.length) - 1; i >= 0; i -= 1) {
  await row(ids[i]).click();
  await beat(900);
  await ask(seeds[i]);
  await settle(turnMs);
  await beat(500);
}

/* Seeding and recording are separable because with several checkouts they have to
   be: each repository needs prompts about *its own* code, and one recording spans
   both rails. So the setup is one `--seed-only` run per checkout, and the take is a
   `--no-seed` run over the lot. */
if (has('--seed-only')) {
  console.log(`seeded ${Math.min(count, seeds.length)} session(s)${only ? ` in ${only}` : ''}`);
  await context.close();
  await browser.close();
  process.exit(0);
}

// Rolling from here, so none of the setting-up is in the picture. `--panes` and
// `--revive` start it themselves, because each has a beat that must happen before
// the camera does.
if (!has('--panes') && !has('--revive')) await cdp.send('Page.startScreencast', {
  format: 'jpeg',
  // High, because this is re-encoded to a GIF palette afterwards and every
  // artefact introduced here survives into that.
  quality: 92,
  maxWidth: width,
  maxHeight: height,
});

/* **Two scripts, because there are two claims.** The default one is about several
   agents over *one* repository; `--multi` is about several repositories in one
   window, which is a different sentence and needs the rail's checkout blocks in
   shot rather than a single flat list. */
/* **The panes, which are the half of the board the other GIFs only have in the
   corner of frame.** Both start folded and are opened one at a time, because
   "the PR pane and the review queue have things in them" is a statement about
   content, and content arriving is the only way a still strip can show it. */
if (has('--panes')) {
  const prs = page.locator('.prgroup-head');
  const queue = page.locator('#rvhead');
  // Folded first, off camera. Fold state is this browser's, so a fresh context
  // opens them and there would otherwise be nothing to reveal.
  await prs.click().catch(() => {});
  await queue.click().catch(() => {});
  await beat(600);
  await cdp.send('Page.startScreencast', { format: 'jpeg', quality: 92, maxWidth: width, maxHeight: height });

  await beat(2400);
  // 1 — your open PRs, with the review threads still waiting on you counted on
  // the row, and the button that hands the whole thread to an agent.
  await prs.click();
  await beat(3400);
  // 2 — and the queue of other people's PRs waiting on *you*, ranked: red for a
  // stopper, amber where somebody asked for you by name, grey for a team request.
  await queue.click();
  await beat(4200);
} else if (has('--revive')) {
  /* **Two claims in one take**, because they are the same claim: the daemon owns
     the work, not the window. First the changed file the agent wrote, opened as a
     real diff against the merge-base; then the daemon killed outright, and every
     session back where it was. */
  await cdp.send('Page.startScreencast', { format: 'jpeg', quality: 92, maxWidth: width, maxHeight: height });
  await beat(2200);

  // 1 — a changed file is a diff, not a filename.
  await page.locator('.frow').first().click();
  await beat(3000);
  // 2 — stepped through, the way you would read it.
  await page.locator('text=NEXT').first().click().catch(() => {});
  await beat(2400);
  await page.locator('body').press('Escape');
  await beat(1600);

  /* 3 — the kill. Not a close and not a restart button: the checkout's daemon is
     killed where it stands, which is what a crash looks like. The host notices,
     restarts it once, and hands the page the new port and token — so the page
     never reloads and nothing here re-navigates. `auto_resume` is what brings the
     agents back with it. */
  const killed = flag('--kill', null);
  if (!killed) {
    console.error('--revive wants --kill <substring of the child daemon argv>');
    process.exit(1);
  }
  /* **Matched in here, not by `pkill`.** A `pkill -f` whose pattern names the
     checkout matches the shell `execSync` spawned to run it — the pattern is in
     that shell's own argv — so the first attempt killed this recorder instead of
     the daemon, with a `SIGTERM` and no output. `ps` lists processes without the
     pattern ever appearing in one. */
  const mine = process.pid;
  const victims = execSync('ps -eo pid=,args=', { encoding: 'utf8' })
    .split('\n')
    .map((l) => l.trim().match(/^(\d+)\s+(.*)$/))
    .filter((m) => m && Number(m[1]) !== mine)
    /* `--host-origin` starts with `--host`, and the child carries it — so excluding
       the host by that substring excluded the child too, and the first run of this
       found nothing to kill. The host is the one where `--host` is followed by a
       space. */
    .filter((m) => m[2].includes('--main') && m[2].includes(killed) && !/--host\s/.test(m[2]))
    .map((m) => Number(m[1]));
  if (!victims.length) {
    console.error(`no child daemon matched ${killed}; nothing to kill`);
    process.exit(1);
  }
  for (const pid of victims) {
    try {
      process.kill(pid, 'SIGKILL');
    } catch {
      /* already gone */
    }
  }
  console.log(`killed ${victims.join(', ')}`);
  await beat(3000);
  // 4 — back, without a reload. Held long: this is the frame that is the claim.
  await page
    .waitForFunction(`document.querySelectorAll('${LIVE.replace(/'/g, "\\'")}').length >= 2`, null, {
      timeout: 60000,
    })
    .catch(() => console.error('warning: the sessions did not come back in time'));
  /* **And wait for the page to stop saying so.** The rows come back while the
     sockets are still re-establishing, so a take that ended on the row count ended
     on a `reconnecting…` chip — which reads as the opposite of the claim.

     Asked of the two elements, never of `document.body.textContent`: `#connbar`
     is in `index.html` permanently with that word inside it and `hidden` on it,
     and `textContent` reads hidden nodes — so a text match is true before anything
     has even been killed, and the wait always timed out. */
  await page
    .waitForFunction(
      () => {
        const bar = document.querySelector('#connbar');
        if (bar && !bar.hasAttribute('hidden')) return false;
        return ![...document.querySelectorAll('.term-badge')].some((b) => !b.hasAttribute('hidden'));
      },
      null,
      { timeout: 45000 },
    )
    .catch(() => console.error('warning: the page was still reconnecting'));
  await beat(4500);
} else if (has('--multi')) {
  const blocks = await page.$$eval('#rail .co-block', (els) =>
    els.map((e) => e.getAttribute('aria-label')),
  );
  if (blocks.length < 2) {
    console.error(`--multi wants two checkouts open; the rail shows ${blocks.length}`);
    process.exit(1);
  }
  const first = (name) =>
    page.locator(`#rail .co-block[aria-label="${name}"] button.sess:not(.arc)[data-id]`).first();

  // 1 — the whole rail, still: two repositories, their sessions under their own
  // names. This is the frame the GIF exists for, so it is held the longest.
  await beat(3000);

  // 2 — a session in the second repository. The pane swaps to another codebase
  // entirely, and the changed-files and PR panes swap with it: they belong to the
  // checkout, not to the window.
  await first(blocks[1]).click();
  await beat(3400);

  // 3 — and back. Nothing reconnected, nothing restarted.
  await first(blocks[0]).click();
  await beat(3000);

  // 4 — a live turn in the first repository, so the picture is not only furniture.
  await ask(flag('--ask', 'in one line: what does reserve() do for an unknown sku?'));
  await settle(turnMs);
  await beat(3000);
} else {

// 1 — the board, still. Whoever is watching has to read the layout before
// anything moves, and a GIF that starts mid-gesture reads as a glitch.
await beat(2400);

// 2 — the claim, made first because it is the one nothing else here does: three
// agents are running over one repository, and each is a click away with its
// conversation where it was left. Nothing restarts, nothing reconnects.
await row(ids[1]).click();
await beat(2400);
if (count > 2) {
  await row(ids[2]).click();
  await beat(2400);
}
await row(ids[0]).click();
await beat(1800);

// 3 — and it is live, not a recording of one. Typed at a human speed into the
// agent's own pty, which is what the centre pane has always been.
await ask(flag('--ask', 'in one line: what happens if reserve() is called for an unknown sku?'));

// 4 — the turn. Leave as soon as the rail stops saying anything is working, so
// the GIF is as long as that machine made it rather than a fixed guess.
await settle(turnMs);

// 5 — the answer, held long enough to read a line of.
await beat(3400);
}

await cdp.send('Page.stopScreencast').catch(() => {});
await context.close();
await browser.close();

if (frames.length < 2) {
  console.error(`only ${frames.length} frame(s) captured — nothing to encode`);
  process.exit(1);
}

/* The concat demuxer, with a real duration per frame. Chrome only sends a frame
   when the page changes, so the gaps between them *are* the pacing — feeding the
   list to `-framerate N` instead would stretch every still hold to one frame's
   worth and throw the timing away. The last entry is repeated because concat
   ignores the final `duration`. */
const list = frames
  .map((f, i) => {
    const next = frames[i + 1];
    const secs = next ? Math.min(next.at - f.at, 4) : 1 / fps;
    return `file '${f.file}'\nduration ${secs.toFixed(3)}`;
  })
  .join('\n');
const listFile = `${videoDir}/frames.txt`;
writeFileSync(listFile, `${list}\nfile '${frames[frames.length - 1].file}'\n`);

/* Two passes, because a one-pass GIF falls back to a generic palette and this UI
   is a dark theme of close greys — which posterises into bands exactly where the
   panes meet. `palettegen` reads the whole clip first and `paletteuse` maps to
   it, and `stats_mode=diff` weights the palette towards what actually moves
   rather than the large still background. */
mkdirSync(out.replace(/\/[^/]+$/, ''), { recursive: true });
const palette = `${videoDir}/palette.png`;
const cropf = crop ? `crop=${crop},` : '';
const read = ['-f', 'concat', '-safe', '0', '-i', listFile];
execFileSync('ffmpeg', ['-y', '-loglevel', 'error', ...read,
  '-vf', `fps=${fps},${cropf}scale=${scale}:-1:flags=lanczos,palettegen=stats_mode=diff`, palette]);
execFileSync('ffmpeg', ['-y', '-loglevel', 'error', ...read, '-i', palette,
  '-lavfi', `fps=${fps},${cropf}scale=${scale}:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=3`,
  '-loop', '0', out]);

if (!has('--keep-video')) rmSync(`${videoDir}/frames`, { recursive: true, force: true });
console.log(`${frames.length} frames captured`);
const mb = (statSync(out).size / 1e6).toFixed(1);
console.log(`${out}  ${mb} MB`);
if (Number(mb) > 8) {
  console.error(`warning: ${mb} MB is a lot for a README — try --scale 900 or --fps 8`);
}
