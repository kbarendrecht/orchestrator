// The paths and URLs inside a line of terminal output, as offsets.
//
// **Its own module because it is the part with cases**, and a pure string
// function is one a node script can drive without a browser: `tools/check-pathlink.mjs`
// is the test that `term.js` could not have. Nothing in here knows about xterm,
// a workspace or a viewer — the caller turns an offset into a cell and a path
// into a file.
//
// What it is reading is an agent's prose, not a machine format. Claude Code
// writes `web/js/find.js:123`, `src/Foo.php:12:34`, a bare `Cargo.toml`, and any
// of them inside backticks, quotes, parentheses or at the end of a sentence. So
// the rule is a wide scan followed by a narrow accept: take every run that could
// be a path, strip what punctuation put around it, and then refuse what does not
// look like one.

/** A character a path can be made of. Deliberately no `:` — that is the line
 *  separator, and a path containing one is rarer than a sentence ending in one.
 *  `-` is in the set for the sake of file names and of the `124-129` range. */
const PATH_CHAR = /[A-Za-z0-9_./~@+\-\\]/;
/** Punctuation prose wraps a path in. Stripped from both ends, repeatedly. */
const AROUND = /^[`'"([{<]+|[`'"),.;:!?\]}>]+$/g;

/** Every path-like run in `text`, with where it sits and what it points at.
 *
 *  `start` and `end` are offsets into `text`, `end` exclusive, and they cover the
 *  path *and* its suffix — the whole thing is what you click. `last` is the end of
 *  a `:124-129` range and 0 when there is not one.
 *
 *  A URL is not a path and never comes back from here — [`urlsIn`] is the other
 *  half, and [`linksIn`] is what a caller offering both wants.
 *
 *  @param {string} text
 *  @returns {{ kind: 'path', start: number, end: number, path: string, line: number,
 *              last: number, col: number }[]}
 */
export function pathsIn(text) {
  /** @type {{ kind: 'path', start: number, end: number, path: string, line: number, last: number, col: number }[]} */
  const out = [];
  let i = 0;
  while (i < text.length) {
    if (!isRun(text[i] ?? '')) { i++; continue; }
    let j = i;
    while (j < text.length && isRun(text[j] ?? '')) j++;
    const found = read(text.slice(i, j));
    if (found) out.push({ ...found, kind: 'path', start: i + found.start, end: i + found.end });
    i = j;
  }
  return out;
}

/** A run is path characters plus the colons that may carry a line number. The
 *  colons are taken here and judged in `read`, because `foo.js:12` is one run to
 *  the eye and two to a scanner that stops at punctuation. */
const isRun = (/** @type {string} */ ch) => PATH_CHAR.test(ch) || ch === ':';

/** One candidate run, judged and split.
 *
 *  @param {string} run
 *  @returns {{ start: number, end: number, path: string, line: number, last: number,
 *              col: number } | null} */
function read(run) {
  const trimmed = run.replace(AROUND, '');
  if (!trimmed) return null;
  const start = run.indexOf(trimmed);
  /* The suffix, if the run ends in one. Taken from the right, so a Windows-style
     `C:` or a `http://` prefix cannot be mistaken for one.
     **Three shapes, because agents write all three**: `:12`, `:12:34` and the
     range `:124-129`, which is what Claude Code writes when it quotes a block. A
     range and a column cannot both follow, and nothing writes both. */
  const m = /^(.*?)(?::(\d+)(?:-(\d+))?)?(?::(\d+))?$/.exec(trimmed);
  if (!m) return null;
  const path = m[1] ?? '';
  const line = Number(m[2] ?? 0);
  const last = Number(m[3] ?? 0);
  const col = Number(m[4] ?? 0);
  if (!looksLikeAPath(path)) return null;
  return { start, end: start + trimmed.length, path, line, last, col };
}

/** Whether a string is worth offering as a file.
 *
 *  **The refusals are the useful half.** A URL is not this app's to open, a bare
 *  number pair is a time or a range, and `...` is an ellipsis somebody typed. A
 *  run with a slash in it is a path; one without needs an extension, or every
 *  ordinary word in a sentence would underline.
 *
 *  @param {string} s */
function looksLikeAPath(s) {
  if (!s || s.length > 512) return false;
  // A scheme means somebody meant the web, and this opens files.
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(s)) return false;
  // Nothing but dots and separators: `.`, `..`, `...`, `/`.
  if (/^[./\\]+$/.test(s)) return false;
  /* **A home path is refused rather than guessed at.** The page has no `$HOME` to
     expand `~` with, and joining it onto the pty's directory makes
     `<cwd>/~/.bashrc` — which passes every check after this one and names a file
     that does not exist. Only a leading one: a trailing `~` is an editor's backup
     and a real file. */
  if (s.startsWith('~')) return false;
  /* A slash is enough to be *asked about*, not to be a link: a branch
     (`chore/bump-deps`) and a slash command (`/implement`) have one too. The
     workspace's file list decides those, in [`matching`], because no rule over the
     text tells `chore/bump-deps` from `bin/orchd`. */
  if (s.includes('/')) return true;
  // No directory, so it has to name itself: a dot, then an extension of letters
  // and digits. `find.js` yes, `v1.2` no, `Makefile` no — a path with neither a
  // slash nor an extension is indistinguishable from a word.
  const named = /^([^.][^/\\]*)\.([A-Za-z][A-Za-z0-9]{0,15})$/.exec(s);
  if (!named) return false;
  /* **`e.g` is the one that gets through everything above**, and `i.e` with it:
     one letter, a dot, one letter is the shape of an abbreviation and of nothing
     anybody names a file. So a single-letter extension needs a real stem in front
     of it — `main.c` keeps working, `a.c` is lost, and losing `a.c` is cheaper
     than underlining every "e.g." an agent writes. */
  return (named[1] ?? '').length > 1 || (named[2] ?? '').length > 1;
}

