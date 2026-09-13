# Portability, and the processes the daemon spawns

Every entry here cost something. `CLAUDE.md` indexes them by their first
line; this file is the rest — what happened, what was measured, and why the
code is the shape it is.

## No `std::process::Command` and no `std::fs` on a tokio worker.
`proc::run_blocking` is the helper, and it takes a label so a panic says what
died. The rule was applied unevenly for a long time and the sweep that fixed
that is finished, so what is worth carrying is the rule plus the three places
it deliberately does *not* apply — each measured, so nobody re-opens them on a
hunch.
- **Single syscalls stay where they are.** `hooks.rs` has four `canonicalize`
  calls and one `exists`; `api.rs` has `revive`'s `cwd.exists()`,
  `forget_session`'s one `remove_file` and `free_worktree_name`'s stat loop. A
  `spawn_blocking` hop costs more than a stat, and `post_tool_use` runs per
  `Edit`.
- **The `with_*` store writes stay under the global write lock.**
  `automation.json` is **17 bytes**, and `manual.json`, `resolve-runs.json` and
  `stories.json` have never been written on this machine at all — a `write` +
  `rename` of tens of bytes is sub-millisecond. Getting them off the lock needs
  a channel, a writer task and an ordering guarantee, and it would either break
  the "mutating a durable store carries its own write" invariant or make every
  call site remember to persist, which is the exact shape `with_*` exists to
  prevent. Revisit if a store grows (`stories` is the only candidate, being a
  cache), with a number.
- **A `debug_assert` cannot enforce it.** `Handle::try_current()` succeeds on
  blocking-pool threads too, because the runtime handle stays in scope across
  `spawn_blocking`, so the check flagged 36 correctly-wrapped calls. The note is
  in `git::run`.

## Shelling out to coreutils is the other portability trap.
The review queue
ran its command under `timeout`, which is GNU and not on a Mac, so it failed at
the spawn and the pane blamed the review command for a missing binary it never
named. `proc::run_bounded` enforces the deadline in Rust instead — one
bounded-exec primitive (own process group; at the deadline SIGTERM the group,
a second's grace, then SIGKILL, because git removes its `.lock` files on TERM
and not on KILL; pipes drained on threads; the timeout error carries the stderr
tail), used by the review queue, `worktree_setup`, `mise` queries and every
network git call. Every other
command the daemon spawns is POSIX (`git`, `curl`, `gh`, `ps`, `which`, `kill`) —
keep it that way, and check `command -v` before reaching for a GNU flag.
**The tests are not exempt, and that is where it got in.** A sweep test backdated
a directory with `touch -d @<epoch>`, which BSD `touch` has no `-d` for at all —
its `-t` takes `[[CC]YY]MMDDhhmm[.SS]` — so it passed on every Linux run and went
red on macos-14 alone, with `out of range or illegal time specification`. Two
spellings are fine: `touch -t 202001010000` (POSIX, and what `worktree.rs`'s
reaper tests already used) or `std::fs::File::set_times`, which is `futimens` and
needs no process.

## `WorktreeCreate` is not a setup hook. It *is* the creation, and a daemon-cut worktree therefore never fires one.
Claude Code's own error text says what the
event is for: worktree isolation "with other VCS systems". The hook reads the
request on stdin, creates the tree by whatever means it likes, and prints the
path, which Claude Code then validates (absolute, no dot segments, a real
directory, not a symlink). So it cannot be re-run over a tree that already exists,
and it cannot be asked to honour a base the daemon chose: the monorepo's copy
hardcodes `upstream/develop`, which is exactly wrong for a PR worktree pinned to a
head ref or a fork cut from its parent. That is why `ensure_pr_worktree` and the
fork path cut their own.
**The post-create seam is `SessionStart`**, because Claude Code has no post-create
worktree event, and that one *does* fire for a daemon-cut tree: the daemon's
`--settings` merges with the repo's rather than replacing it. Measured, after a
session spent believing the opposite: three daemon-cut PR worktrees all had
`remote.pushDefault`, the shared `.plan` and every symlink, because the monorepo
hangs its `worktree-link` there. Do not "fix" a gap here without checking which
event the repo in front of you actually uses.
`worktree_init` and `worktree_setup` remain for a repo that puts real setup inside
`WorktreeCreate`, where a daemon-cut tree would genuinely miss it:
`spawn::run_worktree_hooks` runs both in each daemon-cut worktree, before the
session, non-fatal. They mirror the monorepo's two hooks — `worktree-create` bases
the tree, `worktree-link` puts the shared files in place — so a repo with two
scripts needs no wrapper to fan back out. Order is fixed and the second runs even
if the first failed, because an un-based tree is still worth linking.
A relative script path resolves against `main_checkout`; cwd is the worktree.
**There is no `claude --worktree` arm any more** — the daemon cuts every tree, so
every tree runs this. See the entry below on the isolation pin for why that arm
went.
The matching teardown event is `WorktreeRemove`, which the daemon also does not
fire; `git::worktree_remove` does its own thing.

