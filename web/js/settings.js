// The settings panel. The zoom control it offers lives in core, because the
// terminals read the scale too.

import { ctl, $, ZOOM, call, el, get, saveZoom, setZoom, snap, zoomScale } from './core.js';

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
  ctl('setlang').value = cfg.output_language || '';
  ctl('settracker').value = cfg.tracker || 'none';
  ctl('setupref').value = cfg.upstream_ref || '';
  ctl('setupremote').value = cfg.upstream_remote || '';
  ctl('setreviews').value = (cfg.reviews_command || []).join(' ');
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
    fold.appendChild(el('span', 'caretr', '\u203a'));
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
    output_language: ctl('setlang').value.trim(),
    tracker: ctl('settracker').value,
    upstream_ref: ctl('setupref').value.trim(),
    upstream_remote: ctl('setupremote').value.trim(),
    reviews_command: argv(ctl('setreviews').value),
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
    $('setnote').textContent = 'saved — restart orchd to apply';
  } catch (e) {
    $('setnote').textContent = e.message;
  }
}

function setupSettings() {
  setZoom(Number(localStorage.getItem(ZOOM.key)) || ZOOM.def);

  $('gearbtn').onclick = (ev) => {
    ev.stopPropagation();
    if (settingsOpen()) closeSettings();
    else openSettings();
  };
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

  /* A click on another pane puts it away; a click anywhere inside this one does
     not. The scrim clause that used to be here was right while settings floated
     over the window as a modal — its own backdrop was the way out. It fills the
     centre column now, so that backdrop is just the empty half of a form, and
     missing an input closed the panel. */
  document.addEventListener('mousedown', (e) => {
    if (!settingsOpen()) return;
    if (!(/** @type {HTMLElement} */ (e.target)).closest('#settings, #gearbtn')) closeSettings();
  }, true);
}

export { settingsOpen as isOpen, closeSettings as close, setupSettings as setup };
