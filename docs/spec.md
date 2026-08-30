<!-- Vendored from the design document the code cites. Nearly every `(§N)` in
     this repo points here: §2 is the object model, §6b the review queue, §8 the
     automation rules, and so on. It is a requirements document written before
     and during the build, so it describes intent — where the code and this
     disagree, the code is what runs. -->

# Claude Code Orchestrator — Requirements

**Target:** Linux, single monorepo, single user. Triangular remotes: branches
track `upstream/develop` for fetch/pull, push goes to `origin` (fork) via
`remote.pushDefault`. Stack: a dockerized web monorepo with a compiled front end and a browser test
runner. Nothing below depends on that; it is the shape the defaults were sized
against.

**Shape:** Rust daemon + browser-served SPA. Shell (Tauri/Electron) deferred.

> The repo already owns a worktree system: `worktree-create` (WorktreeCreate),
> `worktree-link` (SessionStart), `worktree-edit-boundary` and `pre-bash`
> (PreToolUse). **The orchestrator layers on top of these and must not fight
> them.** See §11.

---

## 1. Architecture

```
┌──────────────────────────────────────────────────┐
│ SPA (localhost:7777)                             │
│  rail: sessions  │ xterm + build drawer │ files  │
│              WebSocket ▲  │ HTTP                 │
└──────────────────────────┼──┼────────────────────┘
┌──────────────────────────┴──┴────────────────────┐
│ orchd (Rust: tokio, axum, portable-pty)          │
│  pty host · hook receiver · git svc · PR poller  │
│  test-capability registry · path mapper          │
└──┬──────────┬──────────────┬─────────────────────┘
   │ ptys     │ HTTP hooks   │ GraphQL
   ▼          ▼              ▼
claude(1)  claude(1)      GitHub
```

Daemon owns all state. SPA is stateless and disposable; closing the browser
kills nothing. Reopening replays from the daemon's buffers.

Shell out to `git`, not a library binding — you need fsmonitor and the real
worktree/remote semantics.

---

## 2. Object model

### Workspace

A checkout. Exactly one is privileged.

```rust
struct Workspace {
    id: WorkspaceId,
    path: PathBuf,               // main, or <main>/.claude/worktrees/<name>
    kind: WorkspaceKind,
    branches: HashSet<String>,
    processes: Vec<Process>,
    occupant: Option<SessionId>, // main only: exclusivity mutex
    tree: Tree,                  // last reconcile: changed files, base, divergence
}

enum WorkspaceKind {
    Main,                            // docker stack, dev URL, ng build --watch
    Worktree { name: String },
}
```

Worktrees live **inside** main at `.claude/worktrees/<name>/`. Consequences:

- Main's file tree contains every worktree. Exclude `.claude/worktrees/` from
  main's changed-file view or you'll see sibling sessions' work.
- **New worktree sessions must be spawned with cwd = main checkout.** The
  `worktree-create` hook refuses to nest a worktree inside a worktree.

**Main is exclusive.** One Claude session at a time, enforced by a simple mutex.
No queue: while main is occupied, the UI disables "new session in main" and shows
which session holds it. Ending that session releases it. The dev URL is bound to
main, so occupancy *is* the lease — no separate mechanism.

### Session

```rust
struct Session {
    id: Uuid,                           // = $ORCH_SESSION_ID
    claude_session_id: Option<String>,
    workspace: WorkspaceId,
    state: State,
    kind: Kind,                         // Interactive | Automation { pr, skill }
    pty: PtyHandle,
    buffer: RingBuffer,
    transcript_path: Option<PathBuf>,   // ~/.claude/projects/<cwd-slug>/<uuid>.jsonl
    archived_transcript: Option<PathBuf>,
    recovery: Option<ArchiveState>,     // how to rebuild for --resume
}

enum State {
    Starting,
    Working,
    YourTurn { since: Instant, reason: TurnReason },
    BuildFailing,
    Error,
    Exited,
    Archived { resumable: bool },
}

enum TurnReason {
    TurnComplete,     // Stop — the common case under auto-accept
    AskedAQuestion,   // Notification / agent_needs_input
    NeedsPermission,  // Notification / permission_prompt — rare in auto mode
}
```

**`YourTurn` is the attention state, and `Stop` is its main source.** Running in
auto-accept mode, permission prompts almost never fire; what actually gates
progress is Claude finishing a turn and waiting for the next prompt. So a
completed turn is not a quiet success — it is an idle agent.

