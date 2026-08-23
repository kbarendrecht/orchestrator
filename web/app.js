'use strict';

// The SPA is a module now, so what it reaches for is written down. `core.js` holds
// the primitives every part needs; `queue.js` is the first seam extracted whole.
import {
  TOKEN, WS_BASE, $, el, toast, call, get, duration,
  snap, receive, sinceSnap, refreshButton, keyActivate,
  uiScale, setZoom, saveZoom, onScaleChange, ZOOM, zoomScale,
  selected, setSelected, onSelection, prForWorkspace,
  terms, CHROME, stateLabel, dotClass, stateClass, isWaiting, isArchived,
  pending, isConversation, byNewest, sessionsOf, currentSession,
  activeWorkspaceId, currentWorkspaceId, openMenu, closeMenu, menuOpen,
  newSession, newWorktree, newShell,
  selectedProc, setSelectedProc, prState, handedToPr,
  drawerTouched, setDrawerTouched, drawerCollapsed, setDrawerCollapsed,
  pendingProcFocus, setPendingProcFocus, pendingSelect, setPendingSelect,
  prOf, onDrawerChange,
} from './js/core.js';

// The daemon owns all state. This SPA is stateless and disposable: closing the
// browser kills nothing, and reopening replays from the daemon's buffers (§1).



// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// State presentation
// ---------------------------------------------------------------------------

/** The PR this session's work ended up on, if any.
 *
 *  An automation row's workspace is the placeholder `pr-10006`, which matches no
 *  real workspace, so it is asked by number instead. */







// ---------------------------------------------------------------------------
// Terminals
// ---------------------------------------------------------------------------
import * as Term from './js/term.js';

// The terminals are the scalable thing zoom used to reach into; now they ask.
onScaleChange(() => Term.applyScale());

// Collapsing the drawer redraws it and gives the terminal above its height back;
// xterm only refits on an explicit nudge, not on a sibling's size change.
onDrawerChange(() => { renderDrawer(); Term.refit(); });

// ---------------------------------------------------------------------------
// Context menu
// ---------------------------------------------------------------------------





// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------


/** An open diff belongs to the session it was opened from.
 *
 *  So switching away closes it. It used to re-point at the new workspace, which
 *  meant a switch silently swapped the file under you — and a diff is a thing you
 *  opened deliberately, not a pane that should follow you around. */
function syncDiffToSession() {
  if (!Diff.state.open) return;
  const ws = activeWorkspaceId();
  if (!ws || ws !== Diff.state.ws) Diff.close();
}

function render() {
  syncDiffToSession();
  Rail.render();
  renderContext();
  renderDrawer();
  Diff.renderFiles();
  Queue.render();
  renderInteraction();
  renderUpdate();
}

/** The question the selected session is blocked on.
 *
 *  Rendered from the snapshot rather than held locally, so it survives a reload
 *  and shows up in every window at once: the agent is stopped until somebody
 *  answers, and which browser you happen to be looking at is not part of that.
 *
 *  Over the terminal on purpose. The agent could print the question into its own
 *  pane, but then answering means typing into a wall of scrollback, and a
 *  question that scrolls away is one nobody notices. */
function renderInteraction() {
  const host = $('oq');
  const s = currentSession();
  const q = s && s.interaction && !s.interaction.answer ? s.interaction : null;
  if (!q) { host.hidden = true; host.replaceChildren(); return; }

  host.replaceChildren();
  const head = el('div', 'oqh');
  head.appendChild(el('span', 'dia', '\u25C6'));
  head.appendChild(el('span', null, 'needs your call'));
  if (q.thread_id) head.appendChild(el('span', 'oqt', q.thread_id));
  host.appendChild(head);

  host.appendChild(el('div', 'oqq', q.question));
  // Whatever the agent thought you needed to see to decide: a diff, a file, the
  // reviewer's words. Shown verbatim, in the diff's own type.
  if (q.detail) host.appendChild(Diff.detailEl(q.detail));

  const opts = el('div', 'oqopts');
  for (const o of q.options) {
    const b = el('button', 'oqopt' + (o.free ? ' esc' : ''));
    b.appendChild(el('div', 'ol', o.label));
    if (o.sub) b.appendChild(el('div', 'od', o.sub));
    // An option that asks for words does not answer on click: it opens the box.
    // Answering straight through would be the button saying "let me write it"
    // and then not letting you.
    b.onclick = o.free
      ? () => openFreeAnswer(opts, s.id, q.id, o)
      : () => answerInteraction(s.id, q.id, o.value, host);
    opts.appendChild(b);
  }
  host.appendChild(opts);
  host.hidden = false;
}

/** The escape hatch's box. Replaces the option row it belongs to, so there is one
 *  thing on screen to finish rather than a form beside a button that also works. */
