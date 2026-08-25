use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use uuid::Uuid;

use crate::config::{Config, ManagedSpec};
use crate::model::*;
use crate::pty::PtyHandle;
use crate::state::AppState;

pub(crate) const DEFAULT_SIZE: (u16, u16) = (40, 140);

/// How long *each* worktree hook may run before it is killed.
///
/// Generous — a hook might install or codegen — but bounded, because they run
/// *before* the session spawns, so a hang here is a worktree that never opens.
/// The deadline is enforced in Rust (`proc::run_bounded`), which also takes any
/// children the script left behind. Per hook, not shared: `worktree_setup` should
/// not lose its budget to a slow `worktree_init`.
const WORKTREE_SETUP_TIMEOUT_SECS: u64 = 300;

/// Resolve a worktree hook's command word.
///
/// The command runs *in* the worktree (cwd), but a **relative script path** is
/// resolved against main, not the worktree. That is the common idiom made to
/// work — its hooks are `$CLAUDE_PROJECT_DIR/.claude/hooks/…` operating on the
/// worktree — and without it `.claude/hooks/setup` would be looked up inside a
/// just-created worktree that may not carry it. A bare command name (no slash,
/// e.g. `just`) is left alone for a normal PATH lookup; an absolute path is
/// already unambiguous.
fn resolve_setup_exe(main: &std::path::Path, exe: &str) -> String {
    let p = std::path::Path::new(exe);
    if p.is_relative() && exe.contains('/') {
        main.join(exe).to_string_lossy().into_owned()
    } else {
        exe.to_string()
    }
}

/// Both worktree hooks, in order, in a worktree the daemon just cut.
///
/// `worktree_init` then `worktree_setup`, mirroring the repo's own
/// `worktree-create` and `worktree-link`. Sequential, not concurrent: linking into
/// a tree that has not been based yet is the ordering the repo's hooks already
/// assume.
///
/// The second runs even if the first failed. They answer different questions — is
/// this branch based correctly, does it have the files it needs beside the code —
/// and a tree that is merely un-based is still worth linking. Skipping the link
/// would turn one visible failure into two invisible ones.
pub(crate) async fn run_worktree_hooks(app: &Arc<AppState>, path: &std::path::Path) {
    run_worktree_hook(app, path, &app.cfg.worktree_init, "worktree init").await;
    run_worktree_hook(app, path, &app.cfg.worktree_setup, "worktree setup").await;
}

/// One of the two, named for its logs.
///
/// **Non-fatal.** A hook failing must not strand the worktree: the session is more
/// useful open-with-a-warning than refused, and the same reasoning is why the
/// repo's own hook treats its settings write as best-effort. A failure is logged
/// with the command's own stderr tail, never swallowed.
async fn run_worktree_hook(
    app: &Arc<AppState>,
    path: &std::path::Path,
    configured: &[String],
    label: &'static str,
) {
    let mut argv = configured.to_vec();
    if argv.is_empty() {
        return;
    }
    if let Some(exe) = argv.first_mut() {
        *exe = resolve_setup_exe(&app.cfg.main_checkout, exe);
    }
    let at = path.to_path_buf();
    let shown = argv.join(" ");
    let result = tokio::task::spawn_blocking(move || {
        crate::proc::run_bounded(&at, WORKTREE_SETUP_TIMEOUT_SECS, &argv, label)
    })
    .await;

    match result {
        Ok(Ok(out)) if out.status.success() => {
            tracing::info!(worktree = %path.display(), "ran {label}: {shown}");
        }
        Ok(Ok(out)) => {
            let tail: String = String::from_utf8_lossy(&out.stderr)
                .lines()
                .rev()
                .take(3)
                .collect::<Vec<_>>()
                .join(" / ");
            tracing::error!(
                worktree = %path.display(),
                "{label} `{shown}` exited {}: {}",
                out.status.code().unwrap_or(-1),
                if tail.is_empty() { "no stderr" } else { &tail }
            );
        }
        Ok(Err(e)) => {
            tracing::error!(worktree = %path.display(), "{label} `{shown}` failed: {e:#}");
        }
        Err(e) => {
            tracing::error!(worktree = %path.display(), "{label} task panicked: {e}");
        }
    }
}

/// Placeholder workspace for a worktree whose name Claude Code has not reported
/// yet. Replaced at `SessionStart`.
pub const PENDING_WORKTREE: &str = "\u{2026}creating";

/// How a spawn relates to a conversation that already exists.
#[derive(Debug, Clone, Copy)]
pub enum Source {
    /// Carry on under the same id: one conversation, more turns. The rail row
    /// comes back to life rather than gaining a sibling.
    Resume(Uuid),
    /// Branch off it. Same context, new id, and the original is left exactly
    /// where it is — the "same context, new direction" case (§2).
    Fork(Uuid),
}

/// Spawn a session and return only once the process has *stayed* up.
///
/// [`spawn_session`] answering `Ok` is a weaker claim than it looks: the pty
/// started, and `claude` can still exit a moment later — which is exactly what a
/// `--resume` that finds no conversation does. Any caller that commits something
/// on the strength of the new session (closing the one it forked from, keeping a
/// worktree it just cut) has to ask this instead, or a spawn that died leaves it
/// having paid for a session that is already gone.
///
/// Waits on the exit channel rather than polling it, and only *reads* it:
/// deciding what a death means stays `watch_session_exit`'s job, so there is
/// still one observer of the exit itself. Timing out is the good answer.
pub async fn spawn_session_confirmed(
    app: &Arc<AppState>,
    workspace: &str,
    kind: Kind,
    resume: Option<Source>,
    grace: std::time::Duration,
) -> Result<SessionId> {
    let id = spawn_session(app, workspace, kind, resume).await?;
    let handle = {
        let inner = app.inner.read().await;
        inner.sessions.get(&id).and_then(|s| s.pty.clone())
    };
    let Some(handle) = handle else {
        bail!("session {} was gone before it could be confirmed", &id.to_string()[..8]);
    };
    if let Ok(code) = tokio::time::timeout(grace, handle.wait()).await {
        bail!("session {} exited immediately, code {code}", &id.to_string()[..8]);
    }
    Ok(id)
}

/// Where a relocated conversation ended up.
#[derive(Debug, Clone, Copy)]
pub struct Relocated {
    pub id: SessionId,
    /// True when the resume would not stay up and this is a *fork* instead, so the
    /// conversation survived under a new id rather than the one it had. Worth
    /// reporting: the caller promised a move and delivered a copy.
    pub degraded: bool,
}

