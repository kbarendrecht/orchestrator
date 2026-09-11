// The settings panel. The zoom control it offers lives in core, because the
// terminals read the scale too.

import { ctl, $, WHEEL, ZOOM, call, callHost, caret, currentPreset, detectedFonts, FONTS, fontStack, PRESETS, resetTheme, SEE_THROUGH,
  setTheme, theme, validFontName, closeLegend, el, get, MOD_LABEL, saveWheel, saveZoom, setWheel, setZoom, snap, wheelScale, zoomScale } from './core.js';
import { parseHex, toHex } from './palette.js';

const settingsOpen = () => !$('settings').hidden;

function closeSettings() {
  $('settings').hidden = true;
  $('gearbtn').setAttribute('aria-expanded', 'false');
}

// A working copy of `main_processes` while the panel is open. Each field is kept
// as the string the input shows (command joined by spaces, patterns by commas);
// `saveSettings` parses them back to arrays. Mutated in place by the row inputs.
let procDraft = [];

function openSettings() {
  // Two panes over the same pane is one too many, and the legend is the one you
  // were done with the moment you reached for this.
  closeLegend();
  $('settingsver').textContent = snap.version ? `orchd ${snap.version}` : '';
  $('setnote').textContent = '';
  $('settings').hidden = false;
  $('gearbtn').setAttribute('aria-expanded', 'true');
  // The panel edits the daemon's config, not the snapshot, so read it fresh.
  loadConfigInto();
}

