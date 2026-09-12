# TODO

Hand-written, and it survives. The daemon's live findings used to be spliced into
this file, which churned it from every build; that feature is gone.

## Next

- **Split `orchd` into crates, bottom up. `docs/crate-split.md` has the sketch.**
  Measured: the module graph is already a clean nine-layer DAG once the runtime
  core is condensed, so four crates — `orchd-base`, `orchd-repo`, `orchd-run`,
  `orchd-serve` — are legal *today* with zero upward edges and no cycle to break
  first. `testutil` splits along the same lines, which is the sign the boundaries
  are right. What it buys is `cargo` enforcing what `tools/rust-modules.mjs`
  currently ratchets; what it does not buy is build time, and the doc says so with
  numbers. Do it one crate at a time or it is one unreviewable commit — the same
  objection that keeps the graph from being made a DAG in a single pass.

- **`edit::read` closes the symlink race on the final component only.** The parents
  are canonicalised earlier and can still be swapped between the check and the open;
  closing it properly needs `openat2` with `RESOLVE_BENEATH`, which is Linux-only
  and so wants a second path for macOS. The threat is narrower than the leaf's was:
  a symlink *committed* on a PR branch is caught by the parent check, so what is
  left needs a live process racing the open rather than content somebody pushed.
  The last of the four gaps the v2 review pass found.

- **`rerequest()` has never run.** The fixture drives everything else in the review
  flow (`mise run fixture`, `docs/fixture-pr.md`), but its threads are posted by
  `github-actions[bot]` and a bot cannot be a requested reviewer. That one button
  wants a second human identity: a throwaway account, or a fine-grained token for
  one.

- **The two-phase resolve flow — proven on a fixture, not yet on real work.**
  `docs/resolve-flow-plan.md` has the nine decisions behind it, and the three that
  landed differently once driven.
  Every phase has landed and a run has answered real reviewers: plan → session → a
  commit per thread → the real diff beside the drafted reply → the daemon posting on
  its own credential, with nothing pushed and no thread resolved until you press
  those buttons yourself.

  Getting there cost four bugs no test could have found, each now recorded where it
  will be read again — `is_ask_route` and "`is_resolved` can never mean handled" are
  CLAUDE.md entries, the rest are comments at the seams they broke.

  *What is still unproven.* `manual` mode has never executed. The story arm has
  never run (the fixture daemon has `tracker: none`). `rerequest()` cannot be
  verified without a second human identity. And every drive so far went through the
  API, so `rvRun`/`rvOverview` and the cards as the *overlay* draws them are
  type-checked but never opened in a browser.

  *The old batch stays, and its retirement bar was raised.* `/api/pr/:n/post` and
  the manual phase are still the secondary button. Retiring them would make every
  review answer cost an agent session, delete the proven path for the unproven one,
  lose a resumability the run does not have, and take ~1500 lines with no
  replacement for `patch.rs`'s apply ladder. A fairer bar: `manual` mode exercised,
  the overlay driven in a browser, and a run against a real monorepo PR. The only
  argument that did hold — two implementations of "the daemon answers a reviewer" —
  is gone: `post_outward` now goes through the same `with_story_id`,
  `send_reply_once`, `react_one` and `rerequest_all` a run uses.

  *Not to be confused with the beta gate below*, which is a separate decision and
  needs no deletion.

