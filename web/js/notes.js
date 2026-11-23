// Review notes on a line of the diff or a file, and handing them to the session (#52).
//
// **One conversation, two speeds.** A note is either asked at once, or kept and
// sent later with the others as one prompt: reading a diff while the agent edits
// it moves the lines under you, so most notes wait until the read is done.
//
// **Kept in this browser, per checkout and workspace.** A review takes minutes,
// and a note is worth nothing once it is sent, so the daemon does not keep them.
// A storage that throws (private mode) keeps them for the page's life.
//
// **Drawn over the rows, never between them.** The file viewer draws a band of
// rows of one measured height with spacers for the rest, so a taller row breaks
// its scroll. The + is one button moved to the hovered row, a note row is a mark
// in its gutter, and the box is a popover under the line.

import { MOD_LABEL, activeCheckout, callFor, confirmBox, currentSession, el, reason, toast } from './core.js';
import * as Split from './split.js';

/** @typedef {{ side: 'new' | 'old', line: number, text: string }} Anchor */
/** A note on one line, or on `line` to `to`. `text` is the line itself, which is
 *  what names a removed line: it is not in the file on disk any more.
 *  @typedef {{ path: string, side: 'new' | 'old', line: number, to: number, text: string, body: string }} Note */
/** One pane that can hold notes: the diff or the file viewer.
 *  @typedef {{ mount: HTMLElement, host: HTMLElement, button: HTMLElement, rows: string,
 *              ws: () => string | null, path: () => string | null,
 *              anchor: (row: HTMLElement) => Anchor | null }} View */

/** @type {Map<string, Note[]>} what storage could not hold */
const memory = new Map();
/** @type {Map<string, string>} half-written notes, by line */
const drafts = new Map();
/** @type {View[]} */
const views = [];

const keyOf = (/** @type {string} */ ws) => `orch.notes:${activeCheckout().path}:${ws}`;
const draftKey = (/** @type {string} */ ws, /** @type {string} */ path, /** @type {Anchor} */ a) =>
  `${keyOf(ws)}\u0000${path}\u0000${a.side}\u0000${a.line}`;
const same = (/** @type {Note} */ n, /** @type {string} */ path, /** @type {Anchor} */ a) =>
  n.path === path && n.side === a.side && n.line === a.line;

/** Every note in a workspace, in the order they were added. */
export function list(/** @type {string} */ ws) {
  const key = keyOf(ws);
  try {
    const kept = JSON.parse(localStorage.getItem(key) ?? 'null');
    if (Array.isArray(kept)) return /** @type {Note[]} */ (kept);
  } catch { /* private mode: the page's own copy */ }
  return memory.get(key) ?? [];
}

function store(/** @type {string} */ ws, /** @type {Note[]} */ notes) {
  const key = keyOf(ws);
  memory.set(key, notes);
  try {
    if (notes.length) localStorage.setItem(key, JSON.stringify(notes));
    else localStorage.removeItem(key);
  } catch { /* kept in memory above */ }
  for (const v of views) paint(v);
}

/** Add a note, or replace the one already on its line. */
function put(/** @type {string} */ ws, /** @type {Note} */ note) {
  const at = { side: note.side, line: note.line, text: note.text };
  store(ws, [...list(ws).filter((n) => !same(n, note.path, at)), note]);
}

function drop(/** @type {string} */ ws, /** @type {Note[]} */ gone) {
  store(ws, list(ws).filter((n) => !gone.includes(n) && !gone.some((g) => same(n, g.path, g))));
}

/** How the prompt names a note's line. A removed line is not in the file on disk,
 *  so its old number alone would point at whatever replaced it. */
function refOf(/** @type {Note} */ n) {
  if (n.side === 'old') return `${n.path}:${n.line} (removed: ${n.text.trim()})`;
  return `${n.path}:${n.to > n.line ? `${n.line}-${n.to}` : n.line}`;
}

/** The batch as one prompt, by file and line. */
export function prompt(/** @type {Note[]} */ notes) {
  const sorted = [...notes].sort((a, b) => a.path.localeCompare(b.path) || a.line - b.line);
  const items = sorted.map((n) => `- ${refOf(n)}\n  ${n.body.trim().replace(/\n/g, '\n  ')}`);
  return `Review notes, work through them in order:\n\n${items.join('\n')}`;
}

