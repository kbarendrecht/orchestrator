---
name: review
description: Answer a pull request's review threads end to end with a human deciding in the orchestrator's overlay — read the threads and propose options, make the changes they pick, then post the replies. Use when the orchestrator starts a review session, or when somebody asks you to work a PR's review feedback with the overlay driving.
---

# Review a PR's threads — the orchd overlay session

`/orchd:review <pr>`. One session answers the whole of a PR: you read the
threads, the human picks a way to resolve each one in the review overlay, you make
the changes they picked, and you post.

This is **overlay-driven**, not a pane you are steered from. You do not sort the
threads or choose between options — you propose, the human decides in the overlay,
and their decisions come back to you over the one channel that reaches them. So there
is no numbered list to keep stable and no `AskUserQuestion`: the cards are the UI.

## First, ask the daemon what this is

Everything below needs values only the daemon has. One call, before anything else:

```bash
curl -sS "$ORCH_URL/api/pr/$PR/triage-context" -H "x-orch-token: $ORCH_POST_TOKEN"
```

```jsonc
{
  "pr": 35264,
  "owner": "…", "repo": "…",   // the repo the PR is on
  "login": "…",                // you, on the forge
  "answerable": ["PRRT_…"],    // the threads still waiting on you — the set you work
  "may_push": true,            // may you amend and force-push this branch
  "language": "…",             // what to write replies and stories in
  "tracker": true,             // false: never propose `story+reply`
  "tracker_host": "…",         // the tracker's host: never link to it yourself
  "upstream": "…",             // the ref this branch is measured against
  "proposals_url": "…"         // where phase 1 ends
}
```

`$PR` is the number you were invoked with. `$ORCH_URL`, `$ORCH_POST_TOKEN` and
`$ORCH_ASK_TOKEN` are in your environment already. If they are not, you are not in a session the daemon started:
say so and stop, because nothing here can be handed anywhere.

Below, `$PR`, `$OWNER`, `$REPO`, `$LOGIN`, `$ANSWERABLE`, `$MAY_PUSH`, `$LANGUAGE`,
`$TRACKER`, `$TRACKER_HOST`, `$UPSTREAM` and `$PROPOSALS_URL` are the fields of that
answer.

Three phases, in order: **read** (write nothing), **change** (only what they picked),
**post** (only on their go). Do not run ahead of the human between them.

## Where you are

The daemon created a worktree pinned to this PR's head branch and started you inside
it. Do not switch branches and do not create one — the run stays on the PR's head ref.
Confirm rather than assume: `git rev-parse --abbrev-ref HEAD` against `headRefName`
from the fetch. A mismatch is a stop, not something to correct by switching.
`$MAY_PUSH` not `true` → stop, it is a branch you may not force-push. That is the
daemon's answer about push access, and the only one to use: do not compare the head
repo's owner with `$LOGIN`, which refuses every org PR, whose owner is the org.

# Phase 1 — Read (write nothing)

Read only. Not the worktree, not a commit, not a comment — you work out what each
thread asks and *how it could be answered*, and hand that to the daemon. Making changes
here is what used to make this slow.

## Fetch

```bash
gh api graphql -f query='
query($owner:String!,$repo:String!,$num:Int!){
  repository(owner:$owner,name:$repo){ pullRequest(number:$num){
    headRefName headRefOid headRepositoryOwner{login}
    reviewThreads(first:100){ nodes{ id isResolved isOutdated
      comments(first:20){ nodes{ databaseId author{login} body path line url diffHunk } } } } } } }
' -F owner=$OWNER -F repo=$REPO -F num=$PR
```

Plus `gh pr view $PR --json reviews,comments` for review-level bodies. Those often
carry a `path` and `line` too — keep them when they do.

**Work exactly the threads in `$ANSWERABLE`**, and leave every other one alone. Which
threads are already answered is the daemon's call, not yours: answered means the last
comment is yours, or you 👍'd the last comment, and the fetch above does not ask for
reactions, so a rule you derived from it would offer back every thread settled with a
👍. `$ANSWERABLE` keeps outdated threads (the code moved, the point may still stand)
and drops resolved ones. A thread in it that `$LOGIN` already replied to, where the
reviewer came back, is `continued` — read it as a conversation, lead the `read` with
the earlier commitment, and set `"continued": true`.

Record `headRefOid` before anything else — it goes back as `base_sha`, and is how the
daemon notices a force-push that lands while the human is deciding.

## Read and propose solutions

Read the code at each thread before judging it, not just the diff. The `read` is the one
thing the human sees on every card, so keep it **terse**: a sentence, or a few when the
thread earns it. Say whether the reviewer is right and what turns on it — not a walk
through the code, not a plan for the fix.

Per thread, offer the **ways to resolve it**, not wordings of one reply. Each option is a
distinct solution the human might pick; you carry out the one they choose in phase 2.

- Lead with `agree` — **do what they asked, then a thumbs up and no words** — wherever the
  reviewer is simply right and there is nothing left to decide. Agreeing is not the same as
  doing nothing: if the thread asks for a change, taking this option is a promise to make it.
