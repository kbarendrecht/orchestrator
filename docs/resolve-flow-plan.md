# Resolve flow — implementation plan

Reshape the in-UI review resolution (`openReview` overlay + `/resolve`) into a
two-phase flow: **human triage → one agent session that builds**. This plan is
the output of a design pass; the nine decisions it encodes are listed at the end.

## Context

Today the overlay stages decisions locally and the daemon applies everything in
one non-interactive batch: `pr_post` → `crate::post::run` runs `git apply` +
commit for staged patches, then posts replies/reactions/stories through
`forge/github_write.rs`, with a durable half-done model so a crash is resumable
(`post.rs` — "everything before the outward step is local and undoable").

That batch conflates *deciding* with *building* and hides code changes behind a
send button. The new shape splits them:

- **Phase 0 — Triage run** (explicit): the triage agent produces proposals.
- **Phase 1 — Triage** (human): decide per thread — stance, reply, mode — nothing runs.
- **Phase 2 — Session** (agent): one session implements, pausing for you in-context.
- **Phase 3 — Overview**: an honest account of what happened; push/re-request are yours.

Intended outcome: answer a whole PR's review in one sitting, with the agent
doing the mechanical work and stopping to ask — never posting, committing,
pushing, or resolving without you.

## Roles (locks the one ambiguity in "full agent session")

- **The agent session owns code**: it applies *and adapts* each fix, commits,
  and pushes (its own git creds, under `guards/push.py`). This is the "full
  agent session" decision — the agent writes the fix, not a daemon `git apply`.
- **The daemon owns outward writes + bookkeeping**: replies, reactions, stories,
  re-request stay in `forge/github_write.rs` / `story.rs`, keeping token handling,
  ordering (commit-before-reply), and story idempotency in one place — as today.
- They coordinate **per thread**: agent commits → daemon shows the diff and posts
  the reply. The daemon's read-only PAT is never used to write.

## The central new subsystem: session ↔ SPA interaction channel

Phase 2 has two points where the running session must reach the SPA and block:

1. **Open-question** (a `manual` thread): the agent raises a structured
   multiple-choice question; the overlay renders it; the turn blocks until you
   answer; the answer returns to the agent.
2. **Committed-diff confirm** (an agent fix): after the commit, the overlay shows
   the real diff; the reply auto-posts unless you object within a short window.

Nothing today does request/response between an agent turn and the SPA — hooks are
one-way observers, and the only daemon→agent path is the pty write
(`hooks.rs` `pending_prompt`). **This is the riskiest piece; spike it first.**

- **Preferred:** a small blocking tool the skill invokes (a CLI run via Bash, or
  a tiny MCP tool) that POSTs the question/diff to the daemon and long-polls for
  the answer. The daemon holds `pending_interaction` on the session; the SPA
  renders it from the snapshot and POSTs to a new `/api/session/:id/answer`;
  the daemon unblocks the tool call with the answer.
- **Fallback if blocking is unreliable:** the agent emits a sentinel to stdout,
  the daemon parses it, surfaces the card, and injects the answer back through
  the existing pty-write path. No new tool, but brittle.

Spike question to settle: can a skill reliably block a turn on a daemon
round-trip and resume cleanly? Build nothing else in Phase C until this is known.

## Data model

- **Per-thread decision** carries `mode: Agent | Manual` explicitly (today it's
  implied by `does` — `change+reply` vs `manual`). Stance stays `Agree | Reply |
  Story | Skip`; `Story` reuses the existing `story+reply` path, `Skip` leaves
  the thread open and untouched.
- **Resolve run record** — durable, resumable, per PR. Extend `post.rs`'s
  existing durable batch with a per-thread status:
  `pending → fix_applied → awaiting_confirm → awaiting_answer → replied →
  {failed | needs_you}`. On session death, resume from the last completed unit;
  never double-post (reuse the half-done discipline already in `post.rs`).

## Reuse (do not rebuild)

- Triage: `src/triage.rs::spawn`, `src/proposal.rs` (positions, `StoryDraft`,
  `STORY_TOKEN`), `pr_triage` / `pr_proposals` in `src/api.rs`.
- Outward writes + ordering: `src/forge/github_write.rs` (`reply`, `thumbs_up`,
  `rerequest` — **no** `resolveReviewThread`, by design), `src/story.rs`
  (`file_all`, dedupe), and `post.rs`'s commit-before-reply + story-token rules.
