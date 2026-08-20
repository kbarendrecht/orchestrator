# TODO

The **Live findings** block below is rewritten by the daemon every poll. It only
ever lists conditions that are true right now, so it stays worth reading.
Everything outside that block is hand-written and survives.

## Next

- **The two-phase resolve flow — handover.** All four phases of
  `docs/resolve-flow-plan.md` have landed, and none of it has answered a real
  reviewer yet. Where it stands:

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

  *Built, never executed.* `POST /api/pr/:n/resolve-run` resolves your decisions
  through the same `post::resolve` the batch uses, writes `plan.json` beside the
  prompt, and spawns a session on `commands/resolve-run.md`. The session commits
  per thread and calls `…/thread/:id/committed`, which blocks while you look at
  the *real* commit diff beside the drafted reply and decide; the daemon posts on
  its own credentials or holds it back. The run's per-thread state is in the
  snapshot as `resolve_runs` and rendered by `rvRun`, with push and re-request as
  their own buttons. Every line of that path is typed and unit-tested and has
  never met a real PR, because testing it needs a review comment from somebody
  who is not you — `acknowledged()` in `src/github.rs` treats a thread whose last
  comment is yours as answered, so you cannot self-review your way to a test.

  *Deliberately still there.* The old batch (`/api/pr/:n/post` and the manual
  phase) is the secondary button on the final screen. It is proven and a
  words-only review does not need an agent. Retire it once a run has done real
  work, not before — and that retirement is what finally answers the beta-gate
  item below.

  *Known gaps.* A run ends with commits and posted replies but nothing pushed
  until you press the button, which is intended, but the overview does not yet
  say "unpushed" anywhere. `needs_you` is never set on a thread — the session has
  no way to report that it could not finish one, so a failed thread just stays
  `pending`. The run record is memory-only, so a daemon restart mid-run loses the
  account of it while the commits survive; the plan calls for that to be durable
  and resumable. And nothing re-validates drift *per thread*: the prompt checks
  `base_sha` once at the start.

- **Promote the in-UI review overlay out of beta.** The overlay now does the
  real work: threads listed under the PR with their file, hunk, and a reply box,
  and replies/reactions/re-request go straight through the GitHub API
  (`src/github_write.rs`). It ships as the `resolve in ui [beta]` menu item
  beside the old `/resolve`-into-a-terminal path. What remains is deciding when
  to make the overlay the default and retire the terminal spawn. Resolving a
  thread is deliberately *not* an API call — that is the author's button, by
  design (`github_write.rs:10-13`) — so this item is about the beta gate, not
  the missing action.

- **Stacked-PR support.** Two halves. First, a context-menu `stack` action on a
  PR row that opens a session starting from that PR's code — a new branch based
  on the selected PR's head, its own worktree (cwd = main, via the existing
  `worktree-create`/`worktree-link` hooks), and an interactive session. This is
  the `/resolve` spawn machinery pointed at a *new* branch off a PR head rather
  than the PR's own branch. The stack is then detected for free: `link_stacks`
  (`src/github.rs`) already matches `child.base_ref == parent.head_ref`. Second,
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

- **The fix run asks to trust `~/development`.** Pressing `fix` starts the run and
  Claude Code puts up its "Accessing workspace" prompt for `~/development`, which
  is neither the worktree nor anything the daemon names. Not reproduced, and the
  obvious causes are ruled out: `spawn_fix_pr_session` spawns with the PR's
  worktree as cwd, that dialog prints the cwd verbatim, trust is inherited from an
  ancestor so a worktree under a trusted checkout needs none (verified against a
  throwaway repo with the same `.claude/worktrees` layout), and there is no
  `~/.claude/projects/-home-kbarendrecht-development` directory, which there would
  be if a session had ever started up there. The one thing a run touches outside
  the checkout is its own instructions at `~/.config/orchd/fix-pr-<pr>/prompt.md`
  (`spawn.rs`, deliberate: the repo's edit-boundary hook blocks writing them
  inside, and a file in the worktree would dirty the tree). Next step is the exact
  path the dialog names.

