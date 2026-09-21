#!/usr/bin/env node
// What `web/js/pathlink.js` must and must not offer as a file.
//
//   node tools/check-pathlink.mjs
//
// **The test `term.js` could not have.** The matcher runs inside a terminal, over
// an agent's prose, and every one of its cases is a string — so it lives in a
// module with no imports and is driven here, in node, rather than through a
// browser, a daemon and a pty.
//
// The refusals carry the weight. A matcher that is merely generous underlines
// half of every sentence Claude Code writes, and an affordance that is wrong four
// times out of five is one people learn to ignore.

import { pathsIn } from '../web/js/pathlink.js'

let failed = false
const check = (ok, what) => {
  console.log(`  ${ok ? 'ok  ' : 'FAIL'}  ${what}`)
  if (!ok) failed = true
}

/** The one path in `text`, as `path:line:col`, or what went wrong. */
const one = (text) => {
  const found = pathsIn(text)
  if (found.length !== 1) return `${found.length} matches`
  const f = found[0]
  return `${f.path}:${f.line}:${f.col}`
}

/** The same, with the range's end in it. */
const span = (text) => {
  const [f] = pathsIn(text)
  return f ? `${f.path}:${f.line}-${f.last}` : null
}

/** That the offsets cover exactly the path and its suffix, which is what decides
 *  where the underline is drawn. */
const underlined = (text) => {
  const [f] = pathsIn(text)
  return f ? text.slice(f.start, f.end) : null
}

// --- what an agent actually writes ------------------------------------------

check(one('web/js/find.js:123') === 'web/js/find.js:123:0', 'a path with a line')
check(one('src/Foo.php:12:34') === 'src/Foo.php:12:34', 'a path with a line and a column')
check(one('see web/js/find.js for it') === 'web/js/find.js:0:0', 'a path with neither')
check(one('/home/me/repo/src/main.rs:8') === '/home/me/repo/src/main.rs:8:0', 'an absolute path')
check(one('Cargo.toml') === 'Cargo.toml:0:0', 'a bare filename, because it carries an extension')

/* The range, which is what an agent writes when it quotes a block of a file. */
check(span('see overlay.service.ts:124-129 there') === 'overlay.service.ts:124-129',
  'a line range, both ends')
check(span('web/js/find.js:12') === 'web/js/find.js:12-0', 'and a plain line has no end')
check(underlined('see overlay.service.ts:124-129 there') === 'overlay.service.ts:124-129',
  'the underline covers the range, not just the path')

// --- and how prose wraps it -------------------------------------------------

check(underlined('read `web/app.css` first') === 'web/app.css', 'backticks are not part of the path')
check(underlined('in (web/app.css:9) there') === 'web/app.css:9', 'nor are parentheses')
check(underlined('web/app.css, then term.js') === 'web/app.css', 'nor is a trailing comma')
check(underlined('it is in web/app.css.') === 'web/app.css', 'nor a full stop that ends the sentence')
check(underlined('"web/js/core.js":') === 'web/js/core.js', 'nor quotes with a colon after them')

// --- the refusals -----------------------------------------------------------

check(pathsIn('https://example.com/a/b').length === 0, 'a URL is not this app\'s to open')
check(pathsIn('at 12:34 yesterday').length === 0, 'a time is not a path')
check(pathsIn('and so on ... done').length === 0, 'an ellipsis is not a path')
check(pathsIn('the word file is not one').length === 0, 'a word with no slash and no extension')
check(pathsIn('version v1.2 shipped').length === 0, 'a version number is not a filename')
check(pathsIn('/ and . and ..').length === 0, 'separators alone name nothing')

/* The three a review found, each of which had reached the page. `e.g` and `i.e`
   are the shape of an abbreviation and of nothing anybody names a file; `~` is a
   path the page cannot expand, and joining it onto the pty's directory makes
   `<cwd>/~/.bashrc`, which passes every check after the matcher and names a file
   that is not there. */
check(pathsIn('see e.g. the rail').length === 0, 'e.g. is an abbreviation, not a file')
check(pathsIn('i.e. that one').length === 0, 'and so is i.e.')
check(pathsIn('open ~/.bashrc now').length === 0, 'a home path has no $HOME here to expand')
check(one('edit main.c here') === 'main.c:0:0', 'but a one-letter extension on a real stem stays')
check(one('see a/b/c.h line') === 'a/b/c.h:0:0', 'and a slash answers for itself')

// --- several in one line ----------------------------------------------------

const many = pathsIn('moved web/js/a.js to web/js/b.js:4')
check(
  many.length === 2 && many[0].path === 'web/js/a.js' && many[1].line === 4,
  `two paths in one line, got ${JSON.stringify(many.map((m) => m.path))}`,
)

console.log(failed ? '\ncheck-pathlink: FAILED' : '\ncheck-pathlink: ok')
process.exit(failed ? 1 : 0)
