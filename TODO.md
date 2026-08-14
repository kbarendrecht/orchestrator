# TODO

The **Live findings** block below is rewritten by the daemon every poll. It only
ever lists conditions that are true right now, so it stays worth reading.
Everything outside that block is hand-written and survives.

## Next

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

- **transcripts are off** — spawned sessions write no `.jsonl`, so resume and the teardown transcript check do nothing. Set `persist_transcripts` back to true when you are done developing the daemon.
- **review queue is unavailable** — `mise run reviews --json` is not answering: not polled yet
- **`dfafdf` cannot be trusted to run PHP suites** — autoload resolves to `/home/kbarendrecht/development/scienta/vendor/composer/ClassLoader.php`, outside the worktree, so a suite run there loads main's code. §7's post-WIP table assumes otherwise.

<!-- <<< orchd live findings <<< -->
