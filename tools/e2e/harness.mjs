// The sandbox an end-to-end flow runs in: a real repo, a real daemon, a fake
// agent, and nothing shared with the machine it runs on.
//
// Three things make this safe to run anywhere, and each of them is load-bearing:
//
//   * `ORCHD_CONFIG_DIR` relocates every piece of durable state — config,
//     `sessions.json`, `automation.json`, the hook settings, the instance lock.
//     So a fixture daemon does not fight the real one, and the one-daemon lock is
//     per-sandbox rather than per-machine.
//   * `HOME` is relocated too, which is normally the wrong thing to do: the real
//     `claude` reads its credentials from there. With a fake agent there are no
//     credentials to lose, and it is what keeps transcripts out of the user's
//     `~/.claude/projects`.
//   * `PATH` is prefixed with a shim directory, which is how the fake agent takes
//     the place of `claude` without the daemon knowing.

import { spawn, spawnSync } from 'node:child_process'
import fs from 'node:fs'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
export const repoRoot = path.resolve(here, '../..')
const DAEMON = path.join(repoRoot, 'target/debug/orchd')

/** A port nobody is on, asked of the kernel rather than guessed, so flows can
 *  run beside each other and beside a real daemon. */
function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer()
    srv.once('error', reject)
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address()
      srv.close(() => resolve(port))
    })
  })
}

export function git(cwd, args) {
  const r = spawnSync('git', args, { cwd, encoding: 'utf8' })
  if (r.status !== 0) {
    throw new Error(`git ${args.join(' ')} in ${cwd}\n${r.stdout}${r.stderr}`)
  }
  return r.stdout.trim()
}

export const branchOf = (cwd) => git(cwd, ['rev-parse', '--abbrev-ref', 'HEAD'])
export const isDirty = (cwd) => git(cwd, ['status', '--porcelain']) !== ''

/** Wait for something to become true, or say what it still was when time ran out.
 *
 *  Every wait in these flows is one of these: the daemon is a state machine
 *  driven by hooks arriving over HTTP, so "the POST returned" is never the same
 *  as "the state changed". A fixed sleep would trade flakiness for slowness and
 *  get both. */
export async function until(what, predicate, { timeout = 10_000, every = 50, context } = {}) {
  const deadline = Date.now() + timeout
  for (;;) {
    const last = await predicate()
    if (last) return last
    if (Date.now() > deadline) {
      // A timeout that only says what it wanted is the least useful failure in a
      // suite like this: the daemon is gone by the time you read it. `context`
      // renders what was actually there on the last look.
      const seen = context ? `\nlast saw: ${await context()}` : ''
      throw new Error(`timed out waiting for ${what}${seen}`)
    }
    await new Promise((r) => setTimeout(r, every))
  }
}

/**
 * Build a sandbox and start a daemon in it.
 *
 * @param {object} [opts]
 * @param {boolean} [opts.delegated] Put worktrees under `.claude/worktrees`,
 *   Claude Code's own default, which is what makes the daemon delegate the cut to
 *   `claude --worktree` instead of doing it itself. The two arms register the
 *   workspace at different moments, so a flow that matters in both says so.
 * @param {number} [opts.turns] Turns the agent takes unprompted. `0` is the
 *   session that was never typed into — the one fork and resume refuse.
 * @param {string} [opts.repo] `owner/name` to poll PRs for. Setting it is what
 *   turns GitHub on at all: without it the daemon derives the repo from the
 *   origin remote, which here is a local path, and switches polling off. It also
 *   installs the `curl` shim, so a flow that asks for PRs gets canned ones and
 *   every other flow keeps reaching whatever curl the machine has.
 * @param {string} [opts.githubToken] `ORCHD_GITHUB_TOKEN`. Non-empty short-circuits
 *   the token ladder before it reaches `gh`, which would be a real account.
 * @param {number} [opts.pollSeconds] The poll period. The daemon floors it at 30,
 *   and a flow drives `/api/prs/refresh` rather than waiting for it either way.
 */
