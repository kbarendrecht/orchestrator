// The editable buffer: load a file, watch it, write it back.
//
// **Lifted out of `diff.js` because a second viewer wanted it.** It was the diff's
// right-hand pane and read `diffState` directly — which workspace, which path,
// which base revision — so the search viewer could not have opened it without
// pretending to be a diff. What actually differs between the two is small and is
// now the argument: where to mount, which file, and whether there is a base
// revision to sit beside.
//
// **One defect the move corrects.** The load asked about `diffState.ws`, pinned
// when the overlay opened, and the save posted `currentWorkspaceId()`, which
// follows the selection and answers `main` when nothing is selected. One `host`
// holds the workspace now, so a write can only ever land in the tree the read
// came from.
//
// What did not change: the version is taken at load, a poll notices an agent
// editing underneath you, and a mismatch is refused at the write. That is §5's
// invalidation, in the direction that loses work.

import { appMod, call, confirmBox, el, get, MOD_LABEL, reason, toast } from './core.js';
import { commentFor, indent, indentUnit, newline, toggleComment } from './editkeys.js';
import { hlTokens, langFor, paintRanges } from './source.js';

/** The chord is the same in both overlays, so the label is written once and from
 *  the platform's own modifier — a hardcoded glyph here was the Mac key
 *  everywhere. */
export const SAVE_LABEL = `Save ${MOD_LABEL} S`;

/** What an editor is opened against.
 *
 *  @typedef {{
 *    mount: HTMLElement,
 *    mountClass: string,
 *    workspace: string,
 *    path: string,
 *    base?: string | null,
 *    prBase?: string | null,
 *    save: HTMLElement,
 *    edit: HTMLElement,
 *    onClosed: () => void,
 *    onSaved?: () => void | Promise<void>,
 *  }} Host */

/** The open editor. `version` is what the buffer was loaded at, and `watch` is
 *  the poll that notices somebody editing the file underneath you.
 *
 *  @type {{ on: boolean, path: string | null, version: string | null,
 *           dirty: boolean, watch: ReturnType<typeof setInterval> | null,
 *           host: Host | null, mountWas: string | null }}
 */
export const state = {
  on: false,
  path: null,
  version: null,
  dirty: false,
  watch: null,
  host: null,
  /** The mount's own class, to put back when the buffer goes away.
   *
   *  **Restored here rather than by each caller**, which is what the diff had
   *  been doing by accident: its `onClosed` rebuilds the pane and reassigns the
   *  class on the way past, so nothing missed it until two overlays used this
   *  that only replace their children. Then `fnsrc editing` — `overflow:hidden`
   *  and a row-direction flex — stayed on the viewer for the life of the page,
   *  and the band's three children laid out as columns in a pane that no longer
   *  scrolled. */
  mountWas: null,
};

/** The `/api/file` query for whatever is open. One place, so the read, the poll
 *  and the write cannot disagree about which file they mean.
 *
 *  @param {Record<string, string>} extra */
function query(extra) {
  const h = state.host;
  const q = new URLSearchParams({ workspace: h?.workspace ?? '', path: h?.path ?? '', ...extra });
  if (h?.prBase) q.set('pr_base', h.prBase);
  return q;
}

/** Open `host.path` for editing.
 *
 *  @param {Host} host */
