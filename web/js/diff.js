// The diff viewer, its editable right pane, and the changed-files list that
// drives it. One module because the three call each other; splitting them would
// only have turned that into circular imports.

import { $, activeWorkspaceId, call, currentSession, currentWorkspaceId, el, get, openMenu, pending, prForWorkspace, snap, toast } from './core.js';


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
function renderDivergence(w) {
  const box = $('diverge');
  box.replaceChildren();
  // Reset the class too, not just the children: `on` is what makes this visible,
  // and leaving it behind left an empty bar above the file list.
  box.className = 'diverge';
  if (!w) return;

  if (w.rebasing) {
    box.className = 'diverge on bad';
    box.appendChild(el('span', 'dvtext', 'rebase stopped on conflicts'));
    const a = el('button', 'dvbtn', 'Abort');
    a.onclick = () => act(`/api/workspace/${encodeURIComponent(w.id)}/rebase/abort`, 'aborted');
    box.appendChild(a);
    return;
  }
  if (!w.behind) {
    box.className = 'diverge';
    return;
  }

  box.className = 'diverge on';
  const ahead = w.ahead ? `, ${w.ahead} ahead` : '';
  const base = snap.upstream_ref || 'the base';
  box.appendChild(el('span', 'dvtext',
    `${w.behind} behind ${base}${ahead}`));
  const b = el('button', 'dvbtn', 'Rebase');
  // Never a merge: history stays linear.
  b.title = `git rebase ${base} in ${w.id}`;
  b.onclick = async () => {
    b.disabled = true;
    await act(`/api/workspace/${encodeURIComponent(w.id)}/rebase`, 'rebased');
    b.disabled = false;
  };
  box.appendChild(b);
}

async function act(path, verb) {
  try {
    const r = await call(path);
    toast(verb);
    // An endpoint can succeed and still have something to say — a rebase onto a
    // base whose fetch failed, say. Surface it as its own warning toast, the way
    // openArchived and forkSession already do.
    if (r && r.warning) toast(r.warning, true);
  } catch (e) {
    toast(e.message, true);
  }
}

/** Open a changed file on the forge.
 *
 *  The URL is built by the daemon, not here: which ref to read the file at and
 *  what a blob URL looks like are both the forge's business, and a second forge
 *  is meant to be a `ForgeKind` arm rather than an edit to this file. So the
 *  client says only which file it wants. */
