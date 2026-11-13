# TODO

Hand-written, and it survives. The daemon's live findings used to be spliced into
this file, which churned it from every build; that feature is gone.

## Next

- **orchd on a phone: its own PWA, over a Happy relay.** Much of the agent work
  around this project is driven from a phone, and orchd is a window on the desktop,
  so an agent blocked on a question waits until somebody is back at the machine.
  The decision is made: **orchd's own PWA**, showing what only orchd has (the rail,
  the ask box, the PR rows), reaching the host **through a Happy relay**, and
  working away from home and for other people. What follows is what was measured
  and read on the way, so the next session starts from it rather than from the
  question.

  **Why a relay, and why Happy's.** Both ends dial out and the relay sees only
  ciphertext, so there is no inbound port — which matters, because the daemon
  listens on loopback inside WSL2, behind NAT, and the page's token can spawn a
  pty, so anything that carried it to a phone would carry a shell. Happy already
  is that relay, MIT, self-hostable (`happy server`, the `happy-server-self-host`
  package), with a public instance beside it. A tailnet straight to the SPA was the
  alternative, and it was turned down on both counts: the shell, and the network
  between WSL, Windows and the phone.

  **The client is the host, not each child.** Closing the window stops the host,
  the host stops each child, and each child stops its sessions, so a client in the
  children survives nothing the host does not. The host holds one connection, one
  key and one machine, and reaches the children's transcripts and ptys through
  `child.rs`. **The first version works while the window is open** — Orca's shape
  (below). Surviving the window is Herdr's, and comes later.

  **A spike proved the transport (2026-11-13, against a self-hosted relay).** A
  Node script on `happy/lib` stood in for the host, with `claude` in a tmux pane
  standing in for a session's pty:
  - A session with no registered machine is accepted, and the app shows it.
  - Transcript lines go out with `sendClaudeSessionMessageFromLocalTranscript`
    after `RawJSONLinesSchema` filters them; Claude Code's internal lines
    (`attachment`, `mode`, `ai-title`, `file-history-snapshot`, …) do not parse
    and are dropped, as Happy's own client drops them.
  - A message sent from the Android app arrived through `onUserMessage`, was typed
    into the pane, and the answer came back. The app attaches an
    `appendSystemPrompt` of its own to every message; typing only the text drops it.
  - **Encrypted RPC works end to end.** The session registered `orchd.snapshot`; a
    second, user-scoped client called it with `rpc-call` on
    `<session id>:orchd.snapshot`, AES-256-GCM both ways, **38 ms** round trip.
    That is the PWA's whole transport: the host registers the methods, the PWA
    calls them.
  - CORS on both the self-hosted and the public relay
    (`api.cluster-fluster.com`) is `Access-Control-Allow-Origin: *`, for the REST
    routes and for Socket.IO's `/v1/updates`. A PWA served from anywhere can use
    either.

  **What the spike found that the design must carry.**
  - **A restart must not replay.** A new tag made a new session, which sent the
    whole transcript again and a second "done" push. The host remembers, per
    orchd session, the Happy session id and the transcript offset.
  - **An ended session cannot be reopened by its tag.** `getOrCreateSession`
    returned it with null metadata, and `happy/lib` crashed on it.
  - **`happy/lib` registers `bash`, `readFile` and `writeFile` RPCs on every
    session** (`registerCommonHandlers`), so any client of the account gets a shell
    in that directory. orchd registers only the methods it chooses — a reason, on
    its own, to write the client rather than wrap the library.
  - **A push's title and body reach the relay in plain text**, and Expo after it.
    The text must not carry a session name or a prompt.
  - `happy/lib` changed between 1.2.5 and 1.2.6, and the shipped app may not
    depend on `node` (a launcher-started app finds one too old), so the host's
    client is Rust.

  **Identity and pairing, read from `slopus/happy` at `f74aed1`.** The repo's own
  `docs/encryption.md`, `api.md` and `protocol.md` cover the wire, and
  `packages/happy-android/crypto/` (about 870 lines of Kotlin, each file naming the
  TypeScript it mirrors) is a second implementation to port from.
  - An account **is** a random 32-byte master secret. An Ed25519 key derived from
    it signs a challenge for `POST /v1/auth`, which upserts the account by public
    key and returns a JWT. A PWA can therefore create an account on its own, with
    no Happy app.
  - `deriveKey(secret, "Happy EnCoder", ["content"])` (an HMAC-SHA512 key tree)
    seeds the content box keypair, and every session and machine key is wrapped
    to its public half as `0x00 ‖ ephemeral pk ‖ nonce ‖ box`.
  - **Linking a device** (`/v1/auth/account/request`): the new device shows
    `happy:///account?<box pk>`, a device holding the secret seals it to that key.
    The PWA uses this to join an account the Happy app already has.
  - **Pairing a terminal, v2** (`/v1/auth/request` with `supportsV2`): the approver
    seals `0x00 ‖ contentPublicKey`, and the terminal keeps that plus a random
    `machineKey` — it can wrap keys for the account but read nothing else. **The
    host pairs this way and keeps its own key in its config dir**, rather than
    reading `~/.happy`, so orchd works for people without the Happy CLI.
  - The PWA, by contrast, holds the master secret (in IndexedDB), so whatever runs
    script on its origin reads everything: no third-party scripts, a strict CSP.
  - The secret's human form is base32 in groups of five, the "secret key backup".
  - **Upstream has dropped its web client.** `happy-app` (Expo) is gone from
    `main`, leaving native iOS and Android; a relay that still serves one serves an
    old build. That is one more reason for a PWA of our own.

  **Other people, and the public relay.** Happy's Terms of Use (15 July 2026) are
  the only terms; they cover the software, call it MIT and "a third-party client
  that works with publicly available APIs", and say nothing about other clients on
  the hosted relay, quotas or availability. No rate limiting was found in the
  server source. One clause needs respecting by being open about it: *"must not
  attempt to reverse engineer security features or encryption protocols."*
  Implementing the protocol from the MIT source and its own docs is not that, but
  it is near enough that the maintainers should hear it from us. So: **no default
  relay** — the first run asks, offering a self-hosted one and the public one —
  and an issue on `slopus/happy` asking whether a third-party client on their
  relay is welcome, and within what limits, **before** the public one is offered.

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

  **The first version**, with the window open:
  - The host pairs with a relay (v2, QR in the window) and registers one machine.
  - It registers RPC methods for the PWA: a snapshot of the rail, the open
    questions and the PR rows, and the verbs to answer one and to send a line.
  - **A line sent to a blocked session is refused, and the phone is told why.**
    Text typed into a permission prompt or the ask box answers it — Herdr's rule,
    for Herdr's reason.
  - A push when a session goes blocked or done, with neutral text.
  - The PWA: link to an account (or create one), list, answer, send.
  - Each live session is also a Happy session, so the Happy app sees the
    transcript too. That half is cheap, and the spike already did it.

  **The permission prompt, measured on Claude Code 2.1.318 (2026-11-13).** A
  session in `default` mode was asked for a `touch` and for a `Write`, with every
  hook recorded and the keys sent through tmux:
  - **`PermissionRequest` fires for Claude Code too**, not only for Codex, and it
    is the one that carries the content: `tool_name`, `tool_input` (`command` and
    `description` for Bash, `file_path` and `content` for Write) and
    `permission_suggestions`. `Notification` says only "Claude needs your
    permission" with `notification_type: permission_prompt`. Both carry the same
    `prompt_id`, which is the join. So the card's text comes from
    `PermissionRequest`, which orchd today registers only for Codex.
  - **Allow is `1`**, with no Enter, on every menu seen: option 1 is always "Yes".
  - **The menu is not fixed.** Bash offered four options (Yes, always allow the
    directory, switch to auto mode, No) and Write three (Yes, switch to accept
    edits, No). "No" is always last, and its number varies.
  - **Deny is `Esc`** (or the "No" number), and it **interrupts the turn**:
    "Interrupted · What should Claude do instead?", and no hook fires at all — no
    `Stop`, no `PostToolUse`. The host knows only because it sent the key.
  - **Deny with a reason does not interrupt.** Moving to "No", `Tab` ("Tab to
    amend"), the text, `Enter`: Claude read the reason and carried on in the same
    turn, asking again for the amended command. Reaching "No" means knowing how
    many options the menu has.
  - orchd has no terminal parser (no `vt100` or `alacritty_terminal`), so "is the
    prompt still on screen" cannot be read from the pty today. A prompt answered at
    the desk is followed by `PostToolUse` (allowed) or by nothing (denied), so the
    hooks alone leave a stale prompt after a deny at the desk. A stale "Allow" then
    types a bare `1` into the prompt box — not submitted, but left there.

  So the v1 verbs: **Allow** sends `1`. **Deny** sends `Esc`, and a reason, if
  given, follows as the next line, which costs an interrupted turn and needs no
  screen. Deny-and-continue (`Tab` to amend) waits on either a screen parser or a
  reliable option count, and so does a guard against the stale prompt.

  **A screen parser was evaluated, and it is cheap enough (2026-11-13).** Raw pty
  bytes of real prompts (tmux `pipe-pane`, 140x40) fed into `vt100` 0.18.1 in a
  scratch crate:
  - It reads both menus exactly — Bash's four options and Write's three, "No" last
    — and the same in `CLAUDE_CODE_NO_FLICKER` mode, on the alternate screen.
  - After the prompt was answered the scan found no menu, so it does say whether a
    prompt is still up, which is the guard the hooks cannot give.
  - 85–88 MiB/s through Claude Code's own escape-heavy output, and 18 µs to read
    the screen and scan it. A pty never streams near either.
  - MIT, and four new crates (`vt100`, `vte`, `unicode-width`, `arrayvec`), all
    on `deny.toml`'s licence list.
  - The seam exists: `pty.rs` already pushes every read into `RingBuffer` and holds
    the size `PtyHandle::resize` sets, so the parser sits beside the ring, fed in
    the same place and resized in the same call. Only agent ptys need one.

  The cost is the dependency on Claude Code's wording ("Do you want", "N. No",
  "Esc to cancel"), which is UI and not contract. So the scan is tested against
  the recorded bytes as fixtures, and anything it does not recognise falls back to
  the v1 verbs (`1`, `Esc`) rather than guessing an option number. With it,
  deny-with-a-reason becomes "No", `Tab`, text, `Enter` — no interrupted turn —
  and a stale prompt is refused instead of typed into.

  Not in it: starting a session from the phone (`spawnSession` on the machine,
  which the host can route because it knows every checkout), images, and running
  with the window closed.

  **Later, the window.** Herdr is what surviving it costs: a server process apart
  from the UI. Here that is the host as a background service the app attaches to,
  since `orchd --host` already is a host without a window — the same property the
  entry on a daemon restart wants. A smaller step first would be a close that
  hides the window and a quit that stops the host. Either one depends on the
  machine staying up, and whether WSL keeps a distro alive with no window open is
  unmeasured.

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