function openFreeAnswer(opts, session, ask, option) {
  if (opts.querySelector('.oqfree')) return;
  const wrap = el('div', 'oqfree');
  const box = el('textarea', 'box');
  box.setAttribute('aria-label', option.label);
  box.placeholder = 'Say what you want instead. It reaches the agent as written.';
  wrap.appendChild(box);

  const row = el('div', 'oqfoot');
  const send = el('button', 'oqsend', 'send');
  send.onclick = () => {
    if (!box.value.trim()) return toast('nothing written yet', true);
    answerInteraction(session, ask, option.value, opts.parentElement, box.value);
  };
  const back = el('button', 'oqback', 'back to the options');
  back.onclick = () => renderInteraction();
  row.appendChild(send);
  row.appendChild(back);
  wrap.appendChild(row);

  opts.replaceChildren(wrap);
  box.focus();
}

/** Answer it, and let the next snapshot take the card away.
 *
 *  The buttons go dead immediately: the agent is released the moment the daemon
 *  has the answer, and a second click would be answering a question that is no
 *  longer open. */
async function answerInteraction(session, ask, answer, host, text) {
  const buttons = host.querySelectorAll('.oqopt, .oqsend');
  for (const b of buttons) b.disabled = true;
  try {
    await call(`/api/session/${session}/answer`, { ask, answer, text: text ?? null });
  } catch (e) {
    toast(e.message, true);
    for (const b of buttons) b.disabled = false;
  }
}


// The version the user dismissed this session. A newer release than this shows
// again; the same one stays hidden until the next launch.
let updateDismissed = null;
function renderUpdate() {
  const bar = $('updatebar');
  const u = snap.update;
  if (!u || updateDismissed === u.latest) { bar.hidden = true; return; }
  const link = /** @type {HTMLAnchorElement} */ ($('updatelink'));
  link.textContent = `Update available — v${u.latest} (you have v${u.current}). Run mise up`;
  link.href = u.url || '#';
  $('updatex').onclick = () => { updateDismissed = u.latest; bar.hidden = true; };
  keyActivate($('updatex'));
  bar.hidden = false;
}






import * as Rail from './js/rail.js';




function renderContext() {
  const s = currentSession();
  const wsId = currentWorkspaceId();
  const w = snap.workspaces.find((x) => x.id === wsId);

  // PRs are opened against upstream while branches live on the fork (§6), so
  // the header names both rather than collapsing them into one path.
  const repos = snap.repos || { upstream: null, fork: null };
  $('repoupstream').textContent = repos.upstream
    || (w ? w.path.split('/').slice(-2).join('/') : '—');
  $('repofork').textContent = repos.fork || '';
  $('ctxdot').className = 'dot ' + (s ? dotClass(s) : 'idle');
  $('ctxname').textContent = s ? Rail.rowName(s, { id: wsId }) : (wsId || 'no session');
  $('ctxforked').hidden = !(s && s.forked_from);
  $('ctxbranch').textContent = w ? (w.branches[0] || '') : '';
  const pr = wsId ? prForWorkspace(wsId) : null;
  const bits = [];
  if (s) bits.push(stateLabel(s));
  // Not when the session label is already the PR's, or the header says it twice.
  if (pr && !handedToPr(s)) bits.push(`#${pr.number} ${prState(pr)}`);
  $('ctxstate').textContent = bits.join(' · ');
  $('killbtn').style.display = s && s.alive ? '' : 'none';

}

// ---------------------------------------------------------------------------
// Drawer — available on every workspace, not just main (§9)
// ---------------------------------------------------------------------------