function openFileOnForge(w, path) {
  call('/api/open/file', { workspace: w.id, path })
    .catch((err) => toast(err.message, true));
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

function renderFiles() {
  // The diff overlay is opened against a workspace and keeps describing it while
  // it is open, session or no session.
  const wsId = diffState.open ? diffState.ws : activeWorkspaceId();
  const w = snap.workspaces.find((x) => x.id === wsId);
  renderDivergence(w);
  const panes = $('filepanes');
  panes.replaceChildren();

  $('filestitle').textContent = diffState.open ? 'Changeset' : 'Changes';

  if (!w) {
    const s = currentSession();
    panes.appendChild(el('div', 'fempty', s && pending(s)
      ? 'Creating the worktree…'
      : 'No session open.'));
    $('filesfoot').textContent = '';
    $('filesbase').textContent = '';
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
  const files = sum ? sum.files : (w.changed || []);
  const since = sum ? sum.base : w.changed_since;

  for (const f of files) {
    const row = el('button', sum ? 'dfrow' : 'frow');
    if (sum) row.setAttribute('aria-current', String(f.path === diffState.path));
    const letter = (f.status || 'M')[0];
    row.appendChild(el('span', 'fst ' + letter, letter));
    const n = el('span', 'fname');
    n.textContent = '\u202a' + f.path;
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
        loadFile(f.path);
      } else {
        openDiff(f.path);
      }
    };
    // These rows carry no menu of their own, and the one thing worth reaching
    // for is the file on the forge. Whether it can be linked is the daemon's
    // answer, so the item is always live and a refusal comes back as a toast.
    row.oncontextmenu = (ev) => {
      openMenu(ev, [['open on forge', null, () => openFileOnForge(w, f.path)]]);
    };
    panes.appendChild(row);
  }

  /* **"Nothing changed" and "not counted yet" are different sentences.** Every
     field this pane reads defaults to a value that looks like a real answer — no
     files, zero changed, no base — and the daemon's first sweep used to finish
     before the window opened, so the difference could not be seen. It runs in the
     background now, so an unmeasured worktree would render as a clean one. The
     daemon says which this is (`measured`); the loader is only ever the honest
     half of that.

     Not while the diff overlay is open: `sum` is its own fetched summary, which
     is measured by definition. */
  if (!files.length) {
    panes.appendChild(sum || w.measured
      ? el('div', 'fempty',
        w.is_main ? 'Nothing changed in the main checkout.' : 'Nothing changed in this worktree yet.')
      : counting());
  }

  // "500 of 5,214" when the daemon capped the list, plain count otherwise. Said
  // rather than left to look complete: a truncation presented as the whole answer
  // is the one thing a changed-file pane must not do, and a wiped repository is
  // exactly when you are reading it.
  //
  // A count of something not yet counted is the same fault with a different
  // cause, so the footer says so instead of showing a confident nothing.
  const total = sum ? files.length : (w.changed_total ?? files.length);
  const bits = [total > files.length
    ? `${files.length} of ${total.toLocaleString()} files`
    : `${files.length} file${files.length === 1 ? '' : 's'}`];
  if (sum) bits.push(`+${sum.added} \u2212${sum.deleted}`);
  if (w.is_main) bits.push('worktrees excluded');
  $('filesfoot').textContent = sum || w.measured ? bits.join(' \u00b7 ') : 'counting\u2026';
  // The base belongs in the header, where the toggle used to be: it is the one
  // thing you need to know to read the list, and it is not a choice.
  $('filesbase').textContent = since ? `since ${since.slice(0, 7)}` : '';
}

// Kept short: the right header also carries the title and the refresh control,
// and a long label wraps it onto two lines.
const diffState = {
  open: false,
  /* Which workspace the open diff describes. Every fetch used to read the
   * current one at call time, so switching sessions left the loaded hunks
   * describing the old worktree while the next request quietly asked about the
   * new one. Pinned here, and re-pointed by `syncDiffToSession`. */
  ws: null,
  base: 'upstream',
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
};

/* Byte offsets come from Rust; JS strings are UTF-16. Decode through the byte
   array rather than assuming ASCII, or a line with an accent in it highlights
   the wrong span. */
const ENC = new TextEncoder();
const DEC = new TextDecoder();

/* Prism is vendored whole (every grammar) and driven for its token stream only,
 * never its markup: the daemon already marks the changed slices of a line, and
 * those `.w-add`/`.w-del` ranges have to interleave with the syntax spans rather
 * than nest inside them. So highlighting is flattened to ranges and merged with
 * the word ranges below. Language is the file's extension; anything unmapped —
 * or a grammar Prism does not carry — falls back to plain text, never an error. */
