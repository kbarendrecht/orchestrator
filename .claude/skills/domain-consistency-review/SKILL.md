---
name: domain-consistency-review
description: Read-only product & domain consistency review for orchd — reconstruct the domain model from the implementation, then check whether the domain logic is internally coherent. Finds contradictions, unclear state ownership, impossible states, duplicated business rules, and rules enforced in one place but bypassed in another — especially across the SPA / daemon / persistence / GitHub / background-poller boundaries. Produces inferred domain rules plus evidence-backed inconsistencies and modifies no code. Use to sanity-check that the product makes sense as a system, not to review code quality or architecture. Optionally pass a path or subsystem to scope it.
---

# Product & Domain Consistency Review

Perform a **read-only** product and domain consistency review. Do **not** modify
any code, and do **not** produce a refactoring plan. The deliverable is a report.

Your job is **not** to review code quality, style, architecture, or to suggest
refactoring. (Architecture drift has its own skill —
`architecture-health-check`; stay out of its lane.) Instead: understand what this
product is supposed to do by reading the implementation, then check whether the
domain logic is **internally coherent**.

## Scope

- If the invocation passed a path or subsystem as an argument, focus there.
- Otherwise review the product as a whole, weighting the subsystems with the most
  domain rules — sessions and their lifecycle, workspaces/worktrees, the swap, PR
  automation (fix / resolve), review threads, and persistence.

## Start from what the repo documents as intent

Read the two hand-maintained sources of intent before inferring anything:

- **README.md** — what the product is and the module map.
- **TODO.md** — open decisions, deliberate non-goals, and "decisions worth
  revisiting". Many apparent contradictions are deliberate and explained there.
  Do not report a documented, intentional trade-off as an inconsistency; if a
  finding contradicts a TODO decision, treat the decision as the baseline unless
  you have concrete evidence it has since broken.

## Process

1. Inspect the relevant code and reconstruct the domain model.
2. Infer the product concepts, entities, states, workflows, and rules.
3. Look for contradictions, unclear ownership, duplicated business logic,
   impossible states, and behaviour that does not make product sense.
4. Pay special attention to the boundaries where a domain rule has to be agreed on
   by more than one component. In orchd those are:
   - **SPA ↔ daemon** — the SPA renders the snapshot and calls the API; a rule the
     SPA enforces only in a button's disabled state but the API does not (or vice
     versa) is the classic split.
   - **daemon ↔ persistence** (`sessions.json`, `automation.json`, `manual.json`,
     `stories.json`) — what a restored record is allowed to be, and whether the
     live model and the persisted record agree on it.
   - **daemon ↔ Claude processes** — session id == Claude session id, one pty per
     session, who may declare a session over.
   - **daemon ↔ git / worktrees** and **daemon ↔ GitHub** — where a git concept and
     a GitHub concept (branch vs PR head, worktree vs workspace) are conflated or
     kept apart inconsistently.
   - **background pollers / hooks** — state written by a poll or a one-way hook that
     another path assumes it owns.
5. Classify every rule you rely on:
   - **Explicit** — clearly enforced or documented in code.
   - **Implicit** — an assumption the code appears to rely on but never states.
   - **Suspect** — an assumption that may be accidental or inconsistent.

## What to look for

For each entity and each piece of state, ask:

- What does this entity actually represent?
- Who **owns** this state, and who is allowed to change it?
- What states can it be in? Which transitions are valid?
- What must **always** be true? What can **never** be true?
- Does every code path agree on these rules?
- Are two parts of the system making different assumptions?
- Is a rule enforced in one place but bypassed elsewhere?
- Does the implementation expose an impossible or confusing product behaviour?
- Are there edge cases — restart, teardown, failure, concurrent action — where the
  behaviour stops making domain sense?
- Is technical behaviour leaking into the product model unnecessarily?

## Output

Be concise. No general summary, no refactoring plan.

First, the inferred domain rules — only rules important enough to preserve:

**Inferred rules**

- [Concept]: short rule.
- [Concept]: short rule.

Then the issues:

**Domain inconsistencies**

For each:

- **Issue:** one sentence.
- **Why it doesn't make sense:** one sentence.
- **Evidence:** the relevant files / functions / code paths.
- **Suggested rule:** the concise rule the system should follow.

Then, where something is ambiguous rather than clearly wrong, say so explicitly
instead of guessing:

**Ambiguities / assumptions**

- **Assumption:** ...
- **Why it matters:** ...
- **Needs decision:** yes / no.

## Final rule

Do not invent product requirements — infer them from the code. When the code is
ambiguous, prefer stating the ambiguity over pretending to know the intended
behaviour. The purpose is to judge whether the product and domain hold together as
a coherent system, not to make the code cleaner.
