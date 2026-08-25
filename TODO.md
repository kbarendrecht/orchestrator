# TODO

The **Live findings** block below is rewritten by the daemon every poll. It only
ever lists conditions that are true right now, so it stays worth reading.
Everything outside that block is hand-written and survives.

## Next

- **A fixture PR to test the review flow against.** *Built — `mise run fixture`,
  `tools/fixture-pr.mjs`, written up in `docs/fixture-pr.md`.* The wall was that
  `query_for` polls `author:@me` while `acknowledged()` reads a thread whose last
  comment is yours as answered, so one account cannot both own the PR and leave a
  thread waiting on it. The second identity is `github-actions[bot]`: a
  `workflow_dispatch` workflow on the fixture's default branch posts the threads
  with its own `GITHUB_TOKEN`, which needs no second account and no stored
  credential.

  Driven once, and this is what a daemon on it reports — the state four earlier
  attempts could not produce: `unresolved: 3, awaiting_you: 3`, `answerable: 3`,
  `gate: null`, `head_sha` populated, every thread's last comment by
  `github-actions`. `ORCHD_CONFIG_DIR` keeps all of that off the real
  `sessions.json` and out of this repo's TODO.md.

  **The resolve run end to end — driven, and the seam was broken.** See the
  resolve-flow item below: a run answered three real threads on PR #9, and the
  first attempt could not reach the daemon at all.

  **The thumbs-up idempotency guess — settled, and it was right.** The ignored
  `posts_for_real` test (`forge/github_write.rs`), pointed at the fixture PR,
  👍'd the same comment twice and got the same reaction id both times
  (`429129039`). So GitHub treats a reaction as unique per (user, content) and a
  retry is a no-op — no ledger, no `reactions` in the thread query. The hedge in
  `post.rs` is gone; the reply path (`with_footer`, `in_reply_to_id`) came back
  correct in the same run.

  **`open_file`'s `head_sha` arm — done, no code change.** With a `pr-4` workspace
  on `fixture/pricing`, `POST /api/open/file` minted a blob URL against the PR's
  pushed head sha (`c01649a…`). To prove it was the head-sha arm and not the
  local-`HEAD` fallback, a local-only commit moved the worktree's HEAD to
  `1c7c8f5…`; the URL still used `c01649a…`, so the arm at `api.rs:1543` won as
  designed. The fallback still holds too: `open_file` on `main` (no PR names it)
  used main's local HEAD. This is the arm the note below said had never run.

  **Teardown and the archive it runs — done, and it found a bug.** Driven live
  against a fixture worktree; see the teardown item under "Decisions worth
  revisiting". The archive auto-run is exactly right; the removal after it was
  broken on the default `claude --worktree` layout by a stale lock, now fixed in
  `src/git.rs`.

  **`triage::gate`'s refusal on a dirty worktree — done, no code change.** The
  precondition the daemon could never assemble before: a polled PR (#4) with a
  `pr-4` worktree, dirtied on purpose. `POST /api/pr/4/triage` on it returned
  `400 "2 uncommitted file(s) in this worktree — commit or stash first"` — the
  real refusal at `triage.rs:127`, not a reasoned one — and `GET …/review`
  reported `gate: {"gate":"dirty","files":["debug.js","src/pricing.js"]}`, the
  actual list with both a modified tracked file and an untracked one. Cleaning the
  tree cleared the gate back to `null`. All three callers share the one
  `gate_inner`, so the resolve-run refusal (`api.rs:1876`) is the same check.

  One thing the fixture deliberately does **not** cover: `rerequest()`. A bot
  cannot be a requested reviewer, so that button still wants a second human
  identity — a throwaway account or a fine-grained token for one.

  What was unverifiable before it existed, each found by trying:
  - **The whole resolve flow.** `acknowledged()` (`src/forge/github.rs`) treats a
    thread whose last comment is yours as answered, so you cannot self-review your
    way to a testable thread. Everything downstream — triage, the per-thread card,
    `post_one`'s story substitution and idempotency, `sweep_words_only`, the 👍
    path — is unit-tested and has never made a real round trip.
  - **The `triage::gate` refusal on a run.** *Now verified* against the fixture —
    a dirtied `pr-4` worktree, refused with the real file list. It needed a PR in
    the poll with a worktree that could be dirtied without touching real work,
    which no monorepo PR could offer.
  - **A PR head sha in a link.** *Now verified* — a `pr-4` worktree on the PR's
    head branch made `open_file` mint the URL against the PR's pushed sha, proven
    distinct from local HEAD by a local-only commit. Before the fixture, no
    workspace's branch matched a polled PR, so only the local-`HEAD` fallback ran.
  - **Teardown, and the archive it now runs.** Both only fire on a worktree with a
    session's transcript in it, and the only ones to hand are real work.

  Settled once the fixture existed: the thumbs-up idempotency assumption
  (`post.rs`) — GitHub returns the existing reaction, not a second one (see above).

- **The two-phase resolve flow — it has now answered a real reviewer, and the
  seam it turns on was broken until it did.** Driven against the fixture (PR #9,
  three threads really awaiting an answer): plan → session → a commit per thread →
  the real diff beside the drafted reply → the daemon posting on its own
  credential. What landed on GitHub, checked there and not inferred: two replies,
  one 👍 on the `agree` thread, **no** thread resolved, and both commits left
  local until the push button, which then moved the branch. `sweep_words_only`
  answered the words-only thread at start; the bare-thumbs-up-after-a-commit arm
  (`react_one`) fired for the third.

  **The bug this found, which no test could.** `POST
  /api/session/:id/thread/:t/committed` — the one route the whole design turns on —
  answered `403 bad origin` to its only caller. The guard's ask-token exemption
  matched `/ask`, `/wait` and `/spawn`; `/committed` arrived with the resolve run
  and was never added, and its comment still said "the agent's own *two* routes".
  Broken twice over: a curl that added an `Origin` would then have failed
  `needs_token`, since the agent deliberately holds the ask token and not the app
  token. The agent's own report of it was accurate — "bad origin — the daemon
  rejected that" — and it then spent a turn probing the guard. `is_ask_route` is
  now a named list and a test asserts every path the vendored prompts curl.

  Two smaller things the same run turned up:
  - **`posted: false` meant two opposite things.** A bare 👍 and a held-back reply
    both return it, distinguished only by `reacted` versus `reason`, which the
    prompt did not say — so the run's final report told the human thread 3 was
    "held back for you to answer" when the daemon had reacted exactly as designed.
    Fixed in `commands/resolve-run.md`; the API already carried the distinction.
  - **A run cannot start unattended.** Its first act is reading `plan.json` in
    `config_dir`, outside the worktree, so Claude Code asks permission before the
    agent has read a word of the plan — then again for each commit and for the
    `committed` curl. Fine in the app, where you are watching the pane; worth
    knowing before anyone calls a run walk-away.

  Where the rest stands:

  *Built and working.* The triage card is three decisions rather than one list of
  positions (`rvStance` / `rvReply` / `rvFix` in `web/app.js`), backed by a model
  where `Stance` is agree/reply/story, a patch is the only thing that says code
  changes, and `Mode` (agent/manual) rides the decision because it is yours, not
  the agent's (`src/proposal.rs`). The interaction channel is real and was driven
  end to end against the running app: a session POSTs a question to
  `/api/session/:id/ask`, long-polls `…/ask/:ask/wait` in bounded 60s loops, and
  the card over the pty releases it. An option marked `free` opens a box instead
  of answering, so "let me write it" can. The session holds `ORCH_ASK_TOKEN`,
  which opens asking and nothing else — deliberately not `ORCHD_TOKEN`, which
  would open all 41 routes and make "the daemon owns outward writes" a promise in
  a prompt rather than a mechanism.

  *Built, and now executed once.* `POST /api/pr/:n/resolve-run` resolves your decisions
  through the same `post::resolve` the batch uses, writes `plan.json` beside the
  prompt, and spawns a session on `commands/resolve-run.md`. The session commits
  per thread and calls `…/thread/:id/committed`, which blocks while you look at
  the *real* commit diff beside the drafted reply and decide; the daemon posts on
  its own credentials or holds it back. The run's per-thread state is in the
  snapshot as `resolve_runs` and rendered by `rvRun`, with push and re-request as
  their own buttons. All of that has now run against a real PR once, driven over
  the API rather than through the SPA — so `rvRun`, `rvOverview` and the cards as
  the *overlay* draws them are still unexercised, and so is `manual` mode, the
  story arm (the fixture daemon runs `tracker: none`) and `rerequest`.

  *Deliberately still there, and the bar for retiring it was raised rather than
  met.* The old batch (`/api/pr/:n/post` and the manual phase) is the secondary
  button on the final screen. A run has now done real work, which was the stated
  condition — but judged against this repo's own bar (a concrete problem, evidence
  it bites, a consequence of leaving it) the retirement does not qualify:
  - Every review answer would start costing an agent session. A run always spawns
    one, even words-only, which was the original reason to keep the batch.
  - It would delete the proven path for the unproven one. The run has answered a
    real PR twice, both times over the API, and its `manual` mode has still never
    executed.
  - The batch's resumability is real; the run's is not. `manual.json` carries a
    half-done discipline across a crash mid-post; `resolve-runs.json` makes a run
    *legible* after a restart, not resumable.
  - It is ~1500 lines including machinery with no replacement: `patch.rs`'s
    three-pass `git apply` ladder and `review_commit`'s blame-to-fixup-target
    code. The run has no daemon-side apply at all — the agent does it.

  A fairer bar: `manual` mode exercised, the overlay driven in a browser, and a run
  having answered a real monorepo PR rather than a fixture. Note also that this and
  the **beta gate** below are separable — the gate can be flipped without deleting
  anything.

  *What was worth doing now: the duplication that mattered.* The only real argument
  for retirement was two implementations of "the daemon answers a reviewer", and
  those are collapsed:
  - `post_outward` now goes through the same `with_story_id`, `send_reply_once` and
    `react_one` a run uses, so the three rules (story before reply, `{story}` never
    reaches GitHub, no double post) have one implementation each. The batch keeps
    its bulk story filing — one session for the whole batch, not one per thread —
    and its own report shape, which is right: the report is the batch's output, not
    a rule.
  - `Handled.root` went with it. Both paths look the comment id up from the fetch
    that is doing the writing, so carrying it was a second copy of the answer. The
    lookup stays in `resolve` as a check, because refusing a thread with no comment
    before anything is committed costs nothing.
  - **And it found a bug: the run's re-request button could never re-request
    anyone.** `pr_run_rerequest` derived "still open" from `!is_resolved`, but
    closing a thread is the reviewer's own button and the daemon never presses it
    (that is a design boundary, not a gap) — so every thread a run had just
    answered still counted as holding its author back. `post::rerequest_all` is the
    one implementation now.
  - Driving that fix turned up a *second* layer no amount of sharing would have
    fixed. `split_reviewers` counted only `answerable` threads as reviewers, which
    works for the batch because it judges a fetch taken *before* it posts — and
    left the run's button finding nobody, because by the time it fetches, every
    thread it answered has your comment last. "Who reviewed" is now every thread's
    author; "who is still owed an answer" is `!done && answerable`. Measured both
    ways on the fixture: the old binary answered `rerequested: [], failed: []`, the
    new one selects the reviewer and reports GitHub's real refusal (a bot cannot be
    a requested reviewer — the fixture's known limit). `rerequest()` itself is
    still unverified for want of a second human identity.

  *The three known gaps are closed, and each was driven against the fixture
  daemon rather than reasoned about.*
  - **"Unpushed" is said, and it is measured.** `Tree.unpushed` counts commits the
    branch's own remote does not have, taken beside the divergence on every
    reconcile, and `RunView.unpushed` carries it to the overview. It is a different
    number from `ahead`, which is against the *base* — driven to two commits past
    the base with one pushed, where `ahead` said 2 and `unpushed` said 1. The push
    button re-reconciles, so pressing it clears the line rather than leaving a
    stale claim. A branch that was never pushed falls back to the base count, the
    same reading `git::unpushed` already took.
  - **`needs_you` is reachable.** `POST …/thread/:t/stuck` is the counterpart to
    `committed`: the session says what stopped it, the thread goes to `NeedsYou`
    with that note, nothing is posted and nothing blocks. It refuses an empty note,
    because a thread taken off the "still moving" list without a reason is worse
    than one left on it. `commands/resolve-run.md` now says when to use it and that
    a thread neither committed nor reported reads as one not yet reached.
  - **The run record is durable.** `resolve-runs.json`, written through
    `Inner::with_resolve_runs`, restored at boot. `ResolveRun.ended` is set when the
    session exits — the only place that learns it — and on load, where nothing can
    have survived. So threads left `pending` are shown as abandoned rather than
    imminent. Driven both ways: a killed session ended the run, a restart recovered
    it with its statuses and notes, and a record still marked live on disk came back
    naming the restart.

    Persisting it needed one thing fixing first: `PlannedThread`'s three
    bookkeeping fields were `skip_serializing`, so the type could not round-trip at
    all — the fields a restart most needs were the ones no serializer would write.
    The agent's `plan.json` is now a view (`Plan::for_agent`), which is where that
    concern belonged, with a test that keeps the two documents apart.

  *"Nothing re-validates drift per thread" — the item was wrong, and what it
  pointed at is now checked.* Assessed before being built, and a literal reading
  would have been a bug: the agent commits once per thread, so from the second
  thread on `HEAD` differs from `base_sha` **by design**, and a per-thread equality
  check would fire on every thread but the first. That is why the prompt checks it
  once. The adaptation the plan's decision 2 asked for was already there too — the
  prompt has the agent rebuild a patch whose surroundings moved, and the first
  fixture run did exactly that and said so. Three neighbouring risks were also
  already covered: `commit_diff` fails on a sha that does not exist,
  `thread_committed` re-fetches the threads, and the push is
  `--force-with-lease`.

  What was genuinely missing was one thing, and it was ancestry rather than drift:
  if the branch is **rewritten under the run** — a push from another machine,
  somebody force-pushing your branch — the agent's commits sit on an orphaned
  history, and the daemon would keep posting a reply per thread about a fix that
  can never land. The lease would refuse the final push, but only after several
  public comments had claimed the work. `thread_committed` now asks whether the
  plan's `base_sha` is still an ancestor of `HEAD` before it posts; if it is not,
  the reply is held, the thread goes to `NeedsYou` naming the sha, and the prompt
  tells the session to stop rather than work threads into the same hole. Driven
  both ways on the fixture: ancestry intact posted the 👍, and a branch reset below
  the base was refused with the note. A `base_sha` git cannot resolve counts as
  "not an ancestor" — fail closed, pinned by a test, since nothing covered
  `git::is_ancestor` at all.

  *Still open here.* The overview's own rendering is type-checked, not driven —
  `rvRun` was exercised through the API, never through the overlay in a browser.

