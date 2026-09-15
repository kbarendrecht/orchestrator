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

- **Six places the app still assumes this monorepo, all in what you see rather than
  in what it does.** The config is agnostic and "make it run somewhere other than
  this machine" above closed the mechanical half. What is left is presentation, and
  each of these reads as a fault on a repo that simply is not shaped like this one.
  - ~~**The drawer's stack badge is docker, hardcoded.**~~ **Done, and it drew
    nothing rather than reading `ok_patterns`.** `stack_running` already did the
    filesystem check for a compose file; it was the *return type* that had nowhere
    to put "there is no stack here" and folded it into `false`. It answers
    `Option<bool>` now, `None` is no stack, and the drawer draws no dot and no
    words for it. Held by a unit test on the three answers and by `page-check` on
    the rendered header, the latter checked by breaking it.

    **This repo was one of the affected ones**, which is the part worth knowing:
    `orchestrator/` carries no compose file, so its own drawer had a permanent red
    `stack down` the whole time. A badge that is wrong on the machine it was
    written on is not a portability problem, and nobody had noticed.
  - ~~**The vendored skills name this repo's task runner.**~~ **Done, and the
    command they named exists in no repo on this machine.** Four skills said `mise
    run pre-commit:run`, hedged as "where it exists" — and the hedge was carrying
    the whole sentence: one checkout's task is `pre-commit`, one has `lint` and
    `test`, and this repo has no such task at all, only a git hook. A run following
    that line either found nothing or checked nothing, and neither said so.

    They say **`run this repo's checks`** now, and a test refuses `mise` and four
    other runners by name while insisting the line is still there — checked by
    putting the command back, and by deleting the line.

    **A `checks_command` setting was built and reverted, and that is the part worth
    keeping.** It was a config field, a settings-panel field and `$ORCH_CHECKS`
    through `launch::session_env`, all of it working and tested. It is the wrong
    answer because a repo with many kinds of check has no single command to put in
    one field, so the setting only moves the guess out of the skill and into the
    daemon — and the agent is standing in the repo and can read what it says. The
    same reasoning the tracker rules already run on: *"this file deliberately does
    not name them"*. Reach for a setting when the daemon itself has to run the
    thing; an instruction to the agent is not that.
  - ~~**Boot warnings never reach the window.**~~ **Done.** `Warning` is in the
    snapshot (`Inner::machine`, set once by the caller that ran the check), and a
    fourth bar draws the list — a missing `gh`, a `reviews_command` that is not
    there, a tracker the repo declares no server for. `page-check` holds it, and
    the gate was checked by breaking it twice: dropping the `renderMachine` call
    and emptying the snapshot field each fail the same two lines.

    Two things settled while doing it, both worth knowing before the next one.
    **ts-rs writes one file per crate run**, so a type in `orchd-repo` declaring
    `export_to = "snapshot.d.ts"` truncates that file to what this crate alone
    exports — `TokenSource` was already the pattern, and `repo.d.ts` is the answer.
    And **`cargo test -p <one crate>` truncates the generated types the same way**;
    `mise run check-web` regenerates all of them, so run it after a single-crate
    test before reading a `.d.ts` diff.
  - ~~**Half the settings have no field.**~~ **Done for the two that needed one.**
    `worktree_init` and `workspace_notes` are the things a *new* repo has to say and
    had no way to say them but hand-editing `config.json` — a file most people never
    learn they have. Four fields now: worktree init beside worktree setup, and a
    note each for main and for a worktree. `env_source` and `shared_worktree_paths`
    stay config-file only, which the entry always said was right: they are decisions
    about how the machine works, not about this project.

    The Worktree setup help text is fixed too. It used to explain itself through
    Claude Code's `WorktreeCreate` hook — a sentence about one repo's arrangement
    rather than about the setting, and meaningless for a repo with no such hook or
    an agent with no such event. It says what the two halves *do* now, and why the
    second runs even when the first failed.

    **One thing worth keeping, because it was wrong the first time.** A saved
    setting does not reach the running daemon — `get_config` answers from the
    running config on purpose, so the panel keeps saying what is true until a
    restart. The first version of the browser check reloaded the page and asserted
    the new value was back; it is not, and that is correct. Restart, then read.

    The guard to keep: an empty note saves as `null`, not `""`. Dropping the `||
    null` in `settings.js` was tried and writes `{"main":""}`, which
    `WorkspaceNotes::for_main` hands to an arriving agent as a note. Only a browser
    catches it — the e2e flow posts JSON and never touches the box.

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

