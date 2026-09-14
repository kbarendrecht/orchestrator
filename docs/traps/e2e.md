# The e2e flows

Every entry here cost something. `CLAUDE.md` indexes them by their first
line; this file is the rest — what happened, what was measured, and why the
code is the shape it is.

## An e2e flow must make idleness a condition, not an assumption.
Every
mutating route refuses a workspace whose session is mid-turn, and the rebase
flow settled its session once at the top and then made ten calls against that
one reading. About **one full run in six** failed with `<id> is working here` —
the agent's hooks land on the daemon's clock, not on the flow's. `t.settled` is
idempotent and costs one snapshot read, so the fix was to call it before each
call rather than once. A flaky gate is worse than no gate: it is what teaches
everybody `--no-verify`.

## `mise run e2e` needs no product change, because the agent is a PATH lookup.
The daemon spawns `CommandBuilder::new("claude")` and reaches GitHub only through
`Command::new("curl")`, so a shim earlier on PATH substitutes either without the
daemon knowing. Everything else in those flows is real — real worktrees, real
branch moves, real `stash create` carries, real locks, the real API, and the hooks
read out of the settings file the daemon itself wrote, so a change to
`hooks::write_settings` changes what they exercise instead of passing them by.
Read `docs/e2e.md` before adding one: it has the sandbox options, why every wait
is a condition rather than a sleep, and the limits (no SPA, no real round trip to
GitHub, nothing about what a fix run *does*). What they buy is the class of fault
unit tests structurally cannot see: the first full run turned up a `claim_main`
race, and driving them from the hook turned up what git hands a hook.

## GitHub is two programs, and the write half is `gh`.
`fake-curl.mjs` was the whole of GitHub for a year, because every flow stopped at
reading: the poll, and nothing else. The review session's flow is the first that
*writes*, and writes do not go near `curl` — `forge::github_write` shells `gh` from
the main checkout, so a sandbox with canned PRs and the machine's real `gh` would
have reached api.github.com on its first reply, with a fixture token, against
`acme/monorepo`. Both shims are therefore installed by the same `if (repo)`: a
forge that is half real is worse than one that is not there, because it fails as a
network error on somebody else's machine rather than as a missing binary on yours.

What the `gh` shim keeps real is the part worth testing. It asserts nothing; it
appends the argv and the stdin body to `gh.jsonl` and answers `{}`, so the flow
reads back exactly what `github_write` built — and that is where two things are
pinned that no unit test can see. A reaction hangs off the comment directly
(`repos/o/n/pulls/comments/<id>/reactions`) while a reply is nested under the PR
(`repos/o/n/pulls/<pr>/comments/<id>/replies`): two REST shapes easy to write down
wrongly and impossible to tell apart from a green test. And the footer —
`(via orchestrator)`, which `forge::acknowledged` reads to decide a thread has been
answered — is asserted **on the bytes that left**, so a reply without it is caught
here rather than by a reviewer six weeks later wondering why the thread still says
it is their turn. Checked against deliberate breakage: dropping the footer from
`with_footer` fails the flow.

The read half grew a second document at the same time. `threads_query` is keyed on
`headRefOid`, which only it selects — the poll asks `search(query:)` and the poll's
own paging selects neither, so all three are told apart by a field the parser reads
rather than by a phrase. One thing the shim deliberately does **not** answer is
`answerable`: `Threads::mark_answerable` derives it from the last comment's author
and your own 👍, and it is the rule the re-request rests on, so a flow says who
spoke last and the daemon decides whose turn it is. Canning it would have tested
the shim.

## A flow's own git races the daemon's, and the full suite hides it.
`park main` failed **3 runs in 8** on `git commit -qam work`, `rebase` **1 in 8** on
a CI runner, and `park main` again **5 in 8** on macOS — always
`Unable to create .git/index.lock`. A flow sets up state by doing real git in the
real checkout while the daemon reconciles that same checkout on its own clock, and
git has no wait-and-retry of its own.

**The suite hid all three.** Run in sequence a flow lands in a gap between
reconciles; run alone it lands on one. So `mise run e2e` was green 26/26 on both
platforms while three flows sat at a 12–37% failure rate in isolation. That is what
`mise run deflake` is for — each flow N times, one at a time — and it is the only
reason these were found.

`harness.git` retries, bounded, and **only** on that message: a loop that swallows
the real fault is worse than the flake. Prevention was the wrong shape, since it
would mean a flow coordinating with the daemon's reconcile schedule — unavailable,
and a fiction no real user gets either.

**Use `gitMayFail` rather than a raw `spawnSync` for the one command whose failure
is the fixture.** `26-move-refusals` passed twelve times alone and failed in the
suite because it shelled `git rebase` directly: a rebase that loses the race fails
*before it starts*, so the caller sees a non-zero exit and no rebase in progress —
which reads exactly like git having finished cleanly.
