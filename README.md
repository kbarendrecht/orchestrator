# orchestrator

Run several Claude Code sessions over one repository, from a single window — and
see at a glance which ones are working, which are waiting on you, and which of
your PRs have review threads to answer.

Each session lives in its own git worktree with its own terminal. The daemon owns
every process, so closing the window kills nothing you did not mean to and losing
the browser tab loses nothing at all. Beside the sessions it polls your open PRs,
lists the reviews waiting on you, and drives a review-resolve flow that drafts
replies you approve before anything is posted.

![orchestrator](docs/screenshot.png)

## What it is

A Rust daemon plus a small vanilla-JS web app, shipped as one desktop application
(the daemon runs in-process behind a webview) and also runnable headless in a
browser tab. It hosts [Claude Code](https://www.anthropic.com/claude-code)
sessions; it is not itself an agent.

The pieces:

- **A pty host.** Every session and managed process runs in a daemon-owned pty
  with a replayable scrollback buffer. The web UI is a disposable view of it —
  close it, reopen it, attach from a second window; nothing restarts.
- **A session board.** Spawn a session in the main checkout or in a fresh
  worktree. A state machine (driven by Claude Code's hooks) shows each as
  working, waiting on you, or done, with the build status of any process beside
  it folded in.
- **A PR pane.** Your open PRs, polled from GitHub, with review-thread counts and
  a one-click resolve flow.
- **A review queue.** Optionally, the PRs where your review is requested, ranked
  by a command you configure.
- **A diff viewer** against the merge-base, with an editable pane that warns the
  agent when you have changed a file under it.
- **A CLI.** `orch` drives a running daemon from your shell, and from inside a
  session, so an agent can open a helper session for a subtask, or ask you a
  question and block until you answer. [Install](#install) lists what it does.

## What you need

| What | Why |
| --- | --- |
| **Claude Code** (`claude` on `PATH`, signed in) | The daemon spawns it for every session. Without it a session exits the instant it starts, so the daemon says so at boot rather than letting you find out that way. |
| **git** | Worktrees, branch moves, diffs — all of it. |
| **WebKitGTK 4.1** (Linux only) | The desktop window. Ubuntu 22.04 / Debian 12 or newer; 20.04 ships 4.0 and will not work. macOS uses the system WebView. |
| **`gh`**, signed in | Only for the credential: GitHub itself is reached with `curl`, but the token ladder ends at `gh auth token`. Set `github_token_file` instead and you do not need it. |
| **node** | Only for the review queue that ships with it — the ejected `reviews.js` is a node script. Point `reviews_command` at anything you like, or clear it, and node stops mattering. |

A fresh checkout also needs Claude Code's **workspace trust**, accepted once in
its dialog. Until then sessions die on spawn.

Nothing here is checked at install time. The daemon checks at **boot**, names what
is missing and what stops working, and starts anyway.

## Install

A release attaches an installer per platform and a tarball beside it. The
installers are the shortest path: a `.deb` or the `.dmg` gives you an app in your
launcher, with an icon, and puts `orch` where the shell can find it. The tarball
is what `mise` reads, and is still two binaries you place yourself.

- **`orchestrator-desktop`** is the app. The daemon and the web UI are compiled
  into it, so this one binary on its own is a complete install.
- **`orch` is optional.** A small CLI against a running daemon: `orch new` starts
  another session with a prompt, `orch kill` undoes one of its own spawns,
  `orch ask` puts a question in front of you and blocks until you answer, `orch ls`
  lists what is running. Useful from your own shell, and it is how an *agent*
  reaches the daemon that spawned it — a session can open a helper session for a
  subtask, or ask you something and wait rather than bury the question in its own
  scrollback. `orch new --worktree` gives that helper its own tree and branch, which
  is the difference between two parallel jobs and two agents sharing one git index.
  `orch <command> --help` documents the flags. Nothing requires it.

Apple Silicon and x86-64 Linux are built.

### From an installer

```
sudo apt install ./Orchestrator_<version>_amd64.deb   # Debian/Ubuntu
```

Puts the app at `/usr/bin/orchestrator-desktop`, `orch` on your `PATH`, and a
launcher entry with its icon. `apt remove orchestrator` takes all of it away.

The **AppImage** is the same app for everything that is not Debian: `chmod +x`
and run it. It carries its own GTK/WebKit, which is why it is 79 MB against the
deb's 6.5 MB, and `orch` rides inside it — the daemon puts its own directory on
each session's `PATH`, so an agent can still reach it.

On **macOS**, open the `.dmg` and drag the app to Applications. It is unsigned, so
the first launch is right-click → Open rather than a double-click.

### Through mise (with the `github` backend)

```
mise use -g "github:kbarendrecht/orchestrator"
mise up          # upgrade to the newest release later
```

mise picks the right asset for your platform, verifies its checksum and release
provenance, and extracts **both** binaries — so `orch` lands beside
`orchestrator-desktop` and no second entry is needed. (The older `ubi:` backend
still resolves these releases, but mise has deprecated it.)

### From a release tarball

```
tar -xzf orchestrator-<version>-<platform>.tar.gz   # → orchestrator-desktop, orch
# macOS: the binaries are unsigned, so clear the download quarantine first
xattr -dr com.apple.quarantine orchestrator-desktop orch
```

Put them on your `PATH` (`orch` only if you want it) and run
`orchestrator-desktop`.

A tarball or a mise install ships no launcher entry, because there is no
installer to write one. The app writes its own on request:

```
orchestrator-desktop --install-desktop-entry   # Linux
```

It points at the binary that ran it and uses the same id the `.deb` does, so
installing the package later replaces the entry instead of listing the app twice.

**Linux** needs **WebKitGTK 4.1** at runtime (Ubuntu 22.04 / Debian 12 or newer;
20.04 ships only 4.0 and will not work). **macOS** uses the system WebView and
needs nothing extra.

> **Honesty about macOS:** the macOS build compiles and its daemon tests pass in
> CI, but nobody has launched the app on a Mac yet. Treat the first run as the
> test. Everything below is exercised daily on Linux.

Then launch it and point it at a git checkout when it asks (it shows a folder
picker when it has no config, or when the one on record has moved). That checkout
is *main*; worktrees are cut inside it under `.claude/worktrees/`. State lives in
`~/.config/orchd/` on Linux and `~/Library/Application Support/orchd/` on macOS —
move it with `ORCHD_CONFIG_DIR`.

## Configuring it for your repo

The defaults ask nothing of the repo you point at: no review-queue command, no
managed processes, no tracker. You get the session board, PR pane, diff viewer and
worktrees on a bare `{ "main_checkout": "…" }`, and you turn the rest on as your
repo can support it. Everything here is editable in the settings panel and written
back to `config.json`; changes take effect on restart.

| Setting | Default | What it is |
| --- | --- | --- |
| `upstream_ref` / `upstream_remote` | `origin/HEAD`, `origin` | the base every diff and worktree is measured against. On a **fork workflow** — an `upstream` remote beside `origin` — a first run detects it and writes `upstream/<default branch>` instead, so there is nothing to set by hand. |
| `reviews_command` | the ejected `reviews.js` | argv printing the review queue as JSON. See below. Empty means the pane reads "not configured" rather than "unavailable". |
| `main_processes` | *(empty)* | long-running processes shown in the drawer. See below. |
| `tracker` | `none` | where an out-of-scope review point can be filed as a story. `shortcut` is the one implementation; the seam for adding others is `src/tracker/`. Its token is **not** a config key — set `ORCHD_TRACKER_TOKEN` in the daemon's environment, or let `env_source` read the checkout's own. It also needs the repo to declare a matching **MCP server** — see below. |
| `env_source` | `mise` | which tool is asked what a session's own directory exports — `mise`, `direnv`, or `none`. Config file only, not in the settings panel. See below. |
| `default_language` | `English` | the language the agent *writes* replies and stories in. Prompts and code stay English regardless. |
| `shared_worktree_paths` | *(empty)* | directories inside a worktree that are allowed to be symlinks *out* of it, e.g. a plan dir shared back to main. The editable diff pane refuses every other path that resolves outside the workspace. |
| `worktree_init` / `worktree_setup` | *(empty)* | two commands run in every worktree the daemon cuts itself. See below. |

The table above is the set most repos touch. The rest are operational, and most
repos leave them at the default:

| Setting | Default | What it is |
| --- | --- | --- |
| `main_checkout` | *(required)* | the privileged checkout the daemon manages. The one key with no default; the folder picker writes it on first run. |
| `worktrees_subdir` | `.claude/worktrees` | where worktrees live, relative to `main_checkout`. Point it at the same place a repo's own `WorktreeCreate` hook puts them, so the daemon recognises its own worktrees. Kept relative and inside main on purpose. |
| `port` | `7777` | the loopback port the daemon serves the SPA and API on. Never bound to anything but `127.0.0.1`. |
| `repo` | *(derived)* | `owner/name` override, when it cannot be read off the upstream remote. |
| `github_token_file` | *(none)* | a `0600` file holding a read-only GitHub token, outside the repo. An alternative to `ORCHD_GITHUB_TOKEN` or `gh auth token`. |
| `worktree_processes` | *(empty)* | managed processes for worktree workspaces, the counterpart to `main_processes`. Empty means a shell is opened on demand instead. |
| `poll_seconds` | `300` | how often the PR poll runs. One query per period, negligible against the API budget. |
| `review_timeout_seconds` | `240` | ceiling for `reviews_command` before the poller gives up on it. |
| `story_timeout_seconds` | `300` | ceiling for the borrowed story-filing agent — the one timeout in the daemon, because its caller is a blocking request rather than a rail entry someone is watching. |
| `log_path` | *(none)* | where live findings are written. Unset, they go to a gitignored `daemon.log`, and only when the daemon manages orchd's own checkout. Set it to capture findings from a daemon managing any other repo. |
| `auto_resume` | `true` | relaunch sessions that were live when the daemon last went down, with `--resume`, so a crash costs the scrollback rather than the conversation. |
| `forge` | `github` | which forge the repo lives on. Only GitHub is implemented; the key exists so a second platform is a config choice, not a rebuild. |
| `workspace_notes` | *(empty)* | what to tell an agent whose conversation was just moved into a workspace, keyed by the kind it landed in. The daemon states the factual half (which branch, which directory); this is the half only the repo knows. |

An **unknown key is ignored in silence** — a misspelling does not error, it just
leaves the default in force. Check the spelling against this list if a setting
seems to do nothing.

### Fork workflow, or not

Both are supported and neither needs configuring by hand.

**Not a fork** — one remote, branches pushed to it. This is the default:
`origin/HEAD` is the base, so diffs and worktrees are measured against whatever
your remote's default branch is, whether that is `main`, `master` or something
else. Nothing to set.

**A fork** — `origin` is yours, `upstream` is the one PRs are opened against. A
first run sees the `upstream` remote and writes `upstream/<its default branch>`
into `config.json` itself. If you add the remote later, set the two keys in
settings; naming the remote in `upstream_ref` is enough, since the other is
inferred from it.

Either way the base ref is one setting and both halves of it agree, which is what
`git::detect_base` and the reconciliation in `Config::parse` are for.

### The review queue

A queue ships, so the pane works on a fresh install: on first start the daemon
writes `reviews.js` into its config dir and points `reviews_command` at it. It
asks `gh` for open PRs in your repo where your review is requested, ranks them,
and prints the JSON in [`docs/reviews-json.md`](docs/reviews-json.md). Needs `gh`
authenticated; no npm install, it has no dependencies.

It is **ejected, not embedded** — three consequences worth knowing:

- **Edit it.** The ranking is one opinion (it guesses at `stopper` and `prio`
  labels). It is a normal file in your config dir; change it and the daemon leaves
  your version alone forever.
- **Delete it** to get the shipped version back. Absent is the only case the
  daemon writes, so removing the file is how you ask for a reset.
- **Replace it** by pointing `reviews_command` anywhere else — your own script, a
  `mise` task, whatever already knows your team's real ranking. Clear the setting
  entirely and the pane reads "not configured" rather than pretending.

The one contract is the JSON on stdout. A non-zero exit shows the pane as
*degraded* with the command's own stderr, deliberately distinct from "no reviews",
because silently showing an empty queue when the command is broken is the failure
that would actually cost a colleague a day.

### Filing stories in a tracker

With `tracker` set, a review point that is fair but out of scope can be filed as a
story and answered with its id, instead of a promise nobody is holding.

The tracker is reached **over MCP**, by an agent the daemon borrows for the value.
So two things have to be true beyond the token, and both live in the repo you
pointed the daemon at, not in its config:

- **`.mcp.json` declares a server named for the tracker** (`shortcut`). The daemon
  approves that one server by name for the sessions it spawns — never all of them,
  since a repo may declare a dozen and a story-filing agent has business with none
  of the others.
- **A tracker skill** (`.claude/skills/*/SKILL.md`) holds the team id, the workflow
  state, the story type and the epic routing. Those are yours and they change
  without this project changing, which is why they are not settings.

Get the first one wrong and Claude Code drops the server **silently** — the tool is
simply absent and the run burns its whole timeout mid-review. That is why the
daemon checks at boot and says so.

### The environment a session gets

A session gets the daemon's environment plus whatever `env_source` says the
session's own directory exports. That second half exists because the first is not
what you think it is: the daemon's environment is whatever started it, and started
from a desktop launcher that is the systemd user manager's, which holds no
checkout's variables at all.

The symptom is an MCP server that answers 401. An `.mcp.json` entry spelled
`"Authorization": "Bearer ${SHORTCUT_API_TOKEN}"` expands from the process
environment and nowhere else, so an absent variable goes out as literal text. It
stays hidden because typing `claude` in that checkout still works — `mise activate`
exports at a shell prompt, and an app has no prompt.

So the daemon asks the tool directly, per spawn, in the session's own directory:
`mise env --json` or `direnv export json`. Two things worth knowing:

- **Every failure is silent by design.** No tool, no config, no trust, unreadable
  output: the session starts with what it had. A missing variable is a degraded
  session; a refused spawn is a lost one. An untrusted config is the one case that
  logs a warning, because the variables exist and the session is not getting them.
- **Trust is per config file.** mise refuses a `mise.toml` it has not been told to
  trust, and a worktree is a new path — so a fresh worktree can need `mise trust`
  before its sessions see anything. `worktree_setup` is the place to put that.

Set `env_source` to `none` if the daemon is already started with everything the
checkouts need.

### Worktree hooks

The daemon cuts and removes worktrees itself for PR worktrees, resumes and
relocated layouts. It runs your repo's own hooks around that.

**Creating.** Your repo's `WorktreeCreate` hook is asked first and the daemon
adopts the tree it printed, then puts that tree on the branch it needs; every way
that can decline falls through to the daemon cutting its own. Then these two run,
in order, with cwd set to the new worktree:

| Setting | Mirrors | For |
| --- | --- | --- |
| `worktree_init` | `worktree-create` | the tree *as a checkout* — basing it on a fresh upstream, triangular push |
| `worktree_setup` | `worktree-link` | what it needs *beside* the code — symlinks back to main, generated config |

Two rather than one so a repo with two hooks points each setting straight at the
script it already has. Both are non-fatal and the second runs even if the first
failed: a tree that is merely un-based is still worth linking. A relative script
path resolves against the main checkout, not the worktree, since the worktree may
not carry it yet.

**Removing.** Teardown runs your repo's `WorktreeRemove` hooks, then `git worktree
remove` and `git worktree prune`, which no-op when the hook already did the job. A
refusal is reported as it stands: never `--force`, never a recursive delete, since
a worktree can hold symlinks back into main and following them destroys the main
checkout.

### Declaring a managed process

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

Two things a fresh checkout needs once, both one-time: accept Claude Code's
workspace-trust prompt for it (or `claude --worktree` refuses), and — if you
develop orchd — `git config core.hooksPath .githooks`.

## How it works

- **One process.** `desktop/` is a [Tauri](https://v2.tauri.app/) v2 shell around
  the daemon as a library: `orchd::start` binds a loopback port and the webview is
  pointed at it. No sidecar, no fixed port, nothing left running. The window is
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
- **Worktrees.** At Claude Code's default layout the daemon launches
  `claude --worktree` and the repo's own worktree hooks do the work; for PR
  worktrees, resumes, and relocated layouts it cuts the tree itself with
  `git worktree add` and runs `worktree_setup`. Teardown is a six-check preflight
  and `git worktree remove` only — never `rm -rf`, because a worktree is full of
  symlinks into main.
- **The review flow.** Triage reads a PR's open threads and proposes, per thread,
  a stance and whether code changes; a run commits per thread and drafts a reply
  you see beside the real diff before the daemon posts it on its own credentials.
  Resolving a thread stays your button, by design.
- **`fix-pr` is hand-triggered, never automatic.** The guards that protect the
  machine and the repo remain (authorship, one run per PR, a concurrency cap, the
  push guard below); the automatic trigger does not. It is a gate you read before starting, not one that trips
  while you look elsewhere.

The web UI is compiled into the binary with `include_str!`, so it can never drift
from the daemon serving it — and a change under `web/` needs a rebuild.

## Security

Bound to `127.0.0.1` only, with Origin/Host validation and a per-start token
required on the WebSocket and every mutating route. Hook endpoints are exempt
from the token but confined to their own prefix and can only ever update state.
GitHub auth resolves `ORCHD_GITHUB_TOKEN`, then a `0600` `github_token_file`, then
`gh auth token`; read scopes are all it needs.

**The trust boundary is your user account, not the process.** Loopback keeps the
network out and the Origin check keeps other web pages out. But `GET /` returns
the page with the token substituted into it and is deliberately not gated, so any
process running as you can read the token and then hold everything — including the
pty attach, which means typing into a live agent's terminal. Do not run this on a
machine you share with people you do not trust.

That is a trade rather than an oversight: on a single-user machine a hostile local
process can already ptrace the daemon, and gating the page would break the token
discovery the tooling depends on. It is written down because the alternative is a
sentence that earns trust it has not got.

Agents get narrower credentials than the SPA does, and that part *is* enforced: a
session asks with `ORCH_ASK_TOKEN`, good for its own session's routes, and a triage
run posts with `ORCH_POST_TOKEN`, good for one route on one PR. Neither is the app
token — which matters because those are the runs that read other people's review
comments.

There is a `PreToolUse` guard on `git push` (`orch guard push`) that refuses a
lease-less `--force` and a push to the base branch. Read it as a **mistake-catcher,
not a control**: it sees `Bash` tool calls only, so `gh`, an MCP git server, or a
script the agent writes and then runs all go around it. It is there because a
fix-pr run force-pushes with nobody watching, and that is the mistake worth
catching — not because an agent could be prevented from pushing.

## Layout

```
src/
  lib.rs        wiring, the router, the pollers, startup recovery, SPA serving
  main.rs       the headless CLI over the library; desktop/ is the other caller
  config.rs     config file, defaults, Settings read/write, transcript slug
  model.rs      Workspace / Session / Process, State, ArchiveState
  state.rs      the daemon's owned state, snapshots, reconcile, durable-store writes
  instance.rs   the one-daemon-at-a-time pid lock
  headroom.rs   the pre-spawn resource check every session goes through
  window.rs     Chrome, and the handle the desktop shell registers
  pty.rs        portable-pty host      ring.rs   scrollback
  proc.rs       run a child with a deadline, portably (no coreutils `timeout`)
  hooks.rs      hook receiver and the generated settings file
  spawn.rs      session / worktree / process spawning, and worktree_setup
  health.rs     a managed process's output → health
  diff.rs       numstat, hunk parsing, word-level LCS
  edit.rs       file read/write with containment and conflict detection
  git.rs        status parsing, refs, unpushed, worktree ops, the review writes
  review_commit.rs  which commit a batch's work may be folded into
  forge/        the Forge seam: trait + dispatch (mod.rs), agnostic model
                (model.rs), the GitHub impl (github.rs, github_write.rs)
  triage.rs     the triage run, and the gates a worktree must pass first
  proposal.rs   what triage proposes: Stance × Mode, positions, patches, stories
  post.rs       the review batch end to end
  patch.rs      applying and committing what you approved, with staleness checks
  prompt.rs     rendering the vendored prompts in commands/
  story.rs      filing a tracker story for a fair-but-out-of-scope point
  env_source/   where a session's own variables come from: trait + dispatch
                (mod.rs), mise.rs, direnv.rs
  reviews.rs    review queue: runs reviews_command, parses JSON, degraded states
  fix_pr.rs     automation state, the fix-pr guard table, a run's verdict
  agent_update.rs   is Claude Code behind (via mise), and the one-click upgrade
  worktree.rs   teardown preflight, archive, revive, removal
  store.rs      session record persistence, orphan reaping
  api.rs        HTTP surface and the origin/token guards      ws.rs  event stream + pty attach
  findings.rs   the live findings the daemon can see, written to daemon.log
  guard.rs      the git push rules, run by `orch guard push` as a PreToolUse hook
  machine.rs    what the daemon needs from the machine, warned about at boot
web/            the SPA (vanilla, xterm.js vendored) — one module graph under js/,
                booted by app.js; snapshot.d.ts is generated from the Rust structs
```

## Developing

```
mise install
npm install --prefix tools               # once per clone: check-web and shot need it
git config core.hooksPath .githooks      # once per clone
cargo test                               # the daemon
mise run check-web                       # type-check the SPA + its module graph
cargo run -p orchestrator-desktop        # the app, daemon embedded
mise run shot                            # screenshot the running app (drives Chrome)
mise run fixture                         # a throwaway PR to drive the review flow
```

`cargo run -p orchd -- --main /path/to/your/repo` runs the daemon headless on the
app's own config and prints a tokened URL. `mise run shot` drives Chrome while the
app runs in **WebKitGTK**, so it is good for layout and not the last word.

[`CLAUDE.md`](CLAUDE.md) has the traps, [`TODO.md`](TODO.md) what is open, and
[`docs/spec.md`](docs/spec.md) the requirements the `(§N)` comments point at.

Releases are CalVer (`year.month.n`): bump `Cargo.toml`, `desktop/Cargo.toml`,
`desktop/tauri.conf.json` and `Cargo.lock`, then tag `v<version>`. The workflow
refuses a tag that disagrees with the crate version.

## Licence

Copyright © 2026 Kars Barendrecht.

[AGPL-3.0-only](LICENSE). Use it, run it, change it. If you distribute it, or run
a modified version as a network service, the source has to go with it under the
same terms.

The network clause is not decoration here. In the desktop app it does nothing —
the daemon binds loopback and you are both the operator and the user, so the
obligation is to yourself. It has teeth in the **headless** mode, which binds a
port and prints a URL: point that at an interface your team can reach and it is a
network service, and this licence is what keeps a hosted variant open.

The vendored web assets are not ours: xterm.js, PrismJS and four font families,
all MIT or OFL-1.1. [`THIRD-PARTY.md`](THIRD-PARTY.md) lists each one with its
version and the notice its licence asks to travel with it.