- **Promote the in-UI review overlay out of beta.** The overlay now does the
  real work: threads listed under the PR with their file, hunk, and a reply box,
  and replies/reactions/re-request go straight through the GitHub API
  (`src/forge/github_write.rs`). It ships as the `resolve in ui [beta]` menu item
  beside the old `/resolve`-into-a-terminal path. What remains is deciding when
  to make the overlay the default and retire the terminal spawn. Resolving a
  thread is deliberately *not* an API call — that is the author's button, by
  design (`forge/github_write.rs:10-13`) — so this item is about the beta gate, not
  the missing action.

- **Rewind a session from the rail.** Claude Code already has this: a double
  `Escape` at the prompt opens its own rewind picker, and the strings in the
  bundle show what it can do — `rewindToMessageIndex`, `rewindAnchorUuid`,
  `rewind-files` / `rewindDirectory` (it can put the files back too), plus
  `rewind-refusal` and `rewind-unavailable` for when it cannot. So the daemon
  does not need to build a picker, only a way to reach the one that exists.

  The cheap version is a context-menu item on a session row — `rewind`, or
  `time travel` — that selects the session and writes `\x1b\x1b` into its
  terminal. That is a pure SPA change: `term.onData` (`web/js/term.js:164`)
  already sends keystrokes down the attach socket, so a menu item can send the
  two escapes the same way. No route, no daemon state, nothing to persist.

  Two caveats worth checking before building it. The picker only opens at the
  prompt, so the item wants the same gating the nudge uses — a session mid-turn
  should not get it, and one at `asked_a_question` would answer the question with
  an escape. And a rewind that restores files changes the worktree under the
  changed-file pane, so it should be followed by a reconcile the way
  `watch_session_exit` does.

  A modal of our own is the alternative, and it is a bigger thing: the daemon
  would list the conversation's turns (it already tails the transcript for
  `store::ai_title`) and offer them as rewind points. That only pays off if the
  native picker turns out to be hard to reach or too coarse, and it would need a
  way to select a message index from outside the TUI, which the CLI does not
  expose — `--resume` resumes at the end and nothing else.