`since` is load-bearing. With 4–6 sessions, the cost of this whole tool is
measured in agent-minutes spent waiting on you, so the rail sorts `YourTurn`
sessions by wait time descending: the one that has been idle longest is the one
you are losing the most to.

There is deliberately **no acknowledge or dismiss action.** An earlier draft had
one, to stop finished turns accumulating as permanent ochre. It was wrong:
acknowledging a session does not make the agent less idle, so the only thing it
would achieve is hiding a real cost from the metric the rail exists to surface.

`YourTurn` clears exactly two ways — send the next prompt, or close the session.
Both change the underlying reality. Nothing merely marks it as read.

`BuildFailing`: a main-workspace session that reached `Stop` while `ng-watch` is
red is not waiting on a prompt — it is broken. Red outranks ochre.

### Process

Any non-Claude pty owned by a workspace. Same hosting as a session pty — ring
buffer, reattach — but no hook lifecycle and no rail entry.

```rust
struct Process {
    name: String,
    kind: ProcKind,
    cwd: PathBuf,              // always the owning workspace's path
    pty: PtyHandle,
    buffer: RingBuffer,
}

enum ProcKind {
    Managed { command: Vec<String>, health: Health, restart: Policy },
    Shell   { exit_code: Option<i32> },
}

enum Health { Starting, Ok, Failing { summary: String }, Dead }
```

**Managed** processes are declared per workspace in config. Main declares
`ng build --watch` and `docker compose up`; worktrees declare none by default.
Health is parsed from output — `ng-watch` matches Angular error blocks and the
first error line becomes the summary shown in the rail.

**Shell** is a plain `$SHELL` opened on demand in *any* workspace, always with
`cwd` set to that workspace's path — the same directory as the Claude session
above it. No health parsing, no restart policy, no rail entry. This is what makes
the drawer agnostic: it hosts whatever pty you point at it, and `ng-watch` is
just the one main happens to declare.

Shells are disposable. They die with the daemon and are not resumable; a dead
shell keeps its buffer and shows its exit code until dismissed.

> A shell opened in a worktree inherits the worktree's constraints. `docker
> compose` resolves the wrong project from there and is blocked by `pre-bash`;
> reach the shared stack with `docker exec <container>` instead (§7 rule 5).

### Session lifecycle

The daemon spawns every session; it never adopts a shell-started one. That is
what makes `$ORCH_SESSION_ID` injection and exact hook correlation possible.

**Where transcripts live.** Claude Code writes them to
`~/.claude/projects/<cwd-slug>/<session-uuid>.jsonl`, outside the repo. They
survive worktree teardown. But the directory is *keyed by working directory*, so
`--resume` only finds a session when invoked from a path whose slug matches the
one it was created under.

**Resuming a live worktree** is therefore trivial: relaunch
`claude --resume <claude_session_id>` with cwd set to the recorded worktree path.

**Resuming an archived session requires rebuilding the worktree first**, at the
identical absolute path. The daemon records `(worktree_name, branch, head_sha,
transcript_path)` at `SessionStart` and at teardown, then on resume:

1. Ensure the branch exists. If it was deleted after a merge, recreate it from
   the recorded `head_sha`, or from the PR head still on `origin`.
2. `git worktree add <main>/.claude/worktrees/<name> <branch>` — the repo's
   existing `worktree-create` and `worktree-link` hooks run as normal.
3. Compare the branch tip against the recorded `head_sha`. If they differ, say so
   and offer to check out the recorded commit instead — the transcript describes a
   working tree that no longer exists, and silently resuming onto a moved branch
   makes Claude reason about files it never saw.
4. `claude --resume <claude_session_id>` with cwd set to the rebuilt path.

```rust
enum ArchiveState {
    Recoverable { name: String, branch: String, head_sha: Oid },
    TranscriptOnly,   // branch gone and sha unreachable — read-only
}
```

`TranscriptOnly` is the genuine dead end: the branch is gone and the commit is
unreachable from any remote or reflog. The transcript is still readable; the
session simply cannot be continued.

> **Worktree names must be unique over time.** The projects directory is keyed by
> path, so a new worktree that reuses an archived worktree's name inherits that
> directory and interleaves the two sessions' transcripts. The daemon refuses to
> create a worktree whose name matches any archived session and suggests a
> suffix. This is the strongest argument for the daemon owning worktree creation
> rather than you running `claude -w` by hand.

Offer `--fork-session` alongside resume for the "same context, new direction"
case; it arrives as `source: "fork"`.

**Daemon restart.** The daemon owns every pty, so restarting it terminates every
Claude process. Ring buffers are in-memory and are not persisted — session
*records* are. On restart, every previously live session becomes `Archived` and
the rail offers resume, individually or all at once. Their worktrees still exist,
so these resume without a rebuild step.

### Worktree lifecycle

The daemon creates and tears down worktrees, delegating creation to the existing
`worktree-create` / `worktree-link` hooks (§11). Spawn with cwd = main.

**Teardown preflight — all must pass:**

| Check | Rule |
|---|---|
| No live session | no session in this workspace outside `Exited`/`Archived` |
| Clean tree | `git status --porcelain` empty |
| Nothing unpushed | see below — **never** `@{push}` or `@{u}` |
| Transcript copied | JSONL copied to daemon storage — the original in `~` survives teardown, but the copy protects against Claude Code pruning and against a later name collision |
| Recovery record written | `(name, branch, head_sha)` persisted so the worktree can be rebuilt for resume |
| Processes stopped | no Process still attached |

**Checking for unpushed work.** `@{push}` does not resolve on a branch that was
never pushed, and `@{u}` resolves to `upstream/develop` — neither answers the
question. Resolve the fork branch explicitly:

```bash
git rev-parse --verify --quiet refs/remotes/origin/<branch> \
  && git log origin/<branch>..HEAD --oneline \
  || echo "never pushed"