/** Type `text` into the session as a user turn. The daemon decides whether a
 *  keystroke would land in a prompt, a permission dialog or the middle of a turn
 *  (`api::tell_session`), so a refusal is its sentence. True once it is told. */
async function tell(/** @type {string} */ text) {
  const s = currentSession();
  if (!s) {
    toast('no session in this workspace to send to', true);
    return false;
  }
  try {
    await callFor(s.id, `/api/session/${encodeURIComponent(s.id)}/tell`, { text });
  } catch (e) {
    toast(reason(e), true);
    return false;
  }
  Split.open();
  toast(`sent to ${s.title || 'the session'}`);
  return true;
}

// ---------------------------------------------------------------------------
// The box: one at a time, under the line it is about.
// ---------------------------------------------------------------------------

/** @type {{ view: View, ws: string, path: string, at: Anchor, to: number, node: HTMLElement } | null} */
let box = null;

/** Close the box. What is typed stays as a draft for that line. */
export function closeBox() {
  if (!box) return;
  const text = /** @type {HTMLTextAreaElement} */ (box.node.querySelector('textarea')).value;
  const key = draftKey(box.ws, box.path, box.at);
  if (text.trim()) drafts.set(key, text);
  else drafts.delete(key);
  box.node.remove();
  const mount = box.view.mount;
  box = null;
  mount.focus({ preventScroll: true });
}

function rowOf(/** @type {View} */ v, /** @type {Anchor} */ a) {
  for (const row of /** @type {NodeListOf<HTMLElement>} */ (v.mount.querySelectorAll(v.rows))) {
    const at = v.anchor(row);
    if (at && at.side === a.side && at.line === a.line) return row;
  }
  return null;
}

/** Under its row, or above it when there is no room below; gone with the row. */
function place() {
  if (!box) return;
  const row = rowOf(box.view, box.at);
  if (!row) return closeBox();
  const r = row.getBoundingClientRect();
  const m = box.view.mount.getBoundingClientRect();
  if (r.bottom < m.top || r.top > m.bottom) return closeBox();
  const node = box.node;
  const left = m.left + 48;
  node.style.left = `${left}px`;
  node.style.width = `${Math.max(240, Math.min(560, m.right - left - 24))}px`;
  const below = r.bottom + 2;
  node.style.top = `${below + node.offsetHeight > m.bottom ? Math.max(m.top, r.top - node.offsetHeight - 2) : below}px`;
}

/** Open the box on `from`..`to` in a view, with the note already there if any. */
function open(/** @type {View} */ v, /** @type {Anchor} */ at, /** @type {number} */ to) {
  const ws = v.ws();
  const path = v.path();
  if (!ws || !path) return;
  closeBox();
  const had = list(ws).find((n) => same(n, path, at)) ?? null;
  const note = (/** @type {string} */ body) => /** @type {Note} */ (
    { path, side: at.side, line: at.line, to: Math.max(to, at.line), text: at.text, body });

  const node = el('div', 'notebox');
  node.dataset.form = '';
  const where = at.side === 'old' ? `was ${at.line}` : to > at.line ? `${path}:${at.line}-${to}` : `${path}:${at.line}`;
  node.appendChild(el('div', 'notebox-ref', where));
  const area = /** @type {HTMLTextAreaElement} */ (el('textarea'));
  area.rows = 3;
  area.setAttribute('aria-label', `Note on ${where}`);
  area.value = drafts.get(draftKey(ws, path, at)) ?? had?.body ?? '';
  node.appendChild(area);
  const foot = el('div', 'notebox-foot');
  if (had) {
    const del = el('button', 'head-btn', 'Delete');
    del.onclick = () => {
      drop(ws, [had]);
      drafts.delete(draftKey(ws, path, at));
      area.value = '';
      closeBox();
    };
    foot.appendChild(del);
  }
  foot.appendChild(el('span', 'notebox-hint', 'Esc keeps it as a draft'));
  /* **Ask now is the form's submit**, so MOD Enter reaches it through the one
     binding every form here uses (`data-form` in app.js). */
  const ask = el('button', 'head-btn', 'Ask now ');
  ask.dataset.submit = '';
  ask.appendChild(el('kbd', null, `${MOD_LABEL} ↵`));
  ask.onclick = async () => {
    const body = area.value.trim();
    if (!body) return toast('type the question first', true);
    const n = note(body);
    if (!await tell(`${refOf(n)}\n\n${body}`)) return;
    if (had) drop(ws, [had]);
    area.value = '';
    closeBox();
  };
  const add = el('button', 'head-btn notebox-add', had ? 'Save ' : 'Add note ');
  add.appendChild(el('kbd', null, '↵'));
  add.onclick = () => {
    const body = area.value.trim();
    if (!body) return closeBox();
    put(ws, note(body));
    area.value = '';
    closeBox();
  };
  foot.append(ask, add);
  node.appendChild(foot);
  area.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      closeBox();
    } else if (e.key === 'Enter' && !e.shiftKey && !e.ctrlKey && !e.metaKey && !e.altKey) {
      // Enter keeps it; Shift Enter is the newline a longer note needs.
      e.preventDefault();
      add.click();
    }
  });
  v.host.appendChild(node);
  box = { view: v, ws, path, at, to, node };
  place();
  area.focus();
}