const EXT_LANG = {
  js: 'javascript', mjs: 'javascript', cjs: 'javascript', jsx: 'jsx',
  ts: 'typescript', tsx: 'tsx', rs: 'rust', py: 'python', rb: 'ruby', go: 'go',
  c: 'c', h: 'c', cpp: 'cpp', cc: 'cpp', cxx: 'cpp', hpp: 'cpp', cs: 'csharp',
  java: 'java', kt: 'kotlin', swift: 'swift', php: 'php', sh: 'bash', bash: 'bash',
  zsh: 'bash', fish: 'bash', css: 'css', scss: 'scss', sass: 'sass', less: 'less',
  html: 'markup', htm: 'markup', xml: 'markup', svg: 'markup', vue: 'markup',
  json: 'json', yaml: 'yaml', yml: 'yaml', toml: 'toml', ini: 'ini', cfg: 'ini',
  md: 'markdown', markdown: 'markdown', sql: 'sql', graphql: 'graphql', gql: 'graphql',
  lua: 'lua', pl: 'perl', r: 'r', dart: 'dart', scala: 'scala', clj: 'clojure',
  ex: 'elixir', exs: 'elixir', erl: 'erlang', hs: 'haskell', ml: 'ocaml',
  tf: 'hcl', hcl: 'hcl', proto: 'protobuf', diff: 'diff', patch: 'diff',
  vim: 'vim', nix: 'nix', zig: 'zig', jl: 'julia', groovy: 'groovy', gradle: 'groovy',
};
const BASENAME_LANG = {
  dockerfile: 'docker', makefile: 'makefile', 'cargo.lock': 'toml',
  'go.mod': 'go', 'go.sum': 'go',
};
function langFor(path) {
  if (!path || !window.Prism) return null;
  const base = path.split('/').pop().toLowerCase();
  const byName = BASENAME_LANG[base];
  if (byName) return Prism.languages[byName] ? byName : null;
  const dot = base.lastIndexOf('.');
  const lang = EXT_LANG[dot >= 0 ? base.slice(dot + 1) : ''];
  return lang && Prism.languages[lang] ? lang : null;
}

/** Prism's nested token tree, flattened to non-overlapping `{s,e,cls}` ranges in
 *  character offsets. The deepest token wins, which is what falls out of only
 *  emitting a range at each string leaf. */
function hlTokens(text, lang) {
  if (!lang) return [];
  let tree;
  try { tree = Prism.tokenize(text, Prism.languages[lang]); }
  catch (e) { return []; }
  const out = [];
  let pos = 0;
  (function walk(arr, inherited) {
    for (const t of arr) {
      if (typeof t === 'string') {
        if (inherited) out.push({ s: pos, e: pos + t.length, cls: inherited });
        pos += t.length;
      } else {
        const ty = (t.alias && (Array.isArray(t.alias) ? t.alias[0] : t.alias)) || t.type;
        if (typeof t.content === 'string') {
          out.push({ s: pos, e: pos + t.content.length, cls: ty });
          pos += t.content.length;
        } else {
          walk(t.content, ty);
        }
      }
    }
  })(tree, null);
  return out;
}

/** Split a line at every boundary — syntax-token edges and word-diff edges both
 *  — so each segment can carry a token colour and a change background at once. */
function lineSegments(text, words, lang) {
  const toks = hlTokens(text, lang);
  const bset = new Set([0, text.length]);
  for (const t of toks) { bset.add(t.s); bset.add(t.e); }
  for (const w of words) { bset.add(w.s); bset.add(w.e); }
  const pts = [...bset].filter((p) => p >= 0 && p <= text.length).sort((a, b) => a - b);
  const segs = [];
  for (let k = 0; k < pts.length - 1; k++) {
    const s = pts[k], e = pts[k + 1];
    if (s === e) continue;
    const tok = toks.find((t) => t.s <= s && t.e >= e);
    const word = words.some((w) => w.s <= s && w.e >= e);
    segs.push({ s, e, cls: tok ? tok.cls : null, word });
  }
  return segs;
}

/** The open-question detail is usually a commit diff and a reply, with no file to
 *  name a language from, so it is coloured as a *diff*: whole +/- lines, headers
 *  neutral. Prism's diff grammar is line-aware — a `---`/`+++` header is `coord`,
 *  not a deletion, so the `--- the reply ---` separator does not read as removed.
 *  Only the top-level (per-line) token is taken, so the whole line is coloured
 *  rather than the sign alone. Non-diff prose has no diff tokens and stays plain. */
