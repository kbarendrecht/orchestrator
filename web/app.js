'use strict';

// The SPA is a module now, so what it reaches for is written down. `core.js` holds
// the primitives every part needs; `queue.js` is the first seam extracted whole.
import {
$, el, toast, reason, safeHref, call, callHost, get, activeCheckout, CHECKOUTS, setCheckouts, HOST, snapshotOf, repoSummary, everySession, enterCheckout, snap, receive, keyActivate, setZoom, setUiPx, uiPx, saveZoom, onScaleChange, ZOOM, selected, setSelected, onSelection, prForWorkspace, terms, CHROME, stateLabel, dotClass, isWaiting, isArchived, byNewest, currentSession, activeWorkspaceId, currentWorkspaceId, closeMenu, menuOpen, newSession, newWorktree, newShell, mainWorkspace, workspaceById, prState, handedToPr, drawerCollapsed, setDrawerCollapsed, pendingSelect, setPendingSelect, onDrawerChange, onCreatingChange, creating, creatingIn, startingShown, appMod, IS_MAC, MOD_LABEL, closeLegend, typingElsewhere, mark, reportBoot, confirmBox, dialogOpen, dismissDialog, unchanged, tick,
} from './js/core.js';
import { onThemeChange } from './js/theme.js';

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
/* The terminals are the one consumer that cannot read a CSS custom property —
   xterm takes hex strings — so the theme is announced and they register, the
   same inversion the UI scale already uses. */
onThemeChange(() => Term.applyTermTheme());

// Collapsing the drawer redraws it and gives the terminal above its height back;
// xterm only refits on an explicit nudge, not on a sibling's size change.
onDrawerChange(() => { Drawer.renderDrawer(); Term.refit(); });
// A create claiming or releasing the `+`. Straight through rather than queued: the
// whole point is that the frame after the press shows something, and a worktree
// being cut may be the only thing happening, so there is no snapshot behind it.
/* A press of `+` has to land somewhere before the daemon answers, and until now
   it landed nowhere: the rail was unchanged and the centre pane went on showing
   the session you were leaving. The rail grows a placeholder row (`startingRow`)
   and the terminal region says so over the top. Both come off on the same
   announcement, when the create ends however it ends — including a refusal, where
   the toast is the answer and this must not be left standing. */
onCreatingChange(() => {
  Rail.render();
  renderStarting();
});

/** The overlay over the terminal region while a worktree is being cut.
 *
 *  Two sources, because they answer different halves. The page knows *that* a
 *  create is in flight and what it asked for, from the press — the daemon cannot
 *  say so until it has been asked. The daemon knows which script is running and
 *  what it has printed, which the page cannot see at all.
 *
 *  Read off the creating checkout's own snapshot rather than `snap`: switching to
 *  another session is exactly what this now allows, and `snap` follows the
 *  selection, so the report would otherwise be about whichever checkout you
 *  wandered into. */
function renderStarting() {
  const what = creating();
  $('startwhat').textContent = what ? `${what}\u2026` : 'Starting\u2026';
  $('termstarting').hidden = !startingShown();
  const out = $('startout');
  const where = creatingIn();
  const run = what && where ? snapshotOf(where)?.create_run : null;
  const lines = run?.lines ?? [];
  out.hidden = lines.length === 0;
  if (out.hidden) return;
  // The step is the heading the lines are under, so it travels with them rather
  // than replacing the chip's own word — the chip says what you asked for, this
  // says which script is answering.
  const said = (run?.step ? [`${run.step}:`, ...lines] : lines).join('\n');
  if (out.textContent !== said) {
    out.textContent = said;
    // Newest at the bottom, in view. Only on a change, so a render that said
    // nothing new does not fight a scroll back through the tail.
    out.scrollTop = out.scrollHeight;
  }
  out.classList.toggle('failed', !!run?.failed);
}

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
  if (!ws || ws !== Diff.state.ws) void Diff.close();
}

/** Repaint the board, at most once a frame.
 *
 *  **The daemon pushes snapshots faster than a screen refreshes.** `notify` is
 *  called from about seventy places and `post_tool_use` calls it once per tool
 *  call, so a working agent produces several a second — and each one rebuilt the
 *  rail, the context bar, the drawer, the changed-files pane (up to
 *  `CHANGED_CAP`, which is 500 rows), the queue and three status bars. All of it
 *  on the same main thread as the xterm parse and paint, so a keystroke's echo
 *  queued behind however many boards had been drawn since. That is the reported
 *  typing lag, and none of those extra passes was ever seen: two renders inside
 *  one frame paint once.
 *
 *  Only the *painting* is coalesced. `receive` still takes every snapshot the
 *  moment it lands, because it sets the snapshot and the clock it is measured
 *  against together, and the terminal teardown and selection fixups beside it
 *  still run per snapshot. A frame late is invisible; a snapshot missed is not.
 *
 *  A gesture still renders straight through — see the `onSelection` caller. It is
 *  one pass, and a click should not wait for the next frame to show anything. */
let renderQueued = false;
function scheduleRender() {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => {
    renderQueued = false;
    render();
  });
}

function render() {
  syncDiffToSession();
  Rail.render();
  // The create in flight reports through the snapshot, so its overlay is redrawn
  // with everything else rather than only when the press changed.
  renderStarting();
  renderContext();
  Drawer.renderDrawer();
  Diff.renderFiles();
  Queue.render();
  // The review overlay reacts to its session's ask on this same tick — a
  // decision ask means the read is done, a post ask means the change is. It reads
  // the ask from the snapshot, so no polling and no second source of truth.
  Review.tick();
  Review.bar();
  renderInteraction();
  renderUpdate();
  renderAgentUpdate();
  renderAgentError();
  // Last, because it reads which of the three above ended up showing. Each of them
  // sets its own `hidden` and nothing else; where they sit is decided once, here.
  stackBars();
  renderLegalNotice();
}

/** The one option value the review overlay owns.
 *
 *  An ask carrying it is a checkpoint in the review flow, and the cards answer
 *  it. An ask that does not is the agent asking something of its own, and every
 *  rule below turns on that difference. */
const DECISIONS = 'decisions';

const askDrawn = { sig: null };

/** The ask the user has folded away, by its id.
 *
 *  Per ask rather than a plain flag, so the next question opens by itself: a box
 *  you shut once must not swallow the one after it. Folded, never dismissed —
 *  the agent is still stopped, so a control that made the question go away would
 *  be this box disagreeing with the rail and the waitbar beside it. */
