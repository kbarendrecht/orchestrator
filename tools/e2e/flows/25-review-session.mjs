// The contender: one session reads a PR's threads, you decide, it posts.
//
// The other review flow (15) covers the pane, which is the proven one. This covers
// the overlay session — the thing that had never made a round trip to anything, and
// whose whole case is that it is safer than a person driving `gh` by hand.
//
// Worth a flow because every claim in that case lives across four processes and no
// unit test can see the join: a canned PR becoming a worktree on its own branch, the
// post token the proposals ride in on, the ask channel carrying the human's
// decisions back, and — the part this exists for — the outward writes themselves.
// `fake-gh.mjs` records the argv and the body `forge::github_write` built, so the
// footer, the REST path and who gets re-requested are asserted as they were sent.
//
// The re-request is the half that had never run at all. It is derived from a *fresh*
// fetch rather than from what the session thinks it posted, so this flow changes
// what GitHub answers between the reply and the re-request, exactly as GitHub would
// once the reply landed.

import assert from 'node:assert/strict'
import { branchOf, git, until } from '../harness.mjs'

export const name = 'a review session answers its threads'

const VIEWER = 'e2e-viewer'
const PR = 303
const HEAD = 'feature/session-review'
const SHA = 'abc1234'

// Two reviewers on purpose. `octo` is answered in full and is asked to look again;
// `dora` keeps one thread open, which is what holds her back — per reviewer, never
// per PR.
const OCTO_A = { id: 'PRRT_octo_a', path: 'src/reserve.rs', line: 42, comment_id: 501 }
const OCTO_B = { id: 'PRRT_octo_b', path: 'src/pricing.rs', line: 88, comment_id: 502 }
const DORA = { id: 'PRRT_dora', path: 'src/tax.rs', line: 9, comment_id: 503 }

const REPLY = 'It is per-order on purpose: the rate is quoted once at checkout.'

/** One canned thread whose last word is `author`'s. */
const open = (t, author, body) => ({
  id: t.id,
  path: t.path,
  line: t.line,
  comments: [{ id: t.comment_id, author, body }],
})

/** The same thread after we answered it: our own comment is last, which is what
 *  `Thread::is_answerable` reads to decide it is no longer awaiting us. */
const answered = (t, author, body, said) => ({
  ...open(t, author, body),
  comments: [
    { id: t.comment_id, author, body },
    { id: t.comment_id + 100, author: VIEWER, body: said },
  ],
})

export const options = {
  // Without `repo` the daemon derives owner/name from a local path and turns
  // polling off, so `pr_from_poll` would never see this PR. Same reason as flow 15.
  repo: 'acme/monorepo',
  githubToken: 'e2e-token',
  pollSeconds: 30,
}