/// Move a conversation to another workspace, keeping its id.
///
/// A session cannot be carried across: its cwd is fixed when the pty is spawned
/// (`PtyHandle::spawn`), so nothing can chdir a live one. A move is therefore a
/// kill and a `--resume` at the far end — which is exactly what the id invariant
/// buys, since Claude's session id *is* the daemon's and `--resume` resolves a
/// conversation by id from any working directory.
///
/// # Why the transcript move is not the load-bearing part
///
/// Measured against `claude` 2.1.240: `--resume` finds a conversation by id
/// wherever its file sits, and appends to it there. So the resume works whether or
/// not the file moves, and [`crate::store::move_transcript`] is about keeping the
/// filing straight (see its own note) — its failure is logged, never fatal.
///
/// # The fork fallback
///
/// A resume that finds nothing exits instantly, so this waits out a grace window
/// before believing it. If it does die, forking is tried rather than leaving the
/// conversation with no live session at all: a new id is worse than the one you
/// had, and much better than nothing. Both failing leaves the source killed with
/// its transcript intact — resumable from the rail, nothing lost.
pub async fn relocate_session(
    app: &Arc<AppState>,
    id: SessionId,
    dest_workspace: &str,
    grace: std::time::Duration,
) -> Result<Relocated> {
    let dest_path = app
        .workspace_path(dest_workspace)
        .await
        .with_context(|| format!("unknown workspace {dest_workspace}"))?;
    // Read before anything moves: `spawn_session` rebuilds the record under this
    // same id, so what the conversation *was* has to be captured now or it is
    // overwritten by defaults.
    let (src_cwd, src_workspace, handle, title, created_at, kind) = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .with_context(|| format!("unknown session {}", &id.to_string()[..8]))?;
        (
            s.cwd.clone(),
            s.workspace.clone(),
            s.pty.clone(),
            s.title.clone(),
            s.created_at,
            s.kind.clone(),
        )
    };

    // The pty holds the transcript open, and it is the process that decides when
    // the last turn is flushed. Waiting for it to actually be gone is what makes
    // the move below a move of a file nobody is writing to.
    if let Some(h) = handle {
        let _ = h.kill();
        h.wait().await;
    }

    // Leaving main means giving up the claim, and this is the only thing that can
    // do it: `watch_session_exit` would, but the guard there deliberately stops a
    // relocated session's old watcher from touching state it no longer owns — so
    // relying on the exit would hold main forever under a session that has moved
    // away. Found by driving a two-way swap: the incoming resume was refused with
    // "main is occupied" by the very session on its way out.
    if src_workspace == MAIN && dest_workspace != MAIN {
        app.release_main(id).await;
    }

    // Best effort by construction — the resume does not depend on it.
    match crate::store::move_transcript(id, &src_cwd, &dest_path) {
        Ok(_) => {}
        Err(e) => tracing::warn!(
            session = %id,
            "could not re-file the transcript under {}; resuming anyway: {e:#}",
            dest_path.display()
        ),
    }

    // Its own kind, not `Interactive`: relocating must not quietly promote an
    // automation run into a session the guard table counts differently.
    let resumed =
        spawn_session_confirmed(app, dest_workspace, kind.clone(), Some(Source::Resume(id)), grace)
            .await;

    match resumed {
        Ok(id) => {
            restore_after_relocate(app, id, title, created_at).await;
            Ok(Relocated { id, degraded: false })
        }
        Err(e) => {
            tracing::warn!(session = %id, "the resume in {dest_workspace} did not stay up, forking instead: {e:#}");
            let forked = spawn_session_confirmed(
                app,
                dest_workspace,
                kind,
                Some(Source::Fork(id)),
                grace,
            )
            .await
            .with_context(|| {
                format!(
                    "neither resuming nor forking {} into {dest_workspace} stayed up; \
                     it is closed but its conversation is intact and resumable",
                    &id.to_string()[..8]
                )
            })?;
            restore_after_relocate(app, forked, title, created_at).await;
            Ok(Relocated { id: forked, degraded: true })
        }
    }
}

/// Put back what `spawn_session` reset, and re-find the transcript.
///
/// A resume rebuilds the record from [`Session::new`] defaults, so a relocated
/// session would otherwise lose its title until the next tail read and jump to the
/// top of the rail with a fresh `created_at` — which also changes which session a
/// later swap reads as the newest.
async fn restore_after_relocate(
    app: &Arc<AppState>,
    id: SessionId,
    title: Option<String>,
    created_at: std::time::SystemTime,
) {
    let mut inner = app.inner.write().await;
    if let Some(s) = inner.sessions.get_mut(&id) {
        // A fork has its own title to earn, but it opens on the same conversation,
        // so showing the old one beats showing the workspace name.
        if s.title.is_none() {
            s.title = title;
        }
        s.created_at = created_at;
        // Wherever the file ended up — the move may have been skipped, and Claude
        // may have re-filed it. This is the same self-heal the exit path does.
        crate::store::pin_transcript(s.id, &s.cwd, &mut s.transcript_path);
    }
}

/// Spawn an interactive Claude session in an existing workspace.
///
/// The daemon spawns every session and never adopts a shell-started one. That
/// is what makes `$ORCH_SESSION_ID` injection and exact hook correlation
/// possible (§2).
pub async fn spawn_session(
    app: &Arc<AppState>,
    workspace: &str,
    kind: Kind,
    resume: Option<Source>,
) -> Result<SessionId> {
    let path = app
        .workspace_path(workspace)
        .await
        .with_context(|| format!("unknown workspace {workspace}"))?;

    // Main is exclusive, and the claim is taken before the process starts so a
    // failed spawn cannot leave the lease held.
    let id = match resume {
        Some(Source::Resume(prev)) => prev,
        // A fork is a second conversation, so it needs an id of its own.
        Some(Source::Fork(_)) | None => Uuid::new_v4(),
    };
    if workspace == MAIN {
        app.claim_main(id).await?;
    }

    let settings = Config::hooks_settings_path()?;
    let mut cmd = vec!["claude".to_string()];
    // Assigning the id keeps the daemon's session id and Claude's own the same
    // value, so resume and transcript lookup need no mapping. `--resume` already
    // decides the id; a fork does not, and `--session-id` is honoured alongside
    // `--fork-session`, so the invariant survives there too.
    match resume {
        Some(Source::Resume(prev)) => {
            cmd.push("--resume".into());
            cmd.push(prev.to_string());
        }
        Some(Source::Fork(prev)) => {
            cmd.push("--session-id".into());
            cmd.push(id.to_string());
            cmd.push("--resume".into());
            cmd.push(prev.to_string());
            cmd.push("--fork-session".into());
        }
        None => {
            cmd.push("--session-id".into());
            cmd.push(id.to_string());
        }
    }
    cmd.push("--settings".into());
    cmd.push(settings.to_string_lossy().into_owned());

    // Asked before anything is created, and for every spawn rather than only the
    // ones an agent asks for: the button in the rail can be the last straw just
    // as easily as a CLI call.
    crate::headroom::check().map_err(|why| anyhow::anyhow!("not starting a session: {why}"))?;

    // Built before the spawn so the pty can carry the session's own ask token:
    // it is minted with the session, and the agent reads it from its environment.
    // Read before the insert below replaces it: a resume keeps the id, so the
    // record of what the conversation was doing is about to be overwritten by the
    // session that continues it.
    // A resume comes back at an empty prompt, so whether the turn behind it was
    // interrupted is the one thing it cannot re-derive. A fork starts a fresh
    // direction, so it is never interrupted. `had_a_turn`, though, carries across
    // both: a resumed conversation already had its turns, and a fork replays the
    // parent's — so both open on a real conversation, not an empty pane, and
    // reading it fresh would say otherwise until the next `Working`.
    let (interrupted, had_a_turn) = match resume {
        Some(Source::Resume(prev)) => {
            let inner = app.inner.read().await;
            let prev = inner.sessions.get(&prev);
            (
                prev.is_some_and(|s| s.interrupted),
                prev.is_some_and(|s| s.had_a_turn),
            )
        }
        Some(Source::Fork(prev)) => (
            false,
            app.inner.read().await.sessions.get(&prev).is_some_and(|s| s.had_a_turn),
        ),
        None => (false, false),
    };

    let mut session = Session::new(id, workspace.to_string(), path.clone(), kind);
    session.interrupted = interrupted;
    session.had_a_turn = had_a_turn;
    if let Some(Source::Fork(prev)) = resume {
        session.forked_from = Some(prev);
    }

    let (mut env, unset) = crate::config::transcript_env();
    env.push(("ORCH_SESSION_ID".to_string(), id.to_string()));
    env.push(("ORCH_ASK_TOKEN".to_string(), session.ask_token.clone()));
    // So `orch` needs no configuration: the session's own environment says where
    // the daemon is and who it is.
    env.push((
        "ORCH_URL".to_string(),
        format!("http://127.0.0.1:{}", app.cfg.port),
    ));
    let spawned = PtyHandle::spawn(&cmd, &path, &env, &unset, DEFAULT_SIZE)?;

    session.pty = Some(spawned.handle.clone());
    session.pid = spawned.pid;

    {
        let mut inner = app.inner.write().await;
        inner.sessions.insert(id, session);
    }

    watch_session_exit(app.clone(), id, spawned.handle);
    app.notify().await;
    Ok(id)
}

