// Cut a release, and refuse while CI has not answered for the commit you are on.
//
// The whole point is the wait. `check` is the only thing that runs the test suite
// on macOS, and the release workflow runs it *again* after the tag exists — so a
// tag pushed before `check` is green is a tag that may publish nothing. That has
// happened twice, both times a test that passed on Linux and failed on macos-14
// (a fixture path under `$TMPDIR`, which is a symlink into `/private` there). Each
// time the cost was the same: a version number spent, a tag deleted by hand, and
// the next release starting over.
//
// Everything below is the by-hand list in CLAUDE.md § Releases with the waiting
// done for you, in the same order and with the same files. It never force-pushes,
// never moves an existing tag, and never deletes one.
//
//   mise run release             # bump the last CalVer component
//   mise run release -- 2027.1.1 # or say the version
//   mise run release -- --dry-run

import { execFileSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const args = process.argv.slice(2)
const dry = args.includes('--dry-run')
const asked = args.find((a) => !a.startsWith('--'))

const run = (cmd, argv, opts = {}) =>
  execFileSync(cmd, argv, { cwd: root, encoding: 'utf8', ...opts }).trim()
const git = (...argv) => run('git', argv)

const step = (s) => console.log(`\x1b[36m▸\x1b[0m ${s}`)
const done = (s) => console.log(`  ${s}`)
const die = (s) => {
  console.error(`\x1b[31m✗\x1b[0m ${s}`)
  process.exit(1)
}

// --- where we are ----------------------------------------------------------

// The four files that carry the version, and the two lines of `Cargo.lock`. Listed
// rather than discovered, because a release that bumped three of them and not the
// fourth is exactly what the workflow's own tag-matches-version step refuses —
// after the tag is pushed.
const CARGO = path.join(root, 'Cargo.toml')
const DESKTOP = path.join(root, 'desktop/Cargo.toml')
const TAURI = path.join(root, 'desktop/tauri.conf.json')
const LOCK = path.join(root, 'Cargo.lock')

const read = (f) => fs.readFileSync(f, 'utf8')
const current = read(CARGO).match(/^version = "([^"]+)"$/m)?.[1]
if (!current) die(`no version line in ${CARGO}`)

/** CalVer: `<year>.<month>.<n>`, and only the last component is bumped here. A new
 *  month is a decision, not an increment, so it is passed in. */
function next(v) {
  const m = v.match(/^(\d+)\.(\d+)\.(\d+)$/)
  if (!m) die(`${v} is not <year>.<month>.<n> — pass the version you want`)
  return `${m[1]}.${m[2]}.${Number(m[3]) + 1}`
}

const version = asked ?? next(current)
const tag = `v${version}`
step(`releasing ${current} → ${version}`)

// --- the refusals, before anything is written ------------------------------

if (git('rev-parse', '--abbrev-ref', 'HEAD') !== 'main') {
  die('a release is cut from main')
}
if (git('status', '--porcelain')) {
  die('the tree is dirty — commit or stash first, so the tag names what you tested')
}
if (git('tag', '--list', tag)) die(`${tag} already exists locally`)
if (git('ls-remote', '--tags', 'origin', tag)) die(`${tag} already exists on origin`)

// Up to date with the remote, or the tag names a commit nobody else has and the
// release builds something that is not on main.
git('fetch', '--quiet', 'origin', 'main')
const behind = git('rev-list', '--count', 'HEAD..origin/main')
if (behind !== '0') die(`main is ${behind} commit(s) behind origin — pull first`)

// --- the wait --------------------------------------------------------------
//
// The commit being released is HEAD, so that is the commit whose `check` run has
// to be green. Asked by sha rather than by branch: `--branch main` answers about
// whatever main pointed at when the run started, which is the wrong question the
// moment two pushes land close together.
const head = git('rev-parse', 'HEAD')
step(`waiting for check on ${head.slice(0, 8)}`)

let gh = true
try {
  run('gh', ['--version'])
} catch {
  gh = false
}
if (!gh) {
  die('gh is not installed, so CI cannot be asked — install it, or tag by hand knowing the risk')
}

/** The `check` run for exactly this commit, or `null` when none has been created
 *  yet (a push seconds ago). */
function checkRun() {
  const out = run('gh', [
    'run', 'list', '--workflow=check.yml', '--commit', head, '--limit', '1',
    '--json', 'status,conclusion,databaseId,url',
  ])
  const [r] = JSON.parse(out)
  return r ?? null
}

const started = Date.now()
const LIMIT_MS = 30 * 60 * 1000
for (;;) {
  const r = checkRun()
  if (r && r.status === 'completed') {
    if (r.conclusion !== 'success') {
      die(`check is ${r.conclusion} for this commit — fix it before tagging\n  ${r.url}`)
    }
    done(`check succeeded — ${r.url}`)
    break
  }
  if (Date.now() - started > LIMIT_MS) {
    die('check has not finished in 30 minutes; look at it rather than tagging blind')
  }
  // `gh run watch` would be tighter, but it needs a run id and there may be none
  // yet — a push made seconds ago has no run to watch. Polling covers both.
  done(r ? `check is ${r.status}…` : 'no check run for this commit yet…')
  execFileSync('sleep', ['15'])
}

// --- the bump --------------------------------------------------------------

/** Rewrite `count` occurrences of `from` in `file`, refusing any other number:
 *  a version that appears once where two were expected means the file moved on. */
function bump(file, from, to, count) {
  const s = read(file)
  const seen = s.split(from).length - 1
  if (seen !== count) die(`${path.relative(root, file)} has ${seen} of \`${from}\`, expected ${count}`)
  if (!dry) fs.writeFileSync(file, s.split(from).join(to))
  done(`${path.relative(root, file)}: ${count}`)
}

step('bumping the version')
bump(CARGO, `version = "${current}"`, `version = "${version}"`, 1)
bump(DESKTOP, `version = "${current}"`, `version = "${version}"`, 1)
bump(TAURI, `"version": "${current}"`, `"version": "${version}"`, 1)
// Both crates, and only those two: every dependency in the lock has its own
// version line, so this is the one file where the count is the check.
bump(LOCK, `version = "${current}"`, `version = "${version}"`, 2)

if (dry) {
  console.log(`\n(dry run: nothing written, nothing tagged; ${tag} would have been pushed)`)
  process.exit(0)
}

// --- tag and push ----------------------------------------------------------

step('committing, tagging and pushing')
git('add', '-A')
// `--no-verify`: the hook's checks have just been answered by CI for the commit
// underneath this one, and a version bump cannot fail them.
git('commit', '--no-verify', '-m', `Release ${version}`)
git('push', 'origin', 'main')
git('tag', tag)
git('push', 'origin', tag)
done(`${tag} pushed`)

console.log(`\nThe release workflow is running. Watch it with:\n  gh run watch $(gh run list --workflow=release.yml --limit 1 --json databaseId -q '.[0].databaseId')`)