/** A URL the daemon will hand to the browser, and where it sits.
 *
 *  **`http` and `https` only, because `/api/open` accepts nothing else.** Offering
 *  `ftp://` or `mailto:` would draw an underline the daemon then refuses, and a
 *  link that does nothing is worse than text that never looked like one.
 *
 *  **Its own scan rather than the run scanner above**, and that is the whole
 *  reason this is not three lines inside [`pathsIn`]. A run stops at the first
 *  character `PATH_CHAR` does not hold, so `?`, `=`, `&`, `#` and `%` all end it —
 *  and a URL truncated at its query string is not a shorter URL, it is a different
 *  page. This takes everything up to whitespace and then gives the punctuation
 *  back.
 *
 *  @param {string} text
 *  @returns {{ kind: 'url', start: number, end: number, url: string }[]}
 */
export function urlsIn(text) {
  /** @type {{ kind: 'url', start: number, end: number, url: string }[]} */
  const out = [];
  for (const m of text.matchAll(URL_RUN)) {
    const url = trimTail(m[0]);
    // A scheme and nothing after it is not somewhere to go.
    if (!/^https?:\/\/[^\s/]/i.test(url)) continue;
    out.push({ kind: 'url', url, start: m.index ?? 0, end: (m.index ?? 0) + url.length });
  }
  return out;
}

/** Whitespace and the few characters that can only be markup around a URL, never
 *  in one. The brackets and quotes are left to [`trimTail`], which knows which end
 *  they are on. */
const URL_RUN = /https?:\/\/[^\s<>"'`]+/gi;
/** Punctuation a sentence puts after a URL. Stripped repeatedly, from the end
 *  only — a leading `(` is never in the match, because the scan starts at `http`. */
const URL_TAIL = /[.,;:!?'"`>\]}]+$/;

/** A matched run with the sentence's own punctuation given back.
 *
 *  **The closing parenthesis is the one that needs counting.** `(see https://x/a)`
 *  ends in a `)` that belongs to the prose, and
 *  `https://en.wikipedia.org/wiki/Bar_(unit)` ends in one that belongs to the URL.
 *  The rule that tells them apart is whether the URL opened a parenthesis of its
 *  own, which is what every linkifier worth copying does.
 */
function trimTail(/** @type {string} */ run) {
  let url = run.replace(URL_TAIL, '');
  const count = (/** @type {string} */ ch) => url.split(ch).length - 1;
  while (url.endsWith(')') && count(')') > count('(')) {
    // Again after the bracket, for `(https://x/a).` — the dot was behind the `)`.
    url = url.slice(0, -1).replace(URL_TAIL, '');
  }
  return url;
}

/** Everything in `text` worth offering as a link, in the order it is written.
 *
 *  **A URL wins every overlap, and that is not a tie-breaker — it is the fix for a
 *  real defect.** The run scanner sees `v` and `a.js` inside
 *  `https://x/y?file=a.js`, and `a.js` passes every path test there is: without
 *  this the middle of a URL underlines as a file that does not exist, while the
 *  URL around it does not underline at all.
 *
 *  @param {string} text
 *  @returns {({ kind: 'path', start: number, end: number, path: string, line: number,
 *               last: number, col: number }
 *           | { kind: 'url', start: number, end: number, url: string })[]}
 */
export function linksIn(text) {
  const urls = urlsIn(text);
  const paths = pathsIn(text).filter((p) => !urls.some((u) => p.start < u.end && u.start < p.end));
  return [...urls, ...paths].sort((a, b) => a.start - b.start);
}

/** The files in `list` that `path` names, best first. `list` is a workspace's
 *  files, relative to its root, as `/api/paths` answers.
 *
 *  **An agent names a file, not a path**, so this matches on the tail: a bare name
 *  and a partial path are one rule. Several matches are the normal shape of a
 *  large repo rather than an error, and the caller asks which one was meant.
 *
 *  Here rather than in the viewer because the underline asks it too: a link is
 *  drawn only where the click would find something, and one function for both is
 *  what keeps the two from disagreeing.
 *
 *  @param {string[]} list
 *  @param {string} path workspace-relative
 *  @returns {string[]} */
export function matching(list, path) {
  if (list.includes(path)) return [path];
  const tail = `/${path}`;
  return list
    .filter((p) => p.endsWith(tail))
    // Shortest first: a path with less in it that the name did not ask for is the
    // likelier answer, and it is the tie-break the name ranking already uses.
    .sort((a, b) => a.length - b.length || a.localeCompare(b));
}
