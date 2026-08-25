# Resolve flow — the decisions behind it

The two-phase review flow — **human triage → one agent session that builds** — is
built and has answered real reviewers. What is worth keeping from the design pass
is not the plan but the decisions, because the shape of the code only makes sense
against them, and three of them landed differently once driven.

The phased build-out, the spike plan for the interaction channel, and the
verification checklist are gone: all three are finished, and `TODO.md` carries what
is still open.

## The one boundary everything rests on

- **The agent session owns code.** It applies *and adapts* each fix and commits.
  Not a daemon `git apply` — that is what "full agent session" meant, and it is why
  `patch.rs`'s apply ladder is unused by this path.
- **The daemon owns outward writes and bookkeeping.** Replies, reactions, stories
  and re-requests stay in `forge/github_write.rs` / `story.rs`, so token handling,
  commit-before-reply ordering and story idempotency live in one place.
- **They coordinate per thread**: the agent commits, then the daemon shows the real
  diff and posts the reply. The daemon's read token is never used to write, and the
  session is never given the app token — only an `ask_token` that opens asking and
  nothing else.

## The nine decisions, and what they became

| # | Decision | Choice | How it landed |
|---|----------|--------|---------------|
| 1 | Phase-2 execution | Full agent session (agent applies + adapts fixes) | as decided |
| 2 | Branch drift | Re-validate + auto-rebuild; abort a thread only if unappliable | **differently** — see below |
| 3 | Partial failure | Resumable per-thread units + honest overview | **partly** — durable, not resumable |
| 4 | Manual-question timing | Always in-context (Phase 2) | as decided |
| 5 | "Manual" naming | Keep "manual" | as decided |
| 6 | Reply vs real diff | Show committed diff; auto-post unless you object | **differently** — an explicit choice |
| 7 | Primary button | Per-thread by real effect | as decided |
| 8 | Triage run (Phase 0) | Explicit run screen | as decided |
| 9 | Session scope | Always run one session, even words-only | as decided |

### 2 — drift is checked as ancestry, not equality

Re-validating `base_sha == HEAD` per thread would be a bug: the agent commits once
per thread, so from the second thread on the head has moved *by design*. The prompt
checks equality once, at the start, and rebuilds a moved patch by hand — which is
the "auto-rebuild" half, done by the agent rather than by the daemon.

What the daemon checks per thread is **ancestry**: is the tree this run was triaged
against still in the branch's history? It stops being true when the branch is
rewritten underneath the run, and then a reply would describe a fix that cannot
land, so the reply is held.

### 3 — durable, and deliberately not resumable

The run record persists (`resolve-runs.json`) and a restart recovers the account of
it, marked ended. Nothing respawns a session to finish the remaining threads: the
commits survive in git either way, and a second way for a run to start was more
machinery than anyone had asked for. `TODO.md` has the note.

### 6 — you choose, nothing expires

"Auto-post unless you object within a short window" became an explicit card with
two buttons: post it, or hold it back. A timeout that posts on your behalf is the
one thing this flow exists to avoid, and a card that expires into a public comment
would be exactly that.

## What the design did not anticipate

- **`Skip` is not a stance.** Stance is `agree | reply | story`; a thread you want
  to leave alone is one you do not decide on, which needs no spelling.
- **A run cannot start unattended.** Its first act is reading `plan.json` in
  `config_dir`, outside the worktree, so Claude Code asks permission before the
  agent has read a word of the plan — then again per commit, and for the `committed`
  call. "The session drives, you approve each step" is truer than intended.
