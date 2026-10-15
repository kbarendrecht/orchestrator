# How it works

The shape of the running system: what the host owns, what each daemon owns, and
where a session's state comes from. [README.md](../README.md) is what it does;
[CLAUDE.md](../CLAUDE.md) is what will bite you while changing it.

- **A host, and a daemon per checkout.** `desktop/` is a
  [Tauri](https://v2.tauri.app/) v2 shell that runs `host::serve` on a loopback
  port, spawns one `orchd` child per open checkout, and points the webview at the
  host. The page comes from the host; every `/api/*` call goes to the checkout's
  own child, which mints its own token. No sidecar, no fixed port, nothing left
  running. The window is
  frameless and the web UI draws its own titlebar (real traffic lights on macOS);
  window controls go over the same authenticated HTTP as everything else, never
  Tauri IPC.
- **Sessions are the daemon's.** It never adopts a shell-started session — that
  exactness is the point. Claude Code is told the daemon's id with `--session-id`,
  so one value correlates the row and the hooks. Codex names its own conversation
  and takes no id, so the daemon learns that name from the first hook and keeps it
  beside its own (`agent.rs`).
- **Hooks drive the state.** The agent's hooks (`SessionStart`, `PostToolUse`,
  `Stop`, `SessionEnd`, …) POST to the daemon, which is how a row knows whether it
  is working or waiting. One set of handlers serves both agents, because Codex
  adopted Claude Code's payloads; only the delivery differs. Claude Code reads a
  settings file the daemon writes, which *merges* with the repo's own, so your
  project hooks keep firing. Codex takes the same hooks as `-c` overrides per
  invocation, and runs each one only when it is trusted by hash (`codex.rs`).
- **Worktrees.** The daemon makes the tree, at whatever layout your repo uses: it
  asks your repo's `WorktreeCreate` hook first and adopts what that hook made, cuts
  its own with `git worktree add` when the hook declines, then runs `worktree_init`
  and `worktree_setup`, then starts a session *in* the tree. It used to hand the cut
  to `claude --worktree` at Claude Code's own layout, which pinned that session into
  worktree isolation — and that pin refuses writes as well as git, so a scratch dir
  shared into the tree by symlink could not be written from either side of the link.
  The isolation the daemon needs instead is its own, on the agent's Bash, and it is
  git-only: see the push guard below. Teardown is a seven-check preflight, then your
  repo's `WorktreeRemove` hooks, then `git worktree remove`. Never `rm -rf`, because
  a worktree is full of symlinks into main.
- **The review flow.** The rail's `handle` button starts `/orchd:handle-review`
  in a pane: one agent in the PR's worktree, reading the threads, applying what is
  right, asking you about the rest, and drafting replies it posts through its own
  `gh` only on an explicit go. The daemon writes nothing to GitHub. Resolving a
  thread stays your button, by design. There was a second flow, an overlay of
  cards over one review session; it was removed.
- **`fix-pr` is hand-triggered, never automatic.** The guards that protect the
  machine and the repo remain (authorship, one run per PR, a busy branch, the push
  guard below); the automatic trigger does not. It is a gate you read before starting, not one that trips
  while you look elsewhere.

The web UI is compiled into the binary with `include_str!`, so it can never drift
from the daemon serving it — and a change under `web/` needs a rebuild.

## The module map

Four crates, split so `cargo` enforces the layering rather than a script counting
it. Each depends only on the ones above it; [`crate-split.md`](crate-split.md) has why, what it
cost, and why the runtime core is still one crate.

