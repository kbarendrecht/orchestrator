# Architecture health check

Produced by `/architecture-health-check` on 2026-08-23, against `cb6a6d7`.
Read-only review; every finding was verified against the source before being
recorded here. Baseline was `README.md`'s module map plus `TODO.md` — decisions
those documents already defend are excluded rather than re-flagged.

Weighted towards what has grown: `web/app.js` (4925 lines, 98 commits),
`src/api.rs` (2383, 57), then `lib.rs`, `state.rs`, `spawn.rs`, `post.rs`.

## Architecture summary

- **`state.rs` owns authoritative state** as `Inner` behind one `RwLock`,
  projected to a single `Snapshot` that `ws.rs` pushes over a websocket. The SPA
  **fully replaces** `snap` per message — snapshot-replaces-never-merges, so
  there is no delta protocol to desync.
- **`api.rs` is the HTTP surface**; `lib.rs` wires the router and six
  single-purpose pollers.
- **Infra is layered**: `git.rs` (git primitives), `forge/` (a real `Forge`
  trait + `ForgeImpl` enum dispatch, GitHub the only impl), `spawn.rs`
  (session/worktree/process spawning), `hooks.rs` (hook receiver + generated
  settings), `worktree.rs` (preflight/archive/teardown), `store.rs`
  (persistence, orphan reaping).
- **The review pipeline** (`post.rs` → `proposal.rs`/`patch.rs`/`story.rs`/
  `triage.rs`) has two paths: the proven **batch** (`post::resolve` →
  `post_outward`) and the never-yet-executed **resolve-run**.

The dominant evolutionary pattern: **new behavior lands wherever its trigger
lives** — HTTP behavior in `api.rs`, UI behavior in `app.js`, review-commit
policy in `git.rs` — rather than in the module that owns the concern. That is
the root of most drift below.

## What is healthy (preserve these)

- **The snapshot projection + replace-sync model** (`state.rs::snapshot`,
  `ws.rs`, `app.js::connect`). The backbone; do not add a delta protocol.
- **Self-healing state over trusted flags**: `claim_main` re-checks `is_live()`
  before honouring a stored occupant; `load_automation` demotes persisted
  `Running` to `Exhausted`; auto-resume re-derives instead of resurrecting.
- **The `Forge` seam**: five methods, every one taking `at: &Path`, mechanical
  dispatch, `read_forge`/`write_forge` the only construction sites.
- **Clean git/forge boundary**: no `gh`/`curl` in `git.rs`/`worktree.rs`/
  `spawn.rs`; no `git` in `forge/`.
- **SPA edge discipline**: no client-side multi-step orchestration and no
  forge-URL construction (`openFileOnForge` is the model); the review batch's
  local draft state is deliberately client-owned.
- **`Stance` × `Mode` as independent enums** — a deliberate fix for a worse
  conflated enum, exercised through small named predicates.

## Findings

### HIGH

**H1 · Workspace-keyed derived state outlives the workspace; a reused id
inherits stale data** — *Fixed.* The four maps are gone; `changed`/`base`/
`divergence`/`rebasing` now live in a `Tree` struct on `Workspace`
(`model.rs`), written by reconcile and read by `snapshot`, so they are
destroyed with the entity they describe. Proven both ways: a regression test
(`state.rs`, `a_recreated_workspace_does_not_inherit_the_old_ones_measurements`)
passes on the new shape, and the same assertions run against the pre-fix code
fail with "stale base survived teardown". Read and write paths re-verified in
the running daemon.

- **Location:** `state.rs` (`Inner::divergence`/`changed`/`base`/`rebasing`),
  `worktree.rs` teardown, `spawn.rs` (`pr-{pr}` id reuse).
- **Problem:** a workspace's state is split across the `Workspace` struct *plus
  four parallel `Inner` maps* keyed by `WorkspaceId`. Teardown removes only
  `workspaces` and `files`; the other four are never cleared. Ids are
  deterministic and reused (`pr-{pr}`), so tearing down and recreating the same
  PR's worktree makes `snapshot()` serve the *previous* incarnation's
  changed-file list, ahead/behind counts and rebase flag until the next
  reconcile overwrites them.