/** The line menu and MOD L: the box on `from`..`to` of the new side. */
export function openAt(/** @type {View} */ v, /** @type {number} */ from, /** @type {number} */ to) {
  for (const row of /** @type {NodeListOf<HTMLElement>} */ (v.mount.querySelectorAll(v.rows))) {
    const at = v.anchor(row);
    if (at && at.side === 'new' && at.line === from) return open(v, at, to);
  }
  toast(`line ${from} is not on screen`, true);
}

/** MOD L where there is no active line: the rows the page selection covers. */
export function openOnSelection(/** @type {View} */ v) {
  const sel = window.getSelection();
  const rowAt = (/** @type {Node | null | undefined} */ node) => {
    const e = node instanceof Element ? node : node?.parentElement;
    const row = /** @type {HTMLElement | null | undefined} */ (e?.closest(v.rows));
    return row && v.mount.contains(row) ? row : null;
  };
  const a = rowAt(sel?.anchorNode);
  const b = rowAt(sel?.focusNode);
  const first = a ? v.anchor(a) : null;
  if (!first) return toast('click or select the lines to note first', true);
  const last = b ? v.anchor(b) : null;
  const [lo, hi] = last && last.side === first.side
    ? (last.line < first.line ? [last, first] : [first, last])
    : [first, first];
  open(v, lo, hi.line);
}

// ---------------------------------------------------------------------------
// The marks, the + and the list.
// ---------------------------------------------------------------------------

/** Mark the rows with a note, and say how many notes the header button holds. */
function paint(/** @type {View} */ v) {
  const ws = v.ws();
  const path = v.path();
  const notes = ws ? list(ws) : [];
  v.button.hidden = !notes.length;
  const count = v.button.querySelector('kbd');
  if (count) count.textContent = String(notes.length);
  const here = notes.filter((n) => n.path === path);
  for (const row of /** @type {NodeListOf<HTMLElement>} */ (v.mount.querySelectorAll(v.rows))) {
    const at = v.anchor(row);
    const noted = !!at && !!path && here.some((n) => same(n, path, at));
    if (row.classList.contains('noted') !== noted) row.classList.toggle('noted', noted);
  }
  if (box?.view === v) place();
}

/** Repaint every pane, for a caller that changed what they show. */
export function repaint() {
  for (const v of views) paint(v);
}