- **`ng-watch` sits at `starting` and never comes alive.** Observed on this
  machine and never investigated. Carried over from the swap handoff so it is not
  lost with the file; it is the health-scan spec's own process, so it is either
  the pattern in `config::default_main_processes` or the process really is stuck.

- **Stacked-PR support.** Two halves. First, a context-menu `stack` action on a
  PR row that opens a session starting from that PR's code — a new branch based
  on the selected PR's head, its own worktree (cwd = main, via the existing
  `worktree-create`/`worktree-link` hooks), and an interactive session. This is
  the `/resolve` spawn machinery pointed at a *new* branch off a PR head rather
  than the PR's own branch. The stack is then detected for free: `link_stacks`
  (`src/forge/github.rs`) already matches `child.base_ref == parent.head_ref`. Second,
  a semi-automation in the spirit of `fix-pr` — a `/restack` (or `sync`) skill
  that keeps a stack in sync: when a base PR's head moves (amend/rebase), rebase
  the children onto it bottom-up and re-push, within the existing push guards
  (`--force-with-lease` only, `push -u` ban, protected refs). Reuses the
  `PrAutomation` per-PR run model and the skill-spawn path; the bottom-up
  serialized ordering is the piece §8 described but never built. Two known
  wrinkles: the stack DAG is stored children-only (`Pr.children`), so a restack
  must derive the parent chain by inverting it — there is no `parent`/`base`
  pointer; and if it rides an agent session like `fix-pr`, the `git rebase
  --onto <new-parent-head> <old-parent-head> <child>` logic lives in the skill
  prompt (a new `prompt::RESTACK` + `vendored_prompt_file` arm), so no new Rust
  git primitive is strictly required. The per-PR-keyed guards
  (`authorship`/`branch_busy`) would need a chain-aware variant.

- **Findings from the portability review still worth keeping.** A high-effort
  review of `559c803..ff387df` surfaced these; the ones about the built-in review
  ranking are gone now the queue is back to a command, and what remains is
  recorded below.
  - **`worktrees_subdir` normalisation.** *Done.* `parse` now
    sanitises the field once via `normalize_worktrees_subdir` — drop `.`
    components, refuse absolute / `..` / anything that normalises to nothing —
    and stores the clean path, so `""`/`"."`/`"./x"` no longer collapse the
    worktrees dir onto main or make the exclude prefix `/` (the §2 sibling leak).
    Validation moved out of the per-call accessor that re-warned on every hook
    event; the accessors now trust the field.
  - **The forge seam is now real, not nominal.** *Done.* `ForgeKind` is read by
    `ForgeImpl::for_kind` (`src/forge/mod.rs`), the enum-dispatch handle every
    caller now holds instead of a concrete `GitHubForge` — reads *and* writes go
    through the `Forge` trait, including `post.rs` (which threads the forge + the
    worktree `at` rather than a `Target`) and the two `api.rs` write endpoints.
    The four `GitHubForge::new` sites collapsed into one factory plus `read_forge`
    /`write_forge` helpers in `api.rs`. A second platform is a `ForgeKind` arm, a
    variant, and a `Forge` impl — no call-site edits. Enum rather than `dyn`
    because the trait is `Clone` (the write helpers move a copy into
    `spawn_blocking`). The two GitHub-shaped leaks still stand for a real second
    forge: `ThreadRoot`'s `comment_id` is a REST id, and the read token ladder is
    GitHub's.

  Smaller ones from the same review, same kind as the `ForgeKind` note — all now
  fixed:

  - **Upstream-remote mismatch is surfaced.** *Done (surfaced, not unified).*
    `parse` warns when `upstream_ref`'s remote prefix disagrees with
    `upstream_remote` — the two feed the base fetch and repo detection and were
    silently divergable. Fully collapsing them to one field is a config-shape
    decision left for later; they agree in the defaults.
  - **Pager cursor logic unified.** *Done.* `all_summary_threads` and
    `parse_thread_page` now use the shared `next_cursor`, so the
    hasNextPage/endCursor rule lives in one place, not three.
  - **`default_for` goes through the same parse as a real load.** *Done.*
    `default_for` builds through `parse(json!({main_checkout}))`, so field
    defaults live only in the `#[serde(default = …)]` attributes and a first run
    cannot diverge from the same file parsed off disk.
  - ~~`resolve_repo` + `resolve_token` + `GitHubForge::new` repeated four times~~
    *Done as part of wiring the seam:* the two `api.rs` sites are now
    `read_forge`/`write_forge`, and construction everywhere goes through
    `ForgeImpl::for_kind`. The PR poller still resolves token/repo inline for its
    token-source reporting; the review poller no longer touches the forge at all
    now the queue is a command.
  - CLAUDE.md test count. *Done* — 283 passing, 4 ignored.

