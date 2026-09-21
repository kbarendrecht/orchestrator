#!/usr/bin/env node
// What `web/js/markdown.js` must make of a note.
//
//   node tools/check-markdown.mjs
//
// **The parser is pure so that this can exist.** It answers with plain objects,
// so the block rules — where a paragraph ends, what a fence swallows, when three
// dashes are a rule rather than a heading — are drivable in node with no browser,
// no daemon and no page. The painter is the other half and is not tested here:
// it is a walk over this tree that appends nodes, and `page-check` looks at what
// it drew.
//
// The cases are the ones a note actually contains, plus the three that are easy
// to get wrong: a fence that never closes, a table without its separator, and a
// heading directly under prose.

import { parse } from '../web/js/markdown.js'

let failed = false
const check = (ok, what) => {
  console.log(`  ${ok ? 'ok  ' : 'FAIL'}  ${what}`)
  if (!ok) failed = true
}
const kinds = (md) => parse(md).map((b) => b.kind).join(',')

// --- the ordinary shapes ----------------------------------------------------

check(kinds('# Title\n\nsome prose\n') === 'heading,para', 'a heading and a paragraph')
check(parse('### Deep\n')[0].level === 3, 'the level is the number of hashes')
check(parse('# Title ###\n')[0].text === 'Title', 'and a closing run of hashes is not part of it')

const fenced = parse('before\n\n```rust\nfn x() {}\n```\n\nafter\n')
check(
  fenced.map((b) => b.kind).join(',') === 'para,code,para',
  `a fence is its own block, got ${fenced.map((b) => b.kind)}`,
)
check(fenced[1].lang === 'rust' && fenced[1].text === 'fn x() {}', 'with its language and its text')

const listed = parse('- one\n- two\n  - nested\n')
check(listed.length === 1 && listed[0].kind === 'list', 'a list is one block')
check(
  listed[0].items.map((i) => i.depth).join(',') === '0,0,1',
  `nesting is two spaces to a level, got ${listed[0].items.map((i) => i.depth)}`,
)
check(parse('1. one\n2. two\n')[0].ordered === true, 'a numbered list says so')
check(
  kinds('- one\n1. two\n') === 'list,list',
  'and a change of marker ends the run rather than mixing the two',
)

const table = parse('| a | b |\n|---|---|\n| 1 | 2 |\n')
check(
  table[0].kind === 'table' && table[0].head.join(',') === 'a,b' && table[0].rows[0].join(',') === '1,2',
  `a pipe table, got ${JSON.stringify(table[0])}`,
)

check(kinds('> quoted\n> more\n') === 'quote', 'a blockquote')
check(kinds('---\n') === 'rule', 'a rule')

// --- the three that are easy to get wrong -----------------------------------

check(
  kinds('```\nunclosed\n') === 'code',
  'a fence with no closing fence runs to the end rather than eating nothing',
)
check(
  kinds('a | b\nc | d\n') === 'para',
  'two lines with pipes are prose without a separator row',
)
check(
  kinds('prose\n# Heading\n') === 'para,heading',
  'a heading directly under prose is still a heading',
)
check(
  kinds('see the --- in here\n') === 'para',
  'and dashes inside a line are not a rule',
)

// --- what a paragraph joins -------------------------------------------------

check(
  parse('one\ntwo\n\nthree\n').map((b) => b.text).join('|') === 'one two|three',
  'a paragraph joins its lines and a blank line ends it',
)

console.log(failed ? '\ncheck-markdown: FAILED' : '\ncheck-markdown: ok')
process.exit(failed ? 1 : 0)
