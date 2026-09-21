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
- **Sessions are the daemon's.** It spawns every one with `--session-id`, so its
  own id and Claude Code's are the same value and hook correlation needs no
  mapping. It never adopts a shell-started session — that exactness is the point.
- **Hooks drive the state.** Claude Code's hooks (`SessionStart`, `PostToolUse`,
  `Stop`, `SessionEnd`, …) POST to the daemon, which is how a row knows whether it
  is working or waiting. The daemon's hook settings *merge* with the repo's own,
  so your project hooks keep firing.
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
- **The review flow, and there are two.** The rail's `handle` button starts
  `/orchd:handle-review` in a pane: one agent in the PR's worktree, reading the
  threads, applying what is right, asking you about the rest, and drafting replies
  it posts only on an explicit go. That is the default because the other one is not
  finished. The other one is the review session — the same agent, in the same
  worktree, but it proposes a stance per thread and the overlay puts those on
  cards; it then writes the code and drafts each reply, which the daemon posts on
  its own credentials. It is the second review item in a PR row's menu. Resolving
  a thread stays your button either way, by design.
- **`fix-pr` is hand-triggered, never automatic.** The guards that protect the
  machine and the repo remain (authorship, one run per PR, a busy branch, the push
  guard below); the automatic trigger does not. It is a gate you read before starting, not one that trips
  while you look elsewhere.

The web UI is compiled into the binary with `include_str!`, so it can never drift
from the daemon serving it — and a change under `web/` needs a rebuild.