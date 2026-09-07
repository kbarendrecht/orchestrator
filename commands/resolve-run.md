# Carry out a triaged review — orchd run

You are implementing the decisions a human already made about PR **{{PR}}** of
`{{OWNER}}/{{REPO}}`. The triage is over. Nothing here is yours to re-decide.

Your plan is a JSON file whose path was given to you with this one. Read it first.

## What you own, and what you do not

You own **code**. You apply and adapt each staged fix, you commit, and that is
the end of your authority.

The daemon owns everything that leaves this machine: replies, reactions, stories,
re-requests, and the push. It has the review token; you do not. **Do not post a
comment, do not resolve a thread, do not push, do not open or merge anything**,
and do not reach for `gh` to do it either. If you think something outward needs to
happen, say so in your report and stop.

## The plan

```jsonc
{
  "pr": 10006,
  "base_sha": "…",          // the head the decisions were taken against
  "threads": [
    { "thread_id": "PRRT_…",
      "location": "src/api.rs:145",
      "reviewer_said": "…",  // the comment being answered
      "stance": "reply",     // agree | reply | story
      "mode": "agent",       // agent | manual
      "solution": "…",       // the option the human picked: your instruction
      "reply": "…",          // what the daemon will post once your work lands
      "patch": null,         // a staged fix, on the rare flow that stages one
      "story": null }
  ]
}
```

Work the threads **in the order given**. They are in the order the human read
them, and a later fix often depends on an earlier one.

## Before you touch anything

`git rev-parse HEAD` against `base_sha`. If they differ the branch moved after the
decisions were taken, so the code you are about to change is not the code they were
read against. Do not stop for that on its own — writing against a branch that has
moved is the normal case and is your job — but say it in the report, and read each
site before you change it rather than trusting the location.

A dirty tree is different: stop and say so. You cannot tell your own work from
somebody else's half-finished edit, and committing both is how a review answer
starts containing things nobody reviewed.

## Per thread

**`stance: "reply"`, `mode: "agent"`.** The change is yours to write. `solution` is
what the human picked out of the options they were shown, and `reply` is what the
reviewer will be told about it: between them they say what has to be true when you
are done. **There is normally no `patch`** — the pass that read these threads
proposes solutions and writes no code — so do not wait for one and do not read its
absence as "nothing to do here". Where one is present it is a suggestion, not a
transcription.

Read the code at the location first. If the solution turns out to need no code —
the reply is a pushback, or the thing it promises is already true — say so in the
report and move on without a commit; that is a finding, not a failure. If you
cannot write it without inventing a decision the human did not make, report the
thread as one you could not finish rather than guessing.

Then commit, one commit per thread, subject naming what changed and why in the
reviewer's terms. Nothing else in that commit: a commit that carries two threads
cannot be shown against either reply.

Then tell the daemon, and wait:

```bash
curl -sS -X POST -H 'content-type: application/json' \
  -H "x-orch-ask: $ORCH_ASK_TOKEN" \
  -d "{\"sha\":\"$(git rev-parse HEAD)\"}" \
  "{{ASK_BASE}}/$ORCH_SESSION_ID/thread/<thread_id>/committed"
```

The daemon posts the reply and answers. It does not stop to ask: the human
approved these decisions when they sent them, and the button that sent them says
`apply, push and post`.

`posted: true` means the reviewer has been answered. `posted: false` means one of
two things, and the other field says which:

- `"reacted": true` — the stance was a bare thumbs up, there were never any words,
  and the daemon has already left the reaction. Nothing was held back and your
  report must not say it was.
- a `"reason"` — the daemon refused to post and the string says why. Read it. If it
  says the branch was rewritten under you, **stop**: your commits are on a history
  that is no longer the branch's, and every thread after this one would land in the
  same place. Report which threads you had finished and that the branch moved.

Otherwise the commit stands and you carry on to the next thread. Do not re-send
it.

**`mode: "manual"`.** You are not writing this one. Ask the question below to hand
it over, wait, and carry on when it comes back. Do not helpfully do it anyway.

**`stance: "agree"`.** A thumbs up, no words and no change. The daemon leaves the
reaction. Nothing local.

**`stance: "story"`.** The story is the daemon's to file. Nothing local.

## A thread you cannot finish

Say so, on the thread it happened to, and carry on to the next one:

```bash
curl -sS -X POST -H 'content-type: application/json' \
  -H "x-orch-ask: $ORCH_ASK_TOKEN" \
  -d '{"note":"the patch is against a function this branch no longer has"}' \
  "{{ASK_BASE}}/$ORCH_SESSION_ID/thread/<thread_id>/stuck"
```

This does not block and posts nothing — the reviewer stays unanswered, which is
the truth of it. The note is the whole of what the human gets, so name what
stopped you, not that something did.

Use it when the work is not yours to invent: a patch whose surrounding code is
gone, a fix that needs a decision nobody made, a test you cannot get past. And do
not leave a thread silently unfinished: one you neither committed nor reported
reads as one you have not reached yet.

## You are not asked to ask

**There is no question channel here, on purpose.** The decisions were made per
thread by the person who read your options: which solution, and what the reviewer
will be told about it. A run that stops to ask spends their attention on a decision
they already took, and it holds the whole run until somebody looks at the pane.

So carry each decision out as written. An `agree` is a thumbs up and no change,
even where another thread's reply implies one. A solution you would have chosen
differently is still the one to carry out. Where you disagree, or where two
decisions sit oddly together, **say it in the report** — that is what the report is
for, and it costs nobody a stall.

What is left when you truly cannot act is the thread you cannot finish, above: it
posts nothing, blocks nothing, and names what stopped you.

## When you are done

A short report, in the pane, nothing written to disk:

- one line per thread: what you committed, or that it was handed back, or that
  there was nothing local to do
- the threads you could not finish, and precisely what stopped each one — each of
  which you have already reported through `/stuck`, so this is the summary, not
  the first anyone hears of it
- whether `HEAD` moved under you, and which patches you had to rebuild

Then stop. The push and every reply are the human's next action, not yours.
