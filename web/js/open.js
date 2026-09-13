// Opening a project: the recents, the folder dialog, and the review of what a
// checkout is about to be configured as.
//
// **This was a second application.** `firstrun.rs` ran its own axum server, its
// own router and guard, its own HTML page with a copied palette, and its own
// titlebar — which had to learn the macOS window-drag rule a second time, four
// days after the board learned it, because the two pages did not share a line.
// Meanwhile the board already had the same journey in the rail: `+ open project`
// lists the recents and raises the same dialog. Two journeys, and only one of them
// ever ran the review step, so a base branch or a dev process detected for the
// *second* checkout you opened was detected for nobody.
//
// So it is one screen now, and it is this one. Two panels over one overlay: the
// welcome (a host with no checkouts open) and the review (a folder you just chose,
// before its daemon starts). A recent skips the review deliberately — it is a
// checkout you have opened before, whose config is already written and is the
// daemon's to read, not this screen's to re-answer.

import { $, CHECKOUTS, callHost, chooseBox, el, ctl, getHost, reason, toast } from './core.js';

/** The overlay, built once on first use.
 *
 *  Built here rather than written into `index.html` because nothing else in the
 *  page needs to name these ids, and a screen most launches never show is a screen
 *  most launches should not pay to parse. `$` throws on a missing id by design, so
 *  the root is held rather than looked up.
 *
 *  @type {HTMLElement | null}
 */
let root = null;

/** The path the review panel is about, or `null` when the review is not up.
 *  @type {string | null} */
let reviewing = null;

/** One add at a time. The host refuses a second one anyway; this keeps the screen
 *  from sending it. */
let busy = false;

export const isOpen = () => root !== null && !root.hidden;

/** Close the overlay, unless there is nothing to go back to.
 *
 *  **A host with no checkouts cannot be dismissed.** The board behind it has no
 *  rail, no panes and nothing to select, so an `Esc` that hid this would leave an
 *  empty window with no way back. Returns whether it closed, so the key handler
 *  can fall through to the next thing.
 */
export function close() {
  if (!root || root.hidden) return false;
  if (!anyCheckoutOpen()) return false;
  root.hidden = true;
  return true;
}

/** Whether the host has a checkout to go back to.
 *
 *  `CHECKOUTS` is a live binding, so this reads the list as it is now rather than
 *  as it was when the screen opened — and adding one is the whole point of the
 *  screen. */
const anyCheckoutOpen = () => CHECKOUTS.length > 0;

/** The welcome panel: what a host with no checkouts open shows.
 *
 *  Not only first run. Closing the last checkout lands here too, which is what
 *  `Host::close_checkout` means by staying symmetric down to the last one.
 */
export function showWelcome() {
  build();
  toPanel('welcome');
  void loadRecent();
}

/** Review a folder, then open it. The way in for the dialog and the typed path.
 *
 *  @param {string} path
 */
export async function reviewAndAdd(path) {
  build();
  setMessage('checking…');
  toPanel('review');
  /** @type {import('../serve').Detected} */
  let found;
  try {
    found = await callHost('/api/host/detect', { path });
  } catch (e) {
    setMessage(reason(e), true);
    toPanel('welcome');
    return;
  }
  reviewing = found.path;
  fillReview(found);
  setMessage('');
}

// ---------------------------------------------------------------------------
// The overlay
// ---------------------------------------------------------------------------

function build() {
  if (root) {
    root.hidden = false;
    return;
  }
  root = el('div', 'openco');
  root.append(welcomePanel(), reviewPanel());
  document.body.appendChild(root);
}

/** @param {'welcome' | 'review'} which */
function toPanel(which) {
  build();
  $('opwelcome').hidden = which !== 'welcome';
  $('opreview').hidden = which !== 'review';
}

function welcomePanel() {
  const panel = el('div', 'oppanel');
  panel.id = 'opwelcome';
  const brand = el('div', 'opbrand');
  brand.append(el('span', 'opdot'), el('span', 'opname', 'orchd'));
  panel.append(brand, el('h1', 'ophead', 'Open a project to orchestrate'));
  panel.append(el('p', 'opsub',
    'A project is a git checkout. orchd runs one daemon per checkout, cuts worktrees '
    + 'for its sessions, and watches its pull requests.'));

  const browse = el('button', 'opbtn primary opbrowse', 'Choose a folder…');
  browse.onclick = () => void browseForProject();
  panel.append(browse);

  panel.append(el('div', 'opor', 'or type a path'));
  const row = el('div', 'oprow');
  const input = el('input', 'opfield');
  input.id = 'oppath';
  input.placeholder = '~/code/my-repo';
  input.spellcheck = false;
  input.autocomplete = 'off';
  const go = el('button', 'opbtn ghost opgo', 'Open');
  go.id = 'opgo';
  go.disabled = true;
  input.oninput = () => void checkPath();
  input.onkeydown = (e) => {
    if (e.key === 'Enter' && !go.disabled) void reviewAndAdd(input.value.trim());
  };
  go.onclick = () => void reviewAndAdd(input.value.trim());
  row.append(input, go);
  panel.append(row);

  const msg = el('div', 'opmsg');
  msg.id = 'opmsg';
  panel.append(msg);

  const recent = el('div', 'oprecent');
  recent.id = 'oprecent';
  panel.append(el('div', 'opsechead', 'Recent'), recent);
  return panel;
}