export async function sandbox({
  delegated = false,
  turns = 1,
  repo = undefined,
  githubToken = '',
  pollSeconds = 3600,
} = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'orchd-e2e-'))
  const dirs = {
    root,
    home: path.join(root, 'home'),
    cfg: path.join(root, 'cfg'),
    repo: path.join(root, 'repo'),
    bin: path.join(root, 'bin'),
  }
  for (const d of Object.values(dirs)) fs.mkdirSync(d, { recursive: true })
  fs.writeFileSync(path.join(root, 'turns'), String(turns))

  // The repo under test. One commit, one extra branch, and an `origin` that is a
  // real local clone — the daemon fetches, and a missing remote turns ordinary
  // paths into warnings that read like failures.
  git(root, ['init', '-q', '-b', 'main', 'repo'])
  git(dirs.repo, ['config', 'user.email', 'e2e@test'])
  git(dirs.repo, ['config', 'user.name', 'e2e'])
  fs.writeFileSync(path.join(dirs.repo, 'README.md'), '# fixture\n')
  git(dirs.repo, ['add', '-A'])
  git(dirs.repo, ['commit', '-qm', 'base'])
  git(root, ['clone', '-q', '--bare', dirs.repo, 'origin.git'])
  git(dirs.repo, ['remote', 'add', 'origin', path.join(root, 'origin.git')])
  git(dirs.repo, ['fetch', '-q', 'origin'])

  // The agent shim. `exec` so the daemon's kill reaches node itself rather than a
  // shell holding it, or the pty would outlive the session.
  const shim = path.join(dirs.bin, 'claude')
  fs.writeFileSync(shim, `#!/bin/sh\nexec node ${path.join(here, 'fake-claude.mjs')} "$@"\n`)
  fs.chmodSync(shim, 0o755)

  // The GitHub shim, only for a flow that asked for a repo. It has to sit beside
  // the agent's because the daemon's only route to GitHub is `curl` — and it must
  // *not* exist otherwise, since the same binary carries the SessionStart hook.
  if (repo) {
    const curl = path.join(dirs.bin, 'curl')
    fs.writeFileSync(curl, `#!/bin/sh\nexec node ${path.join(here, 'fake-curl.mjs')} "$@"\n`)
    fs.chmodSync(curl, 0o755)
  }

  const port = await freePort()
  fs.writeFileSync(path.join(dirs.cfg, 'config.json'), JSON.stringify({
    main_checkout: dirs.repo,
    port,
    // The layout decides who cuts the worktree, so it is the one setting a flow
    // may care about.
    worktrees_subdir: delegated ? '.claude/worktrees' : '.worktrees',
    upstream_remote: 'origin',
    upstream_ref: 'origin/main',
    // Absent unless a flow asked: an unset `repo` leaves the daemon deriving one
    // from the origin remote, which is a local path, so PR polling never starts.
    ...(repo ? { repo } : {}),
    // Nothing to poll and nothing to review: both would otherwise spend the run
    // shelling out to `gh` and logging failures that have nothing to do with the
    // flow under test.
    poll_seconds: pollSeconds,
    reviews_command: ['true'],
    main_processes: [],
    worktree_processes: [],
    auto_resume: false,
  }, null, 2))

  const env = {
    ...process.env,
    HOME: dirs.home,
    ORCHD_CONFIG_DIR: dirs.cfg,
    ORCH_E2E_DIR: root,
    PATH: `${dirs.bin}:${process.env.PATH}`,
    RUST_LOG: process.env.E2E_LOG ?? 'warn',
    // A real `gh` here would reach the network on someone's account. Nothing in
    // these flows needs it, and an empty token keeps the GitHub paths off.
    GH_TOKEN: '',
    // Empty by default, and then the ladder falls through to `gh auth token` on a
    // real account — which is fine only because nothing polls. A flow that polls
    // sets one, and the value never leaves the shim.
    ORCHD_GITHUB_TOKEN: githubToken,
  }

  const logPath = path.join(root, 'daemon.log')
  const log = fs.openSync(logPath, 'a')
  const proc = spawn(DAEMON, [], { env, stdio: ['ignore', log, log] })

  const token = await until('the daemon to print its token', () => {
    const out = fs.readFileSync(logPath, 'utf8')
    return out.match(/token=([a-z0-9]+)/)?.[1]
  }, { timeout: 15_000 })

  const base = `http://127.0.0.1:${port}`
  /** The API, with the four things that bite when driving it by hand already
   *  right: the `x-orch-token` header, an `Origin` matching the port, `/api/state`
   *  as the read route, and an error body surfaced rather than swallowed. */
  const api = async (method, route, body) => {
    const res = await fetch(base + route, {
      method,
      headers: {
        'x-orch-token': token,
        origin: base,
        ...(body ? { 'content-type': 'application/json' } : {}),
      },
      body: body ? JSON.stringify(body) : undefined,
    })
    const text = await res.text()
    let json
    try { json = JSON.parse(text) } catch { json = { raw: text } }
    if (!res.ok) {
      const err = new Error(json.error ?? json.raw ?? `HTTP ${res.status}`)
      err.status = res.status
      throw err
    }
    return json
  }

  const state = () => api('GET', '/api/state')

  return {
    ...dirs,
    port,
    token,
    api,
    state,
    log: () => fs.readFileSync(logPath, 'utf8'),

    /** How many turns the next spawned agent takes on its own. */
    setTurns: (n) => fs.writeFileSync(path.join(root, 'turns'), String(n)),

    /** What the next GitHub poll sees. See `fake-curl.mjs` for the fields. */
    setPrs: (prs, viewer) =>
      fs.writeFileSync(path.join(root, 'prs.json'), JSON.stringify({ viewer, prs }, null, 2)),

    /** Force a poll and wait for the fetch to land, not merely to be asked for.
     *
     *  `pr_poll` is the counter the refresh button watches, and it moves on a
     *  failed fetch too — so waiting on it says "GitHub answered", while waiting
     *  on the PR list appearing would hang forever on a shim that answered wrong. */
    async pollPrs() {
      const before = (await state()).pr_poll
      await api('POST', '/api/prs/refresh')
      return until('a PR poll to land', async () => {
        const s = await state()
        return s.pr_poll > before ? s : null
      })
    },

    /** The path a worktree of this name lives at, whichever layout is in play. */
    worktreePath: (name) =>
      path.join(dirs.repo, delegated ? '.claude/worktrees' : '.worktrees', name),

    session: async (id) => (await state()).sessions.find((s) => s.id === id),
    workspace: async (id) => (await state()).workspaces.find((w) => w.id === id),

    /** Wait until a session is idle at its prompt.
     *
     *  `your_turn` and not `working`: a session mid-turn is the one thing the swap
     *  refuses, so a looser wait makes a flow race its own setup. */
    settled: (id, want = ['your_turn']) =>
      until(`session ${id.slice(0, 8)} to reach ${want.join('|')}`, async () => {
        const s = await state()
        const found = s.sessions.find((x) => x.id === id)
        return found && want.includes(found.state.state) ? found : null
      }),

    async stop() {
      proc.kill('SIGTERM')
      await new Promise((r) => setTimeout(r, 400))
      if (!proc.killed) proc.kill('SIGKILL')
      fs.closeSync(log)
    },

    /** Kept on failure so there is something to read; removed otherwise. */
    cleanup: () => fs.rmSync(root, { recursive: true, force: true }),
  }
}
