# Configuring it for your repo

Every setting the daemon reads, what it defaults to, and why. The
[README](../README.md) is what the app does.

The defaults ask nothing of the repo you point at: no review-queue command, no
managed processes, no tracker. You get the session board, PR pane, diff viewer and
worktrees on a bare `{ "main_checkout": "…" }`, and you turn the rest on as your
repo can support it. Everything here is editable in the settings panel and written
back to `config.json`; changes take effect on restart.

| Setting | Default | What it is |
| --- | --- | --- |
| `upstream_ref` / `upstream_remote` | `origin/HEAD`, `origin` | the base every diff and worktree is measured against. On a **fork workflow** — an `upstream` remote beside `origin` — a first run detects it and writes `upstream/<default branch>` instead, so there is nothing to set by hand. |
| `reviews_source` | `default` | where the review pane's rows come from: `default`, `custom` or `command`. `default` is `custom` with no rules applied, so the panel shows the same fields for both and switches to `custom` when one is changed. Absent means what it used to: a command if `reviews_command` is set, the default queue otherwise. |
| `reviews_rules` | *(empty)* | what `custom` lists and how it ranks. See below. |
| `reviews_command` | *(empty — the built-in queue)* | argv printing the review queue as JSON. See below. Used when `reviews_source` is `command`. |
| `main_processes` | *(empty)* | long-running processes shown in the drawer. See below. |
| `tracker` | `none` | the tracker's MCP server, which every session the daemon spawns is allowed, so `handle-review` can file a story for a point that is fair but out of scope. Its token is **not** a config key — set `ORCHD_TRACKER_TOKEN` in the daemon's environment, or let `env_source` read the checkout's own. It also needs the repo to declare a matching **MCP server** — see below. |
| `env_source` | `mise` | which tool is asked what a session's own directory exports — `mise`, `direnv`, or `none`. Config file only, not in the settings panel. See below. |
| `default_language` | `English` | the language the agent *writes* replies in when the thread does not settle it. Prompts and code stay English regardless. |
| `worktree_init` / `worktree_setup` | *(empty)* | two commands run in every worktree the daemon cuts itself. See below. |
| `worktree_retention_days` | `60` | remove the worktree of a conversation not worked in for this many days, hourly, and one that no conversation points at at all. Age is the transcript's last write, not the session's start, so a conversation you kept open for weeks is not old the day after you stop; an orphaned tree is dated by its own directory. **The tree, never the row**: the session stays in the rail and a resume rebuilds the tree, so this reclaims disk rather than losing work, and it removes nothing the teardown preflight would refuse — a dirty tree, an unpushed commit, a live session or a running process all keep it. `0` turns it off. It exists because `claude --worktree` removes its own tree when its session ends and never gets to: the daemon owns the pty and kills it. |

The table above is the set most repos touch. The rest are operational, and most
repos leave them at the default:

| Setting | Default | What it is |
| --- | --- | --- |
| `main_checkout` | *(required)* | the privileged checkout the daemon manages. The one key with no default; the folder picker writes it on first run. |
| `worktrees_subdir` | `.claude/worktrees` | where worktrees live, relative to `main_checkout`. Point it at the same place a repo's own `WorktreeCreate` hook puts them, so the daemon recognises its own worktrees. Kept relative and inside main on purpose. |
| `port` | `7777` | the loopback port the daemon serves the SPA and API on. Never bound to anything but `127.0.0.1`. |
| `repo` | *(derived)* | `owner/name` override, when it cannot be read off the upstream remote. |
| `github_token_file` | *(none)* | a `0600` file holding a read-only GitHub token, outside the repo. An alternative to `ORCHD_GITHUB_TOKEN` or `gh auth token`. Reads only: the daemon writes nothing to GitHub. |
| `worktree_processes` | *(empty)* | managed processes for worktree workspaces, the counterpart to `main_processes`. Empty means a shell is opened on demand instead. |
| `poll_seconds` | `300` | how often the PR poll runs. One query per period, negligible against the API budget. |
| `review_timeout_seconds` | `240` | ceiling for a configured `reviews_command` before the poller gives up on it. The built-in queue is one bounded `curl` and does not read it. |
| `allow_several_in_main` | `false` | let main hold more than one live session. Off because one checkout is one working tree and one git index: two agents there share both, the changed-file pane merges their edits without saying who wrote what, and one agent's `git add` stages the other's work. Moving main's checkout still refuses while any session is live in it. Editable in the settings panel. |
| `default_agent` | unset | what a plain `+` starts when Claude Code and Codex are both installed: `"claude"`, `"codex"`, or unset for the one picked last. One that is not on PATH falls back to the last pick. Editable in the settings panel. |
| `auto_resume` | `true` | relaunch sessions that were live when the daemon last went down, with `--resume`, so a crash costs the scrollback rather than the conversation. |
| `forge` | `github` | which forge the repo lives on. Only GitHub is implemented, and the key is the seam a second one would be added behind — not a config switch that would turn one on. `crates/orchd-repo/src/forge/mod.rs` lists the four things that sit outside the trait and would have to move first. |
| `workspace_notes` | *(empty)* | what to tell an agent whose conversation was just moved into a workspace, keyed by the kind it landed in. The daemon states the factual half (which branch, which directory); this is the half only the repo knows. |

