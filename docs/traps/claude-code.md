# Claude Code, and what it guarantees

Every entry here cost something. `CLAUDE.md` indexes them by their first
line; this file is the rest — what happened, what was measured, and why the
code is the shape it is.

## A skill reaches a session through `--plugin-dir`, and that flag is per *invocation*.
Measured against Claude Code 2.1.260: a session spawned with it
runs the skill both ways, typed as `/orchd:orch` and picked up by the model from
its description — and resuming that same session id *without* the flag answers
`Unknown command` for the skill it had a moment ago. So it is a property of the
process, not of the conversation, and every site that builds a `claude` argv has
to push it. `config::session_flags` is the one spelling, and it carries
`--settings` too — the pair beside `session_env`, for the reason that docblock
gives: these sites have drifted before.
Two things that make it safe to push unconditionally. A directory that is not
there is not an error — Claude Code starts and says nothing — so a failed write
degrades to a session without the skill rather than a session that will not
start. And the layout is Claude Code's, not ours: the manifest at
`.claude-plugin/plugin.json` names the namespace, the skill lives at
`skills/<name>/SKILL.md`, and either one in the wrong place fails silently.
**Adding a skill is a Rust change**, `include_str!` again, like the SPA's
modules.
**Every vendored prompt is a skill now**, and `commands/` and `prompt.rs` are
gone with them. The conversion has one rule worth knowing: a prompt was
substituted per run and written to a file, a skill is static and typed as one
line, so every value a template interpolated has to arrive another way. Two ways
are in use, and which one is not a style choice. A run that already has a token
asks `/api/pr/:n/triage-context` (`review`). A run that deliberately has none
reads its values out of the environment (`fix-pr`, `story`), because a route would have meant handing an unattended force-pushing
run a credential to read what the daemon can just put there. `skills::VAR_*`
names those variables once, since the spawner sets them and the skill reads them
and a rename on one side alone is silent.
**`--allowedTools` does not gate a typed skill, and the opposite was written in
three places.** `story.rs` and `skills.rs` both claimed its allowlist
(`mcp__<tracker> Read Write`) meant that run could not invoke a skill at all, and
this file repeated it. Measured against 2.1.263: `claude -p "/orchd:orch"` under
`--allowedTools "Read Write"` runs the skill and answers out of its contents. The
allowlist gates **tool calls**, and Claude Code expands a typed command before the
model acts — which is also why `-p` is the shape that proves it, since that is
exactly how the story run is spawned. Model-*chosen* skill use is the open half:
that goes through a `Skill` tool, which an allowlist would gate.

## The daemon's session id is Claude's session id.
Every spawn passes
`--session-id`, which is what makes `--resume`, transcript lookup and hook
correlation need no mapping. A fork passes `--session-id <new> --resume <old>
--fork-session`, which is honoured. Keep that invariant.

## Transcript paths slug both `/` and `.`.
`.claude/worktrees/x` becomes
`--claude-worktrees-x`, not `-.claude-...`. Getting this wrong makes every
worktree session look like it has no transcript.

## A transcript is keyed by session uuid, so two sessions in one directory do not interleave.
Said here because the opposite was written into two code comments
and a domain finding, and it justified a guard that refused reviewing any PR
whose worktree you had torn down. `transcript_file`, `find_transcript` and
`archive`'s copy all key on the uuid; sharing a directory slug gets you two
files. The real hazard of reusing a worktree name is elsewhere — a resume landing
in a tree cut again for something else — and `worktree::branch_drift` says so
rather than refusing.

## A tracker is three config fields, and there is one way to write it.
`mcp_server`, `host` and an optional `token_env` (`config::Tracker`), so pointing
the daemon at Linear or Jira is a config edit rather than a release. It was an
enum arm per tracker, then briefly both — the object *and* `"shortcut"`/`"stub"`
as shorthands — and two spellings of one setting is worse than either: the file
stops being readable on its own, the SPA can offer one form and not the other,
and every reader needs an arm per shape. A name that never shipped is **refused
with the object to write**, which is one of the two reasons `Tracker` has a
hand-written `Deserialize`; serde's own answer names the problem and not the fix.
**But the refusal costs you the tracker, not the config.** `config::tracker_or_warn`
turns it into a warning and loads the rest of the file, because the asymmetry is
not close: a tracker is one optional flow, while refusing the file costs the
checkout, the port and every hand-tuned key — and `Config::existing` then reads
that as first run and offers a folder picker. Measured before it: a daemon on
`tracker: "jira"` exited 1 and served nothing. `Tracker`'s own `Deserialize` is
unchanged and still produces the sentence; this only decides who pays. It does
not guess either — an unreadable value leaves the tracker unconfigured rather
than pointed at somebody's host.
**The file is migrated on start, and the reader is the fallback.**
`migrate::config_file` rewrites `"tracker": "<name>"` into the object it meant
before either reader parses the file — see the entry below. `"shortcut"` and
`"stub"` are *also* still read, permanently, because they shipped —
and a refusal there does not cost you a key, it costs you **the whole file**:
`Config::existing` drops a config it cannot parse, and the app then reads that as
first run and shows a *folder picker* for a project you configured months ago.
With no way back, since `firstrun::write_config` merges by JSON key and so keeps
the very line that is being refused. One key nobody touched, every setting gone.
So the two names read as the objects they meant, nothing is written back (a
downgrade keeps working), and this is how the file is read rather than a
migration — `store::OnDiskKind` holds the same position for `sessions.json`.
The constants are back in the code and that is the trade: they are *file
reading*, never what the daemon believes, and no caller can reach them.
It is also gone from `config::Settings` and from the settings pane's controls.
The pane shows `snap.tracker_server` read-only, because a write of that whole
struct is how a hand-edited tracker would have been replaced by whichever name a
dropdown happened to show — and no control can spell a per-site host anyway.
**The first-run page drew a dropdown for it anyway, and it did nothing.** It
offered `None` and `Shortcut` and posted the value; `firstrun::Overrides` has no
such field and serde drops an unknown key in silence, so the control never wrote
anything from the day it was drawn. It is gone now, for the reason the settings
pane's went: a tracker is three fields and no dropdown can spell a per-site host.
This entry used to say the page never collected one — which was true of the
config it wrote and false of what it showed you.
Three things the research settled, none of them guessable from the Shortcut setup
this was built against:
- **Both official remote trackers are OAuth-first.** Linear is
  `https://mcp.linear.app/mcp`, Atlassian `https://mcp.atlassian.com/v2/mcp`, and
  each offers a bearer path *and* there is an open Claude Code issue where a
  configured `Authorization` header is ignored when the server advertises OAuth.
  So `token_env` is optional and its absence is not a broken config: the run
  authenticates out of a login the user did earlier and the boot line says
  "authenticating itself" rather than warning about a variable.