- **A resolve run should amend the PR's own commits, not append one per thread.**
  Wanted, and the decision already exists — it is what the *batch* does and what the
  run never learned. `review_commit::amend_target` blames the reviewed line, finds
  the commit that introduced it, and answers `Fixup(sha)` / `Head(reason)` /
  `OnTop(reason)`; the discriminator is **authorship, not publication**, so it
  refuses to rewrite somebody else's commit and shows the reason at every fallback.
  `git::fold_in` executes. The run uses none of it: `skills/resolve-run/SKILL.md` says
  "one commit per thread, nothing else in that commit", and `patch.rs`'s whole
  apply-and-fold ladder is dead on that path.

  Why the current shape is thinner than it looks: one-commit-per-thread exists only
  so the confirm card can show `commit_diff(sha)` beside the drafted reply — a *UI*
  need leaking into git history. It is prose, not a constraint; nothing enforces it;
  and if an agent commits two threads together and reports the same sha twice,
  `thread_committed` accepts it and posts both replies. Its real cost is the case it
  handles worst: two comments on one function usually want *one* coherent change,
  and splitting it leaves the first commit incoherent on its own.

  Force-with-lease needs no new decision — `src/guard.rs` already permits no other
  form, and refuses a push to the base branch.

  **Three consequences to settle before building it.**
  1. **The card's sha goes stale.** A `fixup!` is squashed later, so the sha the
     agent reports is not the one that survives. Showing the fixup's own diff is
     right — it is exactly the fix — but `PlannedThread::commit` then names a commit
     that no longer exists. The record wants the fixup *target*, or a re-resolve
     after the squash.
  2. **Amending outdates other threads.** GitHub anchors a thread to a commit and a
     line, so rewriting a commit a reviewer read can flip *their other* threads to
     outdated — answering A can make B and C stop pointing at real code. The
     append-only model cannot do that. This is a judgement about reviewers, not about
     git, and it is the real price.
  3. **The per-thread ancestry check would fire on every thread after the first.**
     `thread_committed` holds a reply when the plan's `base_sha` is no longer an
     ancestor of `HEAD` — which is exactly what an autosquash makes true. It would
     have to tell *our own* rewrite from somebody else's, the same provenance problem
     `Exhausted.at_head` already lost once.

  The shape that dodges (3) and keeps the cards honest: the agent still owns code and
  commits `--fixup <target>` where `amend_target` says `Fixup` — the daemon hands the
  target in the plan, since it already blames for the batch — and the squash happens
  **once at the end, before the push**, not per thread. One rewrite instead of N, so
  the ancestry check needs a single exemption rather than continuous forgiveness, and
  every card still shows a real standalone diff while you are approving it.

