# TODO

Hand-written, and it survives. The daemon's live findings used to be spliced into
this file, which churned it from every build; they now go to a gitignored
`daemon.log` instead, and only when the repo being managed is this source tree.

## Next

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

- **Containers and ports, if orchd ever hosts a heavier repo.**
  `docs/workspace-isolation.md` has the decision record and the shape to build,
  from a sourced research pass (`docs/research/worktree-docker.md`): per-worktree
  compose projects (`COMPOSE_PROJECT_NAME`, ports from a pool), **not** the
  shared-stack `docker exec` model that was cut with the capability subsystem. The
  sibling problem is a per-worktree process publishing a fixed port; the peer answer
  is a host port range plus a `$PORT` placeholder, and `ORCHD_PORT_BASE` — already
  used per fix-pr run — is the hook. Neither is wanted yet: orchd carries no
  container config at all, and that is the portable default.

- **Let `fix-pr` ask.** The ask channel is real and proven — the session POSTs
  `…/ask`, long-polls `…/ask/:ask/wait` in bounded loops, the card renders over the
  pty, and a `free: true` option opens a box so "let me write it" can. The resolve
  run uses it. `commands/fix-pr.md` does not mention it once, and
  `vendored_prompt_file` renders `{{ASK_BASE}}` for that template anyway — so the
  machinery is handed to the session and never used.

  What the prompt says instead, in five places, is **stop**: a conflict whose
  resolution is a judgement about behaviour, the same job failing twice, a fix that
  would change behaviour to go green, a rejected `--force-with-lease`. Every one is
  a question with two or three real options. But "stop and ask" means print it in
  the pane and end the turn — so the session lands at `your_turn`, `settle` records
  `Exhausted` ("the run gave up"), and you have to find the pane and read back what
  it wanted. The same information as a card would carry, except the run had to die
  to deliver it. Converting those five into asks is the change; the prompt is where
  it lives, not Rust.

  *The concurrency cap this used to be paired with is gone* — it was vestigial, and
  a run blocked on a question can now hold its slot for as long as you take.

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

  **What was called the actual work here is done.** A daemon-cut tree already runs
  the target repo's own `WorktreeCreate` hooks: `spawn::create_worktree` asks them
  first through `hook_cut_worktree`, adopts the tree they printed, puts it on the
  branch it needs, and cuts its own only when that declines. So this entry is now
  the smaller thing it always claimed to be — an explicit mode instead of a subdir
  inference. Still watch the double-run: a repo with both a `WorktreeCreate` hook
  and a configured `worktree_setup` must not get both.

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

- **macOS: launched now, and mostly working.** A second person ran it on a Mac on
  2026-09-01, which closed the "never executed" half of this. What that afternoon
  found, all fixed: an app started from Finder inherits none of your shell's `PATH`,
  so `gh`, `node` and `claude` were all missing at once; sessions stuck at
  `starting` (a hook arriving before the record was inserted); a `⌃` drawn where the
  modifier is `⌘`; and no Finder entry at all from a mise install.

  What is still unanswered there:
  - **Scrolling feels sluggish, and halts.** Two of the fixes above plausibly
    explain it (hidden terminals were being parsed, and the rail rebuilt at 1 Hz),
    but that is not confirmed. If it survives 2026.9.3, the next suspect is the DOM
    renderer — see the WebGL decision below, whose evidence is entirely WebKitGTK.
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

  *The first real drive found four things and all four are fixed* — the reading
  screen, one click instead of two, a terse read asked for in
  `commands/review-session.md` rather than folded away in the UI, and the card as
  one flat list. The one worth remembering: shortening a thing at its source beats
  hiding it at the end.

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

