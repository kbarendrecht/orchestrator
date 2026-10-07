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

import { appMod, call, confirmBox, el, get, IS_MAC, MOD_LABEL, reason, toast } from './core.js';
import { commentFor, indent, indentUnit, newline, toggleComment } from './editkeys.js';
import { hasMarkers, merge3 } from './merge.js';
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
 *  `base` is the text that version had, which is what a merge measures both
 *  sides from when the file moves underneath.
 *
 *  @type {{ on: boolean, path: string | null, version: string | null,
 *           dirty: boolean, watch: ReturnType<typeof setInterval> | null,
 *           host: Host | null, mountWas: string | null, base: string | null }}
 */
export const state = {
  on: false,
  path: null,
  version: null,
  base: null,
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
  state.base = live.content;
  state.dirty = false;
  host.save.hidden = false;
  host.save.textContent = SAVE_LABEL;
  host.edit.textContent = 'Cancel';

  const body = host.mount;
  body.replaceChildren();
  /* **The scroll goes with the rows.** The mount is the pane the viewer or the
     diff was scrolling, and an element keeps its offset when its children are
     swapped: the buffer sat at the top of a pane still scrolled to line 400, so
     Edit showed an empty pane. The caller puts the caret back where you were. */
  body.scrollTop = 0;
  body.scrollLeft = 0;
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
  body.appendChild(decorate(ta, lang && live.content.length <= COLOUR_MAX ? lang : null));
  ta.focus();
  const unit = indentUnit(live.content);
  const mark = commentFor(lang);
  ta.onkeydown = (e) => {
    /* **Undo is ours to bind on Linux.** WebKitGTK keeps a textarea's undo
       history and answers `execCommand('undo')`, but binds no key to it: Ctrl+Z
       did nothing at all, measured. Not on a Mac, where ⌘Z reaches the buffer
       through the app's Edit menu and binding it here as well would undo twice. */
    if (!IS_MAC && e.ctrlKey && !e.altKey && !e.metaKey && (e.key === 'z' || e.key === 'Z' || e.key === 'y')) {
      e.preventDefault();
      document.execCommand(e.key === 'y' || e.shiftKey ? 'redo' : 'undo');
      return;
    }
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
    if (now.version !== state.version) catchUp(now);
  } catch (e) {
    // A file that vanished is also a change worth knowing about, but not worth
    // a second alarm; the save will report it.
  }
}

/** Take in a version of the file somebody else wrote.
 *
 *  **It used to stop at a warning**, and the only way on was to cancel, lose
 *  your typing or theirs, and reopen. An agent editing the file you have open is
 *  the normal case here, not the rare one. So a buffer nobody typed in simply
 *  follows the file, and a buffer you did type in gets their change merged in by
 *  line: different lines both land, the same lines two ways are marked in the
 *  buffer for you, and Save refuses until the markers are gone.
 *
 *  @param {{ content: string, version: string }} now */
function catchUp(now) {
  const host = state.host;
  const ta = /** @type {HTMLTextAreaElement | null | undefined} */ (host?.mount.querySelector('.editarea'));
  if (!state.on || !host || !ta) return;
  const merged = state.dirty
    ? merge3(state.base ?? '', ta.value, now.content)
    : { text: now.content, conflicts: 0 };
  if (!merged) {
    host.save.textContent = 'Save (conflict)';
    toast('this file changed on disk and is too far from your buffer to merge here. Saving will be refused.', true);
    return;
  }
  const caret = Math.min(ta.selectionStart, merged.text.length);
  const top = ta.scrollTop;
  const wasDirty = state.dirty;
  replace(ta, { from: 0, to: ta.value.length, text: merged.text, selStart: caret, selEnd: caret });
  ta.scrollTop = top;
  state.version = now.version;
  state.base = now.content;
  state.dirty = wasDirty;
  host.save.textContent = wasDirty ? 'Save •' : SAVE_LABEL;
  if (!wasDirty) return toast('the file changed on disk, the buffer follows it');
  if (merged.conflicts) {
    return toast(`the file changed on disk: ${merged.conflicts} conflict${merged.conflicts === 1 ? '' : 's'} marked in your buffer, fix and save`, true);
  }
  toast('the file changed on disk, their change is merged into your buffer');
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
  const content = /** @type {HTMLTextAreaElement} */ (ta).value;
  if (hasMarkers(content)) return toast('refused: the buffer still has conflict markers in it', true);
  let out;
  try {
    out = await call('/api/file', {
      // The workspace the read came from, never the one that happens to be
      // selected now — see the note at the top of this file.
      workspace: host.workspace,
      path: state.path,
      content,
      version: state.version,
    });
  } catch (e) {
    return toast(reason(e), true);
  }
  if (out.result === 'conflict') {
    // Not saved: their change goes into the buffer first, and you look again.
    try {
      catchUp(await get(`/api/file?${query({})}`));
    } catch (e) {
      toast(reason(e), true);
    }
    return;
  }
  state.version = out.version;
  state.base = content;
  state.dirty = false;
  host.save.textContent = SAVE_LABEL;
  toast('saved');
  await host.onSaved?.();
}

