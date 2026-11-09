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
| `orch new` | Start another session, in a worktree of its own or a free one you name |
| `orch kill` | Undo one of your own spawns |
| `orch rename` | Name one of your own spawns, once you know what its work is |
| `orch teardown` | Remove a worktree nobody is working in, when the human asks for a cleanup |
| `orch ls` | The sessions, their state, their branch and their path |
| `orch run` | Start one of the processes this workspace declares |
| `orch tell` | Message a session that changes files you changed, to settle the overlap |

**`orch <command> --help` is the reference, and it is the only one.** Read it
before you use a command. What each flag means lives there, beside the code that
enforces it, so this file does not repeat it and cannot be out of date with it.

## Before you spawn

A workspace is a git index, so it holds one session, and yours is taken by you.
`orch new` cuts a worktree for the new session unless you name a free one, and
takes `--model` and `--effort` when the work wants a different one than yours. Read
`orch ls` before you touch a branch: the same branch in two rows means one
branch.
