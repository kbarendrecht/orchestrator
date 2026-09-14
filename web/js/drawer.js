// The process drawer: the tab strip, what each tab shows, and handing a pane's
// output to the session.
//
// **Out of `app.js` because it is a pane, and every other pane is a module.** The
// drawer's *state* was already in `core` (`selectedProc`, `procOrder`,
// `drawerCollapsed` — every one of them per-checkout-keyed) while its render was
// 400 lines in `app.js`: one feature across two files, which is the shape the
// module split exists to remove. What is left in `app.js` is boot order, the
// socket, the keyboard map and the window chrome.
//
// It reaches for `term` the way `rail` does — a tab is a terminal — and for
// nothing else of the SPA's beyond `core`.

import { $, activeCheckout, call, currentSession, currentWorkspaceId, drawerCollapsed, drawerTouched, el, openMenu, pendingProcFocus, procOrder, reason, selectedProc, setDrawerTouched, setPendingProcFocus, setProcOrder, setSelectedProc, snap, toast, unchanged, workspaceById, wsKey } from './core.js';
import * as Term from './term.js';

/* The workspace whose tab strip is being dragged, or null. A snapshot arriving
   mid-drag would `replaceChildren` the strip out from under the pointer, so the
   render is skipped until the drop — which then renders once, from the order the
   drop just saved. */
/** @type {string | null} */
let tabDrag = null;

/** Reorder the drawer's tabs by dragging one.
 *
 *  Pointer events rather than HTML5 drag-and-drop: the webview is WebKitGTK, and
 *  a native drag brings a drag image, a text selection and its own dragover rules
 *  along with it, none of which a 20px tab wants. The 4px threshold is what keeps
 *  a plain click on a tab a click. */
function startTabDrag(/** @type {PointerEvent} */ ev, /** @type {HTMLElement} */ tab, /** @type {string | null} */ wsId) {
  if (ev.button !== 0) return;
  const strip = $('dtabs');
  const startX = ev.clientX;
  let moved = false;
  let lastX = startX;
  /** @type {ReturnType<typeof setInterval> | null} */
  let edge = null;

  // Land before the first tab whose middle the pointer has passed — the same rule
  // in both directions, so there is no left/right special case.
  const placeAt = (/** @type {number} */ x) => {
    const before = [...strip.children]
      .filter((c) => c !== tab)
      .find((c) => {
        const r = c.getBoundingClientRect();
        return x < r.left + r.width / 2;
      });
    strip.insertBefore(tab, before ?? null);
  };

  /* Held at either end, nudge the strip: the target may be scrolled out of sight,
     and a reorder you can only do within the visible window is not one. On a
     timer rather than on movement, because holding still at the edge is exactly
     the gesture — and it re-places the tab on every tick, since the pointer is
     not moving but everything under it is. */
  const edgeScroll = (/** @type {number} */ x) => {
    const r = strip.getBoundingClientRect();
    const dir = x > r.right - 28 ? 1 : x < r.left + 28 ? -1 : 0;
    if (!dir || edge) {
      if (!dir && edge) { clearInterval(edge); edge = null; }
      return;
    }
    edge = setInterval(() => {
      strip.scrollLeft += dir * 10;
      placeAt(lastX);
    }, 16);
  };

  const onMove = (/** @type {PointerEvent} */ e) => {
    if (!moved && Math.abs(e.clientX - startX) < 4) return;
    if (!moved) {
      moved = true;
      tabDrag = wsId;
      tab.classList.add('dragging');
    }
    lastX = e.clientX;
    placeAt(lastX);
    edgeScroll(lastX);
  };

  const onUp = () => {
    window.removeEventListener('pointermove', onMove);
    window.removeEventListener('pointerup', onUp);
    if (edge) { clearInterval(edge); edge = null; }
    tab.classList.remove('dragging');
    if (!moved) return;
    // The click that follows this pointerup would select whatever the tab landed
    // on. Swallowed once, within the same gesture.
    window.addEventListener('click', (e) => { e.stopPropagation(); e.preventDefault(); },
      { capture: true, once: true });
    // Every tab carries its key; the map's type cannot say so.
    setProcOrder(wsId, [...strip.children].map((c) => /** @type {HTMLElement} */ (c).dataset.key ?? ''));
    tabDrag = null;
    renderDrawer();
  };

  window.addEventListener('pointermove', onMove);
  window.addEventListener('pointerup', onUp);
}