function tokenLen(x) {
  if (typeof x === 'string') return x.length;
  if (Array.isArray(x)) return x.reduce((a, c) => a + tokenLen(c), 0);
  return tokenLen(x.content);
}
function diffRanges(text) {
  if (!window.Prism || !Prism.languages.diff) return [];
  let toks;
  try { toks = Prism.tokenize(text, Prism.languages.diff); }
  catch (e) { return []; }
  const out = [];
  let pos = 0;
  for (const t of toks) {
    if (typeof t === 'string') { pos += t.length; continue; }
    const ty = (t.alias && (Array.isArray(t.alias) ? t.alias[0] : t.alias)) || t.type;
    const len = tokenLen(t);
    out.push({ s: pos, e: pos + len, cls: ty });
    pos += len;
  }
  return out;
}
function detailEl(text) {
  const pre = el('pre', 'oqd');
  const ranges = diffRanges(text);
  if (!ranges.length) { pre.textContent = text; return pre; }
  let at = 0;
  for (const r of ranges) {
    if (r.s > at) pre.appendChild(document.createTextNode(text.slice(at, r.s)));
    pre.appendChild(el('span', 'tok-' + r.cls, text.slice(r.s, r.e)));
    at = r.e;
  }
  if (at < text.length) pre.appendChild(document.createTextNode(text.slice(at)));
  return pre;
}

