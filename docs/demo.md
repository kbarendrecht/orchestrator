# Recording the README's demos

The README carries one video, recorded by
[`tools/video/run.mjs`](../tools/video/run.mjs); [the last
section](#the-readme-video) is how. It replaced eight GIFs, and
[`tools/demo.mjs`](../tools/demo.mjs), which recorded them, still records one
feature per clip:

- the default take — three agents over **one** repository, swapped between,
  then asked a question live.
- `--multi` — **two** repositories in one window, each with its own sessions,
  changed files and PRs.
- `--panes` — the PR pane and the review queue, cropped to the band that holds
  both.
- `--revive` — the diff viewer, then the checkout's daemon killed and every
  session coming back.
- `--diff` — one changed file read as a diff, split, then made editable in place.
- `--find` — a workspace searched, the hits narrowing per keystroke, one opened as
  its own file pane.
- `--skill` — the skills the daemon ships, listed by Claude Code's own menu
  inside a session.
- `--procs` — a managed process in the drawer, restarted on camera so its own
  output decides the health dot.

The video's setup builds on the one below, which the GIFs needed first.

The script's own header carries the mistakes that cost a take each. This is the
setup it assumes, and every step below is here because skipping it produced a GIF
that had to be thrown away.

## Why it is not driven by the e2e sandbox

[`tools/e2e/harness.mjs`](../tools/e2e/harness.mjs) would be the obvious host — a
real daemon, a real repo, offline and deterministic. It cannot be, and the reason
is in `fake-claude.mjs`'s own header: it honours the four things the daemon reads
and **paints nothing to the pty**. The centre pane is the picture, so the agent has
to be the real one.

That is the whole trade. The recording costs Claude Code tokens, it is not
deterministic, and it is therefore **not a gate** — nothing in CI runs it.

## The checkouts

Throwaway clones, with no window open on them:

```
mkdir -p ~/development/orchd-demo
git clone ~/.cache/orchd-fixture/repo ~/development/orchd-demo/inventory-api
git clone ~/development/orchestrator  ~/development/orchd-demo/orchestrator
```

`~/.cache/orchd-fixture/repo` is what [`mise run fixture`](fixture-pr.md) builds.
One small source file is enough: the prompts are about that file, and an answer
citing `src/inventory.js:8` is what makes the picture read as real work.

**Workspace trust is per repository root, and a fresh clone has none.** Without it
`claude` refuses, the session exits before its first turn, and the daemon then
forgets it and removes the worktree — so the rail simply stays empty and nothing
anywhere says why. `~/.claude.json` records it under `projects`:

```
"/home/you/development/orchd-demo/inventory-api": { "hasTrustDialogAccepted": true }
```

Accepting it once by running `claude` in the directory does the same thing.
**It does not inherit from a parent directory** — a trusted `~/development` above
the clone changes nothing. Worktrees under a trusted checkout are covered, because
the key is the repository, and a worktree's repository is the one it was cut from.

## The daemon

One checkout, for the default take:

```
export ORCHD_CONFIG_DIR=~/.cache/orchd-solo/config
export DISABLE_AUTOUPDATER=1
cargo build -p orchd-serve --bins
./target/debug/orchd --main ~/development/orchd-demo/inventory-api
```

Two, for `--multi` — this is `--host`, the same mode the app runs in, and it
prints nothing: read the port out of `$ORCHD_CONFIG_DIR/orchd.log`, and each
child's port and token out of `/api/host/checkouts`.

```
./target/debug/orchd --host ~/development/orchd-demo/inventory-api \
                            ~/development/orchd-demo/orchestrator
```

**`DISABLE_AUTOUPDATER=1` is not cosmetic.** Claude Code prints
`Update available! Run: mise upgrade claude` into the pty whenever it is behind,
and it ships often enough that chasing the version is a race you lose — 2.1.272
was published while a take recorded on 2.1.270. The variable is inherited by every
session the daemon spawns. The app's *own* upgrade bar is separate and the script
dismisses it.

## The sessions

The script seeds and records; it does not create. Per checkout:

```
T=<that child's token>   H=http://127.0.0.1:<the host's port>
for w in reserve-stock low-stock-alert; do
  curl -sS -X POST "http://127.0.0.1:<that child's port>/api/worktree" \
    -H "x-orch-token: $T" -H "origin: $H" \
    -H 'content-type: application/json' -d "{\"name\":\"$w\"}"
done
```

The field is `name`. A `workspace` key is accepted by serde and ignored, so the
daemon invents one of its own and the rail fills with
`delegated-sewing-chameleon`.

Two things to get right before recording:

- **Clean the worktrees before spawning, never after.** A session that finds its
  own edit reverted under it says so, at length, in the middle of the frame.
- **Start the sessions fresh.** The rail labels a row with the session's title,
  which Claude Code derives from the *first* exchange — so a session seeded once
  keeps that title however the prompts are reworded.

## Then

```
mise run demo                                    # one repository
mise run demo -- --multi --out target/demo-repos.gif   # two
```

Seeding and recording split apart when there are several checkouts, because each
repository needs prompts about its own code:

```
node tools/demo.mjs --port $PORT --seed-only --checkout inventory-api --seeds 'a|b'
node tools/demo.mjs --port $PORT --seed-only --checkout orchestrator  --seeds 'c|d'
node tools/demo.mjs --port $PORT --no-seed --multi --out target/demo-repos.gif
```

It prints the size. Over about 8 MB is too much for a README, and `--scale 900` or
`--fps 8` is the lever.

It encodes with the system `ffmpeg`, which `[tools]` does not carry. Without sudo,
`mise exec conda:ffmpeg -- sh -c 'command -v ffmpeg'` installs one and prints
where, and that directory on `PATH` is enough.

**Two things the takes need that nothing in them says.** `--panes` is cropped, and
the value is not a default: `--crop 1440:230:0:670` is the band that holds the PR
pane and the review queue at a 1440x900 capture, derived by recording once
uncropped and measuring. And `--procs` wants a **live** session in main, which the
daemon does not guarantee: auto-resume brings back the sessions it has, so a main
whose session is dormant leaves the rail without the row the take clicks, and the
take dies on a 30-second selector timeout naming a session id that the API happily
reports. `POST /api/session/<id>/resume` is the fix, before recording.

## The one fabricated thing

**The review queue is demo data, and nothing else in any of the four is.** The
daemons are real, the worktrees are real, and a real Claude Code wrote the diffs
the changed-files pane shows.

It has to be. The built-in queue lists open PRs somebody else wrote and marks the
ones that ask for your review (`review-requested:@me`), which is what colours a row
and what the `asked` filter keeps; GitHub will not let you request a review from
yourself, and the fixture's second identity is
`github-actions[bot]`, which [cannot be a requested reviewer](fixture-pr.md) — the
same wall that leaves `rerequest()` unverified. A genuinely populated queue wants a
second human account.

So [`tools/demo-reviews.mjs`](../tools/demo-reviews.mjs) emits a `Queue` in the
shape [`reviews-json.md`](reviews-json.md) documents, and the recording checkout
points `reviews_command` at it:

```
"reviews_command": ["node", "<repo>/tools/demo-reviews.mjs"]
```

That is an ordinary documented setting, not a patch: nothing in a release reads
that file, no default points at it, and the repository the rows describe is the
same throwaway fixture the rest of the recording uses. The README says so under
the picture.

## Which workspace a scene wants

**Three scenes name their own row rather than taking the first one**, and that is
not tidiness: the rail sorts by recency, so `ids[0]` is whichever session last
took a turn. `--diff` and `--find` ask `/api/state` for a session in a
*worktree*, because main carries no agent's diff; `--procs` asks for the one in
*main*, because `main_processes` is main's and a worktree's drawer is empty
unless `worktree_processes` is set.

**Neither pane below the fold may be empty**, with one deliberate exception, and
both are configuration rather than luck. The PR pane needs the checkout to resolve a GitHub repository — a
clone whose only remote is a local path resolves none, and the pane then reads
`no GitHub upstream remote configured`; `"repo": "kbarendrecht/orchd-fixture"`
in the recording checkout's config is what fills it. The review queue needs
`reviews_command`, below. A take with either one empty has to be recorded again,
so check both before rolling.

**The exception is the second checkout in `--multi`.** It is a clone of
this repository, which commits straight to `main` and therefore has no open PRs
and no review requests — so its PR pane reads `none open` and its queue reads
`Nothing waiting on you`. Both are true of that checkout, and the alternative
was either a second clone of the fixture (two checkouts of one repository,
standing in for a claim about two repositories) or fixture rows attributed to a
repository they do not belong to. Leave it empty; it is the honest frame.

## `--panes` and `--revive`

`--panes` folds both panes before the camera starts and opens them one at a time,
because "these panes have things in them" is a claim about content and content
arriving is the only way a still strip can show it. It is recorded whole and then
**cropped** (`--crop 1440:230:0:670`): the two panes are perhaps a seventh of the
frame between them, in opposite corners, and at README width a whole-window take of
them is mostly centre pane.

`--revive` needs the **host**, not a solo daemon, and that is not a detail. A solo
`orchd` mints a new token when it restarts and the page is holding the old one, so
nothing can reconnect. Under a host the child's death is noticed, the daemon is
started again, and the new port and token are pushed to the page over `/ws/host` —
which is the path the app itself runs on. `--kill <substring>` names the child by
its `--main` argument.

**A host over that one checkout, not the two-checkout one.** With two, the page
falls back to the other checkout's session when the diff closes, so the kill
happens off screen and the take is a minute of the wrong pane.

Two things it depends on:

- **The sessions must have taken a turn.** `auto_resume` drops a record with no
  transcript — there is nothing behind it — so three freshly spawned sessions come
  back as nothing at all. Seed them first.
- **The host restarts a dead child once**, and only clears that budget after the
  child has lived for `HEALTHY_UPTIME` (60 s). A second kill inside a minute leaves
  the checkout down, and the take records an empty rail.

## The README video

[`tools/video/run.mjs`](../tools/video/run.mjs) records one video in place of the
GIFs: ten scenes in one take, from the desktop app's splash to a daemon killed and
every session back. It uses [Argo](https://github.com/shreyaskarnik/argo), which
drives the page with Playwright and adds the camera moves, and lays the captions
from [`tools/video/captions.json`](../tools/video/captions.json) over the export.

```
mise run video -- --port <host page port>          # target/video/orchd.mp4
mise run video -- --port <port> --captions-only    # reword, no new take
```

**Its own install, in `tools/video/`.** Argo needs Playwright 1.59 or newer, and
`tools/` pins 1.49.1 to the WebKit build on disk; one `node_modules` cannot hold
both. The task also carries `conda:ffmpeg`, because Argo passes `-fps_mode`, which
ffmpeg 4.4 — Ubuntu 22.04's — refuses. A `PATH` prefix does not do it: under
`mise run`, `/usr/bin` wins.

**A host with two checkouts**, in this order: the fixture clone first, a clone of
this repository second. The first is the one with the PR, the review queue and the
daemon that is killed; the second is where the new session is cut, and the task
typed into it is about `web/js/theme.js`. Read the port out of the host's
`orchd.log`, as above.

```
./target/debug/orchd --host ~/development/orchd-demo/inventory-api \
                            ~/development/orchd-demo/orchestrator
```

What the take needs before it rolls:

- **The clone of this repository cannot push.** Its `origin` is the real one, and
  every agent on camera works in it: `git remote set-url --push origin
  no-push-from-demo-clone` once. Pull it to current `main` too, so nothing an agent
  shows is older than the tree.
- **Sessions in both checkouts that have taken a turn.** Three in the first, each
  with a small edit, and one of them ending on a reply that names
  `src/inventory.js:<line>` — the path scene clicks it (`--path-text` to change
  it). Two in the second. `POST /api/session/<id>/tell` seeds one without a page.
- **Nothing extra from the last take.** Each take adds a session in the second
  checkout and a `handle-review` run in the first; stop both, or the rail grows a
  row per take and the review run carries on against the fixture PR.
- **A minute between takes.** The take kills the first checkout's daemon, and the
  host restarts a dead child once per `HEALTHY_UPTIME`.
- `--codex` cuts the new session as Codex. It needs Codex signed in on this
  machine and on the host's `PATH`; the agent menu exists only with two agents.

**Two things in the page are changed, and both are the recording machine's.**
WebGL is hidden from the page, so the terminal is drawn by xterm's DOM renderer —
how the Linux app draws it — and the path scene can find the text. And the
agent-upgrade bar is hidden: it says this machine's Claude Code is behind, and it
covered the diff toolbar. The review queue is the demo data described above.

**A README cannot play a video committed to the repository.** Drag the MP4 into an
edit of `README.md` on github.com once, and use the link GitHub gives back.
