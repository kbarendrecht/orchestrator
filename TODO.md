# TODO

Hand-written, and it survives. The daemon's live findings used to be spliced into
this file, which churned it from every build; that feature is gone.

## Next

- **Make it run somewhere other than this machine.** The hardcoded assumptions are
  gone: the six stack-specific settings are `#[serde(default)]` values editable in
  the settings panel, `worktrees_subdir` makes the layout configurable, `docker` and
  `ng-watch` are `autostart:false` specs a fresh checkout never starts, the base ref
  is split out of `upstream_ref`, and `default_language` fills the prompts'
  `{{LANGUAGE}}`. Paths, `/proc` reads and GNU coreutils were the other half, and
  those rules are in CLAUDE.md.

  One decision comes before any transport, and `get-bb/bb` is where it is visible
  (`docs/system-overview.md`, `docs/multiple-devices.md`): it separates **where the
  state lives** from **where the work runs** — one always-on server owning the
  database, daemons enrolled per machine, a machine chosen per thread. orchd's rule
  today is the opposite and deliberate: a checkout's daemon owns that checkout's
  state. Running work on a second machine settles that question first, because the
  answer decides whether the child daemon keeps its stores at all.

  What is left is three seams — the agent, the tracker and the forge:
  - **Worktree *creation* is decoupled; the session model is not.** The daemon cuts
    every tree itself now — `spawn_worktree_session` runs the repo's own
    `WorktreeCreate` through `create_worktree` and adopts it, with no `--worktree`
    arm left. But the session still spawns `claude`, and the real coupling is
    untouched: `--session-id` correlation, the transcript slug, the `ai-title`
    field, `--resume`, and the whole hook-observer plumbing. Hosting another agent
    means abstracting *that*.

    `get-bb/bb` has already cut this seam for three agents, and its
    `docs/provider-plugin-api.md` is a ready-made checklist of what the abstraction
    has to name: a **capability handshake** at session start (does this agent
    restore a session, fork one, enforce approvals, accept skills), **session
    verbs** (start, resume, fork, stop, archive, name) and **turn verbs** (start,
    and steer a turn already running). Two of those orchd has no verb for at all —
    fork, and steer as something other than typing into the pty — so the checklist
    is worth reading before the trait is written rather than after. The half that
    does **not** transfer is bb's typed recovery events (`authRequired`,
    `rateLimited`): bb parses them because it renders a timeline, and orchd shows
    the pty, where the person already reads that text. orchd matches no agent error
    string anywhere today, and importing the typing would buy a parser and nothing
    else.

    **`stablyai/orca` names the two pieces bb's checklist leaves out, and both are
    things orchd currently gets from Claude Code for free.**

    *Status has to arrive from somewhere, and hooks are Claude Code's answer, not
    every agent's.* Orca takes agent status down three paths that converge on one
    write (`docs/reference/agent-status-store.md`): hook HTTP posts, relay
    observations, and **an `OSC 9999` escape sequence parsed straight out of the
    pty**. That third one is the interesting one here — it needs nothing of the
    agent but that it print, and orchd is already reading every byte of that pty
    into a ring buffer. It is the fallback for an agent with no hook system at all.
    Two rules Orca states are worth taking with it: one store with one authority
    id, and **precedence decided at write time with provenance on the row**, not
    re-adjudicated by each reader.

    *Skills reach each agent differently, and `--plugin-dir` has no counterpart.*
    `docs/reference/agent-skill-provider-paths.md` gives Codex `$HOME/.agents/skills`
    plus `.agents/skills` at the repository root, a plain copy; Claude Code gets
    `.claude/skills` searched hierarchically from the launch directory upward, laid
    down as a relative symlink (a junction on Windows, a verified copy where
    neither works). `skills::VENDORED` and the one `--plugin-dir` flag are a Claude
    Code fact, so the seam has to be "put this skill where *this* agent looks",
    with the placement mechanism per agent — not one flag with the agent's name
    swapped.
  - **The tracker config has two fields nothing reads, and it leans on the repo.**
    `config::Tracker`'s `host` and `stub` were the daemon's story filer's, which went
    with the overlay review; dropping them is a config-format change, and
    `migrate.rs` is where it belongs. What is left is real: `hooks::write_settings`
    approves the server through `enabledMcpjsonServers` and the daemon pushes the
    credential in under `token_env`, so `handle-review` can file a story only where
    the repo's `.mcp.json` happens to declare a server by that name, plus a tracker
    skill of its own. The daemon warns at boot about a missing *token* and about an
    undeclared server; it cannot see a missing tracker skill.
  - **GitHub-shaped leaks** for a real second forge: `GitHubForge::detect`'s URL
    parsing and the read-token ladder are github.com-specific — `for_kind`'s single
    `token` argument does not yet model per-forge credentials.