function renderDrawer() {
  const wsId = currentWorkspaceId();
  const w = snap.workspaces.find((x) => x.id === wsId);
  const tabs = $('dtabs');
  tabs.replaceChildren();

  // Docker stack status, in place of the path: blue = up, red = down.
  const dcwd = $('dcwd');
  dcwd.replaceChildren();
  const up = snap.stack_up === true;
  dcwd.appendChild(el('span', 'stackdot ' + (up ? 'up' : 'down')));
  dcwd.appendChild(el('span', null, up ? 'stack up' : 'stack down'));

  const procs = w ? w.processes : [];
  const drawer = $('drawer');

  // On a worktree the drawer starts empty and is a thin bar until you open
  // something. `collapsed` is that same bar chosen on purpose while processes
  // run — only offered, and only honoured, when there is a body to hide.
  const collapsed = drawerCollapsed && procs.length > 0;
  drawer.className = 'drawer' + (procs.length ? '' : ' empty') + (collapsed ? ' collapsed' : '');
  const toggle = $('dcollapse');
  toggle.hidden = procs.length === 0;
  // The same rotating caret the PR and review panes use. It was a pair of filled
  // triangles, which is a second vocabulary for the one gesture the app already
  // had a glyph for.
  toggle.replaceChildren(el('span', 'caretr', '\u203a'), el('span', 'eyebrow', 'Processes'));
  toggle.setAttribute('aria-expanded', String(!collapsed));
  toggle.title = collapsed ? 'Expand processes' : 'Collapse processes';
  // The plain label only stands in while there is nothing to collapse; otherwise
  // the same word would sit on screen twice.
  $('dlabel').hidden = procs.length > 0;

  const alive = (p) =>
    p.kind.kind === 'shell' ? p.kind.exit_code == null : p.health.health !== 'dead';

  let active = selectedProc[wsId];
  if (!procs.some((p) => p.id === active)) {
    // Prefer something still running; a dead shell is only shown when it is
    // all there is, or when you picked it yourself.
    active = (procs.find(alive) ?? procs[0])?.id ?? null;
  }
  selectedProc[wsId] = active;

  // Shells are numbered per workspace. Without this every dead one renders as
  // the same "shell (0)" and a drawer with three corpses in it is unreadable.
  let shellNo = 0;
  for (const p of procs) {
    const isShell = p.kind.kind === 'shell';
    if (isShell) shellNo += 1;
    const dead = p.kind.kind === 'shell'
      ? p.kind.exit_code != null
      : p.health.health === 'dead';

    const tab = el('button', 'dtab' + (dead ? ' dead' : ''));
    tab.setAttribute('aria-selected', String(p.id === active));
    /* Green means a process parsed its own output and said it is fine; red means
     * it said otherwise, or it is not running at all. Grey is reserved for "no
     * claim": a shell, which has no health parsing, and a managed process that
     * has not printed anything conclusive yet. Health used to render grey when
     * `ok`, so a green build looked exactly like one nobody had heard from. */
    const health = p.health.health;
    const cls = dead ? 'build'
      : isShell ? 'working'
        : health === 'failing' ? 'build'
          : health === 'ok' ? 'ok'
            : 'working';
    tab.appendChild(el('span', 'dot ' + cls));
    const label = isShell
      ? (dead && p.kind.kind === 'shell'
        ? `shell ${shellNo} · exit ${p.kind.exit_code}`
        : `shell ${shellNo}`)
      : p.name;
    tab.appendChild(el('span', null, label));
    tab.onclick = () => { setSelectedProc(wsId, p.id); setDrawerTouched(true); renderDrawer(); };

    // The same glyph every other dismiss uses; this one was a multiplication sign.
    const x = el('span', 'x', '\u2715');
    x.title = dead ? 'Dismiss' : 'Close';
    x.onclick = (ev) => {
      ev.stopPropagation();
      Term.close(`proc:${p.id}`);
      call(`/api/process/${encodeURIComponent(p.id)}/close`).catch((e) => toast(e.message, true));
    };
    tab.appendChild(x);

    if (p.kind.kind === 'managed') {
      const r = el('span', 'x', '⟳');
      r.title = 'Restart';
      r.onclick = (ev) => {
        ev.stopPropagation();
        Term.close(`proc:${p.id}`);
        call(`/api/workspace/${encodeURIComponent(wsId)}/process/${encodeURIComponent(p.name)}/restart`)
          .catch((e) => toast(e.message, true));
      };
      tab.appendChild(r);
    }
    tabs.appendChild(tab);
  }

  const shown = Term.show(active ? `proc:${active}` : null, $('drawerbody'));
  if (shown && pendingProcFocus && active === pendingProcFocus) {
    setPendingProcFocus(null);
    // After the frame that un-hides it: xterm refuses focus while its host has no
    // dimensions, which is exactly the state it is in right now.
    requestAnimationFrame(() => {
      try {
        shown.term.focus();
      } catch (e) { /* disposed while we waited */ }
    });
  }

  // Auto-expand when a managed process goes red.
  const failing = procs.find((p) => p.health.health === 'failing');
  if (failing && selectedProc[wsId] !== failing.id && !drawerTouched) {
    selectedProc[wsId] = failing.id;
    Term.show(`proc:${failing.id}`, $('drawerbody'));
  }
}





// ---------------------------------------------------------------------------
// Diff (§5)
// ---------------------------------------------------------------------------
import * as Diff from './js/diff.js';

// ---------------------------------------------------------------------------
// Review overlay
// ---------------------------------------------------------------------------
import * as Review from './js/review.js';

// ---------------------------------------------------------------------------
// Review queue (§6b)
// ---------------------------------------------------------------------------

import * as Queue from './js/queue.js';



// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

