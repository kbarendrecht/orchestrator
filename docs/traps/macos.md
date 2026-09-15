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

## A macOS runner can open the window, and it still cannot be clicked by name.
Measured rather than reasoned about, in a throwaway `workflow_dispatch` workflow
that has since been deleted — its findings are the whole of what it was for.

**The window opens and WKWebView renders.** GitHub's macOS images carry an active
Aqua session with Screen Recording pre-granted to the runner's shell, so the app
launches and `screencapture` proves it: a real frame, the menu bar reading
`Orchestrator`, the overlay traffic lights and the first-run screen fully painted.
The bundled binary opened its window in **128ms**; the bare tarball binary did the
same but with the menu bar reading `orchestrator-desktop`, because outside a bundle
there is no product name.

**`open` works, and #10329 does not reproduce.** The app is unsigned by choice, and
[actions/runner-images#10329](https://github.com/actions/runner-images/issues/10329)
reports an ad-hoc-signed app hanging on launch on macos-14 arm64 with no confirmed
fix. It does not happen here: `open "$PWD/$APP"` returned 0 and the app ran. The
first attempt *did* fail, and that was the experiment's own bug — `open -a` takes an
application **name** and hands a relative path to that same lookup, so it reported
"Unable to find application named target/release/…" and proved nothing. A probe that
fails for its own reasons reads exactly like the thing it was looking for.

**Accessibility is granted; the web content is still invisible.** `AXIsProcessTrusted`
is true on a runner, and System Events reads the window's frame straight out of the
tree (`Orchestrator, 700, 25, 1728, 970`) — which is the permission synthetic input
needs. But `entire contents of front window` is **empty**: WebKit builds the DOM's
accessibility tree lazily, exactly as Chrome, Firefox and Electron do. The switch
assistive technology throws is `AXManualAccessibility`, set from outside on another
process, and **wry does not implement it**: `AXManualAccessibility` answers `-25205`
(`attributeUnsupported`) and `AXEnhancedUserInterface` answers `-25208`
(`notImplemented`). Electron implements both, which is why the trick is documented
for it and does not carry over. So a click *by element name* is not available without
changing the app.

**A coordinate click drives the whole stack.** With Accessibility granted, `click at
{x, y}` reaches the webview: clicking `Choose a folder…` opened a real
`NSOpenPanel` titled "Choose the main checkout" — synthetic input → WKWebView → the
page's JS → Rust → `tauri-plugin-dialog`. It works, and it is a *coordinate*: a
layout change moves the target and the failure reads as "the click did nothing",
which is the same argument `page-check` already makes against pixel baselines.

**Which is why `tools/app-check.mjs` drives the API instead.** What clicking would
add is the window chrome and the native dialogs, and neither is what #16 or #17
broke. The remaining route to clicking by name is `tauri-plugin-wdio-webdriver`,
which embeds a W3C WebDriver server in the app — debug-build only, and it needs the
capabilities entry `main.rs` deliberately refuses ("this crate exposes no IPC
surface at all"), so it would drive a binary that is not the one that ships.

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

## A spawned `sysctl` answers for the child, not for this process.
`sysctl.proc_translated` is a **per-process** sysctl, and the translation check
reads it by spawning `sysctl` — so the answer is the *child's*. That is the right
measurement and it had the wrong conclusion bolted to it: "a translated child" was
reported as "this app is running under Rosetta", and the remedy offered was Finder
▸ Get Info ▸ uncheck "Open using Rosetta".
Issue #18 is the case that separates them. All three binaries were arm64, `file`
said so, and `git` still failed with `unable to load libxcrun … need 'x86_64'`. An
arm64-only binary cannot be translated, so the app was native and the processes it
started were not: the x86_64 preference came from whatever launched it. The
reporter was sent to an app bundle that does not exist — it was a mise install,
three binaries under `~/.local/share/mise`.
The spawn stays, because the child is the process class that actually fails; what
was missing is `std::env::consts::ARCH`, which is the half a spawned `sysctl`
cannot report. `aarch64` with a translated child is `Translation::Children`;
`x86_64` with one is `Translation::App`.
**The remedy on the `Children` arm was wrong, and a test held it in place.** It
said the x86_64 preference was inherited from whatever launched the app and told
people to use `arch -arm64` — a guess, written as though it were the finding. The
reporter's launch record disproved it: the launching application was itself arm64,
and the preference came from our own bundle. See the entry below. The remedy is to
rewrite the bundle, and the test now refuses the old string by name.
**The wording is the deliverable, so the wording is what is asserted.** The defect
was never in the detection. `translation_warning` is its own function for no
reason other than that a sentence no test reads is a sentence that can say
anything.

## A shell-script `CFBundleExecutable` has no architecture, so the plist must declare one.
`--install-desktop-entry` writes `/bin/sh` as the bundle's executable — on purpose,
and the reasons are good: a copy goes stale at the next `mise up`, a symlink out of
a bundle is what code-signing rejects, and the binary should stay where its
installer put it. The cost was invisible and total.
**LaunchServices reads the architecture off the `CFBundleExecutable` Mach-O, and a
script is not one.** With no `LSArchitecturePriority` in `Info.plist` it built
`BinaryOrderPreference = {x86_64, arm64}`; `/bin/sh` is universal, so the app ran
**translated**, and every process it started inherited that. The symptom was
nowhere near the cause: `git` could not load `libxcrun`, and the daemon reported
that the checkout was not a git work tree.
This is #18, and it took a reporter reading `log show` on the machine to find —
the launch job, the two `CPUType` values in order, and `rosettaAnalyze` running
against `/bin/zsh` with our install as the responsible path. **Nothing in the app
can see it**: `sysctl.proc_translated` answers for the process that asks, the
binaries are all arm64, and `file` on every one of them says so.
`info_plist` declares `LSArchitecturePriority` now, and it names
`std::env::consts::ARCH` rather than a literal `arm64` — the bug reversed is just
as bad, and `--install-desktop-entry` runs on whatever machine installed the
binary rather than only on the one that released it.
**The effect is reproducible on a macos-14 runner, and was measured.** A throwaway
`workflow_dispatch` probe — the same method as the entry above, deleted once it had
answered — built two bundles differing *only* in that key, each with a `/bin/sh`
script as its executable, and launched them through `open`:

| bundle | `sysctl.proc_translated` | `uname -m` |
|---|---|---|
| no `LSArchitecturePriority` | **1** | **x86_64** |
| `LSArchitecturePriority = arm64` | 0 | arm64 |

So the runner reproduces the bug exactly, which it can only do because **Rosetta is
installed there**: `oahd` is running and `arch -x86_64 /usr/bin/true` succeeds on
macos-14 (14.8.9). That was the open question — with no Rosetta an x86_64
preference cannot take effect, the bug would never appear on a runner, and a check
asserting its absence would guard nothing.

The unit test `the_bundle_declares_the_architecture_its_binary_was_built_for`
asserts the key is present and names this build's own architecture, which is what
can be checked without a Mac. What the measurement adds is that the *effect* is
checkable too, on a runner, for the cost of one `open`.