- Drift + patches: the branch-moved check in `post.rs` ("the branch moved since
  triage"), `src/patch.rs` (`write_batch`, `write_manual`), `src/git.rs`
  (`merge_base`, rebase helpers) for auto-rebuild.
- Spawn + guards: `src/spawn.rs` (`ensure_pr_worktree`, `spawn_command_session`,
  `start_with_prompt`, `spawn_session`), `hooks.rs` SessionStart `pending_prompt`,
  `guards/push.py`, and the single-run-per-PR / `branch_busy` / worktree-ownership
  guards in `src/fix_pr.rs`.
- Overlay + keys: `web/app.js` `rvCard` / `rvFinal` / `rvReport` and `reviewKey`;
  `.rv` styles in `web/app.css`.

## Phased build

**A. Triage card (mostly front-end).** Decision row = `accept · ⏎` / `manual · m`
/ `skip · s` as peers; primary button labeled by real effect per thread
(`reply + fix` / `reply` / `👍`); drop the standalone mode toggle. Stage `mode`
per thread. Files: `web/app.js` `rvCard` + `reviewKey`, `web/app.css`.

**B. Session driver.** `commands/resolve-run.md` + `src/prompt.rs::RESOLVE_RUN`
+ a `vendored_prompt_file` arm; **always** spawn one session (even words-only),
handed the ordered thread plan (stance / reply / mode / staged fix) and the head
it was triaged against. Daemon performs the outward writes per thread as the
agent signals completion.

**C. Interaction channel (spike, then build).** The blocking round-trip above;
`pending_interaction` session state + `/api/session/:id/answer`; SPA rendering of
the open-question card (reuse the `.oq` treatment from the design artifact) and
the diff-confirm card (show committed diff, auto-post-unless-object).

**D. Drift, resumability, honest overview.** Re-validate head at session start and
per thread; auto-rebuild each patch, abort a thread to `needs_you` only if it
can't apply. Durable per-thread run record with clean resume. Overview screen
shows the real state mix (done / retry / conflicted / needs-you); `Push` and
`Re-request` are explicit buttons; resolving stays the author's. Files:
`web/app.js` `rvReport` / `rvOverview`, run-record store, `post.rs`.

## Security / constraints

- Pushes use the agent's git creds under `guards/push.py`; the daemon PAT stays
  read-only (§6). `gh`-based outward writes keep using their own token as today.
- Reuse single-run-per-PR + worktree-ownership guards so a resolve run can't
  collide with `fix-pr` on the same PR.
- New endpoints sit behind the existing Origin/Host + token guard (`api.rs`).

## Verification

- **Unit:** patch rebuild on a moved head; run-record round-trips and resumes
  without double-posting; commit-before-reply ordering preserved.
- **Integration / e2e:** run resolve on a PR with a mix (agree / reply / story /
  agent-fix / manual); kill the session mid-run and resume; force-push mid-run
  and confirm re-validate + rebuild; assert no thread was auto-resolved and that
  push only happens on the explicit button.
- **Drive the real app** (`/run`) to exercise the open-question and diff-confirm
  cards end to end — the interaction channel can't be trusted from tests alone.

## Biggest risks

1. **The interaction channel (C).** Everything interactive depends on it. Spike
   before committing to B/D.
2. **Interactivity cost.** In-context questions (decision 4) + per-fix diff
   confirm (decision 6) make Phase 2 a *driven-with-you* session, not a
   walk-away one. Keep the copy honest: "the session drives, you approve each
   step."

## The nine decisions this encodes

| # | Decision | Choice |
|---|----------|--------|
| 1 | Phase-2 execution | Full agent session (agent applies + adapts fixes) |
| 2 | Branch drift | Re-validate + auto-rebuild; abort a thread only if unappliable |
| 3 | Partial failure | Resumable per-thread units + honest overview |
| 4 | Manual-question timing | Always in-context (Phase 2) |
| 5 | "Manual" naming | Keep "manual" |
| 6 | Reply vs real diff | Show committed diff; auto-post unless you object |
| 7 | Primary button | Per-thread by real effect |
| 8 | Triage run (Phase 0) | Explicit run screen |
| 9 | Session scope | Always run one session, even words-only |
