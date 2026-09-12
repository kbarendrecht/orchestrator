// The rail: what is running, what is waiting on you, and the PRs beside it.
// Twenty-four names, three out; the rest is how a row decides what it says.

import { $, activeCheckout, byNewest, call, callFor, bandOf, callHost, callOn, checkoutOf, CHECKOUTS, chooseBox, enterCheckout, everySession, getHost, snapshotOf, snapshotFor, repoSummary, terms, caret, clock, confirmBox, copyText, creating, dotClass, el, isArchived, isConversation, isWaiting, mainWorkspace, MOD_LABEL, newSession, newWorktree, openMenu, pending, refreshButton, selected, sessionsOf, setSelected, snap, stateClass, stateLabel, reason, toast, unchanged, setPendingSelect } from './core.js';
import * as Review from './review.js';
import * as Term from './term.js';

/* Expanded per group and kept across renders. Main's two conversations and the
 * worktrees' twenty are not the same question. */
/** @type {Record<string, boolean>} */
/** @type {Record<string, boolean>} */
const showArchived = { main: false, worktrees: false };

/* The session whose name is being edited in place, or null. A snapshot lands
 * every second and rebuilds the rail, which would blow the input away mid-type —
 * so the rebuild is held off while it is open, the same way `tabDrag` holds off
 * `renderDrawer`. `renameSession` sets it and clears it. */
/** @type {string | null} */
let editingName = null;


/* **What a tree measures is the changed-files pane's business, not the rail's.**
   A row is a dot, a name, a state and a clock; `sessionRow` says so in as many
   words ("No dirty-file count"). But every edit an agent makes triggers a sweep,
   and the new counts ride the same snapshot — so with these in the signature the
   rail was rebuilt continuously while anyone was working, and the `+` beside the
   worktrees header strobed under the pointer. `dirty_count` is here for a blunter
   reason: nothing in the SPA reads it at all.

   Matched by bare name wherever it appears, the way the `_ms` rule is. Nothing
   else in the snapshot carries these names today; a session field called
   `measured` or `ahead` would be dropped here too, and the rail would go stale on
   it rather than churn. */
const NOT_DRAWN = [
  'changed', 'changed_total', 'changed_since', 'behind', 'ahead', 'rebasing',
  'measured', 'dirty_count',
];

/** What the rail was last built from — see `unchanged`. */
const drawn = { sig: null };

function renderRail() {
  // A drag is a gesture on a node this function replaces: rebuilding mid-drag
  // drops the header out from under the pointer and the drop never lands.
  if (editingName !== null || dragging !== null) return;
  // Before the guard: the bar has its own inputs and its own guard, and being
  // skipped by the rail's would leave it saying "2 need you" after they stopped.
  renderWaitbar();

  /* The whole snapshot rather than the fields this reads, on purpose: a
     signature that lists its inputs is one refactor away from freezing the rail,
     and the rail is the one pane where stale is worse than an extra rebuild. So
     anything the daemon changes rebuilds it, and a push carrying nothing but new
     durations does not. `showArchived` and the rest are the view state the
     snapshot cannot see. */
  const states = CHECKOUTS.map((c) => snapshotOf(c.path));
  /* The active checkout is in the signature in its own right, not only through
     `selected`: activating a checkout with no sessions moves nothing else, so the
     rail would keep its old `aria-current` and the header you pressed would stay
     dim. Found by pressing one. */
  /* `folded` is a `Set`, which `unchanged` cannot compare by value — so it goes in
     as its contents. Without it a fold wrote the key and redrew nothing, and the
     rail only caught up on the next reload. */
  if (unchanged(drawn, [states, CHECKOUTS, activeCheckout().path, [...folded], showArchived,
    showPrs, picked, selected, swapInFlight], NOT_DRAWN)) {
    return;
  }

  const rail = $('rail');
  rail.replaceChildren();

  /* One block per checkout, in the order the host opened them. A checkout is a
     daemon and a daemon describes only itself, so each block is built from that
     checkout's own snapshot — there is no combined one to build from. */
  const several = CHECKOUTS.length > 1;
  for (const [i, c] of CHECKOUTS.entries()) {
    /* One block per checkout, marked as a group so a screen reader can skip it
       whole — the header is its name, and the rail is otherwise a flat list of
       rows from several places.

       **No block and no header at all on a single-checkout install**, which is
       every install today: the rail is then exactly what it always was, and a
       header naming the only checkout there is is noise. */
    const block = several ? el('div', 'co-block') : rail;
    if (several) {
      block.setAttribute('role', 'group');
      block.setAttribute('aria-label', c.name);
      rail.appendChild(block);
      block.appendChild(checkoutHead(c));
    }
    const state = states[i];
    // Folded: the header and nothing else. It still says what it is hiding — see
    // `checkoutHead`.
    if (several && folded.has(c.path)) continue;
    if (!state) {
      // A checkout whose daemon has not reported yet, or is down. The row stays
      // either way, because the row is what `reopen` acts on.
      block.appendChild(el('div', 'railbtn', c.live ? 'starting\u2026' : 'not running'));
      continue;
    }
    const main = mainWorkspace(state);
    // Main is pinned first (§9).
    if (main) block.appendChild(mainGroup(c, state, main));
    block.appendChild(worktreeGroup(c, state, main?.id));
  }

  // The one added piece of chrome, at the foot of the list where "and another
  // one" belongs. Always drawn, including on a single-checkout install: it is how
  // a second checkout is ever opened, and it replaces the header's switcher.
  rail.appendChild(addCheckoutButton());

  // Its own pane below the scroller, so it stays put while sessions scroll. It
  // describes one repository, so it follows the checkout you are in.
  $('prpane').replaceChildren(prGroup());
}

/** `+ open project`, and the menu of ways to name one.
 *
 *  A menu rather than a screen, because the answer is nearly always one of the
 *  checkouts you had open before — the host keeps that list, and the folder dialog
 *  is the way in for the one it has never seen.
 *
 *  **"Project" on the button, "checkout" everywhere in the code.** They are the
 *  same thing said to two audiences: what you pick is a folder you think of as a
 *  project, and what the host opens is a git checkout with a daemon of its own.
 *  The label is for the person standing in front of the rail; the noun stays in the
 *  code, the config and this file's own prose, because every rule about it —
 *  containment, one repository per daemon, `checkout_dir` — is a rule about a
 *  checkout and reads as nonsense about a project.
 */
function addCheckoutButton() {
  const btn = el('button', 'railbtn addco', '+ open project');
  btn.title = 'Open another project beside this one';
  btn.onclick = async (ev) => {
    let recent = [];
    try {
      ({ recent } = await getHost('/api/host/recent'));
    } catch (e) {
      // The list is a convenience; the dialog still works without it.
      toast(reason(e), true);
    }
    const items = recent.slice(0, 8).map((/** @type {{ path: string, name?: string }} */ r) =>
      [r.name, null, () => addCheckout(r.path)]);
    items.push(['browse\u2026', null, browseForCheckout]);
    openMenu(ev, items);
  };
  return btn;
}

/** Raise the native folder dialog, then open what came back.
 *
 *  A cancelled dialog answers with no path, which is an answer rather than an
 *  error. In a browser tab there is no window to raise one from, and the host
 *  says so in the sentence every other window route uses.
 */
async function browseForCheckout() {
  try {
    const { path } = await callHost('/api/host/pick');
    if (path) await addCheckout(path);
  } catch (e) {
    toast(reason(e), true);
  }
}

/** Ask the host to open a checkout, answering its one question if it asks.
 *
 *  **The question is whether to resume.** Closing a checkout keeps its session
 *  records — closing is a statement about the window, not a decision about
 *  conversations — so a path coming back is exactly when "resume those, or start
 *  empty" has to be asked rather than assumed.
 *
 *  @param {string} path
 *  @param {boolean} [resume]
 */
/** @returns {Promise<void>} */
async function addCheckout(/** @type {string} */ path, /** @type {boolean | undefined} */ resume) {
  try {
    const { result } = await callHost('/api/host/checkout',
      resume === undefined ? { path } : { path, resume });
    if (result.added === 'ask') {
      const n = result.sessions;
      const yes = await chooseBox(
        `Resume ${n} conversation${n === 1 ? '' : 's'} in ${result.path}?\n\n`
        + 'They were live when this checkout was last closed. Resuming reopens each '
        + 'one at its prompt; it re-runs nothing.',
        { ok: 'Resume', other: 'Start empty' });
      // `Esc` is neither answer: the checkout stays closed rather than opening
      // one of the two ways nobody chose.
      if (yes !== null) await addCheckout(path, yes);
      return;
    }
    toast(`opened ${result.checkout.name}`);
  } catch (e) {
    toast(reason(e), true);
  }
}

