# Troubleshooting

What each symptom means, in the order people hit them. The
[README](../README.md) is what the app does;
[`docs/configuration.md`](configuration.md) is every setting.

- **Every session dies the instant it starts.** Claude Code's workspace trust has
  not been accepted for that checkout, so `claude` refuses. Accept it in the dialog
  once, per checkout.
- **It will not start: "Orchestrator is already running".** One instance at a time,
  held by a pid file in the config dir, because a second one would spawn sessions
  into the same worktrees and take over the hook settings. Close the running app.
- **A Codex session will not resume: it starts a new conversation instead.**
  Codex names its own conversation, and the daemon learns that name from the first
  hook Codex fires. A session whose hooks never ran leaves nothing to resume. The
  hooks are trusted by hash, and an untrusted one is skipped in silence.
- **A Codex session ignores your own `developer_instructions`.** That field is how
  the daemon hands Codex its skills, because Codex has no `--plugin-dir`, so the
  daemon's value replaces yours for the sessions it spawns.
- **The review pane reads *degraded*.** A configured `reviews_command` exited non-zero and the
  pane is showing its stderr. Deliberately distinct from an empty queue, which is
  what "no reviews" looks like.
- **A setting does nothing.** An unknown key is ignored. The daemon warns at
  start-up with the key it did not know (`orchd.log`, or the terminal); check the
  spelling against the tables in [`docs/configuration.md`](configuration.md).
- **It is not in Finder, Spotlight or your launcher.** A mise or tarball install
  writes its entry on first launch, so start it once from a terminal. If it is
  still missing, run `orchestrator-desktop --install-desktop-entry`, which says
  where it wrote.
- **Started from the launcher, it cannot find `gh`, `claude` or `codex`.** The PR pane
  reports no credential and a session dies on spawn. An app started by Finder or a
  desktop entry does not inherit your
  shell's `PATH`: macOS hands it `/usr/bin:/bin:/usr/sbin:/sbin`, which holds
  neither Homebrew nor mise. The app asks your login shell for its `PATH` at
  startup and adopts it, so this should heal itself. If it does not, your `PATH` is
  probably set somewhere an interactive login shell does not read.
- **A session cannot see a variable your shell has.** The daemon's environment is
  not your shell's. `env_source` bridges that per spawn, and an untrusted
  `mise.toml` is the one case that logs a warning instead of degrading quietly. See
  [The environment a session gets](configuration.md#the-environment-a-session-gets).

