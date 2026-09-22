// The settings panel. The zoom control it offers lives in core, because the
// terminals read the scale too.

import { ctl, $, WHEEL, ZOOM, borrowFocus, call, callHost, caret, setUiPx, uiPx, UI_PX_MAX, UI_PX_MIN, closeLegend, el, get, MOD_LABEL, reason, returnFocus, saveWheel, saveZoom, setWheel, setZoom, snap, wheelScale } from './core.js';
import { currentPreset, detectedFonts, FONTS, fontStack, PRESETS, resetTheme, SEE_THROUGH, setTheme, SIZE_MAX, SIZE_MIN, theme, validFontName } from './theme.js';
/* The arithmetic, for reading a typed hex back. A leaf with no imports of its own,
   so the module graph stays the DAG `dependency-cruiser` insists on — and the same
   parser `loadTheme` uses, so the pane and the store cannot disagree about what
   counts as a colour. */
import * as Palette from './palette.js';

const settingsOpen = () => !$('settings').hidden;

/** The three text boxes that stand for an argv, and the config key each carries. */
const ARGV_FIELDS = /** @type {const} */ ([
  ['setreviews', 'reviews_command'],
  ['setwtinit', 'worktree_init'],
  ['setwtsetup', 'worktree_setup'],
]);

/** What each of those was when the pane last read the config, before joining.
 *
 *  `"a b".split(/\s+/)` cannot recover `["a b"]`, so a field the user never touched
 *  has to go back exactly as it came. See `loadConfigInto`. */
/** @type {Map<string, string[]>} */
const loadedArgv = new Map();

/** Whether the config half holds edits that have not been saved.
 *
 *  **Because closing this pane used to throw them away in silence.** Typing an
 *  upstream ref and a worktree-setup command and then pressing Esc — the documented
 *  way out — left no trace of either, and reopening showed the old values as though
 *  nothing had been typed. The comment by the close handler argued only about clicks
 *  landing outside the pane; Esc and the gear were never in its scope.
 *
 *  So the draft outlives the pane instead: `loadConfigInto` refuses to overwrite it,
 *  the foot says it is there, and Discard is the way to let it go. Nothing asks a
 *  question on the way out, because a dialog on close is a toll paid by everyone who
 *  only came to read a setting.
 */
let dirty = false;

function markDirty() {
  if (dirty) return;
  dirty = true;
  showDirty();
}

function showDirty() {
  $('setdiscard').hidden = !dirty;
  /* **Red, and with a dot.** The foot note is `--faint-solid`, which is right for
     "saved, restarting…" and wrong for the one line in this pane that is asking you
     to do something: it sat at the bottom of a 1850px card in the dimmest colour the
     palette has, next to a Save button that looked exactly as it always does. */
  $('setnote').classList.toggle('dirty', dirty);
  if (dirty) $('setnote').textContent = 'unsaved changes';
  else if ($('setnote').textContent === 'unsaved changes') $('setnote').textContent = '';
}

function closeSettings() {
  $('settings').hidden = true;
  returnFocus('settings', $('settings'));
  $('gearbtn').setAttribute('aria-expanded', 'false');
}

// A working copy of `main_processes` while the panel is open. Each field is kept
// as the string the input shows (command joined by spaces, patterns by commas);
// `saveSettings` parses them back to arrays. Mutated in place by the row inputs.
/** One row of the processes editor: the strings the fields are bound to.
 *  Every field is the string the input holds, not the shape the daemon takes —
 *  `save` splits them back. `open` is the row's fold state and never travels.
 *  @type {{ name: string, command: string, ok_patterns: string,
 *           failure_patterns: string, restart: string, autostart: boolean,
 *           stop_command: string, open?: boolean }[]} */
let procDraft = [];

function openSettings() {
  // Two panes over the same pane is one too many, and the legend is the one you
  // were done with the moment you reached for this.
  borrowFocus('settings');
  closeLegend();
  $('settingsver').textContent = snap.version ? `orchd ${snap.version}` : '';
  if (!dirty) $('setnote').textContent = '';
  $('settings').hidden = false;
  $('gearbtn').setAttribute('aria-expanded', 'true');
  showDirty();
  // The panel edits the daemon's config, not the snapshot, so read it fresh.
  void loadConfigInto();
}

