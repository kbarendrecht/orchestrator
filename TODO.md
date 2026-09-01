# TODO

Hand-written, and it survives. The daemon's live findings used to be spliced into
this file, which churned it from every build; they now go to a gitignored
`daemon.log` instead, and only when the repo being managed is this source tree.

## Next

- **A fixture PR to test the review flow against.** *Built and used heavily —
  `mise run fixture`, `tools/fixture-pr.mjs`, written up in `docs/fixture-pr.md`,
  which carries the why, the two GitHub behaviours that cost an afternoon, and what
  each drive against it settled.*

  Everything it was built to unblock has now been driven: the resolve run end to
  end, `triage::gate`'s dirty refusal, `open_file`'s head-sha arm, teardown and its
  archive, the thumbs-up idempotency question. What it still cannot cover is
  `rerequest()` — a bot cannot be a requested reviewer, so that one button wants a
  second human identity: a throwaway account, or a fine-grained token for one.

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
  `git::fold_in` executes. The run uses none of it: `commands/resolve-run.md` says
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

- **Promote the in-UI review overlay out of beta.** The overlay now does the
  real work: threads listed under the PR with their file, hunk, and a reply box,
  and replies/reactions/re-request go straight through the GitHub API
  (`src/forge/github_write.rs`). It ships as the `resolve in ui [beta]` menu item
  beside the old `/resolve`-into-a-terminal path. What remains is deciding when
  to make the overlay the default and retire the terminal spawn. Resolving a
  thread is deliberately *not* an API call — that is the author's button, by
  design (`forge/github_write.rs:10-13`) — so this item is about the beta gate, not
  the missing action.

  The gate now has one concrete condition rather than a feeling: **the session flow
  has to drive a real PR once.** The overlay is session-only since the flatten, so
  the beta label is carrying the fact that its change and post-go phases have only
  ever been exercised against canned data. Blocked on the fixture, which needs
  Actions minutes. `/resolve` stays whatever the answer is — it is the fallback, not
  the thing being replaced.

- **Rewind a session from the rail.** *Built: `Rewind conversation…` on the session
  row's context menu, `POST /api/session/:id/rewind`.* The daemon builds no picker —
  Claude Code has the whole feature, files included — so the button only reaches it,
  with two escapes into the pty. Measured rather than assumed, against a real
  session: **two escapes 60ms apart register as a double tap** and the picker opens.
  One `\x1b\x1b` burst was not risked, because a double *tap* is two timed events
  and a single write could arrive as one escape or as the `ESC ESC` meta prefix.

  Refused in the four states where an escape means something else — mid-turn it
  interrupts, at a question it cancels, at a permission prompt it declines, and with
  no conversation the picker opens empty. Gated in the daemon rather than the SPA,
  because the attach socket only exists while a terminal is open and a gate the SPA
  owns is advice. No reconcile afterwards: `start_workspace_watcher` already
  re-reads live workspaces every 15s, which is what it exists for.

  *Not built:* a modal of our own, listing the conversation's turns as rewind
  points. It only pays off if the native picker proves too coarse, and it would need
  a way to select a message index from outside the TUI, which the CLI does not
  expose — `--resume` resumes at the end and nothing else.

- **Containers and ports, if orchd ever hosts a heavier repo.**
  `docs/workspace-isolation.md` has the decision record and the shape to build,
  from a sourced research pass (`docs/research/worktree-docker.md`): per-worktree
  compose projects (`COMPOSE_PROJECT_NAME`, ports from a pool), **not** the
  shared-stack `docker exec` model that was cut with the capability subsystem. The
  sibling problem is a per-worktree process publishing a fixed port; the peer answer
  is a host port range plus a `$PORT` placeholder, and `ORCHD_PORT_BASE` — already
  used per fix-pr run — is the hook. Neither is wanted yet: orchd carries no
  container config at all, and that is the portable default.

