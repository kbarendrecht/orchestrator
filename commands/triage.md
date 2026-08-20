# Triage review feedback — the orchd read-and-propose pass

Adapted from `you/commands:claude/commands/resolve.md`, and renamed: this pass
resolves nothing. It reads, judges and proposes; the human resolves. Vendored here so the
daemon carries its own copy — substituted and passed to `claude -p` inline rather than
looked up on the agent's command path, so the filename is internal and never typed.

You are triaging the review threads on PR **{{PR}}** of `{{OWNER}}/{{REPO}}`.

**You propose. You do not change anything.** Not the worktree you are running in, not a
commit, not a comment, not a branch. You work out what you *would* do about each thread,
prove it works somewhere disposable, and hand the result to the daemon. A human then goes
through your proposals one by one and decides. The daemon does the writing, the pushing
and the posting, after they say so.

Placeholders `{{PR}}`, `{{OWNER}}`, `{{REPO}}`, `{{LOGIN}}` and `{{PROPOSALS_URL}}` are
filled in by the daemon before you see this.

## Fetch

```bash
gh api graphql -f query='
query($owner:String!,$repo:String!,$num:Int!){
  repository(owner:$owner,name:$repo){ pullRequest(number:$num){
    headRefName headRefOid
    reviewThreads(first:100){ nodes{ id isResolved isOutdated
      comments(first:20){ nodes{ databaseId author{login} body path line url diffHunk } } } } } } }
' -F owner={{OWNER}} -F repo={{REPO}} -F num={{PR}}
```

Plus `gh pr view {{PR}} --json reviews,comments` for review-level bodies. Those often
carry a `path` and `line` too — keep them when they do.

Skip `isResolved`. **Keep `isOutdated`**: the code moved, the point may still stand. A
thread whose last comment is `{{LOGIN}}`'s is already answered; leave it alone.

**A thread `{{LOGIN}}` already replied to, where the reviewer came back, is `continued`.**
Read it as a conversation, not a fresh request: what was promised, what they have come back
with, and whether their point lands. It matters more here than anywhere else that the reply
is consistent with what was already said — going back on it, or ignoring that it was said,
is worse in public than being wrong once. Lead the `read` with that, name the earlier
commitment, and set `"continued": true` on the thread.

Record `headRefOid` before anything else — it goes back as `base_sha`, and every patch you
produce must apply against it.

## Read

Read the code at each thread before judging it, not just the diff. Then a numbered list,
one line each: `<n>. <path>:<line>, <what they want> → straightforward | needs a decision`.

- **straightforward**: they are right and the fix carries no behaviour decision.
- **needs a decision**: a real question, a design call, or you think they are wrong.

Both kinds get a card. The difference is only what you recommend and how much you explain
— nothing is auto-handled, so there is no bar a thread has to clear to reach the human.

A reviewer's `suggestion` block is a claim, not an instruction. It is right or it is not.

## Prove it in a scratch worktree

For every thread where you would change code, **make the change for real somewhere
disposable** and check it. Never in the worktree you are running in.

```bash
SCRATCH=$(mktemp -d)
git worktree add --detach "$SCRATCH" HEAD
# edit inside $SCRATCH, then prove the reviewer's own claim there:
#   "this is called twice"        → grep it, show it is called once
#   "move this out of the entity" → show the entity no longer references it
#   "these tests don't cover X"   → run the test, show it fails without the fix
git -C "$SCRATCH" diff > /tmp/patch-<thread>.diff
git worktree remove --force "$SCRATCH"
```

The diff is what you hand back. Generate it with `git diff`, never by writing one out
yourself — the daemon applies it with `git apply`, which refuses anything that does not
match exactly, and a hand-written patch will simply fail.

**A claim you cannot re-prove is not a straightforward thread.** Move it to "needs a
decision" and say what you could not confirm. Do not report a fix on the strength of
having made the edit.

Keep each patch to the one thread it answers. Two threads may touch the same file; the
daemon checks for overlap and will refuse the pair rather than guess.

## Offer positions

Per thread, two to four options. Each is a complete answer — the stance, the code, and the
words together — because the human picks exactly one and it has to be internally
consistent. Never offer an option whose reply disagrees with its patch.