- **Evidence:** teardown does `workspaces.remove` + `files.remove` and nothing
  else; `register_worktree` uses `.entry().or_insert()`; `WorkspaceView` reads
  all four maps by id regardless of which incarnation filled them.
- **Direction:** smallest — clear the four maps where `files` is already
  cleared. Deeper — fold the fields onto `Workspace` so they are created and
  destroyed with the entity, removing the whole class of bug.
- **Risk:** trivial for the removes; the fold touches every reconcile write and
  the `WorkspaceView` builder — mechanical but broad.
- **Necessary vs accidental:** accidental. Derived state was bolted on as new
  `Inner` maps feature-by-feature rather than attached to its entity.

**H2 · resolve-run re-implements the batch's write/post ladder instead of
sharing it** — *Partly fixed.* `post::post_one` is now the single-thread twin of
`post_outward`'s per-thread body, and `thread_committed` goes through it instead
of calling `forge.reply` bare — so the run files the story its reply links to,
substitutes `{story}`, and is idempotent on a retry, from the same code the batch
uses. `pr_resolve_run` now refuses on `triage::gate` before spawning, the way the
batch does.

Words-only threads are answered too: `sweep_words_only` runs off the plan once
the session is spawned, because no report will ever arrive for a thread with
nothing to build — the prompt tells the agent exactly that. No per-thread card
for them, deliberately: that card exists to show the real commit diff beside the
drafted reply, and here there is no commit and nothing can drift. It covers both
shapes a words-only thread takes — words (via `post_one`) or a bare 👍 (via
`react_one`) — and a test asserts every stance takes exactly one of the two, so
the sweep cannot silently skip one. The same reaction gap in `thread_committed`
went with it: its early return claimed a thumbs up was "posted with the rest",
and there was no rest.

**Still open:** the ask/wait poll loop in `thread_committed` is hand-rolled
rather than reusing `ask`/`ask_wait`. On reading it that looks justified rather
than drift — its own comment explains why it has no deadline, unlike `ask` — so
it is left alone. And none of this has met a real PR yet; the unit tests cover
the token, the idempotency and the branch coverage, not the round trip.

- **Location:** `api.rs` (`pr_resolve_run`, `thread_committed`), `spawn.rs`
  (`spawn_resolve_run`) vs `post.rs` (`run_inner`, `post_outward`,
  `already_replied`, `story::file_all`, `STORY_TOKEN`).
- **Problem:** the batch path enforces, in Rust: `triage::gate` (dirty tree /
  stopped rebase / fix-pr running) → `patch::check` (staleness, overlap) →
  atomic apply → pre-commit ladder → story filing → `STORY_TOKEN` substitution
  → `already_replied` idempotency. None of it is shared with resolve-run.
  `pr_resolve_run` calls only `spawn_resolve_run`; `thread_committed` replies
  directly with no idempotency check, no story filing and no `{story}`
  substitution, and hand-rolls its own ask/wait poll loop rather than reusing
  the `ask`/`ask_wait` primitive in the same file. No module owns "what happens
  to one thread in a resolve run".
- **Evidence:** `story::file_all`, `already_replied` and `STORY_TOKEN`
  substitution appear only in `post.rs`; `triage::gate` has no resolve-run
  caller.
- **Latent consequence** (the path has never executed): a story-stance thread
  posts the literal `{story}`; a words-only thread has no trigger that ever
  posts it; a retried `…/committed` double-posts.
- **Direction:** one owning function in `post.rs` that reuses the gate, story
  filing + substitution and `already_replied`; reduce `thread_committed` to a
  thin adapter over the shared ask/wait primitive. Do it *before* a real PR
  exercises the path — being unexecuted is exactly what makes it safe to
  consolidate now.
- **Risk:** low — unit-tested, unexercised; adding the gate is additive.

