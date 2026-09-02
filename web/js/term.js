// The terminals: one xterm per session or process, attached to the daemon's pty
// over a websocket. The DOM renderer is deliberate under WebKitGTK, and only
// there — see the renderer comment below, and CLAUDE.md.

import { $, CHROME, IS_MAC, TOKEN, WS_BASE, el, mark, reportBoot, selected, terms, toast, typingElsewhere, uiScale } from './core.js';


const THEME = {
  background: '#101010', foreground: '#D2D2D2', cursor: '#D2D2D2',
  black: '#101010', red: '#C9615A', green: '#5FA97C', yellow: '#E0A244',
  blue: '#4C9AAF', magenta: '#9A7AA0', cyan: '#3E9AAF', white: '#D2D2D2',
  brightBlack: '#5B5B5B', brightRed: '#D6756E', brightGreen: '#74BB90',
  brightYellow: '#EDB55C', brightBlue: '#63AEC2', brightMagenta: '#B08FB6',
  brightCyan: '#57AEC2', brightWhite: '#F0F0F0',
  selectionBackground: '#2C2C2C',
};

/** The terminal's font in px. xterm draws its own text, so the stylesheet's
 *  multiplier cannot reach it; this applies the same factor natively, which is
 *  also why it stays crisp. */
const TERM_FONT = 12;
const termFontSize = () => Math.round(TERM_FONT * uiScale());

/** Put text on the clipboard, whatever the webview allows.
 *
 *  WebKitGTK refuses the async clipboard API in a webview often enough that its
 *  `NotAllowedError` was showing up as a toast that read like a bug. The old
 *  `execCommand` path has no permission to refuse: inside a user gesture it just
 *  copies, which is what a keypress in a terminal is. */
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (e) { /* fall through to the one that works */ }
  try {
    const ta = el('textarea');
    ta.value = text;
    // Off-screen rather than hidden: a `display:none` textarea cannot be selected.
    ta.style.cssText = 'position:fixed;top:-1000px;opacity:0';
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand('copy');
    ta.remove();
    if (!ok) throw new Error('refused');
    return true;
  } catch (e) {
    toast('this window is not allowed to write to the clipboard', true);
    return false;
  }
}