- **Sign in to GitHub from the app, and the daemon stops borrowing `gh`'s
  token.** A person who installs orchd should sign in once in the window, not set
  an environment variable or install somebody else's CLI. Today the daemon's read
  token comes from a ladder in `forge::resolve_token`: `ORCHD_GITHUB_TOKEN`, then
  `github_token_file`, then `gh auth token`. The first does not reach an app
  started from a launcher — `desktop/src/login_path.rs` adopts the login shell's
  `PATH` and nothing else — so on a desktop the choice is really the file or `gh`.

  **The shape is GitHub's device flow, through a GitHub App.** It is what
  `gh auth login` does itself: `POST github.com/login/device/code` with a public
  client id and no secret, show the user code and a link to
  `github.com/login/device`, poll for the token, keep it in the OS keychain
  (Keychain on macOS, the Secret Service on Linux). Two endpoints and a polling
  loop over the `curl` transport the reads already use, plus one screen.

  **A GitHub App, not an OAuth App, and that is the decision rather than a
  detail.** An OAuth App's scopes are coarse: reading a private repo's PRs needs
  `repo`, which is read *and write* to every repo you can reach, and that ends the
  read-only property `docs/security.md` rests on. A GitHub App asks for
  pull requests, checks and contents, read only. What it costs:
  - Its user tokens expire after 8 hours, so the daemon needs a refresh path (the
    refresh token lasts 6 months) and a re-sign-in when that lapses.
  - It has to be installed on the org that owns the repo, which at a company org
    may need an admin's approval. An OAuth App can hit the same wall through an
    org's OAuth access policy, so this is not a reason to pick one.
  - Somebody has to register it and enable device flow on it, once, by hand.

  **What it does not remove.** The *agent* still needs `gh`: `handle-review` posts
  replies through the agent's own `gh`, and pushes go through the agent's git
  credentials. Dropping that would mean handing agents a write token, in the
  process that reads strangers' review comments — the credential the security
  design keeps out of them. So `gh` stays the skills' requirement, and the README
  should say so in those words.

  **Where the ladder ends up.** The app's own sign-in first; `ORCHD_GITHUB_TOKEN`
  kept as the escape hatch for a headless or CI daemon; `gh auth token` kept as a
  silent fallback for people who already have it. `github_token_file` goes —
  except as the keyring's fallback on a Linux box with no Secret Service, where
  the app writes a `0600` file of its own rather than asking you to.

  Until this is built the token file stays: it is the one way a launcher-started
  app without `gh` gets a token.

- **The diff and the thread's comments, drawn on a review question.** The overlay
  review — `handle review in UI`, its cards, its state machine, the daemon-side
  posting and the story filer — is deleted. `handle review` in a pane is the one
  review verb, and it stays exactly as it is until something better exists.

  What the overlay was right about is still open: the context belongs with the
  question being asked, not in a surface you switch to. The seam is
  `renderInteraction` in `web/app.js`, which draws the `.oq` box for a session's
  ask, and that box can already carry a diff (`detail`). The pure diff rendering
  the cards used, `review-diff.js`, is in git history from before the removal.