/** Close a checkout, or start its daemon again when it is down.
 *
 *  Symmetric down to the last one, which is the host's rule and not the page's to
 *  soften: an empty host is the first-run page. Confirmed, because the terminals
 *  of every session in it go.
 *
 *  @param {import('./core.js').Target} c
 */
async function closeCheckout(c) {
  // What it costs, counted rather than described: "its sessions go with it" is
  // one agent or six, and only the number makes that a decision.
  const live = (snapshotOf(c.path)?.sessions ?? []).filter((s) => !isArchived(s)).length;
  if (!await confirmBox(
    `Close ${c.name}?\n\n`
    + (live
      ? `Its daemon stops and ${live} live session${live === 1 ? '' : 's'} go with it. `
      : 'Its daemon stops. ')
    + 'The worktrees and branches stay on disk, and the conversations are kept — '
    + 'opening this checkout again offers to resume them.',
    { ok: 'Close', danger: true })) return;
  try {
    await callHost('/api/host/checkout/close', { path: c.path });
    // Every terminal in that checkout is pointed at a daemon that has stopped.
    for (const [key, entry] of [...terms]) {
      if (entry.checkout.path === c.path) Term.close(c, key.slice(key.indexOf('\u0000') + 1));
    }
  } catch (e) {
    toast(reason(e), true);
  }
}

/* Which checkouts are folded away, by path.
 *
 * **In the browser, unlike the order.** Folding is a view preference — which part
 * of the rail you are looking at right now — and it sits beside the drawer's
 * collapsed state and the column widths, which live here for the same reason. The
 * *order* is the host's, because the set and its order are one thing and the app
 * and a browser tab have to agree about a rail they both draw. */
const FOLDED_KEY = 'orch.checkoutFolded';
const folded = (() => {
  try {
    return new Set(JSON.parse(localStorage.getItem(FOLDED_KEY) || '[]'));
  } catch (e) {
    return new Set();
  }
})();

function setFolded(/** @type {string} */ path, /** @type {boolean} */ on) {
  if (on) folded.add(path);
  else folded.delete(path);
  try {
    localStorage.setItem(FOLDED_KEY, JSON.stringify([...folded]));
  } catch (e) { /* private mode: the fold still holds for this session */ }
  renderRail();
}

/** Which checkout is being dragged, while one is.
 *
 *  The rail rebuilds every second, and rebuilding the node under the pointer ends
 *  the gesture — so `renderRail` stands still while this is set. */
/** @type {string | null} */
let dragging = null;

/** ` in alpha`, for a message read away from the rail — or nothing at all when
 *  there is only one checkout.
 *
 *  **Where identity is actually missing.** Every pane sits on one row beside the
 *  identity chip, which names the checkout you are in; a modal does not, and a
 *  toast outlives the glance that raised it. `main` and `invoice` name a workspace
 *  in *every* checkout, and a session name is no more unique — so a confirmation
 *  that says "Swap branches between main and invoice?" says nothing about which
 *  pair.
 *
 *  It became reachable with the rail: a row in a checkout you are not looking at
 *  is actionable now, and `callFor` sends the action to that row's daemon. Before
 *  that there was one checkout and nothing to confuse.
 *
 *  Empty on a single-checkout install, which is every install today — naming the
 *  only checkout there is would be noise on every dialog.
 *
 *  @param {any} s a session, whose checkout is derived the way everything else is
 */
function inCheckout(s) {
  if (CHECKOUTS.length < 2) return '';
  const c = checkoutOf(s.id);
  return c ? ` in ${c.name}` : '';
}

/** The strip naming one checkout, above its groups.
 *
 *  Drawn only when there is more than one, so a single-checkout install is
 *  untouched. `aria-current` rather than a class alone, because "the one you are
 *  in" is the fact a screen reader needs and the colour is what a sighted reader
 *  gets instead.
 *
 *  @param {import('./core.js').Target} c
 */
function checkoutHead(c) {
  /* A row, not a button, because it holds three controls: the fold, the name and
     the drag. The *name* is the button — it is the only way to reach a checkout
     whose sessions have all finished, so keyboard and screen-reader users need it
     as much as anyone, and `aria-current` is the fact a screen reader gets where a
     sighted reader gets the brighter text. */
  const head = el('div', 'co-head');
  head.title = repoSummary(c);
  const band = bandOf(c.path);
  if (band) {
    head.dataset.band = String(band);
    head.style.setProperty('--band', `var(--co-${band})`);
  }
  const shut = folded.has(c.path);
  if (shut) head.classList.add('folded');
  if (!c.live) head.classList.add('down');

  /* Fold a checkout away. With three or four open the rail is longer than the
     window, and this is how you park the one you are not working in. Its own
     control rather than a click on the name, because the name already means "go
     there" and one gesture cannot mean both. */
  const fold = el('button', 'cofold');
  fold.type = 'button';
  fold.setAttribute('aria-expanded', String(!shut));
  fold.title = shut ? 'Show this checkout' : 'Fold this checkout away';
  fold.appendChild(caret());
  fold.onclick = (ev) => { ev.stopPropagation(); setFolded(c.path, !shut); };
  head.appendChild(fold);

  const name = el('button', 'co-name', c.name);
  name.type = 'button';
  /* Both the row and the name, because a title on the row is shown only where no
     child has one of its own, and the name fills most of the row. What it says is
     what the top strip used to: this is where the repository lives now. */
  name.title = repoSummary(c);
  name.setAttribute('aria-current', String(c.path === activeCheckout().path));
  name.onclick = () => { enterCheckout(c); };
  head.appendChild(name);

  if (c.clash) {
    /* Two daemons polling one repository cannot see each other's fix runs, and
       nothing else in the product would ever say so — the host finds this out
       only when the second daemon reports what it will poll. */
    const warn = el('span', 'co-clash', 'same repo');
    warn.title = `also open as ${c.clash} — their fix runs cannot see each other`;
    head.appendChild(warn);
  }
  if (!c.live) head.appendChild(el('span', 'co-clash', 'down'));
  else head.appendChild(checkoutCount(c));

  head.oncontextmenu = (ev) => openMenu(ev, [
    [shut ? 'show' : 'fold away', null, () => setFolded(c.path, !shut)],
    // A dead checkout's row exists so this can be pressed; a live one has nothing
    // to reopen.
    ['reopen', null, c.live ? null : () => reopenCheckout(c)],
    ['close', 'bad', () => closeCheckout(c)],
  ]);

  /* Drag the header to reorder the rail. HTML5 drag-and-drop rather than pointer
     maths: the rail is one column, so the only question is "above or below this
     one", and `dragover` answers it. The order is the host's — see
     `Host::order_checkouts` — so a drop posts it rather than writing a local key. */
  head.draggable = true;
  head.ondragstart = (ev) => {
    dragging = c.path;
    // Absent only for a synthetic event nothing here dispatches.
    if (!ev.dataTransfer) return;
    ev.dataTransfer.effectAllowed = 'move';
    // Firefox starts no drag at all without a payload, even one nothing reads.
    ev.dataTransfer.setData('text/plain', c.path);
  };
  head.ondragend = () => { dragging = null; renderRail(); };
  head.ondragover = (ev) => { if (dragging) ev.preventDefault(); };
  head.ondrop = (ev) => {
    ev.preventDefault();
    const moved = dragging;
    dragging = null;
    if (moved && moved !== c.path) void reorderCheckouts(moved, c.path);
    else renderRail();
  };
  return head;
}

/** How many live sessions a checkout holds, and whether any of them want you.
 *
 *  **The reason a fold is safe.** Folded, a checkout with an agent waiting on you
 *  looks exactly like an idle one, and the rail is where you would have seen it.
 *  Amber only when something is actually waiting, so the colour keeps meaning what
 *  it means everywhere else in this UI.
 *
 *  @param {import('./core.js').Target} c
 */
function checkoutCount(c) {
  const mine = (snapshotOf(c.path)?.sessions ?? []).filter((s) => !isArchived(s));
  const waiting = mine.filter(isWaiting).length;
  const label = waiting
    ? `${mine.length} · ${waiting} need${waiting === 1 ? 's' : ''} you`
    : `${mine.length}`;
  const count = el('span', 'co-count' + (waiting ? ' attn' : ''), mine.length ? label : 'idle');
  count.title = waiting
    ? `${waiting} of ${mine.length} session(s) here need you`
    : `${mine.length} live session(s)`;
  return count;
}

