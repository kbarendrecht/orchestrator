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

  **The items below are now unblocked but still unverified** — the target exists;
  nothing has been driven against it yet:
  - the resolve run end to end (commit per thread, `…/thread/:id/committed`, the
    daemon posting on its own credentials),
  - `triage::gate`'s refusal on a dirty worktree, now safe to dirty on purpose,
  - `open_file`'s `head_sha` arm, once a workspace sits on the PR branch,
  - teardown and the archive it runs, which delete a real worktree,
  - the thumbs-up idempotency assumption in `post.rs`.

  One thing the fixture deliberately does **not** cover: `rerequest()`. A bot
  cannot be a requested reviewer, so that button still wants a second human
  identity — a throwaway account or a fine-grained token for one.

  What was unverifiable before it existed, each found by trying:
  - **The whole resolve flow.** `acknowledged()` (`src/forge/github.rs`) treats a
    thread whose last comment is yours as answered, so you cannot self-review your
    way to a testable thread. Everything downstream — triage, the per-thread card,
    `post_one`'s story substitution and idempotency, `sweep_words_only`, the 👍
    path — is unit-tested and has never made a real round trip.
  - **The `triage::gate` refusal on a run.** Needs a PR that is in the poll, has a
    triage, *and* whose worktree is dirty. No PR satisfies all three, and
    manufacturing it means dirtying a real worktree.
  - **A PR head sha in a link.** `open_file`'s `head_sha` arm never ran because no
    workspace's branch matched a polled PR; only the local-`HEAD` fallback did.
  - **Teardown, and the archive it now runs.** Both only fire on a worktree with a
    session's transcript in it, and the only ones to hand are real work.

  Also outstanding for the same reason: the thumbs-up idempotency assumption
  (`post.rs` — "unverified until the scratch PR settles it"), which is currently a
  guess about GitHub returning the existing reaction rather than a second one.

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
  never met a real PR. It now has one to meet: `mise run fixture` builds a PR
  whose threads really are awaiting you, which is the thing that was missing.
  Driving a run against it is the next step, and the first thing that would settle
  the known gaps below.

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
  (`src/forge/github_write.rs`). It ships as the `resolve in ui [beta]` menu item
  beside the old `/resolve`-into-a-terminal path. What remains is deciding when
  to make the overlay the default and retire the terminal spawn. Resolving a
  thread is deliberately *not* an API call — that is the author's button, by
  design (`forge/github_write.rs:10-13`) — so this item is about the beta gate, not
  the missing action.

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
    *Landed, then simplified.* This was a `default`/`acme` profile split; it
    was retired in favour of making the six settings acme carried (upstream
    ref/remote, tracker, output language, `main_processes`, review command) the
    built-in `#[serde(default = …)]` values, and exposing them in the settings
    panel (`GET`/`POST /api/config`, `config::Settings`) which writes only those
    keys back to `config.json`. A acme machine writes `{ main_checkout }` and
    gets the lot; anyone else edits them in the panel. Deliberately acme-first
    — the open-source defaults are now Dutch/Shortcut/upstream-develop/ng+docker/
    mise-reviews, not generic. Apply is on restart (the running `cfg` is immutable;
    live-apply would be a sweeping refactor, left for later).
  - **The review queue runs a configured command.** *Reverted to this on
    purpose.* A built-in GraphQL queue with a config-driven ranking engine was
    built and worked, but it was more machinery than the one real user wanted to
    own, so the daemon went back to shelling out to `reviews_command` and
    rendering its JSON (`docs/reviews-json.md`, `src/reviews.rs`). `acme`
    points it at `mise run reviews --json`, where the ranking already lives and is
    edited as bash; a plain checkout leaves it empty and the pane reads `off`. The
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
    the default layout (acme stays on `--worktree` by using that layout), which
    would want an explicit mode rather than keying off the subdir. And the real
    coupling is the *session model*, untouched: `--session-id` correlation,
    transcript slug lookup, the `ai-title` field, `--resume`, and the whole
    hook-observer plumbing (`--settings` injection, SessionStart/PostToolUse/Stop).
    Hosting another agent means abstracting *that* layer; the worktree-creation
    split was the first, self-contained step, not the whole job.
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
    adding another (Jira, Linear, …) is a separate integration, not a setting —
    it would want a tracker seam the way the forge has one.
  - **The base ref is config-driven.** *Done, though the default is now acme's.*
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

- **Is it macOS-compatible? Nobody knows.** The code has the paths — `Chrome::Overlay`
  for the traffic lights, `open` for URLs, `#[cfg(target_os = "macos")]` arms in
  the desktop shell — but nothing builds or runs it there. `release.yml` builds
  `ubuntu-22.04` only and ships one `x86_64-linux` tarball, so there is no macOS
  artifact and no CI that would catch a break. Answering this means adding a macOS
  job to the matrix first; until then the honest claim is "written with macOS in
  mind, never executed on it".

