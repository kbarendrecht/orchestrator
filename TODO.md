# TODO

Hand-written, and it survives. The daemon's live findings used to be spliced into
this file, which churned it from every build; that feature is gone.

## Next

- **The review session is the contender, and the pane is what works.**
  The batch flow — a headless triage pass, cards, then a resolve run carrying out
  what the cards decided — is deleted. It was the third of three flows over one
  review, it had never answered a real reviewer through the UI, and it was what
  made the overlay six screens deep. What it took with it: `skills/triage`,
  `skills/resolve-run`, the manual phase, the plan and run records, both their
  stores, and `patch.rs`'s apply-and-fold ladder, which only the batch used.

  **The fold machinery went too, and that was a decision rather than a sweep.**
  `review_commit` — `amend_target`, which commit owns a reviewed line and may it be
  rewritten — plus `git::fold_in`, `pre_commit`, `blame_line`, `restore_paths`,
  `effective_email`, `authors_in`, `is_merge`, `rev_exists`, `is_ancestor` and
  `user_email`, with their fourteen tests. It looked worth keeping for the session
  flow, and it is not: both surviving flows hand the amending to the agent, in prose
  — `skills/handle-review/SKILL.md:67` and `skills/review/SKILL.md:198` both say
  amend into the commit that owns the change. That machinery was the batch's way of
  doing it *without* an agent, and there is no longer a caller that has no agent.
  Stage 4 posts a reply; it applies no patch. git remembers it if the question
  reopens.

  **The overlay is three screens now**: card → approval → report, with `reading`
  and `changing` as the two the session owns and you only watch. `intake`, the
  worktree `gate` and the before-tally overview are gone — all three were the
  batch's, and the rail's review verb starts the session itself. The gate went
  whole rather than as a screen: `POST /api/pr/:n/commit` and `/stash`, the `gate`
  field in the `/review` payload, `git::commit_all`, `git::stash` and
  `triage::gate_allowing_your_edits`. A dirty tree is still refused, at the one
  place that can act on it — `spawn_posting_run` bails with `Gate::say()` and the
  toast carries the sentence — instead of being fetched with a `git status` on
  every open for a screen in front of a flow that no longer starts there.

  Two things it was right about, kept: a card that waits for you rather than a
  timeout that posts on your behalf, and `Skip` as the absence of a decision rather
  than a stance of its own.

  What is left is two flows. `/orchd:handle-review` in a pane is the default and the
  one proven on real reviews. The review session is the same agent with the overlay
  in front of it, and the open question is whether card-then-approve beats reading
  the pane. It has to be driven on real work before the pane's button changes.

  **Phase 3 goes through the daemon now.** The skill posted with `gh api` and was
  asked to remember four rules while doing it; it makes one call per thread to
  `POST /api/session/:id/thread/:thread/reply` instead, and one call to
  `POST /api/session/:id/rerequest` after the last of them. The footer and the
  refuse-a-duplicate rule live in `post_one` where eleven tests hold them, and
  *which* reviewers are asked is derived from a fresh fetch rather than from what the
  session believes it posted — so a reply that did not go out holds its author back
  on its own. That gave `forge::rerequest` back the caller the batch took with it,
  and made the approval page's `re-request` rows the daemon's promise rather than
  prose in a skill.

  *Still unproven in the session flow.* The story arm has never run (the fixture
  daemon has `tracker: none`), and `session_rerequest` has not either, because the
  fixture's threads are posted by `github-actions[bot]` and a bot cannot be a
  requested reviewer. That is the fixture's identity, not the code.

  **And nothing in the UI can mean "handled".** `is_resolved` is never set by the
  daemon, because `github_write` will not resolve a thread by design — so a thread
  you have answered looks exactly like one you have not. Settle that in the same
  drive: either the overlay stops implying it, or resolving becomes a write the
  daemon is allowed to make.