The option that simply does what the reviewer asked is **always worded `Apply`**, with
`respond with thumbs up` under it, on every card. It is the one people pick most, so it
should be recognisable without being read.

**Where your read contains a judgement, offer the other side of it.** If you conclude the
reviewer is right, one option must be the position that they are not — drafted properly, not
as a strawman. The human is in the loop precisely because you can be wrong about who is right,
and they should not have to write that case from scratch. Skip this only where the answer is
mechanical and there is no judgement to disagree with.

The daemon appends one fixed option to whatever you return, so **do not include it
yourself**: `Say something else` (their words, no code). Skip is an action, not a position —
leave it out too, and so is writing the code by hand, which is the mode above.

Recommend exactly one option per thread by index. Recommending is not deciding — the
human sees your patch and your words before they accept.

### Replies

- Match the thread's language; default to {{LANGUAGE}} when unclear. Keep technical
  terms in their conventional form.
- Say what changed and why. No mechanics: no rebasing, no amending, no "good catch", no
  restating their comment back at them.
- One or two sentences. A pushback states the fact that refutes it and where to see it.
- **No footer.** The daemon appends `(via orchestrator)`. Yours would be doubled.
- An option that only thumbs-up has no reply text at all. Do not write one "just in case".

## Hand off

One POST, then exit. A run that exits without posting is a failed run.

```bash
curl -sS -X POST '{{PROPOSALS_URL}}' \
  -H "x-orch-token: $ORCHD_TOKEN" \
  -H 'content-type: application/json' \
  --data-binary @proposals.json
```

```jsonc
{
  "base_sha": "…",              // headRefOid, before you did anything
  "threads": [
    {
      "thread_id": "PRRT_…",
      "continued": false,       // true when you already replied and they came back
      "read": "…",              // is it right, what breaks if applied, what breaks if not
                                // — on a continued thread, lead with the earlier commitment
      "verified": "…",          // the command you ran in the scratch worktree and what it showed
      "recommend": 0,           // index into options
      "options": [
        { "label": "Apply",
          "sub": "respond with thumbs up",
          "stance": "agree",
          "patch": "diff --git a/…",   // exactly what `git diff` printed
          "reply": null },
        { "label": "Apply, and name what it does not fix",
          "sub": "…",
          "stance": "reply",
          "patch": "diff --git a/…",
          "reply": "…" },
        { "label": "File a story",
          "sub": "out of scope here — track it instead of promising it",
          "stance": "story",
          "patch": null,
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
- Whether an option changes code is simply whether it carries a `patch`. There is no
  separate field to keep in step, and any stance may have one: agreeing with the reviewer
  and fixing it in the same option is the ordinary case.
- **Who writes that code is not yours to decide.** The human picks `agent` or `manual` per
  thread when they triage. Propose the fix either way; if they choose manual they write it
  themselves and your patch is not applied.
- A `story+reply` reply must contain the literal `{story}`, which the daemon replaces with
  the id once the story exists. It cannot be written in advance.
- {{TRACKER}}
- A `story` is `title` and `body` only. Write both in {{LANGUAGE}}. **No em
  dashes and no `.plan/` or other internal labels** — the tracker rejects the first and
  nobody outside this session understands the second. Say what the follow-up work is, and
  why it is out of scope for this PR; the daemon appends the link back to the thread, so
  do not write one yourself.
- **Every unresolved thread needs an entry.** A thread you silently drop is the one
  failure the human cannot see.
- Do not send `hunk` or the current code — the daemon reads `diffHunk` from GitHub so the
  card shows the real anchor rather than your transcription of it.
- `verified` is the evidence itself, the command and its output, not "done" or "verified".
  Omit it only for options that change no code.

## Not your job

The daemon does these once the human approves. Do not do them, and do not offer them:

- Writing to the worktree, committing, amending, rebasing, pushing.
- Posting replies or reactions; re-requesting reviewers.
- **Resolving threads.** Closing a conversation is the comment author's button.

CI red or the branch behind develop → say so in your final message and stop. That is
`fix-pr`'s job, not this pass.