async function loadConfigInto() {
  let cfg;
  try {
    cfg = await get('/api/config');
  } catch (e) {
    $('setnote').textContent = e.message;
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
  procDraft = (cfg.main_processes || []).map((p) => ({
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
function procField(label, p, key) {
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
    del.onclick = () => { procDraft.splice(i, 1); renderProcs(); };
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
  const argv = (s) => (s.trim() ? s.trim().split(/\s+/) : []);
  const list = (s) => s.split(',').map((x) => x.trim()).filter(Boolean);
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
    $('setnote').textContent = e.message;
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
    $('setnote').textContent = `saved, restart orchd to apply (${e.message})`;
  }
}

/** The three wells, their hex boxes, and the sentence that explains a refusal.
 *
 *  Rendered from the theme rather than remembered, so a refusal leaves the
 *  controls showing what is actually applied rather than what was attempted.
 */
/** The slider, its readout and its hint, from the theme. */
function showOpacity() {
  const pct = Math.round(theme.opacity * 100);
  ctl('thopacity').value = String(pct);
  $('thopacityval').textContent = `${pct}%`;
}

function showTheme(note = '') {
  ctl('thpreset').value = currentPreset() ?? 'custom';
  for (const role of ['bg', 'panel', 'text']) {
    ctl(`th${role}`).value = theme[role];
    ctl(`th${role}hex`).value = theme[role];
  }
  showOpacity();
  $('thnote').textContent = note;
  $('thnoterow').hidden = !note;
}

/** The bundled face each role falls back to when a custom name is cleared. */
const THEME_DEF_KEY = { ui: 'plexsans', mono: 'plex', code: 'jetbrains' };

/** Nudge the terminal's base size and redraw the readout. */
function stepTermSize(by) {
  setTheme({ termSize: theme.termSize + by });
  showTermSize();
}

function showTermSize() {
  $('tsval').textContent = `${theme.termSize}px`;
  ctl('tsdown').disabled = theme.termSize <= 8;
  ctl('tsup').disabled = theme.termSize >= 24;
}

/** Fill one role's dropdown: the vendored faces, whatever resolves here, and
 *  "Other…".
 *
 *  The vendored ones come first and are labelled plainly, because they are the
 *  only families certain to be there — everything under `detected` is a name this
 *  machine answered to, which is not the same as a name it has.
 */
function fillFonts(role) {
  const sel = ctl(`th${role}`);
  if (sel.options.length) return;
  const want = role === 'ui' ? false : true;
  const group = (label, names, value) => {
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
    (n) => n.key);
  group('On this machine', detectedFonts()[want ? 'mono' : 'sans'], (n) => `custom:${n}`);
  const other = el('option', null, 'Other\u2026');
  other.value = 'other';
  sel.appendChild(other);
}

/** Show a role's current font, its preview, and its name box when it has one. */
function showFont(role) {
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

/** Apply one colour, or show why it was refused.
 *
 *  **`#fff` is accepted**, because it is the single most likely thing typed into a
 *  hex box. Anything else the parser cannot read snaps the field back — with the
 *  same note, so nothing reverts in silence.
 */
function pickColour(role, raw) {
  const rgb = parseHex(expandHex(raw));
  if (!rgb) {
    showTheme(`"${raw}" is not a colour. Six hex digits, or three.`);
    return;
  }
  showTheme(setTheme({ [role]: toHex(rgb) }) || '');
}

/** `#abc` to `#aabbcc`. Anything else is handed back untouched for the parser to
 *  refuse, so this widens what is accepted without widening what is believed. */
function expandHex(raw) {
  const m = /^#?([0-9a-f])([0-9a-f])([0-9a-f])$/i.exec(String(raw ?? '').trim());
  return m ? `#${m[1]}${m[1]}${m[2]}${m[2]}${m[3]}${m[3]}` : raw;
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
  $('fsreset').title = `Reset · ${MOD_LABEL} 0`;
  $('fsdown').onclick = () => saveZoom(setZoom(zoomScale - ZOOM.step));
  $('fsup').onclick = () => saveZoom(setZoom(zoomScale + ZOOM.step));
  $('fsreset').onclick = () => saveZoom(setZoom(ZOOM.def));
  // No chord for these: the keyboard map's own contract says a plain letter is
  // taken only where the idiom earns it, and nobody expects one for a wheel.
  $('wsdown').onclick = () => saveWheel(setWheel(wheelScale - WHEEL.step));
  $('wsup').onclick = () => saveWheel(setWheel(wheelScale + WHEEL.step));
  $('wsreset').onclick = () => saveWheel(setWheel(WHEEL.def));
  /* `custom` is an option rather than a blank, so a hand-tuned set has something
     to show — and it is `disabled`, because picking it would mean nothing: there
     is no palette called custom to apply. */
  const presets = ctl('thpreset');
  for (const [key, p] of Object.entries(PRESETS)) presets.appendChild(el('option', null, p.label)).value = key;
  const custom = el('option', null, 'Custom');
  custom.value = 'custom';
  custom.disabled = true;
  presets.appendChild(custom);
  presets.onchange = (ev) => {
    const p = PRESETS[ev.target.value];
    if (p) showTheme(setTheme({ bg: p.bg, panel: p.panel, text: p.text }) || '');
  };

  for (const role of ['ui', 'mono', 'code']) {
    fillFonts(role);
    ctl(`th${role}`).onchange = (ev) => {
      const v = ev.target.value;
      // "Other…" is a request to type a name, not a font: keep the face until one
      // arrives, and open the box.
      if (v === 'other') {
        $(`th${role}customrow`).hidden = false;
        ctl(`th${role}custom`).focus();
        return;
      }
      setTheme({ [role]: v });
      showFont(role);
    };
    ctl(`th${role}custom`).onchange = (ev) => {
      const name = String(ev.target.value).trim();
      // Said, not swallowed: a box still holding a name the board is not using is
      // the same silence a colour control reverting with no sentence would be.
      if (name && !validFontName(name)) {
        /* The note, and nothing else: re-rendering here would put the select back
           on the applied font and fold the row away — taking the box you are
           typing in with it, mid-correction. What you typed stays, the board keeps
           the font it has, and the sentence says which is which. */
        showTheme(`"${name}" is not a font name. Letters, digits, spaces, dots and hyphens.`);
        return;
      }
      setTheme({ [role]: name ? `custom:${name}` : THEME_DEF_KEY[role] });
      showTheme();
      showFont(role);
    };
    showFont(role);
  }

  $('tsdown').onclick = () => stepTermSize(-1);
  $('tsup').onclick = () => stepTermSize(1);
  $('tsreset').onclick = () => { setTheme({ termSize: 12 }); showTermSize(); };
  showTermSize();

  showTheme();
  for (const role of ['bg', 'panel', 'text']) {
    /* `input` rather than `change` on the well: the native picker streams while
       you drag, and a board that only catches up when the dialog closes makes
       choosing a colour a guess. The hex box is the opposite — `change`, so it is
       not refused character by character while you type one. */
    ctl(`th${role}`).oninput = (ev) => pickColour(role, ev.target.value);
    ctl(`th${role}hex`).onchange = (ev) => pickColour(role, ev.target.value);
  }
  /* `input`, not `change`: the point of a slider here is watching the board move
     under it. Cheap enough — one `setProperty` of `--ground` per frame. */
  ctl('thopacity').oninput = (ev) => {
    setTheme({ opacity: Number(ev.target.value) / 100 });
    showOpacity();
  };
  /* Said once, at boot, because the window cannot become see-through while it is
     open — `transparent` is fixed when the window is built. The control still
     works: the value is stored and applies at the next launch. */
  if (!SEE_THROUGH) {
    $('thopacityhint').textContent = 'the window is solid — set see_through_window in host.json';
  }

  $('threset').title = 'Back to the palette orchd ships with';
  $('threset').onclick = () => showTheme(resetTheme() || '');

  $('setclose').onclick = () => closeSettings();

  $('setprocadd').onclick = () => {
    procDraft.push({
      name: '', command: '', ok_patterns: '', failure_patterns: '',
      restart: 'never', autostart: false, stop_command: '', open: true,
    });
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
     drawer, a toast or a splitter is not a gesture at this panel at all. And every
     one of them discarded the draft: `procDraft` and each form field live only in
     the DOM until Save, so a stray click lost a half-typed process command with
     nothing said. */
}

export { settingsOpen as isOpen, openSettings as open, closeSettings as close, setupSettings as setup };
