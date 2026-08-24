#!/usr/bin/env node
// The review queue, out of the box: open PRs in this repo awaiting your review.
//
// This is orchd's *default* `reviews_command`, ejected to the daemon's config dir
// on first run so it is yours to edit. It is deliberately a script rather than
// daemon code: the ranking below is one opinion, and a team's real one lives in
// its own tooling. Replace the body, or point `reviews_command` somewhere else
// entirely — the only contract is the JSON printed on stdout, documented in
// `docs/reviews-json.md`.
//
// No dependencies, on purpose: it runs from the config dir, where there is no
// `package.json` and nothing has been installed. `gh` does the authentication.
//
// Run with cwd set to the main checkout. Prints an empty-but-valid queue when
// nothing is waiting; exits non-zero with a message on stderr when it cannot
// answer at all, which the pane shows as degraded rather than as "no reviews".

'use strict';

const { execFileSync } = require('node:child_process');

const QUERY = `
query($q: String!) {
  viewer { login }
  search(query: $q, type: ISSUE, first: 50) {
    issueCount
    nodes {
      ... on PullRequest {
        number title url isDraft createdAt mergeable
        author { login }
        labels(first: 20) { nodes { name } }
        reviewRequests(first: 20) {
          nodes { requestedReviewer { __typename ... on User { login } ... on Team { slug } } }
        }
        latestReviews(first: 20) { nodes { author { login } state } }
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      }
    }
  }
}`;

/** Run gh, or die with its own stderr — never a bare stack trace. */
function gh(args) {
  try {
    return execFileSync('gh', args, { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 });
  } catch (e) {
    if (e.code === 'ENOENT') {
      die('gh is not installed, so there is no review queue to build');
    }
    const tail = String(e.stderr || '').trim().split('\n').filter(Boolean).pop();
    die(`gh ${args.slice(0, 2).join(' ')} failed: ${tail || 'no stderr'}`);
  }
}

function die(message) {
  process.stderr.write(message + '\n');
  process.exit(1);
}

const ageHours = (createdAt) =>
  Math.round(((Date.now() - Date.parse(createdAt)) / 3.6e6) * 10) / 10;

/** Lower sorts first. The one opinion in this file worth arguing with.
 *
 *  Label names are a convention, not a standard, so `stopper` and `prio` are
 *  guesses at yours — change them, or delete the two branches. */
function rank(labels, requestedOfYou, reviewedByYou) {
  if (labels.has('stopper')) return 0;
  if (labels.has('prio')) return 1;
  if (reviewedByYou) return 4; // you have looked once; a re-read is lighter
  if (requestedOfYou) return 2; // asked of you by name
  return 3; // asked of a team you are in
}

function main() {
  // The repo the daemon is pointed at, not a hardcoded one.
  const repo = JSON.parse(gh(['repo', 'view', '--json', 'nameWithOwner'])).nameWithOwner;
  const q = `repo:${repo} is:open is:pr review-requested:@me`;
  const { data } = JSON.parse(gh(['api', 'graphql', '-f', `query=${QUERY}`, '-F', `q=${q}`]));

  const viewer = data.viewer.login;
  const nodes = data.search.nodes.filter(Boolean);
  const actionable = [];
  const blocked = [];

  for (const pr of nodes) {
    const labels = new Set(pr.labels.nodes.map((l) => l.name.toLowerCase()));
    const requestedOfYou = pr.reviewRequests.nodes.some(
      (r) => r.requestedReviewer && r.requestedReviewer.login === viewer,
    );
    const latest = pr.latestReviews.nodes.filter((r) => r && r.author);
    const reviewedByYou = latest.some((r) => r.author.login === viewer);

    const commit = pr.commits.nodes[0] && pr.commits.nodes[0].commit;
    const rollup = (commit && commit.statusCheckRollup && commit.statusCheckRollup.state) || null;

    // Blocked means waiting on its author, not on you — the pane sinks these.
    const blockers = [];
    if (pr.isDraft) blockers.push('draft');
    if (pr.mergeable === 'CONFLICTING') blockers.push('conflicts');
    if (rollup === 'FAILURE') blockers.push('failing checks');

    const entry = {
      // `checks` and `mergeable` live on `pr`, not on the entry — see the `Pr`
      // shape in docs/reviews-json.md. A level up parses fine and then silently
      // reads as null, which is why that is worth saying here.
      pr: {
        number: pr.number,
        title: pr.title,
        url: pr.url,
        author: (pr.author && pr.author.login) || 'unknown',
        isDraft: pr.isDraft,
        mergeable: pr.mergeable,
        checks: rollup,
      },
      prio: rank(labels, requestedOfYou, reviewedByYou),
      ageHours: ageHours(pr.createdAt),
      // Humans who have already left a review, so you can see whether you are the
      // first pair of eyes or the third.
      reviewers: new Set(latest.map((r) => r.author.login)).size,
      needsReReview: reviewedByYou,
      blockers,
    };
    (blockers.length ? blocked : actionable).push(entry);
  }

  const order = (a, b) => a.prio - b.prio || b.ageHours - a.ageHours;
  actionable.sort(order);
  blocked.sort(order);

  process.stdout.write(
    JSON.stringify({
      forLogin: viewer,
      total: data.search.issueCount,
      skipped: data.search.issueCount - nodes.length,
      actionable,
      blocked,
    }) + '\n',
  );
}

main();
