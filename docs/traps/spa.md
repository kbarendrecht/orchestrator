# The SPA, the webview, and the two module graphs

Every entry here cost something. `CLAUDE.md` indexes them by their first
line; this file is the rest — what happened, what was measured, and why the
code is the shape it is.

## ES modules work in the real webview — measured, not assumed.
WebKitGTK
**2.50.4** ships here, and a spike drove the actual desktop window (not Chrome,
not playwright's WebKit): a `type="module"` script imported a second module over
a `/js/:file` route, the relative import resolved, strict mode was on, and the
vendored classic globals (`window.__ORCH__`, `Terminal`, `Prism`) were all
present by the time the module ran — module deferral happens *after* the classic
scripts, so the ordering is safe. Two things the spike settled that matter for
the migration: the content type must be a JavaScript one (`text/plain` loads and
then refuses to execute), and modules come from `include_str!` like everything
else, so **each new module needs an entry in the route's match and a rebuild** —
adding a JS file stops being a JS-only change.

## The page holds one snapshot and one socket per checkout, and `snap` is the active one.
`core.snapshotOf(path)` is any checkout's; `snap` is whichever
checkout you are in, and **the checkout is derived from the selection** —
`activeCheckout()`, never written. A second variable saying which checkout you
are in is a second source of truth, and the one that goes stale is whichever the
next reader forgets. The one exception is a remembered path used *only* when
nothing is selected, so activating an empty checkout does not put you back in the
first one.
Three consequences. **`call` and `get` aim at the active checkout**, so anything
acting on a row in another one uses `callFor(sessionId, …)`, which derives the
target the same way. **Every per-target key carries its checkout** (`termKey`,
`wsKey`): `main` names a workspace in every daemon and `proc:main:ng-watch` a
process, so an unqualified map hands you the other checkout's live terminal — and
`orch.procOrder` is *persisted* under that key, which no amount of disposing
terminals undoes. And **attention is global**: the waitbar, `MOD+Space`,
`Ctrl+Tab` and the screen-reader announcement read every checkout, because a bar
saying "2 need you" while its own chord answers "nothing waiting on you" is the
two disagreeing about one fact.

## A page served by a host is cross-origin to every child daemon.
Every call
carries `x-orch-token`, which makes it a non-simple request, so the browser sends
`OPTIONS` first and drops any answer that does not name its origin.
`api::guard` answers that for the one origin `host_origin` names — never `*`,
which would let any page in the browser drive the daemon. This shipped missing
and the symptom was a board that drew from its websockets (CORS does not cover
those) and could then do nothing at all.
**One port per daemon, and the host proxies nothing.** The cost of not proxying
is one extra origin per daemon — the same string for every one, handed in at
spawn, so it is one rule instantiated N times rather than N rules that have to
agree. The cost of proxying would be a second loopback hop on the keystroke
path, and the pty socket is nothing but small frames in both directions: this
repo already had to set `TCP_NODELAY` because Nagle plus a delayed ACK is ~40 ms
per round trip on exactly that traffic.

## `orchd --host <checkout>…` is how the multi-checkout page gets driven without a screen.
The app is the only other host and it needs a window. It is a *real*
host: each checkout gets its state directory under `ORCHD_CONFIG_DIR` and lands in
`recent.json`, so point that variable at a scratch dir or it writes into your own
config. `mise run shot` then works against it.

## The SPA is a module graph, not a file.
`web/js/core.js` is the shared layer
— the fetch wrappers, the DOM shorthands, the snapshot, the selection, the UI
scale, and the vocabulary every pane needs to describe a session (`stateLabel`,
`dotClass`, `isArchived`, `pending`, …). The features beside it are `term`,
`rail`, `diff`, `review` (+ `review-diff`), `queue`, `settings`, `theme`,
`drawer` and `open`. `app.js` is what is left over: boot order, the websocket,
the keyboard map, the window chrome.
**`core` is a floor, and what makes it one is that nothing feature-shaped lives
there.** Two things did and are out: the theme — 400 lines of palette solving,
font detection and `localStorage` reading that two modules use — and the
drawer, whose *state* was already in `core` (per-checkout keyed) while its
400-line render sat in `app.js`, one feature across two files. `core` now imports
nothing of the SPA's, which is the rule stated as a property rather than as an
intention.
`mise run check-web` prints the current module and dependency count, and the
count is printed rather than written down here: a number a file states and no
tool verifies is one that rots, which this entry proved by saying `app.js` was
"under a thousand lines" long enough to be wrong by a factor of two.

## The daemon's module graph is a DAG now, and `mise run check-modules` refuses a cycle.
It was a ratchet for a long time and its own header carries the three
passes that got it here: 17 mutual pairs and a 16-module strongly connected
component at the start, seven broken by moving a *shape* into the module that
owns shapes, six more when `state::Inner`'s four feature types followed them
into `model`, and the last two by the inversion below.
**The swap from ratchet to rule is the point, not the count.** A pair list
cannot see a three-module cycle, so what it counted was never quite what hurt —
measured by deliberate breakage, one added edge in `model.rs` now reports
`git -> model -> state -> store -> git`, which the old check would have called
no worse. And a ratchet sitting on an empty baseline is a rule with nothing left
to negotiate, so the baseline file is gone.
Two things about the reader are worth carrying. **A number a tool reports is a
claim the tool has to earn**: the SCC was once reported as 23 modules, which was
this script's own pattern failing to cut `pty.rs`'s `pub(crate) mod tests`, and
the figure reached a commit message and a review before anybody checked it. And
**it cuts every `#[cfg(test)]` module rather than slicing the file at the first
one** — the old cut kept the head of the file on the written assumption that the
test module is last, and `api.rs` carried 858 lines of handlers below its tests,
two of whose edges were read nowhere.

## A run's end is published, not dispatched by name.
`spawn` owns the only
`pty.wait()`, so it is where a run ending is learned — and it used to settle the
run itself, calling `fix_pr::settle`, `fix_pr::start` and `triage`'s predicates,
which is exactly why it imported the two modules that call `spawn_run` to start
a run. `watch_session_exit` now fills a `state::RunExit` and calls
`app.run_ended`; `orchd_serve::settle_run` is the one subscriber, installed by
`start` beside the pollers.
**A hook on `RunSpec` is the obvious shape and is wrong.** `RunSpec` is neither
persisted nor in `spawn::Carried`, and `auto_resume` rebuilds a run's session
from its `Pass` alone — so a fix run resumed after a restart would have no hook
left and would never settle, silently. The observer is installed once per
process, so a resume finds it exactly as the first spawn did.
Two details that are load-bearing. It is a plain `fn` returning a boxed future
rather than a channel, so the call keeps its place in the exit sequence; and it
is called **after** `release_main`, because `fix_pr::start` refuses while a live
session holds the branch. An unobserved `AppState` — every unit test — drops the
news, which is why the pair of tests is split: `spawn` asserts the exit is
published with the right pass and flag, and `orchd-serve` asserts a refused
hand-off takes `fix_pr_on_exit` back.
The vocabulary moved with it: the six `Pass` commands are associated constants
on `model::Pass` (`FIX_PR`, `REVIEW`, `TRIAGE`, `RESOLVE_RUN`, `HANDLE_REVIEW`,
`STORY`) with `posts_proposals` and `is_triage_of` beside them, because "which
command is this" is asked by the spawn that records it, the route that finds it,
the rail that colours it and the watcher that settles it.

## The module graph is a DAG, and it was made one on purpose.
`app.js` → the
six; `rail` → `term`, `review`; `review` → `diff`; everything → `core`. Three
cycles had to be broken first, and each inversion is the reason a boundary is
real rather than decorative:
- zoom used to resize the terminals directly while they read the scale back.
  `core.setZoom` now announces through `onScaleChange`, and `term` registers.
- the rail called `select()` which called `render()` which redrew the rail.
  `core` owns `selected`/`setSelected` and announces through `onSelection`;
  `app.js` registers what picking a session *means*.
- the changed-files pane and the diff called each other, so the pane moved
  *inside* `diff` — two modules that call each other are one module with a line
  drawn through it.
Adding a cycle back would work (ESM allows it) and would quietly undo this.

## Each module needs a line in `module()` in `host.rs` and a rebuild.
`include_str!` again: adding a JS file is a Rust change, and
`tools/check-module-routes.mjs` is what says so, from both sides of the hook —
its header has why `dependency-cruiser` cannot. That cost is why the modules
track features rather than being cut finer.

## A pane's paint signature is the one guard with no check over half of it.
`core.unchanged(box, value, drop)` decides whether a pane rebuilds. The `drop`
half is checked — `tools/check-drop-lists.mjs` holds those strings to names the
four generated `.d.ts` files still carry, because a rename in Rust leaves the
old spelling dropping nothing and the pane churns again in silence. The other
half cannot be checked cheaply: whether a signature that *lists* its inputs
listed them all is a question about the whole function body, and getting it
wrong freezes a pane rather than churning it. The rail has been missing an
input twice, each found by pressing something. Prefer the
whole-snapshot-plus-`drop` shape wherever stale is worse than an extra rebuild;
`paintSig` says all of this where the idiom is defined.

## `snap` is a live binding, and only `receive()` may replace it.
It is
`export let` in `core.js`, so a hundred readers keep saying `snap.x` and see the
new snapshot without re-importing. `receive` sets the snapshot and the clock it
is measured against together — those drifting apart is what froze durations.

## Never rewrite an identifier across an SPA file with a regex.
Three of the
four apparent uses of `resize` were the *string* `'resize'` — an event name and a
URL path — and a blind substitution would have broken window resizing with
nothing failing. Rewrite by line number, asserting each line really is a call.
`mise run check-web` catches a *renamed* identifier; it cannot catch a string
that changed meaning.

## The app is WebKitGTK, not Chrome.
`mise run shot` drives Chrome and is fine
for layout and copy, but the two engines disagree often enough to matter.
`tools/` pins `playwright-core` to the version whose WebKit build is on disk so
an engine-specific fault can be reproduced; stub `/vendor/addon-webgl.js` in
such a test, because headless WebKit dies on xterm's WebGL renderer.

## The DOM renderer is a WebKitGTK workaround, and only WebKitGTK's.
WebGL
garbles glyphs there: text arrives as noise and only comes back when a scroll or
a selection forces a redraw. Clearing the texture atlas after every refit and
disposing the addon on context loss both failed, so the canvas is gone under
that engine.
**WKWebView garbles too, and the experiment is settled** (#8): on a Retina Mac
the agent pane turns to noise cell by cell, and worse than on Linux — a scroll
does not clean it up, because the repaint comes from the same corrupted atlas.
The trigger is a *second* terminal writing while the pane repaints; a drawer
shell running `git status` was enough, and a single terminal never garbled.
So the rule is now **one live WebGL context per window on macOS**: the agent
pane keeps the canvas, every drawer terminal takes the DOM renderer. Dropping
`IS_MAC` outright was the other candidate and is worse — it brings back the
typing lag the flag exists for (a Retina panel composites four times the pixels
while Claude Code repaints its whole TUI per keystroke) on the one pane you type
into. **A browser tab keeps WebGL everywhere**, deliberately: Chromium and
Firefox have no such fault, and a shell streaming a build log is where the canvas
earns its keep.
Which renderer a terminal opened with now reaches `orchd.log` (`page:
<target> renderer=… engine=…`, plus a line on context loss), because the report
needed a screen recording to answer "which renderer were you on".

## Slow trackpad scroll in an agent pane is xterm's wheel maths, not the renderer.
With mouse reporting on, `consumeWheelEvent` cuts any event under
50px to 30% and passes only whole lines, and it sends one report per event
whatever the delta. A mouse never enters that branch; a macOS trackpad always
does. Measured against the vendored build: at 1px per event 5 of 300 reached the
agent; a real slow drag has a 13px median, so ~4.6 events per line. `term.js`
takes the wheel over through `attachCustomWheelEventHandler` and emits one
undamped SGR report per line. Three things that matter if you touch it:
**agent panes only** (it writes the SGR encoding, which is right because the
agent asked for `?1006h` and wrong for a program that did not), `scrollSensitivity`
is a dead end (it multiplies before the threshold test, which reads the raw
delta, so fixing a trackpad breaks a mouse), and **Shift+wheel** bypasses the
whole path into xterm's own scrollback, which is in the legend now.

## A window drag is the one call in this app that can abort the process, and it is guarded in two places.
tao's `drag_window` hands AppKit's *current* event to
`performWindowDragWithEvent:`, which accepts nothing but a mouse event — a keyDown
is type 10, and the Objective-C exception takes the process, the daemon and every
session with it. Press a titlebar, then press a key, and the queued request is
handed that keyDown.
**tao's own guard does not fire.** `tao-0.35.3` substitutes a synthetic mouse-down
when the event type is `0x15` — which is 21, while `NSEventTypeApplicationDefined`
is 15. So nearly every call reaches AppKit with whatever event is current. Read
from the vendored source; do not conclude from tao's code that ours is redundant.
**The shell refuses the call** when AppKit is not on a mouse event
(`desktop/src/main.rs`'s `start_dragging` → `on_a_mouse_event`), on the main
thread, with no queue between the check and the call — `[NSApp currentEvent]` is
meaningless anywhere else, and `dispatch` runs on an axum worker. No `unsafe`:
`sharedApplication`, `currentEvent` and `type` are all safe in `objc2-app-kit`, so
the crate keeps `unsafe_code = deny`.
**And the page drawing the titlebar arms on mousedown and asks on mousemove**,
which keeps the request inside a gesture in the first place. **There used to be
two such pages and they paid for this separately** — the board in `2990237`, the
first-run page four days later, because it drew its own chrome rather than
sharing `app.js`. That split also put two sets of window buttons on macOS. There
is one page now: first run is `web/js/open.js` over the board, so a fix to the
chrome is a fix everywhere it is drawn.
The resize strips fire on mousedown and are *not* guarded — they are
`display:none` on macOS, so the AppKit call is unreachable there. That is safety
by platform rather than by design: showing them on a Mac would reopen this.
**Both guards shipped and #14 is still open**, which is the part to read before
concluding this is done. The reporter came back on 2026-09-15 saying it still
aborts on the open-project screen, on v2026.9.15 — a build carrying `9048384` and
`8fb2d0c` both. Everything readable from Linux says the two guards hold: one
`start-drag` request in the whole SPA, no `data-tauri-drag-region` and no
`-webkit-app-region`, and `tauri-runtime-wry`'s `send_user_message` running the
message *synchronously* when it is already on the main thread — so the check and
the AppKit call really are the same turn. Which leaves a path nobody has read, or
a different abort on that screen being reported as this one.
That is why `desktop/src/appkit_abort.rs` is installed at boot. An uncaught
Objective-C exception aborts without unwinding, so the Rust panic hook never runs
and `orchd.log` simply stops — a crash that says nothing is what made this take
two rounds of reading vendored source. The handler cannot prevent the abort and
does not try; it writes the exception's name, reason and `callStackSymbols` into
the log first, so the next report carries the selector instead of a description of
what the person clicked.

## `window.confirm`, `window.prompt` and `window.alert` do nothing in this app on macOS.
WKWebView shows a script dialog only if the host implements the
matching `WKUIDelegate` method, and wry implements exactly three — the file-open
panel, media-capture permission and `window.open`. None of the dialogs. So
`confirm()` returns **false**, `prompt()` returns **null**, `alert()` is a
no-op, and eight guarded actions silently did nothing on a Mac: two naming
flows took the cancel branch, and six destructive guards refused. WebKitGTK
ships default dialogs, which is why Linux never showed it. `core.js` draws its
own (`confirmBox`, `promptBox`) and they are async — the callers had to become
`async` with them. **Never reach for the native three again.** Two properties of
the replacement worth knowing: the same question asked while it is still open
returns the promise already outstanding, because one of these guards is reached
from `render` and a refusing guard would otherwise re-ask every frame; and it is
first in the `Esc` chain, since a confirm over an overlay must not close the
overlay underneath it.