/** Move one checkout to where another sits, and tell the host.
 *
 *  The whole order goes up, not a pair: the host stores a list, and sending it the
 *  list it should end with is one round trip that cannot half-apply.
 *
 *  @param {string} moved
 *  @param {string} onto
 */
async function reorderCheckouts(moved, onto) {
  const paths = CHECKOUTS.map((c) => c.path).filter((p) => p !== moved);
  const at = paths.indexOf(onto);
  paths.splice(at < 0 ? paths.length : at, 0, moved);
  try {
    await callHost('/api/host/checkout/order', { paths });
    // The host answers on `/ws/host`, which is what actually moves the rail — so
    // nothing is drawn from here and the two cannot disagree.
  } catch (e) {
    toast(reason(e), true);
    renderRail();
  }
}

/** @param {import('./core.js').Target} c */
async function reopenCheckout(c) {
  try {
    await callHost('/api/host/checkout/reopen', { path: c.path });
    toast(`reopening ${c.name}`);
  } catch (e) {
    toast(reason(e), true);
  }
}


/** Dot colour for a PR, sharing the session legend so one key covers both (§9). */
function prDot(/** @type {import('../snapshot').PrView} */ p) {
  // Red first, above everything. A PR that is failing or conflicting is failing
  // whoever happens to be sitting in it, and the teal "a session holds this" used
  // to hide exactly that: you opened a session on a red PR and the row went calm.
  if (p.checks === 'failing' || p.mergeable === 'CONFLICTING') return 'build';
  if (p.session) return 'auto';           // a session is holding it
  if (p.is_draft) return 'idle';
  if (p.needs_you) return 'blocked';
  if (p.checks === 'passing') return 'ok';
  return 'idle';
}

let showPrs = true;

/** The session a pointer just picked, so the `click` behind it does not pick it
 *  again. See `sessionRow`. */
/** @type {string | null} */
let picked = null;

/** The one button on a PR row: start the pass that answers its threads.
 *
 *  **It does the thing rather than offering a menu of things.** It used to open
 *  `prMenu`, which is the same list a right-click already gives, so the row had two
 *  gestures for one menu and none for the action everybody wanted. A right-click
 *  still opens the menu; this is the verb.
 *
 *  **And the verb is the pane pass, not the overlay.** `/orchd:handle-review` in a
 *  session you watch: one agent, `AskUserQuestion` for the calls it cannot make,
 *  replies drafted and posted only on a go. The triage-into-cards flow is still
 *  here and still where a decided review is carried out — it is the menu's second
 *  review item — but the cards are not good enough to be the only way through a
 *  review yet, and a button whose result you have to learn a new screen for is a
 *  worse default than one that hands you a terminal. */
function reviewButtons(/** @type {import('../snapshot').PrView} */ p) {
  const wrap = el('span', 'prpair');
  /* `handle`, not `resolve`. GitHub has a literal "Resolve conversation" button,
     and this flow's own first paragraph says marking a thread resolved stays the
     reviewer's — so a row button saying `resolve`, beside a PR, promised the one
     thing the pass refuses to do. It also now says the same word as the skill it
     starts. */
  const btn = el('button', 'pract', 'handle');
  btn.title = `Work #${p.number}'s review threads in a pane you can take over`;
  btn.onclick = (ev) => {
    // The row is an anchor to the PR on GitHub; this is not that.
    ev.preventDefault();
    ev.stopPropagation();
    void startHandleReview(p.number, btn);
  };
  wrap.appendChild(btn);
  return wrap;
}

/* Menu copy, one convention for all of them: a lowercase verb phrase, and the
   object only when it is *not* the row you right-clicked — the row already says
   which session, so "fork session" says it twice, while "swap with main" names
   something else and earns it. No trailing ellipsis on the ones that open a
   prompt or a picker: every item here leads somewhere, so marking three of them
   is noise rather than a distinction. */

/** Everything you can start from a PR row.
 *
 *  The same list behind the `review` button and behind a right-click, because
 *  they are the same question — "do something with this PR" — and having two
 *  different menus for it is how you end up hunting for the one that has the item
 *  you want. */
/** @returns {[string, string | null, (() => void) | null][]} */
function prMenu(/** @type {import('../snapshot').PrView} */ p, /** @type {HTMLButtonElement | null} */ btn) {
  return /** @type {[string, string | null, (() => void) | null][]} */ ([
    ['open in main checkout', null, () => openPr(p.number, 'main')],
    ['open in worktree', null, () => openPr(p.number, 'worktree')],
    /* Two review verbs, and the first is the button's. The pane pass is one agent
       you watch; the read pass proposes into the cards and a later run carries them
       out. Both read the threads, which is the thing that made having two of them
       questionable — the answer for now is that the cards are not finished, so the
       flow that needs no new screen is the default and this menu is where the other
       one lives. */
    ['handle in a pane', null, () => startHandleReview(p.number, btn)],
    ['read into the cards', null, () => startTriage(p.number, btn)],
  ]);
}

/** Start a plain session on a PR: a worktree pinned to its head branch, or the
 *  main checkout moved onto it. */
async function openPr(/** @type {number} */ number, /** @type {string} */ where) {
  try {
    const r = await call(`/api/pr/${number}/open`, { where });
    setPendingSelect(r.session);
    toast(`#${number} in ${r.workspace}`);
  } catch (e) {
    toast(reason(e), true);
  }
}

/** Start the pane pass on a PR, and land on the session doing it. */
async function startHandleReview(/** @type {number} */ number, /** @type {HTMLButtonElement | null} */ btn) {
  if (btn) btn.disabled = true;
  try {
    const r = await call(`/api/pr/${number}/handle-review`);
    setPendingSelect(r.session);
    toast(`handling #${number}`);
  } catch (e) {
    toast(reason(e), true);
  } finally {
    if (btn) btn.disabled = false;
  }
}

/** Start the read pass on a PR, and land on the session doing it.
 *
 *  The overlay is deliberately not opened. The pass takes minutes of somebody
 *  else's work, and a full screen saying so is a window spent on one sentence:
 *  the bar carries it beside the pane where the agent's own questions appear, and
 *  `MOD⇧R` is how you go to the cards once it says they are there. */
async function startTriage(/** @type {number} */ number, /** @type {HTMLButtonElement | null} */ btn) {
  // No button when this came from a right-click on the row.
  if (btn) btn.disabled = true;
  try {
    const r = await call(`/api/pr/${number}/triage`);
    Review.adopt(number, r.session);
    setPendingSelect(r.session);
    toast(`reading #${number}`);
  } catch (e) {
    toast(reason(e), true);
  } finally {
    if (btn) btn.disabled = false;
  }
}

/** A hand-triggered run against a PR. `action` is the endpoint; `label` is what
 *  the button says, because the endpoint's name is not the useful word on a row.
 *  A refusal from the guard table is shown verbatim: it is the whole point of
 *  triggering by hand. */
function actionButton(/** @type {import('../snapshot').PrView} */ p, /** @type {string} */ action, /** @type {string} */ label) {
  const b = el('button', 'pract', label);
  b.title = 'Rebase on develop, fix what CI says, push — in a pane you can take over';
  b.onclick = async (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    b.disabled = true;
    try {
      const r = await call(`/api/pr/${p.number}/${action}`);
      setPendingSelect(r.session);
      toast(`${label} ${p.number}`);
    } catch (e) {
      toast(reason(e), true);
    } finally {
      b.disabled = false;
    }
  };
  return b;
}

