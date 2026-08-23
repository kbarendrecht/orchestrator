// The terminals: one xterm per session or process, attached to the daemon's pty
// over a websocket. The DOM renderer is deliberate in the webview — see CLAUDE.md.

import { $, CHROME, el, terms, toast, uiScale } from './core.js';


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

  const term = new Terminal({
    theme: THEME,
    fontFamily: "'IBM Plex Mono', ui-monospace, monospace",
    fontSize: termFontSize(),
    lineHeight: 1.25,
    cursorBlink: true,
    scrollback: 10000,
    allowProposedApi: true,
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(host);

  /* Ctrl+Shift+C / Ctrl+Shift+V, the terminal convention. xterm passes every
   * keystroke to the pty, so without this the copy shortcut reached the agent as
   * a control code and the selection stayed where it was. Plain Ctrl+C has to go
   * on reaching the pty: interrupting is what it means in a terminal.
   *
   * Returning false tells xterm not to handle the event itself. */
  term.attachCustomKeyEventHandler((e) => {
    if (e.type !== 'keydown' || !e.ctrlKey || !e.shiftKey) return true;
    const key = e.key.toLowerCase();
    if (key === 'c') {
      const text = term.getSelection();
      if (text) copyText(text);
      return false;
    }
    if (key === 'v') {
      navigator.clipboard.readText()
        .then((text) => { if (text) term.paste(text); })
        // Reading the clipboard needs a permission the webview does not grant,
        // and the raw `NotAllowedError` reads like a fault in the app rather than
        // a rule of the platform.
        .catch(() => toast('this window is not allowed to read the clipboard', true));
      return false;
    }
    return true;
  });
  /* No WebGL in the webview. WebKitGTK is the engine this app actually runs on,
   * and xterm's WebGL renderer garbles glyphs there: text arrives as noise and
   * comes back only when a scroll or a selection forces a full redraw, which is
   * the canvas being composited wrong rather than the buffer being wrong.
   * Repainting after every refit and dropping the addon on context loss both
   * failed to fix it, so the canvas goes instead: the DOM renderer draws real
   * text, which cannot garble. It is slower under heavy output, and that is the
   * trade.
   *
   * A browser tab is Chromium or Firefox, where the fast path is fine, so it
   * keeps it. `chrome` comes from the daemon, which is the side that knows
   * whether it is being shown in a window it owns. */
  if (CHROME === 'none') {
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

  const sock = new WebSocket(
    `${WS_BASE}/ws/pty?token=${encodeURIComponent(TOKEN)}&target=${encodeURIComponent(target)}`
  );
  sock.binaryType = 'arraybuffer';
  const entry = { term, fit, sock, host, ready: false };

  sock.onopen = () => {
    entry.ready = true;
    // A fresh socket knows nothing about the size, whatever the last one was told.
    entry.sent = null;
    entry.box = null;
    resize(entry);
    // A session you just created is selected before there is anything to type
    // into, so the focus `select` asked for landed on nothing. Take it once the
    // pty is actually attached, but only if this is still the session you are
    // in, or a slow one would steal the keyboard back later.
    if (terms.get(`session:${selected}`) === entry) {
      try {
        term.focus();
      } catch (e) { /* disposed while the socket was opening */ }
    }
  };
  sock.onmessage = (ev) => {
    if (typeof ev.data === 'string') term.write(ev.data);
    else term.write(new Uint8Array(ev.data));
  };
  sock.onclose = () => { entry.ready = false; };

  term.onData((d) => {
    if (sock.readyState === WebSocket.OPEN) sock.send(new TextEncoder().encode(d));
  });

  terms.set(target, entry);
  return entry;
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
  if (entry.ready && entry.sock.readyState === WebSocket.OPEN) {
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

export { showTerm as show, closeTerm as close, resize, termFontSize as fontSize, refit, applyScale };
