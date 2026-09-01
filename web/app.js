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
  selectedProc, setSelectedProc, prState, handedToPr, procOrder, setProcOrder,
  drawerTouched, setDrawerTouched, drawerCollapsed, setDrawerCollapsed,
  pendingProcFocus, setPendingProcFocus, pendingSelect, setPendingSelect,
  prOf, onDrawerChange, appMod, IS_MAC, MOD_LABEL, closeLegend, typingElsewhere,
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
  // The review overlay reacts to its session's ask on this same tick — a
  // decision ask means the read is done, a post ask means the change is. It reads
  // the ask from the snapshot, so no polling and no second source of truth.
  Review.tick();
  Review.bar();
  renderInteraction();
  renderUpdate();
  // After `renderUpdate`, which decides whether the bar above this one is there
  // and therefore whether this one is stacked.
  renderAgentUpdate();
  renderLegalNotice();
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

  // The PR a review session is answering, or null for every other session. Its
  // checkpoints are the overlay's cards, so this box behaves differently below.
  const rvPr = s.kind.kind === 'automation' && s.kind.command === 'review' ? s.kind.pr : null;
  // The overlay owns the ask while it is driving that session: the decision and
  // post-go checkpoints are the cards, not a question box floating over the pty.
  if (rvPr !== null && Review.state.open && Review.state.session === s.id) {
    host.hidden = true; host.replaceChildren(); return;
  }

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
  // The way back into the cards, first because it is the answer to the question.
  if (rvPr !== null) {
    const back = el('button', 'oqopt');
    back.appendChild(el('div', 'ol', 'back to the review'));
    back.appendChild(el('div', 'od', 'the cards are where this is answered'));
    back.onclick = () => Review.open(rvPr);
    opts.appendChild(back);
  }
  for (const o of q.options) {
    // A review session's free-text option carries the overlay's own payload — the
    // decision set, the replies as edited — so answering it here would send the
    // agent prose where it parses JSON. A plain option beside it (`hold`) still
    // means what it says, and is the one thing worth being able to say without
    // waiting for the overlay to load.
    if (rvPr !== null && o.free) continue;
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


/* The build named in the legend's legal notice.
 *
 * Filled from the snapshot rather than written into `index.html`, because that
 * file is hand-written and a typed version number is exactly the kind of thing
 * that drifts a release behind and is never noticed. Guarded on a change so a
 * notice that cannot move is not rewritten on every snapshot. */
let legendVersion = null;
function renderLegalNotice() {
  if (!snap.version || snap.version === legendVersion) return;
  legendVersion = snap.version;
  $('legalver').textContent = `orchd ${snap.version}`;
}

// The version the user dismissed this session. A newer release than this shows
// again; the same one stays hidden until the next launch.
let updateDismissed = null;
function renderUpdate() {
  const bar = $('updatebar');
  const u = snap.update;
  /* The daemon's own state, for the reasons `renderAgentUpdate` gives: a local
     flag cannot learn that the run died, does not survive a reload, and is unknown
     to a second window. */
  const run = snap.self_upgrade_run;
  if ((!u && !run) || (u && updateDismissed === u.latest)) { bar.hidden = true; return; }
  const done = !!run && !run.running;
  const failed = done && !!run.tail;
  const succeeded = done && !failed;

  const link = /** @type {HTMLAnchorElement} */ ($('updatelink'));
  /* `u` can be gone while a run is not — the release check refreshes on its own
     clock — so every arm that names a version reads it off the run, which carries
     the one it is installing. */
  link.textContent = failed
    ? `v${run.to} did not install: ${run.tail.split('\n')[0]}`
    : succeeded
      ? `v${run.to} installed — restart to run it`
      : run
        ? `installing v${run.to}\u2026`
        : u.tool
          ? `Update available — v${u.latest} (you have v${u.current})`
          : `Update available — v${u.latest} (you have v${u.current}). Run mise up`;
  link.href = u?.url || '#';
  link.title = failed ? run.tail : '';

  /* No button unless mise installed this build. A `.deb` wants apt and a password,
     an AppImage and a `.dmg` are files somebody downloaded, and offering to
     upgrade what we cannot is worse than the link. */
  const go = /** @type {HTMLButtonElement} */ ($('updatego'));
  go.hidden = !(u?.tool || run);
  go.disabled = !!run && run.running;
  go.textContent = run?.running ? 'Upgrading\u2026'
    : failed ? 'Retry' : succeeded ? 'Restart' : 'Upgrade';
  go.title = failed ? run.tail
    : succeeded
      ? 'Quits and comes back on the new version. Your sessions are resumed as they were.'
      : run ? 'Running `mise upgrade`.'
        : `Runs \`mise upgrade ${u.tool}\`. Installed beside this build, so nothing `
          + 'changes until you restart, and your sessions are untouched either way.';
  go.onclick = async () => {
    // A restart takes the window down, so there is nothing to report back into:
    // the answer is the app coming back on the new version.
    try {
      await call(succeeded ? '/api/window/restart' : '/api/update/upgrade');
    } catch (e) {
      toast(e.message, true);
    }
  };

  $('updatex').onclick = async () => {
    // A finished run is the daemon's to forget, or it comes back on the next
    // reload. The nudge itself is dismissed in the page, like it always was.
    if (done) {
      try {
        await call('/api/update/upgrade/dismiss');
      } catch (e) {
        toast(e.message, true);
      }
    }
    if (u) updateDismissed = u.latest;
    bar.hidden = true;
    renderAgentUpdate();
  };
  keyActivate($('updatex'));
  bar.hidden = false;
}

/* The last thing this bar said when you waved it away. Keyed on the message, not
   on a version: every state it can be in — a newer build, a run in flight, a run
   that failed — is a different sentence, so anything new speaks up again while the
   same one stays quiet until the next launch. A version key could not tell a
   failure from the nudge that preceded it. */
let agentDismissed = null;

function renderAgentUpdate() {
  const bar = $('agentbar');
  const u = snap.agent_update;
  /* The daemon's own state (`agent_update::UpgradeRun`), not a flag set on click:
     a local "in progress" boolean has no way to learn that the run died, so it
     would sit disabled forever. It also survives a reload and shows in every
     window, which a local flag cannot. */
  const run = snap.upgrade_run;
  if (!u && !run) { bar.hidden = true; return; }
  // A finished run with nothing in its tail is the one that worked. Reported
  // rather than cleared, because the sessions you already have open go on printing
  // Claude Code's own upgrade notice — they really are still the old build — so a
  // bar that just vanished read as a button that had done nothing.
  const done = !!run && !run.running;
  const failed = done && !!run.tail;

  // A failure keeps the end of the output, which is the part that says why. Its
  // first line here, the whole tail in the tooltip: the bar is one line tall and a
  // stack trace in it would push the button off the end.
  //
  // `u` is only read in the last arm, and that is the only arm reachable with no
  // update pending: the check is refreshed when a run ends, so a run in flight can
  // outlive the nudge that started it.
  const msg = failed
    ? `Claude Code ${run.to} did not install: ${run.tail.split('\n')[0]}`
    : done
      ? `Claude Code ${run.to} installed, restart a session to pick it up`
      : run
        ? `installing Claude Code ${run.to}\u2026`
        : `Claude Code ${u.latest} available (you have ${u.current})`;
  if (agentDismissed === msg) { bar.hidden = true; return; }

  // Below the release bar when that one is up, at the top when it is not.
  bar.classList.toggle('stacked', !$('updatebar').hidden);
  $('agentmsg').textContent = msg;

  const succeeded = done && !failed;
  const go = /** @type {HTMLButtonElement} */ ($('agentgo'));
  go.disabled = !!run && run.running;
  go.textContent = run?.running ? 'Upgrading\u2026'
    : failed ? 'Retry' : succeeded ? 'Restart' : 'Upgrade';
  // Says the safe thing out loud, because "upgrade the tool my agents are
  // running" reads risky and is not: mise repoints a versioned install, so a
  // session already going keeps the binary it loaded.
  const safety = 'Sessions already running are unaffected \u2014 they finish on the '
    + 'version they started with, and the next session you open gets the new one.';
  go.title = failed ? run.tail
    : succeeded
      ? 'Quits and comes back. Your sessions are resumed as they were, on the new '
        + 'version, because a running agent goes on being the build it started as.'
      : run ? `Running \`mise upgrade\`. ${safety}`
        : `Runs \`mise upgrade ${u.tool}\`. ${safety}`;
  go.onclick = async () => {
    // A restart takes the window down, so there is nothing to report back into:
    // the answer is the app coming back. Everything else reports through this bar
    // on the next snapshot, which is why neither points at a result.
    if (succeeded) {
      try {
        await call('/api/window/restart');
      } catch (e) {
        toast(e.message, true);
      }
      return;
    }
    try {
      await call('/api/agent/upgrade');
      // Nothing to point at: the button disables itself on the next snapshot, the
      // one carrying the run, and this same bar reports how it ended.
      toast(`upgrading Claude Code to ${run?.to ?? u.latest}`);
    } catch (e) {
      toast(e.message, true);
    }
  };
  // A finished run lives in the snapshot, so dismissing it there is what makes it
  // stay dismissed: a local flag would put the same bar back on the next reload,
  // and in every other window it never left. The nudge itself has nothing to clear
  // daemon-side — it is recomputed from mise — so that one stays local.
  $('agentx').onclick = () => {
    agentDismissed = msg;
    bar.hidden = true;
    if (done) call('/api/agent/upgrade/dismiss').catch((e) => toast(e.message, true));
  };
  keyActivate($('agentx'));
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

/* The workspace whose tab strip is being dragged, or null. A snapshot arriving
   mid-drag would `replaceChildren` the strip out from under the pointer, so the
   render is skipped until the drop — which then renders once, from the order the
   drop just saved. */
let tabDrag = null;

/** Reorder the drawer's tabs by dragging one.
 *
 *  Pointer events rather than HTML5 drag-and-drop: the webview is WebKitGTK, and
 *  a native drag brings a drag image, a text selection and its own dragover rules
 *  along with it, none of which a 20px tab wants. The 4px threshold is what keeps
 *  a plain click on a tab a click. */
function startTabDrag(ev, tab, wsId) {
  if (ev.button !== 0) return;
  const strip = $('dtabs');
  const startX = ev.clientX;
  let moved = false;
  let lastX = startX;
  let edge = null;

  // Land before the first tab whose middle the pointer has passed — the same rule
  // in both directions, so there is no left/right special case.
  const placeAt = (x) => {
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
  const edgeScroll = (x) => {
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

  const onMove = (e) => {
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
    setProcOrder(wsId, [...strip.children].map((c) => /** @type {HTMLElement} */ (c).dataset.key));
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
const shownTab = {};

function renderDrawer() {
  if (tabDrag !== null) return;
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
    const fallback = (procs.find(alive) ?? procs[0])?.id ?? null;
    /* Unless it is a shell you just asked for that the snapshot has not caught up
       with: writing the fallback back would spend the claim, and the process then
       arrives to find something else selected and never takes the cursor. */
    if (active !== pendingProcFocus) selectedProc[wsId] = fallback;
    active = fallback;
  }

  /* Built here, appended below in the order you dragged them into. Two lists in
     one strip — what is running, and what is declared and is not — and the tab
     key is what the order is remembered by: a managed process by name, so
     `docker` keeps its place whether it is up or not and across a restart, and a
     shell by id, which is the only thing that tells two of them apart. */
  const made = [];

  // Shells are numbered per workspace, and the number comes from *this* loop —
  // the daemon's order, which is creation order — not from the order you dragged
  // them into: `shell 2` has to keep meaning the second one you opened. Without a
  // number every dead one renders as the same "shell (0)".
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
      call(`/api/workspace/${encodeURIComponent(wsId)}/process/${encodeURIComponent(name)}/restart`)
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
  const order = procOrder[wsId] || [];
  const place = (k) => (order.indexOf(k) < 0 ? order.length : order.indexOf(k));
  made.sort((a, b) => place(a[0]) - place(b[0]));
  for (const [k, tab] of made) {
    tab.dataset.key = k;
    tab.onpointerdown = (ev) => startTabDrag(ev, tab, wsId);
    tabs.appendChild(tab);
  }

  // Only when the selection actually moved: doing it every snapshot would drag
  // the strip back while you are reading the far end of it.
  if (active && shownTab[wsId] !== active) {
    shownTab[wsId] = active;
    tabs.querySelector('.dtab[aria-selected="true"]')
      ?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
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

/* The session the last selection landed on, so "you came back to it" can be told
   from "you were already here". Only the review overlay needs the difference, and
   it needs it badly: `go to the pane` closes the overlay and selects the very
   session the overlay is driving, so a reopen rule keyed on the session alone
   fires on the way out and the button appears to do nothing. */
let cameFrom = null;

// What picking a session means: open its terminal, redraw, and put the cursor
// where you are about to type. Registered rather than called by the rail, so the
// rail does not have to know about rendering.
onSelection((id, auto) => {
  const arrived = id !== cameFrom;
  cameFrom = id;
  // Picking a session is going back to work: the legend was an aside, and leaving
  // it up over the pane you just chose is the app arguing with you.
  closeLegend();
  // Same rule for the review overlay, which covers the centre column: picking a
  // session in the rail did change the selection, it just changed it behind an
  // overlay, so the rail looked broken. Its own session is the exception, because
  // the overlay *is* that session's view and it selects it when it spawns. So is
  // selecting nothing, which is what the snapshot does when a session ends: the
  // review session ending is when the overlay has its report to show, and the
  // snapshot then lands you on another session on its own.
  if (id && !auto && Review.state.open && id !== Review.state.session) Review.close();
  // And the other direction, which is the half that was missing: the overlay *is*
  // that session's view, so going to the session is going to the overlay. Without
  // this you landed on the pane with the generic ask box over it and no way back to
  // the cards that were supposed to answer it. Gestures only — the app picking a
  // session for you when another ends is not you asking for the review.
  //
  // On *arriving*, not on being here. `go to the pane` is the overlay closing and
  // selecting its own session in one move, and reopening on that is the button
  // undoing itself. Closed while you stand on the session, the bar is the way back.
  if (id && !auto && arrived && !Review.state.open && Review.state.session === id) {
    Review.open(Review.state.pr);
  }
  const s = currentSession();
  // A session created a moment ago is not in the snapshot yet. Blanking the
  // terminal here would strand it: the next snapshot sees `selected` already
  // set and never opens one.
  const shown = s ? Term.show(`session:${s.id}`, $('termwrap')) : null;
  render();
  // Picking a session is picking where you are about to type. After the frame
  // that un-hides it, for the same reason the drawer waits: xterm refuses focus
  // while its host still has no dimensions. Not while you are typing in a box:
  // the app picks a session for you when one ends, and that must not reach into
  // an open rename and take the keyboard.
  if (shown && !typingElsewhere()) {
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
// The visible way in, beside the gear. Its tooltip names the chord — the whole
// point is that finding the button once is how you stop needing it.
$('keysbtn').title = `Keyboard shortcuts · ${MOD_LABEL} Shift ?`;
$('keysbtn').onclick = (ev) => {
  ev.stopPropagation();
  $('keyhelp').hidden = !$('keyhelp').hidden;
};
keyActivate($('keysbtn'));

/* The legend is written once in `index.html` with `MOD` standing in for whichever
 * key this platform uses, resolved here — a hand-written second copy of the map
 * is the one thing that can silently drift from the bindings, and two of them
 * would be worse. A row whose macOS spelling differs in shape rather than just in
 * modifier carries `data-mac` and is replaced wholesale (terminal copy/paste
 * needs no Shift on a Mac, because ⌘ never reaches the pty).
 *
 * Ctrl+Tab is left alone on purpose: it is Ctrl on both platforms, since ⌘Tab is
 * the macOS application switcher and never arrives. */
for (const dt of $('keyhelp').querySelectorAll('dt[data-mod]')) {
  const mac = dt.getAttribute('data-mac');
  if (IS_MAC && mac) dt.innerHTML = mac;
  else dt.innerHTML = dt.innerHTML.replace(/MOD/g, MOD_LABEL);
}
// The empty-terminal hint teaches the same chords, so it resolves MOD the same.
for (const k of $('termempty').querySelectorAll('kbd')) {
  k.textContent = k.textContent.replace(/MOD/g, MOD_LABEL);
}
/* Descriptions too, not only the chords: one of them names a second spelling
   ("also MOD `"), and substituting the `dt` alone left the placeholder on screen.
   Found by looking at the rendered legend — the test that checked the chords read
   `dt` text and passed happily. */
for (const dd of $('keyhelp').querySelectorAll('.keys dd')) {
  if (dd.innerHTML.includes('MOD')) {
    dd.innerHTML = dd.innerHTML.replace(/MOD/g, MOD_LABEL);
  }
}
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
//   • the app modifier — **⌘ on macOS, Ctrl elsewhere** (`core.appMod`): new
//     worktree / session / shell (n, Shift+n, Shift+t), switch session (Tab /
//     Shift+Tab), jump to what needs you (Space), the diff (Shift+d), zoom
//     (= − 0), save (s). The platform comes from the daemon, which knows it at
//     compile time, not from a sniffed user agent.
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
//   * **This tension is Linux-only.** On macOS ⌘ never reaches the pty, so the
//     app layer there costs the terminal nothing and Ctrl stays entirely the
//     terminal's. Everything below is about the Ctrl spelling.
//   * Plain Ctrl+<letter> shadows the pty, so **default to `Ctrl+Shift+…`** — the
//     zone terminals leave alone, which is why copy/paste already live there. Take
//     a plain letter only when the idiom is worth the key it costs, and say what
//     the cost was: `Ctrl+n` is worth it (universal "new", costs readline's
//     next-history), `Ctrl+d` is not (the diff is `Ctrl+Shift+d`, because `Ctrl+d`
//     is EOF and still has to exit a shell). `Ctrl+n` and `Ctrl+Space` (NUL,
//     emacs set-mark) are the two that currently take something.
//   * `Ctrl+n`, `Ctrl+Shift+n` and `Ctrl+Tab` are browser-reserved (new window,
//     incognito, tab switch) and never arrive in a plain tab; they work in the
//     desktop webview, which is the primary target. The legend says so rather than
//     leaving it to be discovered.
//
// `Ctrl+Shift+?` opens the legend, the one source of truth a user can see. Keep
// it in step with these bindings — a scheme nobody can read is not predictable
// however consistent it is.

/**
 * Move the selection `step` sessions along, wrapping.
 *
 * Live sessions only, in the order the rail draws them (`byNewest`). Iterating
 * `snap.sessions` raw stepped onto archived sessions with no transcript, which
 * have no row anywhere: the centre pane went blank with nothing selected in the
 * rail, and the only way out was pressing the key again.
 *
 * `inRail` fixed that and went one row too far. It also matches archived
 * conversations, which do have a row — folded inside the archive toggle, so
 * normally not on screen — and tabbing into one drops you in a conversation that
 * is over, mid-cycle through the ones that are not. Switching is for the sessions
 * you are working in; the archive is a place you go on purpose.
 *
 * Every live session has a row, so the original bug cannot come back through here.
 */
function switchSession(step) {
  const ordered = snap.sessions.filter((s) => !isArchived(s)).sort(byNewest);
  if (!ordered.length) return;
  const idx = ordered.findIndex((s) => s.id === selected);
  // Nothing selected yet (or the selection is off-rail): step in from the end so
  // `next` lands on the first row rather than the second.
  const from = idx === -1 ? (step > 0 ? -1 : 0) : idx;
  setSelected(ordered[(from + step + ordered.length) % ordered.length].id);
}

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
  /* Session switching is Ctrl+Tab on **both** platforms, the one place the app
     modifier does not apply. ⌘Tab is the macOS application switcher: the OS takes
     it before any app sees it, so binding it there would be a key that silently
     does nothing. Ctrl+Tab is safe to take on a Mac even though Ctrl is otherwise
     the terminal's, because Tab is `^I` and Ctrl+Tab is not a distinct control
     code — there is nothing to shadow. */
  if (e.key === 'Tab' && e.ctrlKey && !e.altKey && !e.metaKey) {
    e.preventDefault();
    switchSession(e.shiftKey ? -1 : 1);
    return;
  }
  // The app layer (see the header). Some of these knowingly shadow terminal keys
  // on Linux, so the block ends with a bare `return` — any combo it does not
  // claim falls through to the pty *without* preventDefault, which is what keeps
  // Ctrl+C an interrupt, Ctrl+D an EOF and Ctrl+S flow-control.
  if (appMod(e)) {
    const k = e.key.toLowerCase();
    // Ctrl+Shift+T beside Ctrl+`: the terminal-emulator "new tab" key, in the
    // Ctrl+Shift zone terminals leave alone. A shell is a process in the drawer,
    // so this is the third rung of the same ladder as Ctrl+N / Ctrl+Shift+N.
    if (e.key === '`' || (e.shiftKey && k === 't')) {
      e.preventDefault(); newShell(); return;
    }
    // Back to a review from anywhere. Shift, like the rest of this layer, and `r`
    // was free; a browser tab spends it on a hard reload, the same trade `Ctrl+N`
    // and `Ctrl+Shift+N` already make for the webview this is built for.
    if (e.shiftKey && k === 'r') {
      if (!Review.state.session) return;   // nothing to go back to; let the pty have it
      e.preventDefault();
      if (Review.state.open) return toast('the review is already open');
      Review.open(Review.state.pr);
      return;
    }
    // Shift, not plain: Ctrl+D is EOF and still has to exit a shell.
    if (e.shiftKey && k === 'd') {
      e.preventDefault();
      if (Review.state.open) return toast('close the review first');
      Diff.state.open ? Diff.close() : Diff.open();
      return;
    }
    /* Ctrl+N keeps the "new" idiom every other app has trained into your fingers,
       and that is worth its one cost: it is readline's next-history, so a shell
       here walks history back with Ctrl+P but not forward. Weighed and accepted
       (see the TODO entry) rather than overlooked — the sole user does not use it,
       and a rebindable map is the real answer if that ever stops being true. */
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
    // Tab is deliberately not here: it is caught above, on Ctrl for both
    // platforms, because ⌘Tab belongs to the OS.
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
    /* The legend, on Ctrl+Shift+? rather than a bare `?`. It was bare first, and
       that was a straight violation of the rule at the top of this file: xterm's
       input is a `<textarea>`, so with a terminal focused — the normal state —
       the typing guard swallowed it and the legend was unreachable by keyboard.
       Dropping the guard would have been worse: `?` is a character you type.
       Matched on `code` because the key's name depends on the layout. */
    if (e.shiftKey && (e.code === 'Slash' || e.key === '?')) {
      e.preventDefault();
      $('keyhelp').hidden = !$('keyhelp').hidden;
      return;
    }
    return;
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

/* Announce a session crossing into "needs you" — the one signal the whole board
 *  is for, and the only thing nothing else says out loud — to a screen reader and
 *  to a backgrounded window. Polite (waits for a pause) and only on the transition
 *  in, so it never nags; the first snapshot seeds the set without speaking. */
let waitingKnown = null;
function announceWaiting() {
  const now = new Set(snap.sessions.filter(isWaiting).map((s) => s.id));
  if (waitingKnown) {
    const fresh = [...now].filter((id) => !waitingKnown.has(id));
    if (fresh.length) {
      const names = fresh.map((id) => {
        const s = snap.sessions.find((x) => x.id === id);
        return s ? Rail.rowName(s, { id: s.workspace }) : id;
      });
      $('live').textContent = names.length === 1
        ? `${names[0]} needs you`
        : `${names.length} sessions need you`;
    }
  }
  waitingKnown = now;
}

function connect() {
  const sock = new WebSocket(`${WS_BASE}/ws/events?token=${encodeURIComponent(TOKEN)}`);
  // Connected (or reconnected): clear the dropped-connection status.
  sock.onopen = () => { $('connbar').hidden = true; };
  sock.onmessage = (ev) => {
    // Through `receive` so the snapshot and the clock it is measured against move
    // together; `snap` is a live binding, so every reader sees this.
    receive(JSON.parse(ev.data));
    // The first snapshot has landed, so drop the "connecting" hold and let the
    // real board — empty or not — show. Idempotent after that.
    document.body.classList.add('ready');
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

    // Switch to a session we asked for as soon as the daemon reports it. `auto`,
    // because this is the app landing you on something it created for you: the
    // review overlay hands work to a session and keeps watching it, so a listener
    // reading this as "you went somewhere else" would close the screen that was
    // put up to report on it.
    if (pendingSelect && snap.sessions.some((s) => s.id === pendingSelect)) {
      const id = pendingSelect;
      setPendingSelect(null);
      setSelected(id, true);
      return;
    }

    if (!selected) {
      // Default to whatever most needs you, among what is actually running.
      const first = snap.sessions.filter((x) => !isArchived(x));
      const pick = first.find(isWaiting) || first[0];
      if (pick) {
        setSelected(pick.id, true);
        Term.show(`session:${pick.id}`, $('termwrap'));
      } else {
        Term.show(null, $('termwrap'));
      }
    }
    render();
    announceWaiting();
    nudgeWebkitInput();
  };
  sock.onclose = () => {
    // A dropped socket is a condition, not an error: a quiet status that clears
    // itself on reconnect (see onopen), rather than a toast that — now that
    // errors persist — would linger after the daemon came back.
    $('connbar').hidden = false;
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
/* The waiting clock has to tick even when nothing else changes — and ticking is
   all it does. This used to call `Rail.render()`, which opens with
   `replaceChildren`: the row under your pointer was destroyed and rebuilt every
   second, `:hover` was not re-targeted until the mouse moved, and a native
   `title` tooltip — which wants the pointer resting on one element for about half
   a second — arrived late or never. `tick` rewrites the duration strings in
   place. */
setInterval(() => { Rail.tick(); }, 1000);

window.orchTeardown = teardown;