/// Create a worktree and start a session in it.
///
/// Two paths, decided by where this repo keeps its worktrees:
///
/// - **At Claude Code's own default** (`.claude/worktrees`), creation is
///   delegated to `claude --worktree` rather than `git worktree add` here (§11).
///   The repo's `worktree-create` / `worktree-link` hooks already base on a
///   freshly fetched upstream and configure triangular push, and reimplementing
///   that would fight them. Spawned with **cwd = main**, because
///   `worktree-create` refuses to nest a worktree inside a worktree (§2).
/// - **Anywhere else**, the daemon cuts the worktree itself and spawns a plain
///   session in it. `claude --worktree` always writes to `.claude/worktrees/`,
///   so delegating there would create the worktree somewhere the daemon does not
///   look: it would register a path that does not exist, reconcile against a
///   missing directory, and fail to adopt the real one at `SessionStart`.
///
/// `fork` carries a conversation into the new worktree. It composes with both
/// paths — `claude --worktree --resume <prev> --fork-session --session-id <new>`
/// cuts the tree and replays the conversation into it — and `--resume` finds a
/// session by id wherever it was recorded, so the fork does not need the
/// original's working directory to exist.
pub async fn spawn_worktree_session(
    app: &Arc<AppState>,
    name: Option<&str>,
    fork: Option<SessionId>,
) -> Result<SessionId> {
    if let Some(name) = name {
        validate_worktree_name(name)?;

        // Worktree names must be unique over time (§2): the projects directory
        // is keyed by path, so reusing an archived worktree's name would
        // interleave the two sessions' transcripts.
        let inner = app.inner.read().await;
        if inner.workspaces.contains_key(name) {
            bail!("a workspace named {name} already exists");
        }
        if inner.sessions.values().any(
            |s| matches!(&s.recovery, Some(ArchiveState::Recoverable { name: n, .. }) if n == name),
        ) {
            bail!(
                "an archived session used the worktree name {name}; pick another (e.g. {name}-2) \
                 so its transcripts do not interleave"
            );
        }
        drop(inner);
        if app.cfg.worktree_path(name).exists() {
            bail!(
                "{} already exists on disk",
                app.cfg.worktree_path(name).display()
            );
        }
    }

    let id = Uuid::new_v4();
    let settings = Config::hooks_settings_path()?;
    let (mut env, unset) = crate::config::transcript_env();
    env.push(("ORCH_SESSION_ID".to_string(), id.to_string()));

    // Where the daemon looks for worktrees decides who creates this one.
    let delegated = app.cfg.worktrees_subdir_is_claude_default();

    // A name is required when the daemon cuts the worktree: only
    // `claude --worktree` can invent one, and the daemon must know the path up
    // front to create it.
    let owned_name = match (delegated, name) {
        (false, None) => Some(format!("wt-{}", &id.simple().to_string()[..8])),
        _ => None,
    };
    let name = name.or(owned_name.as_deref());

    let (spawn_cwd, cmd) = if delegated {
        let mut cmd = vec!["claude".to_string(), "--worktree".to_string()];
        // With no name, Claude Code generates one. That is also the only path
        // that cannot collide with an archived worktree by construction, since
        // it has never been used before.
        if let Some(name) = name {
            cmd.push(name.to_string());
        }
        // cwd is the main checkout, not the worktree-to-be.
        (app.cfg.main_checkout.clone(), cmd)
    } else {
        let name = name.context("a worktree name is required")?;
        let path = app.cfg.worktree_path(name);
        let (main, branch, base) = (
            app.cfg.main_checkout.clone(),
            format!("worktree-{name}"),
            app.cfg.upstream_ref.clone(),
        );
        let p = path.clone();
        tokio::task::spawn_blocking(move || crate::git::worktree_add_new(&main, &p, &branch, &base))
            .await
            .context("the worktree add panicked")??;
        // The daemon cut this tree, so nothing fired the repo's WorktreeCreate —
        // run the setup seam before the session opens.
        run_worktree_hooks(app, &path).await;
        // The session runs *in* the worktree, so it needs no `--worktree`.
        (path, vec!["claude".to_string()])
    };

    let mut cmd = cmd;
    if let Some(prev) = fork {
        cmd.push("--resume".into());
        cmd.push(prev.to_string());
        cmd.push("--fork-session".into());
    }
    cmd.extend([
        "--session-id".to_string(),
        id.to_string(),
        "--settings".to_string(),
        settings.to_string_lossy().into_owned(),
    ]);
    let spawned = PtyHandle::spawn(&cmd, &spawn_cwd, &env, &unset, DEFAULT_SIZE)?;

    // Without a name the path is not known until `SessionStart` reports the
    // cwd, so the workspace is registered there instead.
    let (workspace, cwd) = match name {
        Some(name) => {
            let expected = app.cfg.worktree_path(name);
            app.register_worktree(name, expected.clone(), Some(format!("worktree-{name}")))
                .await;
            (name.to_string(), expected)
        }
        None => (PENDING_WORKTREE.to_string(), app.cfg.main_checkout.clone()),
    };

    let mut session = Session::new(id, workspace, cwd, Kind::Interactive);
    session.forked_from = fork;
    // A fork opens on the parent's replayed conversation, so it has had a turn from
    // birth — the same as the fork path in `spawn_session`. Without this its own row
    // would offer no Fork and no nudge until it was first typed into, the very
    // false negative `had_a_turn` exists to remove.
    session.had_a_turn = fork.is_some();
    session.pty = Some(spawned.handle.clone());
    session.pid = spawned.pid;
    {
        let mut inner = app.inner.write().await;
        inner.sessions.insert(id, session);
    }

    watch_session_exit(app.clone(), id, spawned.handle);
    app.notify().await;
    Ok(id)
}

