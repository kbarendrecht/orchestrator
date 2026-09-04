---
name: triage
description: Read a PR's review threads and propose a way to answer each one, for a human to pick from. Reads only: no commits, no comments, no git. Use when somebody asks you to triage or review the feedback on a pull request, and the orchestrator is there to hand the proposals to.
---

# Triage review feedback

`/orchd:triage <pr>`. This pass **resolves nothing**. It reads, judges and
proposes; the human resolves, and a later pass writes the code.

**Read only. Write nothing, and do not touch git.** Not the worktree, not a
commit, not a comment, not a branch, and no scratch worktree either. Making
changes here is what used to make triage slow; the whole point of this pass is
that it does not.

## First, ask the daemon what this is

Everything below needs values only the daemon has. One call, before anything
else:

```bash
curl -sS "$ORCH_URL/api/pr/$PR/triage-context" -H "x-orch-token: $ORCH_POST_TOKEN"
```

```jsonc
{
  "pr": 35264,
  "owner": "…", "repo": "…",   // the repo the PR is on
  "login": "…",                // you: a thread whose last comment is yours is answered
  "language": "…",             // what to write replies and stories in
  "tracker": true,             // false: never propose `story+reply`
  "proposals_url": "…",        // where this pass ends
  "progress_url": "…"          // one POST per thread, as you go
}
```

`$ORCH_URL` and `$ORCH_POST_TOKEN` are in your environment already. If they are
not, you are not in a session the daemon started: say so and stop, because
nothing here can be handed anywhere.

Below, `$OWNER`, `$REPO`, `$LOGIN`, `$LANGUAGE`, `$PROGRESS_URL` and
`$PROPOSALS_URL` are the fields of that answer.

## Fetch

```bash
gh api graphql -f query='
query($owner:String!,$repo:String!,$num:Int!){
  repository(owner:$owner,name:$repo){ pullRequest(number:$num){
    headRefName headRefOid
    reviewThreads(first:100){ nodes{ id isResolved isOutdated
      comments(first:20){ nodes{ databaseId author{login} body path line url diffHunk } } } } } } }
' -F owner=$OWNER -F repo=$REPO -F num=$PR
```

Plus `gh pr view $PR --json reviews,comments` for review-level bodies. Those often
carry a `path` and `line` too — keep them when they do.

Skip `isResolved`. **Keep `isOutdated`**: the code moved, the point may still stand. A
thread whose last comment is `$LOGIN`'s is already answered; leave it alone.

**A thread `$LOGIN` already replied to, where the reviewer came back, is `continued`.**
Read it as a conversation, not a fresh request: what was promised, what they have come back
with, and whether their point lands. It matters more here than anywhere else that the reply
is consistent with what was already said — going back on it, or ignoring that it was said,
is worse in public than being wrong once. Lead the `read` with that, name the earlier
commitment, and set `"continued": true` on the thread.

Record `headRefOid` before anything else — it goes back as `base_sha`. It is how the daemon
notices a force-push that happened while you were reading and drops decisions made against
code that has since moved.

## Read

Read the code at each thread before judging it, not just the diff. You may read anything in
the worktree; you may not change it. Then a numbered list, one line each:
`<n>. <path>:<line>, <what they want> → straightforward | needs a decision`.

- **straightforward**: they are right and the fix carries no behaviour decision.
- **needs a decision**: a real question, a design call, or you think they are wrong.

**Post where you are the moment you finish a thread, before you start the next
one.** Somebody is watching a bar that reads `triaging thread 2 of 5`, and it is
the only sign the pass is moving. So the first POST goes out when thread one is
done, not when the reading is:

```bash
curl -sS -X POST "$PROGRESS_URL" -H "x-orch-token: $ORCH_POST_TOKEN" \
  -H 'content-type: application/json' -d '{"done":1,"total":5}'
```

**Never send them together at the end.** A loop that posts 1, 2 and 3 once the
reading is over is the same as sending nothing: the bar sat at nought for the
whole pass and then jumped to done in one frame. Measured on the first real run
of this skill, which did exactly that.

`total` is **your** count: the threads you decided to read, not every thread on
the PR. An answered thread you skipped was never going to be reached, and a bar
counting it would stop short of its own end.

Both kinds get a card. The difference is only what you recommend and how much you explain
— nothing is auto-handled, so there is no bar a thread has to clear to reach the human.

A reviewer's `suggestion` block is a claim, not an instruction. It is right or it is not.

The `read` is context the human can open on a card, so keep it **terse**: a sentence, or a
few when the thread genuinely earns it. Give the human what they need to judge the thread
themselves: the fact the comment turns on, and what depends on it. State a plain fact plainly —
a real defect is a real defect, in one line. On a judgement or a design call, lay out the
trade-off, what each side buys and costs, so the decision is theirs to make on full
information. The read is the ground someone reasons from before they have a view of their own;
its job is to inform that judgement. Keep to the facts and the stakes, not a walk through the
code or a plan for the fix. When a claim cannot be confirmed by reading, say what is
unconfirmed and mark the thread *needs a decision*.

