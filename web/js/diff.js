// The diff viewer, its editable right pane, and the changed-files list that
// drives it. One module because the three call each other; splitting them would
// only have turned that into circular imports.
//
// The syntax colour they all paint with is **not** here: it is `source.js`, which
// knows about strings and nothing about changesets. Three other modules wanted
// it and none of them wanted a diff.

import { $, FORGE_LABEL, activeWorkspaceId, borrowFocus, call, confirmBox, currentSession, currentWorkspaceId, el, get, icon, iconLabel, openMenu, pending, prForWorkspace, returnFocus, snap, reason, toast, paintSig, reconcile, unchanged, workspaceById } from './core.js';
import * as Editor from './editor.js';
import { changeBlocks, revertedText } from './revert.js';
import { charRanges, langFor, lineSegments } from './source.js';
import * as Tree from './tree.js';

// ---------------------------------------------------------------------------
// The changed-files pane. Inside this seam rather than beside it: the list is
// what drives the diff, and the diff redraws the list when it closes — two
// modules that call each other are one module with a line drawn through it.
// ---------------------------------------------------------------------------

/** Behind/ahead against the configured base, with the one action worth offering.
 *
 *  The changed-file list is a poor summary of a branch that has simply fallen
 *  behind: what you want then is to take the base in, not to read a list.
 *
 *  The ref comes from the snapshot rather than being written here: it is a
 *  setting, and this used to print the default as a literal — so a repo that had
 *  edited it read the wrong ref beside numbers measured against the right one. */
function renderDivergence(/** @type {import('../snapshot').WorkspaceView} */ w) {
  const box = $('diverge');
  box.replaceChildren();
  // Reset the class too, not just the children: `on` is what makes this visible,
  // and leaving it behind left an empty bar above the file list.
  box.className = 'diverge';
  if (!w) return;

  /* **Banked work outranks everything else this bar says.** A bank only exists
     while the work is *not* in the tree — a clean re-apply drops the ref in the
     same breath — so its presence is the whole condition, and how far behind the
     branch is stops being the interesting number until it is back. */
  const bank = w.banked;
  /* **The first row of the list, not a banner over it** (option 8 of the behind
     review): an icon in the state's colour, the sentence, and the PR rows' button.
     A strip tinted across the pane read as an alert about the pane; this reads as
     one more thing that changed. */
  const mark = (/** @type {boolean} */ bad) => box.appendChild(bad
    ? icon('dvic', 1.4, 'M8 2.5 14 13H2z', 'M8 6.5v3', 'M8 11.4h.01')
    : icon('dvic', 1.4, 'M5 2.5v8', 'M2.5 8 5 10.5 7.5 8', 'M11 2.7a1.8 1.8 0 1 0 0 3.6a1.8 1.8 0 1 0 0-3.6', 'M11 6.3V13.5'));
  if (w.rebasing) {
    box.className = 'diverge on bad';
    mark(true);
    box.appendChild(el('span', 'dvtext', bank
      ? `rebase stopped on conflicts · ${files(bank)} banked`
      : 'rebase stopped on conflicts'));
    /* **Finish it, not undo it.** Abort threw away the rebase you asked for at
       the first conflict; the session in this workspace can resolve the
       conflicts and continue, which is what you pressed Rebase to get. Abort is
       `git rebase --abort` in a shell for the case that should not finish. */
    const r = dvButton('Resolve in session');
    r.title = 'Ask the session in this workspace to fix the conflicts and finish the rebase';
    r.onclick = () => press(r, `/api/workspace/${encodeURIComponent(w.id)}/rebase/resolve`, 'handed it over');
    box.appendChild(r);
    return;
  }
  if (bank) {
    box.className = 'diverge on bad';
    mark(true);
    /* Worded for both states this reaches, which is why it does not say the apply
       failed: a rebase stopped part-way and then finished *in a session* leaves the
       bank standing and never tried, and `rebasing` is false by then. What is
       always true is that the work is in the ref and not in the tree. */
    const t = el('span', 'dvtext', `${files(bank)} banked, not in this tree`);
    // The ref is the recovery that needs nothing of ours, so it is one hover away
    // rather than in a bar that has to stay short.
    t.title = `git stash apply ${bank.at}`;
    box.appendChild(t);

    const ws = encodeURIComponent(w.id);
    const r = dvButton('Resolve in session');
    r.title = 'Hand the conflict to the session in this workspace';
    // Held down until the answer, like Rebase below. A second press here types the
    // whole message into the agent again as a second user turn, and a second press
    // of `Put back` fires a second apply.
    r.onclick = () => press(r, `/api/workspace/${ws}/wip/resolve`, 'handed it over');
    box.appendChild(r);

    const back = dvButton('Put back');
    back.title = 'Try the re-apply again';
    back.onclick = () => press(back, `/api/workspace/${ws}/wip/restore`, 'put back');
    box.appendChild(back);

    // The one thing here git cannot undo: nothing else points at that object, so
    // once it is collected the content is gone. Same asymmetry as `discard changes`
    // on a row.
    const d = dvButton('Discard');
    d.title = 'Forget the banked copy of your work';
    d.onclick = async () => {
      /* One of the four things this UI still asks about — see the rule at
         `confirmBox`. It is the weakest of the four and still earns it: dropping
         the ref leaves the commit dangling, so `git show` has the work until gc
         collects it, but only for somebody holding the sha. The toast below is
         where that sha is handed over; the question is what stops it being needed. */
      if (!await confirmBox(
        `Forget the banked work?\n\n${files(bank)} from before the rebase. Git keeps it `
        + 'for a couple of weeks as an unreferenced commit and then collects it.',
        { ok: 'Discard' })) return;
      // Through `press` like the two buttons above it, so the success-with-a-warning
      // line in `act` is not one this button alone lacks. The verb is a function
      // because the sha only exists in the answer.
      await press(d, `/api/workspace/${ws}/wip/discard`,
        // The sha, because nothing will ever name it again and it is the whole of
        // what "recoverable by hand" means.
        (r) => (r?.was ? `discarded — it was ${String(r.was).slice(0, 12)}, until git collects it` : 'discarded'));
    };
    box.appendChild(d);
    return;
  }
  if (!w.behind) {
    box.className = 'diverge';
    return;
  }

  box.className = 'diverge on';
  mark(false);
  const ahead = w.ahead ? `, ${w.ahead} ahead` : '';
  const base = snap.upstream_ref || 'the base';
  const say = el('span', 'dvtext');
  say.append(el('b', null, `${w.behind} commit${w.behind === 1 ? '' : 's'}`), ` on ${baseLeaf()} you do not have`);
  say.title = `${w.behind} behind ${base}${ahead}`;
  box.appendChild(say);
  const b = dvButton('Rebase', 'M8 3v9', 'M4.5 8.5 8 12l3.5-3.5');
  // Never a merge: history stays linear.
  b.title = `git rebase ${base} in ${w.id}`;
  b.onclick = async () => {
    b.disabled = true;
    await act(`/api/workspace/${encodeURIComponent(w.id)}/rebase`, 'rebased');
    b.disabled = false;
  };
  box.appendChild(b);
}

