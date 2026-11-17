<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.svg">
  <img alt="orchestrator" src="docs/logo-light.svg" width="280">
</picture>

**Orchestrator helps you get from prompt to merged PR, with all your coding agents
in one window.**

[![release](https://img.shields.io/github/v/release/kbarendrecht/orchestrator?label=release&color=blue)](https://github.com/kbarendrecht/orchestrator/releases/latest)
[![check](https://img.shields.io/github/actions/workflow/status/kbarendrecht/orchestrator/check.yml?branch=main&label=check)](https://github.com/kbarendrecht/orchestrator/actions/workflows/check.yml)
[![licence](https://img.shields.io/badge/licence-AGPL--3.0--only-blue)](LICENSE)
![platforms](https://img.shields.io/badge/platforms-macOS%20%C2%B7%20Linux-lightgrey)

https://github.com/user-attachments/assets/618ec22f-d196-4a87-ac29-83e518897016

- **Run agents side by side.** Every [Claude Code](https://www.anthropic.com/claude-code)
  or [Codex](https://developers.openai.com/codex/cli/) session gets its own copy of
  your repo (a git worktree), so they never touch each other's files.
- **See who needs you.** One list shows every agent: working, or waiting for you.
- **Check the work.** Read the diff, edit files and search the code in the same
  window.
- **Get the PR merged.** One button has an agent fix the red checks, another has it
  work through the review comments. Nothing is posted without your go.

## Install

```
brew install --cask kbarendrecht/tap/orchestrator   # macOS, Apple Silicon
mise use -g github:kbarendrecht/orchestrator        # macOS or Linux
```

You need Claude Code or Codex, plus git and `gh`. The apt repository, the downloads
and the details are in [docs/install.md](docs/install.md).

## Start

1. Open Orchestrator and choose your repo folder.
2. Press <kbd>⌘N</kbd> (<kbd>Ctrl+N</kbd> on Linux) and tell the agent what to do.
3. Do that again for the next task. <kbd>⌘Space</kbd> (<kbd>Ctrl+Space</kbd>)
   jumps to the agent that needs you.

## More

[Install](docs/install.md) · [Configuration](docs/configuration.md) ·
[Security](docs/security.md) · [Troubleshooting](docs/troubleshooting.md) ·
[How it works](docs/architecture.md) · [Developing](CONTRIBUTING.md)

## Licence

[AGPL-3.0-only](LICENSE), © 2026 Kars Barendrecht. The vendored web assets are
listed in [THIRD-PARTY.md](THIRD-PARTY.md).