- **Make it run somewhere other than this machine.** Everything below is a
  hardcoded assumption about one monorepo, and each is a setting or a probe
  waiting to be written:
  - **The stack-specific settings are the defaults, editable in settings.**
    *Landed, then simplified, then made generic.* This was a two-profile split; it
    was retired in favour of making the six settings (upstream ref/remote,
    tracker, output language, `main_processes`, review command) built-in
    `#[serde(default = …)]` values, and exposing them in the settings panel
    (`GET`/`POST /api/config`, `config::Settings`) which writes only those keys
    back to `config.json`. Those defaults carried one monorepo's toolchain for a
    while, which made every other repo read as broken rather than as not having
    it; they now ask nothing of the repo they point at. Apply is on restart (the
    running `cfg` is immutable; live-apply would be a sweeping refactor, left for
    later).
  - **The review queue runs a configured command.** *Reverted to this on
    purpose.* A built-in GraphQL queue with a config-driven ranking engine was
    built and worked, but it was more machinery than the one real user wanted to
    own, so the daemon went back to shelling out to `reviews_command` and
    rendering its JSON (`docs/reviews-json.md`, `src/reviews.rs`). A repo with such
    a task points this at it, where the ranking already lives and is edited as
    bash; a plain checkout leaves it empty and the pane reads `off`. The
    trade-off, accepted: a fresh checkout on another machine gets **no** queue
    until it configures a command — the portability the built-in version gave for
    free is gone. The `Forge` seam stays (PRs, threads, writes); only its
    `review_candidates` arm was removed. Revisit only if a second consumer ever
    wants a queue without a script.
  - **Docker and `ng` are managed processes, not hardcoded.** *Done.* `ng-watch`
    (`silent:exec:toolbox ng build --watch`) and `docker` are `ManagedSpec`s in
    `default_main_processes`, both `autostart:false`, editable in settings — so a
    fresh checkout starts nothing behind your back, and a repo without them clears
    the list. A probe that *suggests* processes on first run (compose file,
    package.json script) was scoped and deferred.
  - **The worktree layout is configurable.** *Done.* `worktrees_subdir`
    (`src/config.rs`, default `.claude/worktrees`) is the relative-in-main path
    `worktrees_dir()`/`worktree_path()` compose, and the changed-files exclude in
    `git::status` follows it (`worktrees_subdir_str`) rather than a hardcoded
    literal. An absolute or `..` value is refused back to the default, because the
    container mapping, the exclude and path attribution all assume worktrees sit
    under main. The old premise here was half wrong: Claude Code's built-in
    `claude --worktree <name>` already targets `<repo>/.claude/worktrees/<name>`
    on branch `worktree-<name>`, so a generic checkout needs no `worktree-create`
    hook to be recognised — the only real gap was that the path was a constant.
    Off the default subdir, creation is daemon-owned rather than delegated (see
    the worktree-creation item below), so a relocated layout is created where the
    daemon looks for it.

  - **Worktree creation is half-decoupled; the session model still assumes Claude.**
    `spawn::spawn_worktree_session` now branches: at Claude Code's default layout
    it delegates to `claude --worktree` (the repo's `worktree-create` hooks do the
    work), but at any other `worktrees_subdir` the daemon cuts the worktree itself
    with `git::worktree_add_new` (`-b worktree-<name> … <upstream_ref>`) and spawns
    a plain session — so the *coding agent* no longer has to be the thing that
    creates the worktree. What is not done: forcing daemon-`git` creation even at
    the default layout (staying on `--worktree` is what using that layout means),
    which
    would want an explicit mode rather than keying off the subdir. And the real
    coupling is the *session model*, untouched: `--session-id` correlation,
    transcript slug lookup, the `ai-title` field, `--resume`, and the whole
    hook-observer plumbing (`--settings` injection, SessionStart/PostToolUse/Stop).
    Hosting another agent means abstracting *that* layer; the worktree-creation
    split was the first, self-contained step, not the whole job.

    *Re-checked against the code, and it is accurate — with two things worth
    adding.* **Only interactive worktrees ever delegate.** `ensure_pr_worktree`
    calls `git::worktree_add_existing` unconditionally, whatever the subdir, and
    resume rebuilds with `git::worktree_rebuild` — so every worktree the *review*
    flow makes is already daemon-cut, and `claude --worktree` is reached from one
    place only (`spawn_worktree_session`, default layout, no PR). The daemon-git
    path is therefore the exercised one, not the theoretical one. And **both arms
    still spawn `claude`** — the non-delegated arm runs a bare `["claude"]` — which
    is what "half" means here: creation is decoupled, the session is not. Also
    worth knowing when comparing the two: only the delegated arm leaves a
    `git worktree lock` behind, which is why teardown needs the stale-lock clear
    at all.

    *Repo worktree setup is now first-class, whoever cut the tree.* The gap this
    left — a daemon-cut worktree fires no `WorktreeCreate`, so it skipped whatever
    creation-time setup the repo does — is closed by a `worktree_setup` command
    (`config.rs`, editable in settings). `spawn::run_worktree_setup` runs it in
    every worktree the daemon cuts (PR, resume, relocated), after creation and
    before the session, bounded and non-fatal (`proc::run_bounded`, extracted from
    the reviews timeout so there is one bounded-exec primitive). A relative script
    path resolves against main while cwd is the worktree — the usual hook idiom.
    It is deliberately *not* run on the `claude --worktree` path, where the repo's
    own `WorktreeCreate` already ran. Driven against the fixture: the marker landed
    in a daemon-cut `pr-N` worktree. The concrete symptom that motivated it —
    `pr-*` worktrees missing a rules-dedup file and so loading rules twice — is
    being fixed on the repo side too (moving that write to the
    `SessionStart` hook), so the two approaches converge: the repo makes its own
    setup idempotent, and orchd guarantees a creation-time hook point regardless.
  - **GitHub is the only forge, but the seam is wired.** Every read and write
    goes through the `Forge` trait (`src/forge/mod.rs`), and `ForgeImpl` —
    enum-dispatch keyed on `config.forge` (`ForgeKind`) via `ForgeImpl::for_kind`
    — is what every caller holds; `GitHubForge` (`forge/github.rs` GraphQL,
    `forge/github_write.rs` shelling `gh`) is its one variant. Model types are
    forge-agnostic (`forge/model.rs`). A second platform is a `ForgeKind` arm, a
    `ForgeImpl` variant and a `Forge` impl — no call-site edits. Two known
    GitHub-shaped leaks to generalise then: `ThreadRoot`'s `comment_id` is a REST
    id, and both `GitHubForge::detect`'s URL-parsing and the read-token ladder are
    github.com-specific (a real second forge wants its own credential resolution,
    which `for_kind`'s single `token` arg does not yet model).
  - **Output language is a setting; the tracker is config already.** *Done.*
    `output_language` (`src/config.rs`, default `Dutch`, editable in settings)
    fills a `{{LANGUAGE}}` placeholder the triage and resolve prompts use for the
    prose the agent *writes* — replies and story text. Prompts and code stay English, and a thread's own language still wins
    when it is clear. The `tracker` (`Tracker` enum) is already a config field
    (`none`/`shortcut`/`stub`); Shortcut is still the only real backend, and
    adding another (Jira, Linear, …) wants a seam, not a setting — see the next
    item.
  - **Give the tracker the same seam the forge has.** Right now the tracker is
    Shortcut, nominally behind a `Tracker` enum but not behind a trait — the
    Shortcut specifics are spread through `story.rs`: the MCP server name in the
    allowlist (`mcp__shortcut Read Write`), the `SHORTCUT_API_TOKEN` env the MCP
    entry reads, and `Story::url`'s knowledge of Shortcut's URL scheme. Mirror the
    forge: a `Tracker` trait plus a `TrackerImpl` enum-dispatch keyed on
    `config.tracker` via `TrackerImpl::for_kind`, holding what a tracker needs to
    be described — the MCP server id and tool allowlist, the token env/file, the
    story-URL grammar, and whatever the prompt needs to name the destination — with
    a tracker-agnostic `Story` model beside it (the way `forge/model.rs` sits next
    to the trait). Then a second tracker is a `Tracker` arm, a `TrackerImpl`
    variant and an impl — no `story.rs` edits — exactly as a second forge is today.
    Two things to settle while doing it, both the tracker analogues of the forge's
    known leaks: the token ladder (`ORCHD_SHORTCUT_TOKEN` → file) is Shortcut-named
    and would want per-tracker resolution like `for_kind`'s single `token` arg does
    not yet model; and `Stub` should become the trait's test double rather than the
    `--strict-mcp-config` special-case it is now, so the seam is what the stub
    proves. Not worth building until a second tracker is actually wanted — the same
    bar the forge seam was held to before it earned its keep.
  - **The base ref is config-driven.** *Done; the default still assumes a fork.*
    `fetch_upstream` (`src/git.rs`) splits `remote/branch` out of `upstream_ref`
    instead of the old hardcoded `git fetch upstream develop`: a `HEAD` branch
    fetches the whole remote to keep its symref fresh, a named branch fetches just
    that. The mechanism is portable; the *default* is the fork layout
    (`upstream/develop` + remote `upstream`), edited in settings for a repo that
    merges to its origin's default branch. (A rarely-hit `parse_pr` baseRefName
    fallback still says `develop`; out-of-scope — GitHub reliably supplies it.)
  - The folder picker already exists (`desktop/src/main.rs`, shown when
    `Config::existing()` is `None`), so first-run has a start; what it does not
    have is the rest of the questions.

