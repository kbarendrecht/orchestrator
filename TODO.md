# TODO

The **Live findings** block below is rewritten by the daemon every poll. It only
ever lists conditions that are true right now, so it stays worth reading.
Everything outside that block is hand-written and survives.

## Next

- **Do the review flow in this UI, not through the `/resolve` skill.** Today the
  button spawns a session and types `/resolve <pr>`, so the whole conversation
  happens in a terminal pane and the threads themselves are never on screen.
  What it should be: review threads listed under the PR, each with its file and
  line, the diff hunk it is anchored to, and a reply box. Replying and resolving
  go straight through the GitHub API. The skill stays for the cases that want an
  agent to do the work — but reading threads, agreeing with one, and resolving
  it should not need an agent at all. Needs write scope on the token, which is
  the first real reason to move off the read-only PAT in §6.

- Connector ribbons between the diff panes. §5 lists them under the PhpStorm
  wishlist; synchronized scroll already falls out of the shared scroll
  container, but the ribbons do not exist.
- Paginate `reviewThreads` past 50. The count renders `50+` today so an
  under-count cannot hide work, which was the point, but the real number is
  still unknown on a long-running PR.
- Rebuild a torn-down worktree on resume. `/api/session/:id/resume` refuses
  rather than half-doing it; §2 step 1-3 (recreate the branch, compare against
  the recorded `head_sha`, offer the recorded commit) is not implemented.
- `inotify` on `.git/HEAD` per workspace (§2). The branch set is refreshed on
  reconcile instead, which is correct but lags a branch switch.
- Virtualized diff rows. The eager cap at 2000 lines holds the worst case off,
  but a 2000-line file still renders 2000 nodes.

## Decisions worth revisiting

- **The changed-files pane still refreshes.** The divergence strip now carries
  the thing worth acting on when a branch has fallen behind, but the list under
  it is still `git status`, recomputed on reconcile. Freezing it was the other
  reading of "it shouldn't update"; a pane showing a tree that no longer exists
  seemed worse than one showing a long list. Say so if you want it pinned to a
  snapshot with an explicit refresh instead.

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

## Won't do without a reason

- Adopting shell-started sessions. The daemon spawns every session so that
  `$ORCH_SESSION_ID` correlation is exact (§2); adopting one would reintroduce
  the cwd/pid heuristics the spec rejects.
- A generic "run this command" endpoint (§12).

<!-- >>> orchd live findings >>> -->

## Live findings

Rewritten by the daemon on every poll. Edit anything outside this block.

- **GitHub token is gh's** — it carries write scopes; §6 wants a read-only PAT in `ORCHD_GITHUB_TOKEN` or `github_token_file`.

<!-- <<< orchd live findings <<< -->