- Otherwise offer one to three **distinct solutions**, each a real approach: *make the
  rate per-country*, *read it from the order downstream*, *remove it altogether*. The
  `label` names the approach; the `reply` is what you would say back if it is taken.
- **Where your read contains a judgement, offer the other side of it** — the case the
  reviewer is *not* right, drafted properly, not as a strawman.

Do not describe *how* to implement a solution and do not write any code yet. The daemon
appends one fixed option — the human's own answer, in their words — so do not include it
yourself. Recommend exactly one option per thread by index.

Replies: match the thread's language, default to $LANGUAGE; say what will change and
why, no mechanics, one or two sentences; no footer, the daemon appends `(via
orchestrator)`; an `agree` option has no reply text.

## Hand off the proposals

One POST — this is what fills the overlay's cards.

```bash
curl -sS -X POST '$PROPOSALS_URL' \
  -H "x-orch-token: $ORCH_POST_TOKEN" \
  -H 'content-type: application/json' \
  --data-binary @proposals.json
```

```jsonc
{
  "base_sha": "…",              // headRefOid, recorded before you read anything
  "threads": [
    { "thread_id": "PRRT_…",
      "continued": false,
      "read": "…",              // terse: is the reviewer right, and what turns on it
      "recommend": 1,
      "options": [
        { "label": "Agree", "sub": "make the change, thumbs up", "stance": "agree", "reply": null },
        { "label": "Make the rate per-country", "sub": "the approach the reviewer points at",
          "stance": "reply", "reply": "…" },
        { "label": "Track it as follow-up", "sub": "out of scope here",
          "stance": "story", "story": { "title": "…", "body": "…" }, "reply": "Tracked as {story}." }
      ] }
  ]
}
```

- `stance` is what you say back: `agree` (make the change, thumbs up, no words), `reply`
  (words), or `story` (a follow-up the daemon files, and a reply with its link). An
  `agree` option carries no `reply`; a `reply` or `story` option must have one; a `story`
  option must have a `story`.
- **No patches.** You are not writing code in this phase.
- A `story+reply` reply must contain the literal `{story}`. The daemon files the story and
  replaces it with a markdown link when it posts — you never file one and never write a
  tracker link yourself. `$TRACKER` false means **never propose `story+reply`**: there is
  nowhere to file one and the daemon would refuse it, so a fair but out-of-scope point goes
  in a plain reply rather than a promised story.
- A `story` is `title` and `body` only, in $LANGUAGE, no em dashes and no internal path
  or label references. Every thread in `$ANSWERABLE` needs an entry, and no other:
  the daemon refuses a set with one missing or one extra.
- Do not send `hunk` or the current code — the daemon reads `diffHunk` from GitHub.

# Phase 2 — Change (only what they picked)

Post the proposals, then **wait for the human's decisions**. You reach them one way, and
it blocks until they answer:

```bash
ASK=$(curl -sS -X POST -H 'content-type: application/json' -H "x-orch-ask: $ORCH_ASK_TOKEN" \
  -d '{"question":"Waiting for your decisions in the review overlay.",
       "options":[{"value":"decisions","label":"Decisions submitted","free":true}]}' \
  "$ORCH_URL/api/session/$ORCH_SESSION_ID/ask" | jq -r .ask)

while :; do
  R=$(curl -sS -H "x-orch-ask: $ORCH_ASK_TOKEN" "$ORCH_URL/api/session/$ORCH_SESSION_ID/ask/$ASK/wait")
  [ "$(jq -r .answered <<<"$R")" = true ] && break
done
DECISIONS=$(jq -r .text <<<"$R")   # the JSON below
```

`answered: false` is normal — the human is still deciding. Keep looping; a human takes
minutes and the loop is what makes that safe.

The overlay answers with one decision per thread:

```jsonc
{
  "decisions": [
    { "thread_id": "PRRT_…",
      "stance": "agree" | "reply" | "story" | "skip",
      "solution": "Make the rate per-country",   // the label of the option they picked
      "reply": "…",                              // the final reply, as the human edited it
      "note": "…" }                              // present only when they wrote their own
  ]
}
```

Now do **only** what each decision says:

- **skip** — nothing. Not a reply, not a reaction.
- **agree** — **make the change the reviewer asked for**, by the same route as `reply`
  below, then a 👍 in phase 3 and no written reply. Agreeing and then changing nothing is
  the one outcome this must never produce: it tells the reviewer they were right and leaves
  the code as it was. Only where the thread asks for nothing — praise, a question already
  answered by the code — is there no change to make, and then say so in your report.
- **reply / their own note** — if the solution needs a code change, make it: edit the
  worktree, **amend into the commit that owns each line** (`git log -S`/blame the line to
  find it), run this repo's checks, then push
  `--force-with-lease`. The push guard denies plain `--force` and any push to the base
  branch — those denials are correct. A `note` is the human's own instruction; follow it. Some
  reply solutions change no code (a pushback, an explanation) — then there is nothing to
  build, only the reply to post in phase 3.
