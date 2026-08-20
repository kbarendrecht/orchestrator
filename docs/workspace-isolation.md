# Workspace isolation: where a session works, and where its commands run

A decision record, written while deciding orchd's *defaults* as it heads toward
open source. The daemon had one project's shape baked in as if it were the only
one. Nothing here is a new mechanism — the settings already exist and are already
generic. What was missing was saying which values are the portable default, and
letting a heavier setup be **a set of values** rather than a special case.

## The axes

Every difference between a plain repo and a container-heavy monorepo is one of
these settings. A "profile" is just a column.

| Question | Setting | Default | `acme` |
|---|---|---|---|
| Where does a new session work? | *(session default)* | its own worktree | same |
| Where do worktrees live? | `worktrees_subdir` | `.claude/worktrees` | same |
| Long-running services for the repo | `main_processes` | none | build watcher, `docker compose` |

orchd used to have more columns here — a `capabilities` block modelling test
suites, the container they run in, its `<main>` mapping, per-suite isolation and
the deps a worktree links from main. That whole subsystem was cut for open
source: it existed only to answer "does a command in this worktree reflect *this*
worktree or silently main's?", which is a question only the shared-stack model
raises, and every other repo ran it empty. The container-heavy setup it described
— worktrees nested under a main checkout that owns one long-lived stack, suites
run as `docker exec -w <mapped path>`, e2e serialized by a lock — is a legitimate
but *minority* configuration; if orchd ever grows a general container mode, see
"Left open" below rather than reviving it.

## What the research said

Deep-research pass, adversarially verified; cited findings in
`research/worktree-docker.md`. What bears on the defaults:

- **Containerizing the test runner is the exception.** The mainstream runs the
  toolchain on the host with only services (Postgres/Redis) in containers
  (Testcontainers). So running commands on the host is the right default, and a
  general container mode is left open rather than assumed.
- **For parallel worktrees, per-worktree compose projects dominate** — Compose
  namespaces by project name, defaulting to the directory basename. The
  one-shared-stack + `docker exec` model is a minority pattern. If orchd ever
  grows a *general* container mode, build that one.
- **Peers split, and the container-free half is real** — Uzi (worktrees + tmux on
  the host, per-agent port from a range) and Vibe Kanban (filesystem-only
  isolation) ship no containers at all.
- **Sibling worktrees are the wider convention** (`../feat`), and a worktree
  outside a mounted root breaks in-container git — its `.git` points at a common
  dir outside the mount, which devcontainers had to add
  `--mount-git-worktree-common-dir` to fix.

## The decisions

**A session gets its own worktree by default.** Isolated, on `worktree-<name>`.
The main checkout is untouched unless asked for.

**Worktrees stay nested at `.claude/worktrees`.** It is Claude Code's own
`--worktree` location, so delegation gives it for free, and it is one layout for
every profile. Siblings are the wider convention and are deliberately *not*
offered: they would cost a config-shape change (a worktrees dir that may sit
outside main) and relaxing the in-main guard, for a gain that does not pay while
Claude Code is the only agent hosted. Nesting also keeps the gitdir inside the
mount, which is what a future in-container mode would need. Reconsider if a
non-Claude agent is ever hosted.

**The daemon excludes the worktrees dir from the parent's `git status`.** A
registered worktree inside the working tree shows up as `?? .claude/` — git does
not auto-ignore it. A managed block in `.git/info/exclude`, written by
`git::configure_repo` where fsmonitor is already set, under its own markers (the
shape `todo.rs` uses) so it sits alongside any block the repo's own hooks
maintain.

**A worktree is removed when its session ends clean.** Otherwise
worktree-per-session silts the rail up with throwaway branches. Removal runs
*iff* the existing six-check teardown preflight passes — clean tree, nothing
unpushed, no commits beyond base, transcript archived. Anything carrying work
stays. "Has work" is the gate that already guards manual removal, defined once.

**The main checkout is an explicit choice, and locked.** An opt-in action runs a
session in the real checkout, in place, on its current branch — your uncommitted
work is there. One at a time: the one tree that cannot be duplicated cannot be
handed to two agents. That is a git constraint and survives every column of the
table; it is not the container reason main used to be special either.

**Vocabulary: "worktree" and "main checkout", never bare "main"** — which
collides with the branch. Mirrors git's own "linked working tree / main working
tree".

## Left open

- **A general container mode.** orchd carries no container config at all now that
  the capability subsystem is gone. If containers are ever offered, build
  per-worktree compose projects (`COMPOSE_PROJECT_NAME`, ports from a pool), not
  the shared-stack `docker exec` model that was cut.
- **Dev-server port collisions.** Fine until someone runs a per-worktree process
  publishing a fixed port. The peer answer is a host port range plus a `$PORT`
  placeholder; `ORCHD_PORT_BASE` (already used per fix-pr run) is the hook.

## Implementation shape (not yet built)

- `spawn`: "new session" defaults to cutting a worktree; the main checkout
  becomes an explicit path. `spawn_worktree_session` already exists and already
  branches on whether the subdir is Claude Code's default.
- `git::configure_repo`: idempotent managed-block exclude of
  `/.claude/worktrees/`.
- `watch_session_exit`: on a workspace's last exit, run `preflight`; if
  `can_remove`, tear down.
- `web/app.js`: relabel `main` → "main checkout"; worktree becomes the primary
  new-session gesture; keep an explicit main-checkout action; tooltips on both.

Verification: on a plain repo, a new session lands in a fresh worktree and
disappears on clean exit but survives once it has a commit; the main checkout
runs in place and refuses a second session; a `acme`-profile daemon still
nests worktrees exactly as today (the existing review-parity and isolated-daemon
smokes cover that path).