- **No tracker tool name may live in the daemon.** Linear does not publish theirs
  and Atlassian's are versioned. `skills/story/SKILL.md` says "your tracker's own
  search" and leans on the repo's tracker skill, which is where the README already
  put the team id, the workflow state and the epic routing.
- **The id/URL agreement rule was Shortcut-shaped.** `StoryRef::consistent`
  required a path segment equal to the id's *digits*, which is true of
  `sc-12345` → `/story/12345` and false of every Linear (`/issue/ENG-123`) and
  Jira (`/browse/ABC-123`) URL there is. It accepts the whole id in a segment too
  now. It would have refused every story either tracker filed, as "the agent
  reported an id and URL that disagree".

## Session names come from an undocumented field.
`store::ai_title` tails the
transcript for `{"type":"ai-title","aiTitle":…}`. It degrades to the workspace
name rather than failing, so a rail that suddenly reads `dfafdf` everywhere
means Claude Code changed the format. Identical titles across unrelated sessions
are Claude Code's doing, not a bug here: one `ai-title` string turned up in 8
transcripts across 3 repositories, each correctly attributed to its own
sessionId. The reader is right; the file says that.

## There is no findings log any more.
`daemon.log` and `log_path` are gone: the
one finding the daemon ever produced (the review queue is unavailable) was already
in the snapshot and the pane, so the file was a second copy of one line. It once
spliced a block into `TODO.md` at the build-time path and churned this repo from
every build; do not bring back a file the daemon writes into a checkout.

## Stopping a session is `kill_gracefully`, on every path.
One `SIGHUP` is a
request Node is entitled to decline, and three sites still sent only that: the
rail's kill button "succeeded" while the row stayed Working, a review's hand-off
was armed on an exit that never came, and the story filer outlived its timeout.
`kill_gracefully` signals the *group* (the group id is the leader's pid, since
`portable-pty` `setsid`s the child; asking `getpgid` failed the moment the leader
was reaped, which is exactly when a HUP-ignoring grandchild needed reaching),
waits `KILL_GRACE`, `SIGKILL`s, and sweeps the group once more after the leader
exits. `kill` and `kill_hard` refuse an exited child, because a reaped pid may
already be somebody else's.

## The open screen merges into `config.json`, never replaces it.
It runs on
every reviewed add and used to build the file from scratch, so re-picking a moved
checkout dropped every hand-tuned key. It keeps the previous file as
`config.json.bak`, pins `repo` only when the remote does not already derive it,
and detects a fork layout itself (the daemon's own first write no longer runs,
since this write comes first).
**It writes the checkout's own directory now, and that is what makes the review
run at all.** The root `config.json` it used to write is one checkout's —
`host::checkout_dir` gives each its own `ORCHD_CONFIG_DIR` and the root file is
copied into one of them once — so a base branch or a dev process detected for the
*second* checkout you opened was detected for nobody. There is no undo any more
either: it existed because a refused switch left the root file naming a checkout
the daemon was not on, and a leftover config in a checkout's own directory is
read by that checkout's daemon alone.

## A session's environment is not the shell's, and the gap is invisible.
The
daemon's environment is whatever started it; from a desktop launcher that is the
systemd user manager's, which holds no checkout's variables. So a `.mcp.json`
header spelled `Bearer ${SHORTCUT_API_TOKEN}` went out as literal text and the
server answered 401 — while the same session started by typing `claude` in that
checkout worked, because `mise activate` exports at a shell prompt and an app has
no prompt. That is why the terminal is the worst place to reproduce this.
`config::session_env` now asks the tool itself (`crates/orchd-repo/src/env_source.rs`, `mise` by
default, `direnv` beside it, `none` to turn it off), per spawn, in the session's
own cwd. Two things it will not do: it never fails a spawn (a missing variable is
degraded, a refused spawn is lost), and it cannot trust a config for you — mise
refuses an untrusted `mise.toml`, a fresh worktree is a fresh path, and the only
sign is one warning in the log. Put `mise trust` in `worktree_setup` if that
bites.