/* Horizontal by wheel, because the strip has no scrollbar to grab: a header 30px
   tall is not the place for one, and a dozen shells is exactly when you need to
   reach the far end. */
$('dtabs').addEventListener('wheel', (e) => {
  const strip = $('dtabs');
  if (strip.scrollWidth <= strip.clientWidth) return;
  strip.scrollLeft += e.deltaY || e.deltaX;
  e.preventDefault();
}, { passive: false });

/* Fade whichever edge of the tab strip has more tabs behind it. Driven from
 *  observers rather than `renderDrawer`, so it stays correct however the strip
 *  changes — tabs added/removed (mutation), the drawer resized (resize), or the
 *  strip scrolled by wheel, drag or the shownTab restore (scroll). */
function updateTabOverflow() {
  const strip = $('dtabs');
  const over = strip.scrollWidth - strip.clientWidth;
  strip.classList.remove('of-start', 'of-end', 'of-both');
  if (over <= 1) return;
  const atStart = strip.scrollLeft <= 1;
  const atEnd = strip.scrollLeft >= over - 1;
  strip.classList.add(atStart ? 'of-end' : atEnd ? 'of-start' : 'of-both');
}
$('dtabs').addEventListener('scroll', updateTabOverflow);
new ResizeObserver(updateTabOverflow).observe($('dtabs'));
new MutationObserver(updateTabOverflow).observe($('dtabs'), { childList: true });

/** The tab last scrolled into view, per workspace, so a snapshot does not drag
 *  the strip back while you are reading the other end of it. */
/** @type {Record<string, string | null>} */
const shownTab = {};

/** What the drawer was last built from — see `unchanged`. */
const drawerDrawn = { sig: null };

/** Hand what a process pane is showing to the session beside it.
 *
 *  **Why this exists.** A watcher's output is what explains what an agent just
 *  broke, and the only ways across were to retype it or to describe it. This types
 *  it, so it lands as an ordinary user turn — which is what it is: you pointed at
 *  it, and the transcript should read as though a human did.
 *
 *  **Agnostic on purpose.** It sends *what the pane shows*, never "the build
 *  error": no output is parsed here, no process is special, and the only name
 *  involved is the one the repo's own config gave it. A repo with three watchers
 *  gets the same behaviour three times.
 *
 *  The daemon owns *when* — only it knows whether a keystroke would land in a
 *  prompt, a permission dialog or the middle of a turn (`api::tell_session`) — so
 *  a refusal comes back as its sentence rather than being guessed at here. */
async function sendPaneToSession(/** @type {string} */ target, /** @type {string} */ label) {
  const s = currentSession();
  if (!s) return toast('no session in this workspace to send to', true);
  const text = Term.readTerm(activeCheckout(), target);
  if (!text) return toast('that pane has nothing to send', true);
  // Named, so the turn does not open with a wall of output nobody attributed.
  // The name is the config's, which is what keeps this free of any one workflow.
  const body = { text: `${label} says:\n\n${text}` };
  try {
    await call(`/api/session/${encodeURIComponent(s.id)}/tell`, body);
    toast(`sent to ${s.title || 'the session'}`);
  } catch (e) {
    toast(reason(e), true);
  }
}

/** The one menu, offered from the tab and from the pane itself.
 *
 *  Two items rather than one, because the wording is the affordance: with a
 *  selection this sends *that*, and without one it sends the tail. A single item
 *  saying "send output" would leave you guessing which. */
/** The right-click menu shared by a process tab and the pane body.
 *
 *  @param {string} target
 *  @param {string} label
 *  @returns {[string, string | null, (() => void) | null][]}
 */
function paneMenu(target, label) {
  const picked = Term.hasSelection(activeCheckout(), target);
  const to = currentSession();
  const named = to ? (to.title || 'the session') : null;
  return [
    picked
      ? [`send selection to ${named ?? 'session'}`, null,
        named ? () => sendPaneToSession(target, label) : null]
      : [`send the last lines to ${named ?? 'session'}`, null,
        named ? () => sendPaneToSession(target, label) : null],
  ];
}