- **Stacked-PR support.** Two halves. First, a context-menu `stack` action on a
  PR row that opens a session starting from that PR's code — a new branch based
  on the selected PR's head, its own worktree (cwd = main, via the existing
  `worktree-create`/`worktree-link` hooks), and an interactive session. This is
  the `/resolve` spawn machinery pointed at a *new* branch off a PR head rather
  than the PR's own branch. The stack is then detected for free: `link_stacks`
  (`crates/orchd-repo/src/forge/github.rs`) already matches `child.base_ref == parent.head_ref`. Second,
  a semi-automation in the spirit of `fix-pr` — a `/restack` (or `sync`) skill
  that keeps a stack in sync: when a base PR's head moves (amend/rebase), rebase
  the children onto it bottom-up and re-push, within the existing push guards
  (`--force-with-lease` only, never the base branch). Reuses the
  `PrAutomation` per-PR run model and the skill-spawn path; the bottom-up
  serialized ordering is the piece §8 described but never built. Two known
  wrinkles: the stack DAG is stored children-only (`Pr.children`), so a restack
  must derive the parent chain by inverting it — there is no `parent`/`base`
  pointer; and if it rides an agent session like `fix-pr`, the `git rebase
  --onto <new-parent-head> <old-parent-head> <child>` logic lives in the skill
  itself (a new `skills/restack/SKILL.md` plus its line in `skills::VENDORED`), so
  no new Rust git primitive is strictly required. The per-PR-keyed guards
  (`authorship`/`branch_busy`) would need a chain-aware variant.

- **Make it run somewhere other than this machine.** The hardcoded assumptions are
  gone: the six stack-specific settings are `#[serde(default)]` values editable in
  the settings panel, `worktrees_subdir` makes the layout configurable, `docker` and
  `ng-watch` are `autostart:false` specs a fresh checkout never starts, the base ref
  is split out of `upstream_ref`, and `default_language` fills the prompts'
  `{{LANGUAGE}}`. Paths, `/proc` reads and GNU coreutils were the other half, and
  those rules are in CLAUDE.md.

  One decision comes before any transport, and `get-bb/bb` is where it is visible
  (`docs/system-overview.md`, `docs/multiple-devices.md`): it separates **where the
  state lives** from **where the work runs** — one always-on server owning the
  database, daemons enrolled per machine, a machine chosen per thread. orchd's rule
  today is the opposite and deliberate: a checkout's daemon owns that checkout's
  state. Running work on a second machine settles that question first, because the
  answer decides whether the child daemon keeps its stores at all.

  What is left is three seams — the agent, the tracker and the forge:
  - **Worktree *creation* is decoupled; the session model is not.** The daemon cuts
    every tree itself now — `spawn_worktree_session` runs the repo's own
    `WorktreeCreate` through `create_worktree` and adopts it, with no `--worktree`
    arm left. But the session still spawns `claude`, and the real coupling is
    untouched: `--session-id` correlation, the transcript slug, the `ai-title`
    field, `--resume`, and the whole hook-observer plumbing. Hosting another agent
    means abstracting *that*.

    `get-bb/bb` has already cut this seam for three agents, and its
    `docs/provider-plugin-api.md` is a ready-made checklist of what the abstraction
    has to name: a **capability handshake** at session start (does this agent
    restore a session, fork one, enforce approvals, accept skills), **session
    verbs** (start, resume, fork, stop, archive, name) and **turn verbs** (start,
    and steer a turn already running). Two of those orchd has no verb for at all —
    fork, and steer as something other than typing into the pty — so the checklist
    is worth reading before the trait is written rather than after. The half that
    does **not** transfer is bb's typed recovery events (`authRequired`,
    `rateLimited`): bb parses them because it renders a timeline, and orchd shows
    the pty, where the person already reads that text. orchd matches no agent error
    string anywhere today, and importing the typing would buy a parser and nothing
    else.

    **`stablyai/orca` names the two pieces bb's checklist leaves out, and both are
    things orchd currently gets from Claude Code for free.**

    *Status has to arrive from somewhere, and hooks are Claude Code's answer, not
    every agent's.* Orca takes agent status down three paths that converge on one
    write (`docs/reference/agent-status-store.md`): hook HTTP posts, relay
    observations, and **an `OSC 9999` escape sequence parsed straight out of the
    pty**. That third one is the interesting one here — it needs nothing of the
    agent but that it print, and orchd is already reading every byte of that pty
    into a ring buffer. It is the fallback for an agent with no hook system at all.
    Two rules Orca states are worth taking with it: one store with one authority
    id, and **precedence decided at write time with provenance on the row**, not
    re-adjudicated by each reader.

    *Skills reach each agent differently, and `--plugin-dir` has no counterpart.*
    `docs/reference/agent-skill-provider-paths.md` gives Codex `$HOME/.agents/skills`
    plus `.agents/skills` at the repository root, a plain copy; Claude Code gets
    `.claude/skills` searched hierarchically from the launch directory upward, laid
    down as a relative symlink (a junction on Windows, a verified copy where
    neither works). `skills::VENDORED` and the one `--plugin-dir` flag are a Claude
    Code fact, so the seam has to be "put this skill where *this* agent looks",
    with the placement mechanism per agent — not one flag with the agent's name
    swapped.
  - **Give the tracker the same seam the forge has, and let it own its transport.**
    A tracker is now three config fields (`config::Tracker`) rather than four
    constants in an enum arm, so the naming half is done; what is left is that
    reaching it is still spread through `story.rs` — the allowlist, the MCP entry's
    variable, and the URL rule in `StoryRef::consistent`. Mirror `ForgeImpl`: a
    `Tracker` trait plus enum-dispatch keyed on `config.tracker`, holding the MCP
    id and tool allowlist, the token env/file, the story-URL grammar, and a
    tracker-agnostic `Story` beside it. Two things to settle while doing it — the
    token ladder is Shortcut-named, and `Stub` should become the trait's test
    double rather than the `--strict-mcp-config` special case it is.

    **The transport is the same item, which is why it is one entry and not two.**
    `Tracker::mcp_server()` names a server orchd expects to find in *the repo's*
    `.mcp.json` — `hooks::write_settings` approves it through
    `enabledMcpjsonServers`, `--allowedTools mcp__<name>` scopes the run to it, and
    the daemon pushes the credential in under the variable `token_env()` names. So
    a feature of orchd works only where somebody else happened to configure a
    server with the right name, over a transport we do not control (one repo's is
    `http` to `mcp.shortcut.com`), and the failure lands mid-run on a thread rather
    than at startup: the daemon warns about a missing *token* and says nothing
    about a missing or renamed *server*. **The skill half moved rather than went
    away**, which is worth knowing before this is built: `skills/story` used to
    spell `mcp__shortcut__*` itself and now deliberately names no tool, deferring
    to the repo's own tracker skill for the search tool, the team, the workflow
    state and the epic. So a repo has to supply two things rather than one — an MCP
    server under the name `Tracker::mcp_server()` expects, and a tracker skill of
    its own — and neither absence is checked at startup.

    **The small version is the one to build, and the mechanism is already here.**
    A tracker with `stub: true` passes `--mcp-config` plus `--strict-mcp-config`,
    which ignores every configured server; doing that for the live tracker too
    keeps the agent and the MCP shape and drops the repo dependency. The larger
    version is to call the tracker's API from Rust and drop the agent from filing
    — search and create are two calls, and the agent is in that path only for the
    routing rules the repo's tracker skill holds, which would then need another
    home. Either way `Tracker` starts owning *how it is reached* rather than only
    naming a server somebody else configured.

  - **Two GitHub-shaped leaks** for a real second forge: `ThreadRoot`'s `comment_id`
    is a REST id, and both `GitHubForge::detect`'s URL parsing and the read-token
    ladder are github.com-specific — `for_kind`'s single `token` argument does not
    yet model per-forge credentials.