```

No remote counterpart means every commit is unpushed — block teardown. This is
precisely the case where losing commits is most likely, so it must fail closed.

**Removal is `git worktree remove <path>` followed by `git worktree prune`.**

> **Never `rm -rf` a worktree.** It contains `.plan/` symlinked to main's
> `.plan/`, and since the vendor change a `vendor/` full of per-package symlinks
> into main. A recursive delete that follows symlinks destroys the main
> checkout. If `git worktree remove` refuses, surface the refusal in the rail —
> do not escalate to `--force`, and do not fall back to a filesystem delete.

**Retention.** No automatic teardown on a timer. Teardown is *offered* in the
rail when the associated PR is merged or closed and the preflight passes.
Explicit action always required.

### Session ↔ PR

Many-to-many. A PR belongs to a session if its `headRefName` is in that
session's workspace branch set. Branches accumulate, never removed. One inotify
watch per workspace on `.git/HEAD`; no tree walks. No PR is a normal state.

---

## 3. State detection — hooks, not screen-scraping

| Event | Matcher | Daemon action |
|---|---|---|
| `SessionStart` | — | bind `claude_session_id`, `cwd`; return `sessionTitle` |
| `UserPromptSubmit` | — | → `Working` |
| `PostToolUse` | `Edit\|Write` | mark path dirty (see `.plan/` note, §4) |
| `Notification` | `agent_needs_input` | → `YourTurn { AskedAQuestion }` |
| `Notification` | `permission_prompt` | → `YourTurn { NeedsPermission }` — rare under auto-accept |
| `Notification` | `idle_prompt` | → `YourTurn { TurnComplete }` |
| `Stop` | — | → `YourTurn { TurnComplete }` or `BuildFailing`; git reconcile. **The primary attention event.** |
| `SubagentStop` | — | **no-op** — see below |
| `StopFailure` | — | → `Error`, carry error type |
| `SessionEnd` | — | → `Exited`, release main, retain buffer |

> **`SubagentStop` must never reach the state machine.** A `Task` call finishing
> mid-turn would flip the session to ochre while the main agent is still working,
> poisoning the one metric the rail exists for. Match `Stop` exactly, handle
> `SubagentStop` as an explicit no-op, and assert at spike time that a subagent
> emits only the latter.

**Correlation.** Launch with `ORCH_SESSION_ID=<uuid>`; hook headers interpolate
env vars declared in `allowedEnvVars`:

```json
{ "type": "http",
  "url": "http://127.0.0.1:7777/hooks/stop",
  "headers": { "X-Orch-Session": "$ORCH_SESSION_ID" },
  "allowedEnvVars": ["ORCH_SESSION_ID"],
  "timeout": 5 }