export async function open(host) {
  state.host = host;
  let live;
  /** @type {{ content: string } | null} */
  let base = null;
  try {
    // The base revision only for a caller that has one. The diff's left pane is
    // what this file was; a search result is a file, and often an unchanged one,
    // so there is nothing to sit beside.
    [live, base] = await Promise.all([
      get(`/api/file?${query({})}`),
      host.base ? get(`/api/file?${query({ base: host.base })}`) : Promise.resolve(null),
    ]);
  } catch (e) {
    state.host = null;
    return toast(reason(e), true);
  }

  state.on = true;
  state.path = host.path;
  state.version = live.version;
  state.dirty = false;
  host.save.hidden = false;
  host.save.textContent = SAVE_LABEL;
  host.edit.textContent = 'Cancel';

  const body = host.mount;
  body.replaceChildren();
  state.mountWas = body.className;
  body.className = host.mountClass;

  if (base) {
    // Read-only: this is an editable right pane, not a free-floating editor.
    const left = el('pre', 'editbase');
    left.textContent = base.content;
    body.appendChild(left);
    body.appendChild(el('div', 'gutter'));
  }

  const ta = el('textarea', 'editarea');
  ta.value = live.content;
  ta.spellcheck = false;
  ta.oninput = () => {
    state.dirty = true;
    host.save.textContent = 'Save •';
  };
  const lang = langFor(host.path);
  if (lang && live.content.length <= COLOUR_MAX) body.appendChild(colourUnder(ta, lang));
  else body.appendChild(ta);
  ta.focus();
  const unit = indentUnit(live.content);
  const mark = commentFor(lang);
  ta.onkeydown = (e) => {
    const plain = !e.ctrlKey && !e.altKey && !e.metaKey;
    /** @type {import('./editkeys.js').Edit | null} */
    let ed = null;
    if (e.key === 'Tab' && plain) {
      ed = indent(ta.value, ta.selectionStart, ta.selectionEnd, unit, e.shiftKey);
    } else if (e.key === 'Enter' && plain && !e.shiftKey) {
      ed = newline(ta.value, ta.selectionStart, ta.selectionEnd);
    } else if (e.key === '/' && appMod(e) && !e.shiftKey && !e.altKey && mark) {
      ed = toggleComment(ta.value, ta.selectionStart, ta.selectionEnd, mark);
    }
    if (!ed) return;
    e.preventDefault();
    replace(ta, ed);
  };

  // Invalidation: an agent editing the same file underneath you must not be
  // discovered only at save time (§5).
  clearInterval(state.watch ?? undefined);
  state.watch = setInterval(() => void checkUnderneath(), 4000);
}

async function checkUnderneath() {
  if (!state.on) return;
  try {
    const now = await get(`/api/file?${query({})}`);
    if (now.version !== state.version) {
      clearInterval(state.watch ?? undefined);
      state.watch = null;
      if (state.host) state.host.save.textContent = 'Save (conflict)';
      toast('this file changed on disk — an agent is editing it too. Saving will be refused.', true);
    }
  } catch (e) {
    // A file that vanished is also a change worth knowing about, but not worth
    // a second alarm; the save will report it.
  }
}

/** Put the editor away. `false` means the question was answered "keep editing".
 *
 *  @param {boolean} [silent] */
export async function close(silent) {
  // Async, and every caller awaits it: `confirm` blocked the thread, this does
  // not. Everything below has to stay after the answer, or the editor tears
  // itself down while the question about it is still on screen.
  if (state.on && state.dirty && !silent
      && !await confirmBox('Discard unsaved edits?', { ok: 'Discard' })) return false;
  const host = state.host;
  clearInterval(state.watch ?? undefined);
  state.watch = null;
  state.on = false;
  state.dirty = false;
  state.host = null;
  if (host) {
    host.save.hidden = true;
    host.save.textContent = SAVE_LABEL;
    host.edit.textContent = 'Edit';
    // Before `onClosed`, so a caller that redraws into the mount finds the class
    // it had rather than the editor's.
    if (state.mountWas !== null) host.mount.className = state.mountWas;
    state.mountWas = null;
    host.onClosed();
  }
  return true;
}

/** Write the buffer back, refused if the file moved underneath it. */
export async function save() {
  const host = state.host;
  if (!state.on || !host) return;
  const ta = host.mount.querySelector('.editarea');
  if (!ta) return;
  let out;
  try {
    out = await call('/api/file', {
      // The workspace the read came from, never the one that happens to be
      // selected now — see the note at the top of this file.
      workspace: host.workspace,
      path: state.path,
      content: /** @type {HTMLTextAreaElement} */ (ta).value,
      version: state.version,
    });
  } catch (e) {
    return toast(reason(e), true);
  }
  if (out.result === 'conflict') {
    return toast(
      'refused: the file changed on disk since you opened it. Cancel and reopen to see their version.',
      true);
  }
  state.version = out.version;
  state.dirty = false;
  host.save.textContent = SAVE_LABEL;
  toast('saved');
  await host.onSaved?.();
}