/// Start a headless `fix-pr` run pinned to the PR's head branch.
///
/// §8 writes this as a headless `--worktree` run, but `--worktree`
/// always cuts a fresh branch from `upstream/develop` while the same section
/// requires a worktree "pinned to that PR's head branch". The branch wins: the
/// worktree is created here and `claude -p` runs inside it.
pub async fn spawn_fix_pr_session(
    app: &Arc<AppState>,
    pr: u64,
    head_ref: &str,
) -> Result<SessionId> {
    // One agent per worktree. `ensure_pr_worktree` hands back the worktree a
    // review session is already sitting in, so spawning here unconditionally puts
    // two agents on one index, and fix-pr's first move is a rebase. Refusing is
    // the honest answer of the three: reusing that session silently would leave
    // it running someone else's instructions, and retargeting it mid-flight is
    // not the daemon's call. Finish the review and press fix again, or tell that
    // session to fix the build yourself.
    if let Some(ws) = worktree_holding(app, head_ref).await {
        if !app.live_sessions_in(&ws).await.is_empty() {
            bail!("{ws} already has a live session for #{pr}; finish or close it first");
        }
    }

    let workspace = ensure_pr_worktree(app, pr, head_ref).await?;
    let path = app
        .workspace_path(&workspace)
        .await
        .context("worktree vanished")?;

    // Headed, not `-p`: a run you can watch, answer and take over mid-flight, the
    // same shape as /resolve. The guard table is what decides whether the run may
    // start (§8); it never depended on the run being invisible.
    //
    // The prompt is a file the session is told to read rather than a slash
    // command, so nothing depends on `you/commands` being installed and
    // nothing is written into the checkout being driven.
    let prompt_file = vendored_prompt_file(app, pr, "fix-pr").await?;

    let id = Uuid::new_v4();
    let settings = Config::hooks_settings_path()?;
    let cmd = vec![
        "claude".to_string(),
        "--session-id".to_string(),
        id.to_string(),
        "--settings".to_string(),
        settings.to_string_lossy().into_owned(),
    ];

    let (mut env, unset) = crate::config::transcript_env();
    env.push(("ORCH_SESSION_ID".to_string(), id.to_string()));
    // Parallel runs collide on ports and docker resource names, so each gets
    // its own compose project and port base (§8).
    env.push(("COMPOSE_PROJECT_NAME".to_string(), format!("orchd-pr-{pr}")));
    env.push((
        "ORCHD_PORT_BASE".to_string(),
        (20000 + (pr % 1000) * 20).to_string(),
    ));

    let spawned = PtyHandle::spawn(&cmd, &path, &env, &unset, DEFAULT_SIZE)?;
    let mut session = Session::new(
        id,
        workspace,
        path,
        Kind::Automation {
            pr,
            command: crate::fix_pr::COMMAND.to_string(),
        },
    );
    session.pty = Some(spawned.handle.clone());
    session.pid = spawned.pid;
    session.pending_prompt = Some(format!(
        "Read {} and follow it. Those are your instructions for PR {pr}.",
        prompt_file.display()
    ));
    {
        let mut inner = app.inner.write().await;
        inner.sessions.insert(id, session);
    }

    watch_session_exit(app.clone(), id, spawned.handle);
    app.notify().await;
    Ok(id)
}

/// Spawn an interactive session pinned to a PR's head branch, and type a slash
/// command into it once it is ready.
///
/// The default answer to the rail's review button: a `claude` session in the PR
/// worktree running `/resolve <pr>` in the pane, the agent doing the reading,
/// fixing, pushing and posting itself while you supervise. The daemon does no
/// irreversible writes here — the agent does, in a shell you can take over. This is
/// the robust path; the native overlay is the opt-in alternative.
pub async fn spawn_command_session(
    app: &Arc<AppState>,
    pr: u64,
    head_ref: &str,
    command: &str,
) -> Result<SessionId> {
    // If the branch already has a worktree with a live session, take you there
    // rather than spawning a second one (§8).
    if let Some(ws) = worktree_holding(app, head_ref).await {
        let live = app.live_sessions_in(&ws).await;
        if let Some(id) = live.first() {
            return Ok(*id);
        }
        return start_with_prompt(app, &ws, pr, command).await;
    }

    // Otherwise pin a worktree to that branch. `git worktree add` directly,
    // because the WorktreeCreate hook always cuts a new branch from
    // upstream/develop; `worktree-link` still runs at SessionStart.
    {
        let name = format!("pr-{pr}");
        let inner = app.inner.read().await;
        if inner.sessions.values().any(|s| {
            matches!(&s.recovery, Some(ArchiveState::Recoverable { name: n, .. }) if n == &name)
        }) {
            bail!(
                "an archived session used the worktree name {name}; remove it or rename before \
                 reusing the name, or the two transcripts interleave"
            );
        }
    }
    let name = ensure_pr_worktree(app, pr, head_ref).await?;
    start_with_prompt(app, &name, pr, command).await
}

async fn start_with_prompt(
    app: &Arc<AppState>,
    workspace: &str,
    pr: u64,
    command: &str,
) -> Result<SessionId> {
    let prompt_file = vendored_prompt_file(app, pr, command).await?;
    let id = spawn_session(
        app,
        workspace,
        Kind::Automation {
            pr,
            command: command.to_string(),
        },
        None,
    )
    .await?;
    let mut inner = app.inner.write().await;
    if let Some(s) = inner.sessions.get_mut(&id) {
        // One line, because it is typed into the prompt box: the instructions
        // themselves are in the file. Typing the whole prompt would submit at the
        // first newline, and `/{command}` would resolve from the agent's command
        // path, which is the dependency the vendored prompts exist to remove.
        s.pending_prompt = Some(format!(
            "Read {} and follow it. Those are your instructions for PR {pr}.",
            prompt_file.display()
        ));
    }
    Ok(id)
}

/// Render the vendored prompt for `command` and leave it somewhere the session can
/// read it.
///
/// Under the daemon's own config dir, like `story`'s scratch and for the same two
/// reasons: the repo's `worktree-edit-boundary` hook blocks a write under the main
/// checkout that lands outside the worktree, and a file *inside* the worktree would
/// make the tree dirty — which the review flow then checks. Nothing of the
/// daemon's is written into the checkout it is driving.
async fn vendored_prompt_file(app: &Arc<AppState>, pr: u64, command: &str) -> Result<PathBuf> {
    let template = match command {
        "resolve" => crate::prompt::RESOLVE,
        "resolve-run" => crate::prompt::RESOLVE_RUN,
        "fix-pr" => crate::prompt::FIX_PR,
        other => bail!("no vendored prompt for /{other}"),
    };
    let (owner, repo) =
        crate::resolve_repo(app).context("no GitHub repo configured and none on the remote")?;
    let login = {
        let inner = app.inner.read().await;
        inner.viewer.clone()
    }
    .context("no GitHub login yet — the PR poller has not run")?;
    let body = crate::prompt::render(
        template,
        &crate::prompt::Vars {
            pr,
            owner,
            repo,
            login,
            upstream: app.cfg.upstream_ref.clone(),
            upstream_remote: app.cfg.upstream_remote.clone(),
            ask_base: format!("http://127.0.0.1:{}/api/session", app.cfg.port),
            language: app.cfg.default_language.clone(),
            // The review flow's template uses none of triage's or story's vars.
            ..Default::default()
        },
    )?;

    let dir = Config::config_dir()?.join(format!("{command}-{pr}"));
    std::fs::create_dir_all(&dir)?;
    let file = dir.join("prompt.md");
    // Rewritten per run: the login and the repo are resolved fresh, and a stale
    // copy would be read as this run's instructions.
    std::fs::write(&file, body).with_context(|| format!("writing {}", file.display()))?;
    Ok(file)
}

/// The worktree for a PR's head branch, created if absent.
/// The worktree holding `head_ref`, if one already does.
///
/// Main is never the answer, even when its branch set says it has been on this
/// ref: main's branches accumulate and are never removed (§2), so a PR whose head
/// main once visited would otherwise send a fix or a review run into the main
/// checkout — rebasing and force-pushing the one tree every worktree is cut from.
async fn worktree_holding(app: &Arc<AppState>, head_ref: &str) -> Option<String> {
    let inner = app.inner.read().await;
    inner
        .workspaces
        .values()
        .filter(|w| !w.is_main())
        .find(|w| w.branches.iter().any(|b| b == head_ref))
        .map(|w| w.id.clone())
}

