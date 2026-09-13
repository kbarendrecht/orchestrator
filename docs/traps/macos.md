# macOS, and the tooling around the build

Every entry here cost something. `CLAUDE.md` indexes them by their first
line; this file is the rest — what happened, what was measured, and why the
code is the shape it is.

## The app's modifier is ⌘ on macOS and Ctrl elsewhere
(`core.appMod`, from the
`__ORCH_PLATFORM__` the daemon substitutes into the page — told, not sniffed).
Worth knowing why rather than just that: on a Mac ⌘ never reaches the pty, so the
Ctrl-shadows-the-terminal trade-off the layer contract agonises over is
Linux-only. Two exceptions, both deliberate: session switching is `Ctrl+Tab`
everywhere because ⌘Tab is the macOS app switcher and never arrives, and the
legend's rows carry `MOD` placeholders resolved at boot — including in the
descriptions, not just the chords, which is a bug that shipped once.

## The config dir has a space in it on macOS
, and anything from it that reaches
a shell must be quoted. `config_dir` is `~/Library/Application Support/orchd`
there and `~/.config/orchd` elsewhere. The push guard's hook is a shell string
(`type: "command"` — that is how `SessionStart` gets a pipe and `|| true`), so an
unquoted path splits at the space and the hook runs nothing: the guard fails
open and silently stops existing. Use `hooks::sh_quote`. Prompt-file paths are
fine — they go into prose the agent reads, not a shell.

## A fresh checkout the daemon points at needs Claude Code's workspace trust accepted once.
Until then `claude --worktree` refuses ("Workspace trust not
yet accepted") and the spawned session exits instantly, leaving a workspace
record for a worktree that was never created. Accept it in the dialog or set
`hasTrustDialogAccepted` for that dir in `~/.claude.json`. The monorepo hides
this by having been trusted long ago; `docs/fixture-pr.md` has it.

## `claude --worktree` leaves a lock the daemon must clear at teardown.
Every
worktree it cuts is `git worktree lock`ed, and the lock outlives the session the
daemon kills — so a plain `git worktree remove` refuses it forever.
`git::worktree_remove` clears a lock whose owning pid is dead and retries, still
never `--force` and never a filesystem delete (preflight already proved the tree
clean, so a stale lock is the only thing left to trip on). Do not "simplify" the
retry away.

## `POST /api/pr/:n/fix-pr` starts a run immediately.
No confirmation: the
guard table refuses on authorship — *can you push to the head repo*, read from
`headRepository.viewerPermission`, not whose name is on it — a run already going,
a busy branch and the concurrency cap, and *nothing else*. "The PR looks fine" is not a refusal,
because a run is also how a PR that has fallen behind gets rebased. Easy to fire
by accident while poking at the API.

## Pushes are guarded, by two halves that must agree.
`crates/orchd-base/src/guard.rs` holds the rules and its module doc says what it
is and is not — a **mistake-catcher, not a control**, Bash only, so `gh` or a
script the agent writes goes around it. Do not write docs that claim otherwise;
the README did, and that is the kind of sentence that earns misplaced trust.
Three rules: no lease-less `--force`, no push to the base branch (from
`upstream_ref`, never a list of likely names), and no git aimed out of the
worktree the session works in. Never `git merge` into a branch here, rebase.
**The two halves.** `orch guard push` runs them as a `PreToolUse` hook on the
agent's Bash, and `git::push_with_lease` re-states the base-branch rule because
a *daemon* push never passes through a hook. The hook cannot name two facts per
session, since one settings file serves them all: `--main` is baked in, and the
tree is read from the payload's own cwd. Its one exemption is the session's own
git dir, since a worktree's real one lives under the *main* checkout.
**And it is a question, not a wall.** The refusal names `orch outside <path>`,
which raises an *ordinary* `Interaction` — the same field, the same box, the
same `/ask/:id/wait` the agent already polls — and a yes appends that folder to
`Session::outside_grants`. **A yes is one folder, not the session**: it was a
`bool`, so the first grant let the session reach every checkout for the rest of
the conversation, and the question that named a folder had answered about all of
them. Two more things are deliberate: the grant keys on the **ask id**, because
an agent writes its own option values through `orch ask` and would otherwise be
asking itself; and it is **not** on `SessionRecord`, so a restart asks again
rather than assuming.
`tools/e2e/flows/14-outside-grant.mjs` drives the whole path. **`mise run e2e`
builds `orch` as well as `orchd`**, and did not: that flow runs `orch guard push`
directly, so a change to the guard's own half was measured against whatever
binary was on disk, and it read as the grant refusing every command it had just
allowed.

## A `rust-toolchain.toml` is a no-op here, and silently.
`mise env` exports
`RUSTUP_TOOLCHAIN=stable`, and that variable **outranks** the file in rustup's
precedence — so a pin written there is ignored on any machine with mise active
(which is every developer's) and honoured in CI, which has no mise. That is the
opposite of what a pin is for: it manufactures an invisible divergence instead of
removing one. Measured, not assumed — `rustup show` reports "overridden by
environment variable RUSTUP_TOOLCHAIN", and dropping the variable made rustup
start downloading the pinned toolchain.
So the Rust version is pinned **in the workflows** (`dtolnay/rust-toolchain@<v>`,
the same string in `check.yml` and `release.yml`, bumped together) and local stays
on mise's `rust = "latest"`. The drift that leaves runs the harmless way round: a
developer on a newer rustc meets a new clippy lint *before* CI does, rather than
CI failing on a commit that touched no Rust. Collapsing it to one source of truth
means provisioning Rust through mise in CI too.