// What picking a session means: open its terminal, redraw, and put the cursor
// where you are about to type. Registered rather than called by the rail, so the
// rail does not have to know about rendering.
onSelection(() => {
  const s = currentSession();
  // A session created a moment ago is not in the snapshot yet. Blanking the
  // terminal here would strand it: the next snapshot sees `selected` already
  // set and never opens one.
  const shown = s ? Term.show(`session:${s.id}`, $('termwrap')) : null;
  render();
  // Picking a session is picking where you are about to type. After the frame
  // that un-hides it, for the same reason the drawer waits: xterm refuses focus
  // while its host still has no dimensions.
  if (shown) {
    requestAnimationFrame(() => {
      try {
        shown.term.focus();
      } catch (e) { /* disposed while we waited */ }
    });
  }
});





/** Teardown is offered, never automatic, and the preflight is shown in full
 *  before anything is removed (§2). */
async function teardown(wsId) {
  let pf;
  try {
    pf = await get(`/api/workspace/${encodeURIComponent(wsId)}/preflight`);
  } catch (e) {
    return toast(e.message, true);
  }
  const lines = pf.checks.map((c) => `${c.passed ? '✓' : '✗'} ${c.name} — ${c.detail}`);
  if (!pf.can_remove) {
    return toast(`cannot remove ${wsId}:\n${lines.join('\n')}`, true);
  }
  if (!confirm(`Remove worktree ${wsId}?\n\n${lines.join('\n')}`)) return;
  try {
    await call(`/api/workspace/${encodeURIComponent(wsId)}/teardown`);
    toast(`removed ${wsId}`);
  } catch (e) {
    toast(e.message, true);
  }
}

$('ovclose').onclick = Diff.close;
$('ovprev').onclick = () => Diff.step(-1);
$('ovnext').onclick = () => Diff.step(1);
$('ovmode').onclick = () => {
  if (Diff.edit.on && !Diff.closeEditor()) return;
  Diff.state.split = !Diff.state.split;
  Diff.render();
};
$('ovedit').onclick = () => (Diff.edit.on ? Diff.closeEditor() : Diff.openEditor());
$('ovsave').onclick = Diff.saveEditor;
$('reposwitch').onclick = () =>
  toast('switching repositories is not implemented yet', true);
$('addshell').onclick = newShell;
$('keyhelpx').onclick = () => { $('keyhelp').hidden = true; };
$('dcollapse').onclick = () => setDrawerCollapsed(!drawerCollapsed);
$('refreshbtn').onclick = () => {
  const wsId = currentWorkspaceId();
  if (wsId) call(`/api/workspace/${encodeURIComponent(wsId)}/reconcile`).catch((e) => toast(e.message, true));
};
$('killbtn').onclick = () => {
  const s = currentSession();
  if (s) Rail.closeSession(s.id);
};

// ---------------------------------------------------------------------------
// Keyboard (§9)
// ---------------------------------------------------------------------------
//
// Two layers, and which modifier a key wears says which one it is. A new binding
// belongs to one of them; there is no third to invent.
//
//   • bare keys  — the overlay that is open, and only while it is open (review
//     cards, diff files). Nothing bare is global, because bare keys reach the
//     terminal.
//   • Ctrl+…     — the whole app: new (n / Shift+n / Shift+t), switch session
//     (Tab / Shift+Tab), jump to what needs you (Space), the diff (Shift+d),
//     zoom (= − 0), save (s).
//   • Escape is not a layer, it is one rule: dismiss the topmost thing —
//     legend, then menu, then settings, then the open overlay.
//
// There is deliberately **no Alt layer**. It held the vim-style motion (Alt+j/k
// sessions, Alt+m main, Alt+d diff) and was removed: every action it carried had
// a Ctrl spelling doing the same job, so it was a second vocabulary for one set
// of verbs. Do not reintroduce it to dodge a collision — pick Ctrl+Shift instead.
//
// Two properties of the Ctrl layer worth knowing before extending it:
//
//   * Plain Ctrl+<letter> shadows the pty. `Ctrl+n` is readline next-history and
//     `Ctrl+Space` is NUL, and binding them here takes them from every session.
//     That is a deliberate trade, not an oversight. `Ctrl+Shift+…` is the zone
//     terminals leave alone (which is why copy/paste live there), so prefer it
//     when the terminal's own key is worth keeping — `Ctrl+Shift+d` for the diff
//     rather than `Ctrl+d`, which is EOF and still exits a shell.
//   * `Ctrl+n` and `Ctrl+Tab` are browser-reserved and never arrive in a plain
//     tab; they work in the desktop webview, which is the primary target. The
//     legend says so rather than leaving it to be discovered.
//
// `?` opens that legend, the one source of truth a user can see. Keep it in step
// with these bindings — a scheme nobody can read is not predictable however
// consistent it is.

