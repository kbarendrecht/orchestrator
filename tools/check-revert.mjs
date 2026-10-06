#!/usr/bin/env node
// What the diff's revert arrow writes, case by case.
//
//   node tools/check-revert.mjs
//
// The arrow rewrites a file on disk, and a wrong answer is a quiet one: a
// newline gained at the end, a CRLF file turned half LF, a block put back one
// line off. Each case here is a real diff's rows, so the parser's shape is the
// input rather than a guess at it.

import { changeBlocks, revertedText } from '../web/js/revert.js'

let failed = false
const check = (ok, what) => {
  console.log(`  ${ok ? 'ok  ' : 'FAIL'}  ${what}`)
  if (!ok) failed = true
}

const ctx = (old, nw, text) => ({ kind: 'context', old, new: nw, text })
const del = (old, text, extra = {}) => ({ kind: 'del', old, new: null, text, ...extra })
const add = (nw, text, extra = {}) => ({ kind: 'add', old: null, new: nw, text, ...extra })
const hunk = (old_start, new_start, rows) => ({ old_start, new_start, header: '', gap_before: 0, rows })
const only = (h) => {
  const b = changeBlocks(h)
  if (b.length !== 1) throw new Error(`expected one block, got ${b.length}`)
  return b[0]
}

{
  const b = only(hunk(1, 1, [ctx(1, 1, 'a'), del(2, 'b'), add(2, 'B'), ctx(3, 3, 'c')]))
  check(revertedText('a\nB\nc\n', b) === 'a\nb\nc\n', 'a changed line goes back')
}
{
  const b = only(hunk(1, 1, [ctx(1, 1, 'a'), add(2, 'new'), ctx(2, 3, 'c')]))
  check(revertedText('a\nnew\nc\n', b) === 'a\nc\n', 'an added line goes away')
}
{
  const b = only(hunk(1, 1, [ctx(1, 1, 'a'), del(2, 'gone'), del(3, 'also'), ctx(4, 2, 'c')]))
  check(b.at === 2, 'a removal sits after the context above it')
  check(revertedText('a\nc\n', b) === 'a\ngone\nalso\nc\n', 'removed lines come back where they were')
}
{
  const b = only(hunk(1, 1, [del(1, 'top'), ctx(2, 1, 'rest')]))
  check(revertedText('rest\n', b) === 'top\nrest\n', 'a removal at the very top comes back at the top')
}
{
  // The old file ended without a newline, the new one added a line after it.
  const b = only(hunk(1, 1, [ctx(1, 1, 'a'), del(2, 'b', { no_eol: true }), add(2, 'b'), add(3, 'c')]))
  check(revertedText('a\nb\nc\n', b) === 'a\nb', 'the missing final newline is missing again')
}
{
  // The other way: the new file dropped the final newline.
  const b = only(hunk(1, 1, [ctx(1, 1, 'a'), del(2, 'b'), add(2, 'b', { no_eol: true })]))
  check(revertedText('a\nb', b) === 'a\nb\n', 'a dropped final newline comes back')
}
{
  // git strips the \r, and the file keeps its line endings.
  const b = only(hunk(1, 1, [ctx(1, 1, 'a'), del(2, 'b'), add(2, 'B'), ctx(3, 3, 'c')]))
  check(revertedText('a\r\nB\r\nc\r\n', b) === 'a\r\nb\r\nc\r\n', 'a CRLF file stays CRLF')
}
{
  const b = only(hunk(1, 1, [ctx(1, 1, 'a'), del(2, 'b'), add(2, 'B'), ctx(3, 3, 'c')]))
  check(revertedText('a\nX\nc\n', b) === null, 'lines that moved since the diff are refused')
  check(revertedText('a\n', b) === null, 'a file that got shorter is refused')
}
{
  // An untracked file is all added, so reverting it leaves it empty.
  const b = only(hunk(0, 1, [add(1, 'one'), add(2, 'two')]))
  check(revertedText('one\ntwo\n', b) === '', 'an all-new file reverts to empty')
}
{
  const blocks = changeBlocks(hunk(10, 10, [
    ctx(10, 10, 'a'), del(11, 'b'), add(11, 'B'), ctx(12, 12, 'c'), add(13, 'd'), ctx(13, 14, 'e'),
  ]))
  check(blocks.length === 2 && blocks[0].at === 11 && blocks[1].at === 13, 'two blocks, each where it starts')
}

if (failed) process.exit(1)
console.log('\ncheck-revert: ok')