- **The update nudge now authenticates.** *Fixed.* `latest_release`
  (`src/forge/github.rs`) takes an optional bearer token and
  `start_update_poller` (`lib.rs`) resolves one off-thread per poll through the
  same ladder the PR poller uses (`ORCHD_GITHUB_TOKEN`, the token file, then `gh
  auth token`), so the private release repo answers instead of 404ing to `None`.
  Two things unchanged and worth remembering: a failed check and "no update" are
  still the same answer (the nudge stays silent on error by design), and the
  debug gate means `cargo run` never nags — the nudge only fires from a release
  build. Superseded whenever the release repo goes public.

- **Is it macOS-compatible? Now partly answered — and the answer was "no, twice".**
  The plan here was "add a macOS job to the matrix", and doing that first would
  have shipped a broken app: both bugs **compile cleanly and fail at runtime**, so
  no amount of CI would have caught them. Found by reading for Linux-isms instead.
  - **`pty::pid_alive` stat'd `/proc/<pid>`**, which does not exist on macOS, so it
    returned `false` for everything. It backs teardown's "no live session" check
    and **fails open** — on a Mac you could delete a worktree with a live agent in
    it, the exact thing its comment says it prevents. Now `kill(pid, 0)`, the POSIX
    spelling, with `EPERM` counted as alive (a pid you may not signal is still
    running). New dependency: `libc`.
  - **`instance::holder` read `/proc/<pid>/cmdline`**, so on macOS every lock file
    read as stale and **a second daemon would start beside a running one** —
    defeating the whole one-instance invariant. Now `ps -p <pid> -o command=`,
    which both platforms answer.
  - **`headroom::available_mb` reads `/proc/meminfo` and is left alone**: it is
    documented to return `None` when it cannot read, and every caller treats that
    as "no opinion" and allows the spawn. So the headroom guard is simply *off* on
    macOS, by design rather than by accident.
  - **The review queue shelled out to `timeout`**, GNU coreutils, absent on a Mac.
    It failed at the *spawn*, so the pane read "running `mise run reviews --json`:
    No such file or directory" and blamed the review command for a binary it never
    named. The deadline is enforced in Rust now (`reviews::run_bounded`) and is
    stricter than what it replaced: the child gets its own process group and the
    **group** is signalled, where `timeout` only ever signalled the direct child —
    so a review script shelling out to `gh` no longer leaves those behind, which
    is the orphan the next poll would have raced. Both pipes are drained on
    threads, because a child that fills one would never reach the deadline.

  - **State lives where the platform keeps it.** `config_dir` is
    `~/Library/Application Support/orchd` on macOS and `~/.config/orchd` elsewhere
    (`default_config_dir`, injected `home` so it is testable without mutating the
    process's `HOME`). Nothing is migrated because there is nothing to migrate —
    the app has never run on a Mac, so no `~/.config/orchd` exists there.
    `ORCHD_CONFIG_DIR` still overrides both.

    That move carried a trap worth remembering: **the macOS path has a space in
    it**, and the push guard's hook is a *shell string* (`type: "command"`, which
    is why the `SessionStart` one can use a pipe and `|| true`). Unquoted, the
    space splits the path and the hook runs a command that is not there — the
    guard fails open and silently stops existing, the one thing §8 says it must
    not do. `hooks::sh_quote` single-quotes it, apostrophes included, and the
    tests prove it through a real `sh` in a directory with a space — with the
    unquoted form asserted to fail, or the test would prove nothing.

  - **The keyboard map was Ctrl-only, so ⌘ did nothing on a Mac.** The whole app
    layer was gated `e.ctrlKey && !e.metaKey`; only save and send-batch accepted
    Meta, and terminal copy/paste was `Ctrl+Shift` alone where macOS is ⌘C/⌘V. The
    modifier is now `core.appMod` — ⌘ on macOS, Ctrl elsewhere — from a platform
    flag the daemon substitutes into the served page (`__ORCH_PLATFORM__`, told
    rather than sniffed, since `navigator.platform` is deprecated and lies under a
    webview). The nice part: **on a Mac ⌘ never reaches the pty**, so the whole
    Ctrl-shadows-readline tension is Linux-only — `⌘N` costs nothing where
    `Ctrl+N` costs next-history, and Ctrl stays entirely the terminal's there.
    One documented exception: session switching is `Ctrl+Tab` on *both*, because
    ⌘Tab is the macOS application switcher and the OS takes it before any app sees
    it. The legend renders from `MOD` placeholders resolved at boot, so there is
    one map rather than two to keep in step, and both branches were driven with the
    platform forced.
  - **Symlinked checkouts resolve once, at parse.** `PostToolUse` runs the edited
    path through `canonicalize` before attributing it (so a `.plan/` symlink lands
    in the right pane), but `workspace_for_path` compared that against workspace
    roots that were *not* resolved — only the `--main` argument ever was, so a
    checkout named in `config.json` was not. A resolved path against an unresolved
    root matches nothing, so the edit was attributed to no workspace and never
    reached the changed-files pane. `parse` now resolves `main_checkout` once, so
    `worktrees_dir`/`worktree_path` inherit it, and the agent-reported cwd that
    adopts a delegated worktree is resolved at the same boundary. A path that does
    not resolve is left as written, because that is `validate`'s complaint to make.
    Latent on Linux; ordinary on macOS, where `/tmp`, `/var` and `$TMPDIR` are all
    symlinks into `/private`. There was no test for `workspace_for_path` at all;
    the new one was checked against the unfixed code and does fail without it.

  A sweep for the same class of thing found nothing else: every other external
  command is POSIX (`git`, `curl`, `gh`, `ps`, `which`, `kill <pid>` for SIGTERM),
  and the only hardcoded absolute path is a `/bin/bash` fallback for an unset
  `$SHELL`. Two things left alone deliberately: APFS is case-insensitive by
  default, so two worktrees differing only in case would collide — theoretical, and
  the daemon already refuses a duplicate name; and `ps -o command=` can truncate to
  terminal width on macOS, which does not matter while the check is a `contains`
  against a binary path that appears at the front.

  Shipping: `release.yml` is a matrix and builds `aarch64-macos` beside
  `x86_64-linux` (Apple Silicon only — every Mac since 2020; Intel doubles macOS
  minutes for machines nobody here has). Tauri uses the system WKWebView, so the
  apt step is Linux-gated and macOS installs nothing; `sha256sum` falls back to
  `shasum -a 256`. No `.app`, `.dmg` or code signing, because the release was
  already a bare-binary tarball and keeping that shape avoids Gatekeeper entirely.
  A new `check.yml` builds and tests both platforms on push and by hand, which
  exists because `release.yml` only fires on a tag — cutting a release was
  otherwise the only way to find out.

  **CI ran, went red on macOS, and was right to.** The first `check.yml` run had
  the Linux leg green and macOS failing two `git.rs` tests — both the same
  resolved-vs-unresolved path class as above, and one a **real bug in the
  stale-lock fix itself**: `stale_lock_pid` compared `git worktree list`'s output
  as a raw string, and git answers with the *real* path. Under macOS's `$TMPDIR`
  (which lives under `/var`, a symlink into `/private`) the compare missed, the
  lock was never seen as stale, and teardown refused forever — the exact bug that
  function exists to fix, back again on one platform. Both sides are resolved
  before comparing now. The second failure was a pre-existing test asserting
  git's output starts with `scratch_repo`'s path; the helper now hands back a
  resolved path, since that is what git will agree with.

  Two things learned about the workflow itself. `cargo test` ran *before* the app
  build, so the red tests **skipped** the Tauri build — hiding the one signal only
  CI can give; the build step is `if: always()` now. And because
  `scratch_repo` resolves, the stale-lock test no longer exercises the comparison,
  so there is a dedicated symlinked-path test — checked against the unfixed code in
  the failing direction, since reverting the other half left it passing happily.

  **What is still unproven, and cannot be proven from here.** The daemon crate and
  its tests cross-check clean for `aarch64-apple-darwin`, so the fixes above are
  verified at the type level. The *desktop* crate cannot be cross-checked at all:
  `objc2-exception-helper` compiles Objective-C (`try_catch.m`), which needs a real
  macOS toolchain — on Linux it dies in `cc-rs`, for want of an SDK rather than
  for want of working code — so only the runner can answer for it, and it now has:
  `check.yml` is **green on macos-14** (daemon tests *and* the Tauri build), and
  `v2026.8.11` shipped an `aarch64-macos` tarball beside the Linux one, the first
  release to carry one.

  What remains is the last mile and it cannot be closed from here: **nothing has
  been executed on a Mac.** `Chrome::Overlay`'s traffic lights, `open` for URLs
  and the window chrome are still written-not-run, and a binary that compiles is
  not a window that draws. The honest claim is now "it builds and its tests pass
  there", which is a good deal more than "nobody knows" — but launching it is
  somebody's afternoon with a Mac, not a CI job.

