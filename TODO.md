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

  What is left is three seams — the agent, the tracker and the forge:
  - **Worktree *creation* is decoupled; the session model is not.** The daemon cuts
    every tree itself now — `spawn_worktree_session` runs the repo's own
    `WorktreeCreate` through `create_worktree` and adopts it, with no `--worktree`
    arm left. But the session still spawns `claude`, and the real coupling is
    untouched: `--session-id` correlation, the transcript slug, the `ai-title`
    field, `--resume`, and the whole hook-observer plumbing. Hosting another agent
    means abstracting *that*.
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
    about a missing or renamed *server*. The interactive `/resolve` story step has
    the same dependency, spelled `mcp__shortcut__*` in prose.

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

- **Drag and drop in the rail: the reorder is in, the pair swap is not.**
  *Done:* dragging a session row reorders the list, the order is kept per checkout
  in `localStorage` (`core.sessionOrder`), a session the order has never seen falls
  where `byNewest` would have put it, and `sort by newest` in the row's menu puts
  it back. Driven in a real browser — three worktrees, a drag, a reload, the menu —
  and now gated in `tools/e2e/page.mjs`, which was already booting a browser and a
  sandbox daemon. The gate was checked by breaking it: dropping `sessionOrder` from
  the rail's paint signature fails both drag lines, dropping the `localStorage`
  write fails the reload line. The **mid-drag render guard** is the one part it
  cannot hold — a synthetic drag is over before a snapshot can rebuild the rail
  under the pointer — and `page.mjs` says so where the check is.

  *Left:* dropping a row **onto** another swaps their branches, which is
  `swap-main` generalised to any pair of worktrees and needs a daemon route that
  does not exist yet. The gesture is free — `ondrop` already distinguishes the two
  runs — but the refusals are not: the `swapping` lock, a tree mid-rebase and a
  session mid-turn all have to answer for a pair neither of which is main.

  One thing settled while doing the reorder: the rail uses **HTML5 drag-and-drop**,
  not the drawer's pointer maths. The rail is one column, so `dragover` answers the
  only question there is, and `checkoutHead` was already doing exactly that on the
  same rail. The pointer maths in `startTabDrag` earns its keep on a strip that is
  horizontal and scrolls.

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

- **One credential, and it stops being `gh`'s.** Reads already go out over curl with
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

