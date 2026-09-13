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
