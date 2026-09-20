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

**The naive detector fires while you type.** "Two Shift keydowns within 300ms" is
also the shape of `Shift A Shift B` — two taps with a release between them — so an
interval check alone opens the overlay in the middle of a sentence. The fix is
`dirty`: any other key pressed while Shift is held disqualifies that tap, so a
Shift used *as a modifier* can never arm the next one. What survives is a Shift
pressed and released with nothing between, twice.

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

Two smaller rules travel with it. The gutter number is generated content
(`i::before { content: attr(data-n) }`) for the reason the diff's is — WebKit
takes an unselectable element's text when a selection *crosses* it, so a copied
snippet would carry a column of digits. And a refusal — binary, too large, deleted
underneath you — is a sentence in the pane (`.fnsay`), because an empty viewer is
indistinguishable from a broken one.
