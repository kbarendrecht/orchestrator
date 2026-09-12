// The settings panel. The zoom control it offers lives in core, because the
// terminals read the scale too.

import { ctl, $, WHEEL, ZOOM, call, callHost, caret, currentPreset, detectedFonts, FONTS, fontStack, PRESETS, resetTheme, SEE_THROUGH, SIZE_MAX, SIZE_MIN, setUiPx, uiPx, UI_PX_MAX, UI_PX_MIN,
  setTheme, theme, validFontName, closeLegend, el, get, MOD_LABEL, reason, saveWheel, saveZoom, setWheel, setZoom, snap, wheelScale } from './core.js';

const settingsOpen = () => !$('settings').hidden;

/** The config fields this pane edits — every one of them a draft until Save. */
const CONFIG_FIELDS = ['setlang', 'setupref', 'setupremote', 'setreviews', 'setwtsetup',
  'setretain', 'setseveral'];

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
  ctl('setreviews').value = (cfg.reviews_command || []).join(' ');
  ctl('setwtsetup').value = (cfg.worktree_setup || []).join(' ');
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
  const list = (/** @type {string} */ s) => s.split(',').map((/** @type {string} */ x) => x.trim()).filter(Boolean);
  const body = {
    default_language: ctl('setlang').value.trim(),
    upstream_ref: ctl('setupref').value.trim(),
    upstream_remote: ctl('setupremote').value.trim(),
    reviews_command: argv(ctl('setreviews').value),
    worktree_setup: argv(ctl('setwtsetup').value),
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
/** @type {{ role: import('./core.js').Role, size: 'termSize' | 'diffSize' | null, step?: string }[]} */
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
function noteFor(/** @type {import('./core.js').Role} */ role, text = '') {
  $(`th${role}note`).textContent = text;
  $(`th${role}noterow`).hidden = !text;
  if (text) $('live').textContent = text;
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
  const pct = Math.round(theme.opacity * 100);
  ctl('thopacity').value = String(pct);
  $('thopacityval').textContent = `${pct}%`;
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
function fillFonts(/** @type {import('./core.js').Role} */ role) {
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

/** Show a role's current font, its preview, and its name box when it has one. */
function showFont(/** @type {import('./core.js').Role} */ role) {
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
  /* `custom` is an option rather than a blank, so a hand-tuned set has something
     to show — and it is `disabled`, because picking it would mean nothing: there
     is no palette called custom to apply. */
  /* The presets are the only way to set a colour now, so a picked one has to land
     whole: `setTheme` refuses a pair under the contrast floor, and every preset
     clears it, so the refusal is unreachable from here by construction. */
  const presets = ctl('thpreset');
  for (const [key, p] of Object.entries(PRESETS)) presets.appendChild(el('option', null, p.label)).value = key;
  const custom = el('option', null, 'Custom');
  custom.value = 'custom';
  custom.disabled = true;
  presets.appendChild(custom);
  presets.onchange = (/** @type {Event} */ ev) => {
    const p = PRESETS[/** @type {keyof typeof PRESETS} */ (/** @type {HTMLSelectElement} */ (ev.target).value)];
    if (p) setTheme({ bg: p.bg, panel: p.panel, text: p.text });
    showTheme();
  };

  for (const role of /** @type {import('./core.js').Role[]} */ (['ui', 'mono', 'code'])) {
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
    showTheme();
  };
  showTheme();

  for (const id of CONFIG_FIELDS) ctl(id).addEventListener('input', markDirty);
  /* Delegated, because the process rows are rebuilt on every render and binding
     `markDirty` to each of their seven controls is seven places to forget it.
     Folding a row open is a click on a button and raises neither event, which is
     right: looking at a process is not editing it. */
  for (const ev of ['input', 'change']) $('setprocs').addEventListener(ev, markDirty);
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
