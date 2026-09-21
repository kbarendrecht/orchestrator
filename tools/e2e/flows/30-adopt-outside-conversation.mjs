// A conversation orchd did not start: found on disk, then taken over.
//
// The unit tests in `store.rs` hold what the scan does with a directory. What only
// a real daemon can say is that the two routes are *registered*, since a missing
// `.route(...)` line compiles perfectly; that the scan looks under the slug of a
// workspace this daemon actually owns; and that a `--resume` of an id it has never
// seen produces an ordinary session rather than a record with nothing behind it.
//
// The filter is the half with teeth. Every session the daemon starts writes its
// transcript into the same directory, so a scan that only asked "is there a file"
// would list every row in the rail a second time.

import assert from 'node:assert/strict'
import crypto from 'node:crypto'
import fs from 'node:fs'
import path from 'node:path'
import { until } from '../harness.mjs'

export const name = 'adopt a conversation orchd did not start'

/** Claude Code keys its transcript dir by cwd, slugging every `/` *and* `.`:
 *  `config::transcript_slug`, and the fake agent spells it the same way. */
const transcriptDir = (home, cwd) =>
  path.join(home, '.claude/projects', cwd.replaceAll(/[/.]/g, '-'))

export async function run(t) {
  const dir = transcriptDir(t.home, t.repo)
  fs.mkdirSync(dir, { recursive: true })

  // What a `claude` run in this checkout from a terminal leaves behind: a turn,
  // and the title Claude Code gave it.
  const outside = crypto.randomUUID()
  const file = path.join(dir, `${outside}.jsonl`)
  fs.writeFileSync(
    file,
    '{"type":"user","message":"hi"}\n{"type":"ai-title","aiTitle":"read the pty ring buffer"}\n',
  )

  // And what a session that was started and killed before anyone said anything
  // leaves: headers, no turn. Resuming it answers "no conversation found" and
  // exits, so it must not be offered.
  const turnless = crypto.randomUUID()
  fs.writeFileSync(
    path.join(dir, `${turnless}.jsonl`),
    '{"type":"permission-mode","permissionMode":"default"}\n',
  )

  /* **Written just now, so the daemon must refuse to adopt it.** A transcript
     something is still appending to may have a live agent behind it, and the spawn
     writes to that file. This is the whole of the guard: the daemon cannot see a
     shell's `claude`, so the mtime is the evidence and `force` is the answer. */
  await t.api('POST', '/api/external/refresh')
  const live = (await t.state()).external.find((c) => c.id === outside)
  assert.equal(live?.may_be_live, true, 'a transcript written a moment ago must read as live')
  await assert.rejects(
    () => t.api('POST', `/api/external/${outside}/resume`),
    /may still be open in a terminal/,
    'adopting a live-looking conversation must ask first',
  )

  // Aged an hour, which is what a terminal closed a while back looks like.
  const anHourAgo = new Date(Date.now() - 3600_000)
  fs.utimesSync(file, anHourAgo, anHourAgo)

  await t.api('POST', '/api/external/refresh')
  const listed = (await t.state()).external
  const found = listed.find((c) => c.id === outside)
  assert.ok(found, `the terminal conversation is not in the archive: ${JSON.stringify(listed)}`)
  assert.equal(found.title, 'read the pty ring buffer')
  assert.equal(found.workspace, 'main', 'it was filed under main, so main is where it resumes')
  assert.equal(found.may_be_live, false, 'an hour-old transcript is nobody\'s live session')
  assert.ok(
    !listed.some((c) => c.id === turnless),
    'a transcript with no turn in it was offered',
  )

  /* Taken over: the daemon relaunches it under the id the file is named for, so
     the conversation carries on rather than starting again.

     **Held mid-turn for the assertion that follows**, which is the one that matters
     and is invisible a moment later. `watch_session_exit` forgets a session that
     ends with `had_a_turn` false and *deletes its transcript* — and this transcript
     is Claude Code's own, not the daemon's copy. So the record has to say "this is
     a conversation" from the instant it exists, before a single turn has run. Let
     the agent take its turn first and the flag is true for the other reason, which
     is how this was passing while the file was one early exit from being unlinked. */
  t.hold(true)
  let session
  try {
    ;({ session } = await t.api('POST', `/api/external/${outside}/resume`))
    assert.equal(session, outside, 'a resume must keep the conversation id')
    await until('the adopted session to be live', async () => (await t.session(session))?.alive)
    const early = await t.session(session)
    assert.equal(
      early.has_transcript,
      true,
      'an adopted conversation must read as a conversation before its first turn',
    )
  } finally {
    t.hold(false)
  }
  await t.settled(session)
  const row = await t.session(session)
  assert.equal(row.workspace, 'main')
  assert.ok(fs.existsSync(file), "the daemon must not touch Claude Code's own transcript")

  // And it is a record now, so the scan must stop offering it. That is the filter
  // that keeps every session in the rail from appearing twice.
  await t.api('POST', '/api/external/refresh')
  assert.ok(
    !(await t.state()).external.some((c) => c.id === outside),
    'a conversation the daemon now has a record for was still listed as outside',
  )
  await assert.rejects(
    () => t.api('POST', `/api/external/${outside}/resume`),
    /one of this daemon's own/,
    'the second resume must point at the row rather than spawning again',
  )
}