function reviewPanel() {
  const panel = el('div', 'oppanel');
  panel.id = 'opreview';
  panel.hidden = true;

  const back = el('button', 'oplink', '‹ Choose a different folder');
  back.onclick = () => toPanel('welcome');
  panel.append(back);

  const head = el('h1', 'ophead', 'Set up project');
  head.id = 'opname';
  const path = el('div', 'oppath');
  path.id = 'oprpath';
  panel.append(head, path);

  const form = el('div', 'opform');
  form.append(field('Base branch', 'worktrees branch from here', select('opbase')));
  form.append(field('GitHub repo', 'whose pull requests orchd watches', text('oprepo', 'not on GitHub')));
  const env = el('div', 'opseg');
  env.id = 'openv';
  for (const [value, label] of [['mise', 'mise'], ['direnv', 'direnv'], ['none', 'none']]) {
    const b = el('button', 'opsegb', label);
    b.dataset.v = value;
    b.onclick = () => {
      for (const other of env.children) other.classList.toggle('sel', other === b);
    };
    env.append(b);
  }
  const envNote = el('span', 'opnote');
  envNote.id = 'openvnote';
  const envCell = el('div', 'opcell');
  envCell.append(env, envNote);
  form.append(field('Agent environment', "where a session's variables come from", envCell));
  const wt = el('div', 'opfield ro');
  wt.id = 'opwt';
  form.append(field('Worktrees', 'where sessions are cut', wt));
  panel.append(form);

  const procs = el('div', 'opprocs');
  procs.id = 'opprocs';
  panel.append(procs);

  const actions = el('div', 'opactions');
  const cancel = el('button', 'opbtn ghost', 'Back');
  cancel.onclick = () => toPanel('welcome');
  const open = el('button', 'opbtn primary', 'Open project');
  open.id = 'opdone';
  open.onclick = () => void openReviewed();
  actions.append(cancel, open);
  panel.append(actions);
  return panel;
}

/** One labelled row of the review form.
 *  @param {string} label
 *  @param {string} sub
 *  @param {HTMLElement} control
 */
function field(label, sub, control) {
  const row = el('div', 'opfrow');
  const lab = el('div', 'oplab');
  lab.append(el('span', null, label), el('span', 'opsub2', sub));
  row.append(lab, control);
  return row;
}

/** @param {string} id */
function select(id) {
  const s = el('select', 'opfield');
  s.id = id;
  return s;
}

/** @param {string} id @param {string} placeholder */
function text(id, placeholder) {
  const t = el('input', 'opfield');
  t.id = id;
  t.placeholder = placeholder;
  t.spellcheck = false;
  t.autocomplete = 'off';
  return t;
}

/** @param {string} message @param {boolean} [bad] */
function setMessage(message, bad) {
  const box = $('opmsg');
  box.textContent = message;
  box.classList.toggle('bad', Boolean(bad));
}

// ---------------------------------------------------------------------------
// The welcome panel's work
// ---------------------------------------------------------------------------

/** The typed path, checked per keystroke.
 *
 *  Sequenced, because a slow answer for a path you have since typed past would
 *  otherwise overwrite the answer for the one on screen.
 */
let seq = 0;

async function checkPath() {
  const mine = ++seq;
  const input = ctl('oppath');
  const path = String(input.value).trim();
  ctl('opgo').disabled = true;
  input.classList.remove('bad');
  if (!path) {
    setMessage('');
    return;
  }
  try {
    const answer = await callHost('/api/host/validate', { path });
    if (mine !== seq) return;
    ctl('opgo').disabled = false;
    setMessage(`✓ ${answer.name}`);
  } catch (e) {
    if (mine !== seq) return;
    input.classList.add('bad');
    setMessage(reason(e), true);
  }
}

async function browseForProject() {
  setMessage('');
  try {
    const { path } = await callHost('/api/host/pick');
    // A cancelled dialog is an answer, not a failure.
    if (path) await reviewAndAdd(path);
  } catch (e) {
    setMessage(reason(e), true);
  }
}

/** The recents the host knows, minus the ones already open. */
async function loadRecent() {
  const box = $('oprecent');
  /** @type {import('../serve').RecentProject[]} */
  let list = [];
  try {
    ({ recent: list } = await getHost('/api/host/recent'));
  } catch (e) {
    // The list is a convenience; the folder dialog is the way in without it.
    toast(reason(e), true);
  }
  box.replaceChildren();
  if (!list.length) {
    box.append(el('div', 'opempty', 'No projects yet — choose a folder to open your first.'));
    return;
  }
  for (const r of list) {
    const row = el('button', 'oprec');
    row.append(el('span', 'oprecname', r.name), el('span', 'oprecpath', r.path, r.path),
      el('span', 'oprecwhen', ago(Number(r.last_opened_ms))));
    // No review: a checkout you have opened before has its config written already,
    // and re-answering it is not what pressing a recent means.
    row.onclick = () => void add(r.path);
    box.append(row);
  }
}

