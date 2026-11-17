# Installing it

Every way to install the app, what it needs, and where it keeps its state. The
[README](../README.md) is what the app does.

```
brew install --cask kbarendrecht/tap/orchestrator   # macOS, Apple Silicon
mise use -g github:kbarendrecht/orchestrator        # anywhere
```

On Debian and Ubuntu, from the apt repository:

```
curl -fsSL https://kbarendrecht.github.io/apt/orchestrator.asc \
  | sudo tee /usr/share/keyrings/orchestrator.asc > /dev/null
echo "deb [arch=amd64 signed-by=/usr/share/keyrings/orchestrator.asc] https://kbarendrecht.github.io/apt stable main" \
  | sudo tee /etc/apt/sources.list.d/orchestrator.list > /dev/null
sudo apt update && sudo apt install orchestrator
```

Each of those installs the app, the `orchd` daemon it runs per checkout, and the
`orch` CLI. The
[release page](https://github.com/kbarendrecht/orchestrator/releases/latest) also
attaches a `.dmg`, a `.deb` and a tarball. Apple Silicon and x86-64 Linux are
built.

**All three upgrade from inside the app.** When a release lands, the bar at the
top carries an Upgrade button that runs your own channel's command — `mise
upgrade`, `brew upgrade --cask`, or apt behind a password prompt — and then a
Restart button, because the running process is the old build until it goes. A
downloaded `.dmg` or tarball gets the release link instead.

State lives in `~/.config/orchd/` on Linux and `~/Library/Application
Support/orchd/` on macOS — move it with `ORCHD_CONFIG_DIR`. Every setting is in the
settings panel, and [`docs/configuration.md`](configuration.md) lists each one
with its default.

## What you need

| What | Why |
| --- | --- |
| **Claude Code** or **Codex** (`claude` or `codex` on `PATH`, signed in) | The daemon spawns one for every session. With neither, a session exits the instant it starts, so the daemon says so at boot rather than letting you find out that way. |
| **git 2.23 or newer** | Worktrees, branch moves, diffs — all of it. 2.23 is `switch`/`restore`, the oldest thing the daemon spells. One repo setting needs more: `core.fsmonitor` is a boolean from 2.37 and a *hook path* before it, so on an older git the daemon leaves it alone. |
| **WebKitGTK 4.1** (Linux only) | The desktop window. Ubuntu 22.04 / Debian 12 or newer; 20.04 ships 4.0 and will not work. macOS uses the system WebView. |
| **`gh`**, signed in | The review skills post through the agent's own `gh`. The daemon only reads, with `curl`, and takes its token from `gh auth token` unless `github_token_file` or `ORCHD_GITHUB_TOKEN` is set. |

A fresh checkout also needs Claude Code's **workspace trust**, accepted once in
its dialog. Until then its sessions die on spawn.

Nothing here is checked at install time. The daemon checks at **boot**, names what
is missing and what stops working, and starts anyway.