window.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !$('keyhelp').hidden) {
    e.preventDefault();
    $('keyhelp').hidden = true;
    return;
  }
  // First, or Escape closes the overlay underneath and leaves the menu floating
  // over it.
  if (e.key === 'Escape' && menuOpen()) {
    e.preventDefault();
    closeMenu();
    return;
  }
  if (e.key === 'Escape' && Settings.isOpen()) {
    e.preventDefault();
    Settings.close();
    return;
  }
  if ((e.metaKey || e.ctrlKey) && e.key === 's' && Diff.edit.on) {
    e.preventDefault();
    Diff.saveEditor();
    return;
  }
  /* The overlay wants bare Enter, j/k and digits, and this handler is registered
     with capture:true — it runs before any element listener wherever focus is.
     So the focus guard is not optional here the way it was for Escape/Ctrl+←. */
  if (Review.state.open) {
    const typing = !!/** @type {HTMLElement} */ (e.target).closest?.('textarea, input, [contenteditable="true"]');
    if (e.key === 'Escape') {
      e.preventDefault();
      // Blur rather than close, or Escape out of a half-typed reply discards it.
      if (typing) /** @type {HTMLElement} */ (e.target).blur();
      else Review.close();
      return;
    }
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      // Through the button rather than straight to `sendBatch`, or the shortcut
      // sends a batch the button itself refuses — which it did until now.
      if (Review.state.screen === 'final') {
        const send = /** @type {HTMLButtonElement} */ (
          $('rvoverlay').querySelector('.acts .act.warm'));
        if (send && !send.disabled) send.click();
      }
      return;
    }
    // Bare only: a modified key is the app's (the Ctrl layer), never a card's.
    if (!typing && !e.altKey && !e.ctrlKey && !e.metaKey && Review.key(e)) {
      e.preventDefault();
      return;
    }
  }
  if (Diff.state.open) {
    if (e.key === 'Escape') { e.preventDefault(); Diff.close(); return; }
    // j/k steps through the changeset, matching the review overlay's motion so
    // "next/previous in a list" is one idiom everywhere. Guarded on not-typing
    // because the diff hosts an editor. Ctrl+←/→ stays as an alias — it was the
    // only binding before, so muscle memory keeps working; it was itself once
    // F7/⇧F7, one key doing two jobs by modifier.
    const typingInDiff = !!/** @type {HTMLElement} */ (e.target).closest?.('textarea, input, [contenteditable="true"]');
    if (!typingInDiff && !e.ctrlKey && !e.altKey && !e.metaKey && (e.key === 'j' || e.key === 'k')) {
      e.preventDefault();
      Diff.step(e.key === 'j' ? 1 : -1);
      return;
    }
    if (e.ctrlKey && (e.key === 'ArrowLeft' || e.key === 'ArrowRight')) {
      e.preventDefault();
      Diff.step(e.key === 'ArrowLeft' ? -1 : 1);
      return;
    }
  }
  // The Ctrl layer: the whole app (see the header). Some of these knowingly
  // shadow terminal keys, so the block ends with a bare `return` — any Ctrl combo
  // it does not claim falls through to the pty *without* preventDefault, which is
  // what keeps Ctrl+C an interrupt, Ctrl+D an EOF and Ctrl+S flow-control.
  if (e.ctrlKey && !e.altKey && !e.metaKey) {
    const k = e.key.toLowerCase();
    // Ctrl+Shift+T beside Ctrl+`: the terminal-emulator "new tab" key, in the
    // Ctrl+Shift zone terminals leave alone. A shell is a process in the drawer,
    // so this is the third rung of the same ladder as Ctrl+N / Ctrl+Shift+N.
    if (e.key === '`' || (e.shiftKey && k === 't')) {
      e.preventDefault(); newShell(); return;
    }
    // Shift, not plain: Ctrl+D is EOF and still has to exit a shell.
    if (e.shiftKey && k === 'd') {
      e.preventDefault();
      if (Review.state.open) return toast('close the review first');
      Diff.state.open ? Diff.close() : Diff.open();
      return;
    }
    if (k === 'n') {
      e.preventDefault();
      if (e.shiftKey) {
        const main = snap.workspaces.find((w) => w.is_main);
        if (main) newSession(main.id);
      } else {
        // The rail's + is the named variant (Shift+click); a hotkey takes the
        // common case and lets Claude Code name it.
        newWorktree(false);
      }
      return;
    }
    if (e.code === 'Space') {
      // The first session waiting on you — the one costing you the most.
      e.preventDefault();
      const first = snap.sessions.find(isWaiting);
      if (first) setSelected(first.id);
      else toast('nothing waiting on you');
      return;
    }
    if (e.key === 'Tab') {
      // The one way to walk sessions, since the Alt layer went. Cyclic, so it
      // needs no separate "wrap" or first/last key.
      e.preventDefault();
      const ordered = snap.sessions;
      if (!ordered.length) return;
      const idx = ordered.findIndex((s) => s.id === selected);
      const step = e.shiftKey ? -1 : 1;
      setSelected(ordered[(idx + step + ordered.length) % ordered.length].id);
      return;
    }
    // Zoom. '=' shares its key with '+'; '_' rides '-'; the numpad spells both.
    if (k === '=' || e.key === '+' || e.code === 'NumpadAdd') {
      e.preventDefault(); saveZoom(setZoom(zoomScale + ZOOM.step)); return;
    }
    if (k === '-' || e.code === 'NumpadSubtract') {
      e.preventDefault(); saveZoom(setZoom(zoomScale - ZOOM.step)); return;
    }
    if (e.key === '0' || e.code === 'Numpad0') {
      e.preventDefault(); saveZoom(setZoom(ZOOM.def)); return;
    }
    return;
  }

  // `?` opens the legend — bare, so guarded against firing while you type one.
  // The last binding, and the only global bare key: everything else bare belongs
  // to an overlay and was handled above.
  if (e.key === '?') {
    const typing = !!/** @type {HTMLElement} */ (e.target).closest?.('textarea, input, [contenteditable="true"]');
    if (!typing) { e.preventDefault(); $('keyhelp').hidden = !$('keyhelp').hidden; return; }
  }
}, true);

