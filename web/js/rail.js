// The rail: what is running, what is waiting on you, and the PRs beside it.
// Twenty-four names, three out; the rest is how a row decides what it says.

import { $, activeCheckout, bandOf, byNewest, call, callFor, callHost, callOn, caret, checkoutOf, CHECKOUTS, chooseBox, clock, confirmBox, copyText, creating, creatingIn, creatingIntoMain, dotClass, el, enterCheckout, everySession, getHost, getOn, inTrouble, isArchived, isConversation, isWaiting, mainWorkspace, MOD_LABEL, newSession, newWorktree, openMenu, pending, QUEUE_MAX, reason, refreshButton, repoSummary, safeHref, selected, sessionsOf, sessionOrder, setPendingSelect, setSelected, setSessionOrder, snap, snapshotFor, snapshotOf, startingShown, stateClass, stateLabel, terms, toast, paintSig, reconcile, unchanged, watchStarting } from './core.js';
import * as Open from './open.js';
import * as Review from './review.js';
import * as Term from './term.js';

/* Expanded per group and kept across renders. Main's two conversations and the
 * worktrees' twenty are not the same question. */
/** @type {Record<string, boolean>} */
/** @type {Record<string, boolean>} */
const showArchived = { main: false, worktrees: false };

/* The archive filter, per checkout, and the two halves of its answer.
 *
 * **Names are free and transcripts are not**, which is the whole shape of this:
 * `arcQuery` filters the rows the snapshot already carries, on every keystroke,
 * with no request at all; `arcHits` is what `/api/archive/search` came back with
 * for that same query, and it arrives when it arrives. Keyed by checkout path,
 * because the archive is one checkout's and so is its filter.
 *
 * `busy` is what draws the spinner, and it is the *last* row: hits land above it,
 * so nothing already on screen moves when the slow half lands. */
/** @type {Record<string, string>} */
const arcQuery = {};
/** @type {Record<string, { q: string, busy: boolean, hits: Record<string, string> }>} */
const arcHits = {};
/** @type {Record<string, ReturnType<typeof setTimeout>>} */
const arcTimer = {};

/** How long to wait for the typing to stop before reading any transcripts.
 *
 *  The names have already answered by then. This is the half that opens files,
 *  and a request per keystroke would have the daemon reading the same megabytes
 *  five times to answer the query you were on the way to. */
const ARC_DEBOUNCE = 250;

/** The shortest query worth reading transcripts for.
 *
 *  Two characters match almost every conversation, so a scan for one of them is a
 *  full read of the checkout's archive to tell you nothing. The daemon refuses the
 *  same length — a page is not the only thing that can call it. */
const ARC_MIN = 2;

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
  /* `branches` is the workspace's whole branch history and no pane draws it —
     the context header used to take `[0]` from it and now reads `branch`, which
     is a different field and stays. It grows whenever a tree is put on a new
     branch, which is a thing the rail has no way to show and rebuilt for anyway. */
  'branches',
];

/** What the rail was last built from — see `unchanged`. */
const drawn = { sig: null, name: 'rail+pr' };

function renderRail() {
  /* Before the guards, not between them: the bar has its own inputs and its own
     guard, and being skipped by any of the rail's would leave it saying "2 need
     you" after they stopped. It sat after the first of the three, so a rename or a
     row drag froze it for the length of the gesture. */
  renderWaitbar();
  // A gesture is on a node this function replaces: rebuilding mid-drag drops the
  // header out from under the pointer and the drop never lands.
  if (editingName !== null || dragging !== null || rowDrag !== null) return;

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
  /* **The create in flight is an input too**, and it was not in this list. The
     `+` buttons read `creating()` for their disabled state and `fillSessions`
     now draws a row from it, but the announcement that calls this function changes
     nothing the signature could see — so the render returned early and neither
     appeared until the next snapshot happened along. Which is exactly the window
     both of them exist to cover. */
  /* `arcQuery` and `arcHits` are view state the snapshot cannot see, like
     `showArchived` above them: typing in the filter and the answer coming back are
     both things that change the rail and nothing else. */
  if (unchanged(drawn, [states, CHECKOUTS, activeCheckout().path, [...folded], showArchived,
    arcQuery, arcHits, showPrs, picked, selected, swapInFlight, creating(), creatingIn(),
    sessionOrder], NOT_DRAWN)) {
    return;
  }

  const rail = $('rail');

  /* **Reconciled, not replaced**, and `core.reconcile` holds the measurements
     that say why: a `replaceChildren` here destroys the row under the pointer
     several times a second, and its hover highlight then never finishes fading
     in. Only the session rows carry a signature of their own; everything else
     below takes `chrome`, which moves whenever the rail's own guard let this
     function run at all. That is deliberate — those are single elements nobody
     rests a pointer on, and a signature naming too little freezes a pane, which
     is the worse failure of the two. */
  /* **Taken from the guard rather than computed again.** `unchanged` keeps the
     signature it compared on the box, and this is that same signature — the same
     array, the same drop list. Rebuilding it here would stringify every
     checkout's whole snapshot a second time, which is the most expensive thing
     on this path, and would be a second copy of a twelve-item list to keep in
     step by hand. `drawn.sig` is a string by the time the guard has let us
     through; the fallback is for the type checker. */
  const chrome = drawn.sig ?? '';

  /* One block per checkout, in the order the host opened them. A checkout is a
     daemon and a daemon describes only itself, so each block is built from that
     checkout's own snapshot — there is no combined one to build from. */
  const several = CHECKOUTS.length > 1;
  /** @type {any[]} */
  const items = [];
  for (const [i, c] of CHECKOUTS.entries()) {
    const state = states[i];
    /* **No block and no header at all on a single-checkout install**, which is
       every install today: the rail is then exactly what it always was, and a
       header naming the only checkout there is is noise. Its items go straight
       into the rail rather than into a wrapper holding one thing. */
    if (!several) {
      items.push(...checkoutItems(c, state, chrome, false));
      continue;
    }
    /* One block per checkout, marked as a group so a screen reader can skip it
       whole — the header is its name, and the rail is otherwise a flat list of
       rows from several places. The block is built once and kept; its children
       are reconciled inside it, which is the whole point. */
    items.push({
      key: `co:${c.path}`,
      sig: 'co-block',
      build: () => {
        const b = el('div', 'co-block');
        b.setAttribute('role', 'group');
        return b;
      },
      fill: (/** @type {HTMLElement} */ b) => {
        b.setAttribute('aria-label', c.name);
        reconcile(b, [
          { key: 'head', sig: chrome, build: () => checkoutHead(c) },
          // Folded: the header and nothing else. It still says what it is
          // hiding — see `checkoutHead`.
          ...(folded.has(c.path) ? [] : checkoutItems(c, state, chrome, true)),
        ]);
      },
    });
  }

  /* **Below the sessions, not among them.** There is one of this however many
     projects are open, so a row inside a project's own block said the opposite —
     and inside the scroller it slid away under a long list or an open archive,
     which is the one thing a button you press by hand must not do. Its own strip
     between the scroller and the PR pane, always drawn: it is how a second
     checkout is ever opened, and it replaces the header's switcher.

     Reconciled rather than replaced, like the rail below it: it is a button with
     a hover, and one rebuilt under the pointer on every repaint is the fault
     `core.reconcile` exists for. */
  reconcile($('railfoot'), [{ key: 'addco', sig: chrome, build: () => addCheckoutButton() }]);
  reconcile(rail, items);

  // Its own pane below the scroller, so it stays put while sessions scroll. It
  // describes one repository, so it follows the checkout you are in.
  /* **A checkout with no forge draws one line, not a pane.** With no GitHub remote
     the PR pane read `unavailable` — a header, a count, a refresh button and a
     caret, all of them chrome around "this does not apply here", and beside a
     review queue saying the same thing it made a fresh install look broken.
     `repos.upstream` is the fact itself rather than `pr_error`, which is a sentence
     and would have to be matched as one. The review queue answers a *different*
     question and hides itself on its own signal — a repo with its own
     `reviews_command` has a queue and no forge — so see `queue.js` rather than
     assuming this decides both. */
  const prpane = $('prpane');
  if (snap.repos?.upstream) {
    // Reconciled like the rail above it, and for the same reason: a `.prrow` is
    // hovered and clicked, and this pane is rebuilt by the rail's guard — so it
    // was being thrown away on every session state change, none of which it draws.
    reconcile(prpane, [{
      key: 'prs',
      sig: 'ws',
      build: () => el('div', 'ws'),
      fill: (/** @type {HTMLElement} */ g) => fillPrGroup(prpane, g),
    }]);
  } else {
    /* The band is set on the pane itself by `fillPrGroup`, so the arm that does
       not call it has to take it off: with two checkouts open, the one-line notice
       otherwise wears the previous checkout's colour — which is the one thing the
       band exists to say. */
    prpane.classList.remove('pr-of-checkout');
    prpane.style.removeProperty('--band');
    reconcile(prpane, [{ key: 'noforge', sig: chrome, build: () => noForge() }]);
  }
}