/// The one case where a PR has nowhere to go, said in words.
///
/// Git will not check one branch out twice, and `worktree_holding` deliberately
/// does not count main (§2: a PR run must never rebase or force-push the tree
/// every worktree is cut from). So a PR whose branch main is standing on has
/// nowhere to go, and left to git it arrives as `is already used by worktree at
/// <main>` — true, and no help at all about what to do next.
///
/// [`park_main`] normally makes this unreachable by sending main back as the last
/// session there closes. What is left is the case it cannot handle: main is dirty,
/// or someone is still working in it.
///
/// Asked of git rather than of `Workspace::branches`, which accumulates every
/// branch main has ever been on and never drops one; only the live `HEAD` says
/// where the checkout actually is. An unreadable one is no opinion: the spawn goes
/// ahead and git gives its own answer, as it did before this existed.
async fn refuse_if_main_is_on(app: &Arc<AppState>, pr: u64, head_ref: &str) -> Result<()> {
    let main = app.cfg.main_checkout.clone();
    let on = tokio::task::spawn_blocking(move || crate::git::current_branch(&main))
        .await
        .map_err(|e| anyhow::anyhow!("reading main's branch panicked: {e}"))?;
    if on.as_deref().ok() != Some(head_ref) {
        return Ok(());
    }
    // Named for the message only, so the unresolved form is fine here: it reads
    // as the configured base, which is what you would go and change.
    let base = crate::git::base_branch(&app.cfg.upstream_ref);
    let why = if !app.live_sessions_in(MAIN).await.is_empty() {
        "a session is still open there"
    } else {
        "it has uncommitted changes"
    };
    bail!(
        "the main checkout is on {head_ref} and {why}, so it cannot be sent back to {base} — \
         and git will not check a branch out twice, which leaves no worktree to cut for #{pr}. \
         Close that session, or commit or stash the changes, and try again."
    )
}

/// Send the main checkout back to the base branch once nobody is working in it.
///
/// "Open in main" moves the checkout onto a PR's branch and, before this, nothing
/// ever moved it off: main would stand on a feature branch for days, quietly
/// making every PR flow for that branch impossible, because the worktree those
/// flows need cannot be cut while main holds the branch.
///
/// Deliberately at the *last* session's exit rather than at the moment a PR flow
/// wants the branch. Both fix the collision; this one stops it existing, and it
/// moves the checkout when you have just finished with it rather than in the
/// middle of something else.
///
/// Refuses in exactly the cases [`switch_main_to_pr`] refuses to move it the other
/// way. Uncommitted work is not ours to carry to another branch, and a session
/// still open there is someone still using it. Silence is the right answer to
/// both: nothing was promised, and the pre-flight above explains it if a PR flow
/// later needs the branch.
async fn park_main(app: &Arc<AppState>) {
    // A restart is not "you are done with main". Auto-resume brings that session
    // back expecting the branch it was working on, and moving the checkout here
    // would hand it someone else's code.
    if app.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    if !app.live_sessions_in(MAIN).await.is_empty() {
        return;
    }
    // Only the branch `open_pr(main)` checked in for a PR is ours to return to base.
    // A swapped-in branch (which can itself be a PR head) or a hand-checkout is a
    // deliberate placement, and parking it would undo the swap — the two features
    // fighting. Provenance, not a branch-name guess: `main_pr_park` records the one
    // action that means "return this to base when you're done".
    let parked_for_pr = app.main_pr_park.read().await.clone();
    let Some(parked_for_pr) = parked_for_pr else {
        return;
    };
    let path = app.cfg.main_checkout.clone();
    let base_ref = app.cfg.upstream_ref.clone();
    // Resolved, not the raw branch part: the default base is `origin/HEAD`, and
    // `git switch HEAD` fails with "a branch is expected". Unresolvable means the
    // symref has not been fetched yet, so there is nowhere to park.
    let exclude = app.cfg.worktrees_subdir_str();
    let moved = tokio::task::spawn_blocking(move || {
        // Reading the branch is a git call. If main has moved off the branch the
        // mark named — a swap, or a hand-checkout since — the mark is stale and
        // there is nothing of ours to park.
        let current = crate::git::current_branch(&path).ok()?;
        if current != parked_for_pr {
            tracing::debug!(%current, "main is not on its open-PR branch; not parking it");
            return None;
        }
        let base = crate::git::base_checkout_branch(&path, &base_ref)?;
        crate::git::park_on_base(&path, &base, Some(&exclude)).ok().flatten()
    })
    .await
    .ok()
    .flatten();

    // Whatever the branch was, the mark has done its job now the last session is
    // gone; a fresh `open_pr(main)` sets it again.
    *app.main_pr_park.write().await = None;

    if let Some(was) = moved {
        // Said out loud: the checkout under every worktree just changed, and the
        // rail showing `develop` with no explanation is a worse surprise than a
        // line in the log.
        tracing::info!(from = %was, "the last session in main closed; main is back on its base");
        let _ = app.reconcile(MAIN).await;
    }
}

pub async fn ensure_pr_worktree(app: &Arc<AppState>, pr: u64, head_ref: &str) -> Result<String> {
    if let Some(ws) = worktree_holding(app, head_ref).await {
        return Ok(ws);
    }

    let name = format!("pr-{pr}");
    validate_worktree_name(&name)?;
    let path = app.cfg.worktree_path(&name);
    if !path.exists() {
        refuse_if_main_is_on(app, pr, head_ref).await?;
        crate::git::worktree_add_existing(&app.cfg.main_checkout, &path, head_ref)?;
        // Only when we actually cut it. A PR worktree is always daemon-cut, so it
        // never saw the repo's WorktreeCreate — this is the gap that left the
        // pr-* worktrees without their rule-dedup file. Skipped when the tree was
        // already there, since setup ran when it was first created.
        run_worktree_hooks(app, &path).await;
    }
    app.register_worktree(&name, path, Some(head_ref.to_string()))
        .await;
    Ok(name)
}

/// Move the main checkout onto a PR's branch, so a session can open there.
///
/// The sibling of [`ensure_pr_worktree`]: both answer "get me onto this PR's
/// code", and they returned asymmetric shapes only because this one was inlined
/// in the handler while the other was a call. Refuses rather than half-doing it
/// — main is exclusive, and switching the one tree every worktree is cut from
/// under uncommitted work is not recoverable by pressing back.
pub async fn switch_main_to_pr(app: &Arc<AppState>, head_ref: &str) -> Result<String> {
    if let Some(held) = {
        let inner = app.inner.read().await;
        inner.workspaces.get(MAIN).and_then(|w| w.occupant)
    } {
        bail!(
            "a session already holds main ({}); end it before moving the checkout",
            &held.to_string()[..8]
        );
    }

    let path = app.cfg.main_checkout.clone();
    let branch = head_ref.to_string();
    // Excluding the worktrees dir: main contains it, so plain `is_clean` reads main
    // as dirty on any repo that has not gitignored it, and this refused forever.
    let exclude = app.cfg.worktrees_subdir_str();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if !crate::git::is_clean_excluding(&path, Some(&exclude))? {
            bail!("the main checkout has uncommitted changes; commit or stash them first");
        }
        crate::git::switch_branch(&path, &branch)
    })
    .await
    .map_err(|e| anyhow::anyhow!("switch task failed: {e}"))??;

    // Provenance for `park_main`: main is on this branch because *this* put it here
    // for a PR, so returning it to base when the session closes is right. A swap
    // that later moves main clears this, so its branch is never parked away.
    *app.main_pr_park.write().await = Some(head_ref.to_string());

    // The pane must be right about what is checked out the moment it changes.
    let _ = app.reconcile(MAIN).await;
    Ok(MAIN.to_string())
}