- **Declare the watch as a managed process, with its `stop_command`.** The daemon
  half is done (`stop_command` on `ManagedSpec`): a command that stops what the pty
  is a *client* of, run before the kill, on close, restart and shutdown. What is
  left is repo-side — put the watch in `main_processes` with
  `docker compose exec -T <assets> pnpm run build-watch` and a `pkill -f` beside it,
  and confirm on the box that starting and stopping it twice leaves no watcher
  behind.

  *The compose-service alternative was rejected, and the reasoning is worth keeping.*
  Moving the watch to its own compose service and following `docker compose logs -f`
  costs two things the pty gives for free: `logs -f` replays history, so the health
  parser would open on yesterday's failures, and "the child exited" would become
  "the log follower exited", which says nothing about the service. It does not even
  avoid the problem — stopping a service is still not killing a pty — so it needed a
  stop mechanism too.

## Decisions worth revisiting

- **History keeps its AI attribution.** *Decided, not overlooked.* The monorepo
  half of this is **done**: a rewrite on 2026-09-01 scrubbed the names out of every
  message (0 mentions in 430 commits now) and dropped the commits that added the
  226 KB `design/review-overlay.artifact.html`. What is left is the attribution —
  `Generated with Claude Code` / `Co-Authored-By: Claude` / `happy.engineering`
  trailers, still on most messages. Scrubbing it is one
  `filter-repo --message-callback` pass,
  and the cost is the same as the author rewrite: every SHA changes and the 13
  release tags need re-pushing. (Re-pushing them is free — tag *updates* do not
  retrigger the release workflow, only tag *creation* does, measured when the
  author rewrite moved all 13 and nothing built.) Judged not worth it; say so if
  that changes.

  Three things that would quietly undo a rewrite:
  `../orchestrator-pre-rewrite.bundle` still holds the old commits, another machine
  still has the pre-rewrite history with the personal address configured (commit
  `1092a8f` came from there), and the local tags `pre-rewrite-main` /
  `pre-rewrite-handoff` hold the pre-scrub commits including that artifact. Delete
  those two tags when you are satisfied with the scrub.

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
  cannot garble; a browser tab keeps the fast path.

  **The evidence is entirely WebKitGTK, and macOS is not WebKitGTK.** `CHROME` is
  the test, so a Mac gets the DOM renderer on the strength of a bug measured on
  Linux, and the first Mac user reports scrolling that is sluggish and halts. The
  cheap experiment is one condition — allow WebGL when the platform is macOS — and
  the question it answers is whether WKWebView garbles glyphs the way WebKitGTK
  does. Only a Mac can answer it.

  The cost of the DOM renderer is now partly measured: parsing into a *hidden*
  terminal cost about five times a visible one under WebKit (172-188 ms a frame
  with seven of eight hidden, against 37-41 with all visible), which is why hidden
  terminals now queue instead of writing. What that leaves unmeasured is the
  visible pane under a heavy scroll, which is the report.

- **Sessions archived before the rename still say `green`.** `Kind::Automation`
  carries the command as a free string, so records already in `sessions.json` keep
  the old name and their rows read `green` until they are deleted. Nothing
  switches on the value, so rewriting them on load would be churn for a label. The
  `green-<pr>` prompt directories under `~/.config/orchd` are dead files for the
  same reason.

- **`gh auth token` is the credential — and that is no longer settled.** Reopened
  on 2026-09-01: the plan is now to drop `gh` for the API and keep it only as token
  *discovery*, which is the "One credential" item above. What follows is why it was
  settled, which is the argument that item has to beat. §6 wanted a read-only PAT
  and did not get one: `gh`'s token works out of the box, the daemon's own writes
  are `gh`-shelled anyway, and the extra scopes buy a setup step nobody wants. The daemon used to report this as a live finding every poll —
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

- A rewind modal of our own, listing a conversation's turns as rewind points.
  `Rewind conversation…` on the row's context menu reaches Claude Code's own
  picker, which has the whole feature including files. Ours only pays off if that
  proves too coarse, and it would need a way to select a message index from outside
  the TUI, which the CLI does not expose.
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