- ~~**Drag and drop in the rail.**~~ **Done.** Dragging a session row reorders
  the list, the order is kept per checkout in `localStorage` (`core.sessionOrder`), a session the order has never seen falls
  where `byNewest` would have put it, and `sort by newest` in the row's menu puts
  it back. Driven in a real browser — three worktrees, a drag, a reload, the menu —
  and now gated in `tools/e2e/page.mjs`, which was already booting a browser and a
  sandbox daemon. The gate was checked by breaking it: dropping `sessionOrder` from
  the rail's paint signature fails both drag lines, dropping the `localStorage`
  write fails the reload line. The **mid-drag render guard** is the one part it
  cannot hold — a synthetic drag is over before a snapshot can rebuild the rail
  under the pointer — and `page.mjs` says so where the check is.

  One thing settled while doing the reorder: the rail uses **HTML5 drag-and-drop**,
  not the drawer's pointer maths. The rail is one column, so `dragover` answers the
  only question there is, and `checkoutHead` was already doing exactly that on the
  same rail. The pointer maths in `startTabDrag` earns its keep on a strip that is
  horizontal and scrolls.

- **One credential, and it stops being `gh`'s.** *`gh` must not be a requirement* —
  that is the reason, and it outranks what the change costs below. A person who
  installs orchd should need a token, not somebody else's CLI, and the review queue
  already proved the shape by dropping `gh` for a `curl` on the resolved token.

  Reads already go out over curl with
  a resolved token (`forge/github.rs`); only three places shell `gh` at all:
  `gh auth token` for the credential (`forge/github.rs:60`), every write
  (`forge/github_write.rs:156`). The review queue used to be a third and is not:
  it is a `curl` on the resolved token like every other read. So the plan is to
  move the writes onto the same curl transport, keep
  `gh auth token` as *discovery* when gh happens to be installed, and prompt for a
  token when it is not. A GitHub OAuth flow is the later shape.

  **What it costs, said up front.** Today's read path is documented as wanting a
  read-only PAT precisely because the writes borrow gh's wider credential
  (`forge/github.rs:14-18`). One transport means one token carrying write scopes, so
  "the daemon never pushes and needs read only" stops being true, and the boot
  warning that treats `TokenSource::GhCli` as too wide loses its subject. The README
  describes today's split rather than this plan.

  **Which is a cost to pay, not a reason to stop.** The read-only PAT is a real
  property and it is one orchd can keep offering: two tokens, a read one and a
  write one, is the shape that survives the change — the writes need their own
  credential whether it comes from `gh` or from a field in settings. What goes is
  the *accident* that the wide one is gh's, and with it the requirement that gh be
  installed at all.