/** A button on the divergence row: the PR rows' shape, an icon in the row's
 *  colour beside the word. Only the row's own verb gets an icon; the bank's other
 *  three are plain words, so the one that undoes the state stands out. */
function dvButton(/** @type {string} */ label, /** @type {string[]} */ ...ds) {
  return iconLabel('button', 'dvbtn', label, ...ds);
}

/** `3 changed files`, or `1 changed file`. */
function files(/** @type {import('../snapshot').BankedView} */ bank) {
  return `${bank.files} changed file${bank.files === 1 ? '' : 's'}`;
}

/** `act`, with the button held down until the answer comes back.
 *
 *  Every button on the bank strip does something a second press would do twice —
 *  type the conflict at the agent again, run a second apply — and the snapshot that
 *  redraws the strip arrives after the response, not with the click.
 */
/** @param {HTMLButtonElement} btn @param {string} path
 *  @param {string | ((r: any) => string)} verb */
async function press(btn, path, verb) {
  btn.disabled = true;
  try {
    await act(path, verb);
  } finally {
    btn.disabled = false;
  }
}

/** @param {string} path
 *  @param {string | ((r: any) => string)} verb what to say — a function when the
 *         sentence depends on the answer, as the bank's discard does. */
async function act(path, verb) {
  try {
    const r = await call(path);
    toast(typeof verb === 'function' ? verb(r) : verb);
    // An endpoint can succeed and still have something to say — a rebase onto a
    // base whose fetch failed, say. Surface it as its own warning toast, the way
    // openArchived and forkSession already do.
    if (r && r.warning) toast(r.warning, true);
  } catch (e) {
    toast(reason(e), true);
  }
}

/** Open a changed file on the forge.
 *
 *  The URL is built by the daemon, not here: which ref to read the file at and
 *  what a blob URL looks like are both the forge's business, and a second forge
 *  is meant to be a `ForgeKind` arm rather than an edit to this file. So the
 *  client says only which file it wants. */
function openFileOnForge(/** @type {import('../snapshot').WorkspaceView} */ w, /** @type {string} */ path) {
  call('/api/open/file', { workspace: w.id, path })
    .catch((err) => toast(err.message, true));
}

/** Stage a file, unstage it, or throw its working-tree changes away.
 *
 *  **`discard` asks first, and names the file.** `git restore` overwrites the
 *  working tree from the index, so uncommitted content is gone — there is no
 *  reflog for a change that was never committed. The other two are reversible by
 *  pressing the other one, so they go straight through.
 *
 *  The daemon refuses what the row cannot do, and refuses everything while an
 *  agent is mid-turn in that workspace, so this does not pre-judge: it sends the
 *  verb and shows the answer. */
async function fileVerb(/** @type {import('../snapshot').WorkspaceView} */ w, /** @type {import('../base').DiffFile} */ f, /** @type {string} */ verb) {
  if (verb === 'discard') {
    const yes = await confirmBox(
      `Throw away your changes to ${f.path}? Uncommitted content cannot be `
      + 'recovered — git keeps no copy of it.',
      { ok: 'Discard' },
    );
    if (!yes) return;
  }
  try {
    await call('/api/file/verb', { workspace: w.id, path: f.path, verb });
    toast(`${verb === 'discard' ? 'discarded' : verb + 'd'} ${f.path}`);
  } catch (e) {
    toast(reason(e), true);
  }
}

/** What a changed row can be asked to do, which is not the same for every row.
 *
 *  The list is `git diff <merge-base>`, so most rows on a PR branch differ from
 *  the base because of a **commit** and have nothing to stage or discard: those
 *  get no git verbs at all, rather than items that would do nothing. `staged` and
 *  `unstaged` come from `git status` on the daemon side for exactly this.
 *
 *  `discard` is last, being the only one that cannot be pressed back; `open on
 *  <forge>` stays on top because it is the one you reach for while reading. */
function fileMenu(/** @type {import('../snapshot').WorkspaceView} */ w, /** @type {import('../base').DiffFile} */ f) {
  /** @type {[string, string | null, (() => void) | null][]} */
  const items = [];
  const linkable = f.status !== '?' && !f.path.endsWith('/');
  items.push([`open on ${FORGE_LABEL[snap.forge]}`, null, linkable ? () => openFileOnForge(w, f.path) : null]);
  if (f.unstaged || f.status === '?') items.push(['stage', null, () => fileVerb(w, f, 'stage')]);
  if (f.staged) items.push(['unstage', null, () => fileVerb(w, f, 'unstage')]);
  if (f.unstaged) items.push(['discard changes', 'danger', () => fileVerb(w, f, 'discard')]);
  return items;
}

/** "The daemon has not counted this tree yet."
 *
 *  The same pulsing dot the empty terminal uses while the first snapshot is in
 *  flight, because it means the same thing: the app is up and this particular
 *  answer is still on its way. Reusing `.conn-dot` rather than inventing a
 *  spinner also inherits the reduced-motion rule that already names it — a new
 *  animation would have needed adding to that block, and forgetting is how one
 *  keeps moving for somebody who asked for none. */
function counting() {
  const box = el('div', 'fempty counting');
  box.appendChild(el('span', 'conn-dot'));
  box.appendChild(el('span', null, 'counting the changed files…'));
  // The pane is a live region for this one moment: a screen reader is otherwise
  // told nothing between an empty list and a full one.
  box.setAttribute('aria-live', 'polite');
  return box;
}

/** Workspace fields this pane is handed and never shows.
 *
 *  Read off what `renderFiles`, `renderDivergence` and `fileMenu` actually touch:
 *  `banked`, `rebasing`, `id`, `changed`, `changed_since`, `changed_total`,
 *  `measured` and `is_main`. Everything else on a `WorkspaceView` rides here for
 *  nothing, and `processes` rides here several times a second. */
const NOT_SHOWN = [
  'processes', 'stopped_processes', 'occupant', 'held_by', 'kind', 'branches', 'branch',
  'dirty_count',
];

/** What the pane was last built from — see `unchanged`. */
const drawn = { sig: null, name: 'changed-files' };

/** One changed file: what happened to it, and what git can be asked to do.
 *
 *  Its own function so `renderFiles` can key it — see `core.reconcile`. */
