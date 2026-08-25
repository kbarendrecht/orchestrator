#!/usr/bin/env node
// A stand-in for `curl`, for the flows that need GitHub to answer.
//
// The daemon reaches GitHub only through `forge::github::graphql()`, which shells
// out to `curl` — so, exactly like the agent, GitHub can be substituted by putting
// something earlier on PATH and changing nothing in the daemon. What comes back
// here is the real transport: the real argv, the real header-on-stdin dance, the
// real JSON parser, the real `parse_pr`. Only the network is fake.
//
// The one trap this file exists to avoid: `curl` is not only GitHub's. The
// `SessionStart` hook the daemon writes is a shell command that curls
// 127.0.0.1, so a shim that answered everything would silently unhook every
// session — the flows would still run, and the daemon would simply never learn a
// session had started. Anything that is not an api.github.com URL is handed to the
// real curl instead.
//
// The canned PRs come from `$ORCH_E2E_DIR/prs.json`, read per call rather than
// baked in, so a flow can change what the next poll sees without restarting the
// daemon. Absent means "no open PRs of yours", which is the state a flow starts in.

import { spawnSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import process from 'node:process'

const argv = process.argv.slice(2)
const url = argv.find((a) => /^https?:\/\//.test(a))

if (!url?.startsWith('https://api.github.com')) {
  passThrough()
} else if (url.endsWith('/graphql')) {
  answer(graphql(bodyOf(argv)))
} else {
  // The update nudge's release check (`latest_release`) is the other GitHub
  // caller, and it is documented to treat anything short of a clean answer as
  // "no opinion". An empty object is that, and it keeps the check off the network.
  answer({})
}

/** Hand the call to the real curl: the hook posting to 127.0.0.1 is a real
 *  request the daemon's state depends on receiving. */
function passThrough() {
  const real = findRealCurl()
  if (!real) {
    process.stderr.write('fake-curl: no real curl on PATH\n')
    process.exit(127)
  }
  const r = spawnSync(real, argv, { stdio: 'inherit' })
  process.exit(r.status ?? 1)
}

/** The first `curl` on PATH that is not this shim.
 *
 *  Matched on content rather than only on the shim directory: `ORCH_E2E_DIR` is
 *  read from the environment, and a child that lost it would otherwise find the
 *  shim again and fork-bomb the sandbox. */
function findRealCurl() {
  const shimDir = process.env.ORCH_E2E_DIR && path.join(process.env.ORCH_E2E_DIR, 'bin')
  for (const dir of (process.env.PATH ?? '').split(':')) {
    if (!dir || dir === shimDir) continue
    const p = path.join(dir, 'curl')
    if (!fs.existsSync(p)) continue
    try {
      // A shell wrapper is small; a real curl is a binary and this read is enough
      // to tell them apart without executing anything.
      if (fs.readFileSync(p).includes('fake-curl.mjs')) continue
    } catch { /* unreadable is not ours */ }
    return p
  }
  return undefined
}

function answer(json) {
  // Drained first: the daemon writes the Authorization header into `-H @-` and
  // only then waits, so exiting without reading would race an EPIPE into what
  // looks like a GitHub outage.
  try { fs.readFileSync(0) } catch { /* no stdin is fine */ }
  process.stdout.write(JSON.stringify(json))
}

/** The GraphQL document the daemon sent, out of `--data-binary`. */
function bodyOf(args) {
  const i = args.indexOf('--data-binary')
  if (i < 0) return ''
  try { return JSON.parse(args[i + 1]).query ?? '' } catch { return '' }
}

function graphql(query) {
  const file = process.env.ORCH_E2E_DIR && path.join(process.env.ORCH_E2E_DIR, 'prs.json')
  const canned = file && fs.existsSync(file)
    ? JSON.parse(fs.readFileSync(file, 'utf8'))
    : {}
  const viewer = canned.viewer ?? 'e2e-viewer'

  // Only the poll is answered with PRs. The on-demand thread fetch has a
  // different shape and nothing in these flows drives it, so it gets a valid but
  // empty envelope rather than a lie in the shape of the poll's.
  if (!query.includes('search(query:')) return { data: { viewer: { login: viewer } } }

  const slug = query.match(/repo:(\S+)/)?.[1] ?? 'acme/monorepo'
  return {
    data: {
      viewer: { login: viewer },
      search: { nodes: (canned.prs ?? []).map((p) => node(p, slug, viewer)) },
    },
  }
}

/** One `PullRequest` node, exactly as `parse_pr` reads one.
 *
 *  Spelled out field by field rather than passed through from `prs.json`, because
 *  a name that does not match — `headRefName`, the rollup hanging off the head
 *  *commit* — comes back as a PR the daemon silently drops, not as an error. */
function node(p, slug, viewer) {
  return {
    number: p.number,
    title: p.title ?? `fixture pr ${p.number}`,
    url: `https://github.com/${slug}/pull/${p.number}`,
    headRefName: p.head_ref,
    baseRefName: p.base_ref ?? 'main',
    isDraft: p.is_draft ?? false,
    mergeable: p.mergeable ?? 'MERGEABLE',
    mergeStateStatus: p.merge_state ?? 'CLEAN',
    // Absent means "your own fork", which is what the authorship guard wants to
    // see for the happy path; a flow says otherwise to make the guard refuse.
    headRepositoryOwner: { login: p.head_owner ?? viewer },
    commits: {
      nodes: [{
        commit: {
          oid: p.head_sha ?? 'e2e0000',
          committedDate: p.committed_at ?? '2026-01-01T00:00:00Z',
          statusCheckRollup: { state: p.checks ?? 'FAILURE' },
        },
      }],
    },
    // One page, never capped: a capped page sends the poll off to page the rest,
    // which is a second query shape this shim would have to answer.
    reviewThreads: { pageInfo: { hasNextPage: false, endCursor: null }, nodes: [] },
    reviews: { nodes: p.changes_requested ? [{ author: { login: 'reviewer' } }] : [] },
  }
}
