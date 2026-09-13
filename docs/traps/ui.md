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

## The rail's `handle` button starts a pane, not the overlay.
`/orchd:handle-review`
(`skills/handle-review/SKILL.md`, vendored from the monorepo's own `/resolve` and
generalised) is one agent in the PR's worktree with a person watching: it asks with
`AskUserQuestion`, drafts replies and posts nothing without a go. The
triage-into-cards flow is the menu's second review item and still carries out what
the cards decide.
The label is `handle` rather than `resolve` because GitHub has a literal "Resolve
conversation" button and this pass deliberately does not press it — marking a
thread resolved stays the reviewer's. The internal `resolve-run` keeps its name:
that is the overlay's carry-out step, and it is not a button.
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
