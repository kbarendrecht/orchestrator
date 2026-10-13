---
name: orch
description: How to reach the orchestrator you are running inside: start or end another session, list the sessions and where they run, or start one of the workspace's declared processes. Use when you want a second session to work in parallel, or need to know what else is running before touching a branch or a worktree.
---

# You are running inside orchd

The daemon that started you is `orchd`, and `orch` on your `PATH` is how you talk
to it. Nothing needs configuring: your environment already says where the daemon
is, who you are, and what you may do.

If `orch` is not on your `PATH`, you are not in a session the daemon started.
Then none of this applies, and you must not simulate it.

## What you can ask for

| | |
| --- | --- |
| `orch new` | Start another session, beside you or in a worktree of its own |
| `orch kill` | Undo one of your own spawns |
| `orch teardown` | Remove a worktree nobody is working in, when the human asks for a cleanup |
| `orch ls` | The sessions, their state, their branch and their path |
| `orch run` | Start one of the processes this workspace declares |

**`orch <command> --help` is the reference, and it is the only one.** Read it
before you use a command. What each flag means lives there, beside the code that
enforces it, so this file does not repeat it and cannot be out of date with it.

## Before you spawn

A workspace is a git index, and two sessions in one share it. That is what you
want for a hand with the thing you are doing, and it is wrong for two jobs that
will each commit. Cut a worktree for parallel work, and read `orch ls` before you
touch a branch: a shared path means one git index, and the same branch in two
rows means one branch.