- **Let `fix-pr` ask, and then decide what the concurrency cap is for.** One item
  because the second question only becomes interesting once the first is answered.

  **A fix run cannot ask you anything.** The ask channel is real and proven — the
  session POSTs `…/ask`, long-polls `…/ask/:ask/wait` in bounded loops, the card
  renders over the pty, and a `free: true` option opens a box so "let me write it"
  can. The resolve run uses it. `commands/fix-pr.md` does not mention it once, and
  `vendored_prompt_file` renders `{{ASK_BASE}}` for that template anyway — so the
  machinery is handed to the session and never used.

  What the prompt says instead, in five places, is **stop**: a conflict whose
  resolution is a judgement about behaviour, the same job failing twice, a fix that
  would change behaviour to go green, a rejected `--force-with-lease`. Every one is a
  question with two or three real options. But "stop and ask" means print it in the
  pane and end the turn — so the session lands at `your_turn`, `settle` records
  `Exhausted` ("the run gave up"), and you have to find the pane and read back what
  it wanted. The same information as a card would carry, except the run had to die to
  deliver it. Converting those five into asks is the change; the prompt is where it
  lives, not Rust.

  **`MAX_AUTOMATION = 2` is vestigial, and 2 is the wrong number either way.** It
  comes from two worlds that no longer exist: automation that fired on its own (§8's
  transition rules, deliberately unimplemented — every run is hand-triggered now, so
  passing 2 means pressing the button three times while watching), and the capability
  subsystem that tracked shared resources (deleted; the leftover is two runs
  colliding on a shared e2e instances dir, whose stated mitigation is
  `MAX_AUTOMATION = 1`). At 2 it is neither serialization nor resource protection —
  `headroom` does the latter at spawn, with real numbers. The only thing left that it
  bounds is a runaway API caller, and that route needs the app token, which agents
  deliberately do not hold. So: drop it and let `headroom` be the guard, or set it to
  1 and name the shared-resource case as the reason. Not a number in between.

  **Why they are one item.** A run blocked on a question holds its slot
  indefinitely — a human takes minutes, and the loop is what makes that safe. So the
  moment `fix-pr` can ask, the cap starts mattering *more*, for a new reason: two
  open questions would lock out every further run. Either the cap stops counting
  sessions that are waiting on you, or asking makes it worse than useless.

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
  prompt (a new `prompt::RESTACK` + `vendored_prompt_file` arm), so no new Rust
  git primitive is strictly required. The per-PR-keyed guards
  (`authorship`/`branch_busy`) would need a chain-aware variant.

- **Make it run somewhere other than this machine.** The hardcoded assumptions are
  gone: the six stack-specific settings are `#[serde(default)]` values editable in
  the settings panel, `worktrees_subdir` makes the layout configurable, `docker` and
  `ng-watch` are `autostart:false` specs a fresh checkout never starts, the base ref
  is split out of `upstream_ref`, and `output_language` fills the prompts'
  `{{LANGUAGE}}`. Paths, `/proc` reads and GNU coreutils were the other half, and
  those rules are in CLAUDE.md.

  What is left is deliberate rather than unfinished:
  - **The review queue needs a script.** Reverted to `reviews_command` on purpose: a
    built-in GraphQL queue with config-driven ranking was built, worked, and was
    more machinery than the one real user wanted to own (`docs/reviews-json.md`).
    The accepted cost is that a fresh checkout gets **no** queue until it configures
    one, and the pane reads `off`. Revisit only if a second consumer wants a queue
    without a script.
  - **Worktree *creation* is decoupled; the session model is not.** At Claude Code's
    default layout `spawn_worktree_session` delegates to `claude --worktree`; at any
    other `worktrees_subdir` the daemon cuts the tree itself. But both arms still
    spawn `claude`, and the real coupling is untouched: `--session-id` correlation,
    the transcript slug, the `ai-title` field, `--resume`, and the whole
    hook-observer plumbing. Hosting another agent means abstracting *that*.
  - **Give the tracker the same seam the forge has.** Shortcut is nominally behind a
    `Tracker` enum but not behind a trait, and its specifics are spread through
    `story.rs`: the MCP server name in the allowlist, the `SHORTCUT_API_TOKEN` the
    MCP entry reads, and `Story::url`'s knowledge of the URL scheme. Mirror
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
  - **First run asks one question.** The folder picker exists
    (`desktop/src/main.rs`, shown when `Config::existing()` is `None`); the rest of
    the questions do not. A probe that *suggests* managed processes from a compose
    file or a `package.json` script was scoped and deferred.

