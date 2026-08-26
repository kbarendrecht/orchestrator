# Review a PR's threads — the orchd overlay session

One session answers the whole of PR **{{PR}}** of `{{OWNER}}/{{REPO}}`: you read the
threads, the human picks a way to resolve each one in the review overlay, you make
the changes they picked, and you post. Vendored here so the daemon carries its own
copy — substituted and written to a file the daemon owns, and you are told to read
that file. Nothing is looked up on your command path.

This is **overlay-driven**, not a pane you are steered from. You do not sort the
threads or choose between options — you propose, the human decides in the overlay,
and their decisions come back to you over the one channel that reaches them. So there
is no numbered list to keep stable and no `AskUserQuestion`: the cards are the UI.

Placeholders `{{PR}}`, `{{OWNER}}`, `{{REPO}}`, `{{LOGIN}}`, `{{UPSTREAM}}`,
`{{PROPOSALS_URL}}` and `{{ASK_BASE}}` are filled in by the daemon before you read this.

Three phases, in order: **read** (write nothing), **change** (only what they picked),
**post** (only on their go). Do not run ahead of the human between them.

## Where you are

The daemon created a worktree pinned to this PR's head branch and started you inside
it. Do not switch branches and do not create one — the run stays on the PR's head ref.
Confirm rather than assume: `git rev-parse --abbrev-ref HEAD` against `headRefName`
from the fetch. A mismatch is a stop, not something to correct by switching. Head owner
not `{{LOGIN}}` → stop, it is someone else's branch to force-push.

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
' -F owner={{OWNER}} -F repo={{REPO}} -F num={{PR}}
```

Plus `gh pr view {{PR}} --json reviews,comments` for review-level bodies. Those often
carry a `path` and `line` too — keep them when they do.

Skip `isResolved`. **Keep `isOutdated`**: the code moved, the point may still stand. A
thread whose last comment is `{{LOGIN}}`'s is already answered; leave it alone. A thread
`{{LOGIN}}` already replied to where the reviewer came back is `continued` — read it as a
conversation, lead the `read` with the earlier commitment, and set `"continued": true`.

Record `headRefOid` before anything else — it goes back as `base_sha`, and is how the
daemon notices a force-push that lands while the human is deciding.

## Read and propose solutions

Read the code at each thread before judging it, not just the diff. The `read` is the one
thing the human sees on every card, so keep it **terse**: a sentence, or a few when the
thread earns it. Say whether the reviewer is right and what turns on it — not a walk
through the code, not a plan for the fix.

Per thread, offer the **ways to resolve it**, not wordings of one reply. Each option is a
distinct solution the human might pick; you carry out the one they choose in phase 2.

- Lead with `agree` — a thumbs up, **no words, no change** — wherever the reviewer is
  simply right and there is nothing to decide or write.
- Otherwise offer one to three **distinct solutions**, each a real approach: *make the
  rate per-country*, *read it from the order downstream*, *remove it altogether*. The
  `label` names the approach; the `reply` is what you would say back if it is taken.
- **Where your read contains a judgement, offer the other side of it** — the case the
  reviewer is *not* right, drafted properly, not as a strawman.

Do not describe *how* to implement a solution and do not write any code yet. The daemon
appends one fixed option — the human's own answer, in their words — so do not include it
yourself. Recommend exactly one option per thread by index.

Replies: match the thread's language, default to {{LANGUAGE}}; say what will change and
why, no mechanics, one or two sentences; no footer, the daemon appends `(via
orchestrator)`; an `agree` option has no reply text.

## Hand off the proposals

One POST — this is what fills the overlay's cards.

```bash
curl -sS -X POST '{{PROPOSALS_URL}}' \
  -H "x-orch-token: $ORCHD_TOKEN" \
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
        { "label": "Agree", "sub": "respond with thumbs up", "stance": "agree", "reply": null },
        { "label": "Make the rate per-country", "sub": "the approach the reviewer points at",
          "stance": "reply", "reply": "…" },
        { "label": "Track it as follow-up", "sub": "out of scope here",
          "stance": "story", "story": { "title": "…", "body": "…" }, "reply": "Tracked as {story}." }
      ] }
  ]
}
```

- `stance` is what you say back: `agree` (thumbs up, no words), `reply` (words), or
  `story` (file a follow-up and reply with its id). An `agree` option carries no `reply`;
  a `reply` or `story` option must have one; a `story` option must have a `story`.
- **No patches.** You are not writing code in this phase.
- A `story+reply` reply must contain the literal `{story}`, replaced with the id once the
  story exists. {{TRACKER}}
- A `story` is `title` and `body` only, in {{LANGUAGE}}, no em dashes and no internal path
  or label references. Every unresolved thread needs an entry.
- Do not send `hunk` or the current code — the daemon reads `diffHunk` from GitHub.

# Phase 2 — Change (only what they picked)

Post the proposals, then **wait for the human's decisions**. You reach them one way, and
it blocks until they answer:

```bash
ASK=$(curl -sS -X POST -H 'content-type: application/json' -H "x-orch-ask: $ORCH_ASK_TOKEN" \
  -d '{"question":"Waiting for your decisions in the review overlay.",
       "options":[{"value":"decisions","label":"Decisions submitted","free":true}]}' \
  "{{ASK_BASE}}/$ORCH_SESSION_ID/ask" | jq -r .ask)

