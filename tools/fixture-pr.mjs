#!/usr/bin/env node
// Build a throwaway PR that the resolve flow can actually be driven against.
//
// The wall this exists to get past: `query_for` (src/forge/github.rs) polls
// `author:@me`, so the PR has to be yours, while `acknowledged()` treats a thread
// whose last comment is yours as answered — so the review threads have to be
// somebody else's. One account cannot satisfy both, and four attempts to verify
// the flow against the monorepo died there.
//
// The second identity here is `github-actions[bot]`: a workflow on the fixture's
// default branch posts the threads with its own `GITHUB_TOKEN`, whose author login
// is not yours. That needs no second account and no stored credential, at the cost
// of one dispatch round-trip per build. It does not cover `rerequest()` — a bot
// cannot be a requested reviewer — so that button stays unverified.
//
// Everything is rebuilt from scratch rather than reused, because the things this
// unblocks (teardown deleting a worktree, a run committing, a dirty tree for
// `triage::gate`) are destructive and want a fresh target every time.
//
//   node tools/fixture-pr.mjs            # build it, print how to point a daemon at it
//   node tools/fixture-pr.mjs --threads 5
//   node tools/fixture-pr.mjs --destroy  # delete the repo and the local state

import { execFileSync } from 'node:child_process';
import { mkdirSync, rmSync, writeFileSync, existsSync, readdirSync, unlinkSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { homedir } from 'node:os';

const REPO = 'orchd-fixture';
const BRANCH = 'fixture/pricing';
const WORKFLOW = 'fixture-threads.yml';
// Under ~/.cache, not in the monorepo: a bad run here deletes worktrees and
// force-pushes, and none of that should be able to reach real work.
const ROOT = join(homedir(), '.cache', 'orchd-fixture');
const CLONE = join(ROOT, 'repo');
const CONFIG = join(ROOT, 'config');

const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const value = (name, fallback) => {
  const i = args.indexOf(name);
  return i === -1 ? fallback : args[i + 1];
};
const threadCount = Number(value('--threads', '3'));

const step = (msg) => console.log(`\x1b[36m▸\x1b[0m ${msg}`);
const done = (msg) => console.log(`  \x1b[32m✓\x1b[0m ${msg}`);
const warn = (msg) => console.log(`  \x1b[33m!\x1b[0m ${msg}`);

function run(cmd, argv, opts = {}) {
  try {
    return execFileSync(cmd, argv, { encoding: 'utf8', ...opts }).trim();
  } catch (e) {
    const detail = (e.stderr || e.stdout || e.message).toString().trim();
    throw new Error(`${cmd} ${argv.join(' ')}\n${detail}`);
  }
}
const git = (...argv) => run('git', argv, { cwd: CLONE });
// A trailing object is execFile options, so a caller that needs to swallow
// stderr can, without every other call growing an argument.
const gh = (...argv) => {
  const opts = typeof argv.at(-1) === 'object' ? argv.pop() : {};
  return run('gh', argv, opts);
};

// ── the seed ────────────────────────────────────────────────────────────────
// Two files so the PR has both an added and a modified one: the added file is
// all additions, which is what makes any line of it commentable, and the
// modified one is what a changed-files pane and a diff view have to render.

const INVENTORY_BASE = `export const STOCK = new Map();

export function receive(sku, count) {
  STOCK.set(sku, (STOCK.get(sku) ?? 0) + count);
}

export function available(sku) {
  return STOCK.get(sku) ?? 0;
}
`;

const INVENTORY_HEAD = `export const STOCK = new Map();

export function receive(sku, count) {
  STOCK.set(sku, (STOCK.get(sku) ?? 0) + count);
}

export function available(sku) {
  return STOCK.get(sku) ?? 0;
}

export function reserve(sku, count) {
  if (available(sku) < count) return false;
  STOCK.set(sku, available(sku) - count);
  return true;
}
`;

const PRICING = `const VAT = 0.21;

export function lineTotal(unitPrice, quantity, discountPct) {
  const gross = unitPrice * quantity;
  const discounted = Math.round(gross) * (1 - discountPct / 100);
  return discounted * (1 + VAT);
}

export function format(amount) {
  return '€' + amount.toFixed(2);
}

export function cartTotal(lines) {
  let total = 0;
  for (const line of lines) {
    total += lineTotal(line.price, line.qty, line.discount);
  }
  return total;
}
`;

const WORKFLOW_YML = `name: fixture threads

# Posts review threads as github-actions[bot]. That bot identity is the entire
# reason this repo exists: orchd's \`acknowledged()\` reads a thread whose last
# comment is the viewer's as answered, so a fixture built by one account needs a
# reviewer who is not that account.
on:
  workflow_dispatch:
    inputs:
      pr:
        description: PR number to comment on
        required: true
      threads:
        description: JSON array of {path, line, body}
        required: true

permissions:
  contents: read
  pull-requests: write

jobs:
  post:
    runs-on: ubuntu-latest
    steps:
      - name: post one review thread per entry
        env:
          GH_TOKEN: \${{ github.token }}
          PR: \${{ inputs.pr }}
          THREADS: \${{ inputs.threads }}
        run: |
          set -euo pipefail
          # Anchor on the head sha rather than the base: a comment on a line that
          # is not in the diff of that commit is rejected outright.
          head=$(gh api "repos/$GITHUB_REPOSITORY/pulls/$PR" --jq .head.sha)
          echo "$THREADS" | jq -c '.[]' | while read -r t; do
            gh api "repos/$GITHUB_REPOSITORY/pulls/$PR/comments" \\
              -f commit_id="$head" \\
              -f path="$(jq -r .path <<<"$t")" \\
              -F line="$(jq -r .line <<<"$t")" \\
              -f side=RIGHT \\
              -f body="$(jq -r .body <<<"$t")" \\
              --jq '"posted \\(.path):\\(.line) — \\(.id)"'
          done
`;

// One thread per decision the triage card can make, so a run has all three arms
// to exercise: a defect that wants a patch, a question that wants only words,
// and an out-of-scope suggestion that wants a story.
const THREADS = [
  {
    path: 'src/pricing.js',
    line: 5,
    body:
      'This rounds the gross amount *before* applying the discount, so the ' +
      'discount comes off a number the customer never saw. Round at the end, ' +
      'once, or the total drifts by a cent per line.',
  },
  {
    path: 'src/pricing.js',
    line: 1,
    body:
      'Why is VAT a module constant here? We serve more than one country, and I ' +
      'cannot tell from this whether that is a deliberate simplification for now ' +
      'or an oversight.',
  },
  {
    path: 'src/pricing.js',
    line: 14,
    body:
      'Out of scope for this PR, but `cartTotal` accumulating floats will bite us ' +
      'on a big basket. Worth its own ticket to move the money handling to ' +
      'integer cents rather than widening this change.',
  },
];

// ── destroy ─────────────────────────────────────────────────────────────────

function destroy(owner) {
  step('deleting the fixture');
  if (existsSync(ROOT)) {
    rmSync(ROOT, { recursive: true, force: true });
    done(`removed ${ROOT}`);
  }
  try {
    gh('repo', 'delete', `${owner}/${REPO}`, '--yes');
    done(`deleted ${owner}/${REPO}`);
  } catch (e) {
    if (/HTTP 403|delete_repo/.test(e.message)) {
      warn(
        `cannot delete ${owner}/${REPO}: your token has no \`delete_repo\` scope.\n` +
          '    gh auth refresh -h github.com -s delete_repo\n' +
          '  or delete it in the web UI. Local state is gone either way.'
      );
    } else if (/Could not resolve|not found/i.test(e.message)) {
      done('no such repo remotely, nothing to delete');
    } else {
      throw e;
    }
  }
}

// ── build ───────────────────────────────────────────────────────────────────

const write = (rel, body) => {
  const abs = join(CLONE, rel);
  mkdirSync(dirname(abs), { recursive: true });
  writeFileSync(abs, body);
};
// Commits without touching the caller's signing config: a fixture commit is not
// something to prompt for a key over.
const commit = (msg) => git('-c', 'commit.gpgsign=false', 'commit', '-q', '-m', msg);

// The fixture is reset in place rather than deleted and recreated. Deleting needs
// the `delete_repo` scope, which a `repo`-scoped token does not have and which is
// a lot of authority to ask for so a scratch repo can be rebuilt. Resetting also
// avoids rewriting `main`: the seed is deterministic, so main is pushed once and
// only ever fast-forwarded, and the churn is confined to the PR branch.
function ensureRepo(owner) {
  step(`preparing ${owner}/${REPO} (private)`);
  let existed = true;
  try {
    // stderr swallowed: "no such repo" is the expected answer on a first run,
    // and gh writes its GraphQL complaint about it straight to the terminal.
    gh('repo', 'view', `${owner}/${REPO}`, '--json', 'name',
       { stdio: ['ignore', 'pipe', 'ignore'] });
  } catch {
    existed = false;
  }
  if (existed) {
    done('reusing the existing repo');
  } else {
    // `--add-readme` is load-bearing, not politeness. GitHub only indexes
    // workflow files that a push actually touches, and the first push to an
    // empty repo is not indexed at all — so a workflow arriving in it is never
    // registered, and `gh workflow run` 404s forever with the file plainly
    // sitting on the default branch. An initial commit means the workflow
    // arrives in an ordinary second push, which is indexed. Diagnosed the hard
    // way: `ping.yml` added in a later push registered instantly while the
    // fixture's own workflow, pushed first, stayed invisible.
    gh('repo', 'create', `${owner}/${REPO}`, '--private', '--add-readme',
       '--description',
       'Throwaway target for orchd review-flow verification. Safe to delete.');
    done('repo created with an initial commit');
  }

  // The local clone is always thrown away: the things this fixture unblocks
  // delete worktrees and commit into it, so carrying one over between builds
  // would mean debugging the leftovers of the last run.
  if (existsSync(ROOT)) rmSync(ROOT, { recursive: true, force: true });
  mkdirSync(CLONE, { recursive: true });
  // SSH rather than HTTPS: `gh` holds the API token but is not necessarily
  // installed as a git credential helper, so an HTTPS push prompts. A clone in
  // both branches, because `--add-readme` means the repo always has a commit.
  run('git', ['clone', '-q', `git@github.com:${owner}/${REPO}.git`, CLONE]);
}

function seedMain() {
  step('seeding the default branch');
  write('README.md',
    `# ${REPO}\n\nThrowaway target for verifying orchd's review flow. ` +
    `Rebuilt by \`tools/fixture-pr.mjs\`; nothing here is real. Safe to delete.\n`);
  write('src/inventory.js', INVENTORY_BASE);
  write(`.github/workflows/${WORKFLOW}`, WORKFLOW_YML);
  // Anything else under workflows/ is from an older build of this script. Left
  // in place on a reused repo it stays dispatchable, so a stale fixture could
  // post threads nothing here knows about.
  const dir = join(CLONE, '.github', 'workflows');
  for (const f of readdirSync(dir)) {
    if (f !== WORKFLOW) {
      unlinkSync(join(dir, f));
      done(`dropped a stale workflow: ${f}`);
    }
  }
  git('add', '-A');
  if (!git('status', '--porcelain')) {
    done('main already carries the seed');
    return;
  }
  commit('seed the fixture');
  git('push', '-q', 'origin', 'main');
  done('main pushed, workflow available to dispatch');
}

function resetBranch(owner) {
  step(`resetting ${BRANCH}`);
  // Close first, then drop the branch. Reusing an open PR would pile this
  // build's threads on top of the last one's, and a rebuild is supposed to
  // produce a known number of threads awaiting you.
  const open = JSON.parse(
    gh('pr', 'list', '--repo', `${owner}/${REPO}`, '--head', BRANCH, '--json', 'number')
  );
  for (const { number } of open) {
    gh('pr', 'close', String(number), '--repo', `${owner}/${REPO}`, '--delete-branch');
    done(`closed the previous PR #${number}`);
  }
  try {
    // stderr swallowed: "remote ref does not exist" is the normal answer once
    // `--delete-branch` above has already taken it, and git says so loudly.
    run('git', ['push', '-q', 'origin', '--delete', BRANCH],
        { cwd: CLONE, stdio: ['ignore', 'pipe', 'ignore'] });
  } catch {
    // Already gone — closed with `--delete-branch`, or this is a first build.
  }

  git('checkout', '-q', '-B', BRANCH, 'main');
  write('src/pricing.js', PRICING);
  write('src/inventory.js', INVENTORY_HEAD);
  git('add', '-A');
  commit('add line pricing and stock reservation');
  git('push', '-q', 'origin', BRANCH);
  // Back on the default branch: `main_checkout` is the privileged checkout, and
  // the daemon expects to find it sitting on the base, not on a PR head.
  git('checkout', '-q', 'main');
  done('branch pushed, clone left on main');
}

function openPr(owner) {
  step('opening the PR');
  gh('pr', 'create', '--repo', `${owner}/${REPO}`, '--base', 'main', '--head', BRANCH,
     '--title', 'Add line pricing and stock reservation',
     '--body',
     'Fixture PR for orchd review-flow verification. The review threads on this ' +
     'PR are posted by github-actions[bot] so that their last word is not the ' +
     "author's — see tools/fixture-pr.mjs.");
  const number = Number(
    gh('pr', 'view', BRANCH, '--repo', `${owner}/${REPO}`, '--json', 'number', '--jq', '.number')
  );
  done(`PR #${number}`);
  return number;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function postThreads(owner, pr) {
  const wanted = THREADS.slice(0, threadCount).concat(
    // More threads than prose: cycle the bodies, varying the anchor so each is
    // still its own thread rather than a reply on the same line.
    Array.from({ length: Math.max(0, threadCount - THREADS.length) }, (_, i) => ({
      ...THREADS[i % THREADS.length],
      line: 6 + ((i * 3) % 12),
    }))
  );

  step(`dispatching the workflow for ${wanted.length} thread(s)`);
  const runsBefore = latestRunId(owner);
  // The workflow was pushed seconds ago and indexing is not instant, which reads
  // as a 404 rather than a queue. Retry rather than fail. This is only a delay
  // now that the workflow always arrives in an indexed push (see `ensureRepo`);
  // before that fix the same 404 was permanent, so a bounded retry that gives up
  // with a named error is deliberate — an infinite wait would have hidden it.
  let dispatched = false;
  for (let i = 0; i < 20 && !dispatched; i++) {
    try {
      gh('workflow', 'run', WORKFLOW, '--repo', `${owner}/${REPO}`,
         '-f', `pr=${pr}`, '-f', `threads=${JSON.stringify(wanted)}`);
      dispatched = true;
    } catch (e) {
      if (!/HTTP 404/.test(e.message)) throw e;
      if (i === 0) warn('workflow not registered yet, waiting for GitHub to index it');
      await sleep(3000);
    }
  }
  if (!dispatched) throw new Error(`${WORKFLOW} never became dispatchable`);

  // The dispatch returns before the run is listed, so wait for an id greater
  // than the one that existed a moment ago rather than trusting "the newest".
  let id = null;
  for (let i = 0; i < 30 && id === null; i++) {
    await sleep(2000);
    const now = latestRunId(owner);
    if (now !== null && now !== runsBefore) id = now;
  }
  if (id === null) throw new Error('the workflow run never appeared; check Actions on the repo');
  done(`run ${id} started`);

  step('waiting for it to finish');
  for (let i = 0; i < 60; i++) {
    const r = JSON.parse(gh('run', 'view', String(id), '--repo', `${owner}/${REPO}`,
                            '--json', 'status,conclusion'));
    if (r.status === 'completed') {
      if (r.conclusion !== 'success') {
        throw new Error(
          `the run concluded ${r.conclusion}. Logs:\n` +
          `  gh run view ${id} --repo ${owner}/${REPO} --log-failed`
        );
      }
      done('run succeeded');
      return;
    }
    await sleep(3000);
  }
  throw new Error(`run ${id} did not finish in three minutes`);
}

// `null` for both "no runs yet" and "GitHub has not registered the workflow
// yet" — a push does not make it dispatchable the same second, and the API says
// 404 rather than "empty" until it has been indexed.
function latestRunId(owner) {
  let out;
  try {
    out = gh('run', 'list', '--repo', `${owner}/${REPO}`, '-w', WORKFLOW,
             '-L', '1', '--json', 'databaseId');
  } catch (e) {
    if (/HTTP 404/.test(e.message)) return null;
    throw e;
  }
  const runs = JSON.parse(out);
  return runs.length ? runs[0].databaseId : null;
}

// The assertion that matters. Everything above is setup; this is the
// precondition four earlier attempts could not produce, so it gets checked
// rather than assumed.
function verify(owner, pr, viewer) {
  step('checking each thread\'s last word is not yours');
  const q = `{ repository(owner:"${owner}", name:"${REPO}") {
    pullRequest(number:${pr}) {
      reviewThreads(first:50) { nodes {
        isResolved isOutdated
        comments(last:1) { nodes { author { login } } }
      } }
    } } }`;
  const out = JSON.parse(gh('api', 'graphql', '-f', `query=${q}`));
  const nodes = out.data.repository.pullRequest.reviewThreads.nodes;
  if (!nodes.length) throw new Error('the PR has no review threads');

  const awaiting = nodes.filter(
    (t) => !t.isResolved && !t.isOutdated &&
           t.comments.nodes[0]?.author?.login !== viewer
  );
  for (const t of nodes) {
    const who = t.comments.nodes[0]?.author?.login ?? '(none)';
    console.log(`    ${who === viewer ? '✗' : '✓'} last comment by ${who}`);
  }
  if (awaiting.length !== nodes.length) {
    throw new Error(
      `${nodes.length - awaiting.length} thread(s) read as answered already — ` +
      '`acknowledged()` would skip them, which is the exact failure this fixture exists to avoid'
    );
  }
  done(`${awaiting.length} thread(s) awaiting you, which is what the daemon needs`);
  return awaiting.length;
}

function writeConfig() {
  step('writing the fixture daemon config');
  mkdirSync(CONFIG, { recursive: true });
  const cfg = {
    main_checkout: CLONE,
    upstream_ref: 'origin/main',
    upstream_remote: 'origin',
    // Nothing to build and no review queue in a fixture: left non-empty, the
    // panes would read as broken commands rather than as absent.
    main_processes: [],
    reviews_command: [],
    tracker: 'none',
    // Not this repo's TODO.md, and not inside the clone either — the daemon
    // rewrites the live-findings block, which would make the fixture worktree
    // dirty and quietly change what `triage::gate` sees.
    todo_path: join(CONFIG, 'findings.md'),
    // A fixture daemon coming back up should not relaunch throwaway sessions.
    auto_resume: false,
    output_language: 'English',
  };
  writeFileSync(join(CONFIG, 'config.json'), JSON.stringify(cfg, null, 2) + '\n');
  done(`${join(CONFIG, 'config.json')}`);
}

// ── main ────────────────────────────────────────────────────────────────────

const viewer = (() => {
  try {
    return gh('api', 'user', '--jq', '.login');
  } catch {
    console.error('gh is not authenticated. Run `gh auth login` first.');
    process.exit(1);
  }
})();

if (flag('--destroy')) {
  destroy(viewer);
  process.exit(0);
}

ensureRepo(viewer);
seedMain();
resetBranch(viewer);
const pr = openPr(viewer);
await postThreads(viewer, pr);
const count = verify(viewer, pr, viewer);
writeConfig();

console.log(`
\x1b[1mFixture ready.\x1b[0m ${viewer}/${REPO} PR #${pr}, ${count} thread(s) awaiting you.
  https://github.com/${viewer}/${REPO}/pull/${pr}

Point a daemon at it — its state is entirely separate from your real one, so
sessions, the instance lock and the findings block all land under the fixture:

  ORCHD_CONFIG_DIR=${CONFIG} cargo run -p orchestrator-desktop

Close the real app first if it holds port 7777; the desktop shell falls back to
an ephemeral port, so both can run, but only one can have the default.

Rebuild it fresh before anything destructive (teardown, a resolve run):
  node tools/fixture-pr.mjs
`);