## Offer solutions

Per thread, offer the **ways to resolve it**, not wordings of one reply. Each option is a
distinct solution the human might pick; a later pass carries out the one they choose.

- Lead with `agree` — a thumbs up, **no words, no change** — wherever the reviewer is simply
  right and there is nothing to decide or write. It is the option people pick most.
- Otherwise offer one to three **distinct solutions**, each a real approach: *make the rate
  per-country*, *read it from the order downstream*, *remove it altogether*. The `label` names
  the approach in a few words; the `reply` is what you would say back if that approach is
  taken.
- **Where your read contains a judgement, offer the other side of it.** If you conclude the
  reviewer is right, one option must be the case that they are not — drafted properly, not as
  a strawman. The human is in the loop precisely because you can be wrong about who is right.

Do not describe *how* to implement a solution and do not write any code — the label and the
reply are enough for the human to choose, and the later pass works out the change. The daemon
appends one fixed option, so **do not include it yourself**: the human's own answer, in their
words. Skip is an action, not an option — leave it out too.

Recommend exactly one option per thread by index. Recommending is not deciding — the human
reads your options and picks.

### Replies

- Match the thread's language; default to $LANGUAGE when unclear. Keep technical
  terms in their conventional form.
- Say what will change and why. No mechanics: no rebasing, no amending, no "good catch", no
  restating their comment back at them.
- One or two sentences. A pushback states the fact that refutes it and where to see it.
- **No footer.** The daemon appends `(via orchestrator)`. Yours would be doubled.
- An `agree` option has no reply text at all. Do not write one "just in case".

## Hand off

One POST, then exit. A run that exits without posting is a failed run.

```bash
curl -sS -X POST "$PROPOSALS_URL" \
  -H "x-orch-token: $ORCH_POST_TOKEN" \
  -H 'content-type: application/json' \
  --data-binary @proposals.json
```

```jsonc
{
  "base_sha": "…",              // headRefOid, recorded before you read anything
  "threads": [
    {
      "thread_id": "PRRT_…",
      "continued": false,       // true when you already replied and they came back
      "read": "…",              // terse: the fact the comment turns on and what depends on it,
                                // for the human to judge. on a continued thread, lead with
                                // the earlier commitment
      "recommend": 0,           // index into options
      "options": [
        { "label": "Agree",
          "sub": "respond with thumbs up",
          "stance": "agree",
          "reply": null },
        { "label": "Make the rate per-country",
          "sub": "the approach the reviewer is pointing at",
          "stance": "reply",
          "reply": "…" },
        { "label": "It is set per-order downstream",
          "sub": "the case for leaving it — the reviewer is not right here",
          "stance": "reply",
          "reply": "…" },
        { "label": "Track it as follow-up",
          "sub": "out of scope here — file it instead of promising it",
          "stance": "story",
          "story": { "title": "…", "body": "…" },
          "reply": "Tracked as {story}." }
      ]
    }
  ]
}
```

- `stance` is what you are saying back, and only that: `agree` (a thumbs up on the opening
  comment, no words), `reply` (words), or `story` (file a follow-up and reply with its id).
  It must match the option's words: an `agree` option carries no `reply`, a `reply` or
  `story` option must have one, and a `story` option must have a `story`. The daemon rejects
  the set if they disagree — it is how "do A but say B" is kept impossible.
- **No patches, no `verified`.** You are not writing or proving code in this pass. Describe
  the solution in the `label` and answer it in the `reply`; the later pass writes the change
  for whichever option the human picks.
- A `story+reply` reply must contain the literal `{story}`, which the daemon replaces with
  the id once the story exists. It cannot be written in advance.
- `story+reply` exists only when the context says `"tracker": true`. When it is
  false, never propose it: the daemon would refuse the option, and a refused
  option should not reach a card.
- A `story` is `title` and `body` only. Write both in $LANGUAGE. **No em
  dashes and no internal path or label references** — the tracker rejects the first
  and nobody outside this session understands the second. Say what the follow-up work is, and
  why it is out of scope for this PR; the daemon appends the link back to the thread, so
  do not write one yourself.
- **Every unresolved thread needs an entry.** A thread you silently drop is the one
  failure the human cannot see.
- Do not send `hunk` or the current code — the daemon reads `diffHunk` from GitHub so the
  card shows the real anchor rather than your transcription of it.

## Not your job

The daemon and a later pass do these once the human approves. Do not do them, and do not
offer them:

- Writing to the worktree, committing, amending, rebasing, pushing.
- Posting replies or reactions; re-requesting reviewers.
- **Resolving threads.** Closing a conversation is the comment author's button.

CI red or the branch behind its base → say so in your final message and stop. That is
`/orchd:green`'s job, not this pass.