/** @type {string | null} */
let askFolded = null;

/* Whether the free-text box is open, by the option it belongs to.
 *
 * In the page rather than read off the DOM, because it is an input to the guard
 * above: the box replaces the option row, and `back to the options` puts the row
 * back by re-rendering. Without this the guard saw an unchanged signature and
 * that button did nothing. */
/** @type {string | null} */
let askFree = null;

/** Fold the open question away, or open it again. */
function foldAsk(/** @type {string} */ id) {
  askFolded = askFolded === id ? null : id;
  renderInteraction();
}

/** Fold whatever the box is showing, for the `Esc` chain. */
function foldOpenAsk() {
  const s = currentSession();
  const q = s && s.interaction && !s.interaction.answer ? s.interaction : null;
  if (q) foldAsk(q.id);
}

/** Is the question showing over the terminal right now?
 *
 *  Read off the DOM rather than re-derived: every rule about whether it renders
 *  is in `renderInteraction`, and a second copy of them is a second answer. */
function askShowing() {
  const host = $('oq');
  return !host.hidden && !host.classList.contains('min');
}

/** The question the selected session is blocked on.
 *
 *  Rendered from the snapshot rather than held locally, so it survives a reload
 *  and shows up in every window at once: the agent is stopped until somebody
 *  answers, and which browser you happen to be looking at is not part of that.
 *
 *  Over the terminal on purpose. The agent could print the question into its own
 *  pane, but then answering means typing into a wall of scrollback, and a
 *  question that scrolls away is one nobody notices. Which is also why folding it
 *  is the only way to get it out of the way: the question stays put. */