export async function run(t) {
  // A real branch: the worktree is cut with `git worktree add <path> <branch>`.
  git(t.repo, ['branch', HEAD])

  const threads = [
    open(OCTO_A, 'octo', 'reserve() indexes items[0] before checking the cart.'),
    open(OCTO_B, 'octo', 'Should this rate be per-country?'),
    open(DORA, 'dora', 'The tax table is stale.'),
  ]
  // `checks: 'SUCCESS'`, so the hand-off at the end arms no `fix-pr` run. What that
  // run does is flow 8's; here it would spawn a second agent into the assertions.
  const canned = (items) => [{
    number: PR, head_ref: HEAD, head_sha: SHA, checks: 'SUCCESS', threads: items,
  }]
  t.setPrs(canned(threads), VIEWER)
  await until('the poll to report the canned PR', async () =>
    (await t.pollPrs()).prs.some((p) => p.number === PR))

  // --- the spawn ------------------------------------------------------------
  const { session } = await t.api('POST', `/api/pr/${PR}/review-session`)
  await t.settled(session)
  const s = await t.session(session)
  assert.equal(s.workspace, `pr-${PR}`)
  assert.equal(branchOf(s.cwd), HEAD, 'the session must read the PR\'s own code')
  // `Pass::REVIEW` is what every route below checks before it will act for this
  // session, and what tells the rail and the guards which flow this is.
  assert.equal(s.pass.command, 'review')
  assert.equal(s.pass.pr, PR)
  await until('the session to be typed its instructions', async () =>
    t.agentLog().includes(`turn: /orchd:review ${PR}`))

  // --- the proposals, on the run's own credential ---------------------------
  //
  // Not the app token: a posting run is given a token keyed on this PR and good for
  // nothing else, and `/proposals` is the one route that takes it.
  const agent = (route, body, token = 'x-orch-ask') => fetch(
    `http://127.0.0.1:${t.port}${route}`,
    {
      method: 'POST',
      headers: {
        [token]: token === 'x-orch-ask' ? t.askToken(session) : t.postToken(session),
        'content-type': 'application/json',
      },
      body: JSON.stringify(body ?? {}),
    },
  )

  const position = (label, stance, reply) => ({ label, sub: 'why', stance, reply })
  const proposals = {
    base_sha: SHA,
    threads: [OCTO_A, OCTO_B, DORA].map((x) => ({
      thread_id: x.id,
      read: 'what the reviewer is after',
      recommend: 0,
      options: [
        position('Agree', 'agree', null),
        position('Explain', 'reply', REPLY),
      ],
    })),
  }
  const handed = await agent(`/api/pr/${PR}/proposals`, proposals, 'x-orch-token')
  assert.equal(handed.status, 200, `the proposals were refused: ${await handed.text()}`)

  // What the overlay reads. Both halves matter: the threads came through the fetch
  // the daemon makes for itself, and the proposals came through the route above.
  const review = await t.api('GET', `/api/pr/${PR}/review`)
  assert.equal(review.answerable, 3, 'every canned thread still awaits an answer')
  assert.equal(review.proposals.proposals.length, 3)
  /* Files-changed order, not the chronological order GitHub answers in: people
     review in the Files tab, and `sort_for_review` puts the list back into that
     order. Byte order on the path, so `pricing` precedes `reserve` precedes `tax`. */
  assert.deepEqual(review.threads.map((x) => x.path),
    ['src/pricing.rs', 'src/reserve.rs', 'src/tax.rs'])

  // --- the decisions, over the ask channel ----------------------------------
  //
  // The agent raises the question and the human answers it. The fake agent does not
  // run the review skill, so the flow stands in for both ends — what is real is the
  // channel, which is the same `/ask` every other question in the app uses.
  const raised = await agent(`/api/session/${session}/ask`, {
    question: 'Waiting for your decisions in the review overlay.',
    options: [{ value: 'decisions', label: 'Decisions submitted', free: true }],
  })
  const { ask } = await raised.json()
  await t.api('POST', `/api/session/${session}/answer`, {
    ask,
    answer: 'decisions',
    text: JSON.stringify({
      decisions: [
        { thread_id: OCTO_A.id, stance: 'agree', solution: 'Agree', reply: '' },
        { thread_id: OCTO_B.id, stance: 'reply', solution: 'Explain', reply: REPLY },
        { thread_id: DORA.id, stance: 'skip' },
      ],
    }),
  })

  // --- the outward writes ---------------------------------------------------
  const reacted = await agent(`/api/session/${session}/thread/${OCTO_A.id}/reply`, {})
  assert.deepEqual(await reacted.json(), { posted: false, reacted: true })

  const posted = await agent(
    `/api/session/${session}/thread/${OCTO_B.id}/reply`, { reply: REPLY })
  const said = await posted.json()
  assert.equal(said.posted, true, `the reply did not go out: ${JSON.stringify(said)}`)
  assert.equal(said.already, false)

  /* The argv `github_write` built, which is the whole point of shimming `gh` rather
     than the seam above it. A reaction hangs off the comment directly and a reply is
     nested under the PR — two REST shapes that are easy to write down wrongly and
     impossible to tell apart from a green unit test. */
  const calls = t.ghCalls()
  const react = calls.find((c) => c.argv[1]?.endsWith('/reactions'))
  assert.ok(react, `no reaction was sent: ${JSON.stringify(calls)}`)
  assert.equal(react.argv[1], `repos/acme/monorepo/pulls/comments/${OCTO_A.comment_id}/reactions`)
  assert.equal(react.body.content, '+1')

  const reply = calls.find((c) => c.argv[1]?.endsWith('/replies'))
  assert.ok(reply, `no reply was sent: ${JSON.stringify(calls)}`)
  assert.equal(reply.argv[1],
    `repos/acme/monorepo/pulls/${PR}/comments/${OCTO_B.comment_id}/replies`)
  /* The footer, asserted on the bytes that left. `forge::acknowledged` reads this
     exact string to decide a thread has been answered, so a reply without it leaves
     the thread reading unanswered for ever — and the agent is deliberately not the
     one who remembers to add it. */
  assert.ok(reply.body.body.endsWith('(via orchestrator)'),
    `the reply went out without the footer: ${JSON.stringify(reply.body.body)}`)
  assert.ok(reply.body.body.startsWith(REPLY), 'the human\'s words must go out verbatim')

  // --- the re-request -------------------------------------------------------
  //
  // GitHub now holds what we just said, so the canned answer does too. This is the
  // step that makes the derivation real: nothing is remembered about what was
  // posted, and the fetch is what decides whose turn it is.
  t.setPrs(canned([
    answered(OCTO_A, 'octo', 'reserve() indexes items[0] before checking the cart.',
      'ok'),
    answered(OCTO_B, 'octo', 'Should this rate be per-country?', REPLY),
    // Skipped, so dora still has the floor.
    open(DORA, 'dora', 'The tax table is stale.'),
  ]), VIEWER)

  const asked = await (await agent(`/api/session/${session}/rerequest`)).json()
  assert.deepEqual(asked.asked, ['octo'], `wrong reviewers asked: ${JSON.stringify(asked)}`)
  assert.deepEqual(asked.held, { dora: 1 }, 'dora\'s open thread must hold her back')
  assert.deepEqual(asked.failed, {})

  const edit = t.ghCalls().find((c) => c.argv[0] === 'pr' && c.argv[1] === 'edit')
  assert.ok(edit, 'no reviewer was re-requested')
  assert.deepEqual(edit.argv,
    ['pr', 'edit', String(PR), '--repo', 'acme/monorepo', '--add-reviewer', 'octo'])

  // --- the hand-off ---------------------------------------------------------
  //
  // How a review ends. The overlay reads the session ending as its report, so a
  // hand-off that leaves the session alive is a screen that never moves on.
  await agent(`/api/session/${session}/handoff`)
  await until('the review session to end', async () =>
    !(await t.session(session))?.alive)
}
