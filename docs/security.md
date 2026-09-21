# Security, and what each guard does not cover

The short version is in [README.md](../README.md#security). This is the reasoning
behind each line of it.

Bound to `127.0.0.1` only, with Origin/Host validation and a per-start token
required on the WebSocket and every mutating route. Hook endpoints are exempt from
the token, because a hook Claude Code spawns cannot easily carry a per-start one.
They are confined to their own prefix and a schema that can only ever update state.
GitHub **reads** resolve `ORCHD_GITHUB_TOKEN`, then a `0600` `github_token_file`,
then `gh auth token`, and read scopes are all they need. **Writes are the other
half**: a thread reply, a 👍 and a re-requested review shell `gh` and use gh's own
credential, whatever you set here. So a read-only token does not make the daemon
read-only, and the resolve flow wants `gh` signed in.

**The trust boundary is your user account, not the process.** Loopback keeps the
network out and the Origin check keeps other web pages out. But `GET /` returns
the page with the token substituted into it and is deliberately not gated, so any
process running as you can read the token and then hold everything — including the
pty attach, which means typing into a live agent's terminal. Do not run this on a
machine you share with people you do not trust.

That is a trade rather than an oversight: on a single-user machine a hostile local
process can already ptrace the daemon, and gating the page would break the token
discovery the tooling depends on. It is written down because the alternative is a
sentence that earns trust it has not got.

Agents get narrower credentials than the SPA does, and that part *is* enforced: a
session asks with `ORCH_ASK_TOKEN`, good for its own session's routes, and a review
session posts with `ORCH_POST_TOKEN`, good for one route on one PR. Neither is the app
token — which matters because those are the runs that read other people's review
comments.

There is a `PreToolUse` guard on the agent's git (`orch guard push`) with three
rules: no lease-less `--force`, no push to the base branch, and no git aimed out of
the worktree the session works in. The third replaces the isolation
`claude --worktree` used to pin, and it is deliberately narrower — git only, never
your writes, because main's branch and its recorded occupant are what the daemon
needs protected and a shared scratch dir is not its business.

The third rule is a question rather than a wall: its refusal names `orch outside
<path>`, which puts "may this session run git there?" to you through the same ask
box every other question uses. A yes is remembered **for that folder and what is
under it**, for the rest of that session, so the next checkout is a question of its
own. Nothing persists it, so a restart asks again. Read all three as a
**mistake-catcher, not a control**: it sees `Bash` tool calls only, so `gh`, an MCP
git server, or a script the agent writes and then runs all go around it. It is there
because a fix-pr run force-pushes with nobody watching, and that is the mistake
worth catching — not because an agent could be prevented from pushing.