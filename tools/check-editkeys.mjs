#!/usr/bin/env node
// What the editor's Tab, Enter and comment chord write.
//
//   node tools/check-editkeys.mjs
//
// Each case is text, a selection and the text that must come out. The
// selection after the edit is checked too, because an indent that loses your
// selection is one you cannot press twice.

import { commentFor, indent, indentUnit, newline, toggleComment } from '../web/js/editkeys.js'

let failed = false
const check = (ok, what) => {
  console.log(`  ${ok ? 'ok  ' : 'FAIL'}  ${what}`)
  if (!ok) failed = true
}
const apply = (text, ed) => text.slice(0, ed.from) + ed.text + text.slice(ed.to)

check(indentUnit('a\n    b\n        c\n') === '    ', 'four spaces is the unit when that is the smallest')
check(indentUnit('/**\n * doc\n */\nfn x() {\n  y\n}\n') === '  ', 'a doc star margin is not the unit')
check(indentUnit('a\n\tb\n') === '\t', 'tabs are tabs')
check(indentUnit('no indent at all') === '  ', 'nothing to go on is two spaces')

{
  const t = 'a\nb\nc\n'
  const ed = indent(t, 0, 3, '  ', false)
  check(apply(t, ed) === '  a\n  b\nc\n', 'Tab indents every selected line')
  check(ed.selStart === 2 && ed.selEnd === 7, 'and keeps them selected')
  const back = indent(apply(t, ed), ed.selStart, ed.selEnd, '  ', true)
  check(apply(apply(t, ed), back) === t, 'Shift Tab takes it back off')
}
{
  const t = 'x'
  check(apply(t, indent(t, 1, 1, '    ', false)) === 'x    ', 'a caret alone gets one unit')
}
{
  const t = 'a\n\nb'
  check(apply(t, indent(t, 0, t.length, '  ', false)) === '  a\n\n  b', 'a blank line is not padded')
}
{
  const t = '  b'
  check(apply(t, indent(t, 3, 3, '    ', true)) === 'b', 'Shift Tab takes off what there is when it is less than a unit')
}
{
  const t = '    if (x) {'
  const ed = newline(t, t.length, t.length)
  check(apply(t, ed) === '    if (x) {\n    ' && ed.selStart === t.length + 5, 'Enter keeps the indent')
}
{
  const t = '  a\n    b\n'
  const ed = toggleComment(t, 0, t.length - 1, '//')
  check(apply(t, ed) === '  // a\n  //   b\n', 'a comment goes in at the shallowest indent')
  const off = toggleComment(apply(t, ed), 0, ed.text.length, '//')
  check(apply(apply(t, ed), off) === t, 'and comes back out')
}
check(commentFor('python') === '#' && commentFor('rust') === '//' && commentFor('json') === null,
  'a comment mark per language, and none for json')

if (failed) process.exit(1)
console.log('\ncheck-editkeys: ok')