function lineEl(row, side) {
  // side: 'old' | 'new'. In split view each pane shows only its own side.
  const empty = !row || (side === 'old' && row.kind === 'add') ||
                        (side === 'new' && row.kind === 'del');
  const div = el('div', 'ln' + (empty ? ' empty' : row.kind === 'add' ? ' add' : row.kind === 'del' ? ' del' : ''));
  const num = el('i', null, empty ? '' : String((side === 'old' ? row.old : row.new) ?? ''));
  div.appendChild(num);
  const body = el('s');
  if (!empty) {
    // Word ranges arrive as byte offsets (from Rust); Prism works on the JS
    // string. Convert the ranges to character offsets so the two line up, then
    // merge. A blank line still needs a space so the row has height.
    const bytes = ENC.encode(row.text);
    const b2c = (b) => DEC.decode(bytes.slice(0, b)).length;
    const words = (row.words || []).map(([ws, we]) => ({ s: b2c(ws), e: b2c(we) }));
    const segs = lineSegments(row.text, words, langFor(diffState.path));
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
function pairRows(rows) {
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

function renderDiff() {
  const body = $('diffbody');
  body.replaceChildren();
  body.className = 'diff' + (diffState.split ? ' split' : '');
  const f = diffState.file;

  $('ovpath').replaceChildren();
  if (diffState.path) {
    const parts = diffState.path.split('/');
    const name = parts.pop();
    $('ovpath').appendChild(el('span', null, parts.length ? parts.join('/') + '/' : ''));
    $('ovpath').appendChild(document.createTextNode(name));
  }
  $('ovmode').textContent = diffState.split ? 'Unified' : 'Split';

  const note = (t) => {
    body.appendChild(el('div', 'diffnote', t));
    $('ovcount').textContent = '';
    diffState.anchors = [];
  };
  if (diffState.loading) return note('reading the diff…');
  if (!f) return note('Select a file.');
  if (f.binary) return note('Binary file — not shown.');
  if (!f.hunks.length) return note('No textual changes against this base.');

  const anchors = [];
  let block = -1;

  // Every row is three grid cells in split view and one in unified, so a fold
  // spanning the full width interleaves naturally between hunks.
  const push3 = (a, b) => {
    body.appendChild(a);
    body.appendChild(el('div', 'gutter'));
    body.appendChild(b);
  };

  for (const h of f.hunks) {
    if (h.gap_before > 0) {
      const b = el('div', 'fold', `⋯ ${h.gap_before} unchanged lines — click to expand`);
      b.onclick = () => {
        diffState.context = Math.min(diffState.context + Math.max(h.gap_before, 20), 10000);
        loadFile(diffState.path);
      };
      body.appendChild(b);
    }

    // The hunk's section heading — git's text after the second @@, the function
    // or class the change sits in — pinned so it stays in view while you scroll a
    // long hunk, the way the review overlay's hunk header already does. Empty for
    // a top-of-file hunk, so it is only drawn when it says something.
    const section = (h.header && h.header.split('@@')[2] || '').trim();
    if (section) body.appendChild(el('div', 'diffhh', section));

    if (diffState.split) {
      let splitInBlock = false;
      for (const [o, n] of pairRows(h.rows)) {
        const lo = lineEl(o, 'old');
        const ro = lineEl(n, 'new');
        const changed = o?.kind === 'del' || n?.kind === 'add';
        if (changed) {
          if (!splitInBlock) { block += 1; anchors.push(lo); splitInBlock = true; }
          lo.dataset.blk = ro.dataset.blk = String(block);
        } else {
          splitInBlock = false;
        }
        push3(lo, ro);
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

  diffState.anchors = anchors;
  const last = Math.max(anchors.length - 1, 0);
  diffState.cursor = diffState.pendingCursor === 'last' ? last
    : diffState.pendingCursor === 'first' ? 0
      : Math.min(diffState.cursor, last);
  diffState.pendingCursor = null;
  markCursor();
}

function markCursor() {
  for (const e of $('diffbody').querySelectorAll('.ln.cur')) e.classList.remove('cur');
  const a = (diffState.anchors || [])[diffState.cursor];
  if (!a) return;
  a.classList.add('cur');
  a.scrollIntoView({ block: 'center', behavior: 'smooth' });
  // Within-file position, plus which file of the changeset when there is more
  // than one — the stepper walks the whole PR, so "3 of 7" alone would not say
  // where in it you are.
  const files = diffState.summary?.files || [];
  const fi = files.findIndex((f) => f.path === diffState.path);
  const where = files.length > 1 && fi >= 0 ? ` · file ${fi + 1} of ${files.length}` : '';
  $('ovcount').textContent = `change ${diffState.cursor + 1} of ${diffState.anchors.length}${where}`;
}

/** Walk to the next/previous change block, carrying on into the next file in the
 *  changeset's order rather than wrapping inside the current one. Files with no
 *  change blocks (binary, or nothing textual) are hopped over, and the whole
 *  changeset wraps end to end so the stepper never dead-ends. */
async function stepChange(delta) {
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
    toast(e.message, true);
  }
  renderFiles();
}

async function loadFile(path) {
  const ws = diffState.ws || activeWorkspaceId();
  if (!ws) return;
  if (editState.on && path !== editState.path && !closeEditor()) return;
  diffState.path = path;
  const q = new URLSearchParams({
    workspace: ws, base: diffState.base, path, context: String(diffState.context),
  });
  const pr = prForWorkspace(ws);
  if (pr && pr.base_ref) q.set('pr_base', pr.base_ref);
  // Show "reading the diff…" only if the fetch outlasts a couple of frames, so a
  // fast local diff never flashes it and a slow one stops reading as "did nothing"
  // by leaving the previous file's hunks on screen.
  const slow = setTimeout(() => { diffState.loading = true; renderDiff(); }, 150);
  try {
    diffState.file = await get(`/api/diff/file?${q}`);
  } catch (e) {
    diffState.file = null;
    toast(e.message, true);
  }
  clearTimeout(slow);
  diffState.loading = false;
  renderDiff();
  renderFiles();
}

async function openDiff(path) {
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
  $('overlay').classList.add('on');
  await loadSummary();
  const first = path || diffState.summary?.files?.[0]?.path;
  if (first) {
    diffState.cursor = 0;
    await loadFile(first);
  } else {
    renderDiff();
  }
}

function closeDiff() {
  if (editState.on && !closeEditor()) return;
  diffState.open = false;
  diffState.ws = null;
  diffState.file = null;
  diffState.path = null;
  $('overlay').classList.remove('on');
  renderFiles();
}




// ---------------------------------------------------------------------------
// Editable right pane (§5, step 9)
// ---------------------------------------------------------------------------

const editState = {
  on: false,
  path: null,
  version: null,     // what the buffer was loaded at
  dirty: false,
  watch: null,       // polls for someone editing underneath you
};

function editQuery(extra) {
  const ws = diffState.ws || activeWorkspaceId();
  const q = new URLSearchParams({ workspace: ws, path: diffState.path, ...extra });
  const pr = prForWorkspace(ws);
  if (pr && pr.base_ref) q.set('pr_base', pr.base_ref);
  return q;
}

async function openEditor() {
  if (!diffState.path || !diffState.file || diffState.file.binary) {
    return toast('nothing editable here', true);
  }
  let live, base;
  try {
    [live, base] = await Promise.all([
      get(`/api/file?${editQuery({})}`),
      get(`/api/file?${editQuery({ base: diffState.base })}`),
    ]);
  } catch (e) {
    return toast(e.message, true);
  }

  editState.on = true;
  editState.path = diffState.path;
  editState.version = live.version;
  editState.dirty = false;
  $('ovsave').hidden = false;
  $('ovedit').textContent = 'Cancel';

  const body = $('diffbody');
  body.replaceChildren();
  body.className = 'diff split editing';

  // The left pane stays the base revision, read-only: this is an editable
  // right pane, not a free-floating text editor.
  const left = el('pre', 'editbase');
  left.textContent = base.content;
  body.appendChild(left);
  body.appendChild(el('div', 'gutter'));

  const ta = el('textarea', 'editarea');
  ta.value = live.content;
  ta.spellcheck = false;
  ta.oninput = () => {
    editState.dirty = true;
    $('ovsave').textContent = 'Save •';
  };
  body.appendChild(ta);
  ta.focus();

  // Invalidation: an agent editing the same file underneath you must not be
  // discovered only at save time (§5).
  clearInterval(editState.watch);
  editState.watch = setInterval(checkUnderneath, 4000);
}

async function checkUnderneath() {
  if (!editState.on) return;
  try {
    const now = await get(`/api/file?${editQuery({})}`);
    if (now.version !== editState.version) {
      clearInterval(editState.watch);
      editState.watch = null;
      $('ovsave').textContent = 'Save (conflict)';
      toast('this file changed on disk — an agent is editing it too. Saving will be refused.', true);
    }
  } catch (e) {
    // A file that vanished is also a change worth knowing about, but not worth
    // a second alarm; the save will report it.
  }
}

function closeEditor(silent) {
  if (editState.on && editState.dirty && !silent
      && !confirm('Discard unsaved edits?')) return false;
  clearInterval(editState.watch);
  editState.watch = null;
  editState.on = false;
  editState.dirty = false;
  $('ovsave').hidden = true;
  $('ovsave').textContent = 'Save ⌘S';
  $('ovedit').textContent = 'Edit';
  renderDiff();
  return true;
}

async function saveEditor() {
  if (!editState.on) return;
  const ta = $('diffbody').querySelector('.editarea');
  if (!ta) return;
  let out;
  try {
    out = await call('/api/file', {
      workspace: currentWorkspaceId(),
      path: editState.path,
      content: /** @type {HTMLTextAreaElement} */ (ta).value,
      version: editState.version,
    });
  } catch (e) {
    return toast(e.message, true);
  }
  if (out.result === 'conflict') {
    return toast(
      'refused: the file changed on disk since you opened it. Cancel and reopen to see their version.',
      true);
  }
  editState.version = out.version;
  editState.dirty = false;
  $('ovsave').textContent = 'Save ⌘S';
  toast('saved');
  // Re-diff so the changeset reflects the write.
  await loadSummary();
  const q = editQuery({ context: String(diffState.context) });
  try {
    diffState.file = await get(`/api/diff/file?${q}`);
  } catch (e) {
    /* the editor is still the source of truth on screen */
  }
}

/* `langFor`/`hlTokens` are exported for the review cards' hunks, which want the
   same colours as the viewer. Exported rather than copied: the extension table and
   the token-flattening are the fiddly parts, and two of either would drift. */
export { diffState as state, editState as edit, openDiff as open, closeDiff as close, renderDiff as render, stepChange as step, loadFile, detailEl, renderFiles, openEditor, closeEditor, saveEditor, langFor, hlTokens };