- **Sibling worktrees, for the agent that is not Claude Code.** Not wanted for
  Claude, which is the whole reason it is not built: `.claude/worktrees` is Claude
  Code's own `--worktree` location, so nesting is free and delegation works. Another
  tool will not put them there, and `docs/workspace-isolation.md` already names this
  as the condition for reopening the decision ("Reconsider if a non-Claude agent is
  ever hosted"). Sibling trees (`../feat`) are the wider convention, per the research
  in `docs/research/worktree-docker.md`.

  Today a tree outside `worktrees_dir` is not managed at all: `spawn::worktree_name_of`
  returns `None` and the daemon logs "ignoring worktree outside the managed dir". So a
  repo whose own `WorktreeCreate` hook puts trees beside the checkout gets a daemon
  that manages nothing, and says so once, in a log.

  **What it costs.** The config shape, since `worktrees_subdir` is sanitised to a
  relative in-main path and would have to admit an absolute one outside. The in-main
  guard several flows lean on. And a re-check of path attribution, though
  `workspace_for_path` is longest-match over absolute paths and should hold as it is.
  One thing gets *simpler*: main's `git status` would no longer contain the worktrees,
  so the porcelain exclude prefix and the managed block in `.git/info/exclude` stop
  being needed for that layout.

  **What it loses.** The gitdir moves outside the checkout, which is what a future
  in-container mode would need inside the mount — devcontainers had to add
  `--mount-git-worktree-common-dir` for exactly this. So this decision and the
  container entry below pull in opposite directions, and whichever is built first
  should say so.

- **Containers and ports, if orchd ever hosts a heavier repo.**
  `docs/workspace-isolation.md` has the decision record and the shape to build,
  from a sourced research pass (`docs/research/worktree-docker.md`): per-worktree
  compose projects (`COMPOSE_PROJECT_NAME`, ports from a pool), **not** the
  shared-stack `docker exec` model that was cut with the capability subsystem. The
  sibling problem is a per-worktree process publishing a fixed port; the peer answer
  is a host port range plus a `$PORT` placeholder, and `ORCHD_PORT_BASE` — already
  used per fix-pr run — is the hook. Neither is wanted yet: orchd carries no
  container config at all, and that is the portable default.

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

  What is left is deliberate rather than unfinished:
  - **The review queue needs a script.** Reverted to `reviews_command` on purpose: a
    built-in GraphQL queue with config-driven ranking was built, worked, and was
    more machinery than the one real user wanted to own (`docs/reviews-json.md`).
    The accepted cost is that a fresh checkout gets **no** queue until it configures
    one, and the pane reads `off`. Revisit only if a second consumer wants a queue
    without a script.
  - **Worktree *creation* is decoupled; the session model is not.** The daemon cuts
    every tree itself now — `spawn_worktree_session` runs the repo's own
    `WorktreeCreate` through `create_worktree` and adopts it, with no `--worktree`
    arm left. But the session still spawns `claude`, and the real coupling is
    untouched: `--session-id` correlation, the transcript slug, the `ai-title`
    field, `--resume`, and the whole hook-observer plumbing. Hosting another agent
    means abstracting *that*.
  - **Give the tracker the same seam the forge has.** A tracker is now three
    config fields (`config::Tracker`) rather than four constants in an enum arm, so
    the naming half is done; what is left is that reaching it is still spread
    through `story.rs` — the allowlist, the MCP entry's variable, and the URL rule
    in `StoryRef::consistent`. Mirror
    `ForgeImpl`: a `Tracker` trait plus enum-dispatch keyed on `config.tracker`,
    holding the MCP id and tool allowlist, the token env/file, the story-URL
    grammar, and a tracker-agnostic `Story` beside it. Two things to settle while
    doing it — the token ladder is Shortcut-named, and `Stub` should become the
    trait's test double rather than the `--strict-mcp-config` special case it is.
    Not worth building until a second tracker is actually wanted, the same bar the
    forge seam was held to.
  - **Two GitHub-shaped leaks** for a real second forge: `ThreadRoot`'s `comment_id`
    is a REST id, and both `GitHubForge::detect`'s URL parsing and the read-token
    ladder are github.com-specific — `for_kind`'s single `token` argument does not
    yet model per-forge credentials.
- **macOS: launched now, and mostly working.** A second person ran it on a Mac on
  2026-09-01, which closed the "never executed" half of this. What that afternoon
  found, all fixed: an app started from Finder inherits none of your shell's `PATH`,
  so `gh`, `node` and `claude` were all missing at once; sessions stuck at
  `starting` (a hook arriving before the record was inserted); a `⌃` drawn where the
  modifier is `⌘`; and no Finder entry at all from a mise install.

  What is still unanswered there:
  - **Chrome::Overlay's traffic lights and `open` for URLs** are written-not-run.
  - The desktop crate still cannot be cross-checked from Linux
    (`objc2-exception-helper` wants a real SDK); `check.yml` on macos-14 is the
    only answer, and it now runs that crate's tests as well as building it.

- **Drag and drop in the rail: sort sessions, and swap two by dropping one on the
  other.** The drawer's tabs got this (`startTabDrag` in `web/app.js`, order in
  `localStorage` per workspace), and the rail is the place it would earn more —
  the rail sorts itself by what needs you, which is right for triage and wrong
  when you are working through a list in an order only you know. Two gestures, not
  one: dropping *between* rows reorders, dropping *onto* a row swaps their
  branches, which is `swap-main` generalised to any pair of worktrees and needs a
  daemon route that does not exist yet. The tab drag is the pattern to copy —
  pointer events rather than HTML5 drag-and-drop, a 4px threshold, and the render
  suppressed mid-drag so a snapshot cannot rebuild the list under the pointer.