function prGroup() {
  const prs = snap.prs || [];
  // Just `ws`: the pinned pane it lives in owns the sizing, and carrying
  // `prblock` here too applied max-height twice, nested.
  const group = el('div', 'ws');
  /* It lists one checkout's PRs — the one you are in — and it sits below the
     scroller, so the block whose colour would have said which has scrolled away.
     The band says it instead. Only with several checkouts open, like every other
     piece of this chrome. */
  const band = CHECKOUTS.length > 1 ? bandOf(activeCheckout().path) : null;
  if (band) {
    group.classList.add('pr-of-checkout');
    group.style.setProperty('--band', `var(--co-${band})`);
  }

  const head = el('button', 'prgroup-head');
  head.setAttribute('aria-expanded', String(showPrs));
  head.appendChild(caret());
  head.appendChild(el('span', 'eyebrow', 'PRs'));

  // The summary sits where the detail already is, rather than duplicated at
  // the top of the rail (§9).
  const count = el('span', 'prcount');
  if (snap.pr_error) {
    count.appendChild(el('b', 'f', 'unavailable'));
    head.title = snap.pr_error;
  } else {
    const needs = prs.filter((p) => p.needs_you).length;
    const failing = prs.filter(
      (p) => p.checks === 'failing' || p.mergeable === 'CONFLICTING').length;
    const bits = [`${prs.length}`];
    if (needs) bits.push(`${needs} needs you`);
    if (failing) bits.push(`${failing} failing`);
    count.appendChild(el('b', null, bits.join(' · ')));
    // The `<b>` was appended two lines above, so it is there.
    if (needs) count.querySelector('b')?.classList.add('n');
    // How long since a poll actually landed. Live-ticked off the snapshot clock
    // like the rail's other ages, so a poller that is stuck without erroring
    // reads as stale rather than current. Hidden while a fetch is in flight.
    if (snap.pr_age_ms != null && !snap.pr_polling) {
      count.appendChild(clock('prage', snap.pr_age_ms, ' ago', ' · '));
    }
  }
  head.appendChild(count);
  /* No badge for the token's source. It used to carry a `⚠` when the token came
     from `gh auth token`, whose scopes are wider than §6 wants — but that is the
     fallback which makes the app work at all, so the mark was permanent, could not
     be acted on without setting up a PAT, and sat next to the PR count as if
     something were wrong. `token_source` is still in the snapshot for anyone
     diagnosing over the API; it is just not a thing to look at every day. */
  head.appendChild(refreshButton('pr', snap.pr_poll ?? 0, '/api/prs/refresh', snap.pr_polling));
  head.onclick = () => { showPrs = !showPrs; renderRail(); };
  group.appendChild(head);

  if (!showPrs) return group;

  if (snap.pr_error) {
    const e = el('div', 'railbtn', snap.pr_error.slice(0, 120));
    e.style.color = 'var(--bad)';
    group.appendChild(e);
    return group;
  }
  if (!prs.length) {
    group.appendChild(el('div', 'railbtn', 'none open'));
    return group;
  }

  for (const p of prs) {
    // Rows for PRs that already have a session are dimmed, and the chip at the
    // end of the row goes to it (§9).
    const row = el('a', 'prrow' + (p.session ? ' linked' : ''));
    row.href = p.url || '#';
    row.oncontextmenu = (ev) => openMenu(ev, prMenu(p, null));
    // ⌘-click, middle-click and copy-link all behave, and the browser already
    // holds the GitHub session.
    row.target = '_blank';
    row.rel = 'noreferrer';
    row.appendChild(el('span', 'dot ' + prDot(p)));
    row.appendChild(el('span', 'num', `#${p.number}`));
    row.appendChild(el('span', 'ttl', p.title, p.title));

    const auto = (snap.automation || {})[p.number];
    const needsResolve = p.needs_you;
    const needsFix = p.checks === 'failing' || p.mergeable === 'CONFLICTING';

    // A reason chip next to a button just repeats it and steals width from the
    // title, which is the part you actually read.
    if (!needsResolve && !needsFix) {
      const why = [];
      if (p.unresolved_capped) why.push('50+ threads');
      if (p.children && p.children.length) why.push(`${p.children.length} stacked`);
      if (p.is_draft) why.push('draft');
      if (why.length) row.appendChild(el('span', 'link', why[0]));
    }

    // Both buttons are hand-triggered, and no poll result ever presses one: a PR
    // going red is not a reason for anything to start, which is what keeps the
    // guard table a gate you read rather than one that trips behind you. A run can
    // also arrive here already going, from a review handing on the CI it is not
    // allowed to fix — still something a person set off, by sending the decisions.
    if (auto && auto.state === 'running') {
      const b = el('span', 'pract running', 'fixing');
      b.title = 'Go to the run';
      b.onclick = (ev) => { ev.preventDefault(); ev.stopPropagation(); setSelected(auto.session); };
      row.appendChild(b);
    } else {
      /* **An exhausted record draws nothing.** It drew a `gave up` chip, and a
         restart made that chip lie about every run at once: `load_automation`
         demotes each live `Running` to `Exhausted` while `auto_resume` brings the
         same runs back, so three fix runs came back working and all three rows
         said they had given up. The rail's own row reads the session instead of a
         record written at the last shutdown, so that is where a run's state
         belongs. What is left here is the `fix` button, which is the thing to
         press either way. */
      /* **Neither button while a session holds the branch**, because a button whose
         only outcome is an error toast is worse than no button, and the `session`
         chip beside them is the thing to press.

         `fix`: `spawn_fix_pr_session` bails with "already has a live session for
         #<n>" the moment `branch_busy` answers, and `PrView.session` is that same
         live session.

         `resolve`: it starts a pass in the PR's worktree, and a live session there
         is exactly what `triage::spawn_posting_run`'s gate refuses. The `session`
         chip beside it is where you were going anyway. The menu stays on a
         right-click for anyone who wants to read the refusal. */
      if (needsResolve && !p.session) row.appendChild(reviewButtons(p));
      if (needsFix && !p.session) row.appendChild(actionButton(p, 'fix-pr', 'fix'));
    }

    /* The row opens the PR; going to its session is the explicit chip, so one does
       not swallow the other. It names the destination rather than the motion,
       because every other chip on this row — `review`, `fix`, `fixing` — is about
       the PR, and this is the only one that moves you somewhere.

       **Not while a run is showing.** `PrView.session` is "a live session in that
       workspace", and a running `fix-pr` is exactly that, so the row drew two
       chips onto the same uuid: `fixing`, which says what is happening, and this,
       which only says that something is. */
    if (p.session && !(auto && auto.state === 'running')) {
      /* Marked when it points at the session you are already in, because pressing
         it then is a no-op and a chip that answers before you press it is better
         than one that answers by doing nothing. `setSelected` is instant and
         local — no request, no snapshot — so there was nothing to see at all. */
      const here = p.session === selected;
      const j = el('button', 'jump' + (here ? ' here' : ''), 'session');
      j.title = here ? 'You are in this session' : 'Go to the session on this branch';
      j.onclick = (ev) => {
        ev.preventDefault();
        ev.stopPropagation();
        // Said out loud as well as shown, for the press that changes nothing: the
        // marked state is easy to miss on a row you were already looking past.
        if (here) {
          toast('already in this session');
          return;
        }
        setSelected(p.session);
      };
      row.appendChild(j);
    }
    group.appendChild(row);
  }
  return group;
}

/** A label and the button that adds to the group under it. */
function groupHead(/** @type {string} */ label, /** @type {HTMLElement} */ add) {
  const head = el('div', 'ws-head');
  const name = el('div', 'ws-name');
  name.appendChild(el('span', 'eyebrow', label));
  head.appendChild(name);
  head.appendChild(add);
  return head;
}

/** Main is exclusive: one active session at a time, and no queue. While it is
 *  occupied the button is disabled and the row that holds it says so (§2).
 *
 *  Unless `allow_several_in_main` is set, which is a config-file decision the
 *  daemon reports in the snapshot. Then `+` stays live and the holder's name is
 *  still worth saying, because a second session in one checkout is a thing to do
 *  on purpose rather than by accident. */
function mainGroup(/** @type {import('./core.js').Target} */ c, /** @type {import('../snapshot').Snapshot} */ state, /** @type {import('../snapshot').WorkspaceView} */ w) {
  const group = el('div', 'ws');
  const sessions = sessionsOf(w.id, state);
  const active = sessions.filter((s) => !isArchived(s));
  const occupant = active.find((s) => s.id === w.occupant && s.alive);
  const several = !!state.several_in_main;

  const add = el('button', 'plus', '+');
  // Occupied, or already making something: the second reason is the one that used
  // to be invisible, and pressing through it is how you get two of them.
  add.disabled = (!!occupant && !several) || !!creating();
  /* The chord belongs in the tooltip of the button that does the same thing:
     finding the button once is how you stop needing it, which is the argument the
     legend button already makes for itself. `MOD_LABEL` rather than a literal —
     the modifier is ⌘ on macOS and Ctrl everywhere else. */
  add.title = creating()
    ? creating() ?? ''
    : occupant
      ? `main is held by ${occupant.title || occupant.id.slice(0, 8)}${several ? ' · another is allowed' : ''}`
      : `New session in main · ${MOD_LABEL} Shift N`;
  add.onclick = () => newSession(w.id, c);
  group.appendChild(groupHead('Main checkout', add));

  for (const s of active.sort(byNewest)) group.appendChild(sessionRow(s, w));
  if (!active.length) group.appendChild(el('div', 'railbtn', 'no sessions'));
  appendArchived(c, group, 'main', sessions.filter(isConversation));
  return group;
}

