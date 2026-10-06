// Searching a workspace: the routes are wired, and each workspace answers for
// itself.
//
// The unit tests in `search.rs` hold what the walk does. What only a real daemon
// can say is that the two routes are *registered* — a missing `.route(...)` line
// compiles perfectly — and that a workspace resolves to the tree the caller
// meant. The second half is the one with teeth: main's checkout contains every
// worktree, so a main that forgot its exclude answers with every sibling
// session's work (§2).

import assert from 'node:assert/strict'
import fs from 'node:fs'
import path from 'node:path'

export const name = 'search a workspace, and only that workspace'

const find = (t, ws, extra = '') =>
  t.api('GET', `/api/search?workspace=${encodeURIComponent(ws)}&pattern=frobnicate${extra}`)

export async function run(t) {
  // A worktree with its own file, and the same word written into main.
  const { session } = await t.api('POST', '/api/worktree', { name: 'haystack' })
  await t.settled(session)
  const { workspace, cwd: tree } = await t.session(session)
  fs.writeFileSync(path.join(tree, 'in-the-worktree.txt'), 'frobnicate here\n')
  fs.writeFileSync(path.join(t.repo, 'in-main.txt'), 'frobnicate in main\n')

  // Untracked, and found anyway: nothing was committed above, and an agent's new
  // file is the common case this has to cover.
  const mine = await find(t, workspace)
  const paths = mine.hits.map((h) => h.path)
  assert.deepEqual(paths, ['in-the-worktree.txt'], `a worktree answers for itself: ${paths}`)
  assert.equal(mine.hits[0].line, 1)
  assert.equal(mine.hits[0].text, 'frobnicate here')

  // The exclude: main sees its own file and none of the worktree's.
  const inMain = await find(t, 'main')
  const mainPaths = inMain.hits.map((h) => h.path)
  assert.ok(mainPaths.includes('in-main.txt'), `main answers for itself: ${mainPaths}`)
  assert.ok(
    !mainPaths.some((p) => p.includes('in-the-worktree.txt')),
    `main must not answer with a worktree's work: ${mainPaths}`,
  )

  // The path filter reaches the route rather than being dropped by the query
  // parser — a flattened sub-struct is easy to get wrong and silent when it is.
  const filtered = await find(t, workspace, '&glob=*.rs')
  assert.deepEqual(filtered.hits, [], 'a glob that matches no file finds nothing')

  /* And a *boolean* through that same flatten, which is the half the glob cannot
     answer for. `exact_case` was the one flag the overlay never sent, so nothing
     had ever asked whether it arrives. */
  fs.writeFileSync(path.join(tree, 'capitalised.txt'), 'Frobnicate here\n')
  const smart = await find(t, workspace)
  assert.equal(smart.hits.length, 2, 'smart case finds the capital one too')
  const exact = await find(t, workspace, '&exact_case=true')
  assert.deepEqual(
    exact.hits.map((h) => h.path), ['in-the-worktree.txt'],
    'and exact case leaves it behind',
  )
  fs.rmSync(path.join(tree, 'capitalised.txt'))

  /* The other two flags go through the same parser, and both were refused with
     it — so pressing `.*` or `W` in the overlay turned every search into a
     deserialize error rather than a different answer. */
  const asRegex = await t.api('GET', `/api/search?workspace=${encodeURIComponent(workspace)}`
    + '&pattern=frobni.ate&regex=true')
  assert.equal(asRegex.hits.length, 1, 'a regex query reaches the walk')
  const asWord = await t.api('GET', `/api/search?workspace=${encodeURIComponent(workspace)}`
    + '&pattern=frobnicat&word=true')
  assert.deepEqual(asWord.hits, [], 'and a whole-word query is a whole word')

  // And the name search: same walk, same workspace.
  const listed = await t.api('GET', `/api/paths?workspace=${encodeURIComponent(workspace)}`)
  assert.ok(listed.paths.includes('in-the-worktree.txt'), 'the file is listed')
  assert.equal(listed.truncated, false)

  // An unknown workspace is refused rather than answered from somewhere.
  await assert.rejects(() => find(t, 'no-such-workspace'), /unknown workspace/)
}
