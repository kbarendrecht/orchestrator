# The UI's contracts

Every entry here cost something. `CLAUDE.md` indexes them by their first
line; this file is the rest — what happened, what was measured, and why the
code is the shape it is.

## The keyboard map has a contract, and it is the reason the next binding is obvious.
Above the keydown handler in `web/app.js`: **bare keys belong to the
open overlay, `Ctrl` is the whole app, `Esc` dismisses the topmost thing.** The
whole `Alt` layer was deleted to get here — every action it held already had a
`Ctrl` spelling, and two vocabularies for one set of verbs is what made the map
unpredictable. Do not reintroduce `Alt` to dodge a collision; `Ctrl+Shift` is the
escape hatch. Plain `Ctrl+<letter>` shadows the pty, so `Ctrl+Shift+…` is the
default and a plain letter is taken only where the idiom earns it. The legend
(`Ctrl+Shift+?`) is hand-written HTML and is the one thing here that can silently
drift from the code.
**And a binding is its exact modifiers, never a subset.** The diff overlay's
`Ctrl+←/→` tested `e.ctrlKey` alone, so it matched every chord that merely
*contains* Ctrl — including `Ctrl+Option+Cmd+←`, which is how macOS moves a window
to the next display. The app claimed it, defaulted it and stopped its propagation,
while implementing nothing of the kind (#20). The `j`/`k` alias three lines above
had `!altKey && !metaKey` from the start, so the file already carried its own
answer. `mise run page-check` holds both sides now, and the pair is the gate: the
branch lives under `if (Diff.state.open)`, so the first version of the assertion
passed on a board with no diff up and proved nothing. `Ctrl+Left still steps the
changeset` is what makes the refusal beside it mean something.

## `page-check --mac` ran the Linux branch for its whole life, because of one letter.
The daemon sends `platform: "mac"` (`host.rs`) and the page reads
`IS_MAC = platform === 'mac'` (`web/js/core.js`). `tools/e2e/page.mjs`'s `--mac`
mode set `macos`, so `IS_MAC` stayed **false**: every assertion under that flag
was running the Linux branch while claiming to be the macOS one.
`tools/e2e/renderer.mjs` sets `mac` and is unaffected, which is exactly why
nobody noticed — the two simulations disagreed and only one of them was ever
wrong.

Found by asking a plainer question: **do the tests that press keys ever run on a
Mac?** They do not. The matrix is `ubuntu-22.04` and `macos-14`, but every
keyboard-driving step is `if: runner.os == 'Linux'`; what runs on the Mac is the
30 e2e flows, which drive the HTTP API and press nothing, and `app-check`, which
presses nothing either. So `appMod`'s ⌘ branch — the keyboard map, the
modifier-click, the window chrome — had no gate at all.

It has one now: the Linux job runs `page.mjs` twice, plain and `--mac`, the way it
already runs the renderers. Three things had to be fixed to make the second run
mean anything. The platform string above. The modifier the test presses, which
read `process.platform` — the *runner's* OS — rather than the platform being
simulated, so it pressed `Control` at a page waiting for `Meta`. And every app
chord was written `Control+Shift+…`, which is one of the two spellings `appMod`
accepts; they go through a `chord()` helper now. `Ctrl+←/→` deliberately does
**not**: that binding is literally `ctrlKey` on both platforms.

**What it still cannot answer** is what only a Mac can: whether ⌘ reaches
WKWebView, and that `Ctrl`-click there is a right-click rather than a modifier
click. That would need `page.mjs` on `macos-14` with a browser, which the job does
not set up today.

## The rail's `handle` button starts a pane, not the overlay.
`/orchd:handle-review`
(`skills/handle-review/SKILL.md`, vendored from the monorepo's own `/resolve` and
generalised) is one agent in the PR's worktree with a person watching: it asks with
`AskUserQuestion`, drafts replies and posts nothing without a go. The review
session is the menu's second review item: the same agent, with the overlay showing
its proposals as cards.
The label is `handle` rather than `resolve` because GitHub has a literal "Resolve
conversation" button and this pass deliberately does not press it — marking a
thread resolved stays the reviewer's.
This is a **reversal**, and the reason is the UI rather than the flow: the cards
are not good enough to be the only way through a review yet. `spawn_command_session`
is the seam, and it had no caller but a test for a while — its docblock claimed the
pane was the default the whole time, with a prompt lookup that could not have
answered. If the overlay ever becomes the default again, that docblock and
`README.md`'s "there are two" are the two sentences to change.

## An ask the review overlay does not own must still be answerable in the box.
`renderInteraction` dropped *every* free-text option for a review session, on
the assumption that the only one is the overlay's decision payload. The prompt's
ask template gives an ask exactly one free option, so a review session asking
anything of its own — a problem it hit, in its own words — rendered a box with
no answer in it and a "back to the review" button pointing at cards that had
never heard of the question. Answerable from neither side, and the session sat
on `your_turn` for good, because nothing ever clears `Session::interaction`; only
an answer hides it. It is dropped by *value* now (`decisions`), and the overlay
only claims the ask when that value is present.
The escape hatch beside it is a **fold**, not a dismiss: the header stays as a
one-line strip (`.oq.min`, the `×`, or `Esc`). Hiding it outright would be this
box disagreeing with the rail and the waitbar, which read `wants_attention` off
the daemon and are right — the agent really is still blocked.


## Two panels dock at the bottom of the terminal, and they must not sit on each other.
`.oq` (the open question) and `.rvbar` (what a review is doing) are both
`position:absolute` at `bottom: var(--oq-clear)` with the same `z-index`. That is
deliberate — the height is what clears the agent's own input line, which is the one
row neither may cover. What it overlooked is both being up at once: the question
grows *upward* from that edge, so the bar landed across its lower half, over the
answer field and the buttons. Reported as a screenshot (#21), and the CSS comment
beside it said the two were raised equally "so the two line up", which is exactly
the failure spelled as the intent.
`--oq-h` is the question's height, written by a `ResizeObserver` in `app.js`, and
the bar rises by it. **Measured rather than published from `renderInteraction`**,
because the box has no fixed height — it carries the agent's own words, it can hold
a diff, and folding it (`.oq.min`) makes it one line — and a render path that has
to remember to announce its size is a render path that will forget.
`offsetHeight` rather than the observer entry's `contentRect`: the box has padding
and a border and the bar must clear all of it, and it reads 0 under
`[hidden]{display:none}`, which is the wanted answer then.
The gate in `mise run page-check` asserts the two rectangles **do not intersect**,
not that one is below the other: the bar ends up *above* the question, because the
two have different containing blocks (`#oq` beside `#termwrap`, `#rvbar` inside
it). Which side layout puts it on is not the contract. **Checked against deliberate
breakage twice** — reverting the CSS to the bare anchor and disabling the observer
each reproduce the screenshot, bar 731-769 against question 735-812.

## Shift-Shift is the one gesture that costs no chord, and the guard is the whole of it.
A double tap of Shift opens the file search (`web/app.js`, below the keymap).
It fits the map without bending a rule, which is why it was taken: `Shift` alone
is neither a binding nor a character, so nothing had to move aside, and a bare
Shift is never written to a pty — nothing is taken from an agent and nothing
needs `preventDefault`.

**The naive detector fires while you type.** "Two Shift keydowns inside the
window" is also the shape of `Shift A Shift B` — two taps with a release between
them — so an interval check alone opens the overlay in the middle of a sentence.
The fix is `dirty`: any other key pressed while Shift is held disqualifies that
tap, so a Shift used *as a modifier* can never arm the next one. What survives is
a Shift pressed and released with nothing between, twice.

**The window is 220ms, and it was 300.** `dirty` cannot refuse two bare Shifts, so
the interval is the only thing left guarding the taps nobody meant — and at 300ms
the overlay was opening on people who had not asked for it. A deliberate double
tap is quicker: it is a borrowed gesture, performed at the speed of the double
click it looks like.

`mise run page-check` holds both halves, and **the refusal is the half with
power**: `Shift Shift opens the file search` passes on a broken detector too, and
`typing two capitals does not open it` is what fails when the guard goes. Checked
against deliberate breakage — dropping `!dirty` from the keyup fails exactly that
one. Driven with real `down`/`up`, because the guard turns on the keyup between
the presses and a synthetic keydown would pass while the feature was broken.

## The find viewer draws a band of the file, and the spacers are what make it scroll.
`web/js/find.js` renders 320 rows around the cursor, not the file. A row per line
is a DOM node per line — `core.js` is 2,002 of them — and this app paints into
WebKitGTK with xterm's DOM renderer already on the same page.

**The two spacers are not padding, they are the scrollbar.** Without them the
document is as tall as the band and scrolling stops after 320 lines, which looks
like a truncated file rather than a broken viewer. Their height is the *measured*
height of a real row, taken once per file: a height derived from the CSS goes
wrong the moment the font-size setting moves, and that setting is a slider in this
app.

**The index row above it is a flex line with a priority, and getting that wrong
clipped a column of paths.** The matched line takes the space and is cut with an ellipsis; the
path is capped at 45% of the row and does not shrink with it. The first cut made
both shrinkable, and flex then took the slack out of *both* — squeezing the path's
box while the basename and line number inside it kept their size and ran off the
pane's edge. Inside the path the directory gives way first, and its shrink factor
is 99999 rather than 2 for a reason: flex shares the deficit in proportion to the
factor, so a merely larger number still left the basename a pixel short of its own
text, which is a whole character of ellipsis on 43 of 400 rows. `page-check` holds
both halves, and it took two fixture files to do it — a deep path has a directory
to absorb the squeeze and looks fine, so the row that actually spilled is a file
at the *root*, where nothing in the path can shrink.

Two smaller rules travel with it. The gutter number is generated content
(`i::before { content: attr(data-n) }`) for the reason the diff's is — WebKit
takes an unselectable element's text when a selection *crosses* it, so a copied
snippet would carry a column of digits. And a refusal — binary, too large, deleted
underneath you — is a sentence in the pane (`.fnsay`), because an empty viewer is
indistinguishable from a broken one.

## The mouse's back button undoes a jump, and no gate here can prove the webview delivers it.
A modifier-click replaces what the overlay was showing, so `find.js` keeps a
`trail`: the mode, the query, the hits and the cursor as they were, pushed on
every jump and popped by button 3 (4 is forward). **Only a jump pushes.** Typing a
new query is a place you went rather than one you were sent to, and a back button
that undid your own typing would be a different feature.

Three rules hold it up. The answers are **restored, not asked for again**, so a
step back is instant and cannot come back different because a file changed
underneath; setting the query box's `value` raises no `input` event, which is what
keeps it from re-running. A jump made while the overlay was **closed** pushes a
closed snapshot, so the way out of the first jump is the same gesture as the way
back through the rest. And the buffer answers first, exactly as closing does: a
"keep editing" puts the step back on the trail rather than losing it.

**What is not measured is the delivery.** `mise run page-check` dispatches the
`mousedown` itself, because this playwright's mouse has left, right and middle
only — so the handler and the trail are asserted and the question of whether
WebKitGTK and WKWebView hand button 3 to the page at all is not. Nothing
available here answers it; the app on a real machine is where it gets answered,
and the binding degrades to nothing if the answer is no.

## A path an agent printed is clickable, and three modules each own one third of that.
Click a path in any terminal and the file pane opens on it, at that line. The work is split where the knowledge is: `web/js/term.js` finds the
text in the buffer, `web/js/pathlink.js` decides what is a path, `web/app.js`
turns it into a workspace and a relative path, and `find.js` shows it. None of
them could hold another's half — a terminal does not know what a workspace is,
and the matcher must be drivable without a browser, which is why it is a module
of its own with `tools/check-pathlink.mjs` over it.

**A plain click opens it, and the matcher is the only thing holding that up.**
It was behind the app's modifier first, on the argument that xterm underlines
whatever a provider returns and a terminal full of prose should not underline
itself. The argument lost: the whole point is to click what an agent just
printed, and a key you have to hold is the part you forget. So the refusals in
`web/js/pathlink.js` are now load-bearing rather than tidy — a matcher that is merely
generous turns every word in the scrollback into a link. The modifier still
works, because it is the same link either way.

**The click still reaches the agent, and that is xterm's rule.**
`shouldForceSelection` withholds the mouse report for Shift only (Option on
macOS), so a click on a path is reported to whatever asked for `?1003h` *and*
opens the file. In an agent pane Claude Code sees it too. Read from the vendored
source. Shift would avoid it and is not available: on macOS xterm spends it on
extending a selection.

**An agent names a file, not a path, and that is what the first cut got wrong.**
A component's file name with no directory in front of it, joined onto the pty's
own directory, named a file that was not there — and the viewer
then said so, correctly and uselessly. The workspace's own file list is the
answer: match on the tail, which handles a bare name and a partial path with one
rule. Several matches are not an error either, because two components with one
name in different folders is the normal shape of a large repo — they become the
index and you pick, the branch a symbol defined twice already takes. **The list
is walked fresh on every click**, not read from the cache: a stale list does not
fail visibly, it returns *one* match where there are now two. Measured against the
monorepo this is developed on, that walk is 19,029 files in 100-130ms, against a
click somebody makes a few times a minute. The same measurement is what put a
30-second life on the cached copy the `find files` mode reads, which until then
could not see a file an agent had written since the overlay first opened.

**A line range lights every row in it.** `overlay.service.ts:124-129` is what
Claude Code writes when it means a block, and marking only the first line would
answer a question nobody asked.

**A relative path is relative to the pty, not to the workspace.** A shell started
elsewhere prints paths from there, so the resolver uses the session's or the
process's own `cwd` and only then makes the result workspace-relative. A path that
resolves outside the workspace is refused with a sentence rather than opened and
failed — `/etc/hosts` is a real file and not this workspace's, and a viewer saying
"no such file" would name the wrong fault. The `..` segments are resolved *before*
that comparison, because `src/../../../etc/passwd` starts with the root as a
string and leaves it as a path.

**The right-click menu is the app's, and it has to stop the event.** A path is the
one thing in a terminal with more than one obvious answer, so "open the folder"
and "hand it to the machine" live in a menu rather than being guessed at by a
click. Two things it cost. The cell under the pointer is found by arithmetic over
the **screen** box rather than the host's — a whole number of cells rarely fills
the pane, and the leftover pixels drift every column past the first. And the
handler must `stopPropagation` as well as `preventDefault`: the drawer hangs its
own menu off the pane, it is an ancestor, and without that the pane's menu
replaces this one a moment after it opens.

**The finder hands a file to it, on `Enter` and on a button.** The index is for
finding and the pane is for reading: under an index the file gets two thirds of
the height and no markdown mode. `Enter` is the spelling because a bare key
belongs to the open overlay and the search already runs as you type, so it had
nothing else to mean — and the legend carries a line for it, being the one thing
here that can silently drift.

**The file it opens is its own pane, not the finder.** Clicking a path used to
open the search overlay on a synthetic one-row result: it threw away whatever
search was in it, and answered a question about one file with the machine built
to list many. So the band renderer moved down into `web/js/viewer.js` and
`web/js/fileview.js` is the second thing standing on it. Both mount `.fnsrc` with
`.fnrow`s in it, so one stylesheet and one renderer serve both, and a fix to how
a line is drawn is not a copy that drifts. A name that matches two files is a
picker on the pointer — the same `openMenu` the right-click uses — rather than a
third piece of UI.

**A markdown file opens rendered, and the renderer is two modules for a reason.**
`web/js/markdown.js` imports nothing at all, so `tools/check-markdown.mjs` can
drive it in node where there is no `window` for `core.js` to read — the same
split `web/js/pathlink.js` has. It answers with plain blocks and `web/js/viewer.js` paints
them, which is also where the app's other text-into-nodes work lives. **Nodes,
never markup**: ESLint refuses `innerHTML`, so a note an agent wrote cannot carry
markup into the page, and every link goes through `safeHref`.

**The band is rebuilt on scroll, and it does not know what is mounted.** That is
what made a long note flip back to source as you read it: the scroll handler
re-draws the rows whenever the viewport passes the band's margins, and it did so
straight over the rendered page. The viewer carries the mode now and the handler
answers to it. It took a *long* fixture to gate — a short note never reaches the
margins, so the first version of the assertion passed against the bug.

Two rules inside it worth knowing. A **line number turns the mode off** —
`notes.md:42` means that line and a rendered page cannot point at it, so the
source is what opens and the button is right there. And the six heading classes
are **written out as literals** rather than built from the level, because a
computed class is one `check-dead-css` cannot see and it refuses the rule for it
as dead. The subset is listed at the top of the module: no reference links, no
footnotes, no HTML blocks, no setext headings.

**The two OS items are the daemon's to carry out**, through
`POST /api/open/reveal`, and the containment check is `resolve_in_workspace`'s —
the same one the editor writes through, symlinks included. That is the whole
reason it is a route: the input is text an agent printed, and it must not become
a way to hand `/etc/shadow` to the desktop's default handler. `page-check` stops
at the menu on purpose. Pressing those items would open a file manager on
whatever machine is running the suite.

**Gating it took a reload, and the reason is the renderer.** A browser tab gets
xterm's WebGL renderer (`webglWanted` is `CHROME === 'none'`), and a canvas has no
text for a test to measure or click; the app's window draws into the DOM. So
`page-check` ends by telling the page it has the app's chrome, reloading, opening
a real shell and typing into it. Both halves are asserted and the refusal is the
one with the power: making the matcher accept anything fails "a click on an
ordinary word does nothing" — and fails the jump too, because the word it lands
on is then a link of its own. **What it does not hold is the wrap join** — the
fixture line is short, so a path broken across two rows is covered by reading the
code and by nothing else.