/** Past this a buffer is edited without colour, as the viewer draws it plain. */
const COLOUR_MAX = 512 * 1024;

/** The textarea, with a line-number gutter beside it and — when the file is one
 *  that can be coloured — the visible lines drawn in colour underneath it.
 *
 *  **A textarea cannot colour its own text or number its own lines**, and a
 *  `contenteditable` that could brings its own caret, undo and paste, none of
 *  them a textarea's. So the text stays a textarea's and both decorations are
 *  layers behind it: only the visible band, per line, the way the viewer does it,
 *  so the cost per keystroke is the screen rather than the file. Every layer must
 *  share each metric that places a glyph, which is why they take their font,
 *  padding and tab size from one CSS rule.
 *
 *  **The wrap is built for every file, which it was not.** A file with no grammar
 *  — or one past the colour cap — used to mount the bare textarea, so the viewer's
 *  numbered rows became an unnumbered block the moment you pressed Edit. A line
 *  number is not a colour: it is how you say *which* line to an agent, and the
 *  app's own go-to-line lands on one.
 *
 *  @param {HTMLTextAreaElement} ta
 *  @param {string | null} lang null for a file drawn plain */
function decorate(ta, lang) {
  const wrap = el('div', 'editwrap');
  const nums = el('pre', 'editnums');
  const numsIn = el('div', 'editnums-in');
  nums.appendChild(numsIn);
  wrap.appendChild(nums);
  /** @type {HTMLElement | null} */
  let inner = null;
  if (lang) {
    const under = el('pre', 'edithl');
    inner = el('div', 'edithl-in');
    under.appendChild(inner);
    ta.classList.add('hl');
    wrap.appendChild(under);
  }
  wrap.appendChild(ta);
  let queued = false;
  const paint = () => {
    queued = false;
    /* **A whole pixel per line, or the layers drift apart in WebKit.** The CSS
       says 1.65, which is 18.975px at the default size, and that is what
       `getComputedStyle` reports; but WebKitGTK lays every line out 18px apart,
       textarea and copies alike. So a band placed at `first * 18.975` sat a pixel
       lower per line above it, and thirty lines down the selection was a line off
       the text it selected. A rounded height is one both the layout and this
       arithmetic agree on, in every engine. Set on every paint, since the zoom
       slider moves the font size. */
    const size = parseFloat(getComputedStyle(ta).fontSize) || 11.5;
    // Down, as WebKit lays the viewer's own 1.65 out: rounding up put each line a
    // pixel lower than the rows it replaced.
    wrap.style.setProperty('--edit-lh', `${Math.max(1, Math.floor(size * 1.65))}px`);
    const cs = getComputedStyle(ta);
    const lh = parseFloat(cs.lineHeight) || 18;
    const top = parseFloat(cs.paddingTop) || 0;
    const first = Math.max(0, Math.floor((ta.scrollTop - top) / lh));
    const all = ta.value.split('\n');
    const lines = all.slice(first, first + Math.ceil(ta.clientHeight / lh) + 2);
    /* Wide enough for the last line in the file, not for the ones on screen: a
       gutter that grew as you scrolled would shift every glyph in the buffer. */
    wrap.style.setProperty('--gut-digits', `calc(${String(all.length).length}ch + 12px)`);
    // Drawn rather than written, as the viewer's numbers are: a `user-select`
    // rule still lets a selection across the gutter carry the numbers into what
    // you paste, and generated content is not in the document to be taken.
    numsIn.replaceChildren(...lines.map((l, i) => {
      const row = el('div');
      row.dataset.n = String(first + i + 1);
      return row;
    }));
    numsIn.style.transform = `translateY(${first * lh - ta.scrollTop}px)`;
    if (!inner) return;
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

/** The textarea the buffer lives in, or nothing when none is open. */
const area = () => /** @type {HTMLTextAreaElement | null | undefined} */ (
  state.host?.mount.querySelector('.editarea'));

/** What is in the buffer now, for a search over it. Null when none is open.
 *
 *  **The buffer, not the file**: typing is not on disk, so a search that asked
 *  the daemon would answer about a version of the file nobody is looking at. */
export const text = () => (state.on ? area()?.value ?? null : null);

/** The caret, so a search knows where "next" starts from. */
export const caret = () => (state.on ? area()?.selectionStart ?? 0 : 0);

/** Select `len` characters from `at` and scroll them into the middle.
 *
 *  **It does not take the focus, deliberately.** The caller is a find box being
 *  typed into, and a focus that followed each match would move the keyboard off
 *  the query after the first letter. The selection is still set, so closing the
 *  box leaves the caret on the match, which is what you want it on.
 *
 *  @param {number} at @param {number} len */
export function select(at, len) {
  const ta = area();
  if (!state.on || !ta) return false;
  ta.setSelectionRange(at, at + len);
  scrollToLine(ta, ta.value.slice(0, at).split('\n').length);
  return true;
}

/** Put the keyboard back in the buffer, where the selection already is. */
export function focus() {
  area()?.focus();
}

/** Put `text` in place of the `len` characters at `at`.
 *
 *  **It takes the focus and gives it back**, which is not tidiness: the insert
 *  goes through `document.execCommand`, and that writes into whatever element is
 *  focused — a find box, if the caller is one. The focus is restored to where it
 *  was so that typing carries on where it was going.
 *
 *  @param {number} at @param {number} len @param {string} text */
export function overwrite(at, len, text) {
  const ta = area();
  if (!state.on || !ta) return false;
  const was = document.activeElement;
  ta.focus();
  const to = at + text.length;
  replace(ta, { from: at, to: at + len, text, selStart: at, selEnd: to });
  if (was instanceof HTMLElement && was !== ta) was.focus();
  return true;
}

/** Put the caret at the start of a line and scroll it into the middle. */
export function goTo(/** @type {number} */ line) {
  const ta = area();
  if (!state.on || !ta) return false;
  let at = 0;
  for (let n = 1; n < line; n++) {
    const next = ta.value.indexOf('\n', at);
    if (next < 0) break;
    at = next + 1;
  }
  ta.focus();
  ta.setSelectionRange(at, at);
  scrollToLine(ta, line);
  return true;
}

/** Put line `line` in the middle of the buffer's viewport.
 *
 *  A line is a line high because `.editarea` is `white-space:pre`: nothing wraps,
 *  so the row and the line are the same thing and the arithmetic holds. The
 *  height is read rather than assumed, because the font size is a slider here.
 *
 *  @param {HTMLTextAreaElement} ta @param {number} line */
function scrollToLine(ta, line) {
  const lh = parseFloat(getComputedStyle(ta).lineHeight) || 18;
  ta.scrollTop = Math.max(0, (line - 1) * lh - ta.clientHeight / 2);
}

/** Is anything open to save? The `Ctrl+S` binding is the app's and there are two
 *  overlays that can hold a buffer, so it asks here rather than naming one. */
export const isOpen = () => state.on;