- **Cut every worktree with the daemon, including new interactive ones.** *Low
  priority — nothing is broken today, both paths work.* The point is not tidiness:
  `claude --worktree` is the one place the daemon asks the **agent** to do something
  only that agent can do, so collapsing it is the first real step towards being
  agent-agnostic. It is also the smaller half of the coupling — the session model is
  the rest, see "Worktree creation is decoupled" above.

  Mechanically it is already possible and already the exercised path: every PR
  worktree and every resume is cut with `git worktree add`. Only
  `spawn_worktree_session` delegates, and only when `worktrees_subdir` happens to be
  Claude Code's default — so today you cannot ask for daemon-cutting without moving
  your worktrees elsewhere, which is an unrelated decision. It wants an explicit
  mode, not a subdir inference.

  **From the outside nothing changes.** Same paths (`<main>/.claude/worktrees/<name>`),
  same branch names (`worktree-<name>`), same rail, same gestures.

  **What must keep working, and is the actual work:** the target repo's own
  `WorktreeCreate` hooks. Claude fires those *only* for `claude --worktree`, so a
  daemon-cut tree has to run them itself — read the repo's `WorktreeCreate` entries
  and invoke them, rather than leaning on `worktree_setup` and telling every repo to
  configure the same thing twice. Watch for the double-run: a repo with both a
  `WorktreeCreate` hook and a configured `worktree_setup` must not get both.

  **Trust stays Claude's**, checked when the session opens in the tree. What
  disappears is *creation* depending on it: today an unaccepted trust dialog makes
  `claude --worktree` refuse, the session exit instantly, and leaves a workspace
  record for a worktree that was never created.

  **`PENDING_WORKTREE` goes.** The daemon would know the path before the pty exists,
  so no `…creating` placeholder, no adopting the workspace from the agent-reported
  cwd, and no requirement to `canonicalize` at that boundary. The one thing lost is
  `--worktree` with no name inventing a collision-proof one; the daemon already has
  `wt-<8 hex>` for that.

  **Measure first, in an afternoon:** cut a worktree each way and diff what Claude
  Code writes into its own state. Locking the tree is the only difference anybody has
  confirmed, and that is what `worktree_remove`'s stale-lock retry exists for — which
  becomes deletable once no locked trees remain, but not before.

- **Is it macOS-compatible? Built and tested there, never launched.** Four
  Linux-isms were found by reading for them rather than by CI, because all four
  **compile cleanly and fail at runtime**: `/proc` reads in `pid_alive` and
  `instance::holder`, a `timeout` that is GNU, and a keyboard map that was
  Ctrl-only. Those are fixed and recorded in CLAUDE.md, which is where the rules
  they left behind belong. `release.yml` ships an `aarch64-macos` tarball beside the
  Linux one, and `check.yml` is green on macos-14 for the daemon tests *and* the
  Tauri build.

  What remains cannot be closed from here: **nothing has ever been executed on a
  Mac.** `Chrome::Overlay`'s traffic lights, `open` for URLs and the window chrome
  are written-not-run, and a binary that compiles is not a window that draws. The
  desktop crate cannot even be cross-checked — `objc2-exception-helper` compiles
  Objective-C and wants a real SDK — so this is somebody's afternoon with a Mac,
  not a CI job.

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

  The mechanism to fix it is already here and used for one case only:
  `TrackerKind::Stub` passes `--mcp-config` plus `--strict-mcp-config`, which
  ignores every configured server. Doing that for the live tracker too is the
  small version — the agent and MCP shape stay, the repo dependency goes. The
  larger version is to call the tracker's API from Rust and drop the agent from
  filing altogether; search and create are two calls, and the agent is only in
  that path for the routing rules the repo's tracker skill holds, which would then
  need another home. Either way `Tracker` stops being "four facts" and starts
  owning how it is reached, and the trait's own doc — "which MCP server in the
  repo's `.mcp.json` speaks to it" — is the sentence that changes.