function renderInteraction() {
  const host = $('oq');
  const s = currentSession();
  const q = s && s.interaction && !s.interaction.answer ? s.interaction : null;
  // The PR a review pass is answering, or null for every other session. Its
  // checkpoints are the overlay's cards, so this box behaves differently below.
  // Both commands, because the triage pass reaches the same cards.
  const rvPr = q && s?.pass
    && (s.pass.command === 'review' || s.pass.command === 'triage') ? s.pass.pr : null;
  const mine = !!q && q.options.some((o) => o.value === DECISIONS);
  /* **Rebuilt only when it would come out different**, like every other pane.
     This one had no guard, and the box is up precisely while an agent is taking
     turns: ~7 snapshots a second, each one replacing the header and the option
     buttons under the pointer, so `:hover` strobed and a click split across two
     rebuilds was never delivered. It also threw away what you had typed into the
     free-text answer on every push, which is the same fault with a worse cost.
     The overlay's claim is in the signature because a box hidden while the cards
     own the ask has to come back when they let go of it. */
  if (unchanged(askDrawn, [s && s.id, q, rvPr, mine, Review.state.session, askFolded, askFree])) return;
  if (!q || !s) { host.hidden = true; host.replaceChildren(); return; }

  /* **Only a checkpoint belongs to the overlay.** This used to be true of every
     ask a review session made, and that is what stranded one: the session hit a
     problem, asked about it in its own words, and this box refused to render the
     answer while the cards had no idea the question existed. It could be answered
     from neither place, and the session sat on "needs your call" for good.
     Anything the overlay does not own is answered right here, like any other
     session's question. */
  /* **Whether the overlay is open or not.** It used to be `&& Review.state.open`,
     which was right while the flow ended by opening the overlay for you: closed,
     the box was the only way in. The bar is that now, and it says the same thing
     one line lower ("triage done · 7 threads need your call"), so the pair read as
     two questions where there is one. The cards are still a chord away. */
  if (rvPr !== null && mine && Review.state.session === s.id) {
    host.hidden = true; host.replaceChildren(); return;
  }
  const folded = askFolded === q.id;

  host.replaceChildren();
  host.className = folded ? 'oq min' : 'oq';
  const head = el('div', 'oqh');
  head.appendChild(el('span', 'dia', '\u25C6'));
  head.appendChild(el('span', null, 'needs your call'));
  if (q.thread_id) head.appendChild(el('span', 'oqt', q.thread_id));
  /* The way out of a box that covers the terminal. It answers nothing — the
     question stays open and the rail goes on saying so — it only gets the detail
     off the pane you were trying to read. `Esc` does the same. */
  const fold = el('button', 'oqfold', folded ? '\u25BE' : '\u00D7');
  fold.title = folded ? 'Show the question · Esc' : 'Fold it away, still unanswered · Esc';
  fold.setAttribute('aria-expanded', folded ? 'false' : 'true');
  fold.setAttribute('aria-label', folded ? 'Show the question' : 'Fold the question away');
  fold.onclick = (ev) => { ev.stopPropagation(); foldAsk(q.id); };
  head.appendChild(fold);
  host.appendChild(head);
  if (folded) {
    // The header is the whole box now, so clicking it is the obvious way back.
    head.onclick = () => foldAsk(q.id);
    host.hidden = false;
    return;
  }

  host.appendChild(el('div', 'oqq', q.question));
  // Whatever the agent thought you needed to see to decide: a diff, a file, the
  // reviewer's words. Shown verbatim, in the diff's own type.
  if (q.detail) host.appendChild(Diff.detailEl(q.detail));

  const opts = el('div', 'oqopts');
  // The way back into the cards, first because it is the answer to the question.
  // Only for a checkpoint: on any other ask this button pointed at a screen that
  // could not answer it.
  if (rvPr !== null && mine) {
    const back = el('button', 'oqopt');
    back.appendChild(el('div', 'ol', 'back to the review'));
    back.appendChild(el('div', 'od', 'the cards are where this is answered'));
    back.onclick = () => Review.open(rvPr);
    opts.appendChild(back);
  }
  for (const o of q.options) {
    /* The overlay's own payload — the decision set, the replies as edited —
       carried in a free-text option. Answering it here would send the agent prose
       where it parses JSON, so it is dropped by *value*. It used to be dropped for
       being free-text at all, which took every ad-hoc question's only answer with
       it: the prompt's own template gives an ask one free option, so a session
       asking anything else offered nothing to press. */
    if (rvPr !== null && o.value === DECISIONS) continue;
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
function openFreeAnswer(/** @type {HTMLElement} */ opts, /** @type {string} */ session, /** @type {string} */ ask, /** @type {import('../web/snapshot').InteractionOption} */ option) {
  if (opts.querySelector('.oqfree')) return;
  askFree = option.value;
  const wrap = el('div', 'oqfree');
  const box = el('textarea', 'box');
  box.setAttribute('aria-label', option.label);
  box.placeholder = 'Say what you want instead. It reaches the agent as written.';
  wrap.appendChild(box);

  const row = el('div', 'oqfoot');
  const send = el('button', 'oqsend', 'send');
  send.onclick = () => {
    if (!box.value.trim()) return toast('nothing written yet', true);
    // The box is built into `opts`' parent, so it is there.
    void answerInteraction(session, ask, option.value, /** @type {HTMLElement} */ (opts.parentElement), box.value);
  };
  const back = el('button', 'oqback', 'back to the options');
  back.onclick = () => { askFree = null; renderInteraction(); };
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
async function answerInteraction(/** @type {string} */ session, /** @type {string} */ ask, /** @type {string} */ answer, /** @type {HTMLElement} */ host, /** @type {string | undefined} */ text) {
  const buttons = /** @type {NodeListOf<HTMLButtonElement>} */ (host.querySelectorAll('.oqopt, .oqsend'));
  for (const b of buttons) b.disabled = true;
  try {
    await call(`/api/session/${session}/answer`, { ask, answer, text: text ?? null });
  } catch (e) {
    toast(reason(e), true);
    for (const b of buttons) b.disabled = false;
  }
}


/* The build named in the legend's legal notice.
 *
 * Filled from the snapshot rather than written into `index.html`, because that
 * file is hand-written and a typed version number is exactly the kind of thing
 * that drifts a release behind and is never noticed. Guarded on a change so a
 * notice that cannot move is not rewritten on every snapshot. */
/** @type {string | null} */
let legendVersion = null;
function renderLegalNotice() {
  if (!snap.version || snap.version === legendVersion) return;
  legendVersion = snap.version;
  $('legalver').textContent = `orchd ${snap.version}`;
}

// The version the user dismissed this session. A newer release than this shows
// again; the same one stays hidden until the next launch.
/** @type {string | null} */
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
        : u?.tool
          ? `Update available — v${u?.latest} (you have v${u?.current})`
          : `Update available — v${u?.latest} (you have v${u?.current}). Run mise up`;
  link.href = safeHref(u?.url);
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
        : `Runs \`mise upgrade ${u?.tool}\`. Installed beside this build, so nothing `
          + 'changes until you restart, and your sessions are untouched either way.';
  go.onclick = async () => {
    // A restart takes the window down, so there is nothing to report back into:
    // the answer is the app coming back on the new version.
    try {
      await (succeeded ? callHost('/api/window/restart') : call('/api/update/upgrade'));
    } catch (e) {
      toast(reason(e), true);
    }
  };

  $('updatex').onclick = async () => {
    // A finished run is the daemon's to forget, or it comes back on the next
    // reload. The nudge itself is dismissed in the page, like it always was.
    if (done) {
      try {
        await call('/api/update/upgrade/dismiss');
      } catch (e) {
        toast(reason(e), true);
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
/** @type {string | null} */
let agentDismissed = null;

function renderAgentUpdate() {
  const bar = $('agentbar');
  const u = snap.agent_update;
  /* The daemon's own state (`update::UpgradeRun`), not a flag set on click:
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
        : `Claude Code ${u?.latest} available (you have ${u?.current})`;
  if (agentDismissed === msg) { bar.hidden = true; return; }

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
        : `Runs \`mise upgrade ${u?.tool}\`. ${safety}`;
  go.onclick = async () => {
    // A restart takes the window down, so there is nothing to report back into:
    // the answer is the app coming back. Everything else reports through this bar
    // on the next snapshot, which is why neither points at a result.
    if (succeeded) {
      try {
        await callHost('/api/window/restart');
      } catch (e) {
        toast(reason(e), true);
      }
      return;
    }
    try {
      await call('/api/agent/upgrade');
      // Nothing to point at: the button disables itself on the next snapshot, the
      // one carrying the run, and this same bar reports how it ended.
      toast(`upgrading Claude Code to ${run?.to ?? u?.latest}`);
    } catch (e) {
      toast(reason(e), true);
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

// The message the user dismissed. A *different* failure shows again; the same one
// stays hidden, the way the two update bars treat a version. Local rather than
// daemon-side because the daemon clears it itself on the next spawn — there is no
// stale state for a dismiss to have to reach.
/** @type {string | null} */
let agentErrorDismissed = null;

/** The agent would not start.
 *
 *  Board-level, because the session it happened to is already gone: a turnless row
 *  is forgotten and its worktree removed, so there is nothing left to hang a
 *  notice on. Without this the whole failure is invisible — the pane never opens
 *  and the rail never gains a row.
 */
function renderAgentError() {
  const bar = $('agenterrbar');
  const msg = snap.agent_error;
  if (!msg || agentErrorDismissed === msg) { bar.hidden = true; return; }
  $('agenterrmsg').textContent = msg;
  // Out loud as well: a failure with no row and no pane has nothing else a screen
  // reader could reach, which is the same gap `noteFor` closes in the settings pane.
  $('live').textContent = msg;
  $('agenterrx').onclick = () => {
    agentErrorDismissed = msg;
    bar.hidden = true;
    stackBars();
  };
  keyActivate($('agenterrx'));
  bar.hidden = false;
}

/** Put the bars in a column, in order, however many are showing.
 *
 *  **Computed rather than a class meaning "second".** All three are `position:
 *  fixed` at the same spot, and the old rule was one `.stacked` class that the
 *  release bar's renderer set on the agent bar — a scheme with no spelling for a
 *  third, which would have sat on top of whichever was already there. Counting the
 *  visible ones is the only thing that can be right for any combination.
 */
const BAR_IDS = ['updatebar', 'agentbar', 'agenterrbar'];
function stackBars() {
  let shown = 0;
  for (const id of BAR_IDS) {
    const bar = $(id);
    if (bar.hidden) continue;
    // 10px is the first bar's own offset and 42 the gap the one `.stacked` rule
    // used (52 - 10), so a board with two bars looks exactly as it did.
    bar.style.top = `${10 + shown * 42}px`;
    shown += 1;
  }
}


import * as Rail from './js/rail.js';




function renderContext() {
  const s = currentSession();
  const wsId = currentWorkspaceId();
  const w = workspaceById(wsId);

  /* PRs are opened against upstream while branches live on the fork (§6), so
     the header names both rather than collapsing them into one path.

     **It names the checkout too, once there is more than one.** Everything to the
     right of this strip describes one checkout, and with several in the rail the
     header is the only place that says which — the leaf alone, since two checkouts
     of one repository share the repository name and differ exactly there. */
  /* **The mark's tooltip is the one place the repository survives a single-project
     rail.** Every fold header carries `repoSummary` on hover, but a rail with one
     project draws no header at all — deliberately, since a heading naming the only
     project there is is noise — so with one open this says what the strip used to.
     With several, each says its own and this names the app instead. */
  $('brand').title = CHECKOUTS.length === 1
    ? `orchd ${snap.version || ''}\n\n${repoSummary(activeCheckout())}`.trim()
    : `orchd ${snap.version || ''}`.trim();
  $('ctxdot').className = 'dot ' + (s ? dotClass(s) : 'idle');
  $('ctxname').textContent = s ? Rail.rowName(s, { id: wsId }) : (wsId || 'no session');
  $('ctxforked').hidden = !(s && s.forked_from);
  $('ctxbranch').textContent = w ? (w.branches[0] || '') : '';
  const pr = wsId ? prForWorkspace(wsId) : null;
  const bits = [];
  if (s) bits.push(stateLabel(s));
  // Not when the session label is already the PR's, or the header says it twice.
  if (pr && s && !handedToPr(s)) bits.push(`#${pr.number} ${prState(pr)}`);
  $('ctxstate').textContent = bits.join(' · ');
  $('killbtn').style.display = s && s.alive ? '' : 'none';

}

// ---------------------------------------------------------------------------
// Drawer — available on every workspace, not just main (§9)
// ---------------------------------------------------------------------------
import * as Drawer from './js/drawer.js';

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
onSelection((id, auto) => {
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
  /* **Arriving at a review's session does not open the overlay.** It used to,
     because the overlay was that session's only view and landing on the pane left
     you with an ask box over it and no way back to the cards. The bar is that way
     back now: it names the phase, it carries `open · MOD⇧R`, and it is on screen
     exactly when the overlay is not. Reopening on arrival meant every glance at
     another session cost a full screen on the way back. */
  const s = currentSession();
  // A session created a moment ago is not in the snapshot yet. Blanking the
  // terminal here would strand it: the next snapshot sees `selected` already
  // set and never opens one.
  const shown = s ? Term.show(activeCheckout(), `session:${s.id}`, $('termwrap')) : null;
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
async function teardown(/** @type {string} */ wsId) {
  let pf;
  try {
    pf = await get(`/api/workspace/${encodeURIComponent(wsId)}/preflight`);
  } catch (e) {
    return toast(reason(e), true);
  }
  const lines = pf.checks.map((/** @type {{ passed: boolean, name: string, detail: string }} */ c) => `${c.passed ? '✓' : '✗'} ${c.name} — ${c.detail}`);
  if (!pf.can_remove) {
    return toast(`cannot remove ${wsId}:\n${lines.join('\n')}`, true);
  }
  if (!await confirmBox(`Remove worktree ${wsId}?\n\n${lines.join('\n')}`, { ok: 'Remove' })) return;
  try {
    await call(`/api/workspace/${encodeURIComponent(wsId)}/teardown`);
    toast(`removed ${wsId}`);
  } catch (e) {
    toast(reason(e), true);
  }
}

$('ovclose').onclick = Diff.close;
$('ovprev').onclick = () => Diff.step(-1);
$('ovnext').onclick = () => Diff.step(1);
$('ovmode').onclick = async () => {
  // `closeEditor` is async — it may draw a confirm box — so the guard has to
  // await it. Un-awaited, `!promise` is always false and the mode flipped while
  // "Discard unsaved edits?" was still on screen, whatever you answered.
  if (Diff.edit.on && !(await Diff.closeEditor())) return;
  Diff.state.split = !Diff.state.split;
  Diff.render();
};
$('ovedit').onclick = () => (Diff.edit.on ? Diff.closeEditor() : Diff.openEditor());
$('ovsave').onclick = Diff.saveEditor;
$('addshell').onclick = newShell;
$('keyhelpx').onclick = () => { $('keyhelp').hidden = true; };
// The visible way in, beside the gear. Its tooltip names the chord — the whole
// point is that finding the button once is how you stop needing it.
$('keysbtn').title = `Keyboard shortcuts · ${MOD_LABEL} Shift ?`;
$('addshell').title = `New shell in this workspace · ${MOD_LABEL} \` or ${MOD_LABEL} Shift T`;
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
/* Document-wide, not just the legend: two controls outside it show a chord on
   their face — the drawer's `+ Shell` and the diff editor's Save — and both used
   to carry a hardcoded glyph, which is the wrong key on one of the two platforms.
   Anything marked `data-mod` is resolved here, wherever it lives — the empty-pane
   hint and a legend *description* included: one description names a second
   spelling ("also MOD `"), and resolving the `dt`s alone left the placeholder on
   screen. Found by looking at the rendered legend; the test that checked the
   chords read `dt` text and passed happily. */
for (const dt of document.querySelectorAll('[data-mod]')) {
  const mac = dt.getAttribute('data-mac');
  if (IS_MAC && mac) dt.innerHTML = mac;
  else dt.innerHTML = dt.innerHTML.replace(/MOD/g, MOD_LABEL);
}
$('dcollapse').onclick = () => setDrawerCollapsed(!drawerCollapsed);
/* The bar itself is the second way in, on a double-click: the same gesture the
   splitter above it already takes, and the header is mostly empty space that
   looked inert. Single-click is not offered here — the bar carries the tabs, the
   `+ Shell` button and the stack badge, and a stray click while aiming at one of
   them must not fold the pane. `closest` keeps it to the background: a
   double-click that lands on a control is that control's. */
document.querySelector('.drawer-head')?.addEventListener('dblclick', (ev) => {
  const t = /** @type {HTMLElement} */ (ev.target);
  if (t.closest('button, .dtab, input')) return;
  setDrawerCollapsed(!drawerCollapsed);
});
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
function switchSession(/** @type {number} */ step) {
  /* Across every checkout, in rail order: the chord steps through what the rail
     shows, and the rail shows all of them. Stepping only within the checkout you
     are in would make the last row of one block the first row of the same block
     again, with three other blocks visible underneath. */
  const ordered = everySession()
    .filter((r) => !isArchived(r.session))
    .map((r) => r.session)
    .sort(byNewest);
  if (!ordered.length) return;
  const idx = ordered.findIndex((s) => s.id === selected);
  // Nothing selected yet (or the selection is off-rail): step in from the end so
  // `next` lands on the first row rather than the second.
  const from = idx === -1 ? (step > 0 ? -1 : 0) : idx;
  setSelected(ordered[(from + step + ordered.length) % ordered.length].id);
}

/** Move the selection into the previous or next checkout.
 *
 *  **Carrying a selection, never leaving one behind.** The checkout is derived
 *  from what is selected, so "go to that checkout" has to mean "select something
 *  in it" — the newest live session, or the newest conversation if none is
 *  running. A checkout with neither is still worth landing in: the selection
 *  clears and the rail's own `+` is what you came for.
 *
 *  @param {number} step
 */
function stepCheckout(step) {
  if (CHECKOUTS.length < 2) return;
  const here = CHECKOUTS.findIndex((c) => c.path === activeCheckout().path);
  const next = CHECKOUTS[(here + step + CHECKOUTS.length) % CHECKOUTS.length];
  // Say where you landed when there is nothing to land on: the rail's highlight
  // is otherwise the only sign that the chord did anything.
  if (!enterCheckout(next)) toast(`${next.name} has no sessions`);
}

/* **A key the app claims must not also reach the pty.**
 *
 * This runs in the capture phase, so it sees a keystroke before the terminal
 * does — but `preventDefault` alone does not stop the event travelling on, and
 * xterm's keydown path never asks whether it was defaulted (one `defaultPrevented`
 * in the whole vendored build, in `keyup` bookkeeping). So the key was handled
 * *and* written to the pty: measured, closing the legend with `Esc` while an agent
 * pane had focus sent `1b` on that session's socket, which is an interrupt to
 * Claude Code — and two of them in a row is its rewind picker.
 *
 * One rule in one place, rather than a second call on every branch: whatever the
 * map claimed, the terminal does not see. `defaultPrevented` is exactly the
 * question "did we take it", because every branch that acts calls
 * `preventDefault`. */
window.addEventListener('keydown', (e) => {
  keymap(e);
  if (e.defaultPrevented) e.stopPropagation();
}, true);

function keymap(/** @type {KeyboardEvent} */ e) {
  /* First in the chain, because it is modal and the topmost thing on screen: a
     confirm drawn over the review overlay has to be the thing `Esc` answers, or
     the overlay closes underneath the question about it. Cancelling is the safe
     answer, which is what `Esc` means everywhere else here too. */
  if (e.key === 'Escape' && dialogOpen()) {
    e.preventDefault();
    dismissDialog();
    return;
  }
  /* Modal means modal: below this line the map is about the app's own panes, and
     a bare Enter aimed at a confirm would otherwise also accept the review card
     behind it. The dialog draws its own Enter (`dlgOpen` in core.js), and Escape
     is the branch above. */
  if (dialogOpen()) return;
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
  // Last of the overlays, and it refuses to close when there is nothing behind
  // it — see `Open.close`.
  if (e.key === 'Escape' && Open.isOpen() && Open.close()) {
    e.preventDefault();
    return;
  }
  if ((e.metaKey || e.ctrlKey) && e.key === 's' && Diff.edit.on) {
    e.preventDefault();
    void Diff.saveEditor();
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
      Review.send();
      return;
    }
    // Bare only: a modified key is the app's (the Ctrl layer), never a card's.
    if (!typing && !e.altKey && !e.ctrlKey && !e.metaKey && Review.key(e)) {
      e.preventDefault();
      return;
    }
  }
  /* Below the overlays and above the terminal, which is where the box itself
     sits. It folds rather than closes: `Esc` dismisses the topmost thing, and the
     topmost thing here is a panel over the pty, not the question in it. */
  if (e.key === 'Escape' && askShowing() && !Review.state.open && !Diff.state.open) {
    e.preventDefault();
    // The same rule the review overlay follows: blur first, because folding out
    // of a half-written free-text answer would discard it.
    const writing = /** @type {HTMLElement} */ (e.target).closest?.('.oq textarea');
    if (writing) /** @type {HTMLElement} */ (e.target).blur();
    else foldOpenAsk();
    return;
  }
  if (Diff.state.open) {
    if (e.key === 'Escape') { e.preventDefault(); void Diff.close(); return; }
    // j/k steps through the changeset, matching the review overlay's motion so
    // "next/previous in a list" is one idiom everywhere. Guarded on not-typing
    // because the diff hosts an editor. Ctrl+←/→ stays as an alias — it was the
    // only binding before, so muscle memory keeps working; it was itself once
    // F7/⇧F7, one key doing two jobs by modifier.
    const typingInDiff = !!/** @type {HTMLElement} */ (e.target).closest?.('textarea, input, [contenteditable="true"]');
    if (!typingInDiff && !e.ctrlKey && !e.altKey && !e.metaKey && (e.key === 'j' || e.key === 'k')) {
      e.preventDefault();
      void Diff.step(e.key === 'j' ? 1 : -1);
      return;
    }
    if (e.ctrlKey && (e.key === 'ArrowLeft' || e.key === 'ArrowRight')) {
      e.preventDefault();
      void Diff.step(e.key === 'ArrowLeft' ? -1 : 1);
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
      e.preventDefault(); void newShell(); return;
    }
    // Back to a review from anywhere. Shift, like the rest of this layer, and `r`
    // was free; a browser tab spends it on a hard reload, the same trade `Ctrl+N`
    // and `Ctrl+Shift+N` already make for the webview this is built for.
    if (e.shiftKey && k === 'r') {
      if (!Review.state.session) return;   // nothing to go back to; let the pty have it
      e.preventDefault();
      if (Review.state.open) return toast('the review is already open');
      void Review.open(Review.state.pr);
      return;
    }
    // Shift, not plain: Ctrl+D is EOF and still has to exit a shell.
    if (e.shiftKey && k === 'd') {
      e.preventDefault();
      if (Review.state.open) return toast('close the review first');
      void (Diff.state.open ? Diff.close() : Diff.open());
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
        const main = mainWorkspace();
        if (main) void newSession(main.id);
      } else {
        // The rail's + is the named variant (Shift+click); a hotkey takes the
        // common case and lets Claude Code name it.
        void newWorktree(false);
      }
      return;
    }
    /* Step between checkouts, in rail order. `[` and `]` because they are the
       "previous / next of the same kind" pair every editor uses, and because the
       letters were spent — `Ctrl+Tab` steps sessions and stepping checkouts is the
       coarser move over the same list. Shift, like the rest of this layer.

       It moves the *selection*, because that is what the checkout is derived from:
       there is no "active checkout" to set. Landing on the newest live session in
       that checkout rather than on nothing, so the centre pane never blanks. */
    if (e.shiftKey && (e.key === '[' || e.key === ']' || e.key === '{' || e.key === '}')) {
      e.preventDefault();
      stepCheckout(e.key === '[' || e.key === '{' ? -1 : 1);
      return;
    }
    if (e.code === 'Space') {
      // The first session waiting on you — the one costing you the most, in any
      // checkout. The waitbar counts across all of them and this is the chord it
      // advertises, so the two have to answer the same question.
      e.preventDefault();
      const first = everySession().find((r) => isWaiting(r.session));
      if (first) setSelected(first.session.id);
      else toast('nothing waiting on you');
      return;
    }
    // Tab is deliberately not here: it is caught above, on Ctrl for both
    // platforms, because ⌘Tab belongs to the OS.
    // Zoom. '=' shares its key with '+'; '_' rides '-'; the numpad spells both.
    if (k === '=' || e.key === '+' || e.code === 'NumpadAdd') {
      e.preventDefault(); saveZoom(setUiPx(uiPx() + 1)); return;
    }
    if (k === '-' || e.code === 'NumpadSubtract') {
      e.preventDefault(); saveZoom(setUiPx(uiPx() - 1)); return;
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
}

/* A terminal sizes itself to its host, and the host changes size for more reasons
 * than any one event covers: a window resize, a column drag, a font-size step, or
 * a compositor handing the window back at a different size than it took it —
 * which is the one that left the centre pane short of full height until you
 * switched sessions. So watch the box rather than enumerate the causes.
 *
 * Coalesced to one refit per frame, and a refit that changes nothing sends
 * nothing. */
/** @type {ReturnType<typeof setTimeout> | null} */
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

/* **Coming back to the window takes the geometry back.** A pty has one size and
   any number of clients; the last to speak wins and nobody tells the rest, so a
   second client — `mise run shot` is the one that does this in practice — can
   leave the agent pane painting at a fraction of its box, with nothing here aware
   of it. Nothing drifted means nothing sent: the fit is unchanged and the daemon
   drops a same-size resize. See `resize` in term.js. */
window.addEventListener('focus', () => Term.refit(true));

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
/** @type {Set<string> | null} */
let waitingKnown = null;
function announceWaiting() {
  /* Every checkout, for the reason the waitbar gives — and the transition is
     measured against one set covering all of them, so a checkout being added
     does not make every session in it look newly waiting. */
  const all = everySession().map((r) => r.session);
  const now = new Set(all.filter(isWaiting).map((s) => s.id));
  const known = waitingKnown;
  if (known) {
    const fresh = [...now].filter((id) => !known.has(id));
    if (fresh.length) {
      const names = fresh.map((id) => {
        const s = all.find((x) => x.id === id);
        return s ? Rail.rowName(s, { id: s.workspace }) : id;
      });
      $('live').textContent = names.length === 1
        ? `${names[0]} needs you`
        : `${names.length} sessions need you`;
    }
  }
  waitingKnown = now;
}

/** Open the events socket for one checkout, and keep it open.
 *
 *  **One per checkout, each carrying that daemon's own token.** A checkout is a
 *  daemon, and a daemon only ever describes itself — there is no socket that could
 *  report on all of them, and the page composing N snapshots is what makes one
 *  rail over several checkouts possible.
 *
 *  **A path, not a row, and the distinction is the whole bug.** This used to take
 *  the row and close over it, arguing that a socket re-reading a global would
 *  re-aim itself after a blip and never heal. That is true of *which* checkout —
 *  and this does not read that. It re-reads the current row for the path it was
 *  given, which is the one thing that must not be frozen: a daemon that dies and
 *  starts again keeps its path and gets a new port and a new token, so the frozen
 *  row dialled a dead port every 1.5 seconds for the life of the page. The symptom
 *  was "reconnecting…" that never cleared, on a checkout the rail was drawing as
 *  live, with no session openable in it.
 *
 *  @param {string} path
 */
function connect(path) {
  /* **Looked up now, not captured when the socket was first opened.** A checkout's
     daemon can die and be started again at the same path, and the new process binds
     a fresh port and mints a fresh token — so a retry holding the row it was handed
     reconnects to a port nothing is listening on, for the life of the page. That is
     what "reconnecting…" that never clears was: the bar was telling the truth, and
     the thing behind it was dialling a dead number every 1.5 seconds. */
  const checkout = CHECKOUTS.find((c) => c.path === path);
  // Closed while a retry was pending. Nothing to connect to and nothing to retry.
  if (!checkout) { socketed.delete(path); return; }
  const sock = new WebSocket(
    `${checkout.wsBase}/ws/events?token=${encodeURIComponent(checkout.token)}`
  );
  // Connected (or reconnected): clear this checkout's drop, and the bar with it
  // once every checkout is back.
  sock.onopen = () => { dropped.delete(checkout.path); showConnBar(); };
  sock.onmessage = (ev) => {
    // Through `receive` so the snapshot and the clock it is measured against move
    // together; `snap` is a live binding, so every reader sees this.
    const state = JSON.parse(ev.data);
    receive(checkout, state);
    // The first snapshot has landed, so drop the "connecting" hold and let the
    // real board — empty or not — show. Idempotent after that.
    document.body.classList.add('ready');
    // The board is now drawable, which is the moment the window stops looking
    // broken. Everything after it is the centre pane filling in.
    mark('snapshot');
    reportBoot();
    /* A session whose pty is gone keeps its scrollback until it is dismissed, so
       terminals are only torn down when the session disappears entirely.

       **Against the snapshot that just landed, and only this checkout's
       terminals.** A snapshot describes one daemon, so asking it about another
       checkout's terminal gets the wrong answer in both directions: it would tear
       down a live pane whose checkout simply did not report, and — because
       `proc:main:ng-watch` exists in every checkout — it would *keep* a pane that
       is gone because a different daemon still has one by that name. */
    const liveProcs = new Set(
      state.workspaces.flatMap((/** @type {import('./snapshot').WorkspaceView} */ w) => w.processes.map((/** @type {import('./snapshot').ProcessView} */ p) => `proc:${p.id}`))
    );
    for (const [key, entry] of [...terms]) {
      if (entry.checkout.path !== checkout.path) continue;
      const target = key.slice(key.indexOf('\u0000') + 1);
      if (target.startsWith('session:')) {
        const id = target.slice('session:'.length);
        if (!state.sessions.some((/** @type {import('./snapshot').SessionView} */ x) => x.id === id)) Term.close(checkout, target);
      } else if (!liveProcs.has(target)) {
        // A shell that closed cleanly is gone from the snapshot; drop its
        // terminal rather than leaving a hidden host behind forever.
        Term.close(checkout, target);
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
      /* Default to whatever most needs you, among what is actually running —
         **across every checkout**, not only the first. A rail that lists several
         checkouts and lands you on an empty pane because checkout one happens to
         be idle is the blank pane this stage exists to remove. */
      const running = CHECKOUTS.flatMap((c) =>
        (snapshotOf(c.path)?.sessions ?? [])
          .filter((x) => !isArchived(x))
          .map((x) => ({ checkout: c, session: x })));
      const landing = running.find((r) => isWaiting(r.session)) || running[0];
      if (landing) {
        setSelected(landing.session.id, true);
        Term.show(landing.checkout, `session:${landing.session.id}`, $('termwrap'));
      } else {
        Term.show(null, null, $('termwrap'));
      }
    }
    scheduleRender();
    announceWaiting();
    nudgeWebkitInput();
  };
  sock.onclose = () => {
    // A dropped socket is a condition, not an error: a quiet status that clears
    // itself on reconnect (see onopen), rather than a toast that — now that
    // errors persist — would linger after the daemon came back.
    dropped.add(path);
    showConnBar();
    setTimeout(() => connect(path), 1500);
  };
}

/** Which checkouts' sockets are down right now.
 *
 *  **A set, not a flag.** With one socket "connected" and "dropped" were the same
 *  question; with one per checkout they are not, and a single flag meant the first
 *  checkout to reconnect cleared a bar that another checkout was still down
 *  behind. The rail's rows for that checkout would then be silently stale with
 *  nothing saying so — which is the one thing the bar exists to prevent.
 *
 *  @type {Set<string>}
 */
const dropped = new Set();

/** Show the dropped-connection bar while any checkout is down. */
function showConnBar() {
  // Only checkouts that are still open: one that was closed while its socket was
  // retrying must not hold the bar up forever.
  for (const path of [...dropped]) {
    if (!CHECKOUTS.some((c) => c.path === path)) dropped.delete(path);
  }
  $('connbar').hidden = dropped.size === 0;
}

/** Which checkouts already have an events socket, by path.
 *
 *  A set rather than a count, because the list changes by add and close and a
 *  second socket on one checkout would double every snapshot. Keyed on the path
 *  alone even though a restart changes the port: the socket is per checkout, and
 *  [`connect`] reads the current row every time it dials, so one entry here covers
 *  every process that ever serves that path. */
const socketed = new Set();

/** One events socket per open checkout, for any that does not have one yet. */
function connectAll() {
  for (const c of CHECKOUTS) {
    if (socketed.has(c.path)) continue;
    socketed.add(c.path);
    connect(c.path);
  }
}

/** The host's own socket: the checkout list, whenever it changes.
 *
 *  **The page's substituted list goes stale the moment anything happens** — a
 *  checkout added, closed, or restarted on a new port with a new token. A page
 *  holding the old token would be refused by the very checkout it is drawing, and
 *  reloading to find out would take every terminal in every checkout down with it.
 *
 *  So the list is reconciled in place: new checkouts get a socket, closed ones
 *  lose their terminals, and the rail redraws. Nothing else moves.
 */
function connectHost() {
  const sock = new WebSocket(
    `${HOST.wsBase}/ws/host?token=${encodeURIComponent(HOST.token)}`
  );
  sock.onmessage = (ev) => {
    const { checkouts } = JSON.parse(ev.data);
    const open = new Set(checkouts.map((/** @type {import('./js/core.js').Target} */ c) => c.path));
    /* **A checkout that came back on a new port is as gone as one that left.** Both
       leave terminals attached to a daemon that has stopped, and nothing will ever
       close their sockets for them. A restart keeps the path — the row never leaves
       the list — so comparing paths alone missed it, and the panes stayed wired to a
       dead process while the rail drew the live one beside them.

       The port is what says so. A daemon started again binds a fresh one, and its
       sessions are respawned by `auto_resume` under the same ids, so the pane is
       reopened against the new row the moment it is selected. */
    const moved = new Set(CHECKOUTS
      .filter((was) => checkouts.some((/** @type {import('./js/core.js').Target} */ now) => now.path === was.path && now.port !== was.port))
      .map((c) => c.path));
    for (const [key, entry] of [...terms]) {
      const path = entry.checkout.path;
      if (!open.has(path) || moved.has(path)) {
        Term.close(entry.checkout, key.slice(key.indexOf('\u0000') + 1));
      }
    }
    for (const path of [...socketed]) if (!open.has(path)) socketed.delete(path);
    setCheckouts(checkouts);
    // A checkout that closed takes its drop with it.
    showConnBar();
    // A selection in a checkout that is gone points at nothing.
    if (selected && !currentSession()) setSelected(null);
    connectAll();
    scheduleRender();
  };
  // The host is the process serving this page: if its socket drops, the page is
  // talking to something that is going away. Retry anyway — a reload is worse.
  sock.onclose = () => setTimeout(connectHost, 1500);
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

  // **To the host, not to a checkout.** The window belongs to whatever serves
  // the page; a daemon has no window and answers `200 {}` to the route, so these
  // aimed at `LOCAL` were six buttons that silently did nothing under the app.
  const wcmd = (/** @type {string | undefined} */ cmd) => callHost(`/api/window/${cmd}`).catch((e) => toast(e.message, true));

  for (const b of /** @type {NodeListOf<HTMLElement>} */ (
    document.querySelectorAll('.wctl-btn'))) {
    b.addEventListener('click', () => void wcmd(b.dataset.cmd));
  }

  /** How far the pointer must travel before a press on a bar becomes a drag. */
  const DRAG_SLOP = 3;

  /* **The two overlay headers drag the window too.** They look like titlebars,
     they sit where one sits, and they were the only bars in the app that did
     nothing when you pulled them — which reads as the window being stuck rather
     than as the header not being a handle. Both are `.settings-title` (the legend
     reuses the settings shell), and both are in `index.html` at boot behind
     `hidden`, so one static pass over the document still finds them.
     The guard below already spares their `×`, which is a `<button>`. */
  for (const bar of document.querySelectorAll('.top, .settings-title')) {
    bar.addEventListener('mousedown', (ev) => {
      // `addEventListener` promises the handler an `Event`; narrowing in the
      // parameter is what `strictFunctionTypes` refuses, so it happens here.
      const e = /** @type {MouseEvent} */ (ev);
      // Left button only, and only on the bar's own background: a drag that
      // swallowed clicks on the session name or the close button would make
      // the header unusable.
      if (e.button !== 0) return;
      if (/** @type {HTMLElement} */ (e.target).closest('button, input, a, kbd, .ctx-btn')) return;
      // A double-click is the OS gesture for maximise, so it must not also
      // start a drag; the compositor keeps the drag alive past mouseup, which
      // would eat the second click.
      if (e.detail > 1) return;
      /* **A press is not a drag until the pointer moves, and on macOS asking too
         early crashes the app.** `start_dragging` posts a message the event loop
         drains later, and tao's `drag_window` then hands AppKit's *current* event
         to `performWindowDragWithEvent:`, which accepts nothing but a mouse event.
         So a request still queued when the next real event arrives is handed that
         one instead: press the header, press a key, and a keyDown reaches it. The
         Objective-C exception aborts the process, which on this app is the daemon
         and every session with it.
         tao does try to substitute a synthetic mouse-down, and **its guard does not
         fire**: `tao-0.35.3` compares the event type against `0x15`, which is 21,
         while `NSEventTypeApplicationDefined` is 15. Read from the vendored source.
         The shell refuses the call outright when AppKit is not on a mouse event
         (`desktop/src/main.rs`), which is the half that closes this; waiting for
         movement here is what keeps the request inside a gesture in the first
         place.
         Waiting for movement means the request is only ever sent mid-gesture, with
         the button down and AppKit dispatching mouse events. A plain click on a bar
         now asks for nothing at all, which is also what a click should do. */
      const from = { x: e.clientX, y: e.clientY };
      const stop = () => {
        window.removeEventListener('mousemove', moved);
        window.removeEventListener('mouseup', stop);
        // The native drag takes the mouse, so no `mouseup` is coming once it
        // starts; losing focus is what says the gesture left the page.
        window.removeEventListener('blur', stop);
      };
      const moved = (/** @type {MouseEvent} */ m) => {
        if (Math.abs(m.clientX - from.x) + Math.abs(m.clientY - from.y) < DRAG_SLOP) return;
        stop();
        void wcmd('start-drag');
      };
      window.addEventListener('mousemove', moved);
      window.addEventListener('mouseup', stop);
      window.addEventListener('blur', stop);
    });
    bar.addEventListener('dblclick', (e) => {
      if (/** @type {HTMLElement} */ (e.target).closest('button, input, a, kbd, .ctx-btn')) return;
      void wcmd('toggle-maximize');
    });
  }

  for (const rz of /** @type {NodeListOf<HTMLElement>} */ (
    document.querySelectorAll('.rz'))) {
    rz.addEventListener('mousedown', (/** @type {MouseEvent} */ e) => {
      if (e.button !== 0) return;
      // Stop the browser starting a text selection that outlives the resize.
      e.preventDefault();
      void wcmd(`resize/${rz.dataset.edge}`);
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

const colWidth = (/** @type {{ prop: string, key: string, def: number, min: number }} */ col) =>
  parseInt(getComputedStyle(document.documentElement).getPropertyValue(col.prop), 10) || col.def;

/** Set a column, clamped so the centre always survives and so does the other one. */
function setCol(/** @type {{ prop: string, key: string, def: number, min: number }} */ col, /** @type {number} */ px) {
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
function setDrawer(/** @type {number} */ px) {
  const centre = document.querySelector('.center');
  const room = (centre ? centre.clientHeight : window.innerHeight) - TERM_MIN;
  const h = Math.round(Math.max(DRAWER.min, Math.min(px, Math.max(DRAWER.min, room))));
  document.documentElement.style.setProperty(DRAWER.prop, `${h}px`);
  return h;
}

/** xterm sizes itself to its host, and a column drag is not a window resize. */
function dragColumn(/** @type {HTMLElement} */ handle, /** @type {{ prop: string, key: string, def: number, min: number }} */ col, /** @type {boolean} */ fromLeft) {
  handle.addEventListener('mousedown', (ev) => {
    const e = /** @type {MouseEvent} */ (ev);
    if (e.button !== 0) return;
    // The titlebar's own drag handler lives under this strip.
    e.preventDefault();
    e.stopPropagation();
    handle.classList.add('dragging');
    document.body.classList.add('col-resizing');

    const move = (/** @type {MouseEvent} */ ev) => setCol(col, fromLeft ? ev.clientX : window.innerWidth - ev.clientX);
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
function dragDrawer(/** @type {HTMLElement} */ handle) {
  handle.addEventListener('mousedown', (ev) => {
    const e = /** @type {MouseEvent} */ (ev);
    if (e.button !== 0) return;
    e.preventDefault();
    e.stopPropagation();
    handle.classList.add('dragging');
    document.body.classList.add('row-resizing');

    // `.center` is in `index.html`, so a miss is the page and the code out of
    // step rather than a state to handle — the same argument `$` makes.
    const centre = /** @type {HTMLElement} */ (document.querySelector('.center'));
    const bottom = centre.getBoundingClientRect().bottom;
    const move = (/** @type {MouseEvent} */ ev) => setDrawer(bottom - ev.clientY);
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
import * as Open from './js/open.js';

Settings.setup();
setupColumns();
setupChrome();
connectAll();
connectHost();
/* The waiting clock has to tick even when nothing else changes — and ticking is
   all it does. This used to call `Rail.render()`, which opens with
   `replaceChildren`: the row under your pointer was destroyed and rebuilt every
   second, `:hover` was not re-targeted until the mouse moved, and a native
   `title` tooltip — which wants the pointer resting on one element for about half
   a second — arrived late or never. `tick` rewrites the duration strings in
   place. */
setInterval(() => { tick(); }, 1000);

/* **A host with no checkouts open is the screen this used to be a second
   application for.** First run lands here, and so does closing the last checkout,
   which is what `Host::close_checkout` means by staying symmetric down to the last
   one. Last, so the rail and the chrome are already up behind it. */
if (!CHECKOUTS.length) Open.showWelcome();

window.orchTeardown = teardown;
/* The macOS menu bar's Settings item. A native menu cannot reach a module, so
   `desktop/src/main.rs` evals this name; keep the two spellings together. It
   opens rather than toggles, because choosing Settings from a menu is never a
   request to close it. Absent on the splash and the first-run page, where the
   item therefore does nothing. */
window.orchSettings = Settings.open;