- **The archive is a list you cannot find anything in.** 91 rows behind one caret,
  each carrying a name and an age, with no search, no grouping by date and no PR
  number. `archivedRow` says out loud that this is the list you scan weeks later,
  and scanning is the one thing it does not support. Group by week, and put the PR
  number on the row where there is one.

  **And the thing worth finding is not on the row at all.** Every archived session
  leaves its transcript at `<config dir>/transcripts/<session id>.jsonl`, so what
  the session actually *did* is already on disk and nothing reads it back.
  `stablyai/orca` indexes exactly that (`docs/reference/agent-session-search-contract.md`)
  and its contract is worth copying where it is cheap: index user and assistant
  text in full, **cap tool output** (it uses 3,072 characters a row) so a build log
  does not drown the index, filter by agent, path and date, and tie a pagination
  cursor to the query so a rebuild answers `stale-cursor` rather than a silently
  different page. Grouping by week makes 91 rows scannable; searching the
  transcripts makes them answerable — "which session touched the pty ring buffer"
  is the question people actually arrive with. The redaction half of Orca's
  contract does not apply: orchd's archive never leaves the machine.

- ~~**A repo with nothing configured still pays for every pane.**~~ **Done, and it
  found a snapshot that disagreed with the daemon.** A checkout with no forge drew
  a PR pane reading `unavailable` and a review queue reading `off` beside it — two
  headers, two counts, two refresh buttons, two carets, every label honest and the
  sum reading as a broken install. It is one quiet line now, `noForge` in
  `rail.js`, and the review block is not drawn at all. `stack down` went earlier,
  with the compose entry above.

  **`repos.upstream` is the signal, and fixing it was the real work.** `Repos` was
  built from the git remote alone while `resolve_repo` takes `cfg.repo` first, so a
  checkout that pins its repo in config polled that repo and reported no upstream —
  harmless while nothing read the field, and the moment the panes hid themselves on
  it they vanished from a checkout whose PRs were being fetched. Found exactly that
  way. `08-fix-pr` now asserts the snapshot names the repo the daemon polls.

  **The `Processes + Shell` bar stays**, and that is a decision rather than an
  oversight: `+ Shell` is a real action available on every checkout, configured or
  not. A bar offering something you can press is not chrome. What was chrome were
  the panes describing a feature that does not apply.

  `page-check` holds both halves, each checked by breaking it: drawing the PR pane
  anyway fails, drawing the queue anyway fails, and reverting the `repos` fix fails
  the e2e flow.