- **A README for other people.** The current one is written for whoever already
  knows what orchd is: it is threaded with `§` references to a spec that no longer
  exists in the repo, assumes the acme monorepo throughout (the run example, the
  `ng-watch`/`docker` asides), and documents the
  parts in the order they were built. An open-sourced one needs the thing it is, a
  screenshot, what it needs installed, what it assumes about your repo (the
  editable settings, which currently default to acme's), and how to try it
  without a monorepo to point it at.

- Have a quick look at where the memory goes. Nothing is wrong — measured with
  four sessions and `ng-watch` up, the app itself is ~287 MB PSS against Tabby's
  529 MB idle — but two numbers look higher than they should and neither has been
  looked at:
  - **orchd itself is 76 MB PSS** (169 MB RSS) for a daemon whose live state is a
    few hundred session records and five 512KB ring buffers. Worth an hour with
    a heap profiler before assuming it is fine. First suspects: glibc holding
    freed arenas rather than returning them, and the 128KB transcript tails now
    read for every untitled session at restore, which is ~11 MB of transient
    `Vec` across 87 records.
  - **WebKit is 211 MB PSS** for one page. Terminals keep 10,000 lines of
    scrollback each in `xterm` on top of the daemon's own ring buffer, which is
    the same bytes held twice; the daemon replays on reattach anyway, so the
    client scrollback could be far shorter.
  Two smaller things to check while in there, neither yet measured: `human_edits`
  (`state.rs`) is never pruned, so it grows one entry per hand-edited file for the
  life of the daemon; and each terminal's `xterm` scrollback is the second copy of
  bytes the daemon's ring buffer already holds.

  Do not turn this into a project. If neither is a one-line win, write down what
  it actually is and move on.

- **Audit the keyboard map for logical, consistent coverage.** Not two more
  chords — a pass over the whole scheme so it is predictable: same modifier
  idioms, obvious inverses, no orphan actions. Concrete gaps feeding it:
  jump-to-a-PR has no keybind at all, and `Alt+m` is a "take main" with no
  matching "release" (release today means ending the session). *The two
  `role="button"` keyboard traps this once listed are fixed:* a `keyActivate`
  helper (`web/app.js`) gives the refresh icons and the update-nudge `×` a tab
  stop and Enter/Space, driven end to end (Enter on the refresh icon fires the
  poll). The broader predictability pass is still open.

- **Teardown auto-archives its own prerequisite.** *Fixed.* This was read as a
  dead route but is the opposite: `worktree::archive` (transcripts copied,
  recovery record written) is what satisfies preflight checks 4 and 5, those
  flags are set *only* inside it (`worktree.rs`; the `store.rs` sets are
  disk-load), and its one caller was a route the SPA never invoked — so teardown
  refused on any worktree that had held a session. `teardown` now runs `archive`
  itself when those two are the *only* failing checks and re-runs preflight; a
  live session, dirty tree, unpushed commits or an attached process still refuse
  and never see an archive run under them. The `POST …/archive` route stays as
  the explicit entry point. Verified by construction (archive sets exactly the
  flags preflight reads) and compile, **not** by a live removal — teardown
  deletes a real worktree and there was no disposable target to drive it on.

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

- **`token_source` and `pr_age_ms` are surfaced.** *Fixed.* The PR-pane header
  now shows a live "· N ago" from `pr_age_ms` (ticked off the snapshot clock, so
  a stuck-but-not-erroring poller reads as stale), and a `⚠` with an
  explanatory title when `token_source` is `gh_cli` — the write-scope fallback
  flagged under "Decisions worth revisiting". Env/file say nothing.

- **`save_automation` failures are logged, not dropped.** *Fixed.* Both sites
  (`src/api.rs`, fix-pr run start and end) now `tracing::error!` on a failed
  write, naming the PR and that a restart may re-run past the cap, instead of
  `let _ =`.

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
- **The test-capability subsystem is gone.** orchd used to carry a `Suite` model
  (static/unit/integration/e2e), a composer autoload probe, lockfile-drift
  detection and per-suite trust/isolation — a whole `capability.rs` — so it could
  tell whether a command in a worktree reflected that worktree or silently main's.
  That question is a shared-stack artifact (acme's symlinked `vendor/`); every
  other repo ran it empty. Removed wholesale for open source. `fix-pr` keeps only
  the guards that protect the machine and the repo (authorship, one run per PR,
  branch-busy, the `MAX_AUTOMATION` cap). Two things go with it, both accepted:
  the pre-run trust gate (`fix-pr` is hand-triggered and watched, so a bad run is
  read, not swallowed), and the `main:instances` e2e lock — two concurrent fix
  runs that both reach e2e can now collide on acme's one instances dir. If that
  ever bites, set `MAX_AUTOMATION = 1` (serialize fix runs) rather than rebuild any
  of this.

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
