# The crates, and what a move breaks

Every entry here cost something. `CLAUDE.md` indexes them by their first
line; this file is the rest — what happened, what was measured, and why the
code is the shape it is.

## Four crates, all under `crates/`, and the root is the workspace and nothing else.
`orchd-base` the primitives, `orchd-repo` one checkout described,
`orchd` the runtime core, `orchd-serve` the daemon; `desktop/` sits on top.
The root manifest **was the `orchd` package as well**, and that is the thing to
remember about the last move: a manifest that is both is a default that is wrong
for every tool that has one. `cargo clippy` without `--workspace` linted it and
never `desktop/`, and `cargo about` and `cargo deny` rooted at it silently — so
the desktop shell's ~130 dependencies went unchecked and unlisted until somebody
passed `--workspace`. The name did not change with the directory, deliberately:
`orchd::…` is 162 paths and a dozen sentences, and a rename buys symmetry and
nothing else.
**A crate move breaks whatever reads a path**, and this is the fourth time: the
module ratchet in step 1, `check-module-routes.mjs` in step 2, `typos.toml` —
its `src/names.rs` exclude went stale and a list of computer scientists'
surnames failed the spell check — and **the version**, which is the one that
did not fail loudly. It moved out of the root manifest into five crate
manifests, and both readers of it (`mise run release` and the release
workflow's tag-matches-version step) go on `grep`ing the root; the workflow's
half compared the empty string against every tag and said nothing, because
nothing tags on an ordinary push. `[workspace.package]` holds it now — see
*Releases*. Three of the four failed loudly, which is the good case; look for
the fifth, and prefer the one cargo can enforce over the one a script promises.

## `orchd` is a library only. `crates/orchd-serve` is the daemon
, and it holds
both binaries. The router, `start`, `StartOptions`, `Server` and every
`start_*_poller` are there with `host`, `hooks`, `firstrun` and `ws` — because
`lib.rs` reached all four, and `lib.rs` is not a module so the graph never saw
it. The line to remember: **`orchd` is what the daemon knows; `orchd-serve` is
the daemon.**
Two consequences bite immediately. **`cargo build --bin orchd` no longer works
from the root** — it is `cargo build -p orchd-serve --bin orchd`, and every
mise task, workflow and e2e runner says so. And **`cargo about` and
`cargo deny` needed `--workspace`**: both were rooting at the `orchd` package,
which now produces no binary, and turning that on showed that the desktop
shell's ~130 dependencies had never been checked or listed at all. The notices
went from 111 crates to 355. That hole predates the split.

## The words are `host`, `checkout` and `session`, and `repository` is reserved.
`host` names the process role above the daemons, not a UI metaphor — it began as
`firstrun::BootstrapHost`, the trait by which the app gave a server it hosts the
window side, and `host.rs`, `/api/host/*`, `/ws/host` and `host.json` read as one
vocabulary. The trait is gone with the bootstrap server; the word stayed. Its one cost is named: `api::guard`'s rules are about
the HTTP `Host` header, so `/api/host/checkouts` sits beside "the Host rule" and
reads confusingly for a moment — a collision in one module, where a metaphor
would have been in every sentence. **`repository` stays reserved for
`state::Repos`**, the GitHub owner/name pair, which is why the rail lists
*checkouts* and nothing in the product is called a board.

## `crates/orchd-repo` is what a checkout is
: `config`, `forge`, `diff`,
`patch`, `skills`, `launch`, `migrate` and the rest — everything that reads or
describes one repository and keeps no session state. It was the cheapest of the
three splits, because it was sandwiched between two that already existed.
`sibling_bin_dir` moved down into `launch`, its only caller, and one `skills`
test became `tests/skills_are_named_after_commands.rs` because its two halves
are now in different crates.

## This is a workspace, and `crates/orchd-base` is the first crate out.
Twelve modules — `child edit git guard headroom model proc proposal pty
secret timing window` — and `cargo` now refuses an import from any
of them back up into the daemon. `crates/orchd/src/lib.rs` re-exports every one at
the path it always had, so **`crate::git::…` still reads the same everywhere** and the move
cost no call site a rename. `docs/crate-split.md` has the plan, the measurements
and what the first step actually cost.
Four things about it are worth knowing before touching the next step, and all
four are one fact — **`#[cfg(test)]` does not cross a crate line.**
`migrate` and `names` belonged in base by the graph and stayed in `orchd`
because their *tests* assert against `config` and `spawn` — `migrate` has since
gone down one more step to `orchd-repo`, beside the `config` it repairs, and
`names` is still in `orchd`. `testutil` is a
**feature** (`test-util`), not a `cfg(test)` module, because a dependent crate's
tests cannot see one — base owns `scratch`, `git`, `scratch_repo` and
`TRUE_BIN`, and `orchd`'s `testutil` re-exports them, so there is one `scratch`
in the workspace. `ts-rs` is an **optional dependency** gated by that same
feature, because as a dev-dependency the `TS` derives vanish exactly when
`orchd`'s tests need them.
And **there are four generated type files now, written in a fixed order**:
ts-rs truncates `export_to` per crate *and* exports a type's dependencies, so a
crate's run rewrites its dependencies' files with only the subset it
references — `cargo test --workspace` left `base.d.ts` holding 1 type where it
should hold 15. The gates run the crates **top of the graph down**
(`orchd-serve`, `orchd`, `orchd-repo`, `orchd-base`), which leaves each file
written last by its owner. `orchd-base` writes `web/base.d.ts`, `orchd-repo`
`web/repo.d.ts`, `orchd-serve` `web/serve.d.ts`, `orchd` `web/snapshot.d.ts`, and the `import type … from "./base.d"` between them is
only right because `.cargo/config.toml` points both at one `TS_RS_EXPORT_DIR`.
Set there rather than in the gates, so a bare `cargo test` does not leave a
stray `bindings/`. A type that moves between the crates moves between the files,
and the SPA's `import('../snapshot').X` has to follow — `tsc` names every one.

## `git` is a directory, and the split was the banners.
It was 4,457 lines —
47% of `orchd-base` — already partitioned by banner comments, which became the
file names: `exec` (the timed runner), `status`, `refs`, `unpushed`, `worktree`,
`bank`, `review`. No item moved between them, `crate::git::…` still resolves for
every call site through `mod.rs`'s re-exports, and items the files share are
`pub(super)` rather than `pub`.
**The tests did not follow, and that is measured.** They group by *fixture*
rather than by section: `scratch_repo` is shared by twelve tests spanning refs
and worktrees, `amend_repo` by fourteen, `bank_fixture` by four. Splitting them
needs a shared fixtures module and a hand assignment of 61 tests, and what a
reader navigates while changing behaviour is the shipped code.
**And the split broke the module gate, which is how the gate earned its keep.**
`crates/orchd-base/src/git/tests.rs` is a file rather than a `mod tests {}` block, so the cut that
removes test code did not apply and 2,000 lines of tests read as shipped —
producing a `git <-> review_commit` cycle out of a move that changed no shipped
line. `declaredTestOnly` is the fix: a file whose own directory declares it
`#[cfg(test)] mod <name>;` is test code. Checked both ways, since a rule that
skips too much is worse than the bug.

## `cargo fmt` is the formatter now, gated in CI and the hook.
The tree was
formatted in one commit, and `rustfmt.toml` says what was measured to keep the
defaults — including why `wrap_comments` stays off, which is the setting that
made it affordable. One thing to turn on per clone, beside the hooks line:
`git config blame.ignoreRevsFile .git-blame-ignore-revs`.