async function loadConfigInto(force = false) {
  // A draft is worth more than a fresh read: the values on disk have not changed
  // since the read that produced the draft, and overwriting it here is exactly the
  // silent loss `dirty` exists to stop.
  if (dirty && !force) return;
  let cfg;
  try {
    cfg = await get('/api/config');
  } catch (e) {
    $('setnote').textContent = reason(e);
    return;
  }
  ctl('setlang').value = cfg.default_language || '';
  /* Read-only, and the pane says so. A tracker is three fields, one of them a
     per-site host, so the old dropdown of two names cannot spell one — and a
     control that writes part of it is how a hand-edited tracker would get
     replaced by whatever the control happened to show. `/api/config` no longer
     carries it either, so this reads the snapshot's own answer. */
  $('settracker').textContent = snap.tracker_server
    ? `${snap.tracker_server} (config.json)`
    : 'none';
  ctl('setupref').value = cfg.upstream_ref || '';
  ctl('setupremote').value = cfg.upstream_remote || '';
  /* **Remembered as they were read, because joining an argv is lossy.** A config
     holding `["sh", "-c", "git fetch && git rebase upstream/develop"]` shows here
     as one line, and `argv()` below would split it back into nine words — so
     opening the pane to change the retention days and pressing Save silently
     rewrote a working hook into `sh -c git` with the rest as positional arguments,
     and `run_worktree_hook` is non-fatal, so nothing said so. `argvOf` sends the
     original back whenever the text has not been touched. */
  for (const [id, key] of ARGV_FIELDS) {
    const was = cfg[key] || [];
    loadedArgv.set(id, was);
    ctl(id).value = was.join(' ');
  }
  /* A note is prose, so it is read and written whole — `null` is the project
     saying nothing, and the box has to show that as empty rather than as the word
     "null". The write below turns an empty box back into `null` for the same
     reason: an empty string is a note, and an arriving agent would be handed it. */
  const notes = cfg.workspace_notes || {};
  ctl('setnotemain').value = notes.main || '';
  ctl('setnotetree').value = notes.worktree || '';
  // Numbers go in as numbers: `value = 0` on a number input renders "0", which is
  // the setting being off said out loud, where '' would read as unset.
  ctl('setretain').value = String(cfg.worktree_retention_days ?? 0);
  ctl('setseveral').checked = !!cfg.allow_several_in_main;
  dirty = false;
  showDirty();
  procDraft = (cfg.main_processes || []).map((/** @type {any} */ p) => ({
    name: p.name || '',
    command: (p.command || []).join(' '),
    ok_patterns: (p.ok_patterns || []).join(', '),
    failure_patterns: (p.failure_patterns || []).join(', '),
    restart: p.restart || 'never',
    autostart: !!p.autostart,
    stop_command: (p.stop_command || []).join(' '),
  }));
  renderProcs();
}

// A labelled text input bound to one string field of a process draft.
function procField(/** @type {string} */ label, /** @type {{ name: string, command: string, ok_patterns: string, failure_patterns: string, restart: string, autostart: boolean, stop_command: string, open?: boolean }} */ p, /** @type {'name' | 'command' | 'ok_patterns' | 'failure_patterns' | 'stop_command'} */ key) {
  const row = el('label', 'settings-field');
  row.appendChild(el('span', 'settings-k', label));
  const inp = el('input', 'settings-in');
  inp.type = 'text';
  inp.spellcheck = false;
  inp.value = p[key];
  inp.oninput = () => { p[key] = inp.value; };
  row.appendChild(inp);
  return row;
}