/** "2 h ago", from a millisecond stamp. Coarse on purpose: the list is a glance.
 *  @param {number} ms */
function ago(ms) {
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 90) return 'just now';
  const m = s / 60;
  if (m < 90) return `${Math.round(m)} min ago`;
  const h = m / 60;
  if (h < 36) return `${Math.round(h)} h ago`;
  return `${Math.round(h / 24)} days ago`;
}

// ---------------------------------------------------------------------------
// The review panel's work
// ---------------------------------------------------------------------------

/** @param {import('../serve').Detected} d */
function fillReview(d) {
  $('opname').textContent = `Set up ${d.name}`;
  $('oprpath').textContent = d.path;

  const base = ctl('opbase');
  base.replaceChildren();
  const options = d.base_branches.length ? d.base_branches.slice() : [d.base_branch];
  if (!options.includes(d.base_branch)) options.unshift(d.base_branch);
  for (const b of options) {
    const o = el('option', null, b);
    o.value = b;
    o.selected = b === d.base_branch;
    base.append(o);
  }
  ctl('oprepo').value = d.repo ?? '';
  for (const b of $('openv').children) {
    if (b instanceof HTMLElement) b.classList.toggle('sel', b.dataset.v === d.env_source);
  }
  $('openvnote').textContent = d.env_source === 'mise' ? 'found mise.toml'
    : d.env_source === 'direnv' ? 'found .envrc'
      : "nothing detected — sessions get the daemon's environment";
  $('opwt').textContent = d.worktrees;

  const procs = $('opprocs');
  procs.replaceChildren();
  if (!d.processes.length) return;
  procs.append(el('div', 'opsechead', 'Processes found in this repo — optional'));
  procs.append(el('p', 'opsub',
    'Ticked, orchd starts and manages these beside your sessions — you can stop, '
    + 'restart and watch their output in the Processes drawer. Left unticked, they are '
    + 'not touched.'));
  for (const p of d.processes) {
    const row = el('button', 'opprow');
    const box = el('span', 'opbox');
    row.append(box, el('span', 'opcmd', p.label), el('span', 'opsrc', p.source));
    row.dataset.name = p.name;
    row.dataset.command = JSON.stringify(p.command);
    row.onclick = () => {
      const on = row.classList.toggle('on');
      box.classList.toggle('on', on);
    };
    procs.append(row);
  }
}

/** What the review answered, in the shape `firstrun::Overrides` takes. */
function reviewed() {
  const env = [...$('openv').children].find((b) => b.classList.contains('sel'));
  const processes = [...$('opprocs').querySelectorAll('.opprow.on')].map((row) => ({
    name: row instanceof HTMLElement ? (row.dataset.name ?? '') : '',
    command: row instanceof HTMLElement ? JSON.parse(row.dataset.command ?? '[]') : [],
  }));
  return {
    base_branch: String(ctl('opbase').value) || null,
    repo: String(ctl('oprepo').value).trim() || null,
    env_source: env instanceof HTMLElement ? (env.dataset.v ?? null) : null,
    /* No tracker. It is three fields (`mcp_server`, `host`, `token_env`) and no
       dropdown can spell a per-site host — the settings pane dropped its control
       for that reason and the page this replaces kept one that did nothing at all:
       `Overrides` never had the field, so serde dropped the value in silence from
       the day it was drawn. */
    processes,
  };
}

async function openReviewed() {
  if (!reviewing) return;
  await add(reviewing, reviewed());
}

/** Ask the host to open a checkout, with what the review answered if it ran.
 *
 *  The resume question is the host's, and it is asked here the same way the rail
 *  asks it — see `rail.addCheckout`, which is the other caller of this route.
 *
 *  @param {string} path
 *  @param {ReturnType<typeof reviewed>} [settings]
 *  @param {boolean} [resume]
 */
async function add(path, settings, resume) {
  if (busy) return;
  busy = true;
  try {
    const body = { path, ...(settings ? { settings: { path, ...settings } } : {}) };
    const { result } = await callHost('/api/host/checkout',
      resume === undefined ? body : { ...body, resume });
    if (result.added === 'ask') {
      busy = false;
      const n = result.sessions;
      const yes = await chooseBox(
        `Resume ${n} conversation${n === 1 ? '' : 's'} in ${result.path}?\n\n`
        + 'They were live when this checkout was last closed. Resuming reopens each '
        + 'one at its prompt; it re-runs nothing.',
        { ok: 'Resume', other: 'Start empty' });
      if (yes !== null) await add(path, settings, yes);
      return;
    }
    toast(`opened ${result.checkout.name}`);
    if (root) root.hidden = true;
  } catch (e) {
    setMessage(reason(e), true);
    toPanel(reviewing ? 'review' : 'welcome');
  } finally {
    busy = false;
  }
}