export function renderDrawer() {
  if (tabDrag !== null) return;
  const wsId = currentWorkspaceId();
  const w = workspaceById(wsId);
  /* Rebuilt only when it would come out different. Every input this reads is
     listed, because unlike the rail this pane is small enough to enumerate and
     every one of them is right here in the function. A tab is a button you click
     and drag; rebuilding the strip on every snapshot took the drag target out
     from under the pointer several times a second while an agent worked. */
  if (unchanged(drawerDrawn, [wsId, w ? w.processes : null, snap.stack_up, drawerCollapsed,
    selectedProc[wsKey(wsId)], procOrder[wsKey(wsId)], drawerTouched, pendingProcFocus,
    shownTab[wsKey(wsId)]])) {
    return;
  }
  const tabs = $('dtabs');
  tabs.replaceChildren();

  /* Docker stack status, in place of the path: blue = up, red = down, and
     **nothing at all when this checkout has no stack**. `null` is the daemon
     saying there is no compose file here (and, for its first twenty seconds, that
     it has not looked yet) — drawn as `stack down` it was a red dot that never
     went out on every repo that carries no containers, which is most of them. A
     label is only honest about a thing that exists. */
  const dcwd = $('dcwd');
  dcwd.replaceChildren();
  if (snap.stack_up !== null && snap.stack_up !== undefined) {
    const up = snap.stack_up === true;
    dcwd.appendChild(el('span', 'stackdot ' + (up ? 'up' : 'down')));
    dcwd.appendChild(el('span', null, up ? 'stack up' : 'stack down'));
  }

  const procs = w ? w.processes : [];
  /* The rows below spell the workspace into their URLs. `procs` is empty unless
     there is a workspace, so `wsId` is there whenever a row is drawn — encoded
     once here rather than asserted twice inside the loop. */
  const wsUrl = encodeURIComponent(wsId ?? '');
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

  const alive = (/** @type {import('../snapshot').ProcessView} */ p) =>
    p.kind.kind === 'shell' ? p.kind.exit_code == null : p.health.health !== 'dead';

  let active = selectedProc[wsKey(wsId)];
  if (!procs.some((p) => p.id === active)) {
    // Prefer something still running; a dead shell is only shown when it is
    // all there is, or when you picked it yourself.
    const fallback = (procs.find(alive) ?? procs[0])?.id ?? null;
    /* Unless it is a shell you just asked for that the snapshot has not caught up
       with: writing the fallback back would spend the claim, and the process then
       arrives to find something else selected and never takes the cursor. */
    if (active !== pendingProcFocus) selectedProc[wsKey(wsId)] = fallback;
    active = fallback;
  }

  /* Built here, appended below in the order you dragged them into. Two lists in
     one strip — what is running, and what is declared and is not — and the tab
     key is what the order is remembered by: a managed process by name, so
     `docker` keeps its place whether it is up or not and across a restart, and a
     shell by id, which is the only thing that tells two of them apart. */
  /** @type {[string, HTMLButtonElement][]} */
  const made = [];

  // Shells are numbered per workspace, and the number comes from *this* loop —
  // the daemon's order, which is creation order — not from the order you dragged
  // them into: `shell 2` has to keep meaning the second one you opened. Without a
  // number every dead one renders as the same "shell (0)".
  let shellNo = 0;
  for (const p of procs) {
    const isShell = p.kind.kind === 'shell';
    if (isShell) shellNo += 1;
    const dead = !alive(p);

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
    /* The tab's menu, which is the same one the pane body offers. It goes here
       rather than on a fourth glyph: the tab already holds a dot, a label, ✕, ⟳
       and a drag, and "type this into your agent" one stray click from Close and
       Restart is the wrong neighbourhood. */
    tab.oncontextmenu = (ev) => openMenu(ev, paneMenu(`proc:${p.id}`, label));

    // The same glyph every other dismiss uses; this one was a multiplication sign.
    const x = el('span', 'x', '\u2715');
    x.title = dead ? 'Dismiss' : 'Close';
    x.onclick = (ev) => {
      ev.stopPropagation();
      Term.close(activeCheckout(), `proc:${p.id}`);
      call(`/api/process/${encodeURIComponent(p.id)}/close`).catch((e) => toast(e.message, true));
    };
    tab.appendChild(x);

    if (p.kind.kind === 'managed') {
      const r = el('span', 'x', '⟳');
      r.title = 'Restart';
      r.onclick = (ev) => {
        ev.stopPropagation();
        Term.close(activeCheckout(), `proc:${p.id}`);
        call(`/api/workspace/${wsUrl}/process/${encodeURIComponent(p.name)}/restart`)
          .catch((e) => toast(e.message, true));
      };
      tab.appendChild(r);
    }
    made.push([isShell ? p.id : p.name, tab]);
  }

  /* Declared and not running (`stopped_processes`). A hollow dot, no ✕, and a
     click starts it — the drawer used to list only what autostarted, which left
     a `docker compose up` that is deliberately not autostarted with no way in:
     the restart button its config comment points at is drawn on a tab, and there
     was no tab until something started it. */
  for (const name of w ? w.stopped_processes : []) {
    const tab = el('button', 'dtab stopped');
    tab.title = `${name} is declared and not running`;
    // The same hollow dot an archived session uses: declared, not running.
    tab.appendChild(el('span', 'dot archived'));
    tab.appendChild(el('span', null, name));
    /* Starting is the ⟳, not the tab. Every other tab in this strip selects on
       click, so a tab that instead *launches* something is the one place a
       misplaced click costs you a `docker compose up` — and the running tabs put
       their restart behind the same glyph, so this is one gesture rather than two.
       The tab itself stays clickable for the drag and does nothing else. */
    const go = el('span', 'x', '⟳');
    go.title = `Start ${name}`;
    go.onclick = (ev) => {
      ev.stopPropagation();
      setDrawerTouched(true);
      call(`/api/workspace/${wsUrl}/process/${encodeURIComponent(name)}/restart`)
        // You pressed it to watch it come up, so land on it. The response carries
        // the id; the snapshot that will carry the tab has not arrived yet.
        .then((r) => { setSelectedProc(wsId, r.process); renderDrawer(); })
        .catch((e) => toast(e.message, true));
    };
    tab.appendChild(go);
    made.push([name, tab]);
  }

  /* Your order. Stable, and a key the order has never seen sorts last — which is
     where a process you have just started belongs. */
  const order = procOrder[wsKey(wsId)] || [];
  const place = (/** @type {string} */ k) => (order.indexOf(k) < 0 ? order.length : order.indexOf(k));
  made.sort((a, b) => place(a[0]) - place(b[0]));
  for (const [k, tab] of made) {
    tab.dataset.key = k;
    tab.onpointerdown = (ev) => startTabDrag(ev, tab, wsId);
    tabs.appendChild(tab);
  }

  // Only when the selection actually moved: doing it every snapshot would drag
  // the strip back while you are reading the far end of it.
  if (active && shownTab[wsKey(wsId)] !== active) {
    shownTab[wsKey(wsId)] = active;
    tabs.querySelector('.dtab[aria-selected="true"]')
      ?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  }

  const shown = Term.show(activeCheckout(), active ? `proc:${active}` : null, $('drawerbody'));
  /* The pane's own menu, which is where this feature is really used: you select
     the lines that matter and send *those*. Registered on the wrapper rather than
     on the host xterm builds, because `Term.show` replaces hosts and a listener on
     one would go with it — and the wrapper is the element that survives.
     Set every render, which is idempotent: one property, one handler. */
  $('drawerbody').oncontextmenu = (ev) => {
    if (!active) return;
    const p = procs.find((x) => x.id === active);
    if (!p) return;
    // The tab's own label, so the turn says the same word the strip does.
    const label = p.kind.kind === 'shell' ? 'the shell' : p.name;
    openMenu(ev, paneMenu(`proc:${active}`, label));
  };
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
  if (failing && selectedProc[wsKey(wsId)] !== failing.id && !drawerTouched) {
    selectedProc[wsKey(wsId)] = failing.id;
    Term.show(activeCheckout(), `proc:${failing.id}`, $('drawerbody'));
  }
}
