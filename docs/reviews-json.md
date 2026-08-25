# `mise run reviews --json` — what the daemon needs

**§6b of the spec is wrong on its premise.** It says "this mode does not exist yet"
and then invents a JSON contract. The mode already existed as a `queue` task,
`//MISE alias=["reviews"]`, and it already takes `--json`.

Its ranking is also better than the one §6b describes: prio labels, personal vs
team requests, re-review detection, reviewer-count tiebreak, and a separate
`ownBlocked` nudge list. None of that is in §6b.

So: **keep the existing shape, add six fields.** The daemon adapts to `queue`,
not the other way round. Rewriting `queue` to match an invented contract would
throw away working ranking logic and break its other two consumers (the human
output and `--slack`).

---

## What `--json` emits today

`--json` prints one `Queue` (or an array of them, when several logins were asked
for — the daemon always asks for one, so it always gets the object).

```ts
Queue {
  forLogin: string
  total: number          // open PRs seen
  skipped: number        // not review work for this login
  actionable: QueueEntry[]
  blocked: QueueEntry[]  // conflicts / failing checks / draft
  ownBlocked: QueueEntry[]  // your own PRs waiting on you
}

QueueEntry {
  pr: Pr
  reviewers: string[]    // humans who already reviewed
  blockers: string[]     // 'draft' | 'conflicts' | 'failing checks' | 'changes requested'
  needsReReview: boolean
  ageDays: number
  prio: number           // 0 stopper, 1 prio, 2 requested-personally,
                         // 3 requested-of-team, 4 re-review, 5 other, 6 sidequest
}

Pr {
  number, title, url, createdAt, isDraft
  mergeable: 'MERGEABLE' | 'CONFLICTING' | 'UNKNOWN'
  author: string | null
  labels: string[]
  reviews: { author, state, commitOid }[]
  requestedReviewers: string[]
  requestedTeams: string[]
  headOid: string | null
  checks: 'SUCCESS' | 'FAILURE' | 'ERROR' | 'PENDING' | 'EXPECTED' | null
}
```

`actionable` is already sorted by `compare` — rank first, then fewest reviewers,
then oldest.

---

## The delta

### 1. Envelope: `version` and `generated_at`

The daemon keys its degraded-pane logic on `version` (§6b), and needs to know how
stale the payload is. Currently there is neither.

```ts
if (args.includes('--json')) {
	const payload = { version: 1, generated_at: now.toISOString(), ... };
	console.log(JSON.stringify(payload, null, 2));
}
```

Wrap the existing value rather than replacing it. For the single-login case the
daemon wants the `Queue` fields at the top level alongside `version`:

```json
{ "version": 1, "generated_at": "...", "forLogin": "kbarendrecht",
  "total": 61, "skipped": 44, "actionable": [...], "blocked": [...], "ownBlocked": [...] }
```

Multi-login stays as-is under a `queues` key. The daemon never asks for it.

Bump `version` only on a breaking change. Added keys are ignored, not rejected.

### 2. Query: four more fields on `Pr`

All four are plain scalars on the existing `pullRequests.nodes` selection. No new
connection, no extra request, negligible point cost.

```graphql
nodes {
  number
  title
  url
  createdAt
  updatedAt        # NEW
  baseRefName      # NEW
  changedFiles     # NEW
  additions        # NEW
  deletions        # NEW
  isDraft
  ...
}
```

Mirror them into `GraphQlPr`, `Pr` and `toPr` the same way the existing scalars go.

**`changedFiles` is the one that actually matters.** §6b puts the changed-file
count on every review row as the review-cost signal — "37 files is a different
commitment from 1". Without it that column can't be built. `additions`/`deletions`
are cheap alongside it and give the daemon a fallback if it wants a size band
instead of a raw count.

`baseRefName` lets the daemon detect stacked PRs in the review queue the same way
§6 does for your own. `updatedAt` distinguishes a PR that has been sitting
untouched from one being actively pushed to while it waits on you.

---

## What §6b asks for that you should *not* build

**`requested_at`.** GitHub does not expose a timestamp on `reviewRequests`. Getting
it means a `timelineItems(itemTypes: [REVIEW_REQUESTED_EVENT])` connection on every
PR, which is a real per-PR cost increase over the whole open-PR set, for a sort key
that `prio` + `ageDays` already covers better. Skip it, and rewrite §6b's sort rule
to "rank ascending, then reviewer count, then `ageDays` descending" — which is what
`compare` already does.

**`state`** (`requested` / `re_requested` / `changes_requested` / `approved` /
`commented`). `prio`, `needsReReview` and `blockers` carry the same information with
more resolution — `prio` alone separates a personal request from a team request,
which §6b's `state` cannot express. The daemon should consume those three directly.

**`checks` remapping.** Leave the raw GitHub enum. The daemon maps
`SUCCESS→passing`, `FAILURE|ERROR→failing`, `PENDING|EXPECTED→pending`,
`null→unknown` at its own boundary. Normalising inside `queue` would change what
the human and Slack output see.

---

## Daemon side (orchd, not the queue task)

- Treat non-zero exit, unparseable stdout, or an unknown `version` as a **degraded**
  pane — `reviews unavailable`, stderr tail on hover. Never an empty queue. This is
  unchanged from §6b and is the important part.
- `queue` shells out to `gh api graphql`, so it uses gh's own auth, not the daemon's
  token. It will fail differently from the PR poller and needs its own degraded path.
- Run it in the main checkout on its own 5-minute timer, offset from the PR poll.
- Rows render from `actionable`; `blocked` goes behind the same disclosure the
  `--blocked` flag gates in the human output.

## Against the spec

`spec.md` §6b describes a built-in review queue with server-side ranking. That was
built, worked, and was deliberately reverted — it was more machinery than the one
real user wanted to own. **This file is the contract, not §6b**: the daemon shells
out to `reviews_command` and renders its JSON, and a checkout that configures none
gets a pane reading `off`. `TODO.md` has the trade-off that was accepted.