/* A terminal sizes itself to its host, and the host changes size for more reasons
 * than any one event covers: a window resize, a column drag, a font-size step, or
 * a compositor handing the window back at a different size than it took it —
 * which is the one that left the centre pane short of full height until you
 * switched sessions. So watch the box rather than enumerate the causes.
 *
 * Coalesced to one refit per frame, and a refit that changes nothing sends
 * nothing. */
let refitTimer = null;
function queueRefit() {
  // Settled, not per-frame: a drag or a compositor animation fires this dozens of
  // times, and fitting mid-flight is how a terminal ends up sized to a box that
  // is still moving.
  if (refitTimer) clearTimeout(refitTimer);
  refitTimer = setTimeout(() => {
    refitTimer = null;
    Term.refit();
  }, 120);
}

const hostObserver = new ResizeObserver(queueRefit);
for (const id of ['termwrap', 'drawerbody']) {
  const host = $(id);
  if (host) hostObserver.observe(host);
}
// Belt and braces for the focus case: if the window comes back with the same box
// but a parked renderer, nothing above fires and this costs nothing.
window.addEventListener('focus', queueRefit);
document.addEventListener('visibilitychange', queueRefit);

/* Both bottom panes are lists of links you follow out of the app, and what you
 * do out there (approve, comment, merge) is the very thing they list. Coming
 * back to a queue that still holds the review you just finished is the pane
 * lying until the next poll, so returning to the window pulses both pollers.
 *
 * Throttled, because alt-tabbing is not a reason to spend the GitHub budget, and
 * silent, because nobody asked for this one: the ↻ buttons stay the loud path. */
const RETURN_REFRESH_MS = 30_000;
let lastReturnRefresh = 0;
function refreshOnReturn() {
  if (document.hidden) return;
  if (Date.now() - lastReturnRefresh < RETURN_REFRESH_MS) return;
  lastReturnRefresh = Date.now();
  call('/api/prs/refresh').catch(() => {});
  call('/api/reviews/refresh').catch(() => {});
}
window.addEventListener('focus', refreshOnReturn);
document.addEventListener('visibilitychange', refreshOnReturn);

// ---------------------------------------------------------------------------
// Live state
// ---------------------------------------------------------------------------

function connect() {
  const sock = new WebSocket(`${WS_BASE}/ws/events?token=${encodeURIComponent(TOKEN)}`);
  sock.onmessage = (ev) => {
    // Through `receive` so the snapshot and the clock it is measured against move
    // together; `snap` is a live binding, so every reader sees this.
    receive(JSON.parse(ev.data));
    // A session whose pty is gone keeps its scrollback until it is dismissed,
    // so terminals are only torn down when the session disappears entirely.
    const liveProcs = new Set(
      snap.workspaces.flatMap((w) => w.processes.map((p) => `proc:${p.id}`))
    );
    for (const target of [...terms.keys()]) {
      if (target.startsWith('session:')) {
        const id = target.slice('session:'.length);
        if (!snap.sessions.some((s) => s.id === id)) Term.close(target);
      } else if (!liveProcs.has(target)) {
        // A shell that closed cleanly is gone from the snapshot; drop its
        // terminal rather than leaving a hidden host behind forever.
        Term.close(target);
      }
    }
    // The three panes describe one thing: the session you are in. The rail says
    // which, the centre shows its pty, the right pane its changes. So the
    // selection only ever points at something running — a session that finished
    // is not something to land on, and its scrollback is not what the centre is
    // for once it has stopped.
    if (selected) {
      const cur = snap.sessions.find((s) => s.id === selected);
      if (!cur || isArchived(cur)) setSelected(null);
    }

    // Switch to a session we asked for as soon as the daemon reports it.
    if (pendingSelect && snap.sessions.some((s) => s.id === pendingSelect)) {
      const id = pendingSelect;
      setPendingSelect(null);
      setSelected(id);
      return;
    }

    if (!selected) {
      // Default to whatever most needs you, among what is actually running.
      const first = snap.sessions.filter((x) => !isArchived(x));
      const pick = first.find(isWaiting) || first[0];
      if (pick) {
        setSelected(pick.id);
        Term.show(`session:${pick.id}`, $('termwrap'));
      } else {
        Term.show(null, $('termwrap'));
      }
    }
    render();
    nudgeWebkitInput();
  };
  sock.onclose = () => {
    toast('daemon disconnected — retrying', true);
    setTimeout(connect, 1500);
  };
}

