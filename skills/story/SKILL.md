---
name: story
description: File tracker stories for review threads a human already approved — search for an existing one first, create at most one per entry, and report each id and URL back. Places records only: never rewrites the approved text, never edits the worktree, never posts. Use when the orchestrator starts a story pass.
---

# File the approved stories

`/orchd:story <pr>`. A human has already read each of these on a card and approved
it. **You are placing records, not writing them.** Everything else about the
review — the code, the commits, the replies — is already done or is the daemon's.

Three paths are in your environment, and the run cannot work without them:

| variable | what it is |
| --- | --- |
| `$ORCH_STORIES` | the entries to file, as JSON — read it first |
| `$ORCH_DROP` | the one file you write, and the only file you write |
| `$ORCH_TRACKER_HOST` | the host the daemon will accept a story URL on |

## The rules that matter most

**Do not rewrite `title` or `body`.** They were approved as they stand, on screen,
by the person whose name goes on the story. Improving them is re-authoring approved
content. Use them verbatim, including the `Source:` line the daemon appended — that
line is what makes a second run find this story instead of filing a duplicate, so
it must survive into the description exactly.

**Search before you create.** For each entry, first look for a story that already
exists, using your tracker's own search with **the thread URL from the `Source:`
line as the query**. A previous run may have created the story and died before it
could report back. If a search turns up a story whose description contains that
exact thread URL, **that is the story** — report it and create nothing. This is the
whole reason the URL is in there.

Which tool that is depends on the tracker, and this file deliberately does not
name one: the repo's own tracker skill does, and tool names differ per tracker and
change without either of us noticing. If your tracker offers no text search, say so
in the entry's `error` rather than creating a story you could not check for.

**One story per entry. Never two.** If a create is refused — a hook blocks it, a
field is rejected — retry *the same* create after fixing what was named. Do not
work around a refusal by creating a second story. If it is refused twice, record the
failure for that entry and move on.

## Where a story goes

If the repo has a tracker skill of its own (`.claude/skills/*/SKILL.md`), follow it
for the team, the workflow state, the story type and the epic — it holds the ids and
the routing rules, and they change without this file changing. Read it before your
first call.

Two things that are easy to get wrong wherever you are filing: a new story from
automation belongs in whatever the repo calls its **backlog** rather than in
progress, and a create call often will not accept custom fields, so anything the
repo's skill sets that way needs a second update call.

## Report back

Write **one file** and exit, at `$ORCH_DROP`:

```jsonc
{
  "stories": [
    {
      "thread_id": "PRRT_…",       // exactly as given in $ORCH_STORIES
      "id": "ENG-123",             // the short form, as the tool returned it
      "url": "https://…/issue/ENG-123",
      "created": true              // false if the search found it already there
    },
    {
      "thread_id": "PRRT_…",
      "error": "create refused: …"  // what went wrong, in its own words
    }
  ]
}
```

- **`id` and `url` must both come from the tool response.** Do not assemble either
  one. The daemon checks that the URL is on `$ORCH_TRACKER_HOST` and that a path
  segment in it is either the id's number or the whole id — because a mismatched
  pair would put a permanent public link to somebody else's story into a comment on
  a colleague's review.
- Every `thread_id` given must appear exactly once, with either a story or an
  `error`. A missing entry reads as "the run died" and is treated as a failure.
- Write the file even if everything failed. An empty run and a failed run look the
  same otherwise, and only one of them is worth retrying the same way.

## Not your job

- Editing any file in the worktree. You are running inside a real checkout of a real
  branch and the daemon has already committed and pushed work there; the only thing
  you write is the report file named above.
- Posting to the forge. The daemon posts the replies, with the story links
  substituted in.
- Deciding whether a story *should* exist. That was decided on the card.