**H3 · `web/app.js` is a 4925-line monolith with no internal seams** — *First
section done.* The review overlay (~1900 lines, 69 names) is now behind
`const Review = (() => { … })()` exporting exactly four: `state`, `open`,
`close`, `key`. Measured before touching it — only those four were reached from
outside, and the `queue`/`offered` hits the first pass reported were prose in
comments, not code. The bodies keep their old indentation on purpose, so the
change is 21 insertions rather than a whole-file reflow. Driven in Chrome
against the running daemon: the four entries exist, the other 65 names are gone
from `window`, `Review.open(10007)` renders the intake screen, `Review.close()`
empties it, and a real Escape keystroke still closes it.

The diff viewer went next, and the measurement decided its shape: the editable
pane is the right half of the diff and reads `diffState` for which file it is on,
so the two are one `Diff` namespace rather than two — sealing them apart would
have turned that reach into a public call. Folding the *files* pane in as well
was measured and rejected: it drags in `act` and `prForWorkspace`, which are
general helpers that merely live in that range. Eleven names leave, more than
the overlay's four, because the file list drives the diff and the keyboard drives
both. Driven in Chrome: 1 hunk parsed and 124 nodes rendered, split toggles and
re-renders, the editor opens and closes, `Diff.close()` clears the overlay, no
page errors, nothing leaked to `window`.

The rail went third and came out the cleanest of the three: twenty-four names,
three out (`render`, `rowName`, `closeSession`). Getting there needed one move
first — `pending` lived in the rail but the drawer, the files pane, the overlay
and the review queue all ask it, so it went to sit with `PENDING_WORKTREE`
outside the seam. Same shape of finding as `act`/`prForWorkspace` keeping the
files pane out of `Diff`: what a section *contains* and what it *owns* are not
the same list, and only measuring tells them apart.

The privacy check got better here too, and it matters for the earlier two: `n in
window` cannot see a top-level `const`, so it passes vacuously for half the
names. Evaluating the bare name and requiring a `ReferenceError` is the real
test. Under it all sixteen rail internals are private and all seven app-level
names (`pending`, `isArchived`, `currentSession`, `activeWorkspaceId`,
`currentWorkspaceId`, `refreshButton`, `render`) are still reachable. The rail
draws (4 PR rows, 3 groups), survives its 1s re-render, `Rail.rowName` answers
for a real session, and a right-click still opens the PR menu — which is
`prMenu` inside `Rail` reaching `Review.open` inside another seam, so the seams
compose.

The terminals went fourth: nine names, four out (`show`, `close`, `resize`,
`fontSize`). `terms` stays outside deliberately — the zoom and the window chrome
iterate the map too, so the map is the app's and only the behaviour around it is
this section's.

This one nearly went wrong in a way worth writing down. Three of the four
apparent external uses of `resize` were **string literals** — two `'resize'`
events and a `resize/<edge>` URL path — so the regex rewrite used for the earlier
seams would have turned `new Event('resize')` into `new Event('Term.resize')` and
broken window resizing silently. Rewriting by explicit line number, asserting
each line really contains a *call*, is the safe form. The earlier three seams
were checked for the same damage afterwards and are clean, but they were lucky
rather than careful.

Verified against a real pty rather than a mock: a shell created in main,
`Term.show` attached it (7 rows, 104 cols, socket open), typing `echo
seam-alive-42` came back through the pty and rendered, `Term.resize` refit
104→74 cols, `Term.close` removed it from `terms`, no page errors, all nine
internals private and `terms`/`uiScale`/`toast` still shared.

The last two closed it out. The review queue is the smallest section — five
names, one out (`render`, which is all `render()` ever wanted from it). The
settings panel is nine names and three out (`isOpen`, `close`, `setup`), with the
*zoom* block deliberately left outside it: `uiScale` is read by `Term`, so it is
the app's rather than the panel's, and the panel merely offers the control that
changes it. That is the third time the same shape decided a boundary, after
`pending` for `Rail` and `act`/`prForWorkspace` for `Diff`.