- **A README for other people.** *Rewritten.* The old one was for whoever already
  knew orchd — `§` references to a spec not then in the repo, one monorepo assumed
  throughout, sections in build order. The new one leads with what it is and a
  screenshot (`docs/screenshot.png`, the fixture board so no real repo leaks),
  then install for both platforms (the private-repo token, the macOS
  builds-but-never-launched caveat), a first run, and a table of what it assumes
  about your repo — the defaults are the author's, editable in settings, said
  plainly. Internals trimmed to a short "How it works"; the module map stays,
  since CLAUDE.md points here for it. Written for a stranger evaluating it, not an
  adopter wiring it to their own repo — if this ever needs an onboarding guide for
  the latter, that is a different, longer document.

- **Where the memory goes — looked at, and one of the two numbers was measuring
  the wrong thing.** *Done, one line changed.* Both suspects are settled:

  - **orchd is not 76 MB. It is 7.6 MB RSS / 5.3 MB PSS**, release build, idle,
    polling a PR — of which the *heap* (`RssAnon`) is **1.1 MB**. The old figure
    was a `cargo run` debug build, whose binary is 113 MB against release's 11 MB,
    and it is nearly all `RssFile`: paged-in debug text, not data. Same daemon
    built debug measured 15.3 MB RSS / 13.2 MB PSS idle with `RssAnon` still only
    1.7 MB. So the 76 MB was the cost of the *build profile*, and the shipped app
    never paid it. Both first suspects were wrong for the same reason — there is
    no arena to hold and no tail to blame when anonymous memory is one megabyte.
    Nothing to fix; measure release before believing a daemon number again.
  - **WebKit's scrollback was real, and it was a one-line win.** `xterm` keeps
    each line as a `Uint32Array` of `cols * 3` words, so depth costs process
    memory: at 40x140, one fully-scrolled terminal took **+36.7 MB RSS at 10000
    lines against +13.3 MB at 2000** (measured in Chrome over the vendored xterm;
    the curve is monotonic — 0/1000/2000/5000/10000 → 87.4/92.2/100.7/109.6/124.1
    MB of process-tree RSS). `web/js/term.js` is now `scrollback: 2000`, saving
    ~23 MB per terminal — and buffers are held whether or not a terminal paints,
    so parked drawer sessions were paying it too. The depth also was not durable:
    `BUFFER_BYTES` is a 512KB ring ≈ 3600 dense lines, so anything past that
    vanished on reload while still costing memory. The two are now in the same
    range on purpose.

  Measuring note for next time: JS-heap metrics are useless here. CDP's
  `JSHeapUsedSize` reported 0.9 MB for 9000 lines that cost ~23 MB, because
  typed-array backing stores are external memory. Process RSS, or nothing.

  Checked and dismissed: **`human_edits`** (`state.rs`) is never pruned, but an
  entry is a `PathBuf` + `SystemTime` + a `HashSet` of session ids — it would take
  hundreds of thousands of hand-edited files to reach a megabyte, so the unbounded
  map is theoretical, not a leak worth code. The **five 512KB ring buffers** are
  2.5 MB at worst, consistent with the 1.1 MB idle heap.

