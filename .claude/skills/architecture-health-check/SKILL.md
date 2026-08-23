---
name: architecture-health-check
description: Read-only architecture health check for orchd — find where many individually reasonable changes have gradually degraded the architecture (responsibility accumulation, workflow fragmentation, conditional sprawl, boundary violations, unclear state ownership, incremental duplication, over-stretched abstractions). Produces a prioritized, evidence-backed report and modifies no code. Use for a periodic review, or after a subsystem has grown a lot. Optionally pass a path, subsystem, or git range to scope it.
---

# Periodic Architecture Health Check

Perform a **read-only** architecture health check of the codebase. Do **not**
modify any code. The deliverable is a report, nothing else.

This is a long-lived, AI-maintained application. The goal is to find the places
where many individually reasonable changes have gradually made the architecture
worse — not to grade it against a textbook.

## Scope

- If the invocation passed a path, subsystem name, or git range as an argument,
  focus there.
- Otherwise focus on the parts that have **changed or grown significantly** since
  the last review. There is no stored review marker, so establish "recent" from
  history — e.g. `git log --since=... --stat`, `git log --stat -- <path>`, or the
  last few dozen commits — and weight the churny, growing files. Say in the
  report what window you used.

## Start from what the repo already documents

Before inspecting code, read the two hand-maintained sources of intent:

- **README.md** — the architecture and the module map.
- **TODO.md** — open decisions, "Decisions worth revisiting", and "Won't do
  without a reason". Several apparent problems are deliberate and explained
  there; do not re-flag a documented, intentional trade-off as drift. If a
  finding contradicts a TODO decision, say so and treat the decision as the
  baseline unless you have concrete evidence it has since broken.

## Understand the architecture first

Before evaluating anything, inspect the relevant code and establish:

- The major layers and subsystems
- The owner of each important workflow
- Where application state is authoritative
- How the SPA communicates with the Rust backend/daemon
- How Claude Code processes are managed
- How Git and worktrees are managed
- How GitHub operations are managed
- The important dependency directions between modules

Do **not** judge the code against a generic architecture such as DDD, Clean
Architecture, or SOLID. Evaluate whether the architecture is internally coherent
and whether responsibilities have remained clear as the application evolved.

## Look specifically for architectural drift

Identify concrete examples of:

### 1. Responsibility accumulation

Modules, services, functions, or components that have gradually accumulated
unrelated responsibilities. Ask:

- Does this component have multiple distinct reasons to change?
- Has it become the default place where new behavior gets added?
- Does it coordinate concerns that should have clearer owners?

### 2. Workflow fragmentation

Workflows whose logic is spread across multiple places without a clear owner.
For each important workflow, determine:

> Workflow → primary owner → entry point → dependencies

Flag workflows where this is unclear.

### 3. Conditional complexity

Look for growing complexity caused by boolean flags, optional parameters, mode
enums, special cases, large conditional trees, or "if this came from X, do Y
differently" logic. Determine whether these conditions represent genuinely
different behaviors that deserve separate workflows/functions/components. Do
**not** flag ordinary conditionals merely for existing.

### 4. Boundary violations

Check whether responsibilities are leaking between layers. Pay particular
attention to:

- UI orchestrating backend workflows
- UI managing Git, worktree, GitHub, filesystem, or process details
- Session lifecycle logic spread between frontend and daemon
- Raw infrastructure details appearing inside higher-level workflows
- GitHub concerns leaking into Git concerns
- Claude-process concerns leaking into unrelated application code

### 5. Unclear state ownership

For important pieces of state, identify:

> State → authoritative owner → readers/caches

Look for multiple competing sources of truth; frontend state duplicating daemon
state without a clear synchronization model; derived state stored independently;
state whose owner is ambiguous; lifecycle state that can go inconsistent after
restart or failure.

### 6. Duplication caused by incremental development

Behavior implemented multiple times because new features were added locally
rather than extending an appropriate existing workflow. Focus on meaningful
duplication, not superficial repetition.

### 7. Abstractions that no longer fit

Abstractions that were reasonable originally but have been stretched to
accommodate too many variations. Signs: generic names with many special cases;
large option objects; interfaces whose implementations behave fundamentally
differently; parameters that exist only for one or two callers; a growing number
of callers that must understand internal quirks. Do **not** recommend
abstraction for its own sake.

## Evaluate complexity honestly

For each problem, distinguish:

- **Necessary complexity** — inherent to the application's requirements.
- **Accidental complexity** — introduced by how the code evolved.

Do not recommend refactoring necessary complexity just because it looks
complicated.

## Produce a prioritized report

Do not modify code. Return:

**Architecture summary** — briefly describe the architecture as it actually
exists today.

**What is healthy** — the decisions and boundaries currently working well.
Preserve these.

**Findings** — for every meaningful issue:

- **Severity:** High / Medium / Low
- **Location:** specific files, modules, or workflows.
- **Problem:** what has drifted and why it makes future changes harder.
- **Evidence:** concrete code paths, dependencies, state flows, or examples.
- **Recommended direction:** the smallest architectural change that would improve
  the situation.
- **Migration risk:** what could break or get more complex during the change.

**Top 3 highest-value improvements** — rank the three changes giving the greatest
reduction in future complexity relative to implementation cost. Prefer small,
incremental migrations over rewrites. For each: (1) what should change, (2) what
should remain unchanged, (3) the likely migration steps, (4) how to verify
behavior was preserved.

**Things that look suspicious but should NOT be changed** — explicitly list any
complexity you investigated but believe is justified (including anything TODO.md
already defends). This prevents unnecessary refactoring.

## Final rule

Do not produce a generic list of clean-code recommendations. Every finding must
be tied to the actual codebase and supported by concrete evidence. The purpose
is not to make the architecture more abstract, more layered, or more
"enterprise." The purpose is to make the next 20 features easier to implement
without making the following 20 progressively harder.
