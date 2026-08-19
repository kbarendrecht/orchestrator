# TODO

The **Live findings** block below is rewritten by the daemon every poll. It only
ever lists conditions that are true right now, so it stays worth reading.
Everything outside that block is hand-written and survives.

## Next

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

- **The update nudge cannot fire on this repo.** `github::latest_release`
  (`src/github.rs:152`) asks `api.github.com/repos/.../releases/latest` through
  plain `curl` with no token, and the release repo is private, so the call is a
  404 and the poller sees `None`. Authenticated, the same request answers
  `v2026.8.5`. The token ladder the PR poller already uses
  (`ORCHD_GITHUB_TOKEN`, the token file, then `gh auth token`) is right there for
  it to ride, or the repo goes public. Note also that a failed check and "no
  update" are the same answer today, which is why this went unnoticed. The debug
  gate in `lib.rs` is not the bug: `cargo run` deliberately never nags, and the
  nudge would still not fire from a release build.

- **Finish the rail row now the names mean something.** A session says which PR it
  is on, or whatever Claude Code called the conversation, so the 8-character uuid
  beside the name is no longer what tells two rows apart and the worktree is no
  longer worth the headline. Move the id into a tooltip and put the workspace on
  the second line next to the state.

- **Audit the keyboard map for logical, consistent coverage.** Not two more
  chords — a pass over the whole scheme so it is predictable: same modifier
  idioms, obvious inverses, no orphan actions. Concrete gaps feeding it:
  jump-to-a-PR has no keybind at all, and `Alt+m` is a "take main" with no
  matching "release" (release today means ending the session).

- **Global kill switch / pause.** §8 lists it in the guards table; no pause state
  exists anywhere in `src/` or `web/`. One switch that stops automation from
  firing or spawning.

- Enforce the 8-process cap on *every* spawn, not just `fix-pr`. §8b caps total
  concurrent Claude processes at 8. `MAX_CLAUDE_PROCESSES` is checked in the
  `fix-pr` guard (`src/fix_pr.rs`) and by auto-resume on startup (`src/lib.rs`);
  ordinary interactive, worktree and `/resolve` spawns all bypass it.

- Paginate the *list/poll* `reviewThreads` past 50. The detailed overlay fetch
  now pages fully (`src/github.rs`, ~100/page), but the summary poll still caps
  at 50 and the PR row renders `50+` (`web/app.js`). An under-count cannot hide
  work, which was the point, but the real number is still unknown on a
  long-running PR.
- Bump `actions/checkout@v4` and `softprops/action-gh-release@v2` in
  `.github/workflows/release.yml`. GitHub is retiring Node 20 and both are being
  forced onto Node 24, which works but annotates every release run until one of
  them stops being forced.
- `inotify` on `.git/HEAD` per workspace (§2). The branch set is refreshed on
  reconcile instead, which is correct but lags a branch switch.

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

- **The terminal keeps the WebGL renderer.** Glyphs coming back as garbage that a
  scroll or a selection cleaned up were read as a paint fault after the canvas is
  resized under the renderer, so a refit now clears the glyph atlas and refreshes,
  and a lost context disposes the addon. Diagnosed from the symptom rather than
  watched working. If it comes back the next step is no WebGL inside the webview
  at all, keeping it for a browser tab: WebKitGTK is already the engine this repo
  treats as the flaky one, and headless WebKit dies on that renderer outright.

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

<!-- >>> orchd live findings >>> -->

## Live findings

Rewritten by the daemon on every poll. Edit anything outside this block.

Nothing outstanding.

<!-- <<< orchd live findings <<< -->
