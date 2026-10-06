#!/usr/bin/env node
// Find-in-file, case by case.
//
//   node tools/check-seek.mjs
//
// The cases are the ones that are wrong in silence rather than loudly: a match
// on a line with an accent in it, a pattern that can match nothing, the wrap at
// either end, and smart case. A wrong column here puts the mark on the wrong
// glyph, which reads as a renderer bug rather than as a search bug.

import { MAX, hitLines, matches, nextIndex, offsetOf } from '../web/js/seek.js'

let failed = false
const check = (ok, what) => {
  console.log(`  ${ok ? 'ok  ' : 'FAIL'}  ${what}`)
  if (!ok) failed = true
}
const L = (...xs) => xs.join('\n')
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b)

const text = L('let a = 1', 'let bb = 2', 'LET c = 3')

{
  const h = matches(text, 'let', false)
  check(h.length === 3, 'a lower-case query is case-insensitive (smart case)')
  check(same(h[0], { line: 1, col: 0, len: 3, at: 0 }), 'the first hit is line 1, column 0')
  check(h[2].line === 3 && h[2].col === 0, 'the upper-case line is found too')
}
{
  const h = matches(text, 'LET', false)
  check(h.length === 1 && h[0].line === 3, 'an upper-case letter makes the query sensitive')
}
{
  const h = matches(text, 'b+', false)
  check(h.length === 0, 'a literal query is not a pattern')
  check(matches(text, 'b+', true).length === 1, 'the same query as a regex is one')
}
{
  // The trap this exists for: a column counted in bytes puts the mark two glyphs
  // to the right of the word on a line with an accent in it.
  const t = L('// héllo wörld', 'const x = 1')
  const h = matches(t, 'wörld', false)
  check(h[0].col === 9 && h[0].at === 9, 'columns are characters, not bytes')
  const h2 = matches(t, 'const', false)
  check(h2[0].line === 2 && h2[0].col === 0, 'a line after a multi-byte one is placed right')
}
{
  // `lastIndex` does not move on an empty match, so this is an infinite loop
  // rather than a wrong answer.
  const h = matches(text, 'x*', true)
  check(Array.isArray(h) && h.length === 0, 'a pattern that can match nothing terminates')
  check(matches(text, '^', true).length === 0, 'and so does an anchor on its own')
}
{
  check(matches(text, '[', true) === null, 'a regex that does not compile answers null')
  check(matches(text, '[', false).length === 0, 'the same text as a literal is just not there')
  check(same(matches(text, '', false), []), 'an empty query matches nothing')
}
{
  // `^` and `$` mean the line, because this searches inside one file.
  check(matches(text, '^bb', true).length === 0 && matches(text, 'bb', true).length === 1,
    'the start anchor is the line, not the file')
  check(matches(text, '= 1$', true).length === 1, 'and so is the end anchor')
}
{
  const h = matches(text, 'let', false)
  check(nextIndex(h, -1, 1) === 0, 'forward from before the file lands on the first hit')
  check(nextIndex(h, h[0].at, 1) === 1, 'forward from a hit lands on the next one')
  check(nextIndex(h, h[2].at, 1) === 0, 'forward from the last hit wraps')
  check(nextIndex(h, h[0].at, -1) === 2, 'back from the first hit wraps')
  check(nextIndex(h, h[2].at, -1) === 1, 'back from a hit lands on the previous one')
  check(nextIndex([], 0, 1) === -1, 'no hits is no index')
}
{
  const many = matches('x\n'.repeat(MAX + 500), 'x', false)
  check(many.length === MAX, 'the collection stops at the cap')
}
{
  check(offsetOf(text, 1) === 0 && offsetOf(text, 2) === 10, 'a line start is where the line starts')
  check(offsetOf(text, 99) === offsetOf(text, 3), 'a line past the end is the last one')
}
{
  const h = matches(L('a a', 'b', 'a'), 'a', false)
  check(same(hitLines(h), [1, 3]), 'the ruler is told each line once')
}

console.log(failed ? '\n✘ seek' : '\n✔ seek')
process.exit(failed ? 1 : 0)