/// Start the session that carries out a triaged PR: the plan, then the agent.
///
/// The plan is written beside the prompt rather than fetched, because it is fixed
/// the moment you press the button: it is your decisions, resolved against a
/// fetch taken then. A session that re-read it later would be working from a
/// different set of answers than the one you approved.
pub async fn spawn_resolve_run(
    app: &Arc<AppState>,
    pr: u64,
    head_ref: &str,
    plan: &crate::post::Plan,
) -> Result<SessionId> {
    let workspace = ensure_pr_worktree(app, pr, head_ref).await?;
    if !app.live_sessions_in(&workspace).await.is_empty() {
        bail!("{workspace} already has a live session for #{pr}; finish or close it first");
    }

    let dir = Config::config_dir()?.join(format!("resolve-run-{pr}"));
    std::fs::create_dir_all(&dir)?;
    let plan_file = dir.join("plan.json");
    std::fs::write(&plan_file, serde_json::to_string_pretty(plan)?)
        .with_context(|| format!("writing {}", plan_file.display()))?;

    let prompt_file = vendored_prompt_file(app, pr, "resolve-run").await?;
    let id = spawn_session(
        app,
        &workspace,
        Kind::Automation {
            pr,
            command: "resolve-run".to_string(),
        },
        None,
    )
    .await?;
    let mut inner = app.inner.write().await;
    if let Some(s) = inner.sessions.get_mut(&id) {
        s.pending_prompt = Some(format!(
            "Read {} and follow it. Your plan for PR {pr} is {}.",
            prompt_file.display(),
            plan_file.display()
        ));
    }
    Ok(id)
}

/// Worktree names become directory names and branch names (`worktree-<name>`),
/// so anything that would escape the worktrees dir is refused outright.
pub fn validate_worktree_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("worktree name is empty");
    }
    if name.len() > 64 {
        bail!("worktree name is too long");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!("worktree name may only contain letters, digits, '-', '_' and '.'");
    }
    if name.starts_with('.') || name.contains("..") {
        bail!("worktree name may not start with '.' or contain '..'");
    }
    Ok(())
}

fn watch_session_exit(app: Arc<AppState>, id: SessionId, handle: Arc<PtyHandle>) {
    tokio::spawn(async move {
        handle.wait().await;
        // What this session *was* decides whether anything else has to be settled
        // now it is over. Read while the lock is already held; acted on below.
        let mut fix_pr_for: Option<u64> = None;
        // A turnless interactive session leaves nothing to come back to, so rather
        // than archive an empty row it is forgotten outright — and its headers-only
        // transcript is deleted here, outside the lock. `(cwd, recorded)` is what
        // finds the file.
        let mut forget: Option<(PathBuf, Option<PathBuf>)> = None;
        let workspace = {
            let mut inner = app.inner.write().await;
            // A relocation resumes under the *same id*, so by the time this wakes the
            // record may already describe a live session at another path — one that
            // installed its own pty and its own watcher. Settling it here would flip
            // it to `Exited` and, because `release_main` keys on the id, hand main's
            // claim back out from under it. The pty is the identity that distinguishes
            // them: only the watcher whose handle is still the session's own may
            // decide the session is over.
            if let Some(cur) = inner.sessions.get(&id).and_then(|s| s.pty.as_ref()) {
                if !Arc::ptr_eq(cur, &handle) {
                    return;
                }
            }
            match inner.sessions.get_mut(&id) {
                Some(s) => {
                    if s.state.is_live() {
                        s.set_state(State::Exited);
                    }
                    if let Kind::Automation { pr, command } = &s.kind {
                        if command == crate::fix_pr::COMMAND {
                            fix_pr_for = Some(*pr);
                        }
                    }
                    // Last chance to find the conversation. A session closed
                    // between two `Stop`s can be carrying a transcript path
                    // Claude Code reported and never wrote to, and once it is
                    // archived nothing else goes looking.
                    crate::store::pin_transcript(s.id, &s.cwd, &mut s.transcript_path);
                    let ws = s.workspace.clone();
                    // An interactive session that never had a turn is an empty pane,
                    // not a conversation: keeping its row and header file would only
                    // offer a resume that exits instantly and a fork that dies in a
                    // fresh worktree. Automation is exempt — a run that never got
                    // going is tracked as `Exhausted`, not deleted (§8).
                    if matches!(s.kind, Kind::Interactive) && !s.had_a_turn {
                        forget = Some((s.cwd.clone(), s.transcript_path.clone()));
                        inner.sessions.remove(&id);
                    }
                    Some(ws)
                }
                None => None,
            }
        };
        if let Some((cwd, recorded)) = forget {
            crate::store::delete_transcript(id, &cwd, recorded.as_deref());
            tracing::info!(session = %id, "closed before its first turn; forgotten");
        }
        // A fix run's verdict belongs to `fix_pr`, and this is the only place that
        // learns the run is over.
        if let Some(pr) = fix_pr_for {
            crate::fix_pr::settle(&app, pr).await;
        }
        app.release_main(id).await;
        if let Some(ws) = workspace {
            // The drawer's processes belong to whoever was working here. Only once
            // the workspace is empty, though: a worktree holds one session at a time
            // now, but the swap's relocation briefly leaves the incoming one live
            // beside the outgoing, and killing the shells then would pull them out
            // from under the session that is staying.
            if app.live_sessions_in(&ws).await.is_empty() {
                let killed = app.kill_processes_in(&ws).await;
                if killed > 0 {
                    tracing::info!(workspace = %ws, "stopped {killed} process(es) with the last session");
                }
                // And main goes back to its base branch, so it stops holding a
                // PR's branch hostage the moment you are done with it.
                if ws == MAIN {
                    park_main(&app).await;
                }
            }
            // Closing Claude is a moment the changed-file pane must be right
            // about: whatever the last turn left behind is now the whole story.
            let _ = app.reconcile(&ws).await;
        }
        app.notify().await;
    });
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// Start a managed process declared in config for this workspace.
pub async fn start_managed(
    app: &Arc<AppState>,
    workspace: &str,
    spec: &ManagedSpec,
) -> Result<String> {
    let path = app
        .workspace_path(workspace)
        .await
        .with_context(|| format!("unknown workspace {workspace}"))?;
    let spawned = PtyHandle::spawn(&spec.command, &path, &[], &[], DEFAULT_SIZE)?;
    // Subscribe here rather than inside the watcher task: a process that prints
    // its first lines immediately would otherwise have them delivered before
    // the task runs, and a build that was green from its very first line would
    // sit at `Starting` forever.
    let rx = spawned.handle.subscribe();
    let proc_id = format!("{workspace}:{}", spec.name);

    let process = Process {
        id: proc_id.clone(),
        name: spec.name.clone(),
        kind: ProcKind::Managed {
            command: spec.command.clone(),
        },
        health: Health::Starting,
        cwd: path,
        pty: Some(spawned.handle.clone()),
        pid: spawned.pid,
        started_at: SystemTime::now(),
    };

    {
        let mut inner = app.inner.write().await;
        if let Some(w) = inner.workspaces.get_mut(workspace) {
            w.processes.retain(|p| p.id != proc_id);
            w.processes.push(process);
        }
    }

    watch_health(
        app.clone(),
        workspace.to_string(),
        proc_id.clone(),
        spec.clone(),
        spawned.handle,
        rx,
    );
    app.notify().await;
    Ok(proc_id)
}

/// A plain `$SHELL` in the selected workspace's directory — the same directory
/// as the Claude session above it.
///
/// This is what makes the drawer agnostic: it hosts whatever pty you point at
/// it, and `ng-watch` is just the one main happens to declare (§2).
pub async fn spawn_shell(app: &Arc<AppState>, workspace: &str) -> Result<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    spawn_in_drawer(app, workspace, "shell", &[shell], AfterDrawerExit::Nothing).await
}