- **Codex: two things are still unmeasured.** Measured on 0.162.1 with `codex`
  from mise, driven in a real pty with a dummy API key and the daemon's hooks trusted the way `codex::flags` trusts them.
  A dummy key is enough to measure a failing turn, but not a successful one, and
  that leaves two gaps:

  - **Whether a turn that completes fires `Stop`.** The rail's "your turn" for a
    Codex session rests on it, and nothing has shown it yet. Measure it with a real
    key before a Codex session's state is trusted the way a Claude one's is.
  - **`hooks::watch_turn_end` has not met a live Codex through the daemon.** It
    reads a failed turn off the rollout, because Codex fires no hook for one (a 401
    after five reconnects sent nothing). Its test drives the handler against lines
    shaped like 0.162.1's, so a rollout format change would pass the test and miss
    in use.

  **They need a Codex login, and this machine has none** — no `auth.json` in
  `~/.codex` and no `OPENAI_API_KEY` — so they wait for one. The probe below runs
  with a real key in the same shape: put it in a scratch `CODEX_HOME` with
  `codex login --with-api-key`, never in the user's own.

  The `/new` picker that used to be the third gap is closed: `codex::flags` turns
  Codex's own worktrees off, which removes the picker, and `fake-codex.mjs` refuses
  a start without that, so the e2e flow fails if it is dropped.

  Re-measure after a Codex upgrade before relying on any of this. The probe that
  measured it is a `pty.fork` that has to answer Codex's terminal queries (cursor
  position, OSC 10/11 colours, the kitty `?u` keyboard query and DA), and a fresh
  `CODEX_HOME` needs `[projects."<path>"] trust_level = "trusted"` in its
  `config.toml` or it stops on "Trust this folder?". That probe is the capture
  harness the next entry describes, in its smallest form. **It also has to set
  the pty's size** (`TIOCSWINSZ`, 40×140 is enough): at the default 0×0 Codex
  draws nothing past its first keyboard query, and a probe reads that as a hang.

- **Record real agent screens, before there is a second agent to record.** orchd
  parses agent pty bytes in three places already — `agent_complaint` reads the ring
  buffer to turn a fast non-zero exit into a sentence (`spawn.rs`), `is_interrupt`
  classifies keystrokes (`ws.rs`), and `health.rs` strips ANSI to
  reach a verdict — and every fixture behind them is a byte string **typed by hand
  into a test**. That is affordable for one agent whose screens are known. It stops
  being affordable at the second, and the roadmap has more.

  `stablyai/orca` built the harness for this and wrote down what it cost
  (`docs/reference/agent-pty-transcript-capture.md`). The shape:

  - A capture command spawns the agent **in a real pty** and records every byte —
    escape sequences and carriage returns included — while mirroring the session to
    your terminal so you drive it by hand. `Ctrl+]` ends the capture, chosen so it
    does not dismiss whatever dialog is on screen.
  - `--cols`/`--rows` pin the pty size, because **where the agent wraps is part of
    the evidence**. A fixture recorded at a different width tests a different
    string.
  - `--duration` for an unattended screen, `--send "<ms>:<text>"` to type at a
    timestamp, `--note` for the account type and CLI version.
  - The bytes land unchanged beside the tests, with a sibling `.meta.json` holding
    the timestamp, the platform, the command, the pty size and the exit code — so a
    fixture that stops matching can be dated and re-cut rather than argued about.
  - **Scrubbing is mandatory and same-length.** Tokens, hostnames, usernames,
    absolute paths and branch names come out, replaced by placeholders of the same
    width so the wrapping the fixture exists to prove survives the redaction.

  The gotcha Orca records is the one worth having in advance: their early fixtures
  were **pasted from a rendered terminal**, so they carry no escape bytes and no
  carriage returns. Those pass a word-match rule and prove nothing about a parser.
  orchd's hand-typed strings are the same class of fixture, and `agent_complaint`'s
  is the closest to real precisely because somebody typed the `\x1b[2J\x1b[H`
  prefix in by hand.

  Two orchd-specific notes for whoever builds it. The capture belongs beside the
  suite as a `mise` task, not as a test — it needs a human driving a real agent, so
  what CI gets is the recorded file, not the recording. And the platform matters
  the way `docs/traps/macos.md` says it does: a screen recorded on Linux is not
  evidence about the same agent under WebKitGTK's pty on a Mac.

- **An image differ.** The file pane can show one file
  and the diff can show one changed file, and neither has an answer for a `.png`:
  `edit::read` refuses a binary file rather than mangling it, and `git diff`
  reports `-` for both counts on one, which the parser surfaces as `binary`. So
  today a changed screenshot is a row that says so and nothing else — in a repo
  whose agents write screenshots, that is the one file type you most want to look
  at without leaving the window.

  Showing one image is done (`/api/file/image`, painted by `viewer.js`).

  **Differing one** is the part with a design question rather than a mechanism.
  Side by side is the least surprising, a slider or an onion-skin reads better for
  a layout change, and a pixel delta answers "did anything move" without a person
  comparing two pictures — they are three different questions and the pane should
  probably not try to answer all three at once. What the app already has to build
  on: the file pane's mode toggle is the natural place for it (source, rendered,
  and image would be the third), `viewer.js` owns "one file, one picture" now, and
  the diff's own base-revision read (`/api/file?base=…`) is already the way to get
  the *other* version of a changed file.

  Worth doing when a screenshot is the thing being reviewed. Recorded now because
  the file pane landed today and this is the first thing it cannot show.

