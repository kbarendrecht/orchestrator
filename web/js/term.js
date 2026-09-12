// The terminals: one xterm per session or process, attached to the daemon's pty
// over a websocket. The DOM renderer is deliberate under WebKitGTK, and only
// there — see the renderer comment below, and CLAUDE.md.

import { $, CHROME, IS_MAC, copyText, fontStack, theme, el, mark, note, reason, reportBoot, selected, terms, termKey, typingElsewhere, uiScale, wheelScale } from './core.js';
import { termColours } from './palette.js';


const THEME = {
  background: '#101010', foreground: '#D2D2D2', cursor: '#D2D2D2',
  black: '#101010', red: '#C9615A', green: '#5FA97C', yellow: '#E0A244',
  blue: '#4C9AAF', magenta: '#9A7AA0', cyan: '#3E9AAF', white: '#D2D2D2',
  brightBlack: '#5B5B5B', brightRed: '#D6756E', brightGreen: '#74BB90',
  brightYellow: '#EDB55C', brightBlue: '#63AEC2', brightMagenta: '#B08FB6',
  brightCyan: '#57AEC2', brightWhite: '#F0F0F0',
  selectionBackground: '#2C2C2C',
};

/** A client that watches without reshaping what it watches.
 *
 *  `mise run shot` loads this same SPA to photograph it, and fitting a terminal
 *  is what a browser at some other size does on the way — which resizes the pty
 *  of a session somebody is using. A screenshot has no business doing that, so it
 *  asks for `?observe`, and this stops the resize being sent. The pane still fits
 *  itself, so the picture is of a laid-out terminal; it just shows the geometry
 *  the real window chose. Never set in the desktop window. */
const OBSERVE = new URLSearchParams(location.search).has('observe');

/** The terminal's font in px. xterm draws its own text, so the stylesheet's
 *  multiplier cannot reach it; this applies the same factor natively, which is
 *  also why it stays crisp.
 *
 *  **A base of its own, still multiplied by the UI scale.** The base is the
 *  theme's, so the pane you stare at can be bigger than the chrome around it; the
 *  multiplier stays so `MOD +` keeps zooming the whole board together rather than
 *  everything except the terminal. 12 was the constant this replaces, and it is
 *  still the default. */
const termFontSize = () => Math.round(theme.termSize * uiScale());

/** Attach to a pty, replaying the daemon's buffer first.
 *
 *  @param {import('./core.js').Target} checkout which daemon holds the pty
 *  @param {string} target the wire target, as that daemon names it
 *  @param {HTMLElement} parent
 */
