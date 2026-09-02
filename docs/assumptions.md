# What the daemon assumes

Read against the code, not against intent. Every entry names where it is relied on
and what goes wrong when it is false, because that second half is the only reason
to write the list: an assumption nobody can see is one the next repo breaks
quietly.

Three kinds are mixed together on purpose, and the difference matters more than the
list does. What **Claude Code** guarantees is a contract. What **git** guarantees is
a contract. What **a repo** happens to do is a convention, and orchd is developed
against one repo, so a convention is a guess until something else confirms it.
Each entry says which it is.

## Session and worktree

**1. At most one *live* session per workspace.** Enforced, not hoped for:
`api::refuse_if_occupied` at runtime and `first_per_workspace` on the restore path,
which also defends a `sessions.json` written before the rule existed.
`allow_several_in_main` relaxes it for the main checkout alone, since a worktree
exists precisely so two pieces of work do not share one index.
*Kind:* orchd's own rule. *Breaks:* nothing silently. The refusal is a message.

**2. A worktree belongs to a *branch*, and any number of conversations about that
branch share it.** Decided rather than inherited: git will not check one branch out
in two worktrees (`git::refused_as_already_checked_out`), so this is the only shape
available, and it is already what `swap` assumes when it exchanges what two trees
have checked out. Measured here: 9 of 85 workspaces ever held more than one
conversation, and `pr-34896` held five because five runs shared that PR's head ref.

The consequence is that code reading "the session in this workspace" is wrong
wherever it appears. `transcripts_archived`, `recovery_recorded` and
`worktree::reap_old` all iterate the set; `prForWorkspace` in the SPA returns the
*first* PR it finds. *Kind:* git's constraint, adopted as orchd's model.
*Breaks:* a one-session read attributes the wrong conversation to a tree, and
nothing errors.

**3. A session's worktree is disposable once the conversation is turnless.**
`spawn::watch_session_exit` forgets the record of an interactive session that never
had a turn, and now removes its tree with it. *Kind:* orchd's own rule, new.
*Breaks:* a tree that mattered is removed, which the preflight is what prevents.

**4. A tree no conversation points at is disposable, and is dated by its
directory.** `worktree::reap_old`. Nothing can resume it and `git worktree remove`
never deletes the branch, so the commits stay reachable from main.
*Kind:* orchd's own rule, new. *Breaks:* a tree cut outside the daemon's knowledge
and left deliberately gets removed after the retention period.

**5. The teardown preflight is sufficient authorisation to remove a tree.** Six
checks, and all three automatic paths lean on them rather than adding rules of
their own: `api::discard_spawned`, the turnless exit, and the retention timer.
*Kind:* orchd's own rule. *Breaks:* everything above it. This is the load-bearing
one, and it is why none of those paths may grow a second gate or a `--force`.

**6. A conversation's age is its transcript's last write, not its start.**
`store::last_used`: Claude Code appends a line per turn and the daemon only reads
that file, so its mtime is the conversation's own last activity. Readable for 87 of
102 records here. It falls back to the archived copy, then to `created_at`, which
can only make a tree look older than it is. *Kind:* orchd's own rule.
*Breaks:* nothing quietly. The reason it is not `created_at` is that a conversation
you worked in for weeks would read as ancient the day after you stopped.

**6b. `created_at` is the only clock persisted on the record.** `state_since` is
not in `SessionRecord`, so anything reasoning about "archived when" is really
reasoning about the last daemon start. That is why age comes off the transcript
rather than off the state machine. *Kind:* orchd's own rule.

**7. The per-worktree git index mtime says nothing about when a tree was last
used.** Measured, not assumed: the daemon's own reconcile runs `git status` in every
tree and refreshes it. All 32 orphaned trees read 0.0 days while their directories
read 8 to 19. *Kind:* a consequence of orchd polling. *Breaks:* any "last used"
signal built on it reads every tree as fresh.

## Layout and naming

**8. A workspace's id *is* its directory name, and a name may be reused.**
`spawn::worktree_name_of` returns the first path component under the worktrees dir
and that string is the workspace id everywhere, which is what makes it survive a
swap, a branch rename and boot adoption from `git worktree list`.

§2's "names must be unique over time" is **not** enforced, on purpose. Its reason
was that the projects directory is keyed by path, so a reused name would interleave
two conversations' transcripts, and that is false (see 15). `ensure_pr_worktree` had
already outvoted it by reusing `pr-<n>` for every run on a PR. What is still refused
is a name a *live* workspace holds, and a directory already on disk.
*Kind:* orchd's own rule. *Breaks:* a resume can land in a tree cut again for
something else, which `worktree::branch_drift` warns about on the resume rather than
refusing the creation.

**9. Worktrees sit exactly one level under `worktrees_subdir`, which is inside
main.** `Config::worktrees_dir` joins it onto `main_checkout`, `worktree_name_of`
takes one component, and the changed-files exclude is a porcelain-relative prefix.
*Kind:* a repo convention this repo happens to share with Claude Code's default.
*Breaks:* sibling worktrees (`../feat`), the wider convention, are not supported;
the daemon logs "ignoring worktree outside the managed dir" and manages nothing.
Kept on purpose for Claude Code, whose own `--worktree` puts them here. Revisited
when a second agent is hosted, which is an entry in `TODO.md`.

**10. A worktree's branch is `worktree-<name>` when it has to be guessed.**
`worktree::derive_recovery` and the `register_worktree` convention. Only reached
when no recovery record exists. *Kind:* Claude Code's convention, adopted.
*Breaks:* a rebuild looks for a branch that was never called that, and says which
one is missing rather than failing silently.