- **story** — nothing yet. The daemon files it in phase 3, before it posts the reply.

Prove each change landed by the reviewer's own claim, not by your edit succeeding: "called
twice" → grep it, show it is called once; "these tests miss X" → run the test, show it
fails without the fix. A claim you cannot re-prove is one to report and hold, not to push.

Keep each change to the thread it answers. Do not touch a thread the human skipped.

## Out of scope: the daemon files the story

A `story` decision gets a story, and **the daemon files it**, not you: it is filed when
you post that thread's reply in phase 3, from the title and body the human approved on
the card, before the reply goes out with `{story}` replaced by a markdown link to it.
The daemon appends the `Source:` line that lets a retry find the story rather than file
a second one, and checks the link it puts in against `$TRACKER_HOST`.

So do not call the tracker, do not search it, and do not write a tracker link into any
reply: the daemon refuses a reply that links to `$TRACKER_HOST` itself, and one that
names `{story}` without a story to file.

# Phase 3 — Post

The code is pushed. Post the replies now, without asking again.

**There is one gate and you are past it.** The decisions you were handed in phase 2 are
the human's last word: they carry the reply for each thread, as edited, and sending them
authorised the change *and* what gets said about it. There is no second ask — a screen
that showed the same replies back and asked "really?" is what this flow removed. So the
replies to post are the ones in `$DECISIONS`, verbatim.

Say nothing about a thread whose decision was `skip`.

## One call per thread

```bash
curl -sS -X POST -H "x-orch-ask: $ORCH_ASK_TOKEN" -H 'content-type: application/json' \
  -d '{"reply":"…"}' \
  "$ORCH_URL/api/session/$ORCH_SESSION_ID/thread/<thread_id>/reply"
```

- **agree** — send `{}`, with no `reply` at all. The change it agreed to was made and
  pushed in phase 2, so a 👍 is the whole of what is said. The daemon puts it on the
  thread's last comment, which is the one "answered" reads.
- **reply** — send the reply from `$DECISIONS`, verbatim.
- **story** — send the reply from `$DECISIONS` verbatim, `{story}` and all, with the
  story from the option you proposed for that thread beside it:
  `{"reply":"…{story}…","story":{"title":"…","body":"…"}}`. The daemon files it, puts
  the link in and posts. The card's own draft wins where it still has it, so the story is
  the one the human approved. An answer of `held` with a reason means no story could be
  filed and nothing was said; report it and leave the thread.

**Not `gh`, and the reason is not tidiness.** The daemon appends `(via orchestrator)` to
every reply and compares the footed text against what is already on the thread, so a
retry says nothing twice — a reply posted without the footer is one the next retry posts
again. It also files the story a reply links to. Neither is yours to remember. (Whether a
thread still waits on you is not read off the footer: it is the last comment's author and
your 👍 on it.)

The answer says what happened: `posted`, `reacted`, `already` (those words were already
there, which is a success and not a retry to make), and `held` with the reason when
nothing went out.

## Then one call for the re-request

```bash
curl -sS -X POST -H "x-orch-ask: $ORCH_ASK_TOKEN" \
  "$ORCH_URL/api/session/$ORCH_SESSION_ID/rerequest"
```

Once, after the last reply. Which reviewers are asked is the daemon's: it refetches the
threads and asks each reviewer who had a thread in this review and has nothing of theirs
still awaiting an answer. So a reply that did not go out holds its author back on its
own, a thread somebody opened while you were working holds them back too, and a reviewer
this review never touched is not asked.

The answer is three lists — `asked`, `held` (how many threads still hold each one back)
and `failed` (one login's refusal, not the call's: a bot cannot be a requested reviewer
at all). Report them. Do not re-run it per reviewer and do not fall back to `gh`.

Resolving the threads stays the reviewer's button — never resolve one yourself.

## Not your job, and when to stop

- **Resolving threads.** Closing a conversation is the comment author's button.
- CI still red or the branch behind `$UPSTREAM` → say so and stop. That is `fix-pr`'s
  job; do not rebase for it and do not start one yourself. Phase 4 hands it over.

When you are done, a short report in the pane: what you changed, what you posted, and any
thread you held and why.

# Phase 4 — Hand over

Last thing, after the report. One call, and it ends the session:

```bash
curl -sS -X POST -H "x-orch-ask: $ORCH_ASK_TOKEN" "$ORCH_URL/api/session/$ORCH_SESSION_ID/handoff"
```

Do not report its answer or act on it — the pty is closing as it returns, so a `curl:
(52)` here is the call having worked.

**Say only that you are finished.** The daemon re-reads the PR itself and decides whether
a `fix-pr` run picks up the checks; it does not read this report and it does not take your
word for the check state. So there is nothing to argue here and nothing to withhold: call
it whether you think the PR is green, red, or you never got far enough to know.

Call it exactly once, and only after phase 3. Not calling it is the one real failure —
the overlay waits on this to know the review is over, and without it the human is left
watching a screen that says you are still applying.