```

Exact mapping, no cwd/pid heuristics. Handlers are pure observers: they return
200 with an empty body and never block a turn.

**Where this config lives: a daemon-owned file passed with `--settings` at
spawn.** Not `~/.claude/settings.json` — global config would make every Claude
session on the machine POST to the daemon, including unrelated repos, and every
one of them would pay the hook timeout while the daemon is down. Not the
worktree's `.claude/settings.local.json` either, since `worktree-create` already
owns that file for `claudeMdExcludes`. A daemon-owned file at
`~/.config/orchd/hooks.json` avoids both, and only daemon-spawned sessions are
affected — which is all of them by design (§2).

> ⚠ **Verify at spike time whether `--settings` merges with project and user
> settings or replaces them.** If it replaces, the repo's own
> `worktree-edit-boundary` and `pre-bash` hooks would silently stop firing —
> far worse than the problem this solves. If it replaces, fall back to
> generating a merged file that includes the repo's hooks verbatim.

Set the hook `timeout` to **1s**. These are observers; a slow or dead daemon must
cost a turn as little as possible.

---

## 4. File model

**Primary signal: hooks.** `PostToolUse` on `Edit|Write` gives the exact path.
No filesystem watcher, no inotify pressure, no debounce tuning.

**Reconcile** on `Stop`, manual refresh, and ≤ once/30s while `Working` — catches
Bash-driven changes (codegen, builds, git ops) no Edit hook reported.

```bash
git status --porcelain=v2 --untracked-files=normal -z
```

**Repo config:**

```bash
git -C <main> config core.fsmonitor true      # main only — see below
git config core.untrackedCache true
git config fetch.writeCommitGraph true
```

**fsmonitor on main only.** Each worktree with `core.fsmonitor` enabled runs its
own daemon watching its whole tree; seven of them over a large monorepo puts the
`fs.inotify.max_user_watches` question right back. Main is where you work
interactively and where a snappy `git status` is felt. Worktree reconciles are
event-driven off hooks and run at most once per 30s, so they do not need it.

**No sparse-checkout.** `vendor`/`node_modules` are relative symlinks into main,
so a worktree costs tracked files only, and the existing hooks deliberately
avoid `worktree.sparsePaths`. Measure `git ls-files | wc -l` before revisiting.

**`.plan/` attribution.** `.plan/` in a worktree is a symlink to main's `.plan/`.
A `PostToolUse` reporting `.plan/plan_foo.md` from a worktree session must be
attributed to **main's** workspace, not the worktree's — otherwise it shows as a
phantom untracked file in the wrong pane. Resolve every hook path through
`realpath` before attributing it.

**Do not touch `$GIT_COMMON_DIR/info/exclude`.** It's shared across all
worktrees and `worktree-link` already manages it.

**Right pane:** changed files only, grouped staged / unstaged / untracked.
For main, exclude `.claude/worktrees/`.

---

## 5. Diff

### Base

Everything you've done on the branch, including uncommitted work:

```bash
BASE=$(git merge-base upstream/develop HEAD)
git diff --numstat $BASE
git diff $BASE -- <path>
```

Two-dot against the merge-base commit — not the ref — or `develop`'s own commits
appear as your deletions. This matches how `worktree-create` bases branches and
how `gh pr create` resolves the PR base, so the diff view and the PR agree.

Keep `upstream/develop` fresh or the merge-base drifts: piggyback
`git fetch upstream develop --no-tags` on the 5-minute poll; recompute only when
the ref moves.

Toggles: `vs upstream/develop` (default) · `vs HEAD` · `vs PR base`.

### Viewer

*From PhpStorm:* word-level intra-line highlighting (the biggest readability win
over GitHub's line granularity), connector ribbons between panes, synchronized
scroll, next/prev-change keybind, collapse unchanged regions with expand-on-
click, **editable right pane**.

*From GitHub:* whole-changeset file list with per-file status, unified/split
toggle, expand context beyond the hunk, review-comment anchors on PR view.

### Performance

`--numstat` first, file list immediately, hunks lazily per file. Virtualized
rows, eager cap ~2000 lines then explicit load. Binary/generated collapsed.
Highlighting off the main thread, skipped above a size threshold.

Editable right pane needs a live buffer, a disk write path, and invalidation
when an agent edits the same file underneath you. Build it last.

---

## 6. PR tracking

One GraphQL query per 5 minutes. **No ETag caching** — conditional requests are
a REST feature and the GraphQL endpoint is a POST; budget by points instead.
288 queries/day is negligible against 5000 points/hour.

Fork workflow: query PRs on the **upstream** repo where `author == you`; head
refs live on `origin`, so `headRepositoryOwner` matters for push targeting.

Per PR: `number`, `title`, `headRefName`, `headRepositoryOwner`, `baseRefName`,
`mergeable`, `mergeStateStatus`, `isDraft`,
`commits(last:1){nodes{commit{statusCheckRollup{state}}}}` — the rollup hangs off
the head commit, not off the PR — `reviewThreads(first:50){pageInfo{hasNextPage}
nodes{isResolved,isOutdated}}`, and `reviews(states:CHANGES_REQUESTED)`.

**Paginate `reviewThreads`.** A long-running PR exceeds 50 and the unresolved
count gates the `/resolve` button. Until pagination is implemented, render
`50+` rather than `50` so an under-count cannot silently hide work.

**Stacks:** PR B is stacked on A when `B.baseRefName == A.headRefName`. Build
the DAG each poll; it drives remediation ordering.

**Auth.** github.com, fine-grained PAT owned by the daemon. It needs **read
scopes only** — `pull_requests: read`, `checks: read`, `contents: read`, `metadata:
read`. The daemon never pushes; `/green` pushes through the agent's own git
credentials. Any write scope on this token is an unnecessary blast radius.

Store it in the OS keyring (`secret-service`/libsecret) or a `0600` file outside
the repo, injected as an env var at daemon start. Never in the repo, never in the
SPA bundle.

---

## 6b. Review queue

PRs where your review is requested — other people's work. Bottom-right, mirroring
your own PRs bottom-left. Left column is what you owe yourself, right column is
what colleagues are blocked on.

### Source

Shell out in the main checkout, on its own 5-minute timer offset from the PR
poll so the two don't burst together:

```bash
mise run reviews --json
```

**This mode does not exist yet.** The daemon treats a non-zero exit, unparseable
output, or an unknown `version` as a *degraded* pane — header reads
`reviews unavailable` with the stderr tail on hover — never as an empty queue.
Silently showing zero reviews when the command is broken is the one failure that
would actually cost a colleague a day.

### Expected contract

```json
{
  "version": 1,
  "generated_at": "2026-08-14T06:14:22Z",
  "reviews": [
    {
      "number": 2001,
      "title": "Refactor a config loader",
      "url": "https://github.com/acme/monorepo/pull/2001",
      "author": "dana",
      "requested_at": "2026-08-09T09:02:00Z",
      "updated_at": "2026-08-13T16:40:11Z",
      "state": "requested",
      "is_draft": false,
      "additions": 1204,
      "deletions": 880,
      "changed_files": 37,
      "checks": "passing",
      "base_ref": "develop"
    }
  ]
}
```

`state` ∈ `requested` · `re_requested` (they pushed after your change request)
· `changes_requested` (waiting on them) · `approved` (waiting on merge)
· `commented`.

`checks` ∈ `passing` · `failing` · `pending` · `unknown`.

Everything except `number`, `title`, `author`, `requested_at` and `state` is
optional; the daemon degrades each field independently rather than rejecting the
payload. Add fields freely — unknown keys are ignored. Bump `version` only on a
breaking change.

### Behaviour

Sort: `re_requested` first (they are actively waiting on your second look), then
`requested` by `requested_at` ascending — **oldest first**, same reasoning as the
idle-time sort on sessions. `approved` and `changes_requested` fall to the
bottom; they are waiting on someone else.

**Dots are always grey here.** Colour is reserved for your own work; a review
queue that competes chromatically with your sessions would undo the point of the
palette. This queue is a list, not an alarm.

Rows are single-line and share exact geometry with the PR list: same padding,
same gap, and a **fixed-width number column** so titles start at an identical
offset in both panes regardless of PR number length. Layout is dot, number,
title, author, changed-file count, then an optional reason when one applies
(`re-requested`, `approved`, `draft`).

File count stands in for review cost — 37 files is a different commitment from 1
— and the author tells you whose queue you are holding up, which is often the
deciding factor when two reviews are the same size.

Age moves to the group header (`3 waiting · oldest 5d`) rather than repeating on
every row. It still drives the sort; it just does not need to be restated four
times.

**Clicking a row opens GitHub's review mode** — `<url>/files`, the Files-changed
tab, not the conversation tab. That is where reviewing actually happens.

Rows are `<a>` elements in the SPA, not buttons handled by the daemon. Two
reasons: ⌘-click, middle-click and copy-link then behave as expected, and the
browser already holds your GitHub session — routing through the daemon and
`xdg-open` risks landing in a different browser profile that is not logged in.

This makes `url` effectively required in the JSON payload. If it is absent, derive
it from `number` and the configured repo rather than disabling the row.

Optionally a row can also spawn a review session in a fresh worktree on that
branch — same machinery as `/resolve`, pointed at someone else's PR — but that is
not required for v1.

---

## 7. Test capability model — **removed**

This section specified a `TestCapabilities` model — `Suite` (static/unit/
integration/e2e), a composer autoload probe, lockfile-drift detection, per-suite
trust and isolation — that gated all automation. **None of it exists.** It was cut
wholesale for open source: it answered "does a command in this worktree reflect
*this* worktree or silently main's", which is a question only a shared-stack
monorepo raises, and every other repo ran it empty. `TODO.md`'s decisions section
has what went with it and the one thing to do if that ever bites
(`MAX_AUTOMATION = 1`).

Kept as a numbered heading because the spec's `§` references are cited throughout
the code and renumbering would break every one of them. **Rule 2** — a run holds
shared resources, tracked as `locks_held` — is the only part still standing, and
`state.rs` cites it.

`fix-pr` now keeps only the guards that protect the machine and the repo:
authorship (can you push to the head repo), one run per PR, branch-busy, and the
concurrency cap.

## 8. Automation

Both skills take the PR number as an argument. Each runs in a worktree pinned to
that PR's head branch, created if absent, spawned with cwd = main.

`/green` is headless:

```bash
claude -p "/green <pr>" --output-format stream-json --worktree
```

`/resolve` is **interactive** and cannot use `-p`. `initialUserMessage` is only
honoured in non-interactive mode, so the daemon spawns a normal interactive
session and writes `/resolve <pr>\r` into the pty once `SessionStart` fires for
that `$ORCH_SESSION_ID`. The session then behaves like any other interactive one
— it appears in the rail, you can take it over mid-flight, and it obeys the same
state machine.

An automation run is an ordinary Claude Code session whose first prompt is the
skill invocation — `/green 4812`. It appears in the rail like any other session,
renders in the same terminal pane, and has a kill button. Nothing about it needs
a separate view; the only differences are that the daemon started it and that it
carries `Kind::Automation { pr, skill }`. No invisible work. Automation **never occupies main** — but it may
acquire the `main:instances` lock for e2e teardown (§7 rule 2), which is
displayed on the main workspace row while held.

### `/green` — fires immediately, subject to §7

**Trigger:** transition into (checks failing) OR (`mergeable == CONFLICTING`) on
a PR you authored, **and** §7 rule 1 passes, **and** the PR is not `Exhausted`.
Otherwise → `NeedsMain` or `Exhausted`.

**Retry lives in the skill, not the daemon.** `/green` amends and rebases, so the
head SHA changes on every internal attempt regardless of who acted. SHA-based
provenance is therefore impossible *and* unnecessary: the skill iterates
internally and stops on its own when it cannot make progress. The daemon's job is
only to avoid starting a second one and to avoid re-firing after the first gave
up.

```rust
enum PrAutomation {
    Eligible,
    Running { session: SessionId },
    Exhausted { at_head: Oid },   // skill gave up; wants you
}
```

- **One live `/green` session per PR**, keyed on PR number. While `Running`, the
  daemon does nothing further for that PR.
- **A run that ends with the PR still red means the skill is asking for you.**
  Record `Exhausted { at_head }` and surface it as attention in the rail. Never
  re-fire.
- **Exhaustion clears when the head moves while no automation session is alive
  for that PR.** Nothing of the daemon's was running and the branch changed —
  therefore you did it. No commit markers, no timestamps, no provenance. Back to
  `Eligible`.

> Because the state is a per-PR lock rather than a per-commit ledger, an earlier
> draft's `(pr, head_sha)` idempotency key, 3-per-24h cap, 2/8/30-minute backoff
> and two-strike circuit breaker are all **removed**. Each was a daemon-side
> reimplementation of retry logic the skill already owns, and each would have
> fought it.

**Ordering.** Stacks remediate bottom-up, strictly serialized, holding a lock on
the whole stack. This is now load-bearing rather than precautionary: amend-and-
rebase rewrites a base PR's history, which invalidates every child by
construction. Unrelated PRs run in parallel, cap 2.

**Port isolation.** Parallel runs still collide on ports and docker resource
names. Unique `COMPOSE_PROJECT_NAME` and dynamic port range per automation
worktree. Verify by hand before ever setting concurrency > 1.

**Guards:**

| Guard | Rule |
|---|---|
| Capability | §7 rule 1 — no trustworthy test path, no automation |
| Dep freshness | §7 rule 3 — stale copied autoload/lockfile blocks the run |
| Shared resource | §7 rule 2 — `main:instances` lock acquired before e2e; preemptible by you (§7 rule 2a) |
| Single run per PR | `Running` blocks any second `/green` for the same PR |
| Exhaustion | `Exhausted` blocks re-fire until the head moves with no run alive |
| Authorship | author must be you; head repo must be your fork |
| Push target | bare `git push` only, relying on `remote.pushDefault` |
| **`push -u` ban** | `PreToolUse` deny on `Bash(git push -u *)` and `Bash(git push --set-upstream *)` — rebinds upstream to `origin` and breaks `git pull` tracking |
| Blast radius | `--force-with-lease` only; `develop`, `main`, any protected upstream ref hard-denied |
| Active-session suppression | never remediate a PR whose head branch is checked out in a workspace holding a live, non-idle session — you are working on it |
| Drafts | treated as normal PRs; single-run-per-PR and exhaustion carry the load |
| Concurrency | 2 automation runs; see also the global process cap (§8b) |
| Kill switch | global pause in UI |

Persist `PrAutomation` to SQLite. A daemon restart must not resurrect a
`Running` state whose session is gone — reconcile against live PIDs on startup
(§8b) and demote orphaned `Running` to `Exhausted`.

**Killing a run** simply ends the session. The PR becomes `Exhausted` at its
current head and is eligible again as soon as you touch the branch. There is no
attempt budget to forfeit.

## 8b. Process accounting

Cap total concurrent Claude processes — interactive plus automation — at **8**.
Automation yields first and *defers* rather than fails; a deferred `/green` costs
nothing, a failed one is noise.

**Child PIDs are recorded next to every session record.** On daemon startup each
is checked against the process table: anything still alive is an orphan from a
crashed daemon. Kill it, mark the session `Archived`, and demote any `Running`
PR automation to `Exhausted`.

Teardown's "no live session" preflight consults `/proc`, not in-memory state — a
crashed daemon is exactly when a stale in-memory answer would let you delete a
worktree with a live agent in it.

### `/resolve` — manual

Button appears on unresolved, non-outdated review threads. Spawns an interactive
session as described above. Never automatic — review responses are your voice,
and the skill expects you in the loop.

The PR number comes from the button's context, so the daemon never has to infer
it. If the PR's branch has no worktree, one is created first; if it already has
one with a live session, the button targets that session instead of spawning a
second.

---

## 9. UI

No PR strip. PRs live in the rail; the top edge carries context for the session
you are in.

### Context bar (top edge, full width)

Replaces the centre pane's own header, so it costs no net vertical space:

`● invoice-export · feat/invoice-pdf → upstream/develop · #4812 checks failing ·
green running 2m` … `merge-base a91f0c3 · fetched 2m ago` … `[Diff] [Shell] [Kill]`

Everything here describes the selected session. It is the one row you read
constantly, which is why it earns the top edge over an ambient PR list.

### Rail (left)

Three groups, main pinned first:

1. **Main checkout** — one session, occupancy indicator.
2. **Worktrees** — session rows: dot, name, then a detail line (state, duration,
   PR number). Two lines each. No dirty-file count — that lives in the right
   column, one click away, and duplicating it here costs width on every row to
   answer a question you only ask about the session you have selected.
3. **PRs** — every open PR authored by you. One line each: dot, number, title.
   Rows for PRs that already have a session are dimmed and act as jump links to
   that session; the rest are the working set, since PRs usually outlive their
   worktree.

**Group header carries the summary** — `PRs · 1 needs you · 1 failing` — and
collapses. That is the whole value of a summary line, placed where the detail
already is instead of duplicated at the top.

Sort within the PR group: needs-resolving → failing → automation running → open
and clean → draft.

Sessions sort `BuildFailing` → `YourTurn` (by wait time, longest first) →
`Working` → **`Automation`** → `Archived`. The waiting duration is
shown on the row and is the number to optimise down.

The attempt counter is not shown on a first attempt. A `/green` run doing its job
needs no annotation; the count only earns space once it is heading somewhere bad,
so it surfaces from attempt 2 onward and the row goes red when the circuit
breaker trips. The general rule: if it needs you, it changes colour — quiet
states do not narrate themselves.

Automation sits second-from-bottom on purpose: a `/green` run in progress is
unattended by definition and needs nothing from you. **It promotes back into the
attention band only on failure** — a failed remediation, a tripped circuit
breaker, or a run that ends `NeedsMain`. Those are the only moments an automation
session is worth your attention, so those are the only moments it moves.

Dot colours are shared across sessions and PRs so one legend covers both:
**red** failing or conflicted · **yellow** needs resolving · **green** open and
clean · **grey** draft, idle or archived · **teal** a `/green` session is active.
Teal outranks everything: while automation holds a PR, "already being handled" is
the more useful signal than the underlying failure.

> **Not purple.** GitHub uses purple for merged, so a purple dot on an open PR
> reads as already-merged at a glance. Teal is unclaimed in GitHub's PR
> vocabulary (open green, draft grey, closed red, merged purple), which keeps the
> two colour languages from contradicting each other.

At 4–6 sessions plus PRs the rail runs long, so PR rows stay single-line and the
group is collapsible.

### Centre

xterm.js with `@xterm/addon-webgl`, attached to the selected session's pty. No
header of its own — the terminal starts directly under the context bar. Tab
switch replays the daemon buffer, never respawns. Diff renders as an overlay.

### Process drawer

Collapsible pane below the terminal, available on **every** workspace. One tab
per Process. On main that means `ng-watch` and `docker`; on a worktree it starts
empty and is a thin bar until you open something.

`+ Shell` (⌃`) opens a plain shell in the selected session's workspace directory.
Managed processes get a health dot and a restart button; shells get an exit code
and a close button. Auto-expands when a managed process transitions to `Failing`.

