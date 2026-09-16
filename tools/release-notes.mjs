#!/usr/bin/env node
// What a release says it changed.
//
// `generate_release_notes: true` builds notes from merged pull requests. This
// repo commits straight to `main`, so there are none — and every release said the
// same single line: a compare link, for 8 commits in v2026.9.18 and for 34 in
// v2026.9.17. Nobody reading a release could tell what had changed in it.
//
// The raw material was already there and already good, because the Style section
// made it so: subjects are imperative one-liners and bodies carry the why. So the
// notes are built from the log rather than written a second time by hand — a
// changelog kept beside git is the list-written-twice problem `check-traps.mjs`
// exists to solve.
//
// **Two passes, and the second is optional.** The mechanical pass picks by path:
// a commit touching `crates/`, `desktop/`, `web/` or `skills/` changed what ships,
// and one touching only `.github/`, `tools/` or `docs/` did not. That is the floor,
// and a release can always be cut from it. The agent pass then narrows *that* to
// what a person upgrading would notice and says it in their terms — and it is
// allowed to fail, because a release must not wait on a model call.
//
// **The agent chooses and phrases; this script owns completeness.** Every commit
// it does not cite falls into the collapsed block automatically, so the union is
// always the whole range. `--check` asserts exactly that, which is what makes a
// drafted line something a script can refuse rather than something to trust: a
// cited sha outside the range, or a commit appearing twice or not at all, fails.
//
// Run by `mise run release`, which prints the notes and waits for you before the
// tag exists.
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';

const REPO = 'kbarendrecht/orchestrator';

/** How long the agent gets before the release falls back to the mechanical notes. */
const AGENT_TIMEOUT_MS = 120_000;

const git = (...args) => execFileSync('git', args, { encoding: 'utf8', maxBuffer: 64 << 20 });

/** The commits in a range, oldest first, with the paths each touched. */
function commits(range) {
  const raw = git('log', '--reverse', '--format=%H%x1f%s%x1f%b%x1e', range);
  return raw
    .split('\x1e')
    .map((rec) => rec.replace(/^\n/, ''))
    .filter((rec) => rec.trim())
    .map((rec) => {
      const [sha, subject, body] = rec.split('\x1f');
      const files = git('show', '--name-only', '--format=', sha).split('\n').filter(Boolean);
      return { sha, short: sha.slice(0, 7), subject, body: body ?? '', files };
    })
    // The version bump is not a change anybody upgrades for.
    .filter((c) => !/^Release \d/.test(c.subject));
}

/** The trailers every commit here carries, which are not prose. */
const TRAILER = /^(Generated with|via \[Happy\]|Co-Authored-By:|Signed-off-by:)/;

/** The body's first sentence, or null. Capped, because a release list is scanned. */
function why(body, cap = 200) {
  const prose = body
    .split('\n')
    .filter((l) => !TRAILER.test(l))
    .join(' ')
    .replace(/\s+/g, ' ')
    .trim();
  if (!prose) return null;
  const sentence = (prose.match(/^(.*?[.!?])(\s|$)/)?.[1] ?? prose).trim();
  if (sentence.length <= cap) return sentence;
  // Cut at the last clause boundary that fits, so the line ends somewhere a
  // reader would pause rather than mid-word.
  const head = sentence.slice(0, cap);
  const at = Math.max(head.lastIndexOf(' — '), head.lastIndexOf(', '), head.lastIndexOf('; '));
  return `${(at > 60 ? head.slice(0, at) : head).trim()}…`;
}

/** Did this commit change what ships, by the paths it touched? */
const SHIPS = ['crates/', 'desktop/', 'web/', 'skills/'];
const ships = (c) => c.files.some((f) => SHIPS.some((p) => f.startsWith(p)));

/** What the agent is asked, and the definition it is asked to apply. */
function prompt(cs) {
  const body = cs
    .map((c) => `--- ${c.short}\n${c.subject}\n${why(c.body, 600) ?? '(no body)'}`)
    .join('\n');
  return `You are writing the release notes for orchd, a desktop app and daemon that
hosts Claude Code sessions over a monorepo. Below are the commits in this release.

Pick only the changes a person upgrading would notice, and write one line each.

A change is worth a line when it alters what they see or what they must do:
new behaviour, a failure they could have hit, a changed default, something they
have to reconfigure. It is not worth a line when it is a refactor, a test, a
gate, a doc, a dependency bump, or work only this repo's authors can observe.
Most releases have between two and six such lines. Some have none, and answering
with none is correct when that is true.

Write each line as what changed for them, not as what was done to the code. Use
the body to say why it matters when it does. Do not invent anything that is not
in the commit below it.

Answer with nothing but the lines, one per line, in this exact form:

- <sentence> (<the 7-character sha>)

${body}`;
}