- **Audit the keyboard map for logical, consistent coverage.** *Done, and the
  scheme is now written down, and it is smaller than it was.* The audit found the
  map already implicitly layered but carrying **two vocabularies for one set of
  verbs**, so the fix was subtraction: the whole `Alt` layer is gone (`Alt+j/k`
  sessions, `Alt+b` blocked, `Alt+m` main, `Alt+d` diff), because every action it
  held had a `Ctrl` spelling doing the same job. What is left is a contract above
  the keydown handler (`web/app.js`) — **bare keys belong to the open overlay,
  `Ctrl` is the whole app, `Esc` dismisses the topmost thing** — so the next
  binding has a rule to obey rather than a precedent to copy. Do not reintroduce
  `Alt` to dodge a collision; `Ctrl+Shift` is the escape hatch.
  - Orphan actions bound: `Ctrl+N` new worktree, `Ctrl+Shift+N` new session on
    main, `Ctrl+Shift+T` (and `Ctrl+`` `) new shell, `Ctrl+Shift+D` the diff,
    `Ctrl+Space` the first session waiting on you, `Ctrl+Tab`/`Ctrl+Shift+Tab`
    session switch, `Ctrl+=`/`−`/`0` zoom — which was mouse-only.
  - List motion is one idiom: the diff overlay took bare `j/k` like the review
    overlay, `Ctrl+←/→` kept as an alias.
  - A legend (`?`, `web/index.html` `#keyhelp`) is the visible source of truth,
    because a scheme nobody can read is not predictable however consistent.

  Two properties of the `Ctrl` layer, recorded so they do not read later as
  oversights. Plain `Ctrl+<letter>` **shadows the pty**, so the default is
  `Ctrl+Shift+…` — the zone terminals do not send — and a plain letter is taken
  only where the idiom earns the key: the diff is `Ctrl+Shift+D` because `Ctrl+D`
  is EOF and still has to exit a shell, while `Ctrl+N` **is** taken, because
  "Ctrl+N is new" is worth more than what it costs. What it costs, stated plainly
  and decided rather than missed: `Ctrl+N` is readline's next-history and `Ctrl+P`
  is not bound, so a shell here walks history back but not forward. It was briefly
  moved to `Ctrl+Shift+N`/`M` to avoid that and moved back — the sole user does not
  use `Ctrl+N` at a prompt, and the muscle memory is worth more than the asymmetry.
  `Ctrl+Space` (NUL, emacs set-mark) is the other plain-Ctrl key that takes
  something. `Ctrl+N`, `Ctrl+Shift+N` and `Ctrl+Tab` are browser-reserved, so they
  only arrive in the desktop webview; the legend says so, and says the `Ctrl+N`
  trade too.

  **The real answer, when there is a second user: make the map rebindable.** Every
  binding lives in one `keydown` handler with a stated layer rule, so the shape is
  already right for it — a table of action → chord, a settings pane over it, and
  the legend rendered from the same table instead of hand-written HTML (which is
  the one thing today that can silently drift from the code). Not worth building
  for one user who can edit `app.js`.

  The legend itself moved off a bare `?` for the reason the contract already
  gave and the first implementation ignored: xterm's input is a `<textarea>`, so
  with a terminal focused — the normal state — the typing guard swallowed it and
  the list was unreachable by keyboard, while dropping the guard would have eaten
  a character you type. `Ctrl+Shift+?` needs no guard. Found by trying to
  screenshot it, not by reading the code.

  Driven in a real browser, both rounds: `?`/`Esc` toggle the legend, `Ctrl+=`/`0`
  zoom and reset, `Ctrl+N` hits `/api/worktree`, `Ctrl+Shift+D`/`Shift+T`/`Tab`/
  `Space` are claimed, plain `Ctrl+D` is left to the terminal, and all four
  removed `Alt` chords fall through. Nothing is left open here: the jump-to-a-PR
  key that fed this item is not wanted (see "Won't do"), and there is no longer
  any "take main" chord to want an inverse for. *The two `role="button"` keyboard
  traps once listed here are fixed:* a `keyActivate` helper gives the refresh
  icons and the update-nudge `×` a tab stop and Enter/Space.

- **Teardown auto-archives its own prerequisite.** *Fixed.* This was read as a
  dead route but is the opposite: `worktree::archive` (transcripts copied,
  recovery record written) is what satisfies preflight checks 4 and 5, those
  flags are set *only* inside it (`worktree.rs`; the `store.rs` sets are
  disk-load), and its one caller was a route the SPA never invoked — so teardown
  refused on any worktree that had held a session. `teardown` now runs `archive`
  itself when those two are the *only* failing checks and re-runs preflight; a
  live session, dirty tree, unpushed commits or an attached process still refuse
  and never see an archive run under them. The `POST …/archive` route stays as
  the explicit entry point.

  *Now driven live against the fixture*, which the disposable target finally
  allowed — and it found a real bug the compile could not. The archive half is
  exactly right: kill an exited session's worktree and teardown auto-archives
  (transcript copied into `config_dir/transcripts`, recovery record persisted,
  all six checks flip to pass). But the removal that follows *never worked* on the
  default layout: `claude --worktree` locks every worktree it cuts, and the lock
  outlives the session the daemon kills, so `git worktree remove` refused with
  "cannot remove a locked working tree" forever. `git::worktree_remove` now clears
  a lock whose owning pid is dead and retries — still a plain remove, so a dirty
  tree still refuses and nothing does a filesystem delete (`src/git.rs`, two new
  tests). Verified end to end: a stale-locked worktree that 400'd on the old
  binary tore down cleanly on the new one, gone from disk, git and the daemon.
  (Two adjacent observations, neither a bug: a fresh clone needs Claude Code's
  workspace trust accepted once or every `claude --worktree` spawn fails silently;
  and a *clean* `/quit` has claude remove its own worktree, which the daemon
  absorbs on reload as an archived, resumable session.)

- **Two genuinely dead routes — removed.** `GET /api/merge-base` (the
  changed-files pane computes the merge-base server-side) and `GET
  /api/pr/:number/threads` (the overlay uses `/review`, which shares the
  `fetch_threads` helper). Both handlers, their routes, and — found while
  verifying — the write-only `ThreadCache` map both `pr_threads` and
  `fetch_threads` fed (no reader anywhere; its doc comment claimed the post step
  used it, the post step reads `plan.threads` instead) are gone. Confirmed at
  runtime: both paths now hit the daemon's `{}` catch-all while `/review` still
  answers.

- **A failed upstream fetch no longer rebases silently onto a stale base.**
  *Fixed.* `rebase` (`src/api.rs`) captures the `fetch_upstream` result: on
  failure it logs and returns a `warning` in the response, which `act` in
  `web/app.js` now shows as a warning toast rather than the bare "rebased". It
  still proceeds — a rebase onto a known-old base is sometimes wanted — but it
  can no longer look identical to a clean one.

