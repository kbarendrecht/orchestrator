# Developing

```
mise install
mise run deps                            # tools/node_modules; every task that
                                         # needs it depends on this already
git config core.hooksPath .githooks      # once per clone
git config blame.ignoreRevsFile .git-blame-ignore-revs   # once per clone
cargo test --workspace                   # the four crates
mise run check-web                       # type-check and lint the SPA + its graph
cargo run -p orchestrator-desktop        # the app: it hosts the page and spawns
                                         # one orchd per checkout
mise run shot                            # screenshot the running app (drives Chrome)
mise run fixture                         # a throwaway PR to drive the review flow
```

`cargo run -p orchd-serve --bin orchd -- --main /path/to/your/repo` runs one daemon
headless and prints a tokened URL. `mise run shot` drives Chrome while the app runs
in **WebKitGTK**, so it is good for layout and not the last word.

| Where | What is in it |
| --- | --- |
| [`docs/architecture.md`](docs/architecture.md) | how the running system fits together, and the module map |
| [`CLAUDE.md`](CLAUDE.md) | the traps, one line each |
| [`docs/traps/`](docs/traps/gates.md) | what each trap cost |
| [`docs/assumptions.md`](docs/assumptions.md) | what the daemon assumes, and what breaks when each is false |
| [`docs/spec.md`](docs/spec.md) | the requirements the `(§N)` comments point at |
| [`TODO.md`](TODO.md) | what is open |

Releases are CalVer (`year.month.n`), and `mise run release` cuts one: it bumps
the version from the clock, **waits for `check` to go green on the commit you are on**, then
commits, tags and pushes. The waiting is the point — `check` is the only thing
that runs the suite on macOS, and a tag pushed before it answers may publish
nothing.