**H3 is done: six seams, and nothing leaks.** `Term` (4 out of 9), `Rail` (3 of
24), `Diff` (11 of 26), `Review` (4 of 69), `Queue` (1 of 5), `Settings` (3 of
9). Every internal name throws `ReferenceError` from outside; the genuinely
app-level ones — `terms`, `pending`, `isArchived`, `uiScale`, `currentSession`,
`activeWorkspaceId`, `currentWorkspaceId`, `refreshButton`, `render`, `toast` —
are still shared, checked from both sides so the test cannot pass by
over-sealing. The file is the same length it was; what changed is that a new
feature now has an obvious place to go, and reaching across a boundary has to be
spelled `Diff.state` rather than happening by accident.

Verified in Chrome against the running daemon, per seam and then all six
together: the rail draws and re-renders on its 1s tick, a real pty attaches and
echoes, a real diff parses and folds, the overlay walks its screens, the queue
renders its rows, and the settings panel opens on the gear, loads real config
(`upstream/develop`) and closes — with `config.json` byte-identical afterwards,
since the panel was never saved. No page errors anywhere.

- **Problem:** seven feature areas (rail, terminals, diff, editable pane, review
  overlay at ~1900 lines, review queue, settings) are flat top-level functions
  sharing ~25 module-scope mutables; anything can mutate anything. 98 commits
  land here because it is where UI behaviour defaults to. It does not cause
  today's bugs; it is why small drift like M5 goes unnoticed for months and why
  each feature raises the cost of the next.
- **Direction:** keep the single-file `include_str!` constraint — it is
  deliberate. Add seams *inside* the file: a namespace object per area
  (`const Review = { state, render, open, close }`), review overlay first. No
  build step needed.
- **Risk:** low if incremental, one section per commit, verified against the
  existing `render()` entry points.
- **Necessary vs accidental:** the single *file* is necessary; the absence of
  internal *module* boundaries is accidental.

### MEDIUM

- **M1 · "This session is done" was observed twice.** *Fixed, and more simply
  than the direction first suggested.* No `Notify` was needed: `watch_session_exit`
  already exists, already runs for every session including a fix run, and is
  already the one thing that sees a pty end. It now reads the exiting session's
  `Kind` and, for a fix run, calls the new `fix_pr::settle` — so the verdict lives
  in `fix_pr.rs` where the guard table is, `watch_fix_pr` is gone from `api.rs`,
  and there is exactly one observer. The `"fix-pr"` command string is now
  `fix_pr::COMMAND`, since the spawn site and the place reacting to the exit have
  to agree and two literals is how they stop agreeing quietly.

  `settle` splits into a pure `verdict` plus the write, because the first version
  of its test called `settle` — which persists — and wrote a fake record into the
  *real* `automation.json`. A unit test must not touch the config dir; the
  decision is what is worth pinning, so `verdict` is what the tests call.

  Verified end to end, by accident: a genuine fix run was started against a real
  PR (see the note under M4 about the guard), and killing it exercised exactly
  this path — the single watcher saw the exit, dispatched to `settle`, and because
  the PR was green the record was correctly removed rather than marked exhausted.
- **M2 · `git.rs` carries review-batch commit policy (~500 lines).**
  `amend_target`/`fold_in`/`pre_commit`/`Amend`/`PreCommit` encode batch domain
  policy, and `patch.rs` is their only caller; the file's own section comment
  (`// The review flow's writes`) admits it. *Direction:* move to
  `review_commit.rs` or into `patch.rs`. Pure code motion.
- **M3 · `spawn.rs`'s health parser mutates session state, duplicating
  `hooks.rs`.** `scan()` flips `BuildFailing`/`YourTurn` from inside the process
  module — the same transition `hooks.rs::stop` derives independently, with
  duplicated build-failure logic. *Direction:* extract a `health.rs`; reconcile
  the health→session-state rule in one place.
- **M4 · api.rs handlers inline orchestration owned elsewhere.** `revive()`
  inlines worktree rebuild (`worktree.rs` owns every other worktree verb);
  `open_pr`'s `"main"` arm inlines occupancy + `is_clean` + `switch_branch`
  while its `"worktree"` arm is a one-line delegate. *Direction:*
  `worktree::revive` and `spawn::switch_main_to_pr` so both arms are delegates.

  Worth knowing while working near this: `POST /api/pr/:n/fix-pr` on a green PR
  of your own **starts a run immediately**, no confirmation. That is the design
  (hand-triggered means the trigger is the confirmation), but the guard table only
  refuses on authorship, an existing run, a busy branch and the concurrency cap —
  "the PR is fine" is not a refusal, because a run is also how you rebase a PR
  that has simply fallen behind. Easy to fire by accident when poking at the API.
