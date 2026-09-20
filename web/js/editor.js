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

import { call, confirmBox, el, get, MOD_LABEL, reason, toast } from './core.js';

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
 *           host: Host | null }}
 */
export const state = {
  on: false,
  path: null,
  version: null,
  dirty: false,
  watch: null,
  host: null,
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
  body.appendChild(ta);
  ta.focus();

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

/** Is anything open to save? The `Ctrl+S` binding is the app's and there are two
 *  overlays that can hold a buffer, so it asks here rather than naming one. */
export const isOpen = () => state.on;