function fileRow(/** @type {import('../snapshot').WorkspaceView} */ w, /** @type {any} */ f, /** @type {any} */ sum) {
    const row = el('button', sum ? 'dfrow' : 'frow');
    if (sum) row.setAttribute('aria-current', String(f.path === diffState.path));
    const letter = (f.status || 'M')[0];
    /* The letter is the class too: `.fst.M`, `.fst.A`, `.fst.D`, `.fst.R` and
       `.fst.U` colour git's modified, added, deleted, renamed and untracked. Named
       here because `check-dead-css` looks for a class by its literal name, and
       `.fst.R` had only ever passed by the legend's MOD+Shift+R. */
    row.appendChild(el('span', 'fst ' + letter, letter));
    const n = el('span', 'fname');
    /* The directory dim and the name in the text colour, so a column of paths
       scans by name. The embedding mark leads the first part: the cell is
       right-to-left so a long path loses its *start*, and the mark keeps the
       characters in order across both parts. A whole untracked directory
       (`src/`) is all name. */
    const cut = f.path.lastIndexOf('/', f.path.length - 2);
    n.append(el('i', null, '\u202a' + f.path.slice(0, cut + 1)), f.path.slice(cut + 1));
    n.title = f.old_path ? `${f.old_path} → ${f.path}` : f.path;
    row.appendChild(n);
    const nums = el('span', 'dfnum');
    if (f.binary) {
      nums.textContent = 'bin';
    } else if (f.status === '?') {
      // Untracked: entirely new by definition, so a count would only ever say
      // "all of it".
      nums.appendChild(el('span', 'p', 'new'));
    } else {
      nums.appendChild(el('span', 'p', `+${f.added}`));
      nums.appendChild(document.createTextNode(' '));
      nums.appendChild(el('span', 'm', `\u2212${f.deleted}`));
    }
    row.appendChild(nums);
    row.onclick = () => {
      if (sum) {
        diffState.cursor = 0;
        diffState.context = 3;
        void loadFile(f.path);
      } else {
        void openDiff(f.path);
      }
    };
    /* The row's menu: the file on the forge, and what git can be asked to do with
       it. `open on <forge>` is greyed rather than hidden for the two rows the forge
       cannot serve, because a missing item reads as a missing feature:

         * `status: '?'` is untracked (`DiffFile::untracked`). Git has never seen
           it, so no ref has a blob for it and the link is a guaranteed 404.
         * a path ending in `/` is a whole untracked *directory*, collapsed by
           `--untracked-files=normal`. GitHub spells those `/tree/`, not `/blob/`.

       The daemon refuses the same two anyway (`api::open_file`), the way the fork
       guard lives on both sides. */
    row.oncontextmenu = (ev) => openMenu(ev, fileMenu(w, f));
  return row;
}

function renderFiles() {
  // The diff overlay is opened against a workspace and keeps describing it while
  // it is open, session or no session.
  // The tree is of a checkout's files, so with no session open it shows main's,
  // as the drawer does.
  const wsId = diffState.open ? diffState.ws
    : diffState.view === 'files' ? currentWorkspaceId() : activeWorkspaceId();
  const w = workspaceById(wsId);
  /* Rebuilt only when it would come out different: a file row is a button you
     hover and click, and a snapshot lands several times a second while an agent
     works.
     **This workspace, not the snapshot.** Passing the whole snapshot is the safe
     default and it is the wrong one here: with sixty worktrees and several agents
     running, an edit in any *other* worktree rewrote its changed-file list and
     rebuilt this pane, which describes one workspace and never reads another. So
     the inputs are named — `w` and the current session cover both branches below,
     and the `diffState` fields are the ones drawn here, `file` and its hunks
     being the overlay's rather than the list's.

     **`w` still carried seven fields this pane never shows**, and one of them —
     `processes` — moves on every line a managed process prints. `NOT_SHOWN` names
     them, the way the rail's `NOT_DRAWN` does, and `tools/check-drop-lists.mjs`
     holds the names to what the daemon still sends.

     **The session is in as the two things it is read for**, not whole: it is used
     once below, for `pending(s)` in the arm with no workspace, and its `state`
     moves every time the agent does. The id is there so switching sessions still
     redraws — `wsId` alone would not, for two sessions in one workspace. */
  const cur = currentSession();
  const mode = diffState.base;
  const fetched = mode !== 'upstream';
  const listSum = w && fetched && !diffState.open ? listedSummary(w) : null;
  const known = w ? changedOf(w) : [];
  const tree = diffState.view === 'files' && !diffState.open;
  // Only the changes view's tooltip reads them, so the tree does not fetch them.
  const commits = w && !tree ? commitsOf(w) : null;
  if (unchanged(drawn, [wsId, w, cur?.id ?? null, !!(cur && pending(cur)), snap.upstream_ref,
    diffState.open, diffState.path, diffState.summary, mode, listSum, known, commits, diffState.view,
    Tree.rev()], NOT_SHOWN)) return;
  if (w) renderDivergence(w);
  const panes = $('filepanes');

  $('filesname').textContent = diffState.open ? 'Changeset' : MODES[mode].title;
  $('filestitle').setAttribute('aria-pressed', String(!tree));
  $('filestree').setAttribute('aria-pressed', String(tree));
  // The changeset is what the open diff steps through, so it has no other view.
  $('filestree').hidden = diffState.open;

  if (!w) {
    const s = currentSession();
    reconcile(panes, [{
      key: 'nows',
      sig: paintSig([!!(s && pending(s))]),
      build: () => el('div', 'fempty', s && pending(s)
        ? 'Creating the worktree…'
        : 'No session open.'),
    }]);
    $('filesfoot').textContent = '';
    $('filescount').textContent = '';
    $('filestitle').title = 'Click to pick what this pane lists';
    return;
  }

  /* One list, one meaning: everything this workspace changed since it branched.
   *
   * Not `git status`, which is uncommitted work only — a session that commits
   * would empty its own pane. Not a diff against develop's tip either, which
   * would add every file a colleague landed meanwhile. The base is the
   * merge-base, so the list is what happened *here*.
   *
   * With the diff open the same question is asked of the diff's own summary,
   * which carries line counts per file and a cursor. */
  const sum = diffState.open ? diffState.summary : null;
  /* Pushed, the list is the fetched summary plus the snapshot's untracked rows: a
     diff never sees an untracked file, and an untracked file is not pushed. */
  const files = sum ? sum.files
    : listSum ? [...listSum.files, ...(known || []).filter((f) => f.status === '?')]
      .sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0))
    : fetched ? []
    : (known || []);
  const since = sum ? sum.base : listSum ? listSum.base : fetched ? null : w.changed_since;
  const counted = !!(sum || listSum);
  const total = counted ? files.length : (w.changed_total ?? files.length);
  /* The toggle's count, from the one list both views count. Nothing for none, so
     the toggle is one width in both. */
  $('filescount').textContent = total ? String(total) : '';

  if (tree) {
    $('filesfoot').textContent = Tree.render(panes, w.id, known || [], renderFiles);
    $('filestitle').title = 'Show the changes';
    return;
  }



  /* **Keyed on the path, and the container is kept**, because this pane draws up
     to five hundred buttons and a rebuild destroys the one under the pointer.
     `core.reconcile` carries the measurements. A row's signature is its own file
     record plus the two things its handlers read from outside it — which list it
     belongs to, and whether it is the row the open diff is showing. */
  /** @type {any[]} */
  const items = files.map((/** @type {any} */ f) => ({
    key: `f:${f.path}`,
    sig: paintSig([f, !!sum, sum ? f.path === diffState.path : false, w.id]),
    build: () => fileRow(w, f, sum),
  }));
  if (!files.length) {
    items.push({
      key: 'empty',
      sig: paintSig([!!sum, w.measured, w.is_main, mode, !!listSum, known === null]),
      build: () => (fetched && !sum
        ? (listSum ? el('div', 'fempty', mode === 'head' ? 'Nothing uncommitted.' : 'Nothing to push.') : counting())
        // An omitted list still on its way is not counted yet, not empty.
        : sum || (w.measured && known !== null)
          ? el('div', 'fempty',
            w.is_main ? 'Nothing changed in the main checkout.' : 'Nothing changed in this worktree yet.')
          : counting()),
    });
  }
  reconcile(panes, items);

  /* **"Nothing changed" and "not counted yet" are different sentences.** Every
     field this pane reads defaults to a value that looks like a real answer — no
     files, zero changed, no base — and the daemon's first sweep used to finish
     before the window opened, so the difference could not be seen. It runs in the
     background now, so an unmeasured worktree would render as a clean one. The
     daemon says which this is (`measured`); the loader is only ever the honest
     half of that.

     Not while the diff overlay is open: `sum` is its own fetched summary, which
     is measured by definition. */
  // "500 of 5,214" when the daemon capped the list, plain count otherwise. Said
  // rather than left to look complete: a truncation presented as the whole answer
  // is the one thing a changed-file pane must not do, and a wiped repository is
  // exactly when you are reading it.
  //
  // A count of something not yet counted is the same fault with a different
  // cause, so the footer says so instead of showing a confident nothing.
  const bits = [total > files.length
    ? `${files.length} of ${total.toLocaleString()} files`
    : `${files.length} file${files.length === 1 ? '' : 's'}`];
  const lines = sum ?? listSum;
  if (w.is_main) bits.push('worktrees excluded');
  /* The totals in the list's own green and red, as each row draws its counts. */
  const foot = $('filesfoot');
  if (counted || (!fetched && w.measured)) {
    foot.replaceChildren(bits[0] ?? '');
    if (lines) foot.append(' \u00b7 ', el('span', 'p', `+${lines.added}`), ' ', el('span', 'm', `\u2212${lines.deleted}`));
    for (const b of bits.slice(1)) foot.append(` \u00b7 ${b}`);
  } else foot.textContent = 'counting\u2026';
  /* **What the list is measured from, in words** (#47), now the tooltip's first
     line: `4 since develop`, never a bare sha. It was a second header line, and
     the Files view had nothing to put there, so switching moved the header. The
     count on the toggle is the files the list holds. */
  $('filestitle').title = [
    ...(since ? [MODES[mode].line(w)] : []),
    mode === 'head' ? `uncommitted work on top of ${since ? since.slice(0, 7) : 'HEAD'}`
      : mode === 'pushed' ? `what the next push would send, from ${since ? since.slice(0, 7) : '…'}`
      : `everything since this workspace branched from ${snap.upstream_ref} at ${since ? since.slice(0, 7) : '…'}`,
    ...(commits || []).slice(0, COMMITS_CAP).map((c) => `${c.sha} ${c.subject}`),
    // The daemon sends one past the cap when there are more, so a list of exactly
    // the cap is whole and gets no `…`.
    ...(commits && commits.length > COMMITS_CAP ? ['…'] : []),
    'click to pick what this pane lists',
  ].join('\n');
}