## Paths are resolved at one boundary, and comparing across it silently fails.
`main_checkout` is `canonicalize`d in `Config::parse`, so `worktrees_dir` and
`worktree_path` are resolved too, and the agent-reported cwd is resolved where a
delegated worktree is adopted. That is what lets `workspace_for_path` match the
resolved paths `PostToolUse` hands it — an unresolved workspace root matches
nothing, and the symptom is not an error but an edit that never appears in the
changed-files pane. Do not introduce a workspace path that skipped that step.
Barely visible on Linux; on macOS `/tmp`, `/var` and `$TMPDIR` are symlinks into
`/private`, so it is the normal case.
**Which is why `testutil::scratch` canonicalises.** A fixture that skips that
step is on the wrong side of this boundary from everything it will be compared
against — the daemon's own paths (resolved by `Config::parse`) and every path git
prints — and the comparisons that then fail are the silent kind:
`workspace_for_path` decides no workspace owns the directory, and
`git::holder_of_branch`'s answer looks like a different tree. Two tests shipped
that way, passed on every Linux run, and failed only on the macos-14 runner after
the tag had been pushed. `holder_of_branch` resolves its own answer for the same
reason, so the invariant does not rest on git happening to.

## A `/proc` read is a portability bug that compiles.
Two guards stat'd `/proc`
and so answered *wrongly*, not loudly, off Linux: `pid_alive` read every session
as dead (teardown would delete a worktree with a live agent — it fails open), and
the instance lock's `holder` read every lock as stale (a second daemon starts).
`pid_alive` is now `kill(pid, 0)`; the lock stopped asking about pids at all and
took an `flock` instead, which is the kernel's answer rather than a guess about a
command line. `headroom` still
reads `/proc/meminfo` on purpose, because it is documented to mean "no opinion"
when it cannot read. Before adding a `/proc` read, ask which way it fails when
the file is absent; CI cannot catch this, since it compiles everywhere.

## The daemon cross-checks for macOS; the app cannot.
`cargo check --target
aarch64-apple-darwin -p orchd` works and is worth running after touching
anything platform-shaped. `-p orchestrator-desktop` does *not*:
`objc2-exception-helper` compiles Objective-C and needs a real macOS SDK, so it
fails in `cc-rs` on Linux for reasons that say nothing about your code. That
half is only answered by `check.yml` on the macos-14 runner.

## A mise install path is version-pinned, so never write one into a file that outlives the process.
mise installs each version in its own directory and
removes the old one on upgrade, while `current_exe` resolves symlinks and so
hands back the pinned path rather than the `latest` beside it. Three things
wrote that path down and each broke on the next `mise up`: the `.desktop` entry
(a launcher pointing at a version that is gone), `relaunch` (a restart that
cannot find itself), and the push guard's `PreToolUse` hook — which is the worst
of the three, because a `type: "command"` hook whose binary is missing fails
**open**. That one showed up as four `PreToolUse:Bash hook error` lines in a
session, with the guard silently not running for any of those pushes.
`update::stable_exe` is the one rule: swap the version component for
`latest` when that path really exists, else keep what you had. Use it anywhere a
path is persisted.

## A missing `cwd` is not an error to `portable-pty` — it is `$HOME`.
`CommandBuilder::as_command` filters the cwd on `is_dir()` and falls back to the
home directory, so a session aimed at a worktree that no longer exists does not
fail: it starts in `~` and runs there. A fix-pr run did exactly that, and the
only thing that stopped it was Claude Code's workspace-trust prompt for a
directory nobody had chosen. `PtyHandle::spawn` now refuses a `cwd` that is not a
directory, which is the one place every session, process and shell goes through.
The record that pointed there is the other half. A workspace record outlives its
directory — `claude --worktree` removes its own tree when that session ends, and
only `worktree::teardown` ever drops a record — so the PR flows were handed a
name whose tree was gone. **The repair is to rebuild it where it stood**, not to
cut a second tree or to prune the record: the session owns that directory,
because transcripts are keyed by it. A resume already did that
(`api::revive` → `worktree::revive`); `ensure_pr_worktree` now does it too, via
`recorded_worktree_for`, which hands back the recorded path precisely so the tree
can be cut again at it. `worktree_holding` deliberately ignores whether the
directory exists — a live session whose tree was deleted still holds its branch,
and `branch_busy` must keep saying so.