- **Unsettled review findings on the portability work.** A high-effort review of
  `559c803..ff387df` confirmed eight problems; four were fixed in `ff387df`, and
  these are the ones left, held deliberately because each wants a decision rather
  than an obvious patch. To work through together:
  - **`requested_team` means two different things.** In `requested` coverage it is
    "matched the search but not by name"; in `all_open` it is real team
    membership. So a machine that flips `review_ranking.coverage` while inheriting
    the acme rule ladder silently reorders its whole queue — every
    non-personally-requested PR satisfies `requested: team`, takes rank 3, and the
    re-review, sidequest and catch-all rules below it become unreachable. The test
    `a_machine_key_overrides_the_profile` blesses exactly that combination. Either
    make the field truthful in both modes or split it (`matched_only`).
  - **`worktrees_subdir` accepts `""`, `"."` and `"./x"`.** The guard refuses
    absolute and `..` but not these: `""` collapses the worktrees dir onto main
    and makes the changed-files exclude prefix `/`, which no porcelain path
    matches, so main's pane lists every sibling worktree's edits — the §2 leak the
    exclude exists to prevent. Normalising components at parse time (dropping
    `CurDir`, rejecting empty) would close it, and would also move validation out
    of an accessor that re-warns on every hook event.
  - **The `all_open` walk has no whole-fetch deadline.** `review_timeout_seconds`
    used to bound the entire external fetch; `--max-time 120` bounds one request,
    and the walk can issue up to 200 of them plus the teams query. A slow-but-not
    -erroring GitHub can hold the poll far past its interval with the pane
    spinning instead of going `Degraded`. Also worth sizing the point cost: a
    shared-pool repo with ~1,000 open PRs is ~20 pages of a heavy selection every
    5 minutes, against a 5,000/hour budget `config.rs` still calls negligible.
  - **The prose maps are stale.** `README.md` (:119, :226-227) still lists
    `github.rs`/`github_write.rs` and describes the queue as `mise run reviews
    --json` backed by the deleted `docs/reviews-json.md`; hand-written TODO
    entries (:35, :69, :191) and `docs/resolve-flow-plan.md` (:12, :33, :80) still
    name the pre-`forge/` paths. CLAUDE.md points a new reader at README for the
    module map, so this is the one a stranger hits first.
  - **The forge seam is narrower than it reads.** `ForgeKind` is parsed and
    documented but never branched on, and `Forge::reply`/`thumbs_up`/`rerequest`
    have no production callers — every write still goes through the concrete
    `forge::Target`. So the claim below that "every read and write goes through
    the `Forge` trait" overstates it: a second impl would still need the four
    concrete `GitHubForge::new` sites made generic and the write path rerouted.
  - Unverified, not a finding: whether `review-requested:@me` matches PRs
    requested of a *team* you are on. It decides whether the default coverage
    quietly misses team-assigned reviews. acme currently has zero of both, so
    it could not be settled empirically — the `requested` path has never run
    against real data with results.

- **Make it run somewhere other than this machine.** Everything below is a
  hardcoded assumption about one monorepo, and each is a setting or a probe
  waiting to be written:
  - **Config profiles carry the stack-specific settings.** *Done.* A `profile`
    setting (`default` | `acme`, `src/config.rs`) selects a baked-in bundle a
    machine's `config.json` is deep-merged over — config keys win, arrays replace
    whole (`Config::parse_with_profile`). `default` is empty; `acme`
    (`src/profiles/acme.json`) supplies that stack's processes, capabilities,
    tracker, upstream refs and review ranking, so its many machines write only
    `{ main_checkout, profile }` plus whatever they override. Adding a profile is
    a new arm plus a JSON file.
  - **The review queue is built in.** *Done.* It asks the forge directly
    (`Forge::review_candidates`) — no external command, no acme JSON shape, no
    `acme/monorepo` fallback URL. Everything opinionated is config
    (`review_ranking`, `src/reviews.rs`): a `coverage` mode (`requested` default,
    or `all_open` for a shared-pool repo that walks every open PR), a first-match
    rule ladder, `blocked_when`, `tiebreak`, `skip_labels`, `bot_reviewers`, and a
    `without_label` predicate escape. Portable defaults are generic and label-free;
    acme's exact queue (its `.mise/review/queue`) lives in the `acme`
    profile — verified byte-for-byte against `mise run reviews --json` (same
    actionable/blocked sets, order, prio and blockers). A checkout with no
    resolvable forge repo reads `ReviewState::Off`. What is still GitHub-only is
    the *forge* itself — see the forge item below.
  - **Docker and `ng` are no longer assumed.** *Done via profiles.*
    `default_for` ships no `main_processes`, so a fresh checkout autostarts
    nothing that does not exist; acme's `ng-watch` (the real
    `silent:exec:toolbox ng build --watch`, `autostart:false`) and `docker` come
    from the `acme` profile. A probe that *suggests* processes on first run
    (compose file, package.json script) was scoped and deferred — the honest
    default is to start nothing.
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

  - **Worktree creation, and the session model, still assume Claude.** Interactive
    worktrees are made by `claude --worktree` (`spawn::spawn_worktree_session`), so
    the *coding agent* is what cuts the worktree — which pins orchd to Claude Code
    even for that step. The daemon already knows how to do it agent-free: PR
    worktrees use plain `git worktree add` (`ensure_pr_worktree` →
    `git::worktree_add_existing`) and only rely on `worktree-link` at SessionStart,
    not on `worktree-create`. So interactive creation could become a configurable
    strategy — a `git` mode (daemon cuts `git worktree add -b worktree-<name>
    <dir>/<name> <upstream_ref>`, no agent) versus today's `agent` mode — with
    fresh checkouts defaulting to `git` (portable) and acme's existing config
    keeping `agent` so its `worktree-create` push/base setup is untouched (same
    serde-default-vs-`default_for` split used for the review queue). But note the
    honest scope: creation is one Claude coupling among the load-bearing ones. The
    session model is Claude-specific throughout — `--session-id` correlation,
    transcript slug lookup, the `ai-title` field, `--resume`, and the whole
    hook-observer plumbing (`--settings` injection, SessionStart/PostToolUse/Stop).
    Hosting another agent means abstracting *that* layer, not just worktree
    creation; this bullet is the first, self-contained step, not the whole job.
  - **GitHub is the only forge.** The seam is now a real one: every read and
    write goes through the `Forge` trait (`src/forge/mod.rs`), with `GitHubForge`
    the sole impl (`forge/github.rs` GraphQL against github.com, `forge/github_write.rs`
    shelling `gh`). Callers name `crate::forge::` and the model types are
    forge-agnostic (`forge/model.rs`). A `config.forge` enum (`ForgeKind`, GitHub
    only) picks the impl. What remains for a second platform: another `Forge`
    impl, a `ForgeKind` arm, and dyn/enum dispatch at the (currently concrete)
    construction sites. Two known GitHub-shaped leaks to generalise then —
    `ThreadRoot`'s `comment_id` is a REST id, and `GitHubForge::detect`'s
    URL-parsing is github.com-specific.
  - **Output language is a setting; the tracker is config already.** *Done for
    the language.* `output_language` (`src/config.rs`, default `English`, the
    `acme` profile sets `Dutch`) fills a `{{LANGUAGE}}` placeholder the triage
    and resolve prompts use for the prose the agent *writes* — replies and story
    text. Prompts and code stay English, and a thread's own language still wins
    when it is clear. The `tracker` (`Tracker` enum) is already a config field
    (`none`/`shortcut`/`stub`); Shortcut is still the only real backend, and
    adding another (Jira, Linear, …) is a separate integration, not a setting —
    it would want a tracker seam the way the forge has one.
  - **The base ref default is portable.** *Done.* The generic default is
    `origin/HEAD` + remote `origin` (`src/config.rs`) — the remote's own default
    branch, no `develop`, no fork. `fetch_upstream` (`src/git.rs`) is now
    config-driven (it split `remote/branch` out of `upstream_ref` instead of the
    old hardcoded `git fetch upstream develop`); a `HEAD` branch fetches the whole
    remote to keep its symref fresh, a named branch fetches just that. The
    fork-with-`upstream/develop` layout is the `acme` profile's, not a global
    default. (A rarely-hit `parse_pr` baseRefName fallback still says `develop`;
    left as out-of-scope — GitHub reliably supplies the field.)
  - The folder picker already exists (`desktop/src/main.rs`, shown when
    `Config::existing()` is `None`), so first-run has a start; what it does not
    have is the rest of the questions.