while :; do
  R=$(curl -sS -H "x-orch-ask: $ORCH_ASK_TOKEN" "{{ASK_BASE}}/$ORCH_SESSION_ID/ask/$ASK/wait")
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
- **agree** — a 👍 on the opening comment, no code, no reply.
- **reply / their own note** — if the solution needs a code change, make it: edit the
  worktree, **amend into the commit that owns each line** (`git log -S`/blame the line to
  find it), run the repo's checks (`mise run pre-commit:run` where it exists), then push
  `--force-with-lease`. The push guard denies plain `--force`, `-u` and protected refs —
  those denials are correct. A `note` is the human's own instruction; follow it. Some
  reply solutions change no code (a pushback, an explanation) — then there is nothing to
  build, only the reply to post in phase 3.
- **story** — file it now (below), so the reply can carry the id.

Prove each change landed by the reviewer's own claim, not by your edit succeeding: "called
twice" → grep it, show it is called once; "these tests miss X" → run the test, show it
fails without the fix. A claim you cannot re-prove is one to report and hold, not to push.

Keep each change to the thread it answers. Do not touch a thread the human skipped.

## Out of scope: file the story, don't promise it

A `story` decision gets a story **now**, before the reply — the reply carries the id.
Search first so a retry cannot file a second one:

```
mcp__shortcut__stories-search   query: the thread's own URL
mcp__shortcut__stories-create   name + description, Backlog
```

The description ends exactly with `Source: review of #{{PR}} — <thread url>` (the dedup
key). Follow the repo's tracker skill for the team, story type, state and epic. One story
per thread; a refused create is retried as the *same* create, never a second. Then reply
with the id (`sc-12345`). If it fails twice, say so on the thread and leave it open.

# Phase 3 — Post (only on their go)

The code is pushed; nothing has been said to a reviewer yet. Ask for the go — the human
reads the real diff beside each reply in the overlay first:

```bash
ASK=$(curl -sS -X POST -H 'content-type: application/json' -H "x-orch-ask: $ORCH_ASK_TOKEN" \
  -d '{"question":"Ready to post. Waiting for your go in the review overlay.",
       "options":[{"value":"post","label":"Post","free":true},
                  {"value":"hold","label":"Hold"}]}' \
  "{{ASK_BASE}}/$ORCH_SESSION_ID/ask" | jq -r .ask)

while :; do
  R=$(curl -sS -H "x-orch-ask: $ORCH_ASK_TOKEN" "{{ASK_BASE}}/$ORCH_SESSION_ID/ask/$ASK/wait")
  [ "$(jq -r .answered <<<"$R")" = true ] && break
done
[ "$(jq -r .answer <<<"$R")" = hold ] && exit 0   # they held; write nothing
REPLIES=$(jq -r .text <<<"$R")   # the final replies, as edited in the overlay
```

`hold` means post nothing and stop. On `post`, use the replies exactly as they came back
— the human's edits win over your drafts.

- **Reactions**: a thread resolved by agreeing, with nothing to add, gets a 👍 and no
  reply. `gh api -X POST repos/{{OWNER}}/{{REPO}}/pulls/comments/<id>/reactions -f content=+1`
- **Replies**: last line of every posted comment is `(via orchestrator)` — that exact
  string is how the daemon knows its own replies (`post::mine_by_footer`), so a thread
  answered here is not answered again by a run. Post threaded, with the comment id from the
  thread URL's `#discussion_r<id>`:
  `gh api repos/{{OWNER}}/{{REPO}}/pulls/{{PR}}/comments/<id>/replies -f body="$reply"`
- **Re-request** each reviewer whose every thread is now addressed, per reviewer not per
  PR: `gh pr edit {{PR}} --add-reviewer <login>`. Addressed means applied or replied to
  with a posted reply. Report who was skipped and which thread holds each one back.

Resolving the threads stays the reviewer's button — never resolve one yourself.

## Not your job, and when to stop

- **Resolving threads.** Closing a conversation is the comment author's button.
- CI still red or the branch behind `{{UPSTREAM}}` → say so and stop. That is `fix-pr`'s
  job, which the rail triggers; do not rebase for it unless the human asks.

When you are done, a short report in the pane: what you changed, what you posted, and any
thread you held and why.