- **M5 · Divergence banner hardcoded `upstream/develop`.** *Fixed.* The ref is a
  snapshot field now and the strip interpolates it, so the name beside the
  numbers is the ref they were measured against. Verified the only way it could
  be: a daemon started against an isolated `HOME` whose config named
  `origin/develop` — a real ref, deliberately not the default — where the strip
  reads `1 behind origin/develop, 288 ahead` and the button `git rebase
  origin/develop in main`. Against the default config the bug was invisible,
  which is why it survived.

### LOW

- **L1 · Durability is a call-site property.** `sessions.json` re-persists on
  every `notify()`, but `manual`/`automation`/`stories` persist only because
  each mutation site remembers its `save_*`. Disciplined today, structurally
  unenforced. A `mutate-and-persist` wrapper would harden it.
- **L2 · resolve-run record is memory-only** — lost on restart while its commits
  survive. Already a TODO known-gap; located here for precision. Persist beside
  `plan.json` with the pattern `manual`/`automation` already use.
- **L3 · `prompt.rs`'s render test omitted `RESOLVE_RUN`.** *Fixed.* The newest
  and most interpolated prompt (three built URLs) is now in the "nothing left
  over" guard. Checked the guard actually bites: appending a bogus `{{…}}` to
  `commands/resolve-run.md` fails the test, so it is covering the file rather
  than passing on a template with nothing to substitute.
- **L4 · Stringly-typed route classification.** `is_ask` (path suffix) and
  `SPENDS_GITHUB_TOKEN` (hand-maintained list) grow by convention; the list's
  own comment records once being forgotten. Advisory only.
- **L5 · Unbounded growth.** `human_edits` never prunes; `spinFloor` is an
  ad-hoc per-kind flag map. Low blast radius; watch-items.

## Top 3 highest-value improvements

1. **Fix workspace state ownership (H1).** Best value-to-cost. Change: clear the
   four derived maps on teardown; ideally fold them onto `Workspace`. Unchanged:
   the snapshot projection and reconcile logic. Verify: create `pr-N`, dirty it,
   teardown, recreate, assert the pane and behind/ahead are empty before the
   first reconcile.
2. **Consolidate resolve-run onto the shared ladder (H2), before it meets a real
   PR.** Unchanged: `post::resolve`/`plan` (already genuinely shared), the batch
   path, and the in-session commit mechanic — an adaptive agent must touch files
   itself; only the *gate* and the *outward posts* should be shared. Verify: a
   story-stance and a words-only thread driven end to end.
3. **Introduce internal seams in `app.js` (H3), review overlay first.**
   Unchanged: the single-file constraint, the snapshot-replace model, the
   `render()` entry points. Verify: `mise run shot` per section, plus exercising
   that section in the running app (needs a rebuild — the SPA is compiled in).

## Investigated and deliberately left alone

- **`Forge` being GitHub-only** — wired correctly; a second forge is a variant,
  not a rewrite. TODO-owned.
- **`patch.rs`'s `write_batch` vs `write_manual`** — the near-duplication is two
  forced differences (blame source, file-list source), documented, both
  exercised by one flow. Not a merge candidate.
- **`Stance` × `Mode` × `patch`** — deliberate, replaces a worse conflated enum.
- **The batch/manual resolve path** kept as the secondary button — retire only
  once resolve-run has done real work.
- **Snapshot-replace, main self-heal, auto-resume re-derivation** — the healthy
  core.
- **`github_write` refusing to resolve/merge/approve** — a design boundary.
- **The DOM terminal renderer, the `ai-title` field, the changed-files pane
  recomputing** — all TODO-defended.
- **`push.py` materialised from `hooks.rs`** — needs to be an on-disk
  executable for the hook; the odd home has a real reason.