function renderProcs() {
  const host = $('setprocs');
  host.replaceChildren();
  $('setproccount').textContent = procDraft.length ? String(procDraft.length) : 'none';

  procDraft.forEach((p, i) => {
    const box = el('div', 'settings-proc');

    const top = el('div', 'settings-proc-top');
    const fold = el('button', 'settings-fold');
    fold.type = 'button';
    fold.setAttribute('aria-expanded', String(!!p.open));
    fold.appendChild(caret());
    fold.onclick = () => { p.open = !p.open; renderProcs(); };
    top.appendChild(fold);
    const name = el('input', 'settings-in');
    name.type = 'text';
    name.spellcheck = false;
    name.value = p.name;
    name.placeholder = 'name';
    name.oninput = () => { p.name = name.value; };
    top.appendChild(name);
    const auto = el('label', 'settings-proc-auto');
    const cb = el('input');
    cb.type = 'checkbox';
    cb.checked = p.autostart;
    cb.onchange = () => { p.autostart = cb.checked; };
    auto.appendChild(cb);
    auto.appendChild(el('span', null, 'autostart'));
    top.appendChild(auto);
    const del = el('button', 'settings-proc-del', 'remove');
    del.type = 'button';
    del.onclick = () => { procDraft.splice(i, 1); markDirty(); renderProcs(); };
    top.appendChild(del);
    box.appendChild(top);

    // Collapsed shows what it is and whether it starts itself; the four fields
    // underneath are the ones you set once and then scroll past forever.
    if (!p.open) {
      const gist = el('div', 'settings-proc-gist');
      gist.textContent = p.command || 'no command';
      gist.title = p.command || '';
      box.appendChild(gist);
      host.appendChild(box);
      return;
    }

    box.appendChild(procField('command', p, 'command'));
    box.appendChild(procField('ok when', p, 'ok_patterns'));
    box.appendChild(procField('fails when', p, 'failure_patterns'));
    /* Empty for anything ordinary. It is here rather than config-file-only for a
       blunt reason: this panel posts the whole process list, so a field it did not
       carry would be erased by the next save. */
    box.appendChild(procField('stop with', p, 'stop_command'));

    const rrow = el('label', 'settings-field');
    rrow.appendChild(el('span', 'settings-k', 'restart'));
    const sel = el('select', 'settings-in');
    for (const v of ['never', 'on_failure']) {
      const o = el('option', null, v);
      o.value = v;
      sel.appendChild(o);
    }
    sel.value = p.restart;
    sel.onchange = () => { p.restart = sel.value; };
    rrow.appendChild(sel);
    box.appendChild(rrow);

    host.appendChild(box);
  });
}

async function saveSettings() {
  const argv = (/** @type {string} */ s) => (s.trim() ? s.trim().split(/\s+/) : []);
  /* The value to send for an argv field: what was read, unless you edited the box.
     See `loadConfigInto` for the quoting this protects. */
  const argvOf = (/** @type {string} */ id) => {
    const was = loadedArgv.get(id);
    const now = ctl(id).value;
    return was && was.join(' ') === now ? was : argv(now);
  };
  const list = (/** @type {string} */ s) => s.split(',').map((/** @type {string} */ x) => x.trim()).filter(Boolean);
  const body = {
    default_language: ctl('setlang').value.trim(),
    upstream_ref: ctl('setupref').value.trim(),
    upstream_remote: ctl('setupremote').value.trim(),
    reviews_command: argvOf('setreviews'),
    worktree_init: argvOf('setwtinit'),
    worktree_setup: argvOf('setwtsetup'),
    workspace_notes: {
      main: ctl('setnotemain').value.trim() || null,
      worktree: ctl('setnotetree').value.trim() || null,
    },
    // A blank box means "keep forever" rather than NaN, and a negative number is
    // not a shorter retention.
    worktree_retention_days: Math.max(0, Math.trunc(Number(ctl('setretain').value) || 0)),
    allow_several_in_main: !!ctl('setseveral').checked,
    main_processes: procDraft.map((p) => ({
      name: p.name.trim(),
      command: argv(p.command),
      failure_patterns: list(p.failure_patterns),
      ok_patterns: list(p.ok_patterns),
      restart: p.restart,
      autostart: p.autostart,
      stop_command: argv(p.stop_command),
    })),
  };
  try {
    await call('/api/config', body);
  } catch (e) {
    $('setnote').textContent = reason(e);
    return;
  }
  /* Saved is only half of it: nothing here reaches the running daemon. The config
     is read once at start — `upstream_ref` is baked into the push guard's hook
     there, `main_processes` describes things already spawned — so the panel used
     to say "restart orchd to apply" and leave you to it, which made trying a
     review command a restart each time you changed your mind.
     The restart is the same one the agent-upgrade bar offers: the window goes
     down, the daemon takes its sessions with it, and `auto_resume` brings the
     live ones back with `--resume`. */
  $('setnote').textContent = 'saved, restarting\u2026';
  try {
    await callHost('/api/window/restart');
  } catch (e) {
    // A browser tab has no window to restart, and the daemon says so. Then the
    // old sentence is the right one: it is saved, and it applies when you restart
    // it yourself.
    $('setnote').textContent = `saved, restart orchd to apply (${reason(e)})`;
  }
}