An **unknown key is named and then ignored** — a misspelling does not error, it
just leaves the default in force, and the daemon says which key it did not know
on the `WARN` line at start-up. Ignoring it is deliberate: a config this build
rejects costs you the daemon, and an old file with a stale key must still load.

## The review queue

The daemon builds one itself, so the pane works on a fresh install with nothing
configured: it lists every open PR somebody else wrote, using the same token and
the same `curl` the PR pane already uses. No script, no `node`, no `gh`.

- **The `all` / `asked` word in its header narrows it** to the PRs GitHub has your
  review requested on (`review-requested:@me`, a team you are in included).
  Remembered per browser. `all` is every open PR somebody else wrote: you cannot
  review your own, and the PR pane above holds those. The word is offered only
  where it would narrow something, so a queue configured `requested_only` does not
  show it.
- **Requested first, then age**, oldest first within each. How long somebody has
  waited is true whatever a team's labels mean.
- **Amber means you were named.** A request to a team you belong to stays grey and
  says `team`.
- **Draft, conflicting and failing rows sink** below a "not reviewable" fold. They
  are waiting on their author.

**Narrow and rank it without leaving the daemon.** Set `reviews_source` to
`custom` and `reviews_rules` says the rest, in Settings → Git or in `config.json`:

```json
"reviews_rules": {
  "filter": {
    "labels_any": ["needs-review"],
    "hide_drafts": false,
    "hide_blocked": false,
    "requested_only": false
  },
  "high_labels": ["security"],
  "low_labels": ["chore"],
  "skip_labels": ["no-review"],
  "asked_of_me": "above",
  "asked_of_team": "above"
}
```

- **The two asks are placed separately.** `asked_of_me` is somebody picking you by
  name; `asked_of_team` is somebody picking a group you are in, which anybody in it
  may take. Each is `top`, `above` or `normal`. Both default to `above`, which is
  the one band they shared before this existed, so an unconfigured queue is
  unchanged.
- `high_labels` lifts a PR above the rest, over an ordinary request. `low_labels`
  sinks it to the end, still shown. `skip_labels` drops it — **the only exclusion
  list there is**, so there is never a question of which one the pane obeyed.
- **A lift wins over a sink.** Top-placed ask, then `high_labels`, then an ask at
  `above`, then everything else, then `low_labels`. So a chore label does not bury
  a PR somebody asked you to look at; set that ask to `normal` if you want it to.
- **Where two lists name one label**, `skip_labels` wins over both others and
  `high_labels` wins over `low_labels`. A label in `labels_any` *and* `skip_labels`
  is a queue that can hold nothing, and it holds nothing.
- `hide_drafts` and `hide_blocked` do not overlap: a draft is one of the three
  blockers, so `hide_blocked` would otherwise have hidden drafts and made the
  other flag invisible.
- Every field empty is the built-in queue exactly, so switching to `custom` and
  saving nothing changes nothing.
- **Toggles rather than a search string, because a search string is one forge's
  grammar.** What a checkout configures is the question — who wrote it, what is it
  labelled, is it ready — and each forge answers in its own words. `reviews.rs`
  does the translating for GitHub, into the search where GitHub can express it and
  over the rows it already parses where it cannot.

**Your team's ranking wins if you have one.** Point `reviews_command` at anything
that prints the JSON in [`reviews-json.md`](reviews-json.md) and the
built-in never runs. A non-zero exit shows the pane as *degraded* with its own
stderr, deliberately distinct from "no reviews": silently showing an empty queue
when the source is broken is what would cost a colleague a day.

## Filing stories in a tracker