/** Every worktree session under one header.
 *
 *  Rows come from sessions, not from worktrees: a worktree with nothing running
 *  in it is not something you can act on, so it gets no row. The one exception
 *  is a session whose worktree has no name yet, which shows as `…creating`
 *  rather than nothing at all — an invisible session is how you end up
 *  starting a second one. */
function worktreeGroup(/** @type {import('./core.js').Target} */ c, /** @type {import('../snapshot').Snapshot} */ state, /** @type {string | undefined} */ mainId) {
  const group = el('div', 'ws');
  const add = el('button', 'plus', '+');
  /* Dead while one is being cut, and it says which one in the tooltip.
     Two things make one press look like none: the POST is a worktree, the repo's
     hooks and a `claude` boot, and the row that lands after it says `…creating`
     for as long as it takes Claude Code to name the tree. Both are covered — the
     claim in `core` for the first, `pending` for the second — because the second
     window is the longer one and a `+` that came back to life halfway is the same
     invitation to press again.

     Live ones only. A placeholder session that died before `SessionStart` keeps
     the placeholder workspace for good, and counting that would leave the `+`
     dead until a restart. */
  const cutting = creating()
    || (state.sessions.some((/** @type {import('../snapshot').SessionView} */ s) => pending(s) && !isArchived(s)) ? 'creating a worktree' : null);
  add.disabled = !!cutting;
  add.title = cutting || `New worktree session · ${MOD_LABEL} N (shift-click to name it)`;
  add.onclick = (ev) => newWorktree(ev.shiftKey, c);
  group.appendChild(groupHead('Worktrees', add));

  /* Anything that is not main's belongs here — by session, not by workspace.
   * A worktree Claude Code has not named yet has no workspace record at all,
   * only a session pointing at the placeholder, so filtering on the known
   * workspaces dropped exactly the row that says something is happening. */
  const sessions = state.sessions.filter((/** @type {import('../snapshot').SessionView} */ s) => s.workspace !== mainId);
  const active = sessions.filter((/** @type {import('../snapshot').SessionView} */ s) => !isArchived(s));

  for (const s of active.sort(byNewest)) {
    // The workspace is only needed for the name it lends the row.
    group.appendChild(sessionRow(s, { id: s.workspace }));
  }
  if (!active.length) group.appendChild(el('div', 'railbtn', 'no sessions'));
  appendArchived(c, group, 'worktrees', sessions.filter(isConversation));
  return group;
}

/** The group's past conversations, behind a count.
 *
 *  Collapsed by default, because history is not what the rail is for — but
 *  opened whenever the conversation you are looking at is in here, so the rail
 *  never goes silent about what the centre pane is showing.
 */
function appendArchived(/** @type {import('./core.js').Target} */ c, /** @type {HTMLElement} */ group, /** @type {string} */ key, /** @type {import('../snapshot').SessionView[]} */ sessions) {
  if (!sessions.length) return;
  // Qualified, because `main` and `worktrees` name a group in every checkout and
  // one open archive would open all of them.
  const held = `${c.path}\u0000${key}`;
  const open = showArchived[held] || sessions.some((/** @type {import('../snapshot').SessionView} */ s) => s.id === selected);

  const toggle = el('button', 'arctoggle');
  toggle.setAttribute('aria-expanded', String(open));
  toggle.appendChild(caret());
  toggle.appendChild(el('span', null, 'archived'));
  toggle.appendChild(el('span', 'arccount', String(sessions.length)));
  toggle.onclick = () => { showArchived[held] = !open; renderRail(); };
  group.appendChild(toggle);

  if (!open) return;
  for (const s of sessions.sort(byNewest)) group.appendChild(archivedRow(s));
}

/** A past conversation: which worktree it was in, and how long ago.
 *
 *  No state word — `archived` is the state, and the section it sits in already
 *  says it. Clicking rebuilds what it needs and resumes it. */
function archivedRow(/** @type {import('../snapshot').SessionView} */ s) {
  const btn = el('button', 'sess arc');
  btn.setAttribute('aria-current', String(s.id === selected));
  // So a rename can find this row's name span again after any re-render.
  btn.dataset.id = s.id;

  const row = el('div', 'sess-row');
  row.appendChild(el('span', 'dot archived'));
  const arcName = railName(s, { id: s.workspace });
  row.appendChild(el('span', 'sess-name', arcName, arcName));
  const forked = forkBadge(s);
  if (forked) row.appendChild(forked);
  row.appendChild(clock('sess-id', s.created_ms, ' ago'));
  btn.appendChild(row);

  if (!s.resumable) {
    // The transcript is readable, the conversation cannot be continued (§2).
    btn.appendChild(el('div', 'sess-sub', 'transcript only'));
  } else if (!snapshotFor(s.id).workspaces.some((w) => w.id === s.workspace)) {
    /* Its worktree is gone, which the snapshot says by omission: only teardown
       drops a workspace record, and the retention timer is what usually calls it.
       Worth a line, because "archived" alone would leave you to discover on the
       next resume that the directory is not there — and the point of the setting
       is that the row still works, so the row should say so. Main is never
       missing, so this can only read on a worktree. */
    btn.appendChild(el('div', 'sess-sub', 'tree removed · rebuilds on resume'));
  }
  btn.onclick = () => openArchived(s);
  btn.oncontextmenu = (ev) => openMenu(ev, [
    // Worth more here than on a live row: the archive is the list you scan weeks
    // later, and two conversations Claude Code named the same thing are what you
    // are scanning past.
    ['rename', null, () => renameSession(s)],
    // Not gated on `resumable` the way opening it is: a fork cuts its own
    // worktree, so a conversation whose branch is gone can still be branched off.
    ['fork', null, s.has_transcript ? () => forkSession(s) : null],
    ['copy id', null, () => copyId(s)],
    ['delete', 'bad', () => deleteSession(s)],
  ]);
  return btn;
}

/** Continue a past conversation, rebuilding its worktree first if it is gone. */
async function openArchived(/** @type {import('../snapshot').SessionView} */ s) {
  if (!s.resumable) {
    toast('transcript only: the branch is gone and the commit is unreachable', true);
    return;
  }
  try {
    const r = await callFor(s.id, `/api/session/${s.id}/resume`);
    // A resumed session keeps its id, because `claude --resume <id>` continues
    // that same conversation. So the dead terminal is still in `terms` under the
    // key the new pty wants, and `openTerm` would hand back the corpse — you
    // resume and stare at the old scrollback with a closed socket.
    Term.close(checkoutOf(s.id) ?? activeCheckout(), `session:${r.session}`);
    setPendingSelect(r.session);
    // The branch moved since the conversation happened, so the files it talks
    // about are not the files on disk. Worth saying, not worth refusing over.
    if (r.warning) toast(r.warning, true);
  } catch (e) {
    toast(reason(e), true);
  }
}

/** Two lines: dot + name, then state and duration. No dirty-file count — that
 *  lives in the right column, one click away (§9). */

/** What the row calls itself.
 *
 *  In order of how much it tells you: the PR a pass is working on, the
 *  name Claude Code gave the conversation, then the workspace it sits in. The
 *  workspace is last because it is the coarsest — a worktree holds one session at a
 *  time, so its name says where, not which conversation.
 *
 *  The placeholder workspace id is the daemon's own bookkeeping, so a worktree
 *  still being cut says what is happening instead. */
function railName(/** @type {import('../snapshot').SessionView} */ s, /** @type {{ id: string | null } | undefined} */ w) {
  if (pending(s)) return 'creating worktree';
  // A pass's workspace is `pr-10006`, which repeats the number it is about to
  // print and says nothing else. The PR's own title is already in the snapshot,
  // put there for the pane at the bottom of this rail.
  if (s.pass) {
    const pr = (snapshotFor(s.id).prs || []).find((p) => p.number === s.pass?.pr);
    return pr ? `#${s.pass.pr} ${pr.title}` : `#${s.pass.pr}`;
  }
  return s.title || w?.id || '';
}

/** Marks a conversation that was cut from another one.
 *
 *  A fork keeps its parent's title, so two rows read identically and the only
 *  thing telling them apart is an eight-character id. This says which is the
 *  copy, and the title says what it is a copy of.
 *
 *  The word is `fork`, not `forked`: this row *is* the fork, and a past participle
 *  on it reads as "forked from", which points at the other row. A noun for the
 *  thing the row is has no direction to get wrong. */