/// What happens once a drawer job ends.
///
/// Dispatched by the *one* observer already waiting on that pty. §8b's rule —
/// one pty exit, one observer — is what this exists to respect: a second
/// `handle.wait()` for the follow-up would work today and rot the moment "is this
/// over" has two answers maintained apart.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AfterDrawerExit {
    Nothing,
    /// Re-check the agent version, so a successful upgrade clears the nudge that
    /// prompted it instead of nagging until the next six-hourly poll.
    RecheckAgentUpdate,
}

/// Run one command as a drawer process, with a shell's lifecycle.
///
/// Split out of `spawn_shell` so a one-off job — upgrading the agent, so far —
/// gets the same hosting as a shell without a second exit observer: §8b's "one
/// pty exit, one observer" is exactly the rule a copy-pasted watcher breaks, and
/// the semantics wanted here are the shell's already. A clean exit removes the
/// tab, a failure keeps it with its output, which is what you want from a job you
/// pressed a button for and might need to read afterwards.
pub async fn spawn_in_drawer(
    app: &Arc<AppState>,
    workspace: &str,
    name: &str,
    argv: &[String],
    after: AfterDrawerExit,
) -> Result<String> {
    let path = app
        .workspace_path(workspace)
        .await
        .with_context(|| format!("unknown workspace {workspace}"))?;
    let spawned = PtyHandle::spawn(argv, &path, &[], &[], DEFAULT_SIZE)?;
    let proc_id = format!("{workspace}:{name}:{}", Uuid::new_v4().simple());

    let process = Process {
        id: proc_id.clone(),
        name: name.to_string(),
        kind: ProcKind::Shell { exit_code: None },
        // Shells get no health parsing and no restart policy.
        health: Health::Ok,
        cwd: path,
        pty: Some(spawned.handle.clone()),
        pid: spawned.pid,
        started_at: SystemTime::now(),
    };

    {
        let mut inner = app.inner.write().await;
        if let Some(w) = inner.workspaces.get_mut(workspace) {
            w.processes.push(process);
        }
    }

    // Ctrl+D means close. A shell that exits cleanly is removed outright rather
    // than left as a corpse tab you have to hunt down an × to clear.
    //
    // §2 says a dead shell keeps its buffer "until dismissed", and that is still
    // true of the case it was written for: a shell that died on its own, with a
    // non-zero code, keeps its output so the failure is not swallowed.
    let app2 = app.clone();
    let ws = workspace.to_string();
    let pid2 = proc_id.clone();
    tokio::spawn(async move {
        let code = spawned.handle.wait().await;
        {
            let mut inner = app2.inner.write().await;
            if let Some(w) = inner.workspaces.get_mut(&ws) {
                if code == 0 {
                    w.processes.retain(|p| p.id != pid2);
                } else if let Some(p) = w.processes.iter_mut().find(|p| p.id == pid2) {
                    p.kind = ProcKind::Shell {
                        exit_code: Some(code),
                    };
                    p.health = Health::Dead;
                }
            }
        }
        // Onward, from the one place that knows this pty is finished. A failed run
        // is re-checked too: the check is what decides whether the nudge stays, so
        // asking after a failure is how the bar comes back honestly rather than
        // being cleared on the assumption that a button press worked.
        if after == AfterDrawerExit::RecheckAgentUpdate {
            if let Err(e) = crate::agent_update::refresh(&app2).await {
                tracing::warn!("re-checking the agent version after an upgrade failed: {e:#}");
            }
        }
        app2.notify().await;
    });

    app.notify().await;
    Ok(proc_id)
}