/** The bundled face each role falls back to when a custom name is cleared. */
const THEME_DEF_KEY = { ui: 'plexsans', mono: 'plex', code: 'jetbrains' };

/** What each font role is called in the pane, and which size rides with it. */
/** @type {{ role: import('./theme.js').Role, size: 'termSize' | 'diffSize' | null, step?: string }[]} */
const ROLES = [
  { role: 'ui', size: null },        // the interface size is the board zoom
  { role: 'mono', size: 'termSize', step: 'ts' },
  { role: 'code', size: 'diffSize', step: 'ds' },
];

/** Say why something was refused, under the rows it is about — and out loud.
 *
 *  **Two places, because a sentence nobody hears is not a refusal.** The pane already
 *  had the note; what it never had was `#live`, the polite region the waitbar
 *  announces through, so a screen reader was told nothing when a font name was
 *  declined. Passing `''` clears both.
 */
function noteFor(/** @type {import('./theme.js').Role} */ role, text = '') {
  $(`th${role}note`).textContent = text;
  $(`th${role}noterow`).hidden = !text;
  if (text) $('live').textContent = text;
}

/** The three colours a theme is made of, in the order the pane shows them.
 *
 *  Ground, then panel, then text: outside in, which is how the eye reads a board
 *  and how a person builds one.
 */
/** @type {('bg' | 'panel' | 'text')[]} */
const COLOUR_ROLES = ['bg', 'panel', 'text'];

/** Say why a colour was refused, or clear it.
 *
 *  One slot for all three wells, unlike the font notes: the refusal is never about
 *  the well you touched, it is about the contrast between the ground and the text,
 *  so a sentence under one of them would be pointing at the wrong control.
 *  Announced as well as shown, for the reason [`noteFor`] gives.
 */
function noteColour(text = '') {
  $('thcolournote').textContent = text;
  $('thcolournoterow').hidden = !text;
  if (text) $('live').textContent = text;
}

/** Apply one hand-tuned colour, or say why not.
 *
 *  **Refused rather than corrected, in two different ways.** A spelling that is not
 *  a colour never reaches `setTheme`: it would fall back to the stored value there
 *  and the board would silently keep what it had while the box showed something
 *  else. A colour that *is* readable as a colour but leaves the board unreadable is
 *  `setTheme`'s own refusal, and it returns the sentence.
 *
 *  Either way the controls are re-rendered from the theme, so what the wells show
 *  is what the board is — the one property that makes a refusal legible at all.
 */
function applyColour(/** @type {'bg' | 'panel' | 'text'} */ role, /** @type {string} */ value) {
  const rgb = Palette.parseHex(value);
  if (!rgb) {
    noteColour(`"${value}" is not a colour. Six hex digits, like #1E1E1E.`);
    /* The wells alone, not `showTheme`: re-rendering the whole pane would put the
       box you are correcting back to the applied value mid-keystroke, which is the
       same trap the font row's note describes. */
    return;
  }
  const refused = setTheme({ [role]: Palette.toHex(rgb), custom: true });
  noteColour(refused ?? '');
  showTheme();
}