- **Own the tracker's transport instead of borrowing the target repo's MCP.**
  `Tracker::mcp_server()` names a server orchd expects to find in *the repo's*
  `.mcp.json` — `hooks::write_settings` approves it through
  `enabledMcpjsonServers`, `--allowedTools mcp__<name>` scopes the run to it, and
  the daemon pushes the credential in under the variable `token_env()` names. So a
  feature of orchd only works where somebody else happened to configure a server
  with the right name, over a transport we do not control (one repo's is `http` to
  `mcp.shortcut.com`), and the failure lands mid-run on a thread rather than at
  startup: the daemon warns about a missing *token* and says nothing about a
  missing or renamed *server*. The interactive `/resolve` story step has the same
  dependency, spelled `mcp__shortcut__*` in prose.

  The mechanism to fix it is already here and used for one case only: a tracker
  with `stub: true` passes `--mcp-config` plus `--strict-mcp-config`, which ignores
  every configured server. Doing that for the live tracker too is the small version
  — the agent and MCP shape stay, the repo dependency goes. The larger version is
  to call the tracker's API from Rust and drop the agent from filing altogether;
  search and create are two calls, and the agent is only in that path for the
  routing rules the repo's tracker skill holds, which would then need another home.
  Either way `Tracker` starts owning *how it is reached* rather than only naming a
  server somebody else configured.

  What has landed since this was written: `Tracker` is three config fields rather
  than an enum arm of four constants, so another tracker is a config edit; and no
  skill names a tracker tool any more, which was the other half this entry did not
  mention. The dependency on the repo's `.mcp.json` is untouched, and so is the
  failure landing mid-run rather than at startup.

- **The review pane is still `[beta]`, and the label is the honest part.**
  *Two of the three steps are done:* the old non-beta `/resolve` is gone, and the
  triage pass is a skill the rail starts and the bar reports on. What is left is
  running it against a real PR once, and then promoting the overlay. Nothing in
  the new path has made a round trip to GitHub yet. What is left is the
  list of what is actually wrong before anything is touched — collect that from a
  real session rather than guessing. Two gaps already known from the code:
  `is_resolved` is never set by the daemon (`github_write` will not resolve a
  thread, by design), so nothing in the UI can mean "handled"; and the resolve run
  itself has never made a real round trip to GitHub — the suite is unit tests and
  a fixture, which `docs/fixture-pr.md` says out loud.

  *The first real drive found four things and all four are fixed* — the reading
  screen, one click instead of two, a terse read asked for in
  `skills/review/SKILL.md` rather than folded away in the UI, and the card as
  one flat list. The one worth remembering: shortening a thing at its source beats
  hiding it at the end.

- **`Ctrl+Shift+Tab` for the previous session — implemented, wants one real-window
  check.** The diagnosis held: the SPA's `Tab && ctrlKey` arm handles both
  directions, and the one consumer left was WebKitGTK, whose focus chain claims the
  backward chord before the page sees it. `desktop::wire_session_switch_keys`
  intercepts it at the gtk window's `key-press-event` (before focus traversal),
  where Shift+Tab arrives as the `ISO_Left_Tab` keyval, and re-injects the DOM
  keydown the keymap already understands, returning `Propagation::Stop` so GTK does
  not also move focus. The forward chord is left alone because it already works. The
  re-inject payload is verified against the real handler (a synthetic
  `Ctrl+Shift+Tab` moves the selection), but the GTK grab-bypass itself has only
  been reasoned about — it needs the real window and a keyboard to confirm.

- **One credential, and it stops being `gh`'s.** Reads already go out over curl with
  a resolved token (`forge/github.rs`); only three places shell `gh` at all:
  `gh auth token` for the credential (`forge/github.rs:60`), every write
  (`forge/github_write.rs:156`), and the ejected `reviews.js`, which is the user's
  own file. So the plan is to move the writes onto the same curl transport, keep
  `gh auth token` as *discovery* when gh happens to be installed, and prompt for a
  token when it is not. A GitHub OAuth flow is the later shape.

  **What it costs, said up front.** Today's read path is documented as wanting a
  read-only PAT precisely because the writes borrow gh's wider credential
  (`forge/github.rs:14-18`). One transport means one token carrying write scopes, so
  "the daemon never pushes and needs read only" stops being true, and the boot
  warning that treats `TokenSource::GhCli` as too wide loses its subject. The README
  describes today's split rather than this plan.