function forkBadge(/** @type {import('../snapshot').SessionView} */ s) {
  return s.forked_from ? el('span', 'forked', 'fork') : null;
}

function sessionRow(/** @type {import('../snapshot').SessionView} */ s, /** @type {{ id: string | null } | undefined} */ w) {
  const btn = el('button', 'sess');
  btn.setAttribute('aria-current', String(s.id === selected));
  // So a rename can find this row's name span again after any re-render.
  btn.dataset.id = s.id;

  const row = el('div', 'sess-row');
  row.appendChild(el('span', 'dot ' + dotClass(s)));
  const liveName = railName(s, w);
  row.appendChild(el('span', 'sess-name' + (pending(s) ? ' pending' : ''), liveName, liveName));
  const forked = forkBadge(s);
  if (forked) row.appendChild(forked);
  // The session's age, not its id. A hex slice told worktree-sharing rows apart
  // but was unreadable — a value you never recognise — and age is worth reading on
  // every row and moves as the session does. Same token the archive rows show.
  row.appendChild(clock('sess-id', s.created_ms, ' ago'));
  btn.appendChild(row);

  const sub = el('div', 'sess-sub');
  // Which pass it is. The name above says which PR, and `fix-pr` and
  // `handle-review` do very different things to it.
  if (s.pass) sub.appendChild(el('span', 'sess-cmd', s.pass.command));
  sub.appendChild(el('span', 'sess-state ' + stateClass(s), stateLabel(s)));
  // The waiting duration is the number to optimise down (§2). A start has a
  // clock for a different reason: the daemon cuts the worktree and runs the
  // repo's create and link hooks before the agent says anything, which is ten
  // seconds of nothing. A number that moves is the difference between slow and
  // hung.
  if (isWaiting(s) && s.waiting_ms != null) {
    sub.appendChild(clock('', s.waiting_ms));
  } else if (s.state.state === 'starting') {
    sub.appendChild(clock('', s.created_ms));
  }
  btn.appendChild(sub);

  // An agent editing outside its worktree is a prompt problem worth seeing,
  // not noise to swallow (§11).
  if (s.boundary_violations.length) {
    btn.appendChild(el('span', 'sess-warn',
      `${s.boundary_violations.length} blocked edit(s) outside the worktree`));
  }

  btn.appendChild(el('div', 'sess-pad'));
  /* Picked on pointerdown, not on click.
   *
   * A `click` is only dispatched when the press and the release land on the
   * *same* element, and this row is rebuilt whenever a snapshot arrives — which
   * is what selecting a session causes. Move between sessions at a normal pace
   * and about one pick in fifteen simply never happens: the render fell inside
   * the few tens of milliseconds the click was being made in, the row the mouse
   * went down on was gone by the time it came up, and nothing fired. No error,
   * no toast, just a row that did not take.
   *
   * Pointerdown cannot be caught out that way — it fires on the row that is
   * under the pointer at the time, before any of this can be replaced.
   *
   * The click handler stays because a keyboard activates a `<button>` without a
   * pointer ever going down. `picked` is module-level rather than per node, so
   * it survives the row being rebuilt between the two events; without that the
   * pair would select twice and the second one would re-announce the selection,
   * stealing focus back into a terminal you had just left. */
  btn.onpointerdown = (ev) => {
    if (ev.button !== 0) return;      // the secondary button opens the menu
    picked = s.id;
    setSelected(s.id);
  };
  btn.onclick = () => {
    if (picked === s.id) { picked = null; return; }
    setSelected(s.id);
  };
  /* Which way the branch moves, as one item with three answers. Asked of the
     snapshot's own main row rather than of `w`, which is a `{ id }` stub on a
     worktree row — `w.is_main` is `undefined` there, and relying on that being
     falsy would make this right by accident.

       on main                        → move out of main, into a tree of its own
       on a worktree, main is free    → move to main
       on a worktree, main holds work → swap with main

     Never two of them. A session is in main or it is not, so the other could only
     ever be dead, and a greyed "move out of main" on a worktree row reads as the
     app thinking that row is in main — the opposite of what the rail says two
     lines above it. */
  const state = snapshotFor(s.id);
  const mainWs = mainWorkspace(state);
  const inMain = s.workspace === mainWs?.id;
  const moveLabel = inMain
    ? 'move out of main'
    : mainHoldsWork(mainWs, state) ? 'swap with main' : 'move to main';
  // A worktree Claude Code has not named yet has no path to swap. The session
  // carries which checkout it is in, so a row in a checkout you are not looking
  // at still acts on its own daemon.
  const moveDo = inMain
    ? () => moveOutOfMain(s)
    : pending(s) ? null : () => swapWithMain(s.workspace, s);
  // The header's ✕ only ever closes the selected session, so closing any other
  // one meant switching to it first.
  btn.oncontextmenu = (ev) => openMenu(ev, [
    ['rename', null, () => renameSession(s)],
    // Nothing to branch off until the conversation has had a turn.
    ['fork', null, s.has_transcript ? () => forkSession(s) : null],
    // Claude Code's own picker, reached rather than rebuilt. Greyed in the states
    // where the daemon refuses it — mid-turn an escape interrupts the turn, and at
    // a question or a permission prompt it answers instead of rewinding.
    ['rewind', null, isRewindable(s) ? () => rewindSession(s) : null],
    ['copy id', null, () => copyId(s)],
    // The worktree, not the session: the row is the only place a worktree is
    // visible, so its workspace-level action lives here too.
    [moveLabel, null, moveDo],
    ['close', 'bad', s.alive ? () => closeSession(s.id) : null],
    ['delete', 'bad', () => deleteSession(s)],
  ]);
  return btn;
}

/** The session's uuid, on the clipboard.
 *
 *  It is Claude Code's session id as well as the daemon's — every spawn passes
 *  `--session-id` — so it is what `claude --resume`, a transcript path and a hook
 *  correlation all key on. The rail is the only place it is visible, and it is
 *  not selectable text there. */
async function copyId(/** @type {import('../snapshot').SessionView} */ s) {
  if (await copyText(s.id)) toast('id copied');
}

/** Sessions whose prompt would take a double-escape as "rewind".
 *
 *  The picker opens at the prompt and nowhere else, so this is narrower than
 *  "waiting": mid-turn the escape interrupts the turn, and the two waiting states
 *  that expect an answer would take it as one — cancelling a question, declining a
 *  permission prompt. The daemon refuses the same three, so this only decides
 *  whether the item is offered, never whether it is safe. */
const isRewindable = (/** @type {import('../snapshot').SessionView} */ s) =>
  s.alive
  && s.state.state === 'your_turn'
  && s.state.reason !== 'asked_a_question'
  && s.state.reason !== 'needs_permission'
  // Nothing to rewind to: the picker would open with nothing in it.
  && s.has_transcript;

/** Open Claude Code's rewind picker in this session's terminal.
 *
 *  Selects first, because the picker draws in the pane and pressing this on a row
 *  you cannot see would put a modal somewhere out of sight. */
async function rewindSession(/** @type {import('../snapshot').SessionView} */ s) {
  setSelected(s.id);
  try {
    await callFor(s.id, `/api/session/${s.id}/rewind`);
    toast('opened the rewind picker — pick a point in the pane');
  } catch (e) {
    toast(reason(e), true);
  }
}

/** Whether main is holding work of its own, or just sitting on the base branch.
 *
 *  What the swap is *called* turns on this: with a branch of its own on each side
 *  the two trade places, and with only base in main your branch goes there and
 *  base comes back — which is a move, and saying "swap" for it describes an
 *  exchange nobody asked for. A *session* in main counts as work too: swapping
 *  then displaces that conversation into this worktree, which is an exchange
 *  however empty main's branch is.
 *
 *  The base branch comes off `upstream_ref` (`upstream/develop` → `develop`),
 *  which is the same split the daemon makes. `origin/HEAD` cannot be split that
 *  way — the daemon resolves it against the remote and the SPA cannot — so an
 *  unresolvable base answers "yes, it holds something", keeping the wording that
 *  is right either way.
 *
 *  Read off the whole branch *set*, not `branches[0]`: that set is built from a
 *  `HashSet`, so its order says nothing, and "the only thing main has is base" is
 *  a question about the set rather than about its first element. */
