# The daemon's machinery: hooks, worktrees, main, the stores

Every entry here cost something. `CLAUDE.md` indexes them by their first
line; this file is the rest — what happened, what was measured, and why the
code is the shape it is.

## Hooks are observers, not gatekeepers.
They answer immediately and finish
their work detached, because Claude gives a hook one second and a dropped
future silently loses the state change. Do not make a hook wait on anything.

## Every hook finds its session, and the window where one did not is closed.
`insert_and_spawn` puts the record in *before* `PtyHandle::spawn` and takes it out
again if the spawn fails (`a6d4854`, "Record a session before its agent can
speak"). It used to insert *after*, so an agent quick enough to fire
`UserPromptSubmit` lost it, and a `Stop` arriving after the insert left a session
at `your_turn` with `had_a_turn` false, a conversation the rail would not offer to
fork or resume. Real Claude Code took human-scale seconds to a first prompt and
never landed there; anything scripted did, which is why the e2e agent waits to see
itself in `sessions.json` before speaking. That wait is now belt and braces rather
than the thing that makes the flows work.
**What is left is narrower and still silent.** The pty is attached to the record
*after* the spawn returns, and `hooks::session_start` calls `pending_prompt.take()`
unconditionally while only writing it when a pty is present. A `SessionStart`
landing in that gap takes a run's first turn and drops it. The gap is one lock
acquisition against Claude Code's whole boot, so it is documented rather than
guarded.
**A PR run's record follows the same rule now.** A `claude` that exits
at once — a bad `--settings`, the version gate — was reaped before its
`Running` / `ResolveRun` record existed, so the exit watcher found nothing to
settle and the record named a corpse until a restart. The caller mints the id,
writes the record, then spawns, and takes the record back out on failure.
`headroom::check` moved into `insert_and_spawn` for the same reason: it was at
two of the four spawners, so the rail's new-worktree button, the fork path and
the story filer had no check at all.

## `forge/github_write.rs` will not resolve a thread, approve, merge or open a PR.
That is a design boundary, not a gap. Resolving is the comment author's button.
Which means **`is_resolved` can never stand for "handled"**: the daemon never
sets it, so every thread it has ever answered is still unresolved. A re-request
guard derived from `!is_resolved` shipped and could never fire — read
`post::rerequest_all`, and ask "did *we* settle it" instead. The neighbouring
trap is `answerable`, which flips the moment you post: it answers "is anyone
owed a reply", so it is only "who reviewed" on a fetch taken *before* the
posting.

## The daemon no longer asks Claude Code to cut a worktree, and the isolation pin is why.
`claude --worktree` was the creation path at Claude Code's own layout,
and every session it starts is pinned into worktree isolation. That pin refuses
**writes** as well as git, and both refusals landed on one ordinary thing: the
monorepo shares a `.plan` scratch dir into each tree as a relative symlink, and a
pinned session could write neither the link ("the path is spelled in a form that
cannot be safely resolved … a symlink storing a raw dot segment") nor the shared
checkout behind it ("This session is isolated in the worktree …"). Both doors
shut, and the hooks were fine — a session spent chasing them because `.plan`'s
mtime looks like evidence and is not: `worktree-link` re-links on every start, so
that timestamp is the *last* session start in the tree, never the first.
Measured across 119 worktree transcripts: 49 carry a pin and every one came from
that arm; the 70 the daemon cut carry none. So `spawn_worktree_session` cuts every
tree itself (the repo's `WorktreeCreate` still does the work, adopted), and the
isolation the daemon actually needs — main's branch and occupant, which
`claim_main`, `park_main`, `switch_main_to_pr` and `branch_busy` all read — is
`guard::isolation`, on the agent's Bash, git-only and silent about writes.

## Claude Code pins worktree isolation in the transcript, and the daemon clears it by writing to that same file.
Every turn re-appends a `worktree-state` record
(`worktreePath`, `worktreeName`, `hookBased: true`), and on resume its own hook
refuses any git command aimed outside that original worktree — *including the
tree the daemon just moved it into*. A swap that worked perfectly (branch, files,
record, conversation all correct) left the agent unable to run `git status` on
its own work: "This session is isolated in the worktree …, but this command
redirects git to the shared checkout".

**A resume now clears any pin, not only one that disagrees with the cwd.** The old
rule read an agreeing pin as correct isolation; the entry above is why that is
wrong, and sessions cut by the old arm carry exactly that kind of pin, so this is
the only thing that ever releases them.

It used to say here that the daemon cannot clear that from outside, since
`ExitWorktree` is the agent's own tool, so `api::arrival_notice` asked the agent
to call it. **Both halves of that were wrong, and a conversation paid for it for
two days**: it went on editing a worktree that had since been cut again for a
different branch while its own branch sat in main, taking the bare isolation
refusal sixteen times, and it never called `ExitWorktree` once.

- **The pin is a running value, not a header.** The *last* `worktree-state`
  record wins, and letting go is one line —
  `{"type":"worktree-state","worktreeSession":null,"sessionId":…}`, preceded by
  `{"type":"relocated","relocatedCwd":…}`. Measured across 395 transcripts: 128
  end exactly that way. So `store::clear_worktree_pin` appends what Claude Code
  would have written, and `spawn_session` calls it on resume whenever the pin
  disagrees with the cwd. **Only ever between processes** — the old pty dead, the
  new one not started — because a live agent is appending to that file too.
- **Asking was delivered on the wrong tools.** The notice rides `PreToolUse`,
  which was registered `Edit|Write`, and the isolation bites on *git* — Bash. The
  explanation sat queued behind a write the session never made. `PostToolUse` had
  been widened off that same matcher for the same reason; `PreToolUse` now matches
  every tool.

`arrival_notice` stays, because it says in words what the record only implies, but
it is no longer the mechanism.

## Opening a PR in a worktree can move main's branch out from under you
— by
design, since `park_main` will not carry uncommitted work and a branch stuck in
main makes every PR flow for it impossible. If main holds that PR's own branch,
`ensure_pr_worktree` moves branch *and* work into the tree it was about to cut
and puts main back on base, logging that it did. Only a live session in main is
still refused. It is not a read-only flow with respect to main.

## Main goes back to base when the last session leaves it, and it takes the base back to do so.
`park_main` parks whatever main holds — a PR branch, a
swapped-in one, a hand-checkout — and not only what `open_pr(main)` put there.
It used to need that provenance (an `AppState::main_pr_park` mark, now gone), on
the reasoning that parking a swapped-in branch would undo the swap; but nobody is
working it once the last session has gone, and a branch resting in main blocks
every flow that needs main on base. The branch is not lost, and
`move_branch_out` is how it gets a tree if you want one. `park_on_base` still
refuses a dirty main, which is the safety that matters.
**The other half is that base can be somewhere else entirely.** Git allows one
checkout per branch, so a worktree sitting on `develop` makes main's return
*impossible* rather than refused, and a swap is how it gets there: main resting
on base, a worktree swapped in, base handed out as the exchange. It surfaced days
later as `fatal: 'develop' is already used by worktree at …` from four calls deep
inside `move_branch_out`. So `park_main` reclaims it — `git::holder_of_branch`
finds the tree and `git::release_branch` gives that tree a `worktree-<name>` at
the commit it already has, so every file and commit stays put and only the name
changes. Reclaimed at park rather than refused at the swap, because pressing swap
twice has to stay the undo. Two things it will not do: a tree with a **live
session** keeps its branch (a name changing under a working agent is a surprise
the log cannot undo) and a directory that is not a workspace of ours is never
touched; both leave main where it is and say why.

## The drawer can hand a pane's output to the session, and the daemon owns *when*.
`api::tell_session` types text into one session's pty, reached from a
right-click on a process tab or on the pane itself (`paneMenu` in `web/app.js`).
It lands as an ordinary **user turn**, which is what it is — you pointed at
something, and the transcript should read as though a human did.
Three states refuse it, and each is a keystroke meaning something other than a
prompt: mid-turn (Claude Code submits whatever is half-typed), a permission
prompt (consent) and an open question (the highlighted choice). `nudge_sessions`
learned those first; this is the same table with one target. The refusal is a
sentence the pane toasts, because a press that silently did nothing reads as a
broken button.
Two things keep it **agnostic**, which is the whole point of the shape: the text
is read out of *xterm* (`Term.readTerm` — the selection if there is one, else the
last 50 non-blank rows), so no output is parsed anywhere and the payload is
whatever you highlighted; and the only name involved is the one the repo's config
gave the process. `ng-watch` appears nowhere in the code — only in comments and
test fixtures — and this must not be what changes that. 8 KB is the cap, because
a ring buffer holds ~3600 lines and a prompt is a line somebody reads, not a log.

## A config this build cannot read is repaired on disk, not tolerated in memory.
`crates/orchd-repo/src/migrate.rs` runs on start, from both readers of the file
(`Config::existing` for the desktop app, `Config::load_or_init` for a daemon
started from a terminal), and it is idempotent so the second call costs a read.
It exists because tolerating the old spelling in the *reader* was not enough:
a config the parser refuses costs the **whole file**, the app reads that as first
run and offers a folder picker for a project configured months ago, and the
first-run write then merges by key and keeps the very line being refused.
A colleague met that on `"tracker": "none"` — which was the **default** and which
the old settings pane wrote back on every save (`Settings::merge_into` wrote every
field), so it is on most machines that ever pressed save.
Four properties, each deliberate. **Shape-driven, with no schema version**: a
counter is state that has to be maintained and got right, while a rule that
recognises the shape it fixes is idempotent by construction and testable without
a fixture of old files. **It never fails a start**: a missing file is nothing to
do, a file that is not JSON is left for `Config::parse` to report with a line and
a column, and an unwritable dir is a warning — which is exactly why the `Tracker`
reader stays as the fallback for a config we cannot write. **It writes only when
a rule applied**, because the JSON round trip sorts the keys and reformats the
file, so that happens on the one start that migrates and never again. **The
previous file is `config.json.premigrate`**, its own name because
`config.json.bak` belongs to the first-run page and a boot that happened to
migrate would otherwise overwrite it.
Adding one is a `Migration { name, apply }` in the table. A rule must recognise
its own input and leave anything else alone: the name migration does not invent a
host for `"jira"`, because `Tracker`'s refusal already names the object to write
and a guess written to disk is worse than a message.

## The changed-files pane's git verbs are drawn from `git status`, not from its own list.
That list is `git diff <merge-base>` plus untracked files, so most
rows on a PR branch differ from the base because of a **commit** and are clean on
disk — `discard changes` there would offer to throw away nothing on some rows and
a commit's content on others, from a menu that cannot tell them apart.
`DiffFile::staged` / `unstaged` carry `git status`'s two answers, joined on by
path in the same closure that already reads them, and the menu offers exactly
what exists: staged → `unstage`, working-tree → `stage` and `discard changes`,
untracked → `stage`, and a row that is neither gets no git verbs at all.
`api::file_verb` keeps three rules the pane cannot: the path goes through
`edit::resolve_in_workspace` (relative, no `..`, under the root), the verb is
checked against a *fresh* status rather than the snapshot the click came from,
and **everything is refused while a session in that workspace is mid-turn** —
staging under a working agent changes what its next `git commit` picks up, which
is the "changed underneath it" case `pre_edit`'s stale notice exists for, except
this time it would be your doing. Only `discard` is confirmed, and that is the
asymmetry that matters: stage and unstage are each other's undo, while `git
restore` overwrites the working tree and git keeps no copy of content that was
never committed.

## The rebase button banks a dirty tree, and never on `refs/stash`.
`git rebase --autostash` is the obvious implementation and it is wrong here, for
two measured reasons. A failing autostash apply **exits 0** — git says "Applying
autostash resulted in conflicts" and then "Successfully rebased", so
`rebase_onto` reads a lost re-apply as success — and it parks the work on
`refs/stash`, which **every worktree of a repo shares**: a `git stash` in a
worktree is `stash@{0}` in the main checkout, so another tree could pop work it
never took.
So `api::rebase` banks with `git::bank_wip`: `stash create`, a ref of the
daemon's own at `refs/orchd/wip/<workspace>`, then `reset --hard`. Three
properties are deliberate. The **ref goes on before the reset**, so there is no
window in which the work exists only as a sha in memory, and it survives
`git gc --prune=now`, which a bare `stash create` object does not promise. It
**comes off only after a clean re-apply** — `git stash apply` exits 1 on a
conflict and leaves both sides in the tree as `UU`, which is where they can be
resolved, with the bank still standing behind them. And a rebase left **stopped
part-way owns the tree**, so the bank waits for the abort, which puts it back
because an abort is the undo of the press that took it.
`Workspace::banked` is **not** on `Tree`, because nothing measures it in the
sweep: the daemon knows because it did the banking, and a restart re-derives
every bank in the repo with a single `git for-each-ref` on main. Unmerged paths
refuse the whole flow — `stash create` answers "Cannot save the current index
state" — and untracked files never travel, so a base that adds a path you have
untracked is refused by name rather than by git's own header.

## One pty exit, one observer.
`spawn::watch_session_exit` is the only thing
that waits on a session's handle; it dispatches onward (a fix run's verdict goes
to `fix_pr::settle`). A second `pty.wait()` on the same handle would work and
then rot, because "is this over" would have two answers maintained apart.

## Main's claim belongs to the session record, and a relocation reuses the id.
`claim_main` runs before anything is created so a refusal costs no worktree and
no pty — but until the record is installed the map still describes the *outgoing*
session, whose exit watcher is entitled to settle it, and `release_main` keys on
the id. So the claim the incoming session just took gets handed back, and main
holds a live agent with **no occupant recorded** — the value `switch_main_to_pr`
reads before moving the checkout. `spawn::spawn_session` closes the window with
`reclaim_main` after the insert. Reproduced one run in four by the two-way swap
e2e flow, and invisible to every unit test.

## Mutating a durable store carries its own write, and the compiler now says so.
`automation`, `manual`, `stories` and `resolve_runs` are changed through
`Inner::with_automation` and its three siblings, which persist and log with the
caller's own context. Reaching for `store::save_*` at a call site is the shape
where one site gets the fix and the others quietly do not.
It was a paragraph, and it is `state::Durable<T>` now: `Deref` and deliberately
no `DerefMut`, so every reader goes on writing `inner.automation.get(pr)`
unchanged and `inner.automation.insert(…)` outside `state.rs` stops compiling.
**There were no offenders when it went in**, which is the argument for it rather
than against — nothing would have reported the first one, and the failure it
guards is a record changed in memory and never written, which looks right until
a restart drops it.

## You cannot self-review your way to a testable review thread — use the fixture.
`acknowledged()` (`forge/github.rs`) treats a thread whose last
comment is yours as answered, so a PR you comment on yourself has nothing
awaiting an answer, and `query_for` polls `author:@me` so the PR must still be
yours. `mise run fixture` builds a throwaway private repo whose threads are
posted by `github-actions[bot]`, which satisfies both; `docs/fixture-pr.md` has
the why and the two GitHub behaviours that cost an afternoon. It does not cover
`rerequest()` — a bot cannot be a requested reviewer, and nothing calls it since
the batch went. The review session has never made a real round trip either, so do
not read a green suite as more than that.

## The host's own file is `host.json`, and a hosted child must not write the host's files.
It carries the open checkout list — so the app opens what was
open — and `checkout_retention_days`. A child's `ORCHD_CONFIG_DIR` is its *own*
checkout directory, which is the trap: `recent.json` written by a child leaves
one single-entry list per checkout and none of them the list the add screen
reads, so `crate::start` writes it only when `host_origin` is absent and
`Host::open_checkout` is the other writer.
The sweep over `checkouts/` may delete **only what a daemon rebuilds** — the
skills plugin copy, `hooks.json`, `window.json`. `transcripts/` is the only
remaining copy of a conversation once a worktree is gone and a session record
survives because that copy does, so taking the directory would undo what `close`
does on purpose. Its safety cannot be borrowed from `worktree::reap_old`, which
is safe because it routes through `teardown`'s seven checks; a directory of JSON
has no such gate.

## A resume rebuilds a session's environment, so anything the daemon put there has to be re-handed.
The ask token always was, because `Session::new` mints a
fresh one on every spawn and the route compares it against the record. The
*post* token was not: only `triage.rs` set `ORCH_POST_TOKEN`, and a resume goes
through `spawn::spawn_session`, which knows nothing about it. So a resumed
review run came back able to ask you questions and unable to post its
proposals — reported by the agent as `ORCH_POST_TOKEN is absent from this
environment`, after it had read every thread. Two ways in, neither exotic: the
app restarting (`auto_resume` resumes every session that was live, runs
included) and the rail's own resume button (`api::revive` carries the recorded
`Pass`). The rule now lives
in `triage::mint_post_token` / `posts_proposals`, called by every spawn that
posts.
**`spawn::run_env` is the seam, and `clippy::disallowed_methods` now refuses
`launch::session_env` anywhere else.** Three sites built a session's environment
themselves — the worktree spawner, the story filer and `run_env` — and the two
that bypassed it were right only because neither carries a `Pass` today. The
difference between the two spellings is one variable an agent reports missing
hours later, which is what this entry is about.
`proposal_tokens` says it is deliberately not persisted, and that is still right
— the token is only ever compared against the record, so re-minting is the fix
and persisting would be the wrong one.

## A spare worktree is a workspace with no session, and that is the shape the reaper hunts.
The pool (`crate::spare`) keeps `spare_worktrees` trees cut and based ahead of
demand, because the wait it removes is the whole of the wait: measured on the
monorepo by the daemon itself, release build, `worktree ready` is **4,742ms** for
a cut against **84ms** for a claim, and the whole HTTP create drops from about 5s
to about 260ms. `create_worktree` had already written
down half of that — "the repo's own `WorktreeCreate` is usually the whole of the
wait".
**A spare is deliberately an ordinary workspace**, cut by `create_worktree`,
hooked by `run_worktree_hooks` and registered by `register_worktree`, because the
alternative is a second kind of worktree that every reader of `inner.workspaces`
would have to learn. The cost of that choice is that four existing behaviours can
see it, and each had to be answered:
`reap_old` hunts exactly this shape — "a tree with no conversation pointing at it
at all, which is the commonest shape of silt" — so a pooled id is excluded from
its candidates, and `spare::refresh` expires a spare on its own clock instead.
`adopt_pending_worktrees` pairs a lone pending session with a lone orphan
directory, and would have handed a session the spare; it is safe only because a
registered spare is not an orphan, its `known` set being `inner.workspaces.keys()`.
`CreateRun` is **one slot**, not one per workspace, so a background cut that
reported would overwrite the overlay a person is watching, interleave its hook's
output into theirs and flip `running` false mid-fetch. `model::Board` is the
argument that refuses the reporting at the call rather than filtering it later,
and `teardown`'s `WorktreeRemove` hook went quiet with it — that one was harmless
only while nothing removed a tree in the background, which both `reap_old` and
this pool now do.
And the claim itself does **no git**, which removes the thing that used to make a
double-create impossible: `git worktree add` refusing an existing path. So the id
is taken and cleared inside one `inner.write()`, and
`two_claims_cannot_take_the_same_spare` fails without it.
**The claim measures, every time, and never trusts the poller.** `Tree`'s defaults
are indistinguishable from a fresh tree, the poll interval is at least 30 seconds,
and a spare is a real directory anybody can `cd` into. 84ms against 4,742ms
saved is not a trade worth thinking about — and the first claim on a freshly cut
tree cost 524ms, still an order of magnitude the right side of the cut.
**Nothing here resets a tree.** A stale spare is discarded and cut again; one that
has been worked in, or that is no longer on the branch it was cut with, is
*promoted* — dropped from the pool and left standing as an ordinary workspace with
its branch. `git worktree remove` never deletes a branch and the teardown
preflight refuses a dirty tree, so the pool cannot destroy work by being wrong
about what it holds; the branch delete is `git branch -d`, never `-D`, so git
refuses a branch carrying commits rather than this daemon deciding it may.
**The fourth staleness class is not detectable and is not pretended to be.** The
base moving, the tree being touched and a dangling symlink are all measurable. A
dependency that appeared in main after the cut is not: it leaves a tree that is
clean, current, and missing a link the repo's setup hook would have made. The
answer is to re-run that hook on the idle spare each tick — idempotent by
contract, and by construction in this repo, `ln -sfn` behind an `[ -e ] && [ ! -L ]`
guard — and to expire a spare after `MAX_AGE` whatever git says. A repo whose
setup is neither idempotent nor cheap sets `spare_worktrees: 0`.

## Two `git worktree add`s at once fail on the config lock, and the spare pool made that reachable.
`git worktree add -b <branch> <remote-ref>` writes the new branch's upstream into
`.git/config`, takes `.git/config.lock` to do it, and **does not retry**. A second
add landing inside that window does not queue — it fails outright with
`could not lock config file .git/config: File exists`, followed by `unable to
write upstream branch configuration`, and the create fails with it.
Nothing in the daemon made two of them overlap until the pool did. It cuts a tree
in the background at boot, so any create in the first seconds of a daemon's life
races it by construction — which is exactly what `mise run app-check` does, and
the create it drives is *named*, so it never takes the spare and always cuts its
own. `bundle` went red on the tag run of `5ecc500` and green on the push run of
the same commit, which is the signature of a race rather than a break.
`AppState::cutting` is held across `create_worktree` — over the repo's own
`WorktreeCreate` hook as well as the daemon's fallback, because the hook cuts the
tree on that path and is just as able to hold the lock — and across `revive`'s
`worktree_rebuild`, which is a `git worktree add` too. `lock`, not `try_lock`
like `sweeping`: a create still has to happen, so it queues, and the wait it can
inherit is a whole cut.
**A race is not a deterministic test.** Without the mutex
`a_spare_cut_and_a_named_create_do_not_fight_over_the_config_lock` fails often
rather than always, so what it pins is that both paths can be driven at once and
both trees arrive; this entry is the rest of the evidence.
