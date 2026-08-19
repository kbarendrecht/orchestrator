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

- **Make it run somewhere other than this machine.** Everything below is a
  hardcoded assumption about one monorepo, and each is a setting or a probe
  waiting to be written:
  - **The review queue is acme's.** `reviews::fetch` shells out to `mise run
    reviews --json` (`src/reviews.rs`) and the daemon consumes that repo's own
    ranking, documented in `docs/reviews-json.md`. Anywhere else there is no such
    task and the pane is permanently degraded. Needs a configurable command, or a
    built-in fallback that asks GitHub directly.
  - **Docker and `ng` are assumed.** `main_processes` ships `mise run watch` and
    `docker compose up` as the two things a checkout has (`src/config.rs`). They
    are already config, but the defaults are a guess about someone else's stack;
    a first run should ask, or probe, rather than autostart a task that does not
    exist.
  - **The worktree layout is the repo's.** `worktrees_dir()` is hardcoded to
    `<main>/.claude/worktrees`, which is where acme's own `worktree-create`
    hook puts them. A repo without that hook gets Claude Code's default location
    instead and the daemon will not recognise its own worktrees.
  - **GitHub is the only forge.** `github.rs` is GraphQL against github.com and
    `github_write.rs` shells `gh`. Other platforms were floated; the seam is the
    two modules, not the callers.
  - **Shortcut is the only tracker** (`Tracker` in `src/config.rs`), and the
    prompts tell the agent to write stories **in Dutch**. Both belong in settings.
  - **`upstream/develop`** as the base ref, and the fork-with-upstream remote
    layout, are defaults that suit one workflow.
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

- Line numbers and syntax highlighting in the open question's detail block
  (`.oqd`). Same want as the diff viewer below, and probably the same answer, so
  do them together: whatever gets vendored for one should serve both.

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