/** The header button's list: a checkbox per note, and one send for the checked. */
function openList(/** @type {View} */ v) {
  const ws = v.ws();
  if (!ws) return;
  const old = document.querySelector('.notelist');
  if (old) {
    old.remove();
    return;
  }
  const notes = list(ws);
  if (!notes.length) return;
  const panel = el('div', 'notelist');
  panel.setAttribute('role', 'dialog');
  panel.setAttribute('aria-label', 'Notes');
  const head = el('div', 'notelist-head');
  head.appendChild(el('span', null, `${notes.length} note${notes.length === 1 ? '' : 's'} in ${ws}`));
  const clear = el('button', 'notelist-clear', 'Clear all');
  clear.onclick = async () => {
    panel.remove();
    if (await confirmBox(`Delete all ${notes.length} notes in ${ws}?`, { ok: 'Delete' })) drop(ws, notes);
  };
  head.appendChild(clear);
  panel.appendChild(head);
  /** @type {[Note, HTMLInputElement][]} */
  const picks = [];
  for (const n of notes) {
    const row = /** @type {HTMLLabelElement} */ (el('label', 'notelist-row'));
    const check = /** @type {HTMLInputElement} */ (el('input'));
    check.type = 'checkbox';
    check.checked = true;
    const text = el('span');
    text.append(el('span', 'notelist-ref', n.side === 'old' ? `${n.path}:${n.line} · removed` : refOf(n)), el('span', null, n.body));
    row.append(check, text);
    panel.appendChild(row);
    picks.push([n, check]);
  }
  const foot = el('div', 'notelist-foot');
  foot.appendChild(el('span', 'notebox-hint', 'One prompt, by file and line'));
  const send = el('button', 'head-btn notebox-add');
  const label = () => {
    const k = picks.filter(([, c]) => c.checked).length;
    send.textContent = `Send ${k} to the session`;
    /** @type {HTMLButtonElement} */ (send).disabled = !k;
  };
  for (const [, c] of picks) c.addEventListener('change', label);
  label();
  send.onclick = async () => {
    const chosen = picks.filter(([, c]) => c.checked).map(([n]) => n);
    if (!chosen.length || !await tell(prompt(chosen))) return;
    panel.remove();
    drop(ws, chosen);
  };
  foot.appendChild(send);
  panel.appendChild(foot);
  v.host.appendChild(panel);
  const r = v.button.getBoundingClientRect();
  panel.style.top = `${r.bottom + 4}px`;
  panel.style.left = `${Math.max(8, Math.min(r.left, window.innerWidth - panel.offsetWidth - 8))}px`;
  /** @type {HTMLElement | null} */ (panel.querySelector('input'))?.focus();
  const away = (/** @type {MouseEvent} */ e) => {
    if (panel.contains(/** @type {Node} */ (e.target)) || v.button.contains(/** @type {Node} */ (e.target))) return;
    panel.remove();
    window.removeEventListener('mousedown', away, true);
  };
  window.addEventListener('mousedown', away, true);
  panel.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    e.preventDefault();
    panel.remove();
    v.button.focus();
  });
}

/** Make a pane hold notes. Called once per pane, at boot. */
export function attach(/** @type {View} */ v) {
  views.push(v);
  /* **One + for the whole pane, moved to the row under the pointer.** A diff of a
     long file is thousands of rows, and a button in each would be thousands of
     nodes for one that is ever used. Its glyph is drawn, so a selection across
     the row does not copy it. */
  const plus = el('button', 'nplus');
  plus.setAttribute('aria-label', 'Note on this line');
  plus.title = `Note on this line (${MOD_LABEL} L)`;
  plus.addEventListener('mousedown', (e) => e.preventDefault());
  plus.onclick = (e) => {
    e.stopPropagation();
    const row = /** @type {HTMLElement | null} */ (plus.parentElement);
    const at = row ? v.anchor(row) : null;
    if (at) open(v, at, at.line);
  };
  v.mount.addEventListener('mouseover', (e) => {
    const row = /** @type {HTMLElement | null} */ (/** @type {HTMLElement} */ (e.target).closest?.(v.rows));
    if (!row || plus.parentElement === row || !v.anchor(row)) return;
    row.appendChild(plus);
  });
  v.mount.addEventListener('scroll', () => place());
  v.button.onclick = () => openList(v);
  /* Rows come and go under a render and a scroll: mark the new ones once the
     batch has landed. The + moving between rows is a change too, and the only one
     that says nothing about notes. */
  let queued = false;
  new MutationObserver((records) => {
    if (queued || !records.some((r) => [...r.addedNodes].some((n) => n !== plus))) return;
    queued = true;
    requestAnimationFrame(() => {
      queued = false;
      paint(v);
    });
  }).observe(v.mount, { childList: true, subtree: true });
  window.addEventListener('mousedown', (e) => {
    if (box?.view === v && !box.node.contains(/** @type {Node} */ (e.target)) && e.target !== plus) closeBox();
  }, true);
  paint(v);
  return v;
}