With `tracker` set, `handle-review` can file a review point that is fair but out of
scope as a story, and answer the thread with its id. The agent reaches the tracker
over MCP, so three things have to line up:

- **`tracker` is one shape:** `{"mcp_server": "shortcut", "token_env":
  "SHORTCUT_API_TOKEN"}`. `token_env` is optional — name it and the daemon pushes
  that variable into the session's environment instead of the server
  authenticating itself. An older file's `host` is still accepted and ignored.
- **`.mcp.json` declares a server named by `tracker.mcp_server`.** The daemon
  approves that one server for the sessions it spawns, never all of them.
- **A tracker skill** (`.claude/skills/*/SKILL.md`) holds the team id, the
  workflow state and the story type. Those are yours, which is why they are not
  settings.

Get the server name wrong and Claude Code drops it **silently**: the tool is
simply absent mid-review. The daemon checks at boot and says so.

## The environment a session gets

A session gets the daemon's environment plus whatever `env_source` says the
session's own directory exports — `mise env --json` or `direnv export json`, per
spawn, in that directory.

That second half is not optional in practice. The daemon's environment is
whatever started it, and a desktop launcher's holds no checkout's variables at
all — so an `.mcp.json` header or a tool a session shells gets the empty string
and fails in its own words.

Every failure is silent by design: no tool, no config, unreadable output, and the
session starts with what it had. An **untrusted** `mise.toml` is the one case that
logs a warning, because the variables exist and the session is not getting them —
a fresh worktree is a new path, so `mise trust` belongs in `worktree_setup`. Set
`env_source` to `none` if the daemon already has everything.

## Worktree hooks

The daemon cuts and removes worktrees itself for PR worktrees, resumes and
relocated layouts. It runs your repo's own hooks around that.

**Creating.** Your repo's `WorktreeCreate` hook is asked first and the daemon
adopts the tree it printed, then puts that tree on the branch it needs; every way
that can decline falls through to the daemon cutting its own. Then these two run,
in order, with cwd set to the new worktree:

| Setting | For |
| --- | --- |
| `worktree_init` | the tree *as a checkout*: basing it on a fresh upstream, triangular push |
| `worktree_setup` | what it needs *beside* the code: symlinks back to main, generated config |

Both stand in for what your repo does at `WorktreeCreate`. Two rather than one so a
repo that splits that work points each setting straight at the script it already
has. Nothing here stands in for `SessionStart`, which fires for a daemon-cut tree by
itself, so whatever your repo hangs off that event still runs.

Both are non-fatal and the second runs even if the first failed: a tree that is
merely un-based is still worth linking. A relative script path resolves against the
main checkout, not the worktree, since the worktree may not carry it yet.

**Removing.** Teardown runs your repo's `WorktreeRemove` hooks, then `git worktree
remove` and `git worktree prune`, which no-op when the hook already did the job.
The hook gets the input Claude Code gives it, the path under `worktree_path`, so
one hook serves both. A
refusal is reported as it stands: never `--force`, never a recursive delete, since
a worktree can hold symlinks back into main and following them destroys the main
checkout.

## Declaring a managed process

A managed process is a long-running command for the main checkout — a build
watcher, a container stack — with the output patterns that decide whether the rail
reads it as healthy or failing:

```json
"main_processes": [{
  "name": "watch",
  "command": ["npx", "ng", "build", "--watch"],
  "failure_patterns": ["Error:", "ERROR in", "error TS"],
  "ok_patterns": ["bundle generation complete"],
  "autostart": false
}]
```

`ok_patterns` is the part worth getting right: it is what *clears* a failure. A
watcher whose success line is missing from the list leaves the rail stuck on
`build failing` after you have already fixed the compile.

**`stop_command`, when the command is a client rather than the process.** Empty
for anything ordinary, where killing the pty kills the process. Set it where that
is not true:

```json
"command": ["docker", "compose", "exec", "-T", "assets", "pnpm", "run", "build-watch"],
"stop_command": ["docker", "compose", "exec", "-T", "assets", "pkill", "-f", "build-watch"]
```

`docker compose exec` runs the watcher **inside the container**, and docker does
not signal it when the exec client goes away — so every start stacks another one
up in there, with nothing reaping them. The stop command runs in the workspace's
directory, bounded, immediately *before* the pty is killed, on every path that
means stop: the drawer's close, a restart, and the daemon shutting down. A failure
is logged and the pty is killed anyway.