/** `n commits`, `1 commit`. */
const commitCount = (/** @type {number} */ n) => `${n} commit${n === 1 ? '' : 's'}`;

/** The leaf of the base ref, which is the name people say: `develop` of
 *  `upstream/develop`. The remote is in the tooltip. */
const baseLeaf = () => (snap.upstream_ref || 'the base').replace(/^[^/]+\//, '');

/** The pane's three questions, each a base `/api/diff` already answers. Every one
 *  ends at the working tree, which is what lets the diff they open keep its
 *  editing and its folds: a single commit's diff would not (#47).
 *
 *  `size` is what the menu says beside each, from the snapshot alone. The
 *  uncommitted count is not in a workspace's snapshot, so that one says nothing
 *  until it is picked, and then the footer counts it.
 *
 *  @type {Record<string, { title: string, menu: string, line: (w: import('../snapshot').WorkspaceView) => string,
 *                          size: (w: import('../snapshot').WorkspaceView) => string | null }>} */
const MODES = {
  upstream: {
    title: 'Changes', menu: 'Since branched',
    line: (w) => `${w.ahead} since ${baseLeaf()}`,
    size: (w) => commitCount(w.ahead),
  },
  pushed: {
    title: 'Unpushed', menu: 'Unpushed',
    line: (w) => `${w.unpushed} not pushed`,
    size: (w) => commitCount(w.unpushed),
  },
  head: {
    title: 'Uncommitted', menu: 'Uncommitted',
    line: () => 'against HEAD',
    size: () => null,
  },
};

/** Same cap as the daemon's `COMMITS_CAP`; it sends one more when there are more. */
const COMMITS_CAP = 50;

/** @template T
 *  @typedef {{ key: string | null, ws: string | null, val: T | null }} Fetched */

/** A fetch the changes pane keeps one answer of: asked again when `key` moves,
 *  with the previous answer left up while the newer one loads, so an edit does
 *  not blank the list. `null` until the first answer for `ws`, which names the
 *  workspace and the mode, so a switch never shows one mode's list under another.
 *
 *  @template T
 *  @param {Fetched<T>} slot
 *  @param {string} ws
 *  @param {string} key
 *  @param {string} url
 *  @param {(e: unknown) => T | null} fail what the slot holds when the fetch fails
 *  @returns {T | null} */
function fetched(slot, ws, key, url, fail) {
  if (slot.key !== key) {
    slot.key = key;
    if (slot.ws !== ws) slot.val = null;
    get(url).then((v) => {
      if (slot.key !== key) return;
      slot.ws = ws;
      slot.val = v;
      renderFiles();
    }, (e) => {
      if (slot.key === key) slot.val = fail(e);
    });
  }
  return slot.ws === ws ? slot.val : null;
}

/** @param {unknown} e */
const toastFail = (e) => { toast(reason(e), true); return null; };

/** The commits the header lists, fetched again when a count it depends on moves.
 *  Always `null` for `head`, which has none.
 *
 *  @type {Fetched<import('../repo').Commit[]>} */
const commitList = { key: null, ws: null, val: null };
function commitsOf(/** @type {import('../snapshot').WorkspaceView} */ w) {
  if (diffState.base === 'head') return null;
  const q = new URLSearchParams({ workspace: w.id, base: diffState.base });
  // Only the tooltip goes without on a failure: the list and the counts are the snapshot's.
  return fetched(commitList, `${w.id} ${diffState.base}`, paintSig([w.id, diffState.base, w.ahead, w.unpushed, w.changed_since]),
    `/api/diff/commits?${q}`, () => []);
}

// Kept short: the right header also carries the title and the refresh control,
// and a long label wraps it onto two lines.
/** The open diff, and everything a redraw needs to reproduce it.
 *
 *  Typed against the generated `DiffSummary` / `FileDiff` rather than a
 *  hand-written shape, for the reason `snapshot.d.ts` exists at all: these come
 *  off `/api/workspace/:id/diff`, and a renamed field on the Rust side should
 *  fail here rather than render as nothing.
 *
 *  @type {{ open: boolean, ws: string | null, base: string, view: 'changes' | 'files',
 *           summary: import('../repo').DiffSummary | null, path: string | null,
 *           file: import('../repo').FileDiff | null, split: boolean,
 *           cursor: number, pendingCursor: 'first' | 'last' | null, context: number,
 *           anchors?: HTMLElement[], loading?: boolean, lines: string[] | null,
 *           reveal: Map<number, { top: number, bottom: number }>, gone?: string | null }}
 */
const diffState = {
  open: false,
  /* Which workspace the open diff describes. Every fetch used to read the
   * current one at call time, so switching sessions left the loaded hunks
   * describing the old worktree while the next request quietly asked about the
   * new one. Pinned here, and re-pointed by `syncDiffToSession`. */
  ws: null,
  base: 'upstream',
  /* What the pane lists: the changes, or every file as a tree (`tree.js`). The
     open diff always lists its changeset, whichever this is. */
  view: 'changes',
  summary: null,     // { base, files, added, deleted }
  path: null,
  file: null,        // { path, hunks, binary }
  split: true,
  cursor: 0,         // index into the current file's change blocks
  /* Where to land the cursor once the next file's blocks are built, when a step
   * crossed a file boundary: 'first' arriving from below, 'last' from above.
   * Null on a normal load so the cursor is just clamped to what fits. */
  pendingCursor: null,
  context: 3,
  /* The file on disk, line by line, which is where an unchanged line a fold hides
     comes from: the right side is the working tree, so its lines are the
     unchanged ones too. Null when it cannot be read (deleted, binary), and then a
     fold falls back to asking git for more context everywhere. */
  lines: null,
  /** Per fold, how many lines are shown from its top and its bottom. Keyed on
   *  the hunk the fold sits above, and `hunks.length` for the one after the last. */
  reveal: new Map(),
};

/* **Three questions, and the header is a menu between them** (#47). `Changes` is
   everything since the workspace branched, which is what the pane has always
   listed. `Unpushed` is what the next push would send: a diff against
   `origin/<branch>`, uncommitted work and untracked files included.
   `Uncommitted` is the working tree against `HEAD`. It was a blind toggle
   between the first two, which nothing on screen said. The overlay follows the
   same mode, so a row opens the diff the list counted. Remembered per browser,
   like the pane sizes. */
const BASE_KEY = 'orch.changesBase';
try {
  const kept = localStorage.getItem(BASE_KEY);
  diffState.base = kept === 'pushed' || kept === 'head' ? kept : 'upstream';
} catch (err) { /* no storage: the pane opens on `Changes` */ }
const VIEW_KEY = 'orch.changesView';
try {
  if (localStorage.getItem(VIEW_KEY) === 'files') diffState.view = 'files';
} catch (err) { /* no storage: the pane opens on the changes */ }

/** Switch the pane between the changes and the file tree. A press on the view
 *  already showing is the base menu, which is what the title has always been. */
function setView(/** @type {'changes' | 'files'} */ view, /** @type {MouseEvent} */ ev) {
  if (view === 'changes' && (diffState.view === 'changes' || diffState.open)) {
    baseMenu(ev);
    return;
  }
  if (view === diffState.view) return;
  diffState.view = view;
  try {
    localStorage.setItem(VIEW_KEY, view);
  } catch (err) { /* private mode: it still switched for this session */ }
  renderFiles();
}

/** The list for the two modes the snapshot does not carry: fetched, and fetched
 *  again whenever this workspace's own measurements move. `unpushed` is in the
 *  key because a push changes nothing else the snapshot says about the tree.
 *
 *  @type {Fetched<import('../repo').DiffSummary>} */
const pushed = { key: null, ws: null, val: null };
function listedSummary(/** @type {import('../snapshot').WorkspaceView} */ w) {
  const q = new URLSearchParams({ workspace: w.id, base: diffState.base });
  return fetched(pushed, `${w.id} ${diffState.base}`, paintSig([w.id, diffState.base, w.changed, w.changed_since, w.unpushed, w.branch]),
    `/api/diff?${q}`, toastFail);
}

/** An idle workspace's changed files, which the snapshot leaves out to stay
 *  small (`changed_omitted`): fetched once, and again when its measurements move.
 *  `null` while the first answer is in the air.
 *
 *  @type {{ key: string | null, ws: string | null, files: any[] | null }} */
const omitted = { key: null, ws: null, files: null };
function changedOf(/** @type {import('../snapshot').WorkspaceView} */ w) {
  if (!w.changed_omitted) return w.changed || [];
  const key = paintSig([w.id, w.changed_total, w.changed_since, w.measured]);
  if (omitted.key !== key) {
    omitted.key = key;
    if (omitted.ws !== w.id) omitted.files = null;
    get(`/api/workspace/${encodeURIComponent(w.id)}/changed`).then((r) => {
      if (omitted.key !== key) return;
      omitted.ws = w.id;
      omitted.files = r.changed || [];
      renderFiles();
    }, (e) => {
      if (omitted.key === key) toast(reason(e), true);
    });
  }
  return omitted.ws === w.id ? omitted.files : null;
}

/** The header's menu: the three questions, each with its size, the one on screen
 *  ticked. */
function baseMenu(/** @type {MouseEvent} */ ev) {
  const w = workspaceById(diffState.ws || activeWorkspaceId());
  openMenu(ev, Object.entries(MODES).map(([key, m]) => /** @type {import('./core.js').MenuItem} */ ([
    (w && m.size(w)) ? `${m.menu} \u00b7 ${m.size(w)}` : m.menu,
    `pick${key === diffState.base ? ' on' : ''}`,
    () => void setBase(key),
  ])));
}

/** Point the pane at one of [`MODES`]. */
async function setBase(/** @type {string} */ mode) {
  if (mode === diffState.base) return;
  diffState.base = mode;
  try {
    localStorage.setItem(BASE_KEY, diffState.base);
  } catch (err) { /* private mode: it still switched for this session */ }
  pushed.key = null;
  if (!diffState.open) {
    renderFiles();
    return;
  }
  // The open file stays open when the other list still has it.
  await loadSummary();
  const files = diffState.summary?.files || [];
  const keep = files.find((f) => f.path === diffState.path)?.path ?? files[0]?.path;
  if (keep) {
    diffState.cursor = 0;
    await loadFile(keep);
  } else {
    diffState.file = null;
    diffState.path = null;
    renderDiff();
  }
}

function lineEl(/** @type {import('../repo').Row} */ row, /** @type {'old' | 'new'} */ side) {
  // side: 'old' | 'new'. In split view each pane shows only its own side.
  const empty = !row || (side === 'old' && row.kind === 'add') ||
                        (side === 'new' && row.kind === 'del');
  const div = el('div', 'ln' + (empty ? ' empty' : row.kind === 'add' ? ' add' : row.kind === 'del' ? ' del' : ''));
  /* **The number is drawn, not written.** `user-select:none` was already on the
     gutter and is not enough: WebKit includes an unselectable element's text when
     a selection *crosses* it, so dragging over a hunk and copying it brought the
     line numbers along into whatever you pasted it into. Generated content is not
     in the document at all, so no engine can take it. */
  const num = el('i');
  num.dataset.n = empty ? '' : String((side === 'old' ? row.old : row.new) ?? '');
  div.appendChild(num);
  const body = el('s');
  if (!empty) {
    // Word ranges arrive as byte offsets (from Rust); Prism works on the JS
    // string. Convert the ranges to character offsets so the two line up, then
    // merge. A blank line still needs a space so the row has height.
    const words = charRanges(row.text, row.words || []);
    const segs = lineSegments(row.text, words, langFor(diffState.path ?? ''));
    if (!segs.length) {
      body.textContent = row.text || ' ';
    } else {
      const wcls = row.kind === 'add' ? 'w-add' : 'w-del';
      for (const g of segs) {
        const t = row.text.slice(g.s, g.e);
        if (!g.cls && !g.word) { body.appendChild(document.createTextNode(t)); continue; }
        const cls = (g.cls ? 'tok-' + g.cls : '') + (g.word ? (g.cls ? ' ' : '') + wcls : '');
        body.appendChild(el('span', cls, t));
      }
    }
  }
  div.appendChild(body);
  return div;
}

/** Align a hunk's rows into side-by-side pairs.
 *
 *  The server emits deletions then additions; split view needs them abreast,
 *  padding the shorter run so the two panes stay in step. */
function pairRows(/** @type {import('../repo').Row[]} */ rows) {
  const out = [];
  let i = 0;
  while (i < rows.length) {
    if (rows[i].kind === 'context') {
      out.push([rows[i], rows[i]]);
      i += 1;
      continue;
    }
    const dels = [];
    const adds = [];
    while (i < rows.length && rows[i].kind === 'del') dels.push(rows[i++]);
    while (i < rows.length && rows[i].kind === 'add') adds.push(rows[i++]);
    const n = Math.max(dels.length, adds.length);
    for (let k = 0; k < n; k++) out.push([dels[k] ?? null, adds[k] ?? null]);
    // A row that is neither context nor a del/add run would loop forever.
    if (!dels.length && !adds.length) i += 1;
  }
  return out;
}

/** A rename's old path, or null. Only a rename: a copy's source still exists, so
 *  naming it to git would diff that file as well. */
function renamedFrom(/** @type {string | null} */ path) {
  const f = diffState.summary?.files.find((x) => x.path === path);
  return f?.status.startsWith('R') ? f.old_path : null;
}

function renderDiff() {
  const body = $('diffbody');
  body.replaceChildren();
  body.className = 'diff' + (diffState.split ? ' split' : '');
  const f = diffState.file;

  $('ovpath').replaceChildren();
  const from = renamedFrom(diffState.path);
  if (diffState.path) {
    /* A rename is named in the bar, not in the diff: a header row in the scrolling
       body scrolled away, and scrolled part way it read as a stray border. */
    if (from) $('ovpath').appendChild(el('span', 'ov-from', `${from} \u2192 `));
    const parts = diffState.path.split('/');
    // A path always has a last segment; `pop` cannot say so.
    const name = parts.pop() ?? '';
    $('ovpath').appendChild(el('span', null, parts.length ? parts.join('/') + '/' : ''));
    $('ovpath').appendChild(document.createTextNode(name));
  }
  $('ovmode').dataset.mode = diffState.split ? 'split' : 'unified';

  const note = (/** @type {string} */ t) => {
    body.appendChild(el('div', 'diffnote', t));
    $('ovcount').textContent = '';
    diffState.anchors = [];
  };
  if (diffState.loading) return note('reading the diff…');
  if (!f && diffState.gone === diffState.path) return note('This file is gone: it was removed after the list was drawn.');
  if (!f) return note('Select a file.');
  if (f.binary) return note('Binary file — not shown.');
  if (!f.hunks.length) {
    // A new file with nothing in it has no lines to show, which is not the same
    // as having no changes: it is the change.
    const listed = diffState.summary?.files.find((x) => x.path === diffState.path);
    return note(listed?.status === '?' ? 'A new, empty file.'
      : from ? `Renamed from ${from}, no content changes.`
      : 'No textual changes against this base.');
  }

  const anchors = [];
  let block = -1;

  // Every row is three grid cells in split view and one in unified, so a fold
  // spanning the full width interleaves naturally between hunks.
  const push3 = (/** @type {HTMLElement} */ a, /** @type {HTMLElement} */ b,
    /** @type {HTMLElement} */ g = el('div', 'gutter')) => {
    body.appendChild(a);
    body.appendChild(g);
    body.appendChild(b);
  };

  const drawContext = (/** @type {import('../repo').Row} */ r) => {
    if (diffState.split) push3(lineEl(r, 'old'), lineEl(r, 'new'));
    else body.appendChild(lineEl(r, 'new'));
  };
  /** The unchanged lines between two hunks, as many as the fold shows. */
  const drawGap = (/** @type {number} */ idx, /** @type {number} */ oldStart,
    /** @type {number} */ newStart, /** @type {number} */ count) => {
    if (count <= 0) return;
    const lines = diffState.lines;
    if (!lines) {
      // Nothing to draw the lines from, so the old way: ask git for more context.
      if (idx >= f.hunks.length) return;
      const b = el('div', 'fold', `⋯ ${count} unchanged lines, click to expand`);
      b.onclick = () => {
        diffState.context = Math.min(diffState.context + Math.max(count, 20), 10000);
        void loadFile(diffState.path ?? '');
      };
      body.appendChild(b);
      return;
    }
    const seen = diffState.reveal.get(idx) ?? { top: 0, bottom: 0 };
    const row = (/** @type {number} */ k) => /** @type {import('../repo').Row} */ ({
      kind: 'context', old: oldStart + k, new: newStart + k,
      text: (lines[newStart + k - 1] ?? '').replace(/\r$/, ''),
    });
    if (seen.top + seen.bottom >= count) {
      for (let k = 0; k < count; k++) drawContext(row(k));
      return;
    }
    for (let k = 0; k < seen.top; k++) drawContext(row(k));
    const hidden = count - seen.top - seen.bottom;
    const show = (/** @type {number} */ top, /** @type {number} */ bottom) => {
      diffState.reveal.set(idx, { top: seen.top + top, bottom: seen.bottom + bottom });
      renderDiff();
    };
    const bar = el('div', 'fold');
    bar.title = 'Click to show them all';
    bar.onclick = () => show(count, 0);
    /* Twenty at a time from either end: the line above a change is usually the
       one you wanted, and opening a 400-line fold to read it loses your place. */
    const step = (/** @type {string} */ label, /** @type {string} */ tip, /** @type {() => void} */ go) => {
      const b = el('button', 'fold-step', label);
      b.title = tip;
      b.onclick = (e) => { e.stopPropagation(); go(); };
      return b;
    };
    if (idx > 0) bar.appendChild(step('↓ 20', 'Show 20 more below the change above', () => show(20, 0)));
    if (idx < f.hunks.length) bar.appendChild(step('↑ 20', 'Show 20 more above the change below', () => show(0, 20)));
    bar.appendChild(el('span', null, `⋯ ${hidden} unchanged line${hidden === 1 ? '' : 's'}`));
    body.appendChild(bar);
    for (let k = count - seen.bottom; k < count; k++) drawContext(row(k));
  };
  /** The last line a hunk reaches on each side, so the next gap starts after it. */
  const ends = (/** @type {import('../repo').Hunk} */ h) => {
    let o = h.old_start - 1;
    let n = h.new_start - 1;
    for (const r of h.rows) { o = r.old ?? o; n = r.new ?? n; }
    return { o, n };
  };

  for (const [hi, h] of f.hunks.entries()) {
    {
      const prev = hi > 0 ? ends(f.hunks[hi - 1]) : { o: 0, n: 0 };
      drawGap(hi, prev.o + 1, prev.n + 1, h.gap_before);
    }

    // The hunk's section heading — git's text after the second @@, the function
    // or class the change sits in — pinned so it stays in view while you scroll a
    // long hunk, the way the review overlay's hunk header already does. Empty for
    // a top-of-file hunk, so it is only drawn when it says something. And only
    // under a fold: with nothing hidden above, the line it names is on screen, and
    // with every fold opened the heading sat in the middle of continuous code.
    // Nor when it repeats the hunk above, whose heading is still pinned.
    const sectionOf = (/** @type {import('../repo').Hunk} */ x) => (x.header && x.header.split('@@')[2] || '').trim();
    const section = sectionOf(h);
    const seen = diffState.reveal.get(hi) ?? { top: 0, bottom: 0 };
    const folded = h.gap_before > seen.top + seen.bottom;
    if (section && folded && (hi === 0 || section !== sectionOf(f.hunks[hi - 1]))) {
      body.appendChild(el('div', 'diffhh', section));
    }

    if (diffState.split) {
      let splitInBlock = false;
      const blocks = changeBlocks(h);
      let inHunk = -1;
      for (const [o, n] of pairRows(h.rows)) {
        const lo = lineEl(o, 'old');
        const ro = lineEl(n, 'new');
        const changed = o?.kind === 'del' || n?.kind === 'add';
        const gutter = el('div', 'gutter');
        if (changed) {
          /* The right cell, because the right pane is the file as it is now and
             the one you edit, so that is where a change is. A block that only
             deletes still marks its filler row there, where the lines went. */
          if (!splitInBlock) {
            block += 1; anchors.push(ro); splitInBlock = true; inHunk += 1;
            const b = blocks[inHunk];
            if (b) gutter.appendChild(revertArrow(b));
          }
          lo.dataset.blk = ro.dataset.blk = String(block);
        } else {
          splitInBlock = false;
        }
        push3(lo, ro, gutter);
      }
    } else {
      let inBlock = false;
      for (const r of h.rows) {
        const e = lineEl(r, r.kind === 'del' ? 'old' : 'new');
        if (r.kind !== 'context') {
          if (!inBlock) { block += 1; anchors.push(e); inBlock = true; }
          e.dataset.blk = String(block);
        } else {
          inBlock = false;
        }
        body.appendChild(e);
      }
    }
  }

  {
    // After the last change, down to the end of the file.
    const tail = ends(f.hunks[f.hunks.length - 1]);
    drawGap(f.hunks.length, tail.o + 1, tail.n + 1, (diffState.lines?.length ?? 0) - tail.n);
  }

  diffState.anchors = anchors;
  const last = Math.max(anchors.length - 1, 0);
  diffState.cursor = diffState.pendingCursor === 'last' ? last
    : diffState.pendingCursor === 'first' ? 0
      : Math.min(diffState.cursor, last);
  diffState.pendingCursor = null;
  markCursor();
}

function revertArrow(/** @type {import('./revert.js').Block} */ b) {
  const n = Math.max(b.dels.length, b.adds.length);
  const btn = el('button', 'revert', '→');
  btn.title = b.adds.length
    ? `Put these ${n} line${n === 1 ? '' : 's'} back as they were on the left`
    : `Put the ${n} removed line${n === 1 ? '' : 's'} back`;
  btn.onclick = (e) => { e.stopPropagation(); void revertBlock(b); };
  return btn;
}

async function revertBlock(/** @type {import('./revert.js').Block} */ b) {
  const ws = diffState.ws || activeWorkspaceId();
  const path = diffState.path;
  if (!ws || !path) return;
  const q = new URLSearchParams({ workspace: ws, path });
  /** @type {(content: string, version: string) => Promise<string | null>} */
  const write = async (content, version) => {
    try {
      const out = await call('/api/file', { workspace: ws, path, content, version });
      if (out.result === 'conflict') {
        toast('refused: the file changed on disk just now. Look again and retry.', true);
        return null;
      }
      return out.version;
    } catch (e) {
      toast(reason(e), true);
      return null;
    }
  };
  const reload = async () => { await loadSummary(); await loadFile(path); };
  let live;
  try {
    live = await get(`/api/file?${q}`);
  } catch (e) {
    return toast(reason(e), true);
  }
  const next = revertedText(live.content, b);
  if (next === null) {
    toast('refused: those lines changed since the diff was drawn. Look again and retry.', true);
    return reload();
  }
  const version = await write(next, live.version);
  if (version === null) return reload();
  const n = Math.max(b.dels.length, b.adds.length);
  /* An undo rather than a confirm box: a block that was never committed has no
     copy anywhere else, but a dialog per arrow is the cost of the rare mistake
     paid on every click. The undo is a write of its own, version-checked too. */
  toast(`reverted ${n} line${n === 1 ? '' : 's'}`, false, {
    label: 'Undo',
    run: async () => {
      if (await write(live.content, version) !== null) toast('undone');
      await reload();
    },
  });
  await reload();
}

function markCursor() {
  for (const e of $('diffbody').querySelectorAll('.ln.cur')) e.classList.remove('cur');
  const anchors = diffState.anchors || [];
  const a = anchors[diffState.cursor];
  if (!a) return;
  a.classList.add('cur');
  a.scrollIntoView({ block: 'center', behavior: 'smooth' });
  // Within-file position, plus which file of the changeset when there is more
  // than one — the stepper walks the whole PR, so "3 of 7" alone would not say
  // where in it you are.
  const files = diffState.summary?.files || [];
  const fi = files.findIndex((f) => f.path === diffState.path);
  const where = files.length > 1 && fi >= 0 ? ` · file ${fi + 1} of ${files.length}` : '';
  $('ovcount').textContent = `change ${diffState.cursor + 1} of ${anchors.length}${where}`;
}

/** Scroll the file as it is now to `line`, opening the folds when it is inside
 *  one. The number is the right side's, the file on disk, which is the one an
 *  agent, a stack trace and the editor all count in. */
function goTo(/** @type {number} */ line) {
  const find = () => /** @type {HTMLElement | null} */ ($('diffbody').querySelector(
    diffState.split ? `.gutter + .ln > i[data-n="${line}"]` : `.ln:not(.del) > i[data-n="${line}"]`))
    ?.parentElement ?? null;
  let row = find();
  if (!row && diffState.lines && diffState.file) {
    for (let i = 0; i <= diffState.file.hunks.length; i++) {
      diffState.reveal.set(i, { top: Number.MAX_SAFE_INTEGER, bottom: 0 });
    }
    renderDiff();
    row = find();
  }
  if (!row) return false;
  for (const e of $('diffbody').querySelectorAll('.ln.goto')) e.classList.remove('goto');
  row.classList.add('goto');
  row.scrollIntoView({ block: 'center' });
  return true;
}

/** Walk to the next/previous change block, carrying on into the next file in the
 *  changeset's order rather than wrapping inside the current one. Files with no
 *  change blocks (binary, or nothing textual) are hopped over, and the whole
 *  changeset wraps end to end so the stepper never dead-ends. */
async function stepChange(/** @type {number} */ delta) {
  const n = (diffState.anchors || []).length;
  const next = diffState.cursor + delta;
  if (n && next >= 0 && next < n) {
    diffState.cursor = next;
    markCursor();
    return;
  }

  const files = diffState.summary?.files || [];
  if (files.length < 2) {
    // Nowhere else to go: keep the old wrap so a single file still cycles.
    if (n) { diffState.cursor = (next + n) % n; markCursor(); }
    return;
  }
  let idx = files.findIndex((f) => f.path === diffState.path);
  if (idx < 0) idx = 0;
  // At most one lap; if every other file is blank we land back where we started.
  for (let hop = 0; hop < files.length; hop++) {
    idx = (idx + delta + files.length) % files.length;
    diffState.pendingCursor = delta > 0 ? 'first' : 'last';
    await loadFile(files[idx].path);
    if ((diffState.anchors || []).length) return;
  }
}

async function loadSummary() {
  const ws = diffState.ws || activeWorkspaceId();
  if (!ws) return;
  const q = new URLSearchParams({ workspace: ws, base: diffState.base });
  const pr = prForWorkspace(ws);
  if (pr && pr.base_ref) q.set('pr_base', pr.base_ref);
  try {
    diffState.summary = await get(`/api/diff?${q}`);
  } catch (e) {
    diffState.summary = null;
    toast(reason(e), true);
  }
  renderFiles();
}

/** Whether the workspace still has `path`, asked of the list git gives now. */
async function fileExists(/** @type {string} */ ws, /** @type {string} */ path) {
  try {
    await get(`/api/file?${new URLSearchParams({ workspace: ws, path })}`);
    return true;
  } catch {
    return false;
  }
}

/** A file's lines, without the empty one after a final newline. */
function linesOf(/** @type {string | undefined} */ content) {
  if (content == null) return null;
  if (content === '') return [];
  return (content.endsWith('\n') ? content.slice(0, -1) : content).split('\n');
}

async function loadFile(/** @type {string} */ path) {
  const ws = diffState.ws || activeWorkspaceId();
  if (!ws) return;
  if (Editor.isOpen() && path !== Editor.state.path && !await Editor.close()) return;
  diffState.path = path;
  const q = new URLSearchParams({
    workspace: ws, base: diffState.base, path, context: String(diffState.context),
  });
  const from = renamedFrom(path);
  if (from) q.set('old_path', from);
  const pr = prForWorkspace(ws);
  if (pr && pr.base_ref) q.set('pr_base', pr.base_ref);
  // Show "reading the diff…" only if the fetch outlasts a couple of frames, so a
  // fast local diff never flashes it and a slow one stops reading as "did nothing"
  // by leaving the previous file's hunks on screen.
  const slow = setTimeout(() => { diffState.loading = true; renderDiff(); }, 150);
  diffState.reveal = new Map();
  try {
    let live;
    [diffState.file, live] = await Promise.all([
      get(`/api/diff/file?${q}`),
      get(`/api/file?${new URLSearchParams({ workspace: ws, path })}`).catch(() => null),
    ]);
    diffState.lines = linesOf(live?.content);
  } catch (e) {
    diffState.file = null;
    diffState.lines = null;
    /* A short-lived file (a tool's temp or lock file) can be listed and gone by
       the click. Said in the pane, where you are looking, rather than as git's
       refusal in a toast. */
    if (!(await fileExists(ws, path))) diffState.gone = path;
    else toast(reason(e), true);
  }
  clearTimeout(slow);
  diffState.loading = false;
  renderDiff();
  renderFiles();
}

async function openDiff(/** @type {string} */ path) {
  // No falling back to `currentWorkspaceId`, which answers main when nothing is
  // selected. `activeWorkspaceId` already decided what the file pane does with a
  // finished session — nothing — and this fallback walked around that decision:
  // pressing the key with an archived session selected, or none at all, filled
  // the right pane with main's tree while the rail and the centre pane showed
  // there was nothing open.
  const ws = activeWorkspaceId();
  if (!ws) {
    toast('no session open');
    return;
  }
  diffState.open = true;
  diffState.ws = ws;
  diffState.context = 3;
  borrowFocus('diff');
  $('overlay').classList.add('on');
  await loadSummary();
  const files = diffState.summary?.files ?? [];
  /* A collapsed untracked folder (`newdir/`) is a row in the pane but not a file
     git can diff, so it opens its first file, which the summary now lists. */
  const first = (path?.endsWith('/') ? files.find((f) => f.path.startsWith(path))?.path : path)
    || files[0]?.path;
  if (first) {
    diffState.cursor = 0;
    await loadFile(first);
  } else {
    renderDiff();
  }
}

async function closeDiff() {
  if (Editor.isOpen() && !await Editor.close()) return;
  diffState.open = false;
  diffState.ws = null;
  diffState.file = null;
  diffState.path = null;
  $('overlay').classList.remove('on');
  returnFocus('diff', $('overlay'));
  renderFiles();
}




// ---------------------------------------------------------------------------
// Editable right pane (§5, step 9)
// ---------------------------------------------------------------------------

/* The buffer itself is `editor.js` now — the search viewer wanted the same one,
   and it could not have had it while the load read `diffState` directly. What is
   left here is the half that really is the diff's: which base revision the
   read-only left pane shows, and re-diffing after a write. */

function openEditor() {
  if (!diffState.path || !diffState.file || diffState.file.binary) {
    return toast('nothing editable here', true);
  }
  const ws = diffState.ws || activeWorkspaceId() || '';
  return Editor.open({
    mount: $('diffbody'),
    mountClass: 'diff split editing',
    workspace: ws,
    path: diffState.path,
    base: diffState.base,
    basePath: renamedFrom(diffState.path),
    prBase: prForWorkspace(ws)?.base_ref ?? null,
    save: $('ovsave'),
    edit: $('ovedit'),
    onClosed: renderDiff,
    onSaved: async () => {
      // Re-diff so the changeset reflects the write.
      await loadSummary();
      const q = new URLSearchParams({
        workspace: ws,
        base: diffState.base,
        path: diffState.path ?? '',
        context: String(diffState.context),
      });
      const from = renamedFrom(diffState.path);
      if (from) q.set('old_path', from);
      const pr = prForWorkspace(ws);
      if (pr?.base_ref) q.set('pr_base', pr.base_ref);
      try {
        let live;
        [diffState.file, live] = await Promise.all([
          get(`/api/diff/file?${q}`),
          get(`/api/file?${new URLSearchParams({ workspace: ws, path: diffState.path ?? '' })}`)
            .catch(() => null),
        ]);
        // The folds draw from these, so they follow the write too.
        diffState.lines = linesOf(live?.content);
        diffState.reveal = new Map();
      } catch (e) {
        /* the editor is still the source of truth on screen */
      }
    },
  });
}

export { goTo, baseMenu, setView, diffState as state, openDiff as open, closeDiff as close, renderDiff as render, stepChange as step, loadFile, renderFiles, openEditor };
