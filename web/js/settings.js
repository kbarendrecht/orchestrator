// The settings panel. The zoom control it offers lives in core, because the
// terminals read the scale too.

import { ctl, $, FONTS, OPACITY, PRESETS, TRANSPARENT, WHEEL, ZOOM, call, callShell, caret, clampOpacity, closeLegend, el, get, MOD_LABEL, saveWheel, saveZoom, setTheme, setWheel, setZoom, snap, theme, wheelScale, zoomScale } from './core.js';

const settingsOpen = () => !$('settings').hidden;

/** An argv as one line somebody can edit, and `argv` can read back.
 *
 *  **Quotes anything with whitespace in it**, which is the half that was missing:
 *  the box joined an argv with spaces and the save split it on them, so the one
 *  path that always has a space on macOS — `~/Library/Application Support/orchd/
 *  reviews.js` — came apart into two arguments on the first save. */
const showArgv = (list) =>
  (list || [])
    .map((a) => (/[\s"']/.test(a) ? `"${String(a).replace(/"/g, '')}"` : a))
    .join(' ');

function closeSettings() {
  $('settings').hidden = true;
  $('gearbtn').setAttribute('aria-expanded', 'false');
}

// A working copy of `main_processes` while the panel is open. Each field is kept
// as the string the input shows (command joined by spaces, patterns by commas);
// `saveSettings` parses them back to arrays. Mutated in place by the row inputs.
let procDraft = [];

/* ---------------------------------------------------------------------------
 * Theme
 * ------------------------------------------------------------------------- */

/** Which preset the three colours currently match, or `''` for none.
 *
 *  Derived rather than stored, which is what makes the dropdown honest: adjust
 *  one colour off a preset and it stops claiming to be that preset, without
 *  anything having to remember that you did. */
function currentPreset() {
  for (const [k, v] of Object.entries(PRESETS)) {
    if (v.bg === theme.bg && v.panel === theme.panel && v.text === theme.text) return k;
  }
  return '';
}

/** Put the controls where the theme is. Called on setup and after every change,
 *  because a preset moves three fields and a colour moves the preset. */
function showTheme() {
  const preset = currentPreset();
  ctl('thpreset').value = preset;
  ctl('thfont').value = theme.font;
  $('thcustomrow').hidden = theme.font !== 'custom';
  ctl('thcustom').value = theme.custom || '';
  for (const [id, value] of [['thbg', theme.bg], ['thpanel', theme.panel], ['thtext', theme.text]]) {
    ctl(id).value = value;
    ctl(`${id}hex`).value = value;
  }

  /* **Shown but disabled when the window is opaque, rather than hidden.** An
     absent control reads as a missing feature; a dead one with the reason beside
     it reads as a thing to switch on, which is what it is. The hint names where. */
  $('thopval').textContent = `${Math.round(theme.opacity * 100)}%`;
  ctl('thopdown').disabled = !TRANSPARENT || theme.opacity <= OPACITY.min;
  ctl('thopup').disabled = !TRANSPARENT || theme.opacity >= OPACITY.max;
  $('thopachint').hidden = TRANSPARENT;
}

function setupTheme() {
  const presets = ctl('thpreset');
  /* An empty option for "none of them", selected whenever the colours have been
     adjusted. Without it the dropdown would keep naming the preset you started
     from, which is a control lying about the state it is showing. */
  presets.appendChild(el('option', null, 'Custom')).value = '';
  for (const [k, v] of Object.entries(PRESETS)) {
    presets.appendChild(el('option', null, v.label)).value = k;
  }
  presets.onchange = () => {
    const p = PRESETS[presets.value];
    // Only the three colours: a preset is a palette, and taking the font with it
    // would undo a choice you made about something else.
    if (p) applyAndShow({ bg: p.bg, panel: p.panel, text: p.text });
  };

  const fonts = ctl('thfont');
  for (const [k, v] of Object.entries(FONTS)) {
    fonts.appendChild(el('option', null, v.label)).value = k;
  }
  /* Last, and only a fallback now that the list is detected rather than guessed:
     a family this machine has under a name `MONO_CANDIDATES` does not carry. */
  fonts.appendChild(el('option', null, 'Other…')).value = 'custom';
  fonts.onchange = () => applyAndShow({ font: fonts.value });

  /* `change`, not `input`: a font name is typed a character at a time, and
     re-measuring every terminal's cell metrics on each keystroke — which is what
     `applyTheme`'s refit does — would fight the person typing. */
  ctl('thcustom').onchange = () => applyAndShow({ custom: ctl('thcustom').value });

  for (const [id, field] of [['thbg', 'bg'], ['thpanel', 'panel'], ['thtext', 'text']]) {
    /* `input` here, because a colour well is dragged and watching the board
       follow is the whole point of having one. Cheap: this writes tokens and
       repaints, and xterm's refit is the only real cost. */
    ctl(id).oninput = () => applyAndShow({ [field]: ctl(id).value });
    /* And the hex field on `change`, so a half-typed `#1` is not read as a
       colour. Refused rather than corrected when it is not six digits: silently
       rewriting what somebody pasted is worse than leaving it for them to see. */
    ctl(`${id}hex`).onchange = () => {
      const v = ctl(`${id}hex`).value.trim();
      if (/^#?[0-9a-f]{6}$/i.test(v)) applyAndShow({ [field]: v.startsWith('#') ? v : `#${v}` });
      else showTheme();
    };
  }

  /* **Each row resets its own row and nothing else.** The Theme reset used to put
     the font back too, on the grounds that it was "the theme" — a Reset on one row
     silently changing another, which a tooltip papered over rather than fixed.
     Now the row you press is the row that changes. */
  $('threset').title = 'The three colours, back to Orchd dark';
  $('threset').onclick = () => applyAndShow({
    bg: PRESETS.orchd.bg, panel: PRESETS.orchd.panel, text: PRESETS.orchd.text,
  });
  $('thfontreset').onclick = () => applyAndShow({ font: 'plex', custom: null });
  /* "Clear", not "Reset": there is no default name to go back to — the field is
     either empty or it holds one you typed. */
  $('thcustomreset').onclick = () => applyAndShow({ custom: null });
  for (const [id, field] of [['thbg', 'bg'], ['thpanel', 'panel'], ['thtext', 'text']]) {
    $(`${id}reset`).onclick = () => applyAndShow({ [field]: PRESETS.orchd[field] });
  }

  const opacity = (v) => applyAndShow({ opacity: clampOpacity(v) });
  $('thopdown').onclick = () => opacity(theme.opacity - OPACITY.step);
  $('thopup').onclick = () => opacity(theme.opacity + OPACITY.step);
  $('thopreset').onclick = () => opacity(OPACITY.def);
  showTheme();
}

function applyAndShow(patch) {
  setTheme(patch);
  showTheme();
}

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
  ctl('setreviews').value = showArgv(cfg.reviews_command);
  // Same round trip, same reason: a repo whose setup script lives under a path
  // with a space would come apart on the first save too.
  ctl('setwtsetup').value = showArgv(cfg.worktree_setup);
  // Numbers go in as numbers: `value = 0` on a number input renders "0", which is
  // the setting being off said out loud, where '' would read as unset.
  ctl('setretain').value = String(cfg.worktree_retention_days ?? 0);
  ctl('setseveral').checked = !!cfg.allow_several_in_main;
  ctl('settransparent').checked = !!cfg.window_transparent;
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
  /* **Quote-aware, because the default value contains a space on every Mac.**
     `reviews_command` defaults to `<config dir>/reviews.js`, and the config dir is
     `~/Library/Application Support/orchd` there — so a plain `split(/\s+/)` turned
     one path into `["…/Library/Application", "Support/orchd/reviews.js"]` and the
     review queue read as unavailable from then on. Pressing Save without editing
     anything was enough, so it broke on the *first* save on any Mac. Found in a
     real config after a save; `showSettings`'s `join` is the other half.

     Deliberately small: single and double quotes, no escapes, no variables. This
     is a command line a person types into a box, and `orchd` runs it through
     `proc::run_bounded` with an argv rather than a shell — so anything a shell
     would do beyond grouping would be a promise this cannot keep. */
  const argv = (s) => {
    const out = [];
    let cur = '';
    let quote = '';
    let has = false;
    for (const ch of s.trim()) {
      if (quote) {
        if (ch === quote) quote = '';
        else cur += ch;
      } else if (ch === '"' || ch === "'") {
        quote = ch;
        has = true;            // `""` is a real, empty argument
      } else if (/\s/.test(ch)) {
        if (cur || has) out.push(cur);
        cur = '';
        has = false;
      } else {
        cur += ch;
      }
    }
    if (cur || has) out.push(cur);
    return out;
  };
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
    window_transparent: !!ctl('settransparent').checked,
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
    await callShell('/api/window/restart');
  } catch (e) {
    // A browser tab has no window to restart, and the daemon says so. Then the
    // old sentence is the right one: it is saved, and it applies when you restart
    // it yourself.
    $('setnote').textContent = `saved, restart orchd to apply (${e.message})`;
  }
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
  setupTheme();
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