function mainHoldsWork(/** @type {import('../snapshot').WorkspaceView | undefined} */ main, state = snap) {
  /* A conversation in main is work, whatever branch main is on. Without this the
     item read the git side only: main sitting on its base with somebody working in
     it answered "nothing of its own", so the menu offered `move to main` and the
     confirm promised "main has nothing of its own checked out" — while a swap
     would have carried that person's conversation out into this worktree. Reported
     from a Mac: `move to main` on a row while a session was in main.

     The same rule the daemon uses for "is anyone in main" since it learned to
     allow more than one: any live session whose workspace is main, rather than the
     recorded claim, which can name none of them. */
  const busy = state.sessions.some(
    (x) => x.workspace === main?.id && x.alive && !isArchived(x));
  if (busy) return true;
  const leaf = (state.upstream_ref || '').split('/').pop();
  if (!leaf || leaf === 'HEAD') return true;
  // What main has checked out *now*. This used to ask `branches`, which accumulates
  // every branch a tree has ever held and is never pruned — so one visit from any
  // other branch made main look occupied for the rest of the daemon's life, and the
  // row went on offering a swap with a main sitting on its base.
  // Unknown before the first reconcile, and unknown is the cautious answer: a swap
  // refuses when there is nothing to exchange, a move would move onto a branch
  // somebody else holds.
  const on = main?.branch;
  return !on || on !== leaf;
}

/** Move a session out of main, into a worktree of its own.
 *
 *  The swap's missing direction: a swap needs a second branch to exchange, and
 *  this has none — you started something in main, it turned into real work, and
 *  main should be free again. The branch gets a tree named after it, uncommitted
 *  changes travel with it, main goes back to base, and the conversation follows
 *  keeping its id and its place in the rail.
 *
 *  Confirmed for the same reason the swap is: every file under main changes, and
 *  the daemon's refusals are about what it can see, not about whether you meant
 *  it. */
async function moveOutOfMain(/** @type {import('../snapshot').SessionView} */ s) {
  if (!await confirmBox(
    `Move this session out of main${inCheckout(s)}?\n\n`
    + 'Its branch gets a worktree of its own and main goes back to its base branch \u2014 '
    + 'or, if main is already on base, the work gets a branch cut for it and main stays put. '
    + 'Uncommitted changes travel; untracked files stay in main. '
    + 'The conversation moves too, keeping its history.'
  )) return;
  try {
    const r = await callFor(s.id, `/api/session/${s.id}/out-of-main`);
    // A relocated session keeps its id, so the dead terminal is still in `terms`
    // under the key the new pty wants — the same reason the swap and resume close it.
    if (r.session && r.session.session) {
      Term.close(checkoutOf(s.id) ?? activeCheckout(), `session:${r.session.session}`);
      setPendingSelect(r.session.session);
    }
    toast(r.created
      ? `cut ${r.branch} in ${r.workspace}; main is still on ${r.main}`
      : `${r.branch} is in ${r.workspace}; main is on ${r.main}`);
    // The branch moved even if the conversation could not follow, so these are
    // second lines rather than errors over the top of a success.
    if (r.wip_error) toast(`the branch moved, but ${r.wip_error}`, true);
    if (r.session && r.session.error) toast(`the branch moved, but ${r.session.error}`, true);
    else if (r.session && r.session.degraded) {
      toast('the conversation would not resume there, so it was forked instead', true);
    }
  } catch (e) {
    toast(reason(e), true);
  }
}

/** Put this worktree's branch in main, and main's here.
 *
 *  Main is where the managed processes and the dev stack live, so work that needs
 *  them has to be *in* main. Both directories stay put — only what each has
 *  checked out is exchanged — and the conversations follow their branches in both
 *  directions, keeping their ids.
 *
 *  Confirmed rather than immediate: every file under two trees changes, and the
 *  daemon's refusals (mid-turn agent, dirty tree, stopped rebase) are about what
 *  it can see, not about whether you meant it. */
/* A swap takes seconds — two checkouts change every file, and the conversations
   that follow the branches are killed and resumed — and until it lands the rail
   still shows the world as it was. That silence is what gets it pressed twice, and
   the second press is not a no-op: it swaps straight back. The daemon refuses a
   concurrent one outright; this says so without the round trip, and says the first
   is running rather than leaving you to guess. */
let swapInFlight = false;

/** @param {string} wsId
 *  @param {any} s the session whose row this was pressed from, which says which
 *                 checkout the workspace is in */
async function swapWithMain(wsId, s) {
  if (swapInFlight) return toast('a swap is already running — watch the rail', true);
  const state = snapshotFor(s.id);
  const holds = mainHoldsWork(mainWorkspace(state), state);
  if (!await confirmBox(holds
    ? `Swap branches between main and ${wsId}${inCheckout(s)}?\n\n`
      + `main takes this worktree's branch, and this worktree takes main's. `
      + `Uncommitted changes travel with their branch. Each conversation follows `
      + `its branch — this one moves into main, and main's moves here — keeping its `
      + `history and its place in the rail.`
    : `Move this worktree's branch to main${inCheckout(s)}?\n\n`
      + `main has nothing of its own checked out, so its base branch comes back `
      + `here in exchange. Uncommitted changes travel with the branch, and this `
      + `conversation follows it into main, keeping its history and its place in `
      + `the rail.`
  )) return;
  swapInFlight = true;
  toast(`swapping ${wsId} with main${inCheckout(s)}…`);
  try {
    const r = await callFor(s.id, `/api/workspace/${encodeURIComponent(wsId)}/swap-main`);
    // A relocated session keeps its id, so the dead terminal is still in `terms`
    // under the key the new pty wants and `openTerm` would hand back the corpse —
    // the same reason resume closes it. Both directions, since both were respawned.
    for (const dir of [r.into_main, r.into_worktree]) {
      if (dir && dir.session) Term.close(checkoutOf(s.id) ?? activeCheckout(), `session:${dir.session}`);
    }
    // Land in main, where the branch now is — the whole point of pressing this.
    if (r.select) setPendingSelect(r.select);
    toast(`main is on ${r.main}; ${wsId} is on ${r.worktree}${inCheckout(s)}`);
    // The branches moved even if a conversation could not follow, so these are
    // second lines rather than errors over the top of a success.
    for (const [dir, where] of [[r.into_main, 'into main'], [r.into_worktree, `into ${wsId}`]]) {
      if (!dir) continue;
      if (dir.error) toast(`the branches swapped, but ${dir.error}`, true);
      // A fork, not the move that was promised: the id changed, so there is a new
      // row rather than the one you were looking at.
      else if (dir.degraded) toast(`the conversation ${where} would not resume, so it was forked instead`, true);
    }
    // The other partial success: the branches exchanged but the banked work would
    // not re-apply. A second line for the same reason the relocation errors are —
    // the swap happened, and the message says where the work still is.
    if (r.wip_error) toast(`the branches swapped, but ${r.wip_error}`, true);
    // Untracked files cannot be carried, so say which stayed rather than leaving
    // you to notice that half the work did not travel.
    if (r.untracked_left && r.untracked_left.length) {
      toast(
        `left in ${wsId} (untracked, so not carried): ${r.untracked_left.slice(0, 4).join(', ')}`
        + (r.untracked_left.length > 4 ? ` and ${r.untracked_left.length - 4} more` : ''),
        true,
      );
    }
  } catch (e) {
    toast(reason(e), true);
  } finally {
    swapInFlight = false;
  }
}

/** Branch off a conversation: same context, new worktree, original untouched.
 *
 *  The new session appears under worktrees rather than next to its parent, which
 *  is the point — the two are no longer editing the same files.
 *
 *  No `closeTerm` unlike resume, which keeps the old id and would otherwise hand
 *  back the dead terminal. A fork has an id of its own and nothing to collide
 *  with. */
async function forkSession(/** @type {import('../snapshot').SessionView} */ s) {
  try {
    const r = await callFor(s.id, `/api/session/${s.id}/fork`);
    setPendingSelect(r.session);
    toast('forked');
    // The branch moved on since the conversation, same as resume: worth saying,
    // not worth refusing over.
    if (r.warning) toast(r.warning, true);
  } catch (e) {
    toast(reason(e), true);
  }
}

/** Name a session yourself, editing the rail row in place.
 *
 *  The input holds the name you gave it and shows the ai-title as a placeholder,
 *  so you can see what the row falls back to while you retype — the one thing the
 *  old `prompt()` box could not do. `Enter` commits, `Esc` cancels, and leaving
 *  the box commits too. Blank hands the row back to the ai-title.
 *
 *  The row is a `<button>`, so every event the input handles is stopped from
 *  bubbling: a click must not select the session, and a keystroke must not reach
 *  the app's keyboard map. */