- **Give the declared watch its `stop_command`.** Both halves that existed are done:
  `stop_command` on `ManagedSpec` (a command that stops what the pty is a *client*
  of, run before the kill, on close, restart and shutdown), and the watch is now
  declared in `main_processes` and autostarts. Its `stop_command` is still empty,
  which is the whole point of the item: confirm on the box that starting and
  stopping it twice leaves no watcher behind.

  *The compose-service alternative was rejected, and the reasoning is worth keeping.*
  Moving the watch to its own compose service and following `docker compose logs -f`
  costs two things the pty gives for free: `logs -f` replays history, so the health
  parser would open on yesterday's failures, and "the child exited" would become
  "the log follower exited", which says nothing about the service. It does not even
  avoid the problem — stopping a service is still not killing a pty — so it needed a
  stop mechanism too.

- **`canonicalize(p).unwrap_or_else(|_| p.into())` still has five inline copies**,
  in `state.rs` (twice), `git.rs`, `config.rs` and `main.rs`. `hooks.rs` had three of
  the eight and now has one `resolved()` that all of it goes through, which is what
  turned a macOS-only fault into a test that runs everywhere: `session_start`
  recorded the reported cwd raw while `session_end` resolved the one it compared, so
  on a Mac a session's own ending read as a hook from a tree it had left. The
  remaining five are in modules that would have to import each other to share one
  helper, so the real home is a `util` the tree does not have yet. Worth doing the
  day a sixth appears, not before.

- **Archived rows still pile up, and that half is deliberately not automated.**
  The trees are handled, in two places rather than one. `worktree_retention_days`
  (default 60, `0` off, editable in settings) removes the worktree of a conversation
  nobody came back to, hourly, and also one that **no conversation points at at
  all**. Age is the transcript's last write (`store::last_used`), not the session's
  start, so a conversation kept open for weeks is not old the day after you stop; an
  orphaned tree has no transcript to read and is dated by its own directory. And `spawn::watch_session_exit` now removes the tree of a
  turnless session as it forgets its row, which is where that population came from:
  32 of 61 trees on this machine were rows the daemon had deleted and trees it had
  left. Everything goes through the same six-check preflight the button uses.

  Two measurements worth keeping. The per-worktree index mtime looks like the better
  "last used" signal and is worthless: the daemon's own reconcile runs `git status`
  in every tree and refreshes it, 0.0 days for all 32, while the directories read 8
  to 19 days. And the churn here is about **three trees a day at ~230 MB each**, so
  retention is a disk budget: 60 days is roughly 190 trees and 44 GB, 14 days is
  roughly 45 and 10 GB. Nothing on this machine is older than 19 days, so the
  60-day default reaps nothing for the first two months and then holds a steady
  state. Lower it if the budget is the point.

  The **records** were left out on purpose, and the reasoning is the thing to keep:
  a tree is rebuildable (`revive`, at the same absolute path, from the recovery
  record the preflight insists on) so a wrong retention costs one rebuild, while
  `forget_session` deletes the daemon's archived transcript and cannot be undone. A
  timer may take the reversible half only. An orphaned tree is the exception that
  proves it: nothing can resume it and `git worktree remove` leaves its branch, so
  there is nothing to lose in the first place.

  So the pile of rows is still there, and the honest answer to it is presentation
  rather than deletion: group the archive by week and put the PR number on the row,
  which is the entry below. Revisit automatic record deletion only if that is not
  enough, and if it ever happens, `TranscriptOnly` rows are the only ones with an
  argument.

- **The archive is a list you cannot find anything in.** 91 rows behind one caret,
  each carrying a name and an age, with no search, no grouping by date and no PR
  number. `archivedRow` says out loud that this is the list you scan weeks later,
  and scanning is the one thing it does not support. Group by week, and put the PR
  number on the row where there is one.