/** What one checkout puts in the rail, below its header when it has one. */
function checkoutItems(/** @type {import('./core.js').Target} */ c, /** @type {import('../snapshot').Snapshot | null | undefined} */ state, /** @type {string} */ chrome, /** @type {boolean} */ several) {
  if (!state) {
    // A checkout whose daemon has not reported yet, or is down. The row stays
    // either way, because the row is what `reopen` acts on.
    return [{
      key: `down:${c.path}`,
      sig: chrome,
      build: () => el('div', 'railbtn', c.live ? 'starting\u2026' : 'not running'),
    }];
  }
  /* One list per checkout, main's sessions first (§9). The group headings are
     gone; `fillSessions` says why. It draws its own head only on a
     single-checkout install, where there is no `checkoutHead` above it and the
     rail would otherwise open on a bare session row with nothing naming it.

     The `.ws` box is built once and kept — a constant signature — because what
     has to survive a repaint is inside it. */
  return [{
    key: `ws:${c.path}`,
    sig: 'ws',
    build: () => el('div', 'ws'),
    fill: (/** @type {HTMLElement} */ g) => fillSessions(g, c, state, mainWorkspace(state), chrome, !several),
  }];
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

/** Raise the native folder dialog, then review what came back.
 *
 *  A cancelled dialog answers with no path, which is an answer rather than an
 *  error. In a browser tab there is no window to raise one from, and the host
 *  says so in the sentence every other window route uses.
 *
 *  **Through the review, which is the half this journey never had.** A folder the
 *  host has never seen has no config written for it, and the base branch, the
 *  GitHub repo, the environment tool and the repo's own dev processes are exactly
 *  what a first look can answer and a person cannot be expected to type. That step
 *  used to exist only on the first-run page, so it ran once per install and never
 *  for a checkout added from here.
 */
async function browseForCheckout() {
  try {
    const { path } = await callHost('/api/host/pick');
    if (path) await Open.reviewAndAdd(path);
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
 *  soften: an empty host is the first-run page. It does not ask — the body says
 *  why, and `confirmBox` in `core.js` carries the rule.
 *
 *  @param {import('./core.js').Target} c
 */
async function closeCheckout(c) {
  /* **No confirm: nothing here is lost.** The worktrees and branches stay on disk
     and the conversations are kept, so reopening the checkout offers to resume
     them — which is the rule this UI now holds to, that a box is for work that
     cannot be got back. What it *costs* is still worth saying, so the count moved
     from a question into the toast: one agent or six is the difference between a
     misclick you shrug at and one you want to know about. */
  const live = (snapshotOf(c.path)?.sessions ?? []).filter((s) => !isArchived(s)).length;
  try {
    await callHost('/api/host/checkout/close', { path: c.path });
    // Every terminal in that checkout is pointed at a daemon that has stopped.
    for (const [key, entry] of [...terms]) {
      if (entry.checkout.path === c.path) Term.close(c, key.slice(key.indexOf('\u0000') + 1));
    }
    toast(live
      ? `closed ${c.name} — ${live} session${live === 1 ? '' : 's'} stopped, and kept`
      : `closed ${c.name}`);
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

/** Which session row is being dragged, while one is, and out of which list.
 *
 *  Same reason as `dragging` above — the rail rebuilds every second and the
 *  gesture is on a node that rebuild replaces. `list` is here because main's
 *  sessions and the worktrees' are drawn as two runs with main always first, so a
 *  drop from one onto the other could only move a row somewhere it would not be
 *  drawn; those drops are refused rather than silently ignored. */
/** @type {{ id: string, path: string, list: string } | null} */
let rowDrag = null;

/** The rail's own order for one checkout, with anything the order has never seen
 *  **above** every placed row — `byNewest` is ascending over an age, so the newest
 *  session is first, and a worktree you just cut is the newest thing there is. The
 *  body says what sending them to the bottom cost.
 *
 *  @param {string} path
 *  @param {import('../snapshot').SessionView[]} list
 */
function inRailOrder(path, list) {
  /** @type {string[]} */
  const order = sessionOrder[path] ?? [];
  if (!order.length) return list.slice().sort(byNewest);
  // Built once rather than `indexOf` per comparison, which walks the list again
  // for every pair the sort looks at.
  const at = new Map(order.map((id, i) => [id, i]));
  /* A session the order has never seen ranks **above** every placed row, not
     below. `created_ms` is an age, so `byNewest` ascending is newest first — and a
     worktree you have just cut is the newest thing there is. Sent to the bottom it
     dropped off the end of an eight-row rail the moment it appeared, and a single
     drag made that permanent for the checkout. */
  const rank = (/** @type {import('../snapshot').SessionView} */ s) => at.get(s.id) ?? -1;
  return list.slice().sort((a, b) => rank(a) - rank(b) || byNewest(a, b));
}

/** Move one row to where another sits, and remember the whole list.
 *
 *  The whole list rather than the pair, for the reason `reorderCheckouts` gives:
 *  an order stored as a list is one write that cannot half-apply, and it is what
 *  the next render reads. Both runs go in, so main's order and the worktrees' are
 *  one key and a session moving in or out of main keeps its place.
 *
 *  @param {string} path
 *  @param {string} moved
 *  @param {string} onto
 *  @param {boolean} after whether the pointer was past the target row's middle
 *  @param {import('../snapshot').SessionView[]} shown
 */
function dropSessionRow(path, moved, onto, after, shown) {
  const ids = shown.map((s) => s.id).filter((id) => id !== moved);
  const at = ids.indexOf(onto);
  /* **Which side of the row you let go on**, which is what makes the bottom of the
     list reachable at all. It landed before the target in both directions, so a
     drop on the last row put the moved one second-from-last and no gesture could
     put it last — while the comment claimed parity with `startTabDrag`, whose
     `placeAt` compares against each tab's *middle* and appends past the final one. */
  ids.splice(at < 0 ? ids.length : at + (after ? 1 : 0), 0, moved);
  setSessionOrder(path, ids);
  renderRail();
}

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
 *  **The verbs are not here**, and `addRow` carries that argument: they were, for
 *  one commit, and three labels do not fit a header that has to survive a 210px
 *  rail.
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
  /* The whole strip folds on a double-click as well. The caret is still the
     control — this is the gesture people try on a header that looks like a folder,
     and it cost nothing to answer. A *single* click stays "go there": the name
     already means that, and one gesture cannot mean both. */
  head.ondblclick = () => setFolded(c.path, !shut);

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
  if (inTrouble(p)) return 'build';
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
 *  replies drafted and posted only on a go. The overlay flow is the menu's second
 *  review item — one session that reads everything, hands the set over once and
 *  carries out what you pick — but the cards are not good enough to be the only way
 *  through a review yet, and a button whose result you have to learn a new screen
 *  for is a worse default than one that hands you a terminal. It keeps the button
 *  until the other has been used in anger and earned it. */
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
    /* Two review verbs, and the first is the button's. Both are one agent you
       watch and both read the threads; the difference is where you answer. The pane
       asks as it goes, in the terminal. The other reads everything first, hands the
       whole set over at once, and takes your answers on the cards — then the same
       session applies and posts. Having two is deliberate for now: the cards are a
       contender rather than the default, so the flow that needs no new screen keeps
       the button and this menu is where the other one lives.
       **Named after the job, not after the machinery.** They read `handle in a
       pane` and `read into the cards`, which named where the work happened and how
       it was carried — two things a person picking a menu item does not yet know
       and has no way to choose between. Both are the same verb, so both say it,
       and the only difference the label has to carry is that the second one puts a
       screen in front of you. */
    ['handle review', null, () => startHandleReview(p.number, btn)],
    ['handle review in UI', null, () => void Review.startSession(p.number, btn)],
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

/** A hand-triggered run against a PR. `action` is the endpoint; `label` is what
 *  the button says, because the endpoint's name is not the useful word on a row.
 *  A refusal from the guard table is shown verbatim: it is the whole point of
 *  triggering by hand. */
function actionButton(/** @type {import('../snapshot').PrView} */ p, /** @type {string} */ action, /** @type {string} */ label) {
  const b = el('button', 'pract', label);
  /* **The PR's own base, not the checkout's default.** This said "Rebase on
     develop" on every repo, and `snap.upstream_ref` was only half a fix: the run
     this describes resolves `rebase_target`, which takes the PR's `base_ref` and
     falls back to `upstream_ref` only when the poller does not know one. A PR
     against a release branch, or stacked on another PR's head, would be promised
     one ancestor and force-pushed onto another — and this is the one button here
     that rewrites published history. */
  b.title = `Rebase on ${p.base_ref || snap.upstream_ref || 'its base'},`
    + ' fix what CI says, push — in a pane you can take over';
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

/** What sits where the two forge panes would be, when the checkout has no forge.
 *
 *  One line, and it says what to do about it rather than only what is missing —
 *  the panes are off because there is no remote to ask, which is a fact about the
 *  checkout and not a fault. Drawn rather than hidden entirely, because a checkout
 *  that *should* have a remote and does not is worth one sentence; silence there
 *  would read as the panes having been removed. */
function noForge() {
  /* **The PR pane only.** The review queue has its own answer — a checkout with a
     `reviews_command` has a queue and no forge — and this line used to claim both. */
  const line = el('div', 'noforge', 'No GitHub remote here, so the PR pane is off.');
  /* **"then restart" is not padding.** `repos` is built once in `AppState::new`,
     so adding the remote does not bring the pane back on its own, and advice that
     does not work is worse than none. */
  line.title = 'Add an upstream remote, or set one in settings, then restart the app';
  return line;
}

/** Fill the PR pane, keeping every row whose own signature has not moved.
 *
 *  Split out of a `prGroup()` that returned a fresh tree: the pane is redrawn
 *  whenever the rail is, and the rail is redrawn whenever any session's state
 *  moves — none of which this pane shows. `core.reconcile` has the measurements.
 *
 *  The head names its own inputs rather than taking the rail's `chrome`, the way
 *  `queue.js` does for the pane beside it: what it draws is the PR list and the
 *  poll, and neither moves when a session does. */
function fillPrGroup(/** @type {HTMLElement} */ block, /** @type {HTMLElement} */ group) {
  const prs = snap.prs || [];
  /* It lists one checkout's PRs — the one you are in — and it sits below the
     scroller, so the block whose colour would have said which has scrolled away.
     The band says it instead. Only with several checkouts open, like every other
     piece of this chrome. */
  /* On the *block*, not on this group, and that is a fix rather than a detail: the
     band drew down `.ws`, which ends where its rows do, while the block below it
     carries the pane's own bottom padding — so the stripe stopped short of the
     window edge and read as an unfinished line. The review queue's is on its block
     for the same reason. */
  const band = CHECKOUTS.length > 1 ? bandOf(activeCheckout().path) : null;
  block.classList.toggle('pr-of-checkout', !!band);
  if (band) block.style.setProperty('--band', `var(--co-${band})`);
  else block.style.removeProperty('--band');

  /* Whether there is an age to show, not what it says: `paintSig` drops every
     `_ms` field, because the text is a `data-clock` node `tick` rewrites in
     place. `snap.pr_poll` earns its place the same way it does in `queue.js` —
     `refreshButton` clears the spinner it started when that counter moves. */
  const hasAge = snap.pr_age_ms != null && !snap.pr_polling;
  const headSig = paintSig([showPrs, prs, snap.pr_error, snap.pr_poll ?? 0,
    !!snap.pr_polling, hasAge, band]);

  /** @type {any[]} */
  const items = [{ key: 'head', sig: headSig, build: () => prHead(prs) }];
  if (!showPrs) {
    reconcile(group, items);
    return;
  }

  /* The body lives in its own element rather than loose in the group, so this
     pane has the shape the review queue already had: a head that stays put and a
     list that scrolls under it. That is what lets one rule cap both at ten rows
     without either pane having to know how tall its own head is — and it is why
     the two empty states had drifted apart in the first place, each being styled
     where it happened to sit. */
  items.push({
    key: 'list',
    sig: 'prlist',
    build: () => el('div', 'prlist'),
    fill: (/** @type {HTMLElement} */ list) => reconcile(list, prRowItems(prs)),
  });
  reconcile(group, items);
}

/** The PR pane's head: the count, the poll age and the refresh button. */
function prHead(/** @type {any[]} */ prs) {
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
    const failing = prs.filter(inTrouble).length;
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
  return head;
}

/** One item per PR row, plus the two states that replace the whole list. */
function prRowItems(/** @type {any[]} */ prs) {
  // Held in a local, because the narrowing does not reach inside `build`.
  const failed = snap.pr_error;
  if (failed) {
    return [{
      key: 'error',
      sig: paintSig([failed]),
      build: () => {
        const e = el('div', 'railbtn', failed.slice(0, 120));
        e.style.color = 'var(--bad)';
        return e;
      },
    }];
  }
  if (!prs.length) {
    return [{ key: 'empty', sig: 'none', build: () => el('div', 'railbtn', 'none open') }];
  }
  /* Everything a row reads from outside the PR itself is named here: its
     automation record, which draws the `fixing` chip, and whether its session is
     the one you are in, which marks the `session` chip. A reused row keeps the
     closures it was built with, so a signature that missed one would leave a chip
     pointing at the wrong place. */
  return prs.slice(0, QUEUE_MAX).map((/** @type {any} */ p) => ({
    key: `pr:${p.number}`,
    sig: paintSig([p, (snap.automation || {})[p.number], p.session === selected]),
    build: () => prRow(p),
  }));
}

/** One PR: what it is, what it wants, and the one chip that moves you. */
function prRow(/** @type {any} */ p) {

    // Rows for PRs that already have a session are dimmed, and the chip at the
    // end of the row goes to it (§9).
    const row = el('a', 'prrow' + (p.session ? ' linked' : ''));
    row.href = safeHref(p.url);
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
    const needsFix = inTrouble(p);

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
  return row;
}

/** One checkout's sessions: a single list, and the two ways to add to it.
 *
 *  **This replaces two headed groups, and the reason is a count.** With two
 *  projects open the rail drew two project headers, four group headings and four
 *  "no sessions" lines to carry two sessions — five times as much chrome as
 *  content, and none of it shrank when a group was empty. The headings are gone;
 *  main and worktree sessions share one list, ordered newest first within each.
 *
 *  **Main is marked and a worktree is not**, which is the whole of what the two
 *  headings were saying. A tag on every row would be a column of one repeated
 *  word — nearly every session is a worktree session — so only the exception
 *  carries one, and it means something every time it appears.
 */
/** Fill one checkout's list, keeping every row whose own signature has not moved.
 *
 *  **It fills a box rather than returning one**, and that is the change: it used
 *  to build a fresh `.ws` group per repaint, which threw away every row in it.
 *  `core.reconcile` carries the measurements — a row rebuilt under the pointer
 *  loses its hover highlight for as long as the rebuilds keep coming.
 *
 *  `chrome` is the signature everything that is not a session row takes. Only the
 *  rows are worth naming precisely: they are what a pointer rests on, there are
 *  many of them, and a session's own slice of the snapshot is exactly what one
 *  draws from. */
function fillSessions(/** @type {HTMLElement} */ group, /** @type {import('./core.js').Target} */ c, /** @type {import('../snapshot').Snapshot} */ state, /** @type {import('../snapshot').WorkspaceView | undefined} */ main, /** @type {string} */ chrome, /** @type {boolean} */ titled = false) {
  /* One reader for both the header's verbs and this list — see `checkoutFacts`.
     `main` is still a parameter because the caller has it, and the two must be the
     same workspace. */
  const { mainActive, treeActive, archived, external } = checkoutFacts(c, state);

  /** @type {any[]} */
  const items = [];

  /* **A head only when nothing above it is one.** With several checkouts open the
     `.co-head` names each block and this would be a second heading saying less;
     with one — which is every install today — there is no header at all, and the
     rail opened on a bare session row. It counts what it holds, the way the two
     panes below the rail do. */
  const shut = folded.has(c.path);
  if (titled) {
    /* **The project's own name, not the word "sessions".** A rail that holds one
       project and calls its list `Sessions` is naming the obvious and leaving out
       the thing worth knowing — which checkout all of this is in. Set like the
       multi-checkout header's name, so opening a second project changes where the
       name sits and not what it looks like. Folds on a double-click, the same
       gesture that folds the header it mirrors. */
    items.push({
      key: 'head',
      sig: chrome,
      build: () => {
        const head = el('div', 'ws-title');
        head.title = repoSummary(c);
        head.appendChild(el('span', 'co-name solo', c.name));
        const live = mainActive.length + treeActive.length;
        head.appendChild(el('span', 'ws-count', live ? String(live) : 'none'));
        head.ondblclick = () => setFolded(c.path, !shut);
        return head;
      },
    });
    if (shut) {
      reconcile(group, items);
      return;
    }
  }

  /* **The press shows before the daemon answers.** A worktree is a POST, the
     repo's hooks and a `claude` boot; a session in main is a spawn — ten seconds
     and four in which nothing appeared in the rail at all, so the button read as
     dead and pressing again was the reasonable thing to do. The row lands first and
     the real one replaces it, which is what `pendingSelect` was already for.
     **Where the session will land**: at the top of its own group, which for a
     worktree is below main's rows. It sat above them, so the row you watched
     jumped down past main the moment it was done. */
  const placeholder = { key: 'starting', sig: chrome, build: () => startingRow() };
  const starting = creatingIn() === c.path;
  if (starting && creatingIntoMain()) items.push(placeholder);

  /* Newest first until you say otherwise, and then your order — see
     `inRailOrder`. Main stays first whatever the order says: it is the checkout
     itself, and the rest are cut from it. */
  const mainRows = inRailOrder(c.path, mainActive);
  const treeRows = inRailOrder(c.path, treeActive);
  const shown = [...mainRows, ...treeRows];
  /* **The drop handler closes over this list**, so it belongs in every row's
     signature: a reused row keeps the closures it was built with, and one holding
     last repaint's order would write the wrong order back. Ids rather than the
     sessions, because the order is the only part the handler reads. */
  const order = shown.map((/** @type {import('../snapshot').SessionView} */ s) => s.id);

  const draggable = (/** @type {HTMLElement} */ row, /** @type {import('../snapshot').SessionView} */ s, /** @type {string} */ list) => {
    row.draggable = true;
    row.ondragstart = (ev) => {
      rowDrag = { id: s.id, path: c.path, list };
      // Absent only for a synthetic event nothing here dispatches.
      if (!ev.dataTransfer) return;
      ev.dataTransfer.effectAllowed = 'move';
      // Firefox starts no drag at all without a payload, even one nothing reads.
      ev.dataTransfer.setData('text/plain', s.id);
    };
    row.ondragend = () => { rowDrag = null; renderRail(); };
    // No `preventDefault` is the refusal: the pointer keeps the no-drop cursor
    // over a row in another checkout or the other run, so the gesture says so
    // before it is let go.
    /* One predicate for both handlers. It was written twice — once here and once
       with `moved.id !== s.id` bolted on in `ondrop` — which is two places to edit
       when a third run or a third refusal arrives, and a drop the two disagree
       about would write another checkout's ids into this one's order. */
    const accepts = (/** @type {typeof rowDrag} */ d) => !!d && d.path === c.path && d.list === list;
    row.ondragover = (ev) => { if (accepts(rowDrag)) ev.preventDefault(); };
    row.ondrop = (ev) => {
      ev.preventDefault();
      const moved = rowDrag;
      rowDrag = null;
      if (moved && accepts(moved) && moved.id !== s.id) {
        // Past the middle means after, which is the only way to reach the bottom.
        const box = row.getBoundingClientRect();
        dropSessionRow(c.path, moved.id, s.id, ev.clientY > box.top + box.height / 2, shown);
      } else {
        renderRail();
      }
    };
    return row;
  };

  /* One signature per row, over the session and the workspace it takes its name
     from, with `NOT_DRAWN` taking out the same fields the rail as a whole ignores
     — `changed` and the counts beside it ride every snapshot and no row shows
     them. Everything a row reads from outside itself is named here too. */
  const rowSig = (/** @type {import('../snapshot').SessionView} */ s, /** @type {any} */ w, /** @type {boolean} */ fromMain) =>
    paintSig([s, w, s.id === selected, startingShown(), fromMain, order, c.path], NOT_DRAWN);

  for (const s of mainRows) {
    items.push({
      key: `s:${s.id}`,
      sig: rowSig(s, main, true),
      build: () => draggable(sessionRow(s, main, true), s, 'main'),
    });
  }
  if (starting && !creatingIntoMain()) items.push(placeholder);
  // The workspace is only needed for the name it lends the row.
  for (const s of treeRows) {
    items.push({
      key: `s:${s.id}`,
      sig: rowSig(s, { id: s.workspace }, false),
      build: () => draggable(sessionRow(s, { id: s.workspace }), s, 'tree'),
    });
  }

  items.push({ key: 'add', sig: chrome, build: () => addRow(c, state) });
  /* **The rows, in the box `archivedToggle` opens from the add row above.** The box is
     bounded in CSS at about ten rows rather than truncated, and it is also what
     says "section" now that the control naming it rides the header —
     `archivedToggle` and `archiveOpen` carry the rest of that reasoning.

     Keyed inside it, because an archived row is hovered and clicked like a live
     one and has the same claim on surviving a repaint. */
  if ((archived.length || external.length) && archiveOpen(c, 'sessions', archived).open) {
    items.push({
      key: 'arcbox',
      sig: 'arcbox',
      build: () => el('div', 'arcbox'),
      fill: (/** @type {HTMLElement} */ box) => fillArchive(box, c, archived, external),
    });
  }
  reconcile(group, items);
}

/** The archive, filtered.
 *
 *  **One field, one list, two waves.** The names are in the snapshot this page
 *  already holds, so they filter on the keystroke with no request at all; the
 *  transcripts are megabytes on disk, so they are asked for on a debounce and
 *  appended when they come back. Names first is not a preference: anything drawn
 *  above the wait would be pushed down when the slow half lands, and the row you
 *  were reaching for would move under the pointer.
 *
 *  No divider between the two. A transcript hit says what it is by carrying the
 *  line it matched, where a name hit shows its state.
 *
 *  @param {HTMLElement} box
 *  @param {import('./core.js').Target} c
 *  @param {import('../snapshot').SessionView[]} archived
 *  @param {import('../snapshot').ExternalView[]} external
 */
function fillArchive(box, c, archived, external) {
  const q = (arcQuery[c.path] || '').trim().toLowerCase();
  const answer = arcHits[c.path];
  // Only the answer to the query in the box. An older one is about a word that is
  // no longer in there, and showing it is worse than showing nothing.
  const hits = answer && answer.q === q ? answer.hits : {};
  const busy = !!answer && answer.q === q && answer.busy;

  const named = (/** @type {string} */ text) => !q || text.toLowerCase().includes(q);
  /* The daemon's own first, then the outside ones. Two lists rather than one
     merged by age, because the two rows do not offer the same thing: an archived
     row knows its branch and rebuilds its worktree, an outside row knows a file and
     a directory. Interleaving them would make which of those you get depend on when
     you last spoke to it. */
  const byName = [
    ...[...archived].sort(byNewest)
      .filter((/** @type {import('../snapshot').SessionView} */ s) => named(railName(s, { id: s.workspace }))),
    // Already newest-first, and capped, by the daemon that found them.
    ...external.filter((/** @type {import('../snapshot').ExternalView} */ x) => named(x.title || '')),
  ];
  // A session the name already listed must not appear twice: the name hit wins,
  // because it is the one that is certainly about this conversation.
  const shownIds = new Set(byName.map((r) => r.id));
  const byText = [
    ...[...archived].sort(byNewest)
      .filter((/** @type {import('../snapshot').SessionView} */ s) => !shownIds.has(s.id) && hits[s.id]),
    ...external.filter((/** @type {import('../snapshot').ExternalView} */ x) => !shownIds.has(x.id) && hits[x.id]),
  ];

  const row = (/** @type {any} */ r, /** @type {string | undefined} */ said) => ('last_used_ms' in r
    ? { key: `ext:${r.id}`, sig: paintSig([r, c.path, said ?? ''], NOT_DRAWN), build: () => externalRow(c, r, said) }
    : { key: `arc:${r.id}`, sig: paintSig([r, r.id === selected, said ?? ''], NOT_DRAWN), build: () => archivedRow(r, said) });

  reconcile(box, [
    {
      /* Built once and kept, whatever else moves: it holds the caret and the
         focus, and a rebuilt input loses both. Its count is written in `fill`
         instead, which is the one part of it that changes. */
      key: 'filter',
      sig: 'filter',
      build: () => archiveFilter(c),
      fill: (/** @type {HTMLElement} */ f) => {
        const n = f.querySelector('.arcn');
        if (n) n.textContent = q ? `${byName.length + byText.length} of ${archived.length + external.length}` : '';
      },
    },
    ...byName.map((r) => row(r, undefined)),
    ...byText.map((r) => row(r, hits[r.id])),
    // Last, always: hits land above it as they arrive, so nothing already drawn
    // moves when the slow half of the search finishes.
    ...(busy ? [{ key: 'arcbusy', sig: 'arcbusy', build: () => searchingRow() }] : []),
  ]);
}

/** The filter line, pinned at the top of the archive box.
 *
 *  Inside the box rather than above it, because the box is what the caret opens: a
 *  field over a closed archive is a control for something that is not on screen.
 *
 *  @param {import('./core.js').Target} c
 */
function archiveFilter(c) {
  const wrap = el('div', 'arcfilter');
  const input = /** @type {HTMLInputElement} */ (el('input', 'arcq'));
  input.type = 'search';
  input.placeholder = 'filter past conversations';
  input.value = arcQuery[c.path] || '';
  input.oninput = () => {
    arcQuery[c.path] = input.value;
    askTranscripts(c);
    renderRail();
  };
  wrap.appendChild(input);
  // Written by `fillArchive`, because it counts what that pass drew.
  wrap.appendChild(el('span', 'arcn'));
  return wrap;
}

/** Ask the daemon what the transcripts say, once the typing stops.
 *
 *  Fire-and-forget, like everything else on this path: the answer lands in
 *  `arcHits` and the rail redraws from it. A failure leaves the names showing,
 *  which is the half that was always going to be there.
 *
 *  @param {import('./core.js').Target} c
 */
function askTranscripts(c) {
  const q = (arcQuery[c.path] || '').trim().toLowerCase();
  clearTimeout(arcTimer[c.path]);
  if (q.length < ARC_MIN) {
    delete arcHits[c.path];
    return;
  }
  // `busy` from the keystroke, not from the request: the spinner is the answer to
  // "is something still coming", and the debounce is part of the wait.
  arcHits[c.path] = { q, busy: true, hits: {} };
  arcTimer[c.path] = setTimeout(() => {
    void getOn(c, `/api/archive/search?find=${encodeURIComponent(q)}`)
      .then((/** @type {any} */ r) => {
        // The query moved on while this was in flight. Its answer is about a word
        // that is no longer in the box.
        if ((arcQuery[c.path] || '').trim().toLowerCase() !== q) return;
        /** @type {Record<string, string>} */
        const hits = {};
        for (const h of r.hits || []) hits[h.id] = h.line;
        arcHits[c.path] = { q, busy: false, hits };
      })
      .catch(() => {
        if ((arcQuery[c.path] || '').trim().toLowerCase() === q) arcHits[c.path] = { q, busy: false, hits: {} };
      })
      .finally(() => renderRail());
  }, ARC_DEBOUNCE);
}

/** The last row of the archive while the transcripts are being read. */
function searchingRow() {
  const row = el('div', 'arcbusy');
  row.appendChild(el('span', 'arcspin'));
  row.appendChild(el('span', null, 'searching transcripts'));
  return row;
}

/** A session that has been asked for and does not exist yet.
 *
 *  It wears the selected row's own fill while the centre pane is showing the
 *  create, because the two have to agree about what the board is about.
 *
 *  **It is a button now.** It used not to be, on the reasoning that there is no id
 *  to select and a row that looked pressable and was not would be worse than
 *  silence. That held while the overlay was unconditional; once picking a session
 *  takes the overlay down, this row is the only way back to a create that is still
 *  running — and the output it is now showing is worth coming back to. What it
 *  selects is not a session, it is *the create*, which is why it goes through
 *  `watchStarting` rather than `setSelected`.
 */
function startingRow() {
  const row = el('button', 'sess starting');
  row.type = 'button';
  const watched = startingShown();
  if (watched) row.setAttribute('aria-current', 'true');
  row.onclick = () => watchStarting(true);
  /* **Both lines, or the row is a different height from every other one.** A
     session row is a name over its state, and a placeholder with only the first
     of those sat shorter than the rows around it and read as clipped. So it takes
     the same shape: the name it does not have yet, and what is happening to it.
     `…creating` is the word the rail already uses for a worktree Claude Code has
     not named — see `railName` — so the placeholder and the row that replaces it
     say the same thing. */
  const top = el('div', 'sess-row');
  top.appendChild(el('span', 'conn-dot'));
  top.appendChild(el('span', 'sess-name pending', '\u2026creating'));
  row.appendChild(top);
  const sub = el('div', 'sess-sub');
  sub.appendChild(el('span', 'sess-state', creating() ?? 'starting'));
  row.appendChild(sub);
  /* The row's own bottom spacer, which every real row ends on. Without it this one
     measured 49px against their 60 and read as clipped — the height of a row is
     `.sess-row` plus `.sess-sub` plus this, and a placeholder that skipped it was
     not the same object. */
  row.appendChild(el('div', 'sess-pad'));
  return row;
}

/** The two ways to start a session and the archive, on one line under the list.
 *
 *  **Under the list, not on the header, and that was tried the other way.** The
 *  verbs spent a commit on `checkoutHead`, which costs no row and puts them beside
 *  the name they act on — and then three labels did not fit: the width check
 *  failed by 52px at `COLS.rail.min`, and the fixes for that were clipping a word
 *  or trading `archived` for an icon. Here they have the room, and the row reads as
 *  one sentence about the list above it: what you can add on the left, what it is
 *  holding back on the right.
 *
 *  **Both verbs say what they make, and a worktree session is a session too** —
 *  which is why the second of these is `+ main` and not `+ session`. `+ worktree`
 *  leads because it is nearly every session there is; main is the exception, and
 *  often refused.
 *
 *  @param {import('./core.js').Target} c
 *  @param {import('../snapshot').Snapshot} state
 */
function addRow(c, state) {
  const row = el('div', 'ws-add');
  const { main, mainActive, archived, external } = checkoutFacts(c, state);

  /* Dead while one is being cut, and it says which one in the tooltip. Two things
     make one press look like none: the POST is a worktree, the repo's hooks and a
     `claude` boot, and the row that lands after it says `…creating` for as long as
     it takes Claude Code to name the tree. Both are covered — the claim in `core`
     for the first, `pending` for the second — because the second window is the
     longer one and a `+` that came back to life halfway is the same invitation to
     press again.

     Live ones only. A placeholder session that died before `SessionStart` keeps
     the placeholder workspace for good, and counting that would leave the button
     dead until a restart. */
  const cutting = creating()
    || (state.sessions.some((/** @type {import('../snapshot').SessionView} */ s) => pending(s) && !isArchived(s)) ? 'creating a worktree' : null);
  const tree = el('button', 'addbtn', '+ worktree');
  tree.disabled = !!cutting;
  /* Naming a worktree is not a control: it is shift-click and `Shift N`, and it
     stays in this tooltip because that is the only place it is ever discovered. */
  tree.title = cutting || `New worktree session · ${MOD_LABEL} N (shift-click to name it)`;
  tree.onclick = (/** @type {MouseEvent} */ ev) => void newWorktree(ev.shiftKey, c);
  row.appendChild(tree);

  row.appendChild(el('span', 'addsep', '\u00b7'));

  /* Main is exclusive: one active session at a time, and no queue. While it is
     occupied the button is disabled and says who holds it — unless
     `allow_several_in_main` is set, which is a config-file decision the daemon
     reports in the snapshot, and then a second one is a thing you do on purpose. */
  const occupant = mainActive.find((/** @type {import('../snapshot').SessionView} */ s) => s.id === main?.occupant && s.alive);
  const several = !!state.several_in_main;
  const inMain = el('button', 'addbtn', '+ main');
  // Occupied, or already making something: the second reason is the one that used
  // to be invisible, and pressing through it is how you get two of them.
  inMain.disabled = !main || (!!occupant && !several) || !!creating();
  /* The chord belongs in the tooltip of the button that does the same thing:
     finding the button once is how you stop needing it. `MOD_LABEL` rather than a
     literal — the modifier is ⌘ on macOS and Ctrl everywhere else. */
  inMain.title = creating()
    ? creating() ?? ''
    : occupant
      ? `main is held by ${occupant.title || occupant.id.slice(0, 8)}${several ? ' · another is allowed' : ''}`
      : `New session in the main checkout · ${MOD_LABEL} Shift N`;
  // `void` is how this file says fire-and-forget out loud; the row lands on the
  // next snapshot either way.
  inMain.onclick = () => { if (main) void newSession(main.id, c); };
  row.appendChild(inMain);

  /* Hard right, so the row says one thing about the list above it rather than two:
     what you can add, and what it is holding back. Absent when there is no archive,
     which is the one case where the row has nothing to say on that side. */
  const fold = archivedToggle(c, 'sessions', archived, external);
  if (fold) row.appendChild(fold);

  return row;
}

/** The four things both the header and the list read off one snapshot.
 *
 *  One reader, because they used to be two: the add row counted the archive and
 *  the box below it drew the rows, and two filters over one list are two things
 *  that can disagree about what is in it.
 *
 *  @param {import('./core.js').Target} c
 *  @param {import('../snapshot').Snapshot} state
 */
function checkoutFacts(c, state) {
  const main = mainWorkspace(state);
  const mainSessions = main ? sessionsOf(main.id, state) : [];
  /* Anything that is not main's belongs to a worktree — by session, not by
     workspace. A worktree Claude Code has not named yet has no workspace record at
     all, only a session pointing at the placeholder. */
  const treeSessions = state.sessions.filter((/** @type {import('../snapshot').SessionView} */ s) => s.workspace !== main?.id);
  return {
    main,
    mainSessions,
    treeSessions,
    mainActive: mainSessions.filter((/** @type {import('../snapshot').SessionView} */ s) => !isArchived(s)),
    treeActive: treeSessions.filter((/** @type {import('../snapshot').SessionView} */ s) => !isArchived(s)),
    archived: [...mainSessions, ...treeSessions].filter(isConversation),
    /* Conversations orchd never started, which the daemon finds by reading Claude
       Code's own transcript directory for each of this checkout's workspaces. In
       the same fold as the archive because "the conversation I had in this
       checkout" is one question, and which program started it is not part of it. */
    external: state.external || [],
  };
}

/** Is this checkout's archive open?
 *
 *  One answer for the toggle and for the block it opens: they are drawn by
 *  different functions now, and a second reading of this is how they come to
 *  disagree about what the caret is pointing at.
 */
function archiveOpen(/** @type {import('./core.js').Target} */ c, /** @type {string} */ key, /** @type {import('../snapshot').SessionView[]} */ sessions) {
  // Qualified, because `sessions` names a fold in every checkout and one open
  // archive would open all of them.
  const held = `${c.path}\u0000${key}`;
  // Opened whenever the conversation you are looking at is in here, so the rail
  // never goes silent about what the centre pane is showing.
  return { held, open: showArchived[held] || sessions.some((/** @type {import('../snapshot').SessionView} */ s) => s.id === selected) };
}

/** The archive's own control, which rides the header.
 *
 *  `null` when nothing is archived: a count of zero is not news, and the header is
 *  better off with the space.
 *
 *  **No count on it.** The number said nothing you act on, and the box it opens
 *  says it better — as `3 of 24`, against the query it is answering. What it cost
 *  was the widest label on a header that has to hold three of them.
 */
function archivedToggle(/** @type {import('./core.js').Target} */ c, /** @type {string} */ key, /** @type {import('../snapshot').SessionView[]} */ sessions, /** @type {import('../snapshot').ExternalView[]} */ external) {
  /* Both counted, because both are behind this caret. A checkout whose only past
     conversations were started from a terminal has an archive worth opening, and
     counting the daemon's alone would leave it with no caret at all. */
  const total = sessions.length + external.length;
  if (!total) return null;
  const { held, open } = archiveOpen(c, key, sessions);
  const btn = el('button', 'arctoggle');
  btn.type = 'button';
  btn.setAttribute('aria-expanded', String(open));
  btn.title = `${total} past conversation${total === 1 ? '' : 's'} in this checkout`;
  btn.appendChild(caret());
  /* The word stays `archived`: `stateLabel` in `core.js` prints it on the rows
     inside the box, so a control called anything else would be the one place in the
     app using a second word for one state. **A glass was tried here and taken
     back** — it fits anywhere and says "search", which is what the box does, but
     the caret is the thing that says the rows are *behind* it, and this control is
     a fold before it is a search. It has the room for the word where it sits. */
  btn.appendChild(el('span', null, 'archived'));
  btn.onclick = (/** @type {MouseEvent} */ ev) => {
    // The header it rides is the drag handle and folds on a double-click.
    ev.stopPropagation();
    /* Opening it asks the daemon to look again for conversations it did not start.
       Its poller runs on a minute, and the terminal you are opening this to find is
       usually the one you closed a moment ago. Fire-and-forget: the answer arrives
       as a snapshot like everything else, and a refresh that fails leaves the list
       exactly as it was, which is not news worth a toast. */
    if (!open) void callOn(c, '/api/external/refresh').catch(() => undefined);
    showArchived[held] = !open;
    renderRail();
  };
  return btn;
}

/** A past conversation: which worktree it was in, and how long ago.
 *
 *  No state word — `archived` is the state, and the section it sits in already
 *  says it. Clicking rebuilds what it needs and resumes it. */
function archivedRow(/** @type {import('../snapshot').SessionView} */ s, /** @type {string | undefined} */ said) {
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

  if (said) {
    /* This row is here because of what was said in it, not because of its name, so
       the line it matched is what the row has to show — the state is the answer to
       a question nobody asked of this row. */
    btn.appendChild(el('div', 'sess-sub said', said, said));
  } else if (!s.resumable) {
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
    ['copy branch', null, sessionBranch(s) ? () => copyBranch(s) : null],
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

/** A conversation this checkout has that orchd did not start.
 *
 *  **It carries no state word and no dot of its own.** The fold it sits in says
 *  "archived" and the sub-line says where it came from; a second vocabulary for
 *  what is, from the rail's point of view, the same thing, a conversation you can
 *  come back to, is a distinction nobody asked for.
 *
 *  No context menu either. Rename, fork and delete are all verbs over a session
 *  record, and there is none until this is resumed; delete in particular would have
 *  to mean deleting Claude Code's own file, which `api::delete_session` is explicit
 *  about never doing.
 */
function externalRow(/** @type {import('./core.js').Target} */ c, /** @type {import('../snapshot').ExternalView} */ x, /** @type {string | undefined} */ said) {
  const btn = el('button', 'sess arc');
  btn.dataset.id = x.id;
  const row = el('div', 'sess-row');
  row.appendChild(el('span', 'dot archived'));
  /* Claude Code's own `ai-title`, and nothing to fall back to: an outside
     conversation has no name you could have typed, and the workspace it ran in is
     on the line below. */
  const name = x.title || 'untitled';
  row.appendChild(el('span', 'sess-name', name, name));
  row.appendChild(clock('sess-id', x.last_used_ms, ' ago'));
  btn.appendChild(row);
  /* Where it ran, when that is not the obvious answer. `outside` is the word for
     what makes this row different: the daemon never started it — unless the row is
     a transcript hit, and then the line it matched is why it is here at all. */
  if (said) btn.appendChild(el('div', 'sess-sub said', said, said));
  else btn.appendChild(el('div', 'sess-sub', x.workspace === 'main' ? 'outside' : `outside · ${x.workspace}`));
  btn.onclick = () => openExternal(c, x);
  return btn;
}

/** Take an outside conversation over: the daemon relaunches it under its own id.
 *
 *  Addressed to the checkout the row was drawn from rather than to
 *  `activeCheckout`, for the reason `callFor` gives: the rail draws every
 *  checkout, so "the one you are in" is the wrong target for anything on it.
 */
async function openExternal(/** @type {import('./core.js').Target} */ c, /** @type {import('../snapshot').ExternalView} */ x) {
  /* **A destructive ask, which is the only kind there is** — `confirmBox` carries
     that rule. Taking a conversation over appends to its transcript, the daemon
     cannot see a `claude` running in a terminal at all, and turns lost to two
     agents writing one file are not got back. The daemon refuses an unforced adopt
     of a live-looking one; this asks first so that refusal arrives as a question
     rather than as a toast you can do nothing about. */
  if (x.may_be_live && !await confirmBox(
    'This conversation was written to moments ago. If it is still open in a terminal, '
    + 'both agents will be appending to one transcript, and that is how turns go missing.',
    { ok: 'Take it over' },
  )) return;
  try {
    const r = await callOn(c, `/api/external/${x.id}/resume`, { force: x.may_be_live });
    /* The conversation keeps its id, so a terminal left over from an earlier life
       of this id — adopted, then deleted from the rail, then found again — is still
       in `terms` under the key the new pty wants, and `openTerm` would hand back
       the corpse. `openArchived` closes it for the same reason. */
    Term.close(c, `session:${r.session}`);
    setPendingSelect(r.session);
    // The tree has been checked out for something else since this conversation
    // last spoke. Worth saying, not worth refusing over.
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

function sessionRow(/** @type {import('../snapshot').SessionView} */ s, /** @type {{ id: string | null } | undefined} */ w, /** @type {boolean} */ fromMain = false) {
  const btn = el('button', 'sess');
  /* **Current means "what the centre pane is showing", not "what is selected".**
     While you are watching a create, the pane is showing the create — the selection
     is still whatever session you came from, and marking that row as well would put
     two rows in one state. `startingShown` is false the moment you pick a session,
     so this reads as normal again immediately. */
  btn.setAttribute('aria-current', String(s.id === selected && !startingShown()));
  // So a rename can find this row's name span again after any re-render.
  btn.dataset.id = s.id;

  const row = el('div', 'sess-row');
  row.appendChild(el('span', 'dot ' + dotClass(s)));
  const liveName = railName(s, w);
  row.appendChild(el('span', 'sess-name' + (pending(s) ? ' pending' : ''), liveName, liveName));
  const forked = forkBadge(s);
  if (forked) row.appendChild(forked);
  /* **Only the exception is marked.** This row is in the main checkout, which is
     the unusual one — nearly every session is a worktree session, so a tag on
     every row would be a column of one repeated word and would spend width the
     name needs. It carries the title as well as the word, because "no tag means
     worktree" is a thing you have to be told once. */
  if (fromMain) {
    row.appendChild(el('span', 'sess-main', 'main',
      'In the main checkout · every other session is in a worktree'));
  }
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
  /* **Said on the row, because the restart is deferred.** A session queued
     mid-turn respawns the moment its turn ends, and without this the pane you are
     watching clears and reattaches for no reason you can see. */
  if (s.restart_queued) {
    const q = sub.appendChild(el('span', 'sess-restart', 'restarts when idle'));
    q.title = 'Respawns on the installed Claude Code once this turn ends — cancel it from the menu';
  }
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
    /* On the installed Claude Code, without quitting the app. One item that
       reads as what it will do: queued, it takes the restart back. Greyed on a
       closed row, where the thing you want is a resume. */
    s.restart_queued
      ? ['cancel the restart', null, () => restartSession(s, true)]
      : ['restart', null, s.alive ? () => restartSession(s, false) : null],
    ['copy id', null, () => copyId(s)],
    ['copy branch', null, sessionBranch(s) ? () => copyBranch(s) : null],
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

/** What the session's directory has checked out (#31).
 *
 *  **The workspace's branch, not the session's**, and they answer different
 *  questions: `SessionView.branch` is what the conversation is *about*, and this is
 *  what `git` in that directory would say now.
 *
 *  For a live session the two agree — `AppState::reconcile` re-stamps the session's
 *  field from its tree on every sweep, because an agent that checks out another
 *  branch really has changed what it is working on. It stops doing that once the
 *  session is archived, deliberately: the swap needs the difference between "this
 *  conversation was about that branch" and "that branch happens to be here now" to
 *  know which conversation travels. So an old row's field can name a branch its
 *  directory has not held for weeks, and what you paste into a terminal has to be
 *  the directory's.
 *
 *  `null` when the tree is gone, which is an archived session whose worktree was
 *  torn down: there is no directory to be on a branch.
 */
function sessionBranch(/** @type {import('../snapshot').SessionView} */ s) {
  return snapshotFor(s.id).workspaces.find((w) => w.id === s.workspace)?.branch || null;
}

/** Copy it, when there is one.
 *
 *  Offered only when there is: an item that copies an empty string is worse than no
 *  item, which is why `fork` is gated the same way.
 */
async function copyBranch(/** @type {import('../snapshot').SessionView} */ s) {
  const branch = sessionBranch(s);
  if (branch && await copyText(branch)) toast('branch copied');
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
/** Respawn a session on the `claude` installed now, or take that back.
 *
 *  The daemon queues rather than restarts: a session mid-turn goes when its turn
 *  ends, so the toast says which of the two happened rather than "restarted". */
async function restartSession(/** @type {import('../snapshot').SessionView} */ s, /** @type {boolean} */ cancel) {
  try {
    const r = await callFor(s.id, `/api/session/${s.id}/restart`, cancel ? { cancel: true } : {});
    if (cancel) toast('restart cancelled');
    else toast(r.now ? 'restarting on the installed Claude Code' : 'restarts when this turn ends');
  } catch (e) {
    toast(reason(e), true);
  }
}

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

/** Say which files stayed behind, because `stash create` cannot carry untracked ones.
 *
 *  Both moves leave them, and both used to say so in their own copy of this — the
 *  cap of four, the wording and the decision to draw it as an error toast written
 *  twice, eighty lines apart. The swap's copy was the only one for a while: the move
 *  said it in a confirm box, and when the box went the fact went with it.
 *
 *  @param {string} where the workspace they stayed in
 *  @param {string[] | undefined} left
 */
function untrackedToast(where, left) {
  if (!left || !left.length) return;
  toast(
    `left in ${where} (untracked, so not carried): ${left.slice(0, 4).join(', ')}`
    + (left.length > 4 ? ` and ${left.length - 4} more` : ''),
    true,
  );
}

/** Move a session out of main, into a worktree of its own.
 *
 *  The swap's missing direction: a swap needs a second branch to exchange, and
 *  this has none — you started something in main, it turned into real work, and
 *  main should be free again. The branch gets a tree named after it, uncommitted
 *  changes travel with it, main goes back to base, and the conversation follows
 *  keeping its id and its place in the rail.
 *
 *  **It used to ask, and no longer does.** Every file under main changes, which is
 *  loud rather than destructive: the branch is still the branch, the uncommitted
 *  changes travel with it, the conversation keeps its id, and moving it back is the
 *  menu item above. A box is for work that cannot be got back, and nothing here
 *  cannot. The untracked files that stay in main are named in a toast — they were
 *  computed and only logged until the box went, and the box was the only place the
 *  product ever said it. */
async function moveOutOfMain(/** @type {import('../snapshot').SessionView} */ s) {
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
    // What did not travel, named — see `untrackedToast`.
    untrackedToast('main', r.untracked_left);
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
 *  It does not ask — a swap is undone by swapping back. What protects it is the
 *  daemon: the swap lock here, and the refusals (mid-turn agent, dirty tree,
 *  stopped rebase) there. The body says why, and `confirmBox` carries the rule. */
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
  /* **No confirm, for the reason `moveOutOfMain` gives.** A swap is reversible by
     swapping back: both branches keep their uncommitted work, both conversations
     keep their ids, and the toast below says where everything ended up. The thing
     that actually protects this is the daemon's `swapping` lock and its refusals —
     a second press while one is running is turned away here, and a mid-rebase tree
     or a mid-turn session is turned away there. */
  swapInFlight = true;
  /* Which of the two this is, said while it runs rather than asked before it. The
     menu item already reads `swap with main` or `move to main`; the toast has to
     agree with the one you pressed, or the only feedback a reversible move gives
     you is about a different verb. */
  toast(holds
    ? `swapping ${wsId} with main${inCheckout(s)}\u2026`
    : `moving ${wsId}'s branch to main${inCheckout(s)}\u2026`);
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
    // What did not travel, named — see `untrackedToast`.
    untrackedToast(wsId, r.untracked_left)
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
  /* **The row is `draggable` now, and a draggable ancestor eats a text selection.**
     Dragging across the word you are fixing starts the row drag instead of
     selecting: the input blurs, and blur commits — so a half-corrected name was
     saved and the rail reordered underneath it. `draggable=false` on the input
     hands the gesture back to the text. */
  input.draggable = false;
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
const barDrawn = { sig: null, name: 'waitbar' };

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
