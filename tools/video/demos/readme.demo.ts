// The README video: ten scenes in one take, against a running host with two
// checkouts. `run.mjs` starts it; `docs/demo.md` says what to prepare first.
//
// The first checkout is the one with the fixture PR (the PR pane, `Handle`, the
// review queue, and the daemon that gets killed). The second is a clone of this
// repo, where the new session is cut. Rows are found by block and position, never
// by id, so the script survives a fresh set of sessions.
import { test, createHumanCursor } from '@argo-video/cli';
import { execSync } from 'node:child_process';
import { readFileSync } from 'node:fs';

const env = (name: string) => {
  const v = process.env[name];
  if (!v) throw new Error(`${name} is not set; run this through tools/video/run.mjs`);
  return v;
};

const BLOCK = '#rail .co-block';
const LIVE = 'button.sess:not(.arc)[data-id]';
const ALL = `#rail ${LIVE}`;
type Rect = { left: number; top: number; width: number; height: number };

test('readme', async ({ page, narration }) => {
  test.setTimeout(300_000);
  /* **Every action is bounded.** A click that something covers waits for the
     element forever by default, and a take that hangs is a take that recorded
     four minutes of one frame before the test timeout ended it. */
  page.setDefaultTimeout(10_000);
  const beat = (ms: number) => page.waitForTimeout(ms);
  const board = `/?token=${env('HOST_TOKEN')}`;
  const first = page.locator(BLOCK).nth(0);
  const second = page.locator(BLOCK).nth(1);

  const box = (sel: string): Promise<Rect | null> => page.evaluate((s) => {
    const r = [...document.querySelectorAll(s)].map((e) => e.getBoundingClientRect()).filter((x) => x.width);
    if (!r.length) return null;
    const l = Math.min(...r.map((x) => x.left));
    const t = Math.min(...r.map((x) => x.top));
    return { left: l, top: t, width: Math.max(...r.map((x) => x.right)) - l, height: Math.max(...r.map((x) => x.bottom)) - t };
  }, sel);
  /* A camera move as a box rather than a selector: `zoomTo` frames one element,
     and the shots here want a union (the rows and the add row) or a band (the PR
     pane sits on the bottom edge, where a zoom centred on it cannot centre). */
  const camera = (r: Rect | null, scale: number, holdMs: number) => {
    if (!r) return;
    narration.recordCameraMove({
      x: Math.round(r.left + r.width / 2), y: Math.round(r.top + r.height / 2),
      w: Math.round(r.width), h: Math.round(r.height), durationMs: 600, scale, holdMs,
    });
  };
  const bothBlocks = () => page.waitForFunction(([b, l]) => {
    const blocks = document.querySelectorAll(b as string);
    return blocks.length >= 2 && [...blocks].every((x) => x.querySelector(l as string));
  }, [BLOCK, LIVE], { timeout: 30_000 });

  /* **Two things in the page are the recording machine's, not the product's.**
     WebGL goes, so the terminal is drawn by xterm's DOM renderer: that is how the
     Linux app draws it anyway, and a canvas has no text for the path scene to
     find. And the agent-upgrade bar goes: it says this machine's Claude Code is
     behind, and it came back after the splash and sat over the diff toolbar. */
  await page.addInitScript(() => {
    const get = HTMLCanvasElement.prototype.getContext;
    // @ts-ignore: one function standing in for every overload of getContext
    HTMLCanvasElement.prototype.getContext = function (kind: string, ...rest: unknown[]) {
      return /webgl/i.test(kind) ? null : get.call(this, kind, ...rest);
    };
    const hide = () => {
      const s = document.createElement('style');
      s.textContent = '#agentbar{display:none!important}';
      document.head.append(s);
    };
    if (document.head) hide();
    else document.addEventListener('DOMContentLoaded', hide);
  });

  // --- off camera: warm the board once (fonts, sockets), then go dark ---
  await page.goto(board);
  await bothBlocks();
  /* `about:blank` before the splash, not the board's own URL: drawn over the
     board's URL, the later `#splash` navigation is a hash change and never loads
     the board at all. */
  await page.goto('about:blank');
  await page.setContent('<html style="background:#101010"><body style="margin:0"></body></html>');
  await narration.startRecording(page);

  // 1 — the splash types the name, then the board lands from its last frame
  narration.mark('start');
  await page.setContent(readFileSync(env('SPLASH'), 'utf8'));
  await beat(2300);
  await page.goto(`${board}#splash`);
  await bothBlocks();
  await page.waitForFunction(() => !document.documentElement.classList.contains('from-splash'), null, { timeout: 8000 })
    .catch(() => {});
  await beat(900);
  const cursor = await createHumanCursor(page, { seed: 'orchd-readme', size: 26 });

  // 2 — two projects in one rail; a click in the second and every pane follows
  narration.mark('projects');
  await beat(1200);
  await cursor.click(second.locator(LIVE).last());
  await beat(2600);
  await cursor.click(first.locator(LIVE).first());
  await beat(1800);

  // 3 — a new session, in a worktree of its own
  narration.mark('new');
  const before = await page.locator(ALL).count();
  // The second block's rows and its add row, as one box.
  camera(await page.evaluate(([b, l]) => {
    const block = document.querySelectorAll(b as string)[1];
    const r = block ? [...block.querySelectorAll(`${l}, .addbtn`)].map((e) => e.getBoundingClientRect()) : [];
    if (!r.length) return null;
    const left = Math.min(...r.map((x) => x.left));
    const top = Math.min(...r.map((x) => x.top));
    return { left, top, width: Math.max(...r.map((x) => x.right)) - left, height: Math.max(...r.map((x) => x.bottom)) - top };
  }, [BLOCK, LIVE]), 1.6, 4200);
  await beat(400);
  const add = second.locator('.addbtn').first();
  if (process.env.CODEX === '1') {
    // The agent menu exists only with more than one agent installed.
    await add.click({ button: 'right' });
    await beat(700);
    await cursor.click(page.locator('#ctxmenu .ctxmenu-item', { hasText: /codex/i }).first());
  } else {
    await cursor.click(add);
  }
  await page.waitForFunction(([s, n]) => document.querySelectorAll(s as string).length > (n as number),
    [ALL, before], { timeout: 10_000 }).catch(() => {});
  await beat(3600);

  // 4 — give it a task and watch it start
  narration.mark('work');
  await page.locator('#termwrap').click();
  await page.keyboard.press('Control+u');
  /* Typed at a human pace: Claude Code redraws its own composer, and faster
     keystrokes land out of order (`tools/demo.mjs` measured that). */
  await page.keyboard.type(env('WORK_TASK'), { delay: 75 });
  await beat(300);
  await page.keyboard.press('Enter');
  await beat(4200);

  // 5 — a path the agent printed is a link: one click opens the file at that line
  narration.mark('path');
  const needle = env('PATH_TEXT');
  /** Where `needle` is drawn, measured off the DOM renderer's text nodes. */
  const findPath = () => page.evaluate((n) => {
    for (const r of document.querySelectorAll('#termwrap .xterm-rows > div')) {
      const i = (r.textContent ?? '').indexOf(n);
      if (i < 0) continue;
      const walker = document.createTreeWalker(r, NodeFilter.SHOW_TEXT);
      let seen = 0;
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        const len = node.textContent?.length ?? 0;
        if (seen + len > i) {
          const range = document.createRange();
          range.setStart(node, i - seen + 3);
          range.setEnd(node, i - seen + 4);
          const b = range.getBoundingClientRect();
          return { x: b.left + b.width / 2, y: b.top + b.height / 2 };
        }
        seen += len;
      }
    }
    return null;
  }, needle);
  /* The session whose last reply names the path, looked for row by row with
     plain clicks: the drawn cursor travels at a person's pace, and on its way
     between rows it spent six seconds of the scene looking for something. */
  let at = null;
  for (let i = 0; i < (await first.locator(LIVE).count()) && !at; i += 1) {
    await first.locator(LIVE).nth(i).click();
    await beat(700);
    at = await findPath();
  }
  if (at) {
    camera({ left: at.x - 220, top: at.y - 60, width: 440, height: 120 }, 1.8, 1700);
    /* Off the row and back: xterm asks the link provider on *entering* a row, so
       a pointer that arrives on it from the row itself is never offered a link.
       `xterm-cursor-pointer` is xterm saying the link is there. */
    await page.mouse.move(at.x, at.y + 120, { steps: 8 });
    await page.mouse.move(at.x, at.y, { steps: 14 });
    await page.waitForSelector('#termwrap .xterm-cursor-pointer', { timeout: 6000 }).catch(() => {});
    await beat(700);
    await page.mouse.click(at.x, at.y);
  } else {
    console.error(`no session in the first checkout shows ${needle}; the path scene is empty`);
  }
  await page.locator('#fvoverlay.on').waitFor({ timeout: 5000 }).catch(() => {});
  await beat(2400);
  await page.keyboard.press('Escape');
  await beat(500);

  // 6 — what the agent changed, as a diff you can type into
  narration.mark('diff');
  await cursor.click(page.locator('.frow').first());
  await page.locator('#overlay').waitFor({ state: 'visible', timeout: 5000 }).catch(() => {});
  await beat(1500);
  await cursor.click(page.locator('#ovmode')).catch(() => {});
  await beat(1800);
  await cursor.click(page.locator('#ovedit')).catch(() => {});
  await beat(2200);
  await page.keyboard.press('Escape');
  await beat(500);

  // 7 — the PR pane: one press hands the review threads to an agent
  narration.mark('pr');
  const pr = await box('#prpane');
  if (pr) camera({ left: 0, top: pr.top - 120, width: 760, height: pr.height + 120 }, 1.75, 3000);
  await beat(800);
  await cursor.click(page.locator('#prpane').getByText('Handle').first()).catch(() => {
    console.error('no Handle on the PR pane; is the fixture PR open?');
  });
  await beat(4600);

  // 8 — other people's PRs, ranked by how long they have waited on you
  narration.mark('reviews');
  camera(await box('#rvblock'), 1.8, 5200);
  await beat(700);
  const rows = page.locator('#rvlist > *');
  for (let i = 0; i < 3; i += 1) {
    await cursor.moveTo(rows.nth(i)).catch(() => {});
    await beat(650);
  }
  await cursor.click(page.locator('#rvblock').getByText(/not reviewable/).first()).catch(() => {});
  await beat(2200);

  // 9 — themes: each preset is the whole look, applied the moment it is picked
  narration.mark('theme');
  // Settings opens on Theme, so the gear is the only press before the presets.
  await cursor.click(page.locator('#gearbtn'));
  await beat(900);
  /* Picked by value, with no cursor on the select: a click opens the native
     popup, which a screencast does not capture, and the first take spent its
     action timeouts here and recorded 21 s of a dialog that never changed. */
  for (const v of ['paper', 'contrast', 'orchd']) {
    await page.locator('#thpreset').selectOption(v);
    await beat(1700);
  }
  // Escape rather than the close button: the drawn cursor's trip to the far
  // corner was six seconds of a dialog that had already made its point.
  await page.keyboard.press('Escape');
  await beat(700);

  // 10 — the first checkout's daemon killed where it stands, and everything comes back
  narration.mark('revive');
  await beat(900);
  const target = env('KILL_CHECKOUT');
  /* Matched in here rather than by `pkill -f`, whose pattern is in its own
     shell's argv; `tools/demo.mjs` lost a take to exactly that. The child is the
     one with `--main <checkout>` and a `--host-origin`. */
  const victims = execSync('ps -eo pid=,args=', { encoding: 'utf8' }).split('\n')
    .map((l) => l.trim().match(/^(\d+)\s+(.*)$/))
    .filter((m): m is RegExpMatchArray => !!m && m[2].includes(`--main ${target}`) && m[2].includes('--host-origin'))
    .map((m) => Number(m[1]));
  if (!victims.length) console.error(`no child daemon for ${target}; nothing was killed`);
  for (const pid of victims) {
    try { process.kill(pid, 'SIGKILL'); } catch { /* already gone */ }
  }
  await beat(2000);
  await bothBlocks().catch(() => {});
  /* Bounded, unlike `tools/demo.mjs`'s 45 s: the rows coming back is the claim,
     and the first take spent 45 s of frames waiting on a bar that had already
     gone. `run.mjs` holds the last frame instead. */
  await page.waitForFunction(() => document.querySelector('#connbar')?.hasAttribute('hidden') ?? true,
    null, { timeout: 10_000 }).catch(() => {});
  await beat(3500);
  await cursor.dispose();
});