/// Parse health from a managed process's output.
///
/// `ng-watch` matches Angular error blocks and the first error line becomes the
/// summary shown in the rail (§2).
fn watch_health(
    app: Arc<AppState>,
    workspace: String,
    proc_id: String,
    spec: ManagedSpec,
    handle: Arc<PtyHandle>,
    rx: tokio::sync::broadcast::Receiver<bytes::Bytes>,
) {
    tokio::spawn(async move {
        let mut rx = rx;
        // Anything already buffered between spawn and subscribe.
        let mut pending = String::from_utf8_lossy(&handle.snapshot()).into_owned();
        scan(&app, &workspace, &proc_id, &spec, &mut pending).await;
        loop {
            let chunk = match rx.recv().await {
                Ok(c) => c,
                // A lagged consumer only misses health lines, and the next
                // build will restate them; resubscribing beats tearing down.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            pending.push_str(&String::from_utf8_lossy(&chunk));
            scan(&app, &workspace, &proc_id, &spec, &mut pending).await;
        }

        {
            let mut inner = app.inner.write().await;
            if let Some(w) = inner.workspaces.get_mut(&workspace) {
                if let Some(p) = w.processes.iter_mut().find(|p| p.id == proc_id) {
                    p.health = Health::Dead;
                }
            }
        }
        app.notify().await;
    });
}

/// Drain whole lines out of `pending` and apply the health verdict they imply.
async fn scan(
    app: &Arc<AppState>,
    workspace: &str,
    proc_id: &str,
    spec: &ManagedSpec,
    pending: &mut String,
) {
    let Some(health) = crate::health::scan_lines(spec, pending) else { return };
    let mut dirty = false;
    {
        let mut inner = app.inner.write().await;
        if let Some(w) = inner.workspaces.get_mut(workspace) {
            if let Some(p) = w.processes.iter_mut().find(|p| p.id == proc_id) {
                if p.health != health {
                    p.health = health.clone();
                    dirty = true;
                }
            }
        }
        // A red build outranks a finished turn: promote any session in this
        // workspace that is merely waiting, and demote it back when the build
        // recovers (§2).
        if dirty {
            for s in inner.sessions.values_mut() {
                if s.workspace != workspace {
                    continue;
                }
                // Only sessions already at rest: a working agent is not waiting
                // on anything, so a red build is not its news yet. Keyed on the
                // health that just changed rather than a workspace-wide scan —
                // see `health::build_failure_in`, which is what `hooks.rs` asks
                // when a turn ends.
                match (&health, &s.state) {
                    (Health::Failing { summary }, State::YourTurn { .. }) => {
                        s.set_state(crate::health::at_rest(Some(summary)));
                    }
                    (Health::Ok, State::BuildFailing { .. }) => {
                        s.set_state(crate::health::at_rest(None));
                    }
                    _ => {}
                }
            }
        }
    }
    if dirty {
        app.notify().await;
    }
}

/// Resolve the worktree a session actually landed in.
///
/// `claude --worktree` reports the real path at `SessionStart`; until then the
/// daemon only knows where it asked for the worktree to be created.
pub fn worktree_name_of(path: &PathBuf, worktrees_dir: &PathBuf) -> Option<String> {
    path.strip_prefix(worktrees_dir)
        .ok()
        .and_then(|rest| rest.components().next())
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The race a relocation would otherwise lose.
    ///
    /// Relocating resumes under the *same id*, so the record the old watcher wakes
    /// up to find is a **live session at the far end** — and settling it there would
    /// mark it exited and, because `release_main` keys on the id, hand main's claim
    /// straight back out from under it. Two real ptys, because the pty is the
    /// identity that tells the two apart and a mock would be asserting the fix
    /// against itself.
    #[tokio::test]
    async fn a_stale_watcher_does_not_settle_the_session_that_replaced_it() {
        use crate::config::Config;
        use crate::pty::PtyHandle;
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7791}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let pty = |()| {
            PtyHandle::spawn(&["cat".to_string()], std::path::Path::new("/tmp"), &[], &[], (24, 80))
                .unwrap()
        };
        let (old, new) = (pty(()), pty(()));

        // The session as the relocation leaves it: same id, holding the *new* pty,
        // live, and owning main.
        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), dir.clone(), Kind::Interactive);
            s.pty = Some(new.handle.clone());
            s.set_state(State::Working);
            inner.sessions.insert(id, s);
        }
        app.claim_main(id).await.unwrap();

        // The watcher belonging to the pty that was killed on the way out.
        watch_session_exit(app.clone(), id, old.handle.clone());
        let _ = old.handle.kill();

        // Long enough that a watcher which was going to act has acted.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let inner = app.inner.read().await;
        let s = inner.sessions.get(&id).expect("the session is still there");
        assert!(s.state.is_live(), "a stale watcher exited the live session: {:?}", s.state);
        assert_eq!(
            inner.workspaces.get(MAIN).and_then(|w| w.occupant),
            Some(id),
            "a stale watcher released the main claim the resume had taken"
        );
        drop(inner);

        let _ = new.handle.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Park returns main to base only for the branch `open_pr(main)` marked, and a
    /// swapped-in branch (no mark) is left exactly where it is — even when it is a
    /// branch name park could otherwise reach. Driven against a real checkout,
    /// because the whole point is that the decision is provenance, not branch name.
    #[tokio::test]
    async fn park_reclaims_only_the_open_pr_branch_never_a_swapped_one() {
        use crate::config::Config;
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!(
            "orchd-park-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = dir.join("main");
        let git = |args: &[&str], at: &std::path::Path| {
            let ok = std::process::Command::new("git")
                .args(args)
                .current_dir(at)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main", "main"], &dir);
        git(&["config", "user.email", "t@t"], &repo);
        git(&["config", "user.name", "t"], &repo);
        std::fs::write(repo.join("f.txt"), "base\n").unwrap();
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "base"], &repo);
        git(&["branch", "feature/x"], &repo);

        // `origin/main` resolves to the local `main` branch as the base — no remote
        // needed, since only the branch part is used for a non-HEAD ref.
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7793,"upstream_ref":"origin/main"}}"#,
            repo.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let on = |b: &str| git(&["switch", "-q", b], &repo);
        let branch = || crate::git::current_branch(&repo).unwrap();

        // Marked as open-PR provenance → parked back to base, mark cleared.
        on("feature/x");
        *app.main_pr_park.write().await = Some("feature/x".to_string());
        park_main(&app).await;
        assert_eq!(branch(), "main", "an open-PR branch returns to base");
        assert!(app.main_pr_park.read().await.is_none(), "the mark is spent");

        // Same branch checked out, but no mark (the swap case) → left in place.
        on("feature/x");
        park_main(&app).await;
        assert_eq!(branch(), "feature/x", "a swapped-in branch is never parked away");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two hooks in order, and the second surviving the first.
    ///
    /// Order matters because linking into a tree that has not been based yet is
    /// the sequence the repo's own hooks assume. The survival matters because they
    /// answer different questions — is this branch based, does it have the files it
    /// needs beside the code — so a failed init must not silently cost the link
    /// too.
    #[tokio::test]
    async fn both_worktree_hooks_run_in_order_and_the_link_survives_a_failed_init() {
        let dir = std::env::temp_dir().join(format!(
            "orchd-wthooks-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("order.log");

        let mut cfg = crate::config::Config::parse(&format!(
            r#"{{"main_checkout":{:?}}}"#,
            dir.to_string_lossy()
        ))
        .expect("parse");
        // init appends and then fails; setup appends. If the failure short-circuited
        // the pair, the log would hold only "init".
        cfg.worktree_init = vec![
            "sh".into(),
            "-c".into(),
            format!("echo init >> {:?}; exit 3", log.to_string_lossy()),
        ];
        cfg.worktree_setup = vec![
            "sh".into(),
            "-c".into(),
            format!("echo setup >> {:?}", log.to_string_lossy()),
        ];
        let app = crate::state::AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        run_worktree_hooks(&app, &dir).await;

        let got = std::fs::read_to_string(&log).expect("both hooks wrote");
        assert_eq!(
            got.lines().collect::<Vec<_>>(),
            vec!["init", "setup"],
            "init runs first, and a non-zero init does not skip setup"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Neither configured is the default, and it must cost nothing.
    #[tokio::test]
    async fn no_worktree_hooks_configured_runs_nothing() {
        let dir = std::env::temp_dir().join(format!(
            "orchd-wtnone-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = crate::config::Config::parse(&format!(
            r#"{{"main_checkout":{:?}}}"#,
            dir.to_string_lossy()
        ))
        .expect("parse");
        assert!(cfg.worktree_init.is_empty() && cfg.worktree_setup.is_empty());
        let app = crate::state::AppState::new(cfg, "t".into(), crate::window::Chrome::None);
        // No panic, no process, nothing to assert but that it returns.
        run_worktree_hooks(&app, &dir).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_setup_resolves_a_repo_script_but_leaves_commands_alone() {
        let main = std::path::Path::new("/home/me/repo");
        // A repo-relative script path resolves against main, so it is found even
        // though the command runs with cwd set to the worktree.
        assert_eq!(
            resolve_setup_exe(main, ".claude/hooks/wt-setup"),
            "/home/me/repo/.claude/hooks/wt-setup"
        );
        assert_eq!(resolve_setup_exe(main, "scripts/setup.sh"), "/home/me/repo/scripts/setup.sh");
        // A bare command is a PATH lookup — untouched.
        assert_eq!(resolve_setup_exe(main, "just"), "just");
        // An absolute path is already unambiguous.
        assert_eq!(resolve_setup_exe(main, "/usr/local/bin/setup"), "/usr/local/bin/setup");
    }

    #[test]
    fn rejects_worktree_names_that_would_escape_the_worktrees_dir() {
        assert!(validate_worktree_name("invoice-export").is_ok());
        assert!(validate_worktree_name("../../etc").is_err());
        assert!(validate_worktree_name("a/b").is_err());
        assert!(validate_worktree_name(".hidden").is_err());
        assert!(validate_worktree_name("").is_err());
    }

    // Mirrors the real ng-watch spec (`config::default_main_processes`) so these
    // health-scan tests track the patterns actually shipped.
    #[test]
    fn reads_the_worktree_name_out_of_a_path() {
        let dir = PathBuf::from("/repo/.claude/worktrees");
        let p = PathBuf::from("/repo/.claude/worktrees/invoice/src/Foo.php");
        assert_eq!(worktree_name_of(&p, &dir), Some("invoice".to_string()));
        assert_eq!(worktree_name_of(&PathBuf::from("/repo/src"), &dir), None);
    }
}