/** The whole appearance half, rendered from the theme.
 *
 *  **Everything, every time, and that is the fix for a real defect.** This used to
 *  redraw the preset and the three colour rows alone, so pressing Reset left the
 *  font select naming a face the board was no longer using and the size readouts
 *  showing numbers nobody had — and picking that same entry again fired no `change`,
 *  so the control could not be made true. A renderer that covers part of a pane is
 *  a renderer that will disagree with it.
 */
function showTheme() {
  ctl('thpreset').value = currentPreset() ?? 'custom';
  /* The wells follow the dropdown rather than `theme.custom`, so the one thing
     that decides whether they are reachable is the thing the user just picked.
     They are the same answer except for a board hand-tuned by an older build,
     which reads as `custom` here and gets its wells. */
  const custom = ctl('thpreset').value === 'custom';
  for (const role of COLOUR_ROLES) {
    $(`th${role}row`).hidden = !custom;
    // The well takes `#rrggbb` and nothing else, and the theme is normalised to
    // exactly that on the way in, so neither control needs to defend itself here.
    ctl(`th${role}well`).value = theme[role];
    ctl(`th${role}hex`).value = theme[role];
  }
  if (!custom) noteColour();
  /* The slider goes with the wells. Opacity is one of the four things a theme is,
     and every preset carries its own — so leaving it reachable under a preset
     would let you drag the board away from the theme the dropdown still names. */
  $('thopacityrow').hidden = !custom;
  const pct = Math.round(theme.opacity * 100);
  ctl('thopacity').value = String(pct);
  $('thopacityval').textContent = `${pct}%`;
  showTermScheme();
  for (const { role, size, step } of ROLES) {
    showFont(role);
    if (size && step) showSize(step, theme[size]);
  }
  // `setZoom` writes this too; said here so the renderer covers all six controls
  // rather than covering five and relying on something else for the sixth.
  showSize('fs', uiPx(), UI_PX_MIN, UI_PX_MAX);
}

/** One size readout, and its two buttons at the ends of its own range.
 *
 *  The interface range is narrower than the other two and not ours to widen: it is
 *  the board scale, whose bounds keep the rail's own columns from collapsing.
 */
function showSize(/** @type {string} */ step, /** @type {number} */ px, min = SIZE_MIN, max = SIZE_MAX) {
  $(`${step}val`).textContent = `${px}px`;
  ctl(`${step}down`).disabled = px <= min;
  ctl(`${step}up`).disabled = px >= max;
}

/** Fill one role's dropdown: the vendored faces, whatever resolves here, and
 *  "Other…".
 *
 *  The vendored ones come first and are labelled plainly, because they are the
 *  only families certain to be there — everything under `detected` is a name this
 *  machine answered to, which is not the same as a name it has.
 */
function fillFonts(/** @type {import('./theme.js').Role} */ role) {
  const sel = ctl(`th${role}`);
  if (sel.options.length) return;
  const want = role !== 'ui';
  const group = (/** @type {string} */ label, /** @type {(string | { key: string, label: string })[]} */ names, /** @type {(name: any) => string} */ value) => {
    if (!names.length) return;
    const g = el('optgroup');
    g.label = label;
    for (const n of names) {
      const o = el('option', null, typeof n === 'string' ? n : n.label);
      o.value = value(n);
      g.appendChild(o);
    }
    sel.appendChild(g);
  };
  group('Bundled', Object.entries(FONTS).filter(([, f]) => f.mono === want).map(([k, f]) => ({ key: k, label: f.label })),
    (/** @type {{ key: string }} */ n) => n.key);
  group('On this machine', detectedFonts()[want ? 'mono' : 'sans'], (/** @type {string} */ n) => `custom:${n}`);
  const other = el('option', null, 'Other\u2026');
  other.value = 'other';
  sel.appendChild(other);
}

