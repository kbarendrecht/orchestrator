// Finding a past conversation by what was said in it.
//
// The rail's archive filter answers in two waves: the names come out of the
// snapshot the page already holds, and this is the other one. `store.rs` holds
// what a scan does with a file — tool output does not count, a plain-string
// `content` is read, an oversized record is skipped. What only a real daemon can
// say is that the route is *registered*, since a missing `.route(...)` line
// compiles perfectly, and that it looks at both halves of the archive: the
// conversations this daemon finished, and the ones it never started.
//
// The exclusion is the half with teeth. A transcript is what was said plus every
// tool result, and searching all of it matches nearly every conversation there is
// — measured: `rebase` hits 215 of 286 transcripts on the machine this was
// written on, and a filter that answers "all of them" is not a filter.

import assert from 'node:assert/strict'
import crypto from 'node:crypto'
import fs from 'node:fs'
import path from 'node:path'
import { until } from '../harness.mjs'

export const name = 'search the archive by what was said'

/** Claude Code keys its transcript dir by cwd, slugging every `/` *and* `.`:
 *  `config::transcript_slug`, and the fake agent spells it the same way. */
const transcriptDir = (home, cwd) =>
  path.join(home, '.claude/projects', cwd.replaceAll(/[/.]/g, '-'))

const hits = async (t, find) =>
  (await t.api('GET', `/api/archive/search?find=${encodeURIComponent(find)}`)).hits

export async function run(t) {
  // --- a conversation this daemon finished ---------------------------------

  const { session } = await t.api('POST', '/api/worktree', { name: 'ledger' })
  await t.settled(session)
  const tree = t.worktreePath('ledger')

  /* Appended to the transcript the fake agent is already writing, which is where
     Claude Code writes one: a turn somebody typed, and beside it the kind of record
     that must not count — a tool result carrying a word nobody said. */
  const file = path.join(transcriptDir(t.home, tree), `${session}.jsonl`)
  fs.appendFileSync(
    file,
    `${JSON.stringify({
      type: 'user',
      message: { role: 'user', content: [{ type: 'text', text: 'fix the marmalade export' }] },
    })}\n${JSON.stringify({
      type: 'user',
      message: { role: 'user', content: [{ type: 'tool_result', content: 'hippopotamus.rs:12' }] },
    })}\n`,
  )

  await t.api('POST', `/api/session/${session}/kill`)
  await until(`session ${session.slice(0, 8)} to stop being live`, async () =>
    (await t.session(session))?.alive === false)

  const said = await hits(t, 'marmalade')
  assert.equal(said.length, 1, 'the archived conversation was not found by what was said in it')
  assert.equal(said[0].id, session)
  assert.match(said[0].line, /fix the marmalade export/, 'the row must carry the line it matched')

  // The whole reason this is not a grep.
  assert.deepEqual(await hits(t, 'hippopotamus'), [],
    'a word only a tool printed must not find the conversation')

  // A live session is not in the archive, so it is not in the answer either.
  const { session: live } = await t.api('POST', '/api/worktree', { name: 'invoices' })
  await t.settled(live)
  fs.appendFileSync(
    path.join(transcriptDir(t.home, t.worktreePath('invoices')), `${live}.jsonl`),
    `${JSON.stringify({
      type: 'user',
      message: { role: 'user', content: [{ type: 'text', text: 'marmalade again' }] },
    })}\n`,
  )
  assert.deepEqual((await hits(t, 'marmalade')).map((h) => h.id), [session],
    'the box lists past conversations, so the search must not answer for a live one')

  // --- and one it never started --------------------------------------------

  /* The archive's other half: a `claude` run from a terminal leaves its transcript
     in the same directory and the rail lists it beside the daemon's own. A search
     that read only the daemon's records would answer for half the box. */
  const outside = crypto.randomUUID()
  // Main's own transcript directory need not exist yet: nothing in this flow has
  // run a session in the checkout itself.
  const mainDir = transcriptDir(t.home, t.repo)
  fs.mkdirSync(mainDir, { recursive: true })
  fs.writeFileSync(
    path.join(mainDir, `${outside}.jsonl`),
    // A turn first: `store::external_in` lists a conversation, not a file, and a
    // transcript of headers is not one yet.
    `{"type":"user","message":"where is the parser"}\n${JSON.stringify({
      type: 'assistant',
      message: { role: 'assistant', content: [{ type: 'text', text: 'the marmalade parser is in src' }] },
    })}\n{"type":"ai-title","aiTitle":"the parser"}\n`,
  )
  await t.api('POST', '/api/external/refresh')

  const both = (await hits(t, 'marmalade')).map((h) => h.id).sort()
  assert.deepEqual(both, [session, outside].sort(),
    'the outside conversation is in the box, so it must be in the search')

  // --- what it refuses ------------------------------------------------------

  /* One character matches nearly everything and costs a full read of every
     transcript in the checkout. The page debounces and asks for two, and the
     daemon refuses one on its own — a page is not the only thing that can call
     this. */
  assert.deepEqual(await hits(t, 'm'), [], 'a one-character query must not read anything')
  assert.deepEqual(await hits(t, '   '), [], 'and neither must an empty one')
}