```
crates/orchd-base/    the primitives. Nothing here may import anything below.
  git/            every git call, one file per seam: exec (the timed runner),
                  status, refs, unpushed, worktree, bank, review
  pty.rs          portable-pty host, and the scrollback ring every pty keeps
  proc.rs         run a child with a deadline, portably (no coreutils `timeout`)
  child.rs        the protocol for a checkout's daemon: launch, ready line, observer
  atomic.rs       write-and-rename for every durable file, and setting aside one
                  that will not parse
  model.rs        the shared value types: ChangedFile, FileSet, DiffFile, Bank
  guard.rs        the git rules (push blast radius, reach), run by `orch guard push`
  edit.rs         file read/write with containment and conflict detection
  headroom.rs     the pre-spawn resource check every session goes through
  install.rs      which packaging put this binary here: mise, brew, apt, a file
  window.rs       Chrome, and the handle the desktop shell registers
  timing.rs       per-start phase lines: exec counts, share of the time, slow git
  secret.rs       one fresh token, and the leaf that broke a cycle to get here

crates/orchd-repo/    one checkout, described. No session state lives here.
  config.rs       config file, defaults, the tracker and its credential
  launch.rs       the environment and argv a session's process is built with
  agent.rs        which agent a session runs, Claude Code or Codex, and its argv
  codex.rs        Codex's hooks as -c overrides, their trust, its rollouts
  forge/          the Forge seam, reads only: trait + dispatch (mod.rs), agnostic
                  model (model.rs), the GitHub impl (github.rs)
  diff.rs         numstat, hunk parsing, word-level LCS
  patch.rs        what git says changed: the numstat parser and the dirty list
  skills.rs       the vendored skills in skills/: a plugin dir for Claude Code's
                  --plugin-dir, the same set as instructions for Codex
  reviews.rs      review queue: the built-in GitHub search, or reviews_command
  env_source.rs   where a session's own variables come from: mise or direnv, per spawn
  migrate.rs      repairs a config this build could not otherwise read
  instance.rs     the one-daemon-per-checkout flock
  machine.rs      what the daemon needs from the machine, warned about at boot
  logging.rs      the file log, since a launcher-started app has no stdout

crates/orchd/         the runtime core: the `orchd` library, what the daemon knows.
  api.rs          HTTP surface and the origin/token guards
  model.rs        Workspace / Session / Process, State, ArchiveState
  preview.rs      the file pane's html preview: a sandboxed frame's token and route
  relocate.rs     the swap, the move out of main, and the conversation that travels
  restart.rs      respawn sessions in place on the installed agent, each when idle
  review_api.rs   the PR routes that start work, and the file pane's reads and
                  writes
  state.rs        the daemon's owned state, snapshots, reconcile, durable writes
  store.rs        session record persistence, orphan reaping
  spawn.rs        session / worktree / process spawning, and worktree_setup
  worktree.rs     cutting a tree and the hooks that finish it; teardown preflight,
                  archive, revive, removal
  spare.rs        the pool of one: a worktree cut before anybody asks for one
  triage.rs       the handle-review spawn, and the gates a worktree must pass
  fix_pr.rs       automation state, the fix-pr guard table, a run's verdict
  update.rs       both upgrade bars: is the agent behind, and what this
                  install's own upgrade command is, so the app can run it
  health.rs       a managed process's output → health
  names.rs        the worktree names the rail offers

crates/orchd-serve/   the daemon: the server, and everything it starts.
  lib.rs          start, the router, the pollers, startup recovery
  host.rs         the page, the asset routes, the checkout list, the window commands
  link.rs         orchestrator://open?file=… links, and a second launch handing one over
  ping.rs         the daily usage ping: install id, version, platform
  hooks.rs        hook receiver and the generated settings file
  ws.rs           event stream + pty attach
  firstrun.rs     judging a folder, reading a repo, writing its config, the recents
  main.rs         the `orchd` binary; bin/orch.rs is the `orch` CLI a session gets

web/            the SPA (vanilla, xterm.js vendored) — one module graph under js/,
                booted by app.js; the *.d.ts files are generated from the Rust
                structs, one per crate that exports any
desktop/src/    the Tauri shell: main.rs (window, boot, splash), launcher.rs (the
                .desktop entry and the macOS .app bundle), login_path.rs (the
                login shell's PATH, adopted before the runtime exists)
```