**11. A PR worktree is named `pr-<n>`.** `spawn.rs`. *Kind:* orchd's own rule.
*Breaks:* two flows disagree about which tree belongs to a PR.

**12. Main is privileged and is never torn down.** `worktree::preflight` bails on
`MAIN` before any check runs. *Kind:* orchd's own rule, and a real constraint: one
checkout is one working tree, one index, and at most one dev stack.

## The Claude Code contract

**13. The daemon's session id is Claude's session id.** Every spawn passes
`--session-id`, so `--resume`, transcript lookup and hook correlation need no
mapping. *Kind:* contract. *Breaks:* all three at once.

**14. A transcript lives at `~/.claude/projects/<cwd-slug>/<uuid>.jsonl`, and the
slug replaces both `/` and `.`.** *Kind:* contract, undocumented shape.
*Breaks:* every worktree session looks like it has no transcript.

**15. A transcript is keyed by session uuid, so two conversations in one directory
do not interleave.** Said here because the opposite was written into two comments
and justified a guard that refused reviewing any PR whose worktree you had torn
down. *Kind:* contract, measured.

**16. Session names come from an undocumented `ai-title` record.**
`store::ai_title` tails the transcript. Degrades to the workspace name.
*Kind:* contract, undocumented. *Breaks:* the rail reads the workspace name
everywhere, which is the symptom to look for.

**17. `SessionEnd` is not always an ending.** Its `reason` is one of
`clear`, `resume`, `logout`, `prompt_input_exit`, `other`, read off the agent
binary's own schema, and the first two leave the process running.
`hooks::ends_the_process`. *Kind:* contract. *Breaks:* the daemon marks a live
session `Exited` and hands main's claim back out from under it.

**18. Hooks are observers with about a second to answer, and every hook finds its
session.** `hooks::detach` answers immediately and finishes detached, because Claude
Code gives a hook one second and a dropped future silently loses the state change.
*Kind:* contract. *Breaks:* a hook that waits on anything costs a turn.

The "a hook for an unrecorded session is dropped in silence" race is **closed**, and
CLAUDE.md described it as live until this was written. `insert_and_spawn` puts the
record in *before* `PtyHandle::spawn` and removes it again if the spawn fails, since
`a6d4854`, "Record a session before its agent can speak". What remains is far
narrower: the pty is attached to the record after the spawn returns, and
`session_start` calls `pending_prompt.take()` unconditionally while only writing it
when a pty is there, so a `SessionStart` landing in that gap would take a `/resolve`
prompt and drop it without a word. The gap is one lock acquisition against Claude
Code's entire boot, so it is unreachable in practice and documented rather than
guarded.

**19. `WorktreeCreate` *is* the creation, not a setup hook, and a daemon-cut tree
never fires one.** The post-create seam is `SessionStart`, which does fire.
*Kind:* contract. *Breaks:* setup silently skipped for daemon-cut trees, which is
what `worktree_init` and `worktree_setup` exist to stand in for.

**20. Claude Code pins worktree isolation in the transcript, and the last
`worktree-state` record wins.** `store::clear_worktree_pin` appends what Claude
Code would have written, and only ever between processes. *Kind:* contract,
measured across 395 transcripts. *Breaks:* a relocated conversation cannot run git
in the tree it was just moved into.

## Git and the repo

**21. `git worktree remove` refuses rather than destroys.** So a refusal is
surfaced, never escalated to `--force`, and never followed by a filesystem delete:
a worktree can hold symlinks back into main, and a recursive delete that follows
them destroys the main checkout. *Kind:* contract. *Breaks:* catastrophically, once.

**22. `git worktree remove` leaves the branch alone.** What makes an orphaned tree
safe to reap: the commits stay reachable from main. *Kind:* contract.

**23. The base ref splits as `<remote>/<branch>`.** `git::base_branch` and the
SPA's `mainHoldsWork`. `origin/HEAD` is the case that cannot be split that way, and
both sides answer conservatively instead of guessing. *Kind:* git convention.
*Breaks:* the push guard loses its base-branch rule and enforces only
force-with-lease.

**24. Paths are resolved at one boundary.** `main_checkout` is canonicalized at
parse, so `worktrees_dir` and `worktree_path` are too, and a reported cwd is
resolved where it is recorded. *Kind:* orchd's own rule. *Breaks:* comparisons
across the boundary fail silently. Barely visible on Linux; the normal case on
macOS, where `/tmp`, `/var` and `$TMPDIR` are symlinks into `/private`.

**25. One daemon per config dir, and one main checkout per daemon.**
`instance::holder` decides by asking whether the pid is alive, and the lock file is
deliberately left behind. *Kind:* orchd's own rule. *Breaks:* two daemons fight over
`sessions.json` and the hook settings file.

**26. A session's environment is not the shell's.** Started from a launcher it is
the systemd user manager's, holding no checkout's variables, so `env_source` asks
the tool per spawn. *Kind:* platform reality. *Breaks:* anything expanding a
variable gets the empty string and fails in its own words.

## What this repo happens to do, and another will not

Named separately because these look structural and are not: worktrees under
`.claude/worktrees`, a base of `upstream/develop`, a `WorktreeCreate` hook that cuts
from a fixed ref, worktree setup hung off `SessionStart`, `mise` as the task runner,
`origin` as a fork with `upstream` as the real remote, a docker compose stack in
main, and a `.plan` directory shared back to main by symlink.

The failure this section is written for is not a crash. It is a feature that works
everywhere it was tried and means nothing elsewhere.
