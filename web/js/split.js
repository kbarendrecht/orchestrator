// The session under an open diff or file (#52).
//
// **The session's own pane, not a copy.** `#termwrap` is where the session
// already runs, and `body.agentsplit` pins it to the bottom of the centre column
// with the overlay ending above it (app.css says how). The terminal is the same
// xterm, refitted by the `ResizeObserver` on `#termwrap` in app.js.
//
// **A split, not a card over the file.** The card it replaces floated over the
// file and covered part of it on one screen, which is the screen most people
// read a diff on.
//
// **Collapsed until it is needed.** Reading a diff wants the height, so every
// open starts at the session's header row, which still says what the session is
// doing. A send opens it, because that is when an answer is coming; it then stays
// open until it is collapsed.

import { $, activeCheckout, currentSession, toast } from './core.js';
import * as Term from './term.js';

/** The open height, dragged from the header and remembered like the drawer's.
 *  Nothing stored is the stylesheet's default, a share of the window. */
const HEIGHT = { prop: '--split-h', key: 'orch.agentSplit', min: 140, diffMin: 160 };

/** Which overlays have the split up: the diff and a file can both be open, and
 *  the split goes only once neither is. `open` is the one state the header
 *  toggles. */
const state = { owners: /** @type {Set<string>} */ (new Set()), on: false, open: false };

export const isOn = () => state.on;

function paint() {
  document.body.classList.toggle('agentsplit', state.on);
  document.body.classList.toggle('agentsplit-open', state.on && state.open);
  const fold = $('agentfold');
  fold.title = state.open ? 'Collapse to the session\'s header' : 'Show the conversation';
  fold.setAttribute('aria-expanded', String(state.open));
}

/** An overlay came up: the split is there, collapsed, when there is a live
 *  session to show. A second overlay over the first leaves it as it is.
 *  True for the first one, which is where a read starts. */
export function start(/** @type {string} */ owner) {
  const fresh = state.owners.size === 0;
  state.owners.add(owner);
  if (!fresh) return false;
  const s = currentSession();
  state.on = !!(s && s.alive);
  state.open = false;
  paint();
  return true;
}

/** An overlay went: the pane goes back to the column once none is left.
 *  True for the last one, which is where a read ends. */
export function end(/** @type {string} */ owner) {
  const had = state.owners.delete(owner);
  if (!had || state.owners.size) return false;
  state.on = false;
  state.open = false;
  paint();
  return true;
}

/** Open it and hand it the keyboard. False when there is no session to show. */
export function open() {
  if (!state.on) return false;
  state.open = true;
  paint();
  Term.focusSelected(activeCheckout());
  return true;
}

export function collapse() {
  if (!state.on) return;
  state.open = false;
  paint();
}

/** MOD ; and the header button. */
export function toggle() {
  if (!state.on) return toast('no session here to show', true);
  if (state.open) collapse();
  else open();
}

function setHeight(/** @type {number | null} */ px) {
  const root = document.documentElement;
  if (px === null) return root.style.removeProperty(HEIGHT.prop);
  const most = window.innerHeight - HEIGHT.diffMin;
  root.style.setProperty(HEIGHT.prop, `${Math.round(Math.max(HEIGHT.min, Math.min(px, most)))}px`);
}

/** Wire the header. Called once, at boot. */
export function init() {
  try {
    const kept = Number(localStorage.getItem(HEIGHT.key));
    if (kept) setHeight(kept);
    // The floating card's place, from before the split.
    localStorage.removeItem('orch.agentcard');
  } catch { /* private mode: the default height */ }

  $('agentfold').onclick = (ev) => {
    ev.stopPropagation();
    toggle();
  };
  /* **The header is the handle.** Open, a drag resizes from the top edge the way
     the drawer's does; collapsed, a click opens, since the bar is one line and a
     button that small is a hard target. */
  $('agenthead').addEventListener('mousedown', (ev) => {
    if (ev.button !== 0 || /** @type {HTMLElement} */ (ev.target).closest('button')) return;
    ev.preventDefault();
    if (!state.open) {
      open();
      return;
    }
    const y0 = ev.clientY;
    const from = $('termwrap').getBoundingClientRect().height;
    let moved = false;
    document.body.classList.add('row-resizing');
    const move = (/** @type {MouseEvent} */ e) => {
      if (!moved && Math.abs(e.clientY - y0) < 3) return;
      moved = true;
      setHeight(from + y0 - e.clientY);
    };
    window.addEventListener('mousemove', move);
    window.addEventListener('mouseup', () => {
      window.removeEventListener('mousemove', move);
      document.body.classList.remove('row-resizing');
      if (!moved) return;
      try {
        localStorage.setItem(HEIGHT.key, String(Math.round($('termwrap').getBoundingClientRect().height)));
      } catch { /* the drag still worked for this run */ }
      Term.refit();
    }, { once: true });
  });
  // A double-click puts the default height back.
  $('agenthead').addEventListener('dblclick', (ev) => {
    if (/** @type {HTMLElement} */ (ev.target).closest('button') || !state.open) return;
    setHeight(null);
    try {
      localStorage.removeItem(HEIGHT.key);
    } catch { /* nothing to forget */ }
  });
}