/** Past this a buffer is edited without colour, as the viewer draws it plain. */
const COLOUR_MAX = 512 * 1024;

/** The textarea, with the visible lines drawn in colour underneath it.
 *
 *  **A textarea cannot colour its own text**, and a `contenteditable` that could
 *  brings its own caret, undo and paste, none of them a textarea's. So the text
 *  stays a textarea's, drawn transparent, and a copy of the lines in view is
 *  painted behind it with the viewer's tokens. Only the visible band, per line,
 *  the way the viewer does it: the cost per keystroke is the screen, not the file.
 *  The two must share every metric that places a glyph, which is why both take
 *  their font, padding and tab size from one CSS rule. */
function colourUnder(/** @type {HTMLTextAreaElement} */ ta, /** @type {string} */ lang) {
  const wrap = el('div', 'editwrap');
  const under = el('pre', 'edithl');
  const inner = el('div', 'edithl-in');
  under.appendChild(inner);
  ta.classList.add('hl');
  wrap.append(under, ta);
  let queued = false;
  const paint = () => {
    queued = false;
    const cs = getComputedStyle(ta);
    const lh = parseFloat(cs.lineHeight) || 18;
    const top = parseFloat(cs.paddingTop) || 0;
    const first = Math.max(0, Math.floor((ta.scrollTop - top) / lh));
    const lines = ta.value.split('\n').slice(first, first + Math.ceil(ta.clientHeight / lh) + 2);
    inner.replaceChildren(...lines.map((l) => {
      const row = el('div');
      paintRanges(row, l || ' ', l ? hlTokens(l, lang) : []);
      return row;
    }));
    inner.style.transform = `translate(${-ta.scrollLeft}px, ${first * lh - ta.scrollTop}px)`;
  };
  const later = () => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(paint);
  };
  ta.addEventListener('input', later);
  ta.addEventListener('scroll', later);
  new ResizeObserver(later).observe(ta);
  later();
  return wrap;
}

/** Make one edit through the browser's own insert, so `Ctrl+Z` undoes it like
 *  typing. `execCommand` is deprecated and still the only way into a
 *  textarea's undo stack; where it is refused the edit lands without undo. */
function replace(/** @type {HTMLTextAreaElement} */ ta, /** @type {import('./editkeys.js').Edit} */ ed) {
  ta.setSelectionRange(ed.from, ed.to);
  if (!document.execCommand('insertText', false, ed.text)) {
    ta.setRangeText(ed.text, ed.from, ed.to, 'end');
    ta.dispatchEvent(new Event('input'));
  }
  ta.setSelectionRange(ed.selStart, ed.selEnd);
}

/** Put the caret at the start of a line and scroll it into the middle. */
export function goTo(/** @type {number} */ line) {
  const ta = /** @type {HTMLTextAreaElement | null | undefined} */ (
    state.host?.mount.querySelector('.editarea'));
  if (!state.on || !ta) return false;
  let at = 0;
  for (let n = 1; n < line; n++) {
    const next = ta.value.indexOf('\n', at);
    if (next < 0) break;
    at = next + 1;
  }
  ta.focus();
  ta.setSelectionRange(at, at);
  const lh = parseFloat(getComputedStyle(ta).lineHeight) || 18;
  ta.scrollTop = Math.max(0, (line - 1) * lh - ta.clientHeight / 2);
  return true;
}

/** Is anything open to save? The `Ctrl+S` binding is the app's and there are two
 *  overlays that can hold a buffer, so it asks here rather than naming one. */
export const isOpen = () => state.on;
