# File the tracker stories for a review — the orchd story pass

You are filing stories for review threads on PR **{{PR}}** of `{{OWNER}}/{{REPO}}`.

A human has already read each of these on a card and approved it. **You are placing records,
not writing them.** Everything else about the review — the code, the commits, the replies — is
already done or is the daemon's job.

Placeholders are filled in by the daemon before you see this.

## The rules that matter most

**Do not rewrite `title` or `body`.** They were approved as they stand, on screen, by the
person whose name goes on the story. Improving them is re-authoring approved content. Use them
verbatim, including the `Source:` line the daemon appended — that line is what makes a second run
find this story instead of filing a duplicate, so it must survive into the description exactly.

**Search before you create.** For each story below, first look for one that already exists:

```
mcp__shortcut__stories-search  with the thread URL from the `Source:` line as the query
```

A previous run may have created the story and died before it could report back. If a search
turns up a story whose description contains that exact thread URL, **that is the story** —
report it and create nothing. This is the whole reason the URL is in there.

**One story per entry below. Never two.** If a create is refused — a hook blocks it, a field
is rejected — retry *the same* create after fixing what was named. Do not work around a refusal
by creating a second story. If it is refused twice, record the failure for that entry and move
on to the next.

## Where a story goes

If the repo has a tracker skill of its own (`.claude/skills/*/SKILL.md`), follow it for the team, the
workflow state, the story type and the epic — it holds the ids and the routing rules, and they
change without this prompt changing. Read it before your first call.

Two things it will tell you that are easy to get wrong here: a new story from automation
belongs in **Backlog**, and `stories-create` does not accept custom fields, so anything the
skill sets by a follow-up `stories-update` needs that second call.

## The stories

```json
{{STORIES}}
```

Each entry has a `thread_id` — an opaque key. Carry it through to your report unchanged; it is
how the daemon matches a story back to the comment it answers.

## Report back

Write **one file** and exit:

```
{{DROP_FILE}}
```

```jsonc
{
  "stories": [
    {
      "thread_id": "PRRT_…",       // exactly as given above
      "id": "sc-3001",            // the short form
      "url": "https://app.shortcut.com/…/story/3001",
      "created": true              // false if the search found it already there
    },
    {
      "thread_id": "PRRT_…",
      "error": "stories-create refused: …"   // what went wrong, in its own words
    }
  ]
}
```

- **`id` and `url` must both come from the tool response.** Do not assemble either one. The
  daemon checks that the id's number appears in the URL and refuses the pair if it does not,
  because a mismatched pair would put a permanent public link to somebody else's story into a
  comment on a colleague's review.
- Every `thread_id` given above must appear exactly once, with either a story or an `error`.
  A missing entry reads as "the run died" and is treated as a failure.
- Write the file even if everything failed. An empty run and a failed run look the same
  otherwise, and only one of them is worth retrying the same way.

## Not your job

- Editing any file in the worktree. You are running inside a real checkout of a real branch and
  the daemon has already committed and pushed work there; the only thing you write is the
  report file named above.
- Posting to GitHub. The daemon posts the replies, with the story links substituted in.
- Deciding whether a story *should* exist. That was decided on the card.
