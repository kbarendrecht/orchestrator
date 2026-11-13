# TODO

Hand-written, and it survives. The daemon's live findings used to be spliced into
this file, which churned it from every build; that feature is gone.

## Next

- **orchd on a phone, through Happy.** Much of the agent work around this project
  is driven from a phone, through Happy, and orchd is a window on the desktop. So an
  agent blocked on a question waits until somebody is back at the machine. This
  entry used to stop at the transport: the daemon listens on loopback, and the
  page's token can spawn a pty, so whatever carries it carries a shell.

  **Happy answers the transport, and it moves no new trust.** It is three parts: the
  app, a relay that stores and routes only ciphertext, and a client next to the
  agent that encrypts before anything leaves. The relay is self-hostable and one
  already runs. Whoever holds `~/.happy/access.key` can already start an agent on
  this machine through Happy's own daemon, so orchd as a second client of the same
  key gives that key nothing it did not have. The page's token stays on loopback.

  **The client is the host, not each child.** Closing the window stops the host,
  the host stops each child, and each child stops its sessions, so a client in the
  children survives nothing the host does not. The host holds one connection, one
  key and one machine in the app, and the children's transcripts and ptys reach it
  through `child.rs`.

  **The protocol is reachable from Rust or from `happy/lib`.** The `happy` npm
  package (MIT) exports `ApiClient`: `getOrCreateSession({tag, metadata, state})`,
  and per session `sendClaudeSessionMessageFromLocalTranscript` (a transcript line
  out), `onUserMessage` (a phone message in), `keepAlive(thinking)`,
  `updateAgentState`, `sendSessionDeath`, plus a push client with the kinds `done`,
  `permission` and `question`. Underneath it is REST for auth and sessions,
  Socket.IO for realtime, and NaCl secretbox or AES-256-GCM with wrapped per-session
  keys. That library changed between 1.2.5 and 1.2.6 and the shipped app may not
  depend on `node` (a launcher-started app finds one too old), so the shipped
  client is Rust carrying only what a spike proves necessary, and `happy/lib` is
  the spike's tool and the reference. The cost of either is the same and is the
  real one: orchd follows a protocol it does not own, and the side that moves is
  the phone app, which updates from the store and breaks a copy silently.

  **Two competitors were read, and both confirm the order.**
  - `stablyai/orca` ships its own app and relay (`cloud/`). Direct mode is a
    WebSocket to the desktop on port 6768 over LAN or Tailscale, paired by a QR code
    carrying the endpoint, a device token and a TLS fingerprint. The phone has a
    terminal view and a chat view built from the transcript, and can reply, start
    an agent and create a workspace. **"The paired desktop owns the agent
    sessions"**: closing the desktop app drops the phone. Mobile is Beta.
  - `herdrdev/herdr` has no relay and no account. A background server owns the
    panes and survives every client, and phones reach it over SSH, Tailscale
    included. Its socket API is newline-delimited JSON (`agent.read`,
    `agent.prompt`, `events.subscribe`), with the states `idle`, `working`,
    `blocked` and `done` — idle and not yet seen. **`agent.prompt` refuses a
    blocked agent** with `agent_blocked` and sends nothing. The phone clients
    others built on it (Heeler and the rest) all lead with an inbox, blocked first,
    and a push on blocked or done.

  **The first version works while the window is open**, which is Orca's shape:
  - A setting, off by default, reading the key and the `serverUrl` from `~/.happy`.
  - Each live session is one Happy session, carrying its name, path and branch.
  - Transcript lines go out; a phone message is typed into the pty.
  - Busy or idle comes from the state machine.
  - **A message to a blocked session is refused, and the phone is told why.** Text
    typed into a permission prompt or the ask box answers it, which is Herdr's
    rule and the reason for it.
  - A push when a session goes blocked or done, because that is what both
    products treat as the point of the phone.
  - A stopped session ends its Happy session.

  Not in it: starting a session from the phone (machine registration and
  `spawnSession`, which the host can route because it knows every checkout),
  answering a permission prompt or a question from the phone (an answer has to
  become keystrokes in the pty), images, and running with the window closed.

  **Order:** a throwaway Node spike against the relay, using `happy/lib` on one
  existing session, to measure what the code cannot say — which message shape the
  installed app renders, whether it shows a session with no registered machine,
  and which events are the minimum. Then the Rust client in the host, tested
  against a fake relay for the encryption and the framing.

  **Later, the window.** Herdr is what surviving the window costs: a server
  process apart from the UI. Here that is the host as a background service the app
  attaches to, since `orchd --host` already is a host without a window — the same
  property the entry on a daemon restart wants. A smaller step first would be a
  close that hides the window and a quit that stops the host. Either one also
  depends on the machine staying up, and whether WSL keeps a distro alive with
  no window open is unmeasured.

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

- **A session that opens several PRs: what is left.** This replaces the
  stacked-PR entry. Stacking is not orchd's job: the agents cut a branch off their
  own PR and restack it well enough, and a `stack` action plus a `/restack` skill
  would be the daemon doing their work. **The exception, since November, is the
  restack with no decision in it** (`crates/orchd/src/restack.rs`): the branch
  under an idle, clean, unpushed one only grew, and which is under which was proved
  by containment, so `git rebase --onto <new> <old>` replays its own commits, after
  a backup ref. Nothing else moves by itself, and nothing is asked: a stack is
  said to each agent before it pushes (`crates/orchd/src/push.rs`), where its
  branch sits, what to restack, what a rewrite does to the branches above it, a
  base behind upstream, the `--base` of a stacked PR, and whose PR goes first in a
  conflict (`Progress`). The Stacked card's buttons do the rest when you ask. What orchd owns is
  *showing* what an agent did. A session now keeps every branch it held and every PR opened on them
  (`Session::branches`, `Session::prs`), the rail names the worst PR with a count
  of the rest, and the pane header draws one chip per PR. A PR that ended while
  the daemon was down is asked about once per run, by number or by its branch.
  Options and reasoning: https://claude.ai/artifact/QXAB6HmLZRAAGLupnYd7He. Two
  things are not done:
  - **Main with sharing on credits a branch to every live session in it.** The
    reconcile cannot tell which session moved the tree, so two conversations in
    main each get the other's PRs. Crediting the one that was mid-turn when the
    branch changed is the likely fix, once the turn timing is known to be reliable.
  - **The PR pane could group by session** and show the stack as an indent from
    `base_ref` (option C on the page). Worth it only if the flat list starts to
    hide which PRs belong together.

- **Deferred: a checkout with no forge has no way to land work.** A repo with no
  remote is based on main's own branch now (`git::local_base`), so worktrees cut
  and diff the way they do anywhere. What it lacks is the step a merged PR is
  everywhere else: nothing moves a worktree's commits onto the base. The agent's
  push guard refuses git that changes main from a worktree, the swap refuses to
  hand the base out (`relocate::swap_with_main_inner`), and teardown keeps every
  tree whose commits are not on the base yet — so today the work lands only by
  hand, in main. Two shapes when it is needed: a row-menu `merge into base` that
  fast-forwards or merges in main, refused when main is dirty or occupied; or a
  skill a session in main runs. Built when a no-forge checkout is actually used,
  not before.

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
