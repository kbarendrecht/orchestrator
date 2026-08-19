# Resolve review feedback — orchd run

Adapted from `you/commands:claude/commands/resolve.md`. Vendored here so the
daemon carries its own copy: it is substituted and written to a file the daemon owns,
and the session is told to read that file. Nothing is resolved from the agent's
command path, so nothing has to be installed for a run to work.

You are answering the review threads on PR **{{PR}}** of `{{OWNER}}/{{REPO}}`. The
daemon fills the PR, repo, login and upstream in before you read this, so take the
values here as given rather than looking them up.

Nothing is posted without a separate go. Pushing code is fine (own branch,
`--force-with-lease`); posting a comment, resolving a thread and re-requesting a
review are not.

## Where you are

The daemon has already created a worktree pinned to this PR's head branch and
started you inside it. Do not switch branches and do not create one — the run stays
on the PR's head ref, and `--worktree` would cut a fresh branch off `{{UPSTREAM}}`
instead.

Confirm rather than assume: `git rev-parse --abbrev-ref HEAD` against `headRefName`
from the fetch below. A mismatch is a stop, not something to correct by switching.

This is an interactive session in a pane the user is watching, so ask them rather
than guessing, and expect to be taken over mid-flight.

## Fetch

```bash
gh api graphql -f query='
query($owner:String!,$repo:String!,$num:Int!){
  repository(owner:$owner,name:$repo){ pullRequest(number:$num){
    headRefName headRepositoryOwner{login}
    reviewThreads(first:100){ nodes{ isResolved isOutdated
      comments(first:20){ nodes{ author{login} body path line url } } } } } } }
' -F owner={{OWNER}} -F repo={{REPO}} -F num={{PR}}
```

Plus `gh pr view {{PR}} --json reviews,comments` for review-level bodies that aren't
anchored to a line.

Head owner is not `{{LOGIN}}` → stop, it's someone else's branch to force-push.

Skip `isResolved`. Keep `isOutdated`: the code moved, the point may still stand. A
thread whose last comment is `{{LOGIN}}`'s is already answered; don't re-answer it.

## Sort

Read the code at each thread before judging it, not just the diff. Then a numbered
list, one line each: `<n>. <path>:<line>, <what they want> → apply | discuss |
reject`. Numbering is how the user steers ("fix 1, respond to 2"), so keep it stable
for the rest of the turn.

- **apply**: concrete and correct, no behaviour decision in it.
- **discuss**: a real question, a design call, or you think they're wrong.
- **reject**: factually wrong about the code, and you can prove it with the code.

A reviewer's `suggestion` block is still a claim, not an instruction. It's `apply`
only when it's right.

## Apply

Apply the `apply` set, amend into the commit that owns each change, run the repo's
pre-commit (`mise run pre-commit:run` where it exists), push `--force-with-lease`.
The daemon's push guard denies plain `--force`, `-u`/`--set-upstream`, and any push
to a protected ref — those denials are correct, do not work around them.

Then verify each one landed, by the reviewer's own claim, not by your edit
succeeding:

- "this is called twice" → grep the call, show it's called once.
- "move this out of the entity" → show the entity no longer references it.
- "these tests don't cover X" → run the test, show it fails without the fix.

A claim you can't re-prove moves to `discuss`. Do not report an item applied on the
strength of having made the edit.

## Ask about the rest

One `AskUserQuestion` per remaining finding, batched four at a time. Each question
carries the full context so the user never has to open the PR:

- The reviewer's comment verbatim, in their language.
- The code as it stands, with the path and line.
- Your read: is it right, what breaks if applied, what breaks if not.
- Options as real positions ("apply as suggested", "counter with X", "reject, the
  type already guarantees it"), not "yes / no".

## Replies

Draft, show, stop. Post only on an explicit go.

**A thread you applied as asked, with nothing to add, gets a 👍 and no reply.** Reply
only where the reviewer learns something: you deviated, you pushed back, you applied
it somewhere they didn't name, or you're asking them something.

```bash
gh api -X POST repos/{{OWNER}}/{{REPO}}/pulls/comments/<id>/reactions -f content=+1
```

- Reactions take no `(Written by Claude, Acknowledged by Kars)` line, and wait for
  the same go as a reply.
- List them separately from the written replies in the draft.

Written replies:

- Match the thread's language. This team reviews in Dutch, so replies are Dutch by
  default, English tech terms intact.
- Say what changed and why. Nothing about mechanics: no rebasing, no amending, no
  "good catch", no restating their comment back at them.
- One or two sentences. A reject states the fact that refutes it and where to see it.
- Last line of every posted comment: `(Written by Claude, Acknowledged by Kars)`.

Post a threaded reply with the comment id from the thread URL's `#discussion_r<id>`:

```bash
gh api repos/{{OWNER}}/{{REPO}}/pulls/{{PR}}/comments/<id>/replies -f body="$reply"
```

## After posting

Re-request each reviewer whose every thread is addressed, without asking:
`gh pr edit {{PR}} --add-reviewer <login>`.

- Per reviewer, not per PR: Alice's five handled while Bob's two are open
  re-requests Alice alone.
- Addressed: applied, or replied to with a posted reply. An unposted draft is not
  addressed.
- Report who was skipped and which thread holds each one back.

Resolving the threads stays an offer, it's the reviewer's button.

CI still red or the branch behind `{{UPSTREAM}}` → hand off to `fix-pr`, which the
rail triggers. Do not rebase for it yourself unless the user asks.