/** Attach to a pty, replaying the daemon's buffer first. */
function openTerm(target, parent) {
  if (terms.has(target)) return terms.get(target);

  const host = el('div', 'termhost');
  parent.appendChild(host);

  /* Whether an agent is on the other end of this pty, which decides one binding
     below. The daemon's own naming answers it: sessions are keyed by id, every
     process and shell by its workspace. */
  const agentPane = target.startsWith('session:');

  const term = new Terminal({
    theme: THEME,
    fontFamily: "'IBM Plex Mono', ui-monospace, monospace",
    fontSize: termFontSize(),
    lineHeight: 1.25,
    cursorBlink: true,
    /* Sized to what the daemon can actually replay, not to the largest number
     * that felt generous. Two reasons, both measured:
     *
     * xterm keeps every line as a `Uint32Array` of `cols * 3` words, so depth
     * costs real memory — at 40x140, a fully-scrolled terminal held +36.7 MB of
     * process RSS at 10000 lines against +13.3 MB at 2000. That is ~23 MB per
     * terminal, and buffers are held whether or not the terminal paints, so a
     * drawer full of parked sessions paid it too.
     *
     * And the depth beyond this was never durable: `BUFFER_BYTES` (`pty.rs`) is
     * a 512KB ring, which is ~3600 lines of dense 140-column output, so anything
     * deeper vanished at the next reload while still costing memory in the
     * meantime. Keeping the two in the same range makes scrollback survive a
     * reattach instead of silently shortening. Raise both or neither. */
    scrollback: 2000,
    allowProposedApi: true,
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(host);

  /* Copy and paste, in whichever spelling the platform uses: ⌘C/⌘V on a Mac,
   * Ctrl+Shift+C/V elsewhere — the terminal convention, because plain Ctrl+C has
   * to go on reaching the pty, where interrupting is what it means. xterm passes
   * every keystroke through, so without this the copy shortcut arrived at the
   * agent as a control code and the selection stayed where it was.
   *
   * On a Mac ⌘ needs no Shift precisely because it never reaches the pty, so
   * there is no interrupt to protect it from.
   *
   * Returning false tells xterm not to handle the event itself. */
  term.attachCustomKeyEventHandler((e) => {
    if (e.type !== 'keydown') return true;
    /* **Shift+Enter is a newline, and a terminal cannot say so by itself.** Enter
       and Shift+Enter both leave xterm as a bare CR — measured here with `cat -v`,
       which showed no escape at all — so Claude Code has nothing to tell them
       apart by and submits the prompt instead of breaking the line. Its own
       `/terminal-setup` fixes this in iTerm2 and VS Code by binding the chord to
       ESC then CR; this is the same binding, made here so it works out of the box
       and needs no setup command run against a terminal the user does not own.

       Shift alone: with Ctrl, ⌘ or Alt held it is a different chord and belongs to
       whoever claims it. `sock` is closed over rather than passed, and it exists
       by the time any key is pressed.

       **Agent panes only, and that is not caution — a shell needs the opposite.**
       Measured in a drawer shell: with the escape sent, the line is still
       submitted and the ESC arrives *inside the command*, so `cat -v` read `A^[`
       where the user typed `A`. Nothing in a terminal can tell what is running in
       it, but the daemon's own key can: a session is `session:<id>`, a shell or a
       managed process is `<workspace>:…`. */
    if (agentPane && e.key === 'Enter' && e.shiftKey && !e.ctrlKey && !e.metaKey && !e.altKey) {
      // Both, and the second is not belt and braces: returning false keeps xterm
      // from handling the key, but the browser still delivers it to the hidden
      // textarea, which sends a CR of its own. Measured — the pty received
      // `1b 0d` followed by `0d`, so the agent saw the newline *and* the submit.
      e.preventDefault();
      if (sock.readyState === WebSocket.OPEN) sock.send(new TextEncoder().encode('\x1b\r'));
      return false;
    }
    const combo = IS_MAC ? e.metaKey && !e.ctrlKey : e.ctrlKey && e.shiftKey;
    if (!combo) return true;
    const key = e.key.toLowerCase();
    if (key === 'c') {
      const text = term.getSelection();
      if (text) copyText(text);
      return false;
    }
    // Paste is deliberately *not* claimed here. Both spellings already reach
    // xterm's textarea as a native `paste` event, so the text lands either way —
    // and reading the clipboard ourselves needs a permission the webview does not
    // grant, which meant every paste worked and then toasted "this window is not
    // allowed to read the clipboard" on top of it.
    return true;
  });
  /* **No WebGL under WebKitGTK, and that is the whole of the rule.** Its WebGL
   * renderer garbles glyphs there: text arrives as noise and comes back only when
   * a scroll or a selection forces a full redraw, which is the canvas being
   * composited wrong rather than the buffer being wrong. Repainting after every
   * refit and dropping the addon on context loss both failed to fix it, so the
   * canvas goes and the DOM renderer draws real text, which cannot garble.
   *
   * Two engines get the fast path back:
   *
   * - **A browser tab** (`chrome === 'none'`), which is Chromium or Firefox.
   * - **macOS**, which is WKWebView and not WebKitGTK. The condition used to be
   *   `chrome` alone, so a Mac took the DOM renderer on the strength of a bug
   *   measured on Linux — and paid for it, because the DOM renderer's cost is
   *   visible cells times paint rate, a Retina panel composites four times the
   *   pixels, and Claude Code repaints its whole TUI frame per keystroke. That is
   *   the reported typing lag.
   *
   * **The macOS half is an experiment, and only a Mac can settle it**: the open
   * question is whether WKWebView garbles glyphs the way WebKitGTK does. If it
   * does, the symptom is unmistakable (noise that a scroll cleans up) and the fix
   * is to drop `IS_MAC` from this condition. The context-loss disposal below is
   * the safety net either way.
   *
   * Not the *scrolling* complaint, which was the other suspect this was held
   * against: that turned out to be xterm damping sub-50px wheel deltas, measured
   * separately, and no renderer would have changed it. */
  if (CHROME === 'none' || IS_MAC) {
    try {
      const webgl = new WebglAddon.WebglAddon();
      // A lost context leaves the canvas frozen on whatever it last painted, and
      // nothing in xterm notices. Dropping the addon puts the DOM renderer back.
      webgl.onContextLoss?.(() => webgl.dispose());
      term.loadAddon(webgl);
    } catch (e) {
      // Software rendering is slower but correct; not worth failing over.
    }
  }

  /* **A slow trackpad drag scrolls an agent pane in jerks, and xterm's own wheel
   * maths is why.** With mouse reporting on — Claude Code sets `?1000h`,
   * `?1002h`, `?1003h`, `?1006h` and never leaves the normal screen — every
   * wheel event goes down the mouse-report path, and `consumeWheelEvent` does two
   * things to it:
   *
   *   `Math.abs(deltaY) < 50 && (r *= .3)`   — sub-50px events cut to 30%
   *   only whole lines pass; the rest is banked
   *
   * A mouse wheel reports line-mode deltas or pixel deltas over 100 and never
   * enters that branch. **A macOS trackpad reports small pixel deltas and always
   * does.** Measured against this same vendored build: at 1px per event, 5 of 300
   * reached the agent and 295 were dropped; the cliff sits exactly on the 50px
   * constant. A real slow drag has a 13px median, so every event of it is damped,
   * ~4.6 events per line moved. That is the pane sitting still and then jumping.
   *
   * The same path also sends exactly **one report per event** whatever the delta,
   * throwing away the line count it just computed — so a fast drag under-scrolls
   * per pixel of travel. Both defects are in the one place, so both are fixed
   * here: accumulate the real pixel deltas, undamped, and emit one report per
   * whole line.
   *
   * `scrollSensitivity` is not the fix and is worth naming as a dead end: it
   * multiplies *before* the threshold test and the test reads the raw `deltaY`,
   * so a value that cancels the damping for a trackpad makes a mouse three times
   * too fast.
   *
   * **Agent panes only**, the same rule Shift+Enter above follows and for the
   * same reason: this writes a mouse report in the SGR encoding, which is right
   * because the agent asked for `?1006h`, and would be wrong for some other
   * program that enabled reporting in the default encoding. A drawer shell keeps
   * xterm's own behaviour. Two more ways out, both deliberate: `deltaMode !== 0`
   * is a line- or page-mode device, which was never damped, and `shiftKey` is
   * xterm's documented escape to the scrollback — `consumeWheelEvent` returns 0
   * on Shift, so that keeps working and is worth knowing about. */
  let wheelLines = 0;
  term.attachCustomWheelEventHandler((ev) => {
    if (!agentPane || ev.shiftKey || ev.deltaMode !== 0) return true;
    // `modes` is public API. `any`/`drag` is `?1003h`/`?1002h`, which is what an
    // agent TUI sets; anything less capable is left alone rather than guessed at,
    // and `none` means xterm is about to scroll its own buffer, which is correct.
    const tracking = term.modes?.mouseTrackingMode;
    if (tracking !== 'any' && tracking !== 'drag') return true;

    const box = host.getBoundingClientRect();
    const cell = box.height / term.rows;
    if (!(cell > 0)) return true;
    wheelLines += ev.deltaY / cell;
    // `trunc`, so the sign is kept and the remainder is banked rather than
    // rounded away. Banking is what makes a slow drag move at all: 0.7 of a line
    // is not nothing, it is the next event's head start.
    const lines = Math.trunc(wheelLines);
    // Nothing whole yet, but the event is ours: `false` stops xterm applying its
    // own damped version on top of what we have banked.
    if (!lines) return false;
    wheelLines -= lines;

    /* The cell under the pointer, 1-based, which is what the SGR form wants.
     * Derived from the host box rather than asked of xterm, which keeps its own
     * padding-aware version private. Close enough by construction: the agent uses
     * the report to decide *which pane* the wheel is over, and a cell either way
     * inside the right pane changes nothing. */
    const col = Math.min(term.cols, Math.max(1,
      Math.floor((ev.clientX - box.left) / (box.width / term.cols)) + 1));
    const row = Math.min(term.rows, Math.max(1,
      Math.floor((ev.clientY - box.top) / cell) + 1));
    // 64 is wheel up, 65 is wheel down — the same codes xterm's own encoder
    // emits, in the same `ESC [ < btn ; col ; row M` shape.
    const button = lines < 0 ? 64 : 65;
    // A fling can bank a lot of lines. Capped at a page, because past that the
    // reports are a burst the agent has to parse and nobody asked to travel that
    // far in one frame.
    const count = Math.min(Math.abs(lines), term.rows);
    if (sock.readyState === WebSocket.OPEN) {
      sock.send(new TextEncoder().encode(`\x1b[<${button};${col};${row}M`.repeat(count)));
    }
    return false;
  });

  const sock = new WebSocket(
    `${WS_BASE}/ws/pty?token=${encodeURIComponent(TOKEN)}&target=${encodeURIComponent(target)}`
  );
  sock.binaryType = 'arraybuffer';
  const entry = { term, fit, sock, host };

  sock.onopen = () => {
    // The centre pane's two halves, and they fail separately: `attach` is the
    // pty being there at all, `paint` is the daemon's replay arriving. A gap
    // between them is the ring buffer being written into a DOM renderer; a long
    // `attach` is the session not having been spawned yet.
    mark('attach');
    // A fresh socket knows nothing about the size, whatever the last one was told.
    entry.sent = null;
    entry.box = null;
    resize(entry);
    // A session you just created is selected before there is anything to type
    // into, so the focus `select` asked for landed on nothing. Take it once the
    // pty is actually attached, but only if this is still the session you are
    // in, or a slow one would steal the keyboard back later — and never out of a
    // box you are typing in, which is how a rename in the rail lost the keyboard
    // mid-word and committed what had been typed so far.
    if (terms.get(`session:${selected}`) === entry && !typingElsewhere()) {
      try {
        term.focus();
      } catch (e) { /* disposed while the socket was opening */ }
    }
  };
  sock.onmessage = (ev) => {
    const chunk = typeof ev.data === 'string' ? ev.data : new Uint8Array(ev.data);
    /* A terminal nobody is looking at is not written to, it is queued. `hidden`
       is `display:none`, which parks the *renderer* — it does not stop `write`,
       and on WebKit parsing into an unpainted buffer is worse than into a painted
       one: measured at 8 terminals of 140x40, seven hidden cost 172-188 ms a frame
       against 37-41 with all eight visible. That is the late echo when you type,
       because the keystroke's round trip waits behind the main thread.

       The socket stays open, so nothing is renegotiated and the pty is never
       detached; only the parse moves to the moment the pane is looked at. */
    if (host.hidden) return queueChunk(entry, chunk);
    term.write(chunk);
    mark('paint');
    reportBoot();
  };

  term.onData((d) => {
    if (sock.readyState === WebSocket.OPEN) sock.send(new TextEncoder().encode(d));
  });

  terms.set(target, entry);
  return entry;
}

/** How much a hidden terminal may bank before the oldest of it is dropped.
 *
 *  Generous, because dropping is a real loss: unlike a reattach, nothing replays
 *  this. Bounded, because a build watcher left hidden overnight would otherwise
 *  hold everything it ever printed. Beyond this xterm would have thrown it away
 *  anyway — `scrollback: 2000` at 140 columns is about 280 KB of text — so the
 *  cap only discards what the terminal itself would not have kept.
 */
const HIDDEN_BUDGET = 1 << 20;

/** Bank a chunk for a terminal that is not being looked at. */
function queueChunk(entry, chunk) {
  if (!entry.queued) { entry.queued = []; entry.queuedBytes = 0; }
  entry.queued.push(chunk);
  entry.queuedBytes += typeof chunk === 'string' ? chunk.length : chunk.byteLength;
  while (entry.queuedBytes > HIDDEN_BUDGET && entry.queued.length > 1) {
    const old = entry.queued.shift();
    entry.queuedBytes -= typeof old === 'string' ? old.length : old.byteLength;
  }
}

/** Write what arrived while this terminal was hidden, in the order it arrived. */
function flushQueued(entry) {
  if (!entry.queued?.length) return;
  const queued = entry.queued;
  entry.queued = [];
  entry.queuedBytes = 0;
  for (const chunk of queued) entry.term.write(chunk);
}

function resize(entry) {
  if (!entry || entry.host.hidden) return;
  // Nothing moved, nothing to do. Without this a repeated observation refits at
  // the same size, and a box whose width lands between two whole cells can flip
  // the answer back and forth — which reads as the terminal resizing itself.
  const box = entry.host.getBoundingClientRect();
  const seen = `${Math.round(box.width)}x${Math.round(box.height)}`;
  if (entry.box === seen) return;
  // A host that is visible but not laid out yet measures as nothing, and fitting
  // to that hands the pty a couple of columns. The TUI on the other end redraws
  // itself to fit and its previous frame is gone, so the pane comes back shrunk
  // and full of the wreckage of the old one. There is no useful terminal this
  // small, so wait for a real box instead.
  if (box.width < 80 || box.height < 40) return;
  entry.box = seen;
  try {
    entry.fit.fit();
  } catch (e) {
    return;
  }
  // The canvas was just resized under the renderer. On WebKitGTK that is where
  // the glyphs come back as garbage that a scroll or a selection cleans up: the
  // buffer is right and the paint is not, so ask for the paint.
  repaint(entry);
  const { rows, cols } = entry.term;
  // Only tell the pty when the geometry actually moved. That makes a refit
  // idempotent, which is what lets the observer below fire as often as it likes
  // instead of costing a resize message per frame of a drag.
  if (entry.sent && entry.sent.rows === rows && entry.sent.cols === cols) return;
  entry.sent = { rows, cols };
  if (entry.sock.readyState === WebSocket.OPEN) {
    entry.sock.send(JSON.stringify({ type: 'resize', rows, cols }));
  }
}

/** Redraw a terminal from its buffer, glyph atlas and all.
 *
 *  Dropping the atlas is the half that matters after a resize or a spell hidden:
 *  it is the piece that survives the canvas being sized to something else, and
 *  it is what the leftover garbage is made of. */
function repaint(entry) {
  requestAnimationFrame(() => {
    try {
      entry.term.clearTextureAtlas?.();
      entry.term.refresh(0, Math.max(0, entry.term.rows - 1));
    } catch (e) { /* a disposed terminal has nothing to refresh */ }
  });
}

function closeTerm(target) {
  const entry = terms.get(target);
  if (!entry) return;
  try { entry.sock.close(); } catch (e) { /* already gone */ }
  entry.term.dispose();
  entry.host.remove();
  terms.delete(target);
}

/** Tab switch replays the daemon buffer; it never respawns (§9). */
function showTerm(target, parent) {
  const entry = target ? openTerm(target, parent) : null;
  for (const [key, e] of terms) {
    if (e.host.parentElement !== parent) continue;
    e.host.hidden = key !== target;
    // Everything that arrived while it was away, before the repaint below asks
    // xterm what it holds.
    if (!e.host.hidden) flushQueued(e);
  }
  // Only the centre pane owns the empty state. Without this guard, every
  // drawer render un-hides it and "No session selected" sits on top of a
  // perfectly working terminal.
  if (parent === $('termwrap')) $('termempty').hidden = !!target;
  if (entry) {
    requestAnimationFrame(() => {
      resize(entry);
      // A hidden xterm has no dimensions, so its renderer parks; coming back
      // does not always repaint what is already in the buffer, which is the
      // black pane you get from switching sessions quickly. Ask for the redraw
      // rather than hope for one.
      repaint(entry);
    });
  }
  return entry;
}

/** Re-fit every attached terminal. Lives with the terminals rather than with the
 *  zoom control, which is what stopped the two depending on each other. */
function refit() {
  for (const entry of terms.values()) resize(entry);
}

/** Apply a new UI scale here: xterm draws its own text, so its font is set
 *  rather than inherited, and a new glyph size means new rows and cols. */
function applyScale() {
  const px = termFontSize();
  for (const entry of terms.values()) {
    if (entry.term.options.fontSize !== px) entry.term.options.fontSize = px;
  }
  refit();
}

export { showTerm as show, closeTerm as close, refit, applyScale };