// ---------------------------------------------------------------------------
// Native window chrome
// ---------------------------------------------------------------------------


// WebKitGTK (the desktop webview) can leave its input region stale on first
// paint: on launch, clicking a rail row and grabbing the frameless resize edges
// both do nothing until a layout-changing DOM mutation (collapsing a drawer)
// forces a full repaint. Do that repaint ourselves, once, right after the first
// snapshot renders, so the first interaction already lands. A browser tab
// (chrome 'none') does not have the fault and is left alone.
let inputNudged = false;
function nudgeWebkitInput() {
  if (inputNudged || CHROME === 'none') return;
  inputNudged = true;
  const root = document.documentElement;
  root.style.transform = 'translateZ(0)';
  void root.offsetHeight; // force the relayout now, not at the next paint
  requestAnimationFrame(() => {
    root.style.transform = '';
    void root.offsetHeight;
    // A synthetic resize re-establishes the webview's hit regions and refits
    // the visible terminal, the same path the drawer toggle takes.
    window.dispatchEvent(new Event('resize'));
  });
}

function setupChrome() {
  document.body.dataset.chrome = CHROME;
  if (CHROME === 'none') return;

  // The webview opens no target=_blank windows and wires no shell, so external
  // links (review rows, PR rows, the update nudge) go nowhere on their own —
  // under WSLg especially. Route them through the daemon's OS opener. A browser
  // tab (chrome 'none') returns above and opens them natively.
  document.addEventListener('click', (e) => {
    const t = /** @type {HTMLElement} */ (e.target);
    const a = /** @type {HTMLAnchorElement} */ (t.closest && t.closest('a[target="_blank"]'));
    if (!a || !/^https?:/i.test(a.href || '')) return;
    e.preventDefault();
    call('/api/open', { url: a.href }).catch((err) => toast(err.message, true));
  });

  const wcmd = (cmd) => call(`/api/window/${cmd}`).catch((e) => toast(e.message, true));

  for (const b of /** @type {NodeListOf<HTMLElement>} */ (
    document.querySelectorAll('.wctl-btn'))) {
    b.addEventListener('click', () => wcmd(b.dataset.cmd));
  }

  for (const bar of document.querySelectorAll('.top')) {
    bar.addEventListener('mousedown', (/** @type {MouseEvent} */ e) => {
      // Left button only, and only on the bar's own background: a drag that
      // swallowed clicks on the session name or the close button would make
      // the header unusable.
      if (e.button !== 0) return;
      if (/** @type {HTMLElement} */ (e.target).closest('button, input, a, kbd, .ctx-btn')) return;
      // A double-click is the OS gesture for maximise, so it must not also
      // start a drag; the compositor keeps the drag alive past mouseup, which
      // would eat the second click.
      if (e.detail > 1) return;
      wcmd('start-drag');
    });
    bar.addEventListener('dblclick', (e) => {
      if (/** @type {HTMLElement} */ (e.target).closest('button, input, a, kbd, .ctx-btn')) return;
      wcmd('toggle-maximize');
    });
  }

  for (const rz of /** @type {NodeListOf<HTMLElement>} */ (
    document.querySelectorAll('.rz'))) {
    rz.addEventListener('mousedown', (/** @type {MouseEvent} */ e) => {
      if (e.button !== 0) return;
      // Stop the browser starting a text selection that outlives the resize.
      e.preventDefault();
      wcmd(`resize/${rz.dataset.edge}`);
    });
  }
}

// ---------------------------------------------------------------------------
// Column widths
// ---------------------------------------------------------------------------

/* The three-column grid is two CSS variables wide, so a drag is a variable
 * write and nothing re-renders. Widths are a preference of this browser, not
 * state the daemon owns, so they live in localStorage — same reasoning as the
 * rail's collapsed sections. */
const COLS = {
  rail: { prop: '--rail', key: 'orch.railWidth', def: 290, min: 210 },
  files: { prop: '--files', key: 'orch.filesWidth', def: 296, min: 230 },
};
/* The centre pane holds a terminal; squeezing it to nothing to admire a wide
 * rail is not a layout anybody wants to be one drag away from. */