- **The update nudge cannot fire on this repo.** Belongs after run-elsewhere: the
  nudge only matters once orchd is distributed to someone who isn't building it
  from source. `github::latest_release` (`src/github.rs:152`) asks
  `api.github.com/repos/.../releases/latest` through plain `curl` with no token,
  and the release repo is private, so the call is a 404 and the poller sees
  `None`. Authenticated, the same request answers `v2026.8.5`. The token ladder
  the PR poller already uses (`ORCHD_GITHUB_TOKEN`, the token file, then `gh auth
  token`) is right there for it to ride, or the repo goes public. Note also that a
  failed check and "no update" are the same answer today, which is why this went
  unnoticed. The debug gate in `lib.rs` is not the bug: `cargo run` deliberately
  never nags, and the nudge would still not fire from a release build.

- **Is it macOS-compatible? Nobody knows.** The code has the paths — `Chrome::Overlay`
  for the traffic lights, `open` for URLs, `#[cfg(target_os = "macos")]` arms in
  the desktop shell — but nothing builds or runs it there. `release.yml` builds
  `ubuntu-22.04` only and ships one `x86_64-linux` tarball, so there is no macOS
  artifact and no CI that would catch a break. Answering this means adding a macOS
  job to the matrix first; until then the honest claim is "written with macOS in
  mind, never executed on it".

- **A README for other people.** The current one is written for whoever already
  knows what orchd is: it opens on `§` references to a spec that no longer exists
  in the repo, assumes the acme monorepo throughout, and documents the parts in
  the order they were built. An open-sourced one needs the thing it is, a
  screenshot, what it needs installed, what it assumes about your repo (see
  above), and how to try it without a monorepo to point it at.

- **Audit the keyboard map for logical, consistent coverage.** Not two more
  chords — a pass over the whole scheme so it is predictable: same modifier
  idioms, obvious inverses, no orphan actions. Concrete gaps feeding it:
  jump-to-a-PR has no keybind at all, and `Alt+m` is a "take main" with no
  matching "release" (release today means ending the session).

## Decisions worth revisiting

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
- **`main:instances` no longer conflicts with main occupancy.** §7 rule 2 said a
  session occupying main should block a `fix-pr` e2e run (its teardown reaches
  into the main checkout). Narrowed to run-vs-run only (`src/fix_pr.rs:169-176`):
  taking main never blocks a run and vice versa, so the rule 2a wait/kill
  preemption UX is moot and was dropped. The tradeoff accepted with it: an e2e
  teardown and a live main session can touch the main checkout's instances dir /
  docker resources at the same time, unguarded. Revisit if that overlap ever
  actually bites — a non-blocking "e2e touching main" indicator would be the
  lighter fix.

## Won't do without a reason

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