- **The review pane is still `[beta]`, and the label is the honest part.** Needs a
  list of what is actually wrong before anything is touched — collect that from a
  real session rather than guessing. Two gaps already known from the code:
  `is_resolved` is never set by the daemon (`github_write` will not resolve a
  thread, by design), so nothing in the UI can mean "handled"; and the resolve run
  itself has never made a real round trip to GitHub — the suite is unit tests and
  a fixture, which `docs/fixture-pr.md` says out loud.

  *The list, collected from the first real drive of the overlay (fixture PR #13).*
  Keep is the whole point: the overlay showing each thread's file, line, hunk and
  comment reads as a real UI, and beats the old spawn-a-terminal path.
  **All four are done** — the first two by the review-session flow, the last two by
  the flatten (`5bc1800`). Left here because the reasoning is what the next round of
  this will be argued against.
  - ~~**Triage takes too long to sit through.**~~ *Done.* The session's `rvReading`
    screen holds the overlay open while it reads, instead of closing onto a blank pane.
  - ~~**You click `review` twice.**~~ *Done.* One session reads, and the overlay
    advances itself off the decision ask. Permission prompts are still answered in the
    pane, which is why the reading screen says so rather than covering the screen.
  - ~~**The `read` is a wall of text.**~~ *Answered in the prompt, not the UI.* It was
    collapsed to its opening sentence behind a disclosure, and that was reverted after
    driving it: the fold cost a click on every card to reach the one thing the agent
    concluded, which is worse than the length it hid. `commands/review-session.md`
    carries the real fix — it tells the agent the read must be terse. Shortening a
    thing at the source beats hiding it at the end.
  - ~~**Stance + reply is too much machinery per thread.**~~ *Done.* The card is the
    offered positions as one flat list, reply under it, skip on the action bar. What
    made this a design pass rather than a patch was the question underneath it —
    whether the batch survives — and the answer was no: the overlay is session-only
    now. The daemon half of that removal is the item below.

- **Delete the batch the overlay no longer reaches, and flatten `proposal.rs`.**
  The flatten took the triage+batch path out of the UI, so a large amount of proven,
  tested daemon code is now unreachable: `/triage` + `triage::spawn`,
  `pr_resolve_run`/`spawn_resolve_run`, `pr_post`, `post::resolve`'s apply path,
  `resolve_runs` state and its store file, `/committed` + `thread_committed`, the
  gate/commit/stash endpoints, `patch.rs`'s apply-and-fold ladder, and the SPA's
  `rvGate`/`rvRun`/`rvManual`. `proposal.rs` then loses `patch`, `Mode`, `verified`
  and the "change without evidence" check, which is what makes the model actually
  flat rather than flat-looking.

  **Deliberately not done yet, and the order matters.** The session flow has never
  driven a real PR end-to-end: the change and post-go phases are unverified, and the
  fixture cannot post its bot-authored threads without Actions minutes (the run failed
  on the billing gate, not on our code). Removing the proven path before its
  replacement has run once would leave no way back. `/resolve` into a pane is
  untouched and is the real fallback either way.

  Until then the dead code is reachable only by the API, and `web/review-preview.html`
  is what lets the flattened UI be looked at without any of it.

- **`Ctrl+Shift+Tab` for the previous session.** The SPA half is proven correct
  by reading, so the remaining question is delivery, not the binding. The keydown
  listener is registered `capture: true` on `window` (`web/app.js`), so it is the
  first thing to see any key — the `Tab && ctrlKey` arm at the top of it already
  handles both directions (`switchSession(e.shiftKey ? -1 : 1)`), and xterm's
  custom handler claims only `Ctrl+Shift+C` and lets everything else through
  (`web/js/term.js`). Nothing in the page eats it first, and the desktop crate has
  no key handling at all. So the one consumer left is WebKitGTK itself: `Ctrl+Tab`
  and `Ctrl+Shift+Tab` are GTK focus-chain accelerators, and the asymmetry the
  report describes — next works, previous does not — is what a GTK grab on the
  backward-traversal chord looks like. The fix is therefore at the webview layer
  (intercept the GTK `key-press-event` before it reaches focus traversal), not in
  the SPA, and it still wants the real window to confirm the grab and prove the
  interception. An in-page binding cannot fix an event that never arrives.

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

- **The watch build leaks a process every time its client dies, and orchestrator is
  where that gets fixed.** Found on the scienta box: five `ng build --watch` stacked
  inside one `scienta-assets` container, ages 2h to 20h, about 8 GiB with their
  esbuild children, swap at 19 of 23 GiB. `mise run angular:watch` is
  `docker compose exec -ti scienta-assets pnpm run build-watch`, and docker does not
  signal the container-side process when the exec client goes. Verified by counting:
  one live exec client, four live watchers. Every new `mise run watch` stacks another
  and nothing reaps them.

  The model here already fits. `ManagedSpec` gives one `proc_id` per
  `workspace:name`, start is refused when it is already running (`api.rs:1283`), and
  restart kills first (`api.rs:1683`). What is missing is that the pty would own the
  wrong process: `PtyHandle::kill()` takes the local exec client and leaves the build
  running, so declaring the watch in `main_processes` as it stands buys nothing. Two
  ways out. A dedicated compose service puts the lifecycle on docker and there is no
  exec client at all; a `stop_command` on `ManagedSpec` keeps `exec` and adds a
  second thing to keep correct.

  **The compose-service route costs the health parsing, and that is the open
  question.** `ng-watch` health is parsed off pty output (`health.rs`). A service has
  no pty, so it becomes `docker compose logs -f` and the rail's error summary depends
  on that stream behaving the same. Unproven either way.

  Two leaks of the same class are already in here regardless of which route wins.
  `spawn.rs:1552` does `w.processes.retain(|p| p.id != proc_id)` and drops the old
  handle without killing it, and `PtyHandle` has no `Drop` impl, so the child
  survives; the API guards the usual path, but `autostart_processes` on a daemon
  restart does not go through that guard. And `Server::shutdown` kills children
  (`lib.rs:117`) only on `ctrl_c` (`main.rs:39`), so a SIGTERM or a crash leaks every
  managed process.

- **The webview spends its frame budget on terminals nobody is looking at, and a
  hidden one costs more than a visible one.** Two symptoms, one cause: typing
  echoes late, and a `title` tooltip in the rail takes several tries to appear.

  `Term.show` only sets `host.hidden`. The socket stays open and `term.write` keeps
  running, so every terminal opened this session parses its pty stream forever.
  `.termhost[hidden]` is `display:none`, which was assumed to make that free;
  CLAUDE.md still says the renderer stops. The paint stops. The cost does not, and
  on WebKit it goes *up*.

  Measured under playwright's WebKit, 8 terminals at 140x40, `scrollback: 2000`,
  8 KB written to each per frame, two runs per cell:

  | | all 8 visible | 7 of 8 hidden |
  |---|---|---|
  | WebKit | 37-41 ms/frame | 172-188 ms/frame |
  | Chrome | 16.6-16.7 ms/frame | 24.5-25.0 ms/frame |

  A hidden terminal is about five times a visible one, and only on the engine the
  app actually runs on. 180 ms a frame is the typing delay: the keystroke leaves
  immediately and the echo waits for the main thread. It is *not* the DOM renderer
  under heavy output that the WebGL entry below expected to bite, because the
  all-visible column is fine.

  The remedy that entry already names needs no WebGL: stop feeding a hidden
  terminal. `ws::pty_loop` replays the 512 KB ring buffer on connect, so closing
  the socket on hide and reattaching on show loses nothing inside that buffer.
  Queueing chunks while hidden is the smaller change and loses nothing at all.

  Caveat: playwright's WebKit is not WebKitGTK 2.50.4. Same family, different
  build, and the gap measured here is far wider than that difference.

- **The rail rebuilds every second and drops the hover under your pointer.**
  `app.js` runs `Rail.render()` at 1 Hz so the waiting clock ticks, and
  `renderRail` opens with `rail.replaceChildren()` whether anything changed or not.
  Hold the pointer still on a `.sess` row and `:hover` is true before the rebuild
  and false after it, in Chrome and WebKit alike: the node is destroyed and hover
  is not re-targeted until the mouse moves. A native `title` tooltip wants the
  pointer resting on one element for about half a second, so the rebuild leaves it
  a 500 ms window per second, and it arrives late or not at all.

  Speed is not the problem. A 430-node rail renders in 0.46 ms. Rebuilding is the
  problem. The tick exists only for the duration strings, so writing those text
  nodes in place would settle it without anyone having to build a diffing layer.

- **`notify()` writes `sessions.json` before every snapshot.** `AppState::notify`
  calls `persist()` first, a blocking 79 KB write on a tokio worker thread, from
  about seventy call sites, and 92 session records have piled up. Not what makes
  the window feel slow, but it runs on every hook.

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

- **`gh auth token` is the credential, and that is settled.** §6 wanted a
  read-only PAT and it is not going to get one: `gh`'s token works out of the box,
  the daemon's own writes are `gh`-shelled anyway, and the extra scopes buy a setup
  step nobody wants. The daemon used to report this as a live finding every poll —
  removed, for the reason the `⚠` beside `pr_age_ms` went: a condition you have
  decided to live with is furniture, and furniture teaches you to stop reading the
  list. `token_source` is still in the snapshot for anyone diagnosing over the API.
  Revisit only if the daemon ever needs a credential it should not be able to write
  with.
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