const CENTRE_MIN = 420;

/* Same idea on the other axis: the process drawer is one more variable, and the
 * terminal above it gets the same protection the centre column gets. */
const DRAWER = { prop: '--drawer', key: 'orch.drawerHeight', def: 210, min: 96 };
const TERM_MIN = 150;

const colWidth = (col) =>
  parseInt(getComputedStyle(document.documentElement).getPropertyValue(col.prop), 10) || col.def;

/** Set a column, clamped so the centre always survives and so does the other one. */
function setCol(col, px) {
  const other = col === COLS.rail ? COLS.files : COLS.rail;
  const room = window.innerWidth - CENTRE_MIN - colWidth(other);
  const width = Math.round(Math.max(col.min, Math.min(px, Math.max(col.min, room))));
  document.documentElement.style.setProperty(col.prop, `${width}px`);
  return width;
}

const drawerHeight = () =>
  parseInt(getComputedStyle(document.documentElement).getPropertyValue(DRAWER.prop), 10)
  || DRAWER.def;

/** Set the drawer height, clamped so the terminal above it stays usable. */
function setDrawer(px) {
  const centre = document.querySelector('.center');
  const room = (centre ? centre.clientHeight : window.innerHeight) - TERM_MIN;
  const h = Math.round(Math.max(DRAWER.min, Math.min(px, Math.max(DRAWER.min, room))));
  document.documentElement.style.setProperty(DRAWER.prop, `${h}px`);
  return h;
}

/** xterm sizes itself to its host, and a column drag is not a window resize. */
function dragColumn(handle, col, fromLeft) {
  handle.addEventListener('mousedown', (e) => {
    if (e.button !== 0) return;
    // The titlebar's own drag handler lives under this strip.
    e.preventDefault();
    e.stopPropagation();
    handle.classList.add('dragging');
    document.body.classList.add('col-resizing');

    const move = (ev) => setCol(col, fromLeft ? ev.clientX : window.innerWidth - ev.clientX);
    const done = () => {
      window.removeEventListener('mousemove', move);
      handle.classList.remove('dragging');
      document.body.classList.remove('col-resizing');
      try {
        localStorage.setItem(col.key, String(colWidth(col)));
      } catch (err) { /* private mode: the drag still worked for this session */ }
      // Once, at the end: fitting on every mousemove means a pty resize per
      // mouse event, and the terminal reflows fine on release.
      Term.refit();
    };
    window.addEventListener('mousemove', move);
    window.addEventListener('mouseup', done, { once: true });
  });

  handle.addEventListener('dblclick', () => {
    setCol(col, col.def);
    try {
      localStorage.removeItem(col.key);
    } catch (err) { /* nothing to forget */ }
    Term.refit();
  });
}

/* The drawer's own drag. Not `dragColumn` with a flag: it reads clientY against
 * the centre pane rather than clientX against the window, and it has no sibling
 * column to leave room for. */
function dragDrawer(handle) {
  handle.addEventListener('mousedown', (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.stopPropagation();
    handle.classList.add('dragging');
    document.body.classList.add('row-resizing');

    const bottom = document.querySelector('.center').getBoundingClientRect().bottom;
    const move = (ev) => setDrawer(bottom - ev.clientY);
    const done = () => {
      window.removeEventListener('mousemove', move);
      handle.classList.remove('dragging');
      document.body.classList.remove('row-resizing');
      try {
        localStorage.setItem(DRAWER.key, String(drawerHeight()));
      } catch (err) { /* private mode: the drag still worked for this session */ }
      Term.refit();
    };
    window.addEventListener('mousemove', move);
    window.addEventListener('mouseup', done, { once: true });
  });

  handle.addEventListener('dblclick', () => {
    setDrawer(DRAWER.def);
    try {
      localStorage.removeItem(DRAWER.key);
    } catch (err) { /* nothing to forget */ }
    Term.refit();
  });
}

function setupColumns() {
  for (const col of Object.values(COLS)) {
    const saved = Number(localStorage.getItem(col.key));
    if (saved) setCol(col, saved);
  }
  const savedDrawer = Number(localStorage.getItem(DRAWER.key));
  if (savedDrawer) setDrawer(savedDrawer);
  dragColumn($('splitl'), COLS.rail, true);
  dragColumn($('splitr'), COLS.files, false);
  dragDrawer($('splitd'));
  // A window that got smaller can leave a stored size with no room for it.
  window.addEventListener('resize', () => {
    for (const col of Object.values(COLS)) setCol(col, colWidth(col));
    setDrawer(drawerHeight());
  });
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------


import * as Settings from './js/settings.js';

Settings.setup();
setupColumns();
setupChrome();
connect();
// The waiting clock has to tick even when nothing else changes.
setInterval(() => { Rail.render(); }, 1000);

window.orchTeardown = teardown;