- **Deferred: setup can fail and say nothing.** `worktree_init` and
  `worktree_setup` are non-fatal, the second runs even after the first failed, and
  `env_source` failures are silent by design — a degraded session beats a lost one,
  and that stays. What is missing is the *report*: a session whose setup did not
  finish then misbehaves with nothing on screen to explain it. `get-bb/bb` splits
  the two (`docs/environment-provisioning.md`): creation failure is terminal and
  loud, setup failure leaves a usable workspace and is shown as retryable, with no
  automatic retry ladder. That split is the part worth copying — the policy, not
  the state machine.

  **Demoted again on 2026-10-16, because no checkout reaches this path.** It was
  promoted on the `hdiutil -quiet` release-gate failure, which has the same shape
  but is another subsystem. Measured against the two live checkouts:
  `worktree_init` and `worktree_setup` are empty in both. The monorepo cuts
  through its own `WorktreeCreate`, which fails the create loudly, and does its
  setup in a link hook on Claude Code's `SessionStart` — a hook the daemon never
  sees, so a report built here would not catch that failure either. The trigger
  this entry waits for — a session traced back to a setup step that failed
  quietly — has still not fired.

  The earlier note here, that nothing surfaces a setup failure at all, was wrong.
  A loud cut does: `run_worktree_hook` calls `create_failed`, and the starting
  overlay turns `failed` (`renderStarting` in `web/app.js`). The real gap is
  narrower, and it is three things:
  - The mark lives on `CreateRun`, so it goes when the overlay goes — the moment
    the session boots, which is before anybody has read it.
  - A spare is cut with `Board::Quiet`, so its hooks failing reach only
    `orchd.log`, and claiming the spare carries nothing forward.
  - `spare::refresh` re-runs `worktree_setup` through `relink`, also quietly.

  The shape when it becomes real: a `setup_failed` on the workspace record (the
  step, the exit code and the stderr tail), set by quiet and loud runs alike and
  cleared by the next successful one; the rail row and the context bar saying so;
  and one route that re-runs `run_worktree_hooks` for that workspace, by hand
  only. It becomes real when a checkout configures `worktree_setup`, or when a
  repo's setup moves from `SessionStart` into the daemon.

- **Deferred: a restart of the daemon does not have to kill the terminals.**
  `mise run app-check` asserts that a session survives a restart, and it survives
  by being *resumed* — the pty dies with the daemon, `spawn::Carried` rebuilds the
  record, and `auto_resume` starts the agent again 1,200 ms apart. `stablyai/orca`
  does not resume, because nothing died (`docs/reference/orcad-operations.md`): the
  runtime forks a **separate terminal daemon**, detached, owning every local pty,
  with its own socket and its own pid file, and a new runtime **adopts the existing
  daemon** rather than replacing it. Their sentence for the rule is the useful
  part: *code freshness always defers to live work*.

  That is a real architectural difference and a large change, so it is recorded
  rather than proposed. What makes it worth recording is that orchd's update path
  wants exactly this property — an update today interrupts every running agent, and
  resume is the compensation. One smaller thing from the same document needs no
  such change and may be worth taking on its own: orcad distinguishes **exit code
  78, a configuration fault, do not restart** from exit 1, retry with backoff.
  orchd's host restarts a dead child once, so a config fault spends that restart
  achieving nothing — about 1.3 s, after which `retried` stops further attempts and
  the row stays for `reopen`. Small, and that is the honest size of it.

  **The second one is already here, by another route.** This used to also ask for
  orcad's bound of five launches in a rolling 60 seconds. `host::HEALTHY_UPTIME` is
  that guard: 60 seconds of life is what clears `retried`, so a daemon dying *of
  starting* gets one retry and no more, while one that ran and then died gets its
  retry back. The comment there records the version that cleared `retried` on every
  successful start instead, which meant the bound never bit and a daemon dying on
  boot restarted forever. Leaving the ask in invited somebody to build a guard that
  exists.