- **`pr_age_ms` is surfaced; `token_source` no longer is.** *Fixed, then half
  reverted.* The PR-pane header shows a live "· N ago" from `pr_age_ms` (ticked
  off the snapshot clock, so a stuck-but-not-erroring poller reads as stale).
  The `⚠` beside it for a `gh_cli` token is **gone**: `gh auth token` is the
  fallback that makes the app work out of the box, so the mark was permanent and
  could not be acted on without setting up a PAT — a warning you cannot clear is
  furniture. The fact is not lost. `token_source` is still in the snapshot for
  anyone diagnosing over the API, and the daemon's own live-findings block below
  reports it, which is a list you read deliberately rather than a badge you stop
  seeing.

- **`save_automation` failures are logged, not dropped.** *Fixed.* Both sites
  (`src/api.rs`, fix-pr run start and end) now `tracing::error!` on a failed
  write, naming the PR and that a restart may re-run past the cap, instead of
  `let _ =`.

## Decisions worth revisiting

- **`hooks::session_end` settles a session with no identity check, unlike the exit
  watcher.** Deferred, not overlooked. `watch_session_exit` may only settle a
  session whose pty is still its own — the guard that stops a relocated session's
  old watcher from marking the live replacement `Exited` and handing main's claim
  back out. The `SessionEnd` hook does the same two things (`set_state(Exited)`,
  `release_main`) with nothing of the sort, so an arriving hook from the process a
  relocation just killed would reach past `reclaim_main` and do it anyway.
  Unevidenced: nothing has been seen to fire `SessionEnd` on the way to a SIGKILL,
  and the e2e fake agent does not send one, so there is no reproduction to point
  at. If main is ever found live-but-unoccupied again, start here.

- **History keeps its AI attribution, and 14 commits still name the monorepo.**
  *Decided, not overlooked.* The working tree is clean of both, and author history
  was rewritten onto one identity — but 208 of 286 commit messages carry
  `Generated with Claude Code` / `Co-Authored-By: Claude` / `happy.engineering`
  trailers, 14 name the monorepo in subject or body, and a 226 KB
  `design/review-overlay.artifact.html` with 12 internal mentions survives in
  history alone. Scrubbing all three is one `filter-repo --message-callback` pass,
  and the cost is the same as the author rewrite: every SHA changes and the 13
  release tags need re-pushing. (Re-pushing them is free — tag *updates* do not
  retrigger the release workflow, only tag *creation* does, measured when the
  author rewrite moved all 13 and nothing built.) Judged not worth it; say so if
  that changes.

  Two things that would quietly undo the author rewrite:
  `../orchestrator-pre-rewrite.bundle` still holds the old commits, and another
  machine still has the pre-rewrite history with the personal address configured —
  commit `1092a8f` came from there.

- **The changed-files pane still refreshes.** The divergence strip now carries
  the thing worth acting on when a branch has fallen behind, but the list under
  it is recomputed on reconcile. It is no longer `git status`: it is the
  merge-base changeset plus untracked files (`state.rs`, documented in
  `app.js`). Freezing it was the other reading of "it shouldn't update"; a pane
  showing a tree that no longer exists seemed worse than one showing a long
  list. Say so if you want it pinned to a snapshot with an explicit refresh
  instead.

- **Session names come from an undocumented transcript field.** `store::ai_title`
  tails the `.jsonl` for `{"type":"ai-title","aiTitle":…}`, which is Claude Code's
  own format and can change under us. It degrades to the workspace name rather
  than failing, and the transcript slug rule has already been wrong once, so a
  rail that goes back to reading `dfafdf` everywhere is the symptom to look for.

- **No WebGL renderer in the desktop window.** Glyphs came back as garbage that a
  scroll or a selection cleaned up. Two narrower fixes did not hold: clearing the
  glyph atlas and refreshing after every refit, and disposing the addon on context
  loss. So the canvas is gone in the webview and xterm draws real text, which
  cannot garble; a browser tab keeps the fast path. The cost is the DOM renderer
  under heavy output, unmeasured. If it ever feels slow, the fast path is worth
  another look with only the *visible* terminal holding a context, since hidden
  ones can be torn down and replayed from the daemon's ring buffer for free.

- **Sessions archived before the rename still say `green`.** `Kind::Automation`
  carries the command as a free string, so records already in `sessions.json` keep
  the old name and their rows read `green` until they are deleted. Nothing
  switches on the value, so rewriting them on load would be churn for a label. The
  `green-<pr>` prompt directories under `~/.config/orchd` are dead files for the
  same reason.

- **`gh auth token` fallback.** Works out of the box and is what the daemon uses
  today, but its scopes include write and §6 wants read-only. Superseded as soon
  as a fine-grained PAT exists.
- **Two loosened spec rules.** The unpushed check counts commits beyond the base
  rather than blocking any never-pushed branch, and the transcript check
  distinguishes "nothing to copy" from "not copied yet". Both were unescapable
  as written. Revert if you disagree.
- **Dead shells close on a clean exit.** §2 says a dead shell keeps its buffer
  "until dismissed"; applied to every exit that made Ctrl+D leave a corpse. A
  non-zero exit still keeps its buffer.
- **The test-capability subsystem is gone.** orchd used to carry a `Suite` model
  (static/unit/integration/e2e), a composer autoload probe, lockfile-drift
  detection and per-suite trust/isolation — a whole `capability.rs` — so it could
  tell whether a command in a worktree reflected that worktree or silently main's.
  That question is a shared-stack artifact (a symlinked `vendor/`); every
  other repo ran it empty. Removed wholesale for open source. `fix-pr` keeps only
  the guards that protect the machine and the repo (authorship, one run per PR,
  branch-busy, the `MAX_AUTOMATION` cap). Two things go with it, both accepted:
  the pre-run trust gate (`fix-pr` is hand-triggered and watched, so a bad run is
  read, not swallowed), and the `main:instances` e2e lock — two concurrent fix
  runs that both reach e2e can now collide on a single shared instances dir. If that
  ever bites, set `MAX_AUTOMATION = 1` (serialize fix runs) rather than rebuild any
  of this.

## Won't do without a reason

- A jump-to-a-PR key. It was the one concrete gap left by the keyboard audit and
  is declined: PRs are picked by eye from a short list, so the chord would save a
  click you were going to aim anyway, and the audit's own finding was that the
  scheme wins by being *smaller*. Add it only if the PR pane ever grows long
  enough to scroll.
- Adopting shell-started sessions. The daemon spawns every session so that
  `$ORCH_SESSION_ID` correlation is exact (§2); adopting one would reintroduce
  the cwd/pid heuristics the spec rejects.
- A generic "run this command" endpoint (§12).
- Reworking the rail row (id into a tooltip, workspace onto the second line). The
  naming work that motivated it — `railName` showing the PR title or the
  conversation's ai-title — already made the row read well, and the 8-char id
  still earns its inline place as the one thing that tells apart two untitled
  sessions sharing a worktree (`web/app.js`). Not worth the churn.
- A global kill switch / pause (§8's guards table). Nothing automatic fires on
  its own here — `fix-pr` and every spawn are hand-triggered — so the switch that
  stops all of it is closing the app: the daemon owns every pty and takes them
  with it. A separate pause state would guard against a machine that is already
  not doing anything unbidden.

<!-- >>> orchd live findings >>> -->

## Live findings

Rewritten by the daemon on every poll. Edit anything outside this block.

Nothing outstanding.

<!-- <<< orchd live findings <<< -->