### Right

Changed files for the selected session's workspace.

### Keyboard

Next session, next *blocked* session, open diff, next change in diff, close
overlay, open a shell in the current workspace, jump to a PR, take/release main.

---

## 10. Build order

1. Daemon: pty host + WebSocket + xterm in a browser tab. Replaces tabby for one session.
2. Workspace/Session/Process model + hook receiver + state machine.
3. Build drawer and `BuildFailing` for main.
4. Worktree lifecycle (delegating to existing hooks) + right pane from hooks + reconcile.
5. Diff viewer, read-only, `vs upstream/develop`.
6. PR poller + top bar, read-only.
7. `/resolve` button.
8. Test-capability registry + preflight probe, reporting only — no actions yet.
9. Editable right pane in the diff.
10. `/green` — only after 8 has been correct for a week on real PRs.

Steps 1–4 already beat the current setup.

---

## 11. Coexisting with the repo's existing hooks

- Orchestrator hooks go in **`~/.claude/settings.json`** only. Never project
  `.claude/settings.json` (shared) and never a worktree's generated
  `.claude/settings.local.json` (owned by `worktree-create`, carries
  `claudeMdExcludes`). Hook entries merge across levels, so both sets run.
- Set `allowedHttpHookUrls: ["http://127.0.0.1:7777/*"]` so repo config can't
  redirect the daemon's HTTP hooks elsewhere.