- ~~**The guard rule is not one rule.**~~ **Done, and the rule is: only destructive
  asks.** Destructive means work that cannot be got back. Four boxes are left and
  each is one — orchd's copy of a transcript, banked work git keeps no copy of, a
  file's uncommitted content, unsaved typing in the editor.

  **Three were removed, each verified reversible first.** Closing a checkout keeps
  every conversation and offers them on reopen; a swap or a move out of main is
  undone by moving back, and both carry their uncommitted work; the teardown
  preflight refuses a tree that is dirty, unpushed or occupied, and writes the
  record `revive` rebuilds from at the same path. None of them loses anything.

  **The two that never asked still do not**, and that is the same rule rather than
  an exception: `fix` force-pushes with `--force-with-lease` behind a guard that
  denies the base branch, and `open in main checkout` moves a branch. Loud, not
  lossy. What actually stops a wrong move is the daemon — the swap lock, the push
  guard, the preflight — and those work whether or not anybody read a box.

  Loudness is answered with a toast now instead of a question: closing a checkout
  says how many sessions stopped, the teardown says which checks passed, and the
  swap's in-flight toast finally matches the menu item you pressed.

  The rule is written at `confirmBox` in `core.js`, because **nothing static can
  tell destructive from loud**, and `page-check` holds it from both sides — checked
  by growing a box back on the swap and by taking one off the delete.

  **The four were then re-read against what the code actually does, and one claim
  was wrong.** Discarding banked work said "git keeps no copy"; `discard_wip` is
  `git update-ref -d`, which drops the ref and leaves the commit **dangling** until
  gc — so `git show` still has the work for about a fortnight, for anyone holding
  the sha. The daemon already knew the sha and put it in a `tracing::info!`, which
  is the same fault the boot warnings had: the recovery handle went somewhere a
  launcher-started app cannot show you. It is in the response and the toast now,
  asserted in `21-rebase` by checking the sha is a real commit object after the
  ref is gone. The box stays — it is what stops the sha being needed — and its
  wording says what git actually does.

  The other three hold as written. Deleting a session removes the row and
  `remove_file`s orchd's copy; Claude Code's own `.jsonl` survives, at a path
  slugged from a directory that no longer exists, which is not a recovery anyone
  will make. The two file discards are uncommitted content and unsaved typing.
  The two `chooseBox` calls are not guards at all — "Resume or start empty" is a
  real question only the person can answer.

  Two hardcoded branch names fell out of reading this: the fix button's tooltip
  said *"Rebase on develop"* on every repo, and the review header said *"conflicts
  with develop"*. The tooltip reads `upstream_ref`; the header reads the **PR's
  own** `base_ref`, now sent by the daemon, which is also the right answer for a
  stacked PR whose base is another PR's head.

- **Two review verbs on every PR row until the beta gate closes.** `resolve` and
  `resolve in UI [beta]` sit next to each other in `prMenu`, which asks the reader
  to pick between two implementations of one intent. The gate is the overlay entry
  above and is blocked on a real drive; until it closes, the beta item could sit
  behind a setting rather than in the menu everybody uses.

- **Record real agent screens, before there is a second agent to record.** orchd
  parses agent pty bytes in three places already — `agent_complaint` reads the ring
  buffer to turn a fast non-zero exit into a sentence (`spawn.rs:1767`),
  `is_interrupt` classifies keystrokes (`ws.rs:296`), and `health.rs` strips ANSI to
  reach a verdict — and every fixture behind them is a byte string **typed by hand
  into a test**. That is affordable for one agent whose screens are known. It stops
  being affordable at the second, and the roadmap has more.

  `stablyai/orca` built the harness for this and wrote down what it cost
  (`docs/reference/agent-pty-transcript-capture.md`). The shape:

  - A capture command spawns the agent **in a real pty** and records every byte —
    escape sequences and carriage returns included — while mirroring the session to
    your terminal so you drive it by hand. `Ctrl+]` ends the capture, chosen so it
    does not dismiss whatever dialog is on screen.
  - `--cols`/`--rows` pin the pty size, because **where the agent wraps is part of
    the evidence**. A fixture recorded at a different width tests a different
    string.
  - `--duration` for an unattended screen, `--send "<ms>:<text>"` to type at a
    timestamp, `--note` for the account type and CLI version.
  - The bytes land unchanged beside the tests, with a sibling `.meta.json` holding
    the timestamp, the platform, the command, the pty size and the exit code — so a
    fixture that stops matching can be dated and re-cut rather than argued about.
  - **Scrubbing is mandatory and same-length.** Tokens, hostnames, usernames,
    absolute paths and branch names come out, replaced by placeholders of the same
    width so the wrapping the fixture exists to prove survives the redaction.

  The gotcha Orca records is the one worth having in advance: their early fixtures
  were **pasted from a rendered terminal**, so they carry no escape bytes and no
  carriage returns. Those pass a word-match rule and prove nothing about a parser.
  orchd's hand-typed strings are the same class of fixture, and `agent_complaint`'s
  is the closest to real precisely because somebody typed the `\x1b[2J\x1b[H`
  prefix in by hand.

  Two orchd-specific notes for whoever builds it. The capture belongs beside the
  suite as a `mise` task, not as a test — it needs a human driving a real agent, so
  what CI gets is the recorded file, not the recording. And the platform matters
  the way `docs/traps/macos.md` says it does: a screen recorded on Linux is not
  evidence about the same agent under WebKitGTK's pty on a Mac.