function renameSession(/** @type {import('../snapshot').SessionView} */ s) {
  const rail = $('rail');
  const span = rail.querySelector(`[data-id="${s.id}"] .sess-name`);
  // The menu action can outlive the row it was opened on; if a re-render dropped
  // it, there is nothing to edit in place.
  if (!span) return;

  editingName = s.id;
  const input = document.createElement('input');
  input.className = 'sess-rename';
  input.value = s.name || '';
  input.placeholder = s.title || '';
  span.replaceWith(input);
  input.focus();
  input.select();

  let done = false;
  const finish = async (/** @type {boolean} */ commit) => {
    if (done) return;             // blur fires alongside Enter; settle once.
    done = true;
    const given = input.value.trim();
    editingName = null;           // let the rail rebuild again before the await.
    if (commit && given !== (s.name || '')) {
      try {
        await callFor(s.id, `/api/session/${s.id}/rename`, { name: given });
      } catch (e) {
        toast(reason(e), true);
      }
    }
    renderRail();                 // put the row back, whichever way it ended.
  };

  input.onkeydown = (e) => {
    if (e.key === 'Enter') { e.preventDefault(); void finish(true); }
    else if (e.key === 'Escape') { e.preventDefault(); void finish(false); }
    e.stopPropagation();
  };
  input.onblur = () => finish(true);
  input.onclick = (e) => e.stopPropagation();
  input.onpointerdown = (e) => e.stopPropagation();
}

/** Forget a session: the row, the record, and the daemon's own copy of the
 *  transcript.
 *
 *  Confirmed, unlike closing, because closing is reversible in the way that
 *  matters — the conversation is still there to resume — and this is not. The
 *  wording says what survives, so "delete" does not have to be read as deleting
 *  the conversation itself. */
async function deleteSession(/** @type {import('../snapshot').SessionView} */ s) {
  const name = railName(s, { id: s.workspace });
  const ending = s.alive ? 'It is still running, so this ends it first. ' : '';
  if (!await confirmBox(`Delete "${name}"${inCheckout(s)}?\n\n${ending}The row and orchd's copy of the `
    + "transcript go for good. Claude Code's own transcript is left where it is.",
  { ok: 'Delete' })) return;
  callFor(s.id, `/api/session/${s.id}/delete`)
    .then(() => toast('deleted'))
    .catch((e) => toast(e.message, true));
}

/** End a session: kills the pty, keeps the row and its scrollback (§2). */
function closeSession(/** @type {string} */ id) {
  // Claude takes several seconds to shut down and the row only turns `exited`
  // once the daemon sees it go, so without this the click reads as a no-op.
  callFor(id, `/api/session/${id}/kill`)
    .then(() => toast('closing session'))
    .catch((e) => toast(e.message, true));
}

/** The rail exists to surface idle agents, so the count sits at the top of it. */
/** Sessions a nudge would reach: parked at a prompt, and not mid-question.
 *
 *  Wider than `isWaiting`, on purpose. A session that has only just resumed is
 *  `ready`, which `wants_attention` excludes because an idle agent is not
 *  something to shout about — but it is exactly the one you want to send on. */
const isNudgeable = (/** @type {import('../snapshot').SessionView} */ s) =>
  // `ready` alone: resumed mid-conversation and not prompted since. A finished
  // turn is not paused mid-work, it is done, and telling it to continue would
  // invent the next thing for you.
  s.alive && s.state.state === 'your_turn' && s.state.reason === 'ready'
  // A session with no conversation behind it has nothing to continue, and would
  // read the word as its opening instruction. The daemon skips those too, so the
  // count here is what pressing the button actually does.
  && s.has_transcript
  // And it has to have been cut off mid-turn. `ready` alone cannot tell that
  // from a conversation that had finished before the restart — they come back
  // at the same empty prompt — so the bar was calling finished work "paused".
  && s.interrupted;

/** What the bar was last built from — see `unchanged`. */
const barDrawn = { sig: null };

function renderWaitbar() {
  // Across every checkout, which is what makes the bar and the chord it
  // advertises answer the same question.
  const all = everySession().map((r) => r.session);
  const waiting = all.filter(isWaiting);
  const ready = all.filter(isNudgeable);
  const bar = $('waitbar');
  /* Which sessions, not how long they have waited: the duration is a
     `data-clock` node that `tick` rewrites in place, and the longest of a fixed
     set cannot change while the set does not. Without this the `continue` button
     was rebuilt under the pointer several times a second and its border strobed
     as `:hover` was re-targeted on each one. */
  /* The active checkout is an input in its own right: the destination below is
     named only when it is *not* where you are, so walking into that checkout has
     to redraw the bar even though the waiting set did not move. */
  if (unchanged(barDrawn, [waiting.map((s) => s.id), ready.map((s) => s.id),
    activeCheckout().path])) return;
  if (!waiting.length && ready.length < 2) {
    bar.className = 'waitbar';
    bar.replaceChildren();
    return;
  }
  bar.replaceChildren();

  if (waiting.length) {
    const longest = waiting.reduce(
      (a, b) => ((a.waiting_ms ?? 0) >= (b.waiting_ms ?? 0) ? a : b));
    bar.className = 'waitbar on';
    /* "need you", not "waiting". The count is `wants_attention` — any `your_turn`
       but `ready`, plus a red build — so it covers a finished turn as well as a
       permission prompt, and the rows name those separately. "Waiting" promised
       somebody was blocked, and reading "2 waiting" over a rail with one obviously
       blocked row is the bar arguing with the list under it.

       Two nodes, because only the second half moves: the count changes with a
       snapshot, the duration changes every second. */
    bar.appendChild(el('span', null, `${waiting.length} need you · longest `));
    bar.appendChild(clock('', longest.waiting_ms ?? 0));
    /* **Where it will take you, when that is not where you are.** The bar counts
       across every checkout, so pressing it can move you out of the one you are
       looking at — and the rail scrolling to a row under a different header is the
       only other sign. Named only when it is somewhere else, because the usual
       case is the checkout in front of you and saying so every time is noise. */
    const away = CHECKOUTS.length > 1 && checkoutOf(longest.id)?.path !== activeCheckout().path
      ? checkoutOf(longest.id)?.name
      : null;
    if (away) bar.appendChild(el('span', 'waitwhere', ` in ${away}`));
    bar.title = away
      ? `Go to the one that has needed you longest, in ${away} · ${MOD_LABEL} Space`
      : `Go to the one that has needed you longest · ${MOD_LABEL} Space`;
    bar.onclick = () => setSelected(longest.id);
  } else {
    /* Nobody is asking for you; a restart has just put several agents back at an
       empty prompt. Quieter than the waiting bar, because this is an offer rather
       than a queue: the whole point of `ready` not counting as attention. */
    bar.className = 'waitbar on calm';
    bar.appendChild(el('span', null,
      `${ready.length} session${ready.length === 1 ? '' : 's'} paused mid-work`));
    bar.onclick = () => setSelected(ready[0].id);
  }

  /* One poke for the lot. Typing the same word into each of them is the tax on
     auto-resume being worth having. */
  if (ready.length > 1) {
    const all = el('button', 'waitall', 'continue');
    all.title = 'Type "continue" into every session paused mid-work';
    all.onclick = (ev) => { ev.stopPropagation(); void nudgeAll(); };
    bar.appendChild(all);
  }
}

/** Send them all on, in every checkout the bar counted.
 *
 *  One call per daemon, because `/api/sessions/nudge` is a daemon route and a
 *  daemon only knows its own sessions. The bar counts across all of them, so
 *  nudging only the one you are in would leave the count where it was.
 */
async function nudgeAll() {
  const holding = new Set(everySession().filter((r) => isNudgeable(r.session))
    .map((r) => r.checkout.path));
  try {
    const answers = await Promise.all(
      CHECKOUTS.filter((c) => holding.has(c.path)).map((c) => callOn(c, '/api/sessions/nudge')));
    const r = {
      nudged: answers.flatMap((a) => a.nudged || []),
      held: answers.flatMap((a) => a.held || []),
    };
    const n = (r.nudged || []).length;
    toast(n ? `nudged ${n}` : 'nothing to nudge');
    // Named, not silently skipped: a permission prompt or a question takes a
    // keystroke as its answer, so typing into one would be answering for you.
    const held = r.held || [];
    if (held.length) {
      toast(`${held.join(', ')} ${held.length === 1 ? 'is' : 'are'} waiting on an answer from you`, true);
    }
  } catch (e) {
    toast(reason(e), true);
  }
}

export { renderRail as render, railName as rowName, closeSession };
