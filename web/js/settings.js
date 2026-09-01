// The settings panel. The zoom control it offers lives in core, because the
// terminals read the scale too.

import { ctl, $, ZOOM, call, caret, closeLegend, el, get, MOD_LABEL, saveZoom, setZoom, snap, zoomScale } from './core.js';

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
  ctl('settracker').value = cfg.tracker || 'none';
  ctl('setupref').value = cfg.upstream_ref || '';
  ctl('setupremote').value = cfg.upstream_remote || '';
  ctl('setreviews').value = (cfg.reviews_command || []).join(' ');
  ctl('setwtsetup').value = (cfg.worktree_setup || []).join(' ');
  procDraft = (cfg.main_processes || []).map((p) => ({
    name: p.name || '',
    command: (p.command || []).join(' '),
    ok_patterns: (p.ok_patterns || []).join(', '),
    failure_patterns: (p.failure_patterns || []).join(', '),
    restart: p.restart || 'never',
    autostart: !!p.autostart,
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
    tracker: ctl('settracker').value,
    upstream_ref: ctl('setupref').value.trim(),
    upstream_remote: ctl('setupremote').value.trim(),
    reviews_command: argv(ctl('setreviews').value),
    worktree_setup: argv(ctl('setwtsetup').value),
    main_processes: procDraft.map((p) => ({
      name: p.name.trim(),
      command: argv(p.command),
      failure_patterns: list(p.failure_patterns),
      ok_patterns: list(p.ok_patterns),
      restart: p.restart,
      autostart: p.autostart,
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
    await call('/api/window/restart');
  } catch (e) {
    // A browser tab has no window to restart, and the daemon says so. Then the
    // old sentence is the right one: it is saved, and it applies when you restart
    // it yourself.
    $('setnote').textContent = `saved, restart orchd to apply (${e.message})`;
  }
}

function setupSettings() {
  setZoom(Number(localStorage.getItem(ZOOM.key)) || ZOOM.def);

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
  $('setclose').onclick = () => closeSettings();

  $('setprocadd').onclick = () => {
    procDraft.push({
      name: '', command: '', ok_patterns: '', failure_patterns: '',
      restart: 'never', autostart: false, open: true,
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

export { settingsOpen as isOpen, closeSettings as close, setupSettings as setup };