/** Show the terminal's palette, and preview it on the ground it will sit on.
 *
 *  **The eight normal hues, not the sixteen.** The bright half is the same eight
 *  again in most schemes, and a strip of sixteen dots at 6px reads as a smear —
 *  what the preview is for is telling Gruvbox from Nord at a glance, which the
 *  normal eight already do.
 *
 *  Rebuilt rather than recoloured: eight children is cheaper to replace than to
 *  diff, and this runs once per theme change.
 */
function showTermScheme() {
  ctl('thterm').value = theme.term;
  const colours = Palette.termColours(theme, theme.term);
  const swatch = $('thtermsample');
  swatch.style.background = String(colours.background);
  swatch.replaceChildren(...HUES.map((hue) => {
    const dot = el('i');
    dot.style.background = String(colours[hue]);
    return dot;
  }));
}

/** The eight the swatch shows, in the order a terminal numbers them. */
const HUES = /** @type {const} */ ([
  'black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white',
]);

/** Show a role's current font, its preview, and its name box when it has one. */
function showFont(/** @type {import('./theme.js').Role} */ role) {
  const key = theme[role];
  const custom = typeof key === 'string' && key.startsWith('custom:');
  const sel = ctl(`th${role}`);
  // A `custom:` face the machine offered is in the list; one typed by hand is not,
  // and lands on "Other…" with its name in the box below.
  sel.value = [...sel.options].some((o) => o.value === key) ? key : (custom ? 'other' : key);
  // The *row* hides, never the input: hiding both leaves a control that is
  // display:none inside a visible row the moment the row is shown again.
  $(`th${role}customrow`).hidden = sel.value !== 'other';
  // From the theme, so a refused name is replaced by what is actually applied.
  ctl(`th${role}custom`).value = custom ? key.slice('custom:'.length) : '';
  // The preview wears the stack it is previewing, which is the only honest way to
  // show a family the page cannot verify it really has.
  $(`th${role}sample`).style.fontFamily = fontStack(role);
}