- **Deferred: a restart of the daemon does not have to kill the terminals.**
  `mise run app-check` asserts that a session survives a restart, and it survives
  by being *resumed* — the pty dies with the daemon, `spawn::Carried` rebuilds the
  record, and `auto_resume` starts the agent again 1,200 ms apart. `stablyai/orca`
  does not resume, because nothing died (`docs/reference/orcad-operations.md`): the
  runtime forks a **separate terminal daemon**, detached, owning every local pty,
  with its own socket and its own pid file, and a new runtime **adopts the existing
  daemon** rather than replacing it. Their sentence for the rule is the useful
  part: *code freshness always defers to live work*.

  That is a real architectural difference and a large change, so it is recorded
  rather than proposed. What makes it worth recording is that orchd's update path
  wants exactly this property — an update today interrupts every running agent, and
  resume is the compensation. Two smaller things from the same document need no
  such change and may be worth taking on their own: orcad distinguishes **exit code
  78, a configuration fault, do not restart** from exit 1, retry with backoff —
  orchd's host restarts a dead child exactly once, and a config fault spends that
  one restart achieving nothing; and orcad bounds respawning at five launches in a
  rolling 60 seconds rather than trusting a single-shot rule.

- **Say which git the daemon needs, and stop exceeding it by accident.** There is
  no stated minimum git version, no `git --version` read and no capability probe,
  so a flag newer than the reader's git fails silently in whatever way that call
  site happens to fail. One was found by reading `stablyai/orca`'s
  `docs/reference/git-compatibility.md` and is fixed: `rebase_in_progress` asked
  for `rev-parse --path-format=absolute --git-dir` (git 2.31) inside a
  `let Ok(..) else { return false }`, so on Ubuntu 20.04's git 2.25 or Debian 11's
  2.30 every caller — `bank`, `triage`, `relocate`, `reconcile`, `rebase_onto` —
  would read "not rebasing" and act on a tree stopped mid-rebase. It says
  `--absolute-git-dir` now, which is git 2.13 and already the spelling in
  `git::head_file`.

  What is left is the statement and one open question. The oldest flag still in use
  is `switch`/`restore` (2.23), so **2.23 is the honest floor** and belongs in the
  README beside the other requirements. The open one is
  `git config core.fsmonitor true` in `configure_repo`: as a boolean that is git
  2.37, and older git reads `core.fsmonitor` as a *hook path*, so it would try to
  run a hook named `true` on every status. It is written `let _ =`, so nothing
  would say so. Worth answering on an old git before the floor is written down.

  Orca's own answer to the general problem is a `GitCapabilityCache` that probes
  behaviour per execution host and remembers the refusal, explicitly *not*
  branching on a parsed `git --version`. That is the right shape for a product
  running git over SSH and WSL. orchd runs git on one machine, so the cheap version
  is the whole version: name the floor, and do not exceed it.

- **Deferred: a worktree could be given files by copy, not only by symlink.** orchd
  puts a worktree's untracked files there by symlinking back to main, with
  `shared_worktree_paths` for links pointing out. `get-bb/bb` does the same job
  declaratively and with the other semantics (`docs/worktrees.md`): a
  `.worktreeinclude` file in gitignore syntax names untracked files to **copy** in,
  after the tree is cut and before setup runs, and an edit in the worktree then does
  not touch main. The two are not the same feature — a per-worktree `.env` cannot be
  a symlink — and orchd has no answer for that today except a `worktree_setup`
  script. Nothing has gone wrong, so this is recorded, not planned. It becomes real
  the first time a shared file has to differ per worktree.

- **Deferred: setup can fail and say nothing.** `worktree_init` and `worktree_setup`
  are non-fatal, the second runs even after the first failed, and `env_source`
  failures are silent by design — a degraded session beats a lost one, and that
  stays. What is missing is the *report*: a session whose setup did not finish then
  misbehaves with nothing on screen to explain it. `get-bb/bb` splits the two
  (`docs/environment-provisioning.md`): creation failure is terminal and loud, setup
  failure leaves a usable workspace and is shown as retryable, with no automatic
  retry ladder. That split is the part worth copying — the policy, not the state
  machine. Justified the first time a session's odd behaviour is traced back to a
  setup step that failed quietly.