- The daemon's `PreToolUse` deny hooks (push -u, push target) are additive to
  `worktree-edit-boundary` and `pre-bash`. Any of them exiting 2 blocks.
- Do not reimplement worktree creation or dep symlinking. `worktree-create`
  already bases on freshly fetched `upstream/develop` and configures triangular
  push; `worktree-link` owns the symlinks and the shared git exclude.
- A blocked tool call from `worktree-edit-boundary` should surface in the rail as
  a distinct signal — an agent editing outside its worktree is a prompt problem
  worth seeing, not noise to swallow.

---

## 12. Daemon surface and secrets

Single machine, no remote access. Bind `127.0.0.1` only — never `0.0.0.0`.

That is necessary but not sufficient. The daemon can spawn Claude sessions, run
git, and remove worktrees, so its HTTP surface is effectively local code
execution:

- **DNS rebinding.** Any web page you visit can issue requests to
  `http://127.0.0.1:7777`. Validate the `Origin` and `Host` headers on every
  request and reject anything that isn't the SPA's own origin.
- **Token.** Generate a random token at daemon start, embed it in the served SPA,
  require it on the WebSocket and all mutating endpoints. Cheap, and it is what
  makes the Origin check bite: a web page you visit cannot read the token, so it
  cannot forge a call even from a browser that would send one.
  — *Superseded in one respect:* this said it "closes the 'any local process' hole
  too". It does not. `GET /` returns the page with the token in it and is exempt,
  so any local process can read it. That is a deliberate trade, not a boundary;
  `README.md` § Security states it plainly and `api::guard` carries the reasoning.
- **Hook endpoints are the exception** — they come from `claude` subprocesses that
  can't easily carry the token. Keep them on a separate path prefix, accept only
  the documented hook schema, and treat them as write-only observers: they update
  state and can never trigger a spawn, a push, or a teardown.
- **Never expose a generic "run this command" endpoint.** Every action the SPA can
  trigger is a named, enumerated operation with validated arguments.