function setupSettings() {
  setZoom(Number(localStorage.getItem(ZOOM.key)) || ZOOM.def);
  setWheel(Number(localStorage.getItem(WHEEL.key)) || WHEEL.def);

  $('gearbtn').onclick = (ev) => {
    ev.stopPropagation();
    if (settingsOpen()) closeSettings();
    else openSettings();
  };
  // Each names its chord, so the panel teaches the shortcut rather than replacing
  // it. `MOD_LABEL` because the modifier differs by platform.
  $('fsdown').title = `Smaller · ${MOD_LABEL} \u2212`;
  $('fsup').title = `Larger · ${MOD_LABEL} =`;
  // By the pixel the readout shows, not by the scale behind it: a step that moves
  // the number by one is the same control as the other two sizes, which is the whole
  // reason this reads in px.
  $('fsdown').onclick = () => { saveZoom(setUiPx(uiPx() - 1)); showTheme(); };
  $('fsup').onclick = () => { saveZoom(setUiPx(uiPx() + 1)); showTheme(); };
  // No chord for these: the keyboard map's own contract says a plain letter is
  // taken only where the idiom earns it, and nobody expects one for a wheel.
  $('wsdown').onclick = () => saveWheel(setWheel(wheelScale - WHEEL.step));
  $('wsup').onclick = () => saveWheel(setWheel(wheelScale + WHEEL.step));
  $('wsreset').onclick = () => saveWheel(setWheel(WHEEL.def));
  /* A picked preset lands whole — the three colours and the opacity — because that
     is what a theme is now: a board at 72% is not Paper, so picking Paper has to
     put the slider back or the dropdown would be naming something the window is
     not. `setTheme` refuses a pair under the contrast floor, and every preset
     clears it, so the refusal is unreachable from this control by construction.

     `custom` is selectable now, and it is the one entry that applies no values:
     it unlocks the three wells below and leaves the board exactly as it is, which
     is why `theme.custom` is stored rather than derived — see `currentPreset`. */
  const presets = ctl('thpreset');
  for (const [key, p] of Object.entries(PRESETS)) presets.appendChild(el('option', null, p.label)).value = key;
  presets.appendChild(el('option', null, 'Custom')).value = 'custom';
  presets.onchange = (/** @type {Event} */ ev) => {
    const key = /** @type {HTMLSelectElement} */ (ev.target).value;
    const p = PRESETS[/** @type {keyof typeof PRESETS} */ (key)];
    noteColour();
    setTheme(p
      ? { bg: p.bg, panel: p.panel, text: p.text, opacity: p.opacity, custom: false }
      : { custom: true });
    showTheme();
  };

  /* The board's own palette first and by name, then the schemes. `board` is not in
     `TERM_SCHEMES` — it is the absence of one — so the option is written here the
     way `custom` is written into the theme dropdown above it.

     No refusal path: a scheme is a fixed table that cleared the contrast floor at
     check time, where the three wells take whatever you drag them to. */
  const schemes = ctl('thterm');
  schemes.appendChild(el('option', null, 'Board')).value = Palette.TERM_BOARD;
  for (const [key, s] of Object.entries(Palette.TERM_SCHEMES)) {
    schemes.appendChild(el('option', null, s.label)).value = key;
  }
  schemes.onchange = (/** @type {Event} */ ev) => {
    setTheme({ term: /** @type {HTMLSelectElement} */ (ev.target).value });
    showTermScheme();
  };

  /* One handler for the three roles, each with a well and a hex box saying the
     same thing two ways. The well raises `input` while you drag, which is the
     point of a picker — you judge a ground against the board, not against a
     swatch — and the hex box raises `change`, because a half-typed `#1a` is not a
     refusal, it is somebody still typing. */
  for (const role of COLOUR_ROLES) {
    ctl(`th${role}well`).oninput = (/** @type {Event} */ ev) => {
      applyColour(role, /** @type {HTMLInputElement} */ (ev.target).value);
    };
    ctl(`th${role}hex`).onchange = (/** @type {Event} */ ev) => {
      applyColour(role, /** @type {HTMLInputElement} */ (ev.target).value.trim());
    };
  }

  for (const role of /** @type {import('./theme.js').Role[]} */ (['ui', 'mono', 'code'])) {
    fillFonts(role);
    ctl(`th${role}`).onchange = (/** @type {Event} */ ev) => {
      const v = /** @type {HTMLSelectElement} */ (ev.target).value;
      // "Other…" is a request to type a name, not a font: keep the face until one
      // arrives, and open the box.
      if (v === 'other') {
        $(`th${role}customrow`).hidden = false;
        ctl(`th${role}custom`).focus();
        return;
      }
      noteFor(role);
      setTheme({ [role]: v });
      showFont(role);
    };
    ctl(`th${role}custom`).onchange = (/** @type {Event} */ ev) => {
      const name = String(/** @type {HTMLInputElement} */ (ev.target).value).trim();
      // Said, not swallowed: a box still holding a name the board is not using is
      // a control disagreeing with the board and saying nothing about it.
      if (name && !validFontName(name)) {
        /* The note, and nothing else: re-rendering here would put the select back
           on the applied font and fold the row away — taking the box you are
           typing in with it, mid-correction. What you typed stays, the board keeps
           the font it has, and the sentence says which is which. */
        noteFor(role, `"${name}" is not a font name. Letters, digits, spaces, dots and hyphens.`);
        return;
      }
      noteFor(role);
      setTheme({ [role]: name ? `custom:${name}` : THEME_DEF_KEY[role] });
      showFont(role);
    };
  }

  /* One handler for the two px sizes, because they are the same control twice and
     the pane has already paid once for two spellings of one idea. */
  for (const { size, step } of ROLES) {
    // A `filter` does not narrow the element type, and `ui` has neither: its
    // size is the board zoom, set from the other pane.
    if (!size || !step) continue;
    const nudge = (/** @type {number} */ by) => { setTheme({ [size]: theme[size] + by }); showTheme(); };
    ctl(`${step}down`).onclick = () => nudge(-1);
    ctl(`${step}up`).onclick = () => nudge(1);
  }

  /* `input`, not `change`: the point of a slider here is watching the board move
     under it. Cheap enough — one `setProperty` of `--ground` per frame. */
  ctl('thopacity').oninput = (/** @type {Event} */ ev) => {
    setTheme({ opacity: Number(/** @type {HTMLInputElement} */ (ev.target).value) / 100 });
    showTheme();
  };
  /* Said once, at boot, because the window cannot become see-through while it is
     open — `transparent` is fixed when the window is built. The control still
     works: the value is stored and applies at the next launch. */
  if (!SEE_THROUGH) {
    $('thopacityhint').textContent = 'the window is solid — set see_through_window in host.json';
  }

  /* **The board zoom goes back too.** It is the interface size now — one of the six
     controls this button's hint promises — and it lives in its own store, so
     resetting the theme alone would have left the one appearance setting the pane
     still showed as changed. */
  $('threset').title = 'Theme, the three fonts and sizes, and the opacity';
  $('threset').onclick = () => {
    resetTheme();
    saveZoom(setZoom(ZOOM.def));
    for (const { role } of ROLES) noteFor(role);
    // The colour note goes too: Reset puts the shipped palette back, so a refusal
    // about the pair that was there is about a board that no longer exists.
    noteColour();
    showTheme();
  };
  showTheme();

  /* **Delegated, so a field added to the config half cannot be forgotten here.** It
     was a list of ids, and three fields were added to the markup without being
     added to it — so typing into any of them left `dirty` false, the foot said
     nothing, and `loadConfigInto` overwrote the draft on the next open. Exactly the
     silent loss the flag above exists to stop, reintroduced by an edit in another
     file.

     **Scoped to `[data-config]`, not to the pane.** Delegating to `#settings`
     itself was the first attempt and was worse than the list: the appearance half
     lives in the same element, so picking a theme or dragging the opacity slider
     marked the *config* unsaved — and `loadConfigInto` then refuses to re-read for
     the rest of the page's life, so the next open shows another checkout's values
     and Save writes them to this one.

     `change` as well as `input` because a checkbox raises only the first of those
     in some engines, and the process rows are rebuilt on every render — which is
     the other reason this cannot be per-node. Folding a row open is a click and
     raises neither, which is right: looking at a process is not editing it. */
  for (const ev of ['input', 'change']) {
    $('settings').addEventListener(ev, (e) => {
      if (/** @type {HTMLElement} */ (e.target).closest('[data-config]')) markDirty();
    });
  }
  $('setdiscard').onclick = () => { dirty = false; void loadConfigInto(true); };
  $('setdiscard').title = 'Throw the unsaved edits away and read the config again';

  $('setclose').onclick = () => closeSettings();

  $('setprocadd').onclick = () => {
    procDraft.push({
      name: '', command: '', ok_patterns: '', failure_patterns: '',
      restart: 'never', autostart: false, stop_command: '', open: true,
    });
    markDirty();
    renderProcs();
  };
  $('setsave').onclick = saveSettings;
  $('setsave').title = 'Saves, then quits and comes back, because the config is '
    + 'read at start. Live sessions are resumed as they were when `auto_resume` is on.';

  /* **Nothing closes this pane by accident.** The gear, the X and Esc are the
     three ways out, and that is deliberate: what used to sit here was a captured
     `mousedown` on the document that put the panel away on any click outside it.
     That rule is left over from when settings floated over the window as a modal
     with a scrim — its backdrop was the way out, and the scrim clause was already
     deleted once for the same reason ("missing an input closed the panel").

     It fills the centre column now, so a click on the rail, the terminal, the
     drawer, a toast or a splitter is not a gesture at this panel at all.

     **And the three ways out no longer cost the draft.** They used to: `procDraft`
     and every form field lived in the DOM until Save, so Esc — the documented way
     out — threw away a half-typed worktree command with nothing said. `dirty` is
     what changed that; see it for the rest. */
}

export { settingsOpen as isOpen, openSettings as open, closeSettings as close, setupSettings as setup };