function openTerm(checkout, target, parent) {
  const key = termKey(checkout, target);
  if (terms.has(key)) return terms.get(key);

  const host = el('div', 'termhost');
  parent.appendChild(host);

  /* Whether an agent is on the other end of this pty, which decides one binding
     below. The daemon's own naming answers it: sessions are keyed by id, every
     process and shell by its workspace. */
  const agentPane = target.startsWith('session:');

  const term = new Terminal({
    theme: termColours(theme),
    // The theme's, not a literal: the terminal is the pane you read most, so a
    // font choice that skipped it would be a choice about labels.
    fontFamily: fontStack('mono'),
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

  // Declared before the key and wheel handlers below so they can send through
  // `entry.sock`, which `connect` replaces on a reconnect. Closing over the socket
  // directly is what pinned them to the first, dead one (#7).
  /* `checkout` and `key` are built in rather than assigned after: they are the
     two fields every reconnect reads, and an entry without them is not usable. */
  /** @type {import('./core.js').TermEntry} */
  const entry = { term, fit, host, checkout, key, pending: [], pendingBytes: 0, queued: [], queuedBytes: 0 };

  /* The pty-status pill: `connecting…` until the socket is up, `starting…` until
     there is something readable on the pane, `reconnecting…` if an open socket
     later drops. Built here so it exists before the socket does.

     `somethingOnScreen` is the test, and the bar is deliberately that high: bytes
     arriving is not the pane saying anything.

     **It used to come down when the socket opened**, which is a local connection
     and therefore instant — while the thing you are waiting for is the agent,
     which on a resume is several seconds of Claude Code booting. So the pill
     flashed and the pane then sat empty with a blinking cursor, saying nothing,
     for exactly the wait it was built to explain. */
  const badge = el('div', 'term-badge');
  badge.appendChild(el('span', 'conn-dot'));
  badge.appendChild(el('span', 'term-badge-t'));
  host.appendChild(badge);
  entry.badge = badge;
  setBadge(entry, 'connecting');

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
  term.attachCustomKeyEventHandler((/** @type {KeyboardEvent} */ e) => {
    if (e.type !== 'keydown') return true;
    /* **Shift+Enter is a newline, and a terminal cannot say so by itself.** Enter
       and Shift+Enter both leave xterm as a bare CR — measured here with `cat -v`,
       which showed no escape at all — so Claude Code has nothing to tell them
       apart by and submits the prompt instead of breaking the line. Its own
       `/terminal-setup` fixes this in iTerm2 and VS Code by binding the chord to
       ESC then CR; this is the same binding, made here so it works out of the box
       and needs no setup command run against a terminal the user does not own.

       Shift alone: with Ctrl, ⌘ or Alt held it is a different chord and belongs to
       whoever claims it. The newline goes through `sendInput`, so a socket down
       for a reconnect banks it rather than dropping it like the old direct send.

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
      sendInput(entry, new TextEncoder().encode('\x1b\r'));
      return false;
    }
    const combo = IS_MAC ? e.metaKey && !e.ctrlKey : e.ctrlKey && e.shiftKey;
    if (!combo) return true;
    const key = e.key.toLowerCase();
    if (key === 'c') {
      const text = term.getSelection();
      if (text) void copyText(text);
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
   * **The macOS experiment was settled, and WKWebView does garble** (#8): on a
   * Retina Mac the agent pane's glyphs turn to noise, cell by cell, and the
   * trigger is a *second* terminal writing while the pane repaints — a drawer
   * shell running `git status` was enough. Worse than the WebKitGTK case, because
   * a scroll does not clean it up: the repaint comes from the same corrupted
   * atlas.
   *
   * **So macOS gets WebGL for the agent pane only.** That is the narrower of the
   * two fixes the report offered, and it is narrower in the direction that
   * matters: dropping `IS_MAC` outright would bring back the typing lag the flag
   * exists to avoid, on the very pane you type into. Keeping one live context per
   * window instead removes the trigger — a single terminal never garbled — and
   * `CLAUDE.md` already named "a context to the visible terminal only" as the
   * answer for a neighbouring case.
   *
   * A **browser tab keeps WebGL for every terminal**, deliberately: Chromium and
   * Firefox have no such fault, and a drawer shell streaming a build log is
   * exactly where the canvas earns its keep. The cost of the macOS rule is that
   * such a shell falls back to the DOM renderer there, which is the trade the
   * corruption forces.
   *
   * Not the *scrolling* complaint, which was the other suspect this was held
   * against: that turned out to be xterm damping sub-50px wheel deltas, measured
   * separately, and no renderer would have changed it. */
  const webglWanted = CHROME === 'none' || (IS_MAC && agentPane);
  // Named for the log line below: a bug report could not say which renderer it was
  // on, and on a packaged app there is no console to ask.
  const engine = CHROME === 'none' ? 'browser' : IS_MAC ? 'wkwebview' : 'webkitgtk';
  let renderer = 'dom';
  if (webglWanted) {
    try {
      const webgl = new WebglAddon.WebglAddon();
      // A lost context leaves the canvas frozen on whatever it last painted, and
      // nothing in xterm notices. Dropping the addon puts the DOM renderer back.
      webgl.onContextLoss?.(() => {
        note(`${target}: webgl context lost, falling back to the dom renderer`);
        webgl.dispose();
      });
      term.loadAddon(webgl);
      renderer = 'webgl';
    } catch (e) {
      // Software rendering is slower but correct; not worth failing over.
      note(`${target}: webgl refused (${reason(e)}), using the dom renderer`);
    }
  }
  note(`${target} renderer=${renderer} engine=${engine}`);

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
  term.attachCustomWheelEventHandler((/** @type {WheelEvent} */ ev) => {
    if (!agentPane || ev.shiftKey || ev.deltaMode !== 0) return true;
    // `modes` is public API. `any`/`drag` is `?1003h`/`?1002h`, which is what an
    // agent TUI sets; anything less capable is left alone rather than guessed at,
    // and `none` means xterm is about to scroll its own buffer, which is correct.
    const tracking = term.modes?.mouseTrackingMode;
    if (tracking !== 'any' && tracking !== 'drag') return true;

    const box = host.getBoundingClientRect();
    const cell = box.height / term.rows;
    if (!(cell > 0)) return true;
    /* **The scale goes on the pixel delta, before it becomes lines.** Which is the
       one thing `scrollSensitivity` gets wrong (see the dead end named above): it
       multiplies before a threshold test that reads the *raw* delta, so a value
       that suits a mouse breaks a trackpad. Here there is no threshold left to
       fool — the damping is gone — so a plain multiplier is honest, and at the
       default of 1 this line is what it always was.
       Why anyone would change it: macOS reports an accelerated wheel notch as a
       ~180px delta with `deltaMode === 0`, so it never takes the line-mode escape
       above, and at a ~15px cell that is about twelve lines for one notch. */
    wheelLines += (ev.deltaY * wheelScale) / cell;
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
    // A wheel report is transient state, not typing: dropping it while the socket
    // is down is right, so this does not bank the way `sendInput` does.
    if (entry.sock && entry.sock.readyState === WebSocket.OPEN) {
      entry.sock.send(new TextEncoder().encode(`\x1b[<${button};${col};${row}M`.repeat(count)));
    }
    return false;
  });

  term.onData((/** @type {string} */ d) => sendInput(entry, new TextEncoder().encode(d)));

  /* **The checkout is taken here, at open, and never read from a global again.**
     `connect` is also the reconnect path, so a socket that read "the checkout you
     are in now" would re-aim itself at whichever checkout you happened to be
     looking at when the network blipped — and never heal, because nothing
     re-opens a terminal that is working. */
  terms.set(key, entry);
  connect(entry, target);
  return entry;
}

/** Open (or reopen) the pty socket for an entry, replaying the daemon buffer.
 *
 *  Split out of `openTerm` so a dropped socket can be replaced in place. The pty
 *  survives on the daemon — `ws.rs` detaches the client, never the process — and a
 *  reattach costs one ring-buffer replay and lands the pane where it was. Without
 *  a reconnect a closed socket stayed closed, and every keystroke took the false
 *  branch and vanished while the cursor kept blinking on xterm's own buffer (#7). */
function connect(/** @type {import('./core.js').TermEntry} */ entry, /** @type {string} */ target) {
  const { wsBase, token } = entry.checkout;
  const sock = new WebSocket(
    `${wsBase}/ws/pty?token=${encodeURIComponent(token)}&target=${encodeURIComponent(target)}`
  );
  sock.binaryType = 'arraybuffer';
  entry.sock = sock;

  sock.onopen = () => {
    // Back to healthy: clear the backoff and the "reconnecting" mark, then flush
    // anything typed while the socket was down.
    entry.backoff = 0;
    setBadge(entry, 'starting');
    // A reattach replays the *whole* ring buffer, exactly as a first attach does —
    // but this terminal already holds the previous buffer, so writing the replay on
    // top would show the scrollback twice. Clear it before the replay lands, so a
    // reconnect reconstructs the pane rather than doubling it. Not on the first
    // open: xterm is empty there, and resetting before the snapshot arrives would
    // flash. Consumed by `writeChunk` at the first write or queue-flush after this.
    if (entry.everOpened) entry.needsReset = true;
    entry.everOpened = true;
    // The centre pane's two halves, and they fail separately: `attach` is the
    // pty being there at all, `paint` is the daemon's replay arriving. A gap
    // between them is the ring buffer being written into a DOM renderer; a long
    // `attach` is the session not having been spawned yet.
    mark('attach');
    // A fresh socket knows nothing about the size, whatever the last one was told.
    entry.sent = null;
    entry.box = null;
    resize(entry);
    flushInput(entry);
    // A session you just created is selected before there is anything to type
    // into, so the focus `select` asked for landed on nothing. Take it once the
    // pty is actually attached, but only if this is still the session you are
    // in, or a slow one would steal the keyboard back later — and never out of a
    // box you are typing in, which is how a rename in the rail lost the keyboard
    // mid-word and committed what had been typed so far.
    if (selected && terms.get(termKey(entry.checkout, `session:${selected}`)) === entry
        && !typingElsewhere()) {
      try {
        entry.term.focus();
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
    if (entry.host.hidden) return queueChunk(entry, chunk);
    writeChunk(entry, chunk);
    mark('paint');
    reportBoot();
  };
  sock.onclose = () => {
    // A deliberate teardown, or a session that has left the snapshot: `closeTerm`
    // disposes the entry, so reconnecting here would race it into reattaching a pty
    // that is gone — `resolve` would 404 and this would just flap.
    if (entry.closed || terms.get(entry.key) !== entry) return;
    // Mark the pane so a deaf terminal is not silent, then reconnect with backoff
    // the way the events socket does. The replay makes a reattach indistinguishable
    // from a first attach, so the pane heals itself on wake from sleep or a blip.
    setBadge(entry, 'reconnecting');
    const wait = Math.min(600 * 2 ** (entry.backoff || 0), 10000);
    entry.backoff = (entry.backoff || 0) + 1;
    entry.reconnectTimer = setTimeout(() => {
      if (!entry.closed && terms.get(entry.key) === entry) connect(entry, target);
    }, wait);
  };
}

/** How much typed input to bank while the pty socket is down, before dropping it.
 *
 *  A dropped keystroke under a blinking cursor is the worst of the outcomes in #7:
 *  the pane looks alive and silently eats what you type. Banking is bounded by
 *  human typing speed in the reconnect window, which is tiny — but a paste can be
 *  large, so cap it and let the `.detached` mark stand rather than grow forever. */
const INPUT_BUDGET = 1 << 16; // 64 KB

/** Show the pane's pty status, or hide it once the pty is live.
 *
 *  `'starting'` before the first attach, `'reconnecting'` after an open socket
 *  drops, `null` when it is carrying output. One pill for both, the connbar's, so
 *  a pane that is not live never reads as one that is. */
const BADGE = {
  connecting: 'connecting…',
  starting: 'starting…',
  reconnecting: 'reconnecting…',
};

function setBadge(/** @type {import('./core.js').TermEntry} */ entry, /** @type {'connecting' | 'starting' | 'reconnecting' | null} */ state) {
  const b = entry.badge;
  if (!b) return;
  /* **Nothing to blink at while nothing is attached.** A cursor on an empty pane
     reads as a live prompt ignoring what you type, which is the opposite of what
     it is. Hidden through the theme rather than a CSS rule, because the DOM and
     the WebGL renderer draw the cursor in different places and the option is the
     one lever that reaches both. */
  entry.term.options.theme = state ? { ...THEME, cursor: THEME.background } : THEME;
  if (!state) { b.hidden = true; return; }
  const t = b.querySelector('.term-badge-t');
  if (t) t.textContent = (state && BADGE[state]) || BADGE.connecting;
  b.hidden = false;
}

/** Send a keystroke, or bank it if the socket is down so nothing is lost silently. */
function sendInput(/** @type {import('./core.js').TermEntry} */ entry, /** @type {Uint8Array} */ bytes) {
  if (entry.sock && entry.sock.readyState === WebSocket.OPEN) {
    entry.sock.send(bytes);
    return;
  }
  // Over budget: drop, and leave the mark up — a truncated command replayed is
  // worse than one the user retypes against a pane that says it was not live.
  if (entry.pendingBytes + bytes.byteLength > INPUT_BUDGET) return;
  entry.pending.push(bytes);
  entry.pendingBytes += bytes.byteLength;
}

/** Replay everything typed while the socket was down, in order. */
function flushInput(/** @type {import('./core.js').TermEntry} */ entry) {
  const pending = entry.pending;
  entry.pending = [];
  entry.pendingBytes = 0;
  if (!pending?.length || entry.sock?.readyState !== WebSocket.OPEN) return;
  for (const b of pending) entry.sock.send(b);
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
function queueChunk(/** @type {import('./core.js').TermEntry} */ entry, /** @type {string | Uint8Array} */ chunk) {
  if (!entry.queued) { entry.queued = []; entry.queuedBytes = 0; }
  entry.queued.push(chunk);
  entry.queuedBytes += typeof chunk === 'string' ? chunk.length : chunk.byteLength;
  while (entry.queuedBytes > HIDDEN_BUDGET && entry.queued.length > 1) {
    const old = entry.queued.shift();
    if (old === undefined) break;
    entry.queuedBytes -= typeof old === 'string' ? old.length : old.byteLength;
  }
}

/** Write what arrived while this terminal was hidden, in the order it arrived. */
function flushQueued(/** @type {import('./core.js').TermEntry} */ entry) {
  if (!entry.queued?.length) return;
  const queued = entry.queued;
  entry.queued = [];
  entry.queuedBytes = 0;
  for (const chunk of queued) writeChunk(entry, chunk);
}

/** Write a chunk, first clearing the terminal once if a reconnect is about to
 *  replay the whole buffer on top of the old one. Both the live and the
 *  hidden-then-flushed paths go through here so the reset happens exactly once,
 *  before the replay, whichever arrives first. */
function writeChunk(/** @type {import('./core.js').TermEntry} */ entry, /** @type {string | Uint8Array} */ chunk) {
  if (entry.needsReset) { entry.needsReset = false; entry.term.reset(); }
  /* The callback, not the call: xterm parses asynchronously, so the buffer holds
     nothing yet on the line after `write`. */
  entry.term.write(chunk, () => {
    if (entry.closed || !entry.badge || entry.badge.hidden) return;
    if (somethingOnScreen(entry.term)) setBadge(entry, null);
  });
}

/** Is there anything readable on the pane, as opposed to bytes having arrived?
 *
 *  **The pill used to come down on the first message**, which is the same mistake
 *  as taking it down when the socket opened, one layer in: a resuming agent's
 *  first bytes are terminal setup — alt screen, clear, cursor moves — and they
 *  land in milliseconds, seconds before Claude Code draws anything. So the pill
 *  flashed and the pane sat blank for exactly the wait it exists to explain.
 *
 *  The viewport rather than the whole buffer, because that is what the pane shows,
 *  and only while the pill is up, so the cost is bounded to the wait itself. */
function somethingOnScreen(/** @type {any} */ term) {
  const buf = term.buffer.active;
  for (let i = 0; i < term.rows; i++) {
    const line = buf.getLine(buf.viewportY + i);
    if (line && line.translateToString(true).trim()) return true;
  }
  return false;
}

/* `force` re-states the geometry even when nothing here has moved.
 *
 * **A pty has one size and any number of clients, so the last one to speak wins
 * and the others are never told.** `mise run shot` drives the real SPA in a
 * headless browser, so a screenshot fitted a live session to a 1440x900 viewport
 * and left it there: the agent pane in the window went on painting at about half
 * its width and height, with nothing in the app aware of it. The caches below are
 * what made it permanent — the host box had not moved and the geometry matched
 * what this client last sent, so every later refit returned at the first line.
 *
 * There is no message that says "somebody else resized this", and adding one
 * would make the losers fight over the size. So the rule is instead: the client
 * you are looking at re-states its own geometry when you come back to it (window
 * focus, and switching to the pane). A same-size resize is a no-op in the daemon
 * — `PtyHandle::resize` says why — so re-stating costs nothing when nothing
 * drifted. */
function resize(/** @type {import('./core.js').TermEntry} */ entry, /** @type {boolean | undefined} */ force) {
  if (!entry || entry.host.hidden) return;
  // Nothing moved, nothing to do. Without this a repeated observation refits at
  // the same size, and a box whose width lands between two whole cells can flip
  // the answer back and forth — which reads as the terminal resizing itself.
  const box = entry.host.getBoundingClientRect();
  const seen = `${Math.round(box.width)}x${Math.round(box.height)}`;
  if (entry.box === seen && !force) return;
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
  if (entry.sent && entry.sent.rows === rows && entry.sent.cols === cols && !force) return;
  entry.sent = { rows, cols };
  // An observer never reshapes the session it is watching: see `OBSERVE`.
  if (OBSERVE) return;
  if (entry.sock?.readyState === WebSocket.OPEN) {
    entry.sock.send(JSON.stringify({ type: 'resize', rows, cols }));
  }
}

/** Redraw a terminal from its buffer, glyph atlas and all.
 *
 *  Dropping the atlas is the half that matters after a resize or a spell hidden:
 *  it is the piece that survives the canvas being sized to something else, and
 *  it is what the leftover garbage is made of. */
function repaint(/** @type {import('./core.js').TermEntry} */ entry) {
  requestAnimationFrame(() => {
    try {
      entry.term.clearTextureAtlas?.();
      entry.term.refresh(0, Math.max(0, entry.term.rows - 1));
    } catch (e) { /* a disposed terminal has nothing to refresh */ }
  });
}

function closeTerm(/** @type {import('../snapshot').Checkout} */ checkout, /** @type {string} */ target) {
  const entry = terms.get(termKey(checkout, target));
  if (!entry) return;
  // Mark it torn down before closing, so the socket's `onclose` does not read a
  // deliberate close as a drop and schedule a reconnect against a gone pty.
  entry.closed = true;
  if (entry.reconnectTimer) clearTimeout(entry.reconnectTimer);
  try { entry.sock?.close(); } catch (e) { /* already gone */ }
  entry.term.dispose();
  entry.host.remove();
  terms.delete(entry.key);
}

/** Tab switch replays the daemon buffer; it never respawns (§9).
 *
 *  @param {import('./core.js').Target | null} checkout
 *  @param {string | null} target
 *  @param {HTMLElement} parent
 */
function showTerm(checkout, target, parent) {
  const key = checkout && target ? termKey(checkout, target) : null;
  const entry = checkout && target ? openTerm(checkout, target, parent) : null;
  for (const [held, e] of terms) {
    if (e.host.parentElement !== parent) continue;
    e.host.hidden = held !== key;
    // Everything that arrived while it was away, before the repaint below asks
    // xterm what it holds.
    if (!e.host.hidden) flushQueued(e);
  }
  // Only the centre pane owns the empty state. Without this guard, every
  // drawer render un-hides it and "No session selected" sits on top of a
  // perfectly working terminal.
  if (parent === $('termwrap')) $('termempty').hidden = !!key;
  if (entry) {
    requestAnimationFrame(() => {
      // Forced: switching to a pane is one of the two moments this client takes
      // the geometry back off whoever changed it last.
      resize(entry, true);
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
function refit(/** @type {boolean | undefined} */ force) {
  for (const entry of terms.values()) resize(entry, force);
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

/** Repaint every terminal in the theme's colours, font and size.
 *
 *  **`refit(true)`, and the `true` is the whole point.** `resize` returns early
 *  when the host's box has not moved, and a font change does not move it — the
 *  host is `position:absolute;inset:0`. So an unforced refit is a no-op, the cell
 *  metrics change, and the pty is told a column count that no longer matches what
 *  is drawn. The branch this comes from called the unforced one under a comment
 *  describing exactly that bug.
 */
function applyTermTheme() {
  const px = termFontSize();
  const colours = termColours(theme);
  const family = fontStack('mono');
  for (const entry of terms.values()) {
    entry.term.options.theme = colours;
    entry.term.options.fontFamily = family;
    entry.term.options.fontSize = px;
  }
  refit(true);
}

/** What a pane is showing, as text somebody could read: the selection if there is
 *  one, else the last `lines` non-empty rows.
 *
 *  **Read out of xterm rather than out of the daemon's ring buffer**, for two
 *  reasons that point the same way. The useful payload is usually a *selection*,
 *  and only the pane knows what you highlighted. And xterm has already parsed the
 *  stream, so what it holds is text — no escape sequences, no half-written line,
 *  no cursor moves to strip.
 *
 *  `null` when the pane is not attached or holds nothing. Blank rows are dropped
 *  from the tail because a watcher that has been idle leaves the screen padded,
 *  and 50 rows of nothing is not what you meant to send.
 */
function readTerm(/** @type {import('../snapshot').Checkout} */ checkout, /** @type {string} */ target, lines = 50) {
  const entry = terms.get(termKey(checkout, target));
  if (!entry) return null;
  const picked = entry.term.getSelection();
  if (picked && picked.trim()) return picked.replace(/\s+$/, '');
  const buf = entry.term.buffer.active;
  const out = [];
  // Backwards from the last row, so the *end* of the output is what survives the
  // bound — a build error is at the bottom.
  for (let y = buf.length - 1; y >= 0 && out.length < lines; y--) {
    const row = buf.getLine(y);
    if (!row) continue;
    const text = row.translateToString(true).replace(/\s+$/, '');
    if (!text && !out.length) continue;   // trailing blanks, not content
    out.push(text);
  }
  const text = out.reverse().join('\n').replace(/^\n+|\n+$/g, '');
  return text || null;
}

/** Whether this pane has a selection, which is what the menu's wording turns on. */
function hasSelection(/** @type {import('../snapshot').Checkout} */ checkout, /** @type {string} */ target) {
  const entry = terms.get(termKey(checkout, target));
  return !!entry && !!entry.term.getSelection().trim();
}

export {
  showTerm as show, closeTerm as close, refit, applyScale, applyTermTheme, readTerm,
  hasSelection,
};