/** The agent's picks, or null when it could not answer usefully. */
function drafted(cs) {
  let out;
  try {
    out = execFileSync('claude', ['-p', prompt(cs)], {
      encoding: 'utf8',
      timeout: AGENT_TIMEOUT_MS,
      stdio: ['ignore', 'pipe', 'pipe'],
      maxBuffer: 8 << 20,
    });
  } catch (e) {
    process.stderr.write(`release-notes: the draft did not come back (${e.code ?? e.message}); using the picks the paths give\n`);
    return null;
  }
  const known = new Map(cs.map((c) => [c.short, c]));
  const lines = [];
  for (const line of out.split('\n')) {
    const m = /^[-*]\s+(.*?)\s*\(([0-9a-f]{7,40})\)\s*$/.exec(line.trim());
    if (!m) continue;
    const short = m[2].slice(0, 7);
    // A sha outside the range is the failure mode worth refusing: it is what a
    // fabricated line looks like.
    if (!known.has(short)) {
      process.stderr.write(`release-notes: the draft cited ${short}, which is not in this range; using the picks the paths give\n`);
      return null;
    }
    lines.push({ text: m[1], short });
  }
  return lines.length ? lines : null;
}

/** The markdown, with every commit in the range appearing exactly once. */
function render(cs, picked, range) {
  const cited = new Set(picked.map((p) => p.short));
  const rest = cs.filter((c) => !cited.has(c.short));
  const out = [];
  if (picked.length) {
    out.push('### What changed', '');
    for (const p of picked) out.push(`- ${p.text} (${p.short})`);
    out.push('');
  }
  if (rest.length) {
    out.push('<details><summary>Everything else in this release</summary>', '');
    for (const c of rest) out.push(`- ${c.subject} (${c.short})`);
    out.push('', '</details>', '');
  }
  const [from, to] = range.split('..');
  out.push(`**Full Changelog**: https://github.com/${REPO}/compare/${from}...${to}`);
  return out.join('\n');
}

/** Every commit in the range, once and only once. The invariant, checkable. */
function check(md, cs) {
  const seen = [...md.matchAll(/\(([0-9a-f]{7})\)/g)].map((m) => m[1]);
  const problems = [];
  const inRange = new Set(cs.map((c) => c.short));
  for (const s of seen) if (!inRange.has(s)) problems.push(`${s} is cited and is not in this range`);
  for (const c of cs) {
    const n = seen.filter((s) => s === c.short).length;
    if (n === 0) problems.push(`${c.short} ${c.subject} — in the range and in no section`);
    if (n > 1) problems.push(`${c.short} appears ${n} times`);
  }
  return problems;
}

function main(argv) {
  const flags = new Set(argv.filter((a) => a.startsWith('--')));
  const args = argv.filter((a) => !a.startsWith('--'));
  // Default to the last tag, which is what a release is measured from.
  const range = args[0] ?? `${git('describe', '--tags', '--abbrev=0').trim()}..HEAD`;
  const cs = commits(range);
  if (!cs.length) {
    process.stderr.write(`release-notes: no commits in ${range}\n`);
    process.exit(1);
  }

  if (flags.has('--check')) {
    const problems = check(readFileSync(args[1], 'utf8'), cs);
    if (problems.length) {
      process.stderr.write(`release-notes: ${problems.join('\n  ')}\n`);
      process.exit(1);
    }
    process.stdout.write(`release-notes: ${cs.length} commit(s), each in exactly one section\n`);
    return;
  }

  const picked = (flags.has('--no-agent') ? null : drafted(cs))
    ?? cs.filter(ships).map((c) => ({ text: `**${c.subject}**${why(c.body) ? ` — ${why(c.body)}` : ''}`, short: c.short }));

  const md = render(cs, picked, range);
  const problems = check(md, cs);
  if (problems.length) {
    process.stderr.write(`release-notes: the notes do not cover the range\n  ${problems.join('\n  ')}\n`);
    process.exit(1);
  }
  process.stdout.write(`${md}\n`);
}

main(process.argv.slice(2));
