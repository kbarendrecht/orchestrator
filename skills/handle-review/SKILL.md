---
name: handle-review
description: Work through a pull request's review threads in a pane — judge each one, apply what is right, ask about the rest, draft replies and post only on an explicit go. Use when somebody asks you to resolve, handle or answer the review feedback on a PR.
---

# Handle review feedback

`/orchd:handle-review <pr>`. Working through the feedback, not GitHub's
resolve-conversation button — marking threads resolved stays the reviewer's
([After posting](#after-posting)).

**Nothing is posted without a separate go.** Pushing code is fine (your own branch,
`--force-with-lease`); posting a comment, filing a story, resolving a thread and
re-requesting a review are not.

You are in a pane somebody is watching, so ask them things directly with
`AskUserQuestion`. `$ORCH_PR` is the PR and `$ORCH_LANGUAGE` is the language to
write replies in when a thread does not settle it. `gh` runs inside the PR's own
worktree, so it infers the repo; `gh api user --jq .login` is you.

## Fetch

```bash
gh api graphql -f query='
query($owner:String!,$repo:String!,$num:Int!){
  repository(owner:$owner,name:$repo){ pullRequest(number:$num){
    headRefName headRepositoryOwner{login}
    reviewThreads(first:100){ nodes{ isResolved isOutdated
      comments(first:20){ nodes{ databaseId author{login} body path line url } } } } } } }
' -F owner=<owner> -F repo=<repo> -F num=$ORCH_PR
```

`gh repo view --json owner,name` gives the two you need. Plus `gh pr view $ORCH_PR
--json reviews,comments` for review-level bodies that are not anchored to a line.

Skip `isResolved`. **Keep `isOutdated`**: the code moved, the point may still stand.
A thread whose last comment is your own is already answered; do not re-answer it.

## Sort

Read the code at each thread before judging it, not just the diff. Then a numbered
list, one line each: `<n>. <path>:<line>, <what they want> → apply | discuss |
reject | story`. The numbering is how the user steers ("fix 1, respond to 2"), so
keep it stable for the rest of the turn.

- **apply**: concrete and correct, no behaviour decision in it.
- **discuss**: a real question, a design call, or you think they are wrong.
- **reject**: factually wrong about the code, and you can prove it with the code.
- **story**: fair, and belongs in other work — filed now, not promised (below).

A reviewer's `suggestion` block is still a claim, not an instruction. It is `apply`
only when it is right.

## Apply

Apply the `apply` set, amend into the commit that owns each change, run the repo's
pre-commit (`mise run pre-commit:run` where it exists), push `--force-with-lease`.
The daemon's push guard denies plain `--force` and any push to the base branch;
those denials are correct.

Then verify each one landed **by the reviewer's own claim, not by your edit**:

- "this is called twice" → grep the call, show it is called once.
- "move this out of the entity" → show the entity no longer references it.
- "these tests do not cover X" → run the test, show it fails without the fix.

A claim you cannot re-prove moves to `discuss`. Do not report an item applied on
the strength of having made the edit.

## Ask about the rest

One `AskUserQuestion` per remaining finding, batched four at a time. Each question
carries the full context so the PR never has to be opened:

- The reviewer's comment verbatim, in their language.
- The code as it stands, with the path and line.
- Your read: is it right, what breaks if applied, what breaks if not.
- Options as real positions ("apply as suggested", "counter with X", "reject, the
  type already guarantees it"), not "yes / no".

## Replies

Draft, show, stop. Post only on an explicit go.

**A thread you applied as asked, with nothing to add, gets a 👍 and no reply.**
Reply only where the reviewer learns something: you deviated, you pushed back, you
applied it somewhere they did not name, or you are asking them something.

```bash
gh api -X POST repos/<owner>/<repo>/pulls/comments/<id>/reactions -f content=+1
```

- Reactions take no footer line, and wait for the same go as a reply.
- List them separately from the written replies in the draft.

Written replies:

- Match the thread's language; `$ORCH_LANGUAGE` where it does not settle it.
- Say what changed and why. Nothing about mechanics: no rebasing, no amending, no
  "good catch", no restating their comment back at them.
- One or two sentences. A reject states the fact that refutes it and where to see
  it.

Post a threaded reply with the comment id from the thread URL's
`#discussion_r<id>`:

```bash
gh api repos/<owner>/<repo>/pulls/$ORCH_PR/comments/<id>/replies -f body="$reply"
```

## Out of scope: file the story, don't promise it

A thread that is fair but belongs in other work gets a story **now**, before the
reply is drafted, so the reply carries the id. "We'll pull this into a story" is a
promise nobody is holding, and the story is the only part the reviewer cannot check
for themselves.

Search first, with the thread's own URL as the query, so a retry cannot file a
second one; then create with a name, a description and whatever this repo calls its
backlog. Which tools those are depends on the tracker and this file does not name
them: the repo's own tracker skill does, and it holds the team, the story type, the
state and the epic too. No tracker configured means there is nowhere to file — say
so plainly in a reply rather than promising a story.

- The description ends with the dedup key, exactly: `Source: review of #$ORCH_PR —
  <thread url>`. That URL is what a later search finds, here and in the daemon's own
  story pass.
- Title and body in the thread's language, and about the work itself: nobody outside
  this session knows which thread this was.
- One story per thread. A refused create is retried as the *same* create once what
  it named is fixed, never worked around with a second story.

Then reply with the id, not with a plan to get one — and as a markdown link rather
than a bare id, since a naked id is a dead end to anyone outside the tracker. If the
create fails twice, say so on the thread and leave it open: an unfiled story with a
reply promising one is the state this exists to prevent.

## After posting

Re-request each reviewer whose every thread is addressed, without asking:
`gh pr edit $ORCH_PR --add-reviewer <login>`.

- Per reviewer, not per PR: one reviewer's five handled while another's two are open
  re-requests the first alone.
- Addressed means applied, or replied to with a *posted* reply. An unposted draft is
  not addressed.
- Report who was skipped and which thread holds each one back.

Resolving the threads stays an offer; it is the reviewer's button.

CI still red, or the branch behind its base → that is `/orchd:green` in this pane,
or `/orchd:fix-pr` as a run the daemon drives. Not this pass.