- **The main checkout is one repo's constraint, drawn as a universal concept.** The
  reason it is privileged is real and outlives this repo: one checkout hosts one
  docker stack, one dev URL, one database, so the tree that cannot be duplicated
  cannot be handed to two agents. But that is a *per-repo* fact, and the UI states
  it five times whether or not the repo in front of it has a stack: a rail group
  with its own header and `+`, a chord (`MOD Shift N`), a PR menu item, a drawer
  badge, and three menu labels for one action (`move to main`, `swap branch with
  main`, `move out of main`) chosen by git state the reader cannot see. On a repo
  with no `main_processes` the whole group is chrome around a checkout whose only
  distinction is having no worktree.

  The shape worth considering: keep the lock and the exclusivity, drop the category.
  One list of workspaces with main pinned and a lock glyph, its reason read from
  `workspace_notes.main`, and one verb for the move whose confirm says what happens
  to main's current branch. Fix the vocabulary in the same pass:
  `docs/workspace-isolation.md` says never bare "main", and the menu items and every
  toast say exactly that.

- **Six places the app still assumes this monorepo, all in what you see rather than
  in what it does.** The config is agnostic and "make it run somewhere other than
  this machine" above closed the mechanical half. What is left is presentation, and
  each of these reads as a fault on a repo that simply is not shaped like this one.
  - **The drawer's stack badge is docker, hardcoded.** `stack_running` polls `docker
    compose ps` and `renderDrawer` draws `stack up` / `stack down` on every
    workspace, so a repo with no compose file gets a permanent red dot. It also
    contradicts `docs/workspace-isolation.md`, which records that orchd carries no
    container config at all and calls that the portable default. Managed-process
    health already comes from `ok_patterns`; read it from there, or draw nothing.
  - **The vendored skills name this repo's task runner.** `mise run
    pre-commit:run` is in `skills/fix-pr/SKILL.md` and its neighbours, hedged as
    "where it exists", which is a file guessing at a repo. A `checks_command`
    setting is the fix — and a skill cannot interpolate one, so it arrives the way
    every other run value now does: the environment, or the context route.
  - **Boot warnings never reach the window.** `machine::check` finds a missing
    `gh`, `node` or `claude`, and a tracker whose MCP server the repo does not
    declare, and every one becomes a single `tracing::warn!` in `lib.rs`. `Warning`
    is not in the snapshot at all, so a new user reads `unavailable` and `off` with
    the cause only in a log they do not have open. That is the case the module's own
    docs say it exists for.
  - **Half the settings have no field.** `env_source`, `workspace_notes`,
    `worktree_init` and `shared_worktree_paths` are config-file only. Fine for the
    operational ones, wrong for `workspace_notes` and `worktree_init`, which are two
    of the few things a *new* repo has to say. `allow_several_in_main` was in this
    list and now has a checkbox under Sessions. The Worktree setup help text also
    explains itself in terms of Claude Code's `WorktreeCreate` hook, which is a
    sentence about this repo.

- **A repo with nothing configured still pays for every pane.** No reviews, no
  processes, no compose file, and the frame still draws `REVIEW QUEUE off`, `stack
  down` and a `Processes + Shell` bar. Each label is honest on its own and the sum
  of them is a window that looks broken on a fresh install. Collapse a pane whose
  feature is unconfigured rather than labelling it.

- **The guard rule is not one rule.** The boxes themselves are done: nothing in the
  SPA calls `window.confirm` or `window.prompt` any more, `core.js` draws
  `confirmBox`/`promptBox`, and naming a worktree edits the row in place
  (`renameSession`). What is left is which actions get a guard at all. A swap asks,
  `open in main checkout` moves main's branch without asking, and `fix` starts a run
  that force-pushes without asking, which CLAUDE.md already notes is easy to fire by
  accident. Either the gate is "it changes a checkout or it pushes", or there is no
  gate.

- **Two review verbs on every PR row until the beta gate closes.** `resolve` and
  `resolve in UI [beta]` sit next to each other in `prMenu`, which asks the reader
  to pick between two implementations of one intent. The gate is the overlay entry
  above and is blocked on a real drive; until it closes, the beta item could sit
  behind a setting rather than in the menu everybody uses.

