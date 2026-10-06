#!/usr/bin/env node
// The editor's three-way merge, case by case.
//
//   node tools/check-merge.mjs
//
// The merge runs on a buffer somebody typed and a file an agent wrote, so a
// wrong answer loses one of the two. The cases are the shapes that happen: the
// two editing different places, the same place, the same way, and at the ends.

import { hasMarkers, merge3, MINE, SPLIT, THEIRS } from '../web/js/merge.js'

let failed = false
const check = (ok, what) => {
  console.log(`  ${ok ? 'ok  ' : 'FAIL'}  ${what}`)
  if (!ok) failed = true
}
const L = (...xs) => xs.join('\n') + '\n'

const base = L('a', 'b', 'c', 'd', 'e')
{
  const r = merge3(base, L('A', 'b', 'c', 'd', 'e'), L('a', 'b', 'c', 'd', 'E'))
  check(r?.text === L('A', 'b', 'c', 'd', 'E') && r.conflicts === 0, 'edits to different lines both land')
}
{
  const r = merge3(base, L('a', 'B', 'c', 'd', 'e'), L('a', 'b', 'C', 'd', 'e'))
  check(r?.text === L('a', 'B', 'C', 'd', 'e') && r.conflicts === 0, 'neighbouring lines are not a conflict')
}
{
  const r = merge3(base, L('a', 'X', 'c', 'd', 'e'), L('a', 'Y', 'c', 'd', 'e'))
  check(r?.conflicts === 1 && r.text === L('a', MINE, 'X', SPLIT, 'Y', THEIRS, 'c', 'd', 'e'),
    'the same line two ways is a marked conflict')
  check(hasMarkers(r?.text ?? ''), 'and the markers are found again before a save')
}
{
  const r = merge3(base, L('a', 'Z', 'c', 'd', 'e'), L('a', 'Z', 'c', 'd', 'e'))
  check(r?.text === L('a', 'Z', 'c', 'd', 'e') && r.conflicts === 0, 'the same edit on both sides is one edit')
}
{
  const r = merge3(base, L('a', 'b', 'c', 'd', 'e', 'mine'), L('top', 'a', 'b', 'c', 'd', 'e'))
  check(r?.text === L('top', 'a', 'b', 'c', 'd', 'e', 'mine'), 'additions at both ends both land')
}
{
  const r = merge3(base, L('a', 'c', 'd', 'e'), L('a', 'b', 'c', 'd', 'E'))
  check(r?.text === L('a', 'c', 'd', 'E') && r.conflicts === 0, 'a deletion and an edit elsewhere both land')
}
{
  const r = merge3(base, L('a', 'b', 'mine', 'c', 'd', 'e'), L('a', 'b', 'disk', 'c', 'd', 'e'))
  check(r?.conflicts === 1, 'two insertions at the same place are a conflict')
}
check(merge3(base, base, L('x'))?.text === L('x'), 'an untouched buffer takes the disk as it is')
check(merge3(base, L('x'), base)?.text === L('x'), 'an untouched disk keeps the buffer')
check(!hasMarkers(L('a', '<<<<<<< not ours', 'b')), 'a line that only starts like a marker is not one')

if (failed) process.exit(1)
console.log('\ncheck-merge: ok')
