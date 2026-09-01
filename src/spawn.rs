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
    let (src_cwd, src_workspace, handle, title, name, created_at, kind) = {
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
            s.name.clone(),
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
            restore_after_relocate(app, id, title, name, created_at).await;
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
            restore_after_relocate(app, forked, title, name, created_at).await;
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
    name: Option<String>,
    created_at: std::time::SystemTime,
) {
    let mut inner = app.inner.write().await;
    if let Some(s) = inner.sessions.get_mut(&id) {
        // A fork has its own title to earn, but it opens on the same conversation,
        // so showing the old one beats showing the workspace name.
        if s.title.is_none() {
            s.title = title;
        }
        // A name you typed is not the resume's to re-earn: unconditional, unlike
        // the title, because the only thing that clears one is you clearing it.
        if s.name.is_none() {
            s.name = name;
        }
        s.created_at = created_at;
        // Wherever the file ended up — the move may have been skipped, and Claude
        // may have re-filed it. This is the same self-heal the exit path does.
        crate::store::pin_transcript(s.id, &s.cwd, &mut s.transcript_path);
    }
}


/// Put the record in, start the pty, and hang the handle on the record.
///
/// **The insert happens before the process starts, and that ordering is the whole
/// point of this function.** Claude Code fires `SessionStart` while it boots, and a
/// hook naming a session the daemon has not recorded yet is dropped in silence:
/// the handler looks the id up, finds nothing, and answers `ok`. What that leaves
/// behind depends on the session — an interactive one sits at `starting` until
/// something else happens to move it, and one carrying a `pending_prompt` never
/// gets the prompt typed at all, because that hook is what types it.
///
/// The window is between the spawn and the insert, so it is lost by being *fast*.
/// Reported from a Mac, and invisible to every test here, whose fake agent takes
/// longer to speak than a real one.
///
/// A spawn that fails takes the record straight back out, so a refusal still costs
/// nothing. The record is briefly in the map with no pty, which is a state it
/// already has to survive: every session restored from disk starts that way.
async fn insert_and_spawn(
    app: &Arc<AppState>,
    id: SessionId,
    session: Session,
    cmd: &[String],
    cwd: &std::path::Path,
    env: &[(String, String)],
    unset: &[&str],
) -> Result<crate::pty::Spawned> {
    {
        let mut inner = app.inner.write().await;
        inner.sessions.insert(id, session);
    }
    let spawned = match PtyHandle::spawn(cmd, cwd, env, unset, DEFAULT_SIZE) {
        Ok(spawned) => spawned,
        Err(e) => {
            let mut inner = app.inner.write().await;
            inner.sessions.remove(&id);
            return Err(e);
        }
    };
    {
        let mut inner = app.inner.write().await;
        if let Some(s) = inner.sessions.get_mut(&id) {
            s.pty = Some(spawned.handle.clone());
            s.pid = spawned.pid;
        }
    }
    Ok(spawned)
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

    // Carried first, read second. A resume or a fork continues a conversation that
    // already knows what it is about, and the tree it comes back to may have been
    // swapped since, so asking git here would quietly rewrite the conversation's
    // own history to match whatever is checked out now, which is the mismatch this
    // field exists to catch. Only a genuinely new session has to ask the tree.
    //
    // The undelivered arrival notice rides along for the same reason: a session
    // moved while it was not running is told when auto-resume brings it back, and
    // that resume is exactly this path.
    let (carried_branch, carried_notice) = match resume {
        Some(Source::Resume(prev)) | Some(Source::Fork(prev)) => {
            let inner = app.inner.read().await;
            let prev = inner.sessions.get(&prev);
            (
                prev.and_then(|s| s.branch.clone()),
                prev.and_then(|s| s.arrival_notice.clone()),
            )
        }
        None => (None, None),
    };

    // A name you typed survives a resume. The record is rebuilt under the same id,
    // so unless it is carried here it reverts to the workspace default — which is
    // exactly what auto-resume on a restart did, since it resumes through this path
    // and never through `restore_after_relocate`. The title needs no carry: it is
    // re-read from the transcript. Resume only, not fork: a fork is a new
    // conversation and earns its own name (the degraded-relocate fork is the one
    // exception, and it gets the name back through `restore_after_relocate`).
    let carried_name = match resume {
        Some(Source::Resume(prev)) => {
            app.inner.read().await.sessions.get(&prev).and_then(|s| s.name.clone())
        }
        _ => None,
    };

    // A conversation the daemon has moved still believes it is isolated in the tree
    // it started in: Claude Code pins that in the transcript and re-appends it every
    // turn, so it outlives the swap, the restart and the resume. Telling the agent
    // was the whole mitigation and it is not enough — the notice is one instruction
    // an agent may not act on, and one conversation went two days editing a worktree
    // that had been cut again for a different branch, its own branch sitting in main.
    //
    // So the correction is written rather than requested. Here because this is the
    // one moment it is safe: the previous process is gone and the next has not
    // started, so nothing else is appending to that file. Only when the pin really
    // disagrees — a session resumed into the tree it is pinned to is correctly
    // isolated and must stay that way.
    let transcript = resume.and_then(|_| {
        // The id scan as a fallback, because the case that breaks the cheap slug
        // lookup is this one: a relocation whose `move_transcript` failed leaves the
        // file under the old tree's slug, and that is a conversation that is *more*
        // likely to be carrying a stale pin, not less.
        crate::store::transcript_file(id, &path, None).or_else(|| crate::store::find_transcript(id))
    });
    if let Some(t) = transcript {
        match crate::store::worktree_pin(&t) {
            Some(pin) if pin != path => match crate::store::clear_worktree_pin(id, &path, &t) {
                Ok(()) => tracing::info!(
                    session = %id,
                    "cleared a worktree pin on {} for a conversation now in {}",
                    pin.display(),
                    path.display()
                ),
                // Not fatal: the arrival notice still says it in words, and a
                // session that comes back isolated is what happened before this.
                Err(e) => tracing::warn!(session = %id, "could not clear the worktree pin: {e:#}"),
            },
            _ => {}
        }
    }

    let mut session = Session::new(id, workspace.to_string(), path.clone(), kind);
    session.interrupted = interrupted;
    session.had_a_turn = had_a_turn;
    session.branch = carried_branch.or_else(|| crate::git::current_branch(&path).ok());
    session.arrival_notice = carried_notice;
    session.name = carried_name;
    if let Some(Source::Fork(prev)) = resume {
        session.forked_from = Some(prev);
    }

    let (env, unset) = crate::config::session_env(&app.cfg, &path, id, Some(&session.ask_token));
    let spawned = insert_and_spawn(app, id, session, &cmd, &path, &env, &unset).await?;
    // The claim belongs to the record, so it is settled once the record is in.
    // Until this insert the map still described whatever stood here under this id,
    // and a relocation reuses the id — `reclaim_main` has what that let the
    // outgoing session's watcher do to the incoming session's claim. After it the
    // pty guard in `watch_session_exit` turns that watcher away, so this is the
    // last moment the window is open.
    if workspace == MAIN {
        app.reclaim_main(id).await;
    }

    watch_session_exit(app.clone(), id, spawned.handle);
    crate::agent_update::refresh_detached(&app);
    app.notify().await;
    Ok(id)
}

/// Carry a forked-from tree's uncommitted work into the fork, and say what did not.
///
/// Returns the sentence the fork is told at its first prompt, or `None` when the
/// parent was clean and there is nothing to say.
///
/// **Never fatal.** A fork whose files did not travel is still a usable fork on the
/// right commit, and the banked work is still in the parent tree where it always
/// was — `copy_wip` does not reset the source. Failing the spawn would trade a
/// partial success for none.
///
/// Untracked files do not travel: `stash create` has no `--include-untracked`. They
/// are named rather than silently left, the same rule the swap follows, because
/// half your work not arriving is exactly the thing you find out about too late.
fn carry_into(parent: &std::path::Path, fork: &std::path::Path, exclude: &str) -> Option<String> {
    let mut said = Vec::new();
    match crate::git::copy_wip(parent, fork) {
        Ok(Some(_)) => said.push("its uncommitted changes were carried in with it".to_string()),
        Ok(None) => {}
        Err(e) => {
            tracing::warn!("fork could not carry the uncommitted work: {e:#}");
            said.push(format!("its uncommitted changes could NOT be carried in ({e})"));
        }
    }
    // Excluding the worktrees dir, because a fork of a session in main would
    // otherwise walk every worktree in the checkout — the case `Untracked::Collapsed`
    // exists for, and which `reconcile` already guards against.
    match crate::git::untracked_in(parent, Some(exclude)) {
        Ok(f) if !f.is_empty() => {
            let shown: Vec<&str> = f.iter().take(4).map(String::as_str).collect();
            let more = f.len().saturating_sub(shown.len());
            said.push(format!(
                "{} untracked file{} stayed in the original tree and {} NOT here: {}{}",
                f.len(),
                if f.len() == 1 { "" } else { "s" },
                if f.len() == 1 { "is" } else { "are" },
                shown.join(", "),
                if more > 0 { format!(", and {more} more") } else { String::new() },
            ));
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("fork could not list the parent's untracked files: {e:#}"),
    }
    (!said.is_empty()).then(|| said.join(". "))
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
/// `fork` carries a conversation into the new worktree, **and its files with it**.
/// A fork therefore always takes the second path, whatever this repo's layout: it
/// is cut from the parent's HEAD rather than from upstream, and the parent's
/// uncommitted work is copied in before the session starts, so the tree looks like
/// the one the replayed conversation remembers. `claude --worktree` cannot do
/// either half — it takes a name and nothing else, and does not report the path
/// until `SessionStart`. The repo's `WorktreeCreate` still runs: `create_worktree`
/// invokes it and adopts the tree it makes, then puts that tree on the parent's
/// commit.
///
/// `--resume` finds a session by id wherever it was recorded, so a fork still does
/// not *need* the original's working directory to exist. Without it there is simply
/// nothing to cut from and nothing to carry, and the fork comes back on the base
/// branch exactly as it always did.
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
    // Minted here rather than taken off the record, because the record cannot exist
    // yet: without a name the workspace is only known once `SessionStart` reports
    // the cwd, so `Session::new` happens after the pty. The token has to be in the
    // environment the pty is *given*, so it is generated first and written onto the
    // session below.
    let ask_token = crate::state::random_token();

    // Where the daemon looks for worktrees decides who creates this one — **unless
    // this is a fork**, which the daemon always cuts itself. `claude --worktree`
    // takes a name and nothing else: it cannot be told to branch from the parent,
    // and it does not report the path until `SessionStart`, so neither half of
    // carrying the parent's files is possible through it. `create_worktree` still
    // runs the repo's `WorktreeCreate`, so the only thing a fork gives up is Claude
    // Code choosing the name.
    let delegated = app.cfg.worktrees_subdir_is_claude_default() && fork.is_none();

    // The tree a fork is cut from and carries the work of. `None` when there is no
    // parent tree left — a fork is deliberately cheaper than a resume, so a
    // conversation whose worktree is long gone can still be forked; it just comes
    // back on the base branch with nothing carried, exactly as it did before.
    let parent_tree = match fork {
        Some(prev) => {
            let cwd = app.inner.read().await.sessions.get(&prev).map(|s| s.cwd.clone());
            cwd.filter(|p| p.exists())
        }
        None => None,
    };
    // What travelled, in words, for the arrival notice below.
    let mut carried: Option<String> = None;

    // A name is required when the daemon cuts the worktree: only
    // `claude --worktree` can invent one, and the daemon must know the path up
    // front to create it.
    let owned_name = match (delegated, name) {
        (false, None) => Some(format!("wt-{}", &id.simple().to_string()[..8])),
        _ => None,
    };
    let name = name.or(owned_name.as_deref());

    let (spawn_cwd, cmd, made_at) = if delegated {
        let mut cmd = vec!["claude".to_string(), "--worktree".to_string()];
        // With no name, Claude Code generates one. That is also the only path
        // that cannot collide with an archived worktree by construction, since
        // it has never been used before.
        if let Some(name) = name {
            cmd.push(name.to_string());
        }
        // cwd is the main checkout, not the worktree-to-be, and the path is Claude
        // Code's to choose and to report at `SessionStart`.
        (app.cfg.main_checkout.clone(), cmd, None)
    } else {
        let name = name.context("a worktree name is required")?;
        let path = app.cfg.worktree_path(name);
        // A fork branches from the parent's HEAD, not from upstream: the point is a
        // tree that looks like the one the conversation remembers, and the parent's
        // *committed* work is most of that. Resolved to a sha so the base cannot
        // move between here and the `worktree add`.
        let base = match parent_tree.clone() {
            // On a blocking thread like every other git call here: `head_sha` is a
            // subprocess, and the fork path is already the slowest spawn there is.
            Some(p) => tokio::task::spawn_blocking(move || crate::git::head_sha(&p).ok())
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| app.cfg.upstream_ref.clone()),
            None => app.cfg.upstream_ref.clone(),
        };
        let branch = format!("worktree-{name}");
        // The repo's own `WorktreeCreate` if it has one, ours if not. The path can
        // come back different: the hook chooses where it puts things.
        let path = create_worktree(
            app,
            name,
            &path,
            Want::New {
                branch: &branch,
                base: &base,
            },
        )
        .await?;
        // `worktree_setup` on top, for a repo that keeps real setup inside its
        // `WorktreeCreate` and would otherwise have it run twice or not at all. It
        // is configured per repo and does nothing when unset.
        run_worktree_hooks(app, &path).await;
        // Now the parent's uncommitted work, on top of the parent's HEAD the tree
        // was just cut from. Before the session starts, so the agent never sees the
        // tree change under it.
        if let Some(parent) = parent_tree.clone() {
            // Four git subprocesses including a `stash apply`, so not on the executor.
            let exclude = app.cfg.worktrees_subdir_str();
            let to = path.clone();
            carried = tokio::task::spawn_blocking(move || carry_into(&parent, &to, &exclude))
                .await
                .unwrap_or(None);
        }
        // The session runs *in* the worktree, so it needs no `--worktree`. The path
        // travels with it: `create_worktree` may have adopted a tree the repo's hook
        // put somewhere else, and registering the path we *asked* for would leave the
        // daemon reconciling a directory that does not exist.
        (path.clone(), vec!["claude".to_string()], Some(path))
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
    // After the arms, because only they know where this runs — and in the
    // delegated arm that is the main checkout, whose environment is the one the
    // worktree about to be cut from it would have had anyway.
    let (env, unset) = crate::config::session_env(&app.cfg, &spawn_cwd, id, Some(&ask_token));

    // Without a name the path is not known until `SessionStart` reports the
    // cwd, so the workspace is registered there instead.
    let (workspace, cwd) = match name {
        Some(name) => {
            // Where it actually is when the daemon cut it, and where Claude Code puts
            // one otherwise.
            let at = made_at.unwrap_or_else(|| app.cfg.worktree_path(name));
            app.register_worktree(name, at.clone(), Some(format!("worktree-{name}")))
                .await;
            (name.to_string(), at)
        }
        None => (PENDING_WORKTREE.to_string(), app.cfg.main_checkout.clone()),
    };

    // `cwd` is cloned because the arrival notice below names it.
    let mut session = Session::new(id, workspace, cwd.clone(), Kind::Interactive);
    // The one the pty already holds. `Session::new` always mints a fresh token, so
    // leaving this out is not a missing credential but a *mismatched* one, and the
    // agent's asks would be refused rather than failing to be attempted.
    session.ask_token = ask_token;
    session.forked_from = fork;
    // A fork opens on the parent's replayed conversation, so it has had a turn from
    // birth — the same as the fork path in `spawn_session`. Without this its own row
    // would offer no Fork and no nudge until it was first typed into, the very
    // false negative `had_a_turn` exists to remove.
    session.had_a_turn = fork.is_some();
    // And it is *told* where it woke up. The conversation it replays was written in
    // another checkout, so without this the fork reasons about the parent's tree
    // from memory and the first remembered path it uses is wrong — the same failure
    // a relocation's notice exists to prevent, in the one place that never sent one.
    if fork.is_some() {
        let what = carried
            .as_deref()
            .unwrap_or("The original tree had no uncommitted work to carry");
        session.arrival_notice = Some(format!(
            "You are a fork, in a new worktree at {} cut from the commit the \
             conversation you are replaying was on. It is not the tree that \
             conversation was written in, so re-read before trusting a remembered \
             path. {what}.",
            cwd.display()
        ));
    }
    let spawned = insert_and_spawn(app, id, session, &cmd, &spawn_cwd, &env, &unset).await?;

    watch_session_exit(app.clone(), id, spawned.handle);
    crate::agent_update::refresh_detached(&app);
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
    if let Some(ws) = branch_busy(app, head_ref).await {
        bail!("{ws} already has a live session for #{pr}; finish or close it first");
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
    // command, so nothing has to be installed on the agent's side and nothing is
    // written into the checkout being driven.
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

    // No ask token: a fix run is handed its URL substituted into its prompt and has
    // nothing to ask, which is the narrower surface 942d01b chose on purpose.
    let (mut env, unset) = crate::config::session_env(&app.cfg, &path, id, None);
    // Parallel runs collide on ports and docker resource names, so each gets
    // its own compose project and port base (§8).
    env.push(("COMPOSE_PROJECT_NAME".to_string(), format!("orchd-pr-{pr}")));
    env.push((
        "ORCHD_PORT_BASE".to_string(),
        (20000 + (pr % 1000) * 20).to_string(),
    ));

    let mut session = Session::new(
        id,
        workspace,
        path.clone(),
        Kind::Automation {
            pr,
            command: crate::fix_pr::COMMAND.to_string(),
        },
    );
    // Typed in by the `SessionStart` handler, which is the reason this record has
    // to be in the map before the process starts rather than after: a run whose
    // hook arrived first was a run that never received its instructions.
    session.pending_prompt = Some(format!(
        "Read {} and follow it. Those are your instructions for PR {pr}.",
        prompt_file.display()
    ));
    let spawned = insert_and_spawn(app, id, session, &cmd, &path, &env, &unset).await?;

    watch_session_exit(app.clone(), id, spawned.handle);
    crate::agent_update::refresh_detached(&app);
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
    //
    // No name-reuse refusal here, deliberately, and it was removed rather than
    // never written. It refused whenever an archived session's recovery record
    // named `pr-<n>` — which teardown writes — so reviewing a PR whose worktree you
    // had torn down was refused for good, with advice ("rename") that cannot be
    // followed for a name the daemon derives from the PR number. `fix-pr` and
    // `triage` never had the check and were unaffected, so one PR answered two ways.
    //
    // What it claimed to prevent does not happen: transcripts are keyed by session
    // uuid, so two sessions under one directory slug are two files. The real harm
    // was elsewhere — resuming an archived session into a worktree cut again at its
    // path — and `worktree::branch_drift` says so on the resume itself.
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
/// The ref a per-PR run rebases onto: the PR's *own* base branch on the upstream
/// remote, not the daemon's global `upstream_ref`.
///
/// A PR opened against a release branch, an LTS branch, or stacked on another
/// branch has a base that is not the configured default. Rebasing it onto
/// `upstream_ref` regardless then force-pushes history rebased onto the wrong
/// ancestor — silently, since the rebase usually succeeds mechanically. Falls back
/// to the configured base when the PR is not in the poll or GitHub named no base,
/// which for a normal PR is identical to it (`upstream_remote`/`base_ref` ==
/// `upstream_ref`), so nothing changes for the ordinary case.
fn rebase_target(upstream_ref: &str, upstream_remote: &str, base_ref: Option<&str>) -> String {
    match base_ref {
        Some(b) if !b.is_empty() => format!("{upstream_remote}/{b}"),
        _ => upstream_ref.to_string(),
    }
}

async fn vendored_prompt_file(app: &Arc<AppState>, pr: u64, command: &str) -> Result<PathBuf> {
    let template = match command {
        "resolve" => crate::prompt::RESOLVE,
        "resolve-run" => crate::prompt::RESOLVE_RUN,
        "fix-pr" => crate::prompt::FIX_PR,
        other => bail!("no vendored prompt for /{other}"),
    };
    let (owner, repo) =
        crate::resolve_repo(app).context("no GitHub repo configured and none on the remote")?;
    let (login, base_ref) = {
        let inner = app.inner.read().await;
        let base = inner
            .prs
            .iter()
            .find(|p| p.number == pr)
            .map(|p| p.base_ref.clone());
        (inner.viewer.clone(), base)
    };
    let login = login.context("no GitHub login yet — the PR poller has not run")?;
    let upstream = rebase_target(
        &app.cfg.upstream_ref,
        &app.cfg.upstream_remote,
        base_ref.as_deref(),
    );
    let body = crate::prompt::render(
        template,
        &crate::prompt::Vars {
            pr,
            owner,
            repo,
            login,
            upstream,
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

/// Where this branch is already recorded, whether or not the tree is still there.
///
/// A workspace record outlives the directory it names: `claude --worktree` removes
/// its own tree when that session ends, and only `worktree::teardown` ever drops a
/// record. The PR flows key on the record, so a vanished one used to be handed back
/// as if it stood — the run was then aimed at a path that is not there, which
/// `portable-pty` turns into `$HOME` rather than an error.
///
/// The path comes back with the name because the answer to a missing tree is to
/// **rebuild it where it stood**, not to cut a second one somewhere else. The
/// session is what owns that directory: transcripts are keyed by working directory,
/// so a conversation resumed later looks for its own path, and two trees on one
/// branch is a choice `worktree_holding` should never have to make.
///
/// Deliberately not folded into [`worktree_holding`], which answers "is anyone
/// working on this branch". That one must keep saying yes for a live session whose
/// tree was deleted under it, or a fix run would start beside it.
async fn recorded_worktree_for(
    app: &Arc<AppState>,
    head_ref: &str,
) -> Option<(String, std::path::PathBuf)> {
    let ws = worktree_holding(app, head_ref).await?;
    let path = app.workspace_path(&ws).await?;
    Some((ws, path))
}

/// The worktree that makes this PR's branch busy, if one does.
///
/// The single definition of "busy", because the guard and the spawn had two. The
/// guard asked `is_busy()` (mid-turn) while the spawn enforces `is_live()` (any
/// open session), so an idle session on the branch passed a guard whose own
/// refusal says "live session" and was then refused at the spawn — after the run
/// had been announced. `is_live` is the operative rule: fix-pr rebases, and a
/// session sitting at its prompt is one you are still working in.
pub async fn branch_busy(app: &Arc<AppState>, head_ref: &str) -> Option<String> {
    let ws = worktree_holding(app, head_ref).await?;
    (!app.live_sessions_in(&ws).await.is_empty()).then_some(ws)
}
/// Refuse a PR flow only when main holds something that cannot be moved.
///
/// A **live session** is that thing: the checkout it is working in is not ours to
/// change under it, and no amount of git makes that safe.
///
/// Uncommitted changes used to be refused here too, which is what made a closed
/// session with a dirty tree a dead end — the branch stayed in main, and every PR
/// flow for it was impossible until you went and stashed by hand. They are carried
/// now; see the move in [`ensure_pr_worktree`].
async fn refuse_if_main_is_busy(app: &Arc<AppState>, pr: u64, head_ref: &str) -> Result<()> {
    if app.live_sessions_in(MAIN).await.is_empty() {
        return Ok(());
    }
    bail!(
        "the main checkout is on {head_ref} and a session is still open there, so the branch \
         cannot move out of it — and git will not check a branch out twice, which leaves no \
         worktree to cut for #{pr}. Close that session, or move it out of main from its context \
         menu, and try again."
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
    let recorded = recorded_worktree_for(app, head_ref).await;
    if let Some((ws, path)) = &recorded {
        if path.is_dir() {
            return Ok(ws.clone());
        }
    }

    // A record whose tree is gone keeps its name and its path, and the arms below
    // cut it again there, on this PR's head ref. That is the same repair
    // `worktree::revive` does for a resumed session, reached from the other side.
    let (name, mut path) = recorded.unwrap_or_else(|| {
        let name = format!("pr-{pr}");
        let path = app.cfg.worktree_path(&name);
        (name, path)
    });
    validate_worktree_name(&name)?;
    if !path.exists() {
        /* Main holding this very branch is the one case where cutting the tree and
           freeing main are the same act, so it is done rather than refused.
           `park_main` leaves a dirty main exactly where it is — correctly, it will
           not carry your work to another branch — and the branch then sits there
           for days making every PR flow for it impossible, which is the state this
           used to bail out of and send you off to stash.
           `move_branch_out` is the way out that loses nothing: the branch *and* its
           uncommitted work land in the tree this flow was about to create anyway,
           and main goes back to base. Untracked files stay in main, which
           `move_branch_out` documents and this cannot help. */
        if main_is_on(app, head_ref).await? {
            refuse_if_main_is_busy(app, pr, head_ref).await?;
            let moved = {
                let (main, path) = (app.cfg.main_checkout.clone(), path.clone());
                let base_ref = app.cfg.upstream_ref.clone();
                let head_ref = head_ref.to_string();
                tokio::task::spawn_blocking(move || -> Result<crate::git::MovedOut> {
                    let base = crate::git::base_checkout_branch(&main, &base_ref).ok_or_else(
                        || anyhow::anyhow!("no base branch to put main back on — {base_ref} has not been fetched"),
                    )?;
                    crate::git::move_branch_out(&main, &path, &base, &head_ref)
                })
                .await
                .map_err(|e| anyhow::anyhow!("moving main's branch out panicked: {e}"))??
            };
            // Main gave the branch away, and `reconcile` only adds; the open-PR mark
            // has done its job, since main is back on base by our own hand.
            app.forget_branch(MAIN, head_ref).await;
            *app.main_pr_park.write().await = None;
            let _ = app.reconcile(MAIN).await;
            /* A log line rather than something in the response, for `park_main`'s
               reason: the checkout under every worktree just changed and that is
               worth recording, but there are five callers of this and threading a
               warning up through all of them buys little. `wip_error` is close to
               impossible here anyway — the work is re-applied onto a fresh checkout
               of the branch it came from, so the apply lands on the tree it was
               taken from, which is the same argument `swap_branches` makes. */
            tracing::info!(
                %head_ref, wip_error = ?moved.wip_error,
                "main was on #{pr}'s branch, so it moved into {name} and main went back to {}",
                moved.base
            );
            run_worktree_hooks(app, &path).await;
        } else {
            // The repo's own `WorktreeCreate` if it has one, ours if not. It cuts
            // from a base of its own, so the tree is then put on the PR's head ref.
            path = create_worktree(app, &name, &path, Want::Existing { branch: head_ref }).await?;
            // Only when we actually cut it. Skipped when the tree was already there,
            // since setup ran when it was first created.
            run_worktree_hooks(app, &path).await;
        }
    }
    app.register_worktree(&name, path, Some(head_ref.to_string()))
        .await;
    Ok(name)
}

/// Whether the main checkout has this branch checked out right now.
async fn main_is_on(app: &Arc<AppState>, head_ref: &str) -> Result<bool> {
    let main = app.cfg.main_checkout.clone();
    let on = tokio::task::spawn_blocking(move || crate::git::current_branch(&main))
        .await
        .map_err(|e| anyhow::anyhow!("reading main's branch panicked: {e}"))?;
    Ok(on.as_deref().ok() == Some(head_ref))
}

/// Move the main checkout onto a PR's branch, so a session can open there.
///
/// The sibling of [`ensure_pr_worktree`]: both answer "get me onto this PR's
/// code", and they returned asymmetric shapes only because this one was inlined
/// in the handler while the other was a call. Refuses rather than half-doing it
/// — main is exclusive, and switching the one tree every worktree is cut from
/// under uncommitted work is not recoverable by pressing back.
pub async fn switch_main_to_pr(app: &Arc<AppState>, head_ref: &str) -> Result<String> {
    // The canonical live-filtered read, not the bare `occupant`: a stale occupant
    // (session gone, `release_main` not yet run) must not block moving the checkout
    // when `claim_main` would already let a new session in.
    if let Some(held) = app.main_occupant().await {
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
    // The agent's view, not the whole record: `for_agent` drops the daemon's
    // per-thread bookkeeping, which the prompt promises is not in this file.
    std::fs::write(&plan_file, serde_json::to_string_pretty(&plan.for_agent())?)
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
        // Same idea for a resolve run, and the reason it is needed at all: the run
        // record is the daemon's, so nothing else would ever notice that the thing
        // working through it had stopped. Threads left `pending` then read as
        // imminent for as long as the daemon runs.
        let mut resolve_run_for: Option<u64> = None;
        // And the review that asked, on its way out, for the CI it is not allowed to
        // touch to be picked up by a run. Read here for the same reason as the two
        // above: this is where a pty ending is learned, and the guard `fix_pr::start`
        // has to pass — no live session on the branch — is only true once it has.
        let mut hand_off_for: Option<u64> = None;
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
                        if command == "resolve-run" {
                            resolve_run_for = Some(*pr);
                        }
                        // Only a review that said so. Every other way one ends — you
                        // closed it, it fell over reading, you killed it mid-cards —
                        // leaves the flag false and hands on nothing, which is the
                        // whole difference between this and a run that trips behind
                        // you.
                        if s.fix_pr_on_exit {
                            hand_off_for = Some(*pr);
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
        // The run's own account, closed. Only if it is still this session's run: a
        // second run on the same PR replaces the record, and stamping that one as
        // ended would bury a live run under the exit of the one it replaced.
        if let Some(pr) = resolve_run_for {
            let mut inner = app.inner.write().await;
            inner.with_resolve_runs("session exited", |runs| match runs.get_mut(&pr) {
                Some(r) if r.session == id && r.ended.is_none() => {
                    r.ended = Some("the session ended".into());
                    true
                }
                _ => false,
            });
        }
        app.release_main(id).await;
        // The branch is free now, which is the only reason this waited for the exit.
        // A refusal is not raised, because by here nobody is waiting: the guard
        // table's reasons are written for whoever asked, and the rail's own `fix`
        // button is still there to be pressed and will say the same thing.
        //
        // But it is *taken back*. `handed_off` is what tells the overlay to hold its
        // report and wait for a run, so a refusal that left the flag standing would
        // strand the review on "applying" for good — which is the fault this whole
        // hand-off was built to fix, rebuilt one branch over.
        if let Some(pr) = hand_off_for {
            match crate::fix_pr::start(&app, pr).await {
                Ok(session) => {
                    tracing::info!(pr, %session, "review handed the checks to a fix-pr run")
                }
                Err(e) => {
                    tracing::warn!(pr, "review's hand-off to fix-pr refused: {e}");
                    if let Some(s) = app.inner.write().await.sessions.get_mut(&id) {
                        s.fix_pr_on_exit = false;
                    }
                }
            }
        }
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
    let path = app
        .workspace_path(workspace)
        .await
        .with_context(|| format!("unknown workspace {workspace}"))?;
    let spawned = PtyHandle::spawn(&[shell], &path, &[], &[], DEFAULT_SIZE)?;
    let proc_id = format!("{workspace}:shell:{}", Uuid::new_v4().simple());

    let process = Process {
        id: proc_id.clone(),
        name: "shell".to_string(),
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
            /* The child exiting, not merely the end of its output. The `Process`
               record holds the pty handle, so the broadcast sender outlives the
               child and `rx.recv()` never errors — which meant this loop never
               reached the `Dead` below it, and a managed process that had exited
               sat at `Starting` with a live-looking dot in the drawer. Measured:
               a spec that exits immediately still read `starting, alive: false,
               exit 1` a minute later.

               `wait()` clones its own receiver off a watch channel, so it is safe
               to poll here and there is still one observer of this pty. */
            let chunk = tokio::select! {
                r = rx.recv() => match r {
                    Ok(c) => c,
                    // A lagged consumer only misses health lines, and the next
                    // build will restate them; resubscribing beats tearing down.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },
                _ = handle.wait() => break,
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

    /// A per-PR run rebases onto the PR's own base, not the daemon's global one.
    ///
    /// A normal PR based on the configured branch renders exactly `upstream_ref`,
    /// so nothing changes for the ordinary case. A PR on a release branch renders
    /// that branch on the upstream remote instead of the wrong ancestor. A PR the
    /// poll does not carry falls back to the configured base rather than an empty
    /// or malformed ref.
    #[test]
    fn a_run_rebases_onto_the_prs_own_base_not_the_global_one() {
        // The ordinary case: same base as configured, so the ref is unchanged.
        assert_eq!(
            rebase_target("upstream/develop", "upstream", Some("develop")),
            "upstream/develop"
        );
        // A release-branch PR: the run must target that branch, not develop.
        assert_eq!(
            rebase_target("upstream/develop", "upstream", Some("release/2.0")),
            "upstream/release/2.0"
        );
        // Not in the poll, or GitHub named no base: fall back to the configured ref.
        assert_eq!(rebase_target("origin/main", "origin", None), "origin/main");
        assert_eq!(rebase_target("origin/main", "origin", Some("")), "origin/main");
    }

    /// All three variables or none, and this is the seam that decides it.
    ///
    /// `spawn_worktree_session` built its environment by hand and pushed only
    /// `ORCH_SESSION_ID`, so every session started from the new-worktree button had
    /// an `orch` that could not reach the daemon and, on an AppImage or a `.app`,
    /// was not even on `PATH`. The failure was silent in the worst way: the CLI read
    /// the subset as "you are not in a session" and said so.
    #[tokio::test]
    async fn a_session_is_handed_the_whole_orch_environment_or_it_cannot_ask() {
        let dir = std::env::temp_dir().join(format!("orchd-agent-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // `env_source: none`: this pins what the *daemon* puts in, and a real
        // source would add whatever the machine running the test exports.
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7794,"env_source":"none"}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let id = Uuid::new_v4();
        let (env, _) = crate::config::session_env(&app.cfg, &dir, id, Some("ask-tok"));
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{k} is not in the environment"))
        };
        assert_eq!(get("ORCH_SESSION_ID"), id.to_string());
        assert_eq!(get("ORCH_ASK_TOKEN"), "ask-tok");
        // The port the daemon is actually on, or the agent's curl reaches nothing.
        assert_eq!(get("ORCH_URL"), "http://127.0.0.1:7794");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A managed process that exits has to *say* so.
    ///
    /// The health watcher used to end its loop only when the output channel closed
    /// — and the `Process` record holds the pty handle, so the sender outlives the
    /// child and that never happened. A process that had exited sat at `Starting`
    /// forever, which the drawer draws as a live tab. In-tree with a real pty
    /// rather than in `e2e`: nothing here needs a worktree, a branch or the agent,
    /// only a child that ends.
    #[tokio::test]
    async fn a_managed_process_that_exits_is_reported_dead() {
        let dir = std::env::temp_dir().join(format!("orchd-managed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7796}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let spec = ManagedSpec {
            name: "quick".into(),
            // POSIX, and it ends on its own without printing a health line — so
            // only the exit can move it off `Starting`.
            command: vec!["sh".into(), "-c".into(), "exit 3".into()],
            failure_patterns: Vec::new(),
            ok_patterns: Vec::new(),
            restart: crate::config::RestartPolicy::Never,
            autostart: true,
        };
        let id = start_managed(&app, MAIN, &spec).await.expect("started");

        let health = |app: Arc<AppState>, id: String| async move {
            let inner = app.inner.read().await;
            inner
                .workspaces
                .get(MAIN)
                .and_then(|w| w.processes.iter().find(|p| p.id == id))
                .map(|p| p.health.clone())
        };
        // A second is generous for `sh -c 'exit 3'`; before the fix this never
        // arrived, however long you waited.
        for _ in 0..100 {
            if health(app.clone(), id.clone()).await == Some(Health::Dead) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "still {:?} after a second — the exit went unobserved",
            health(app.clone(), id.clone()).await
        );
    }

    /// Main holding the PR's own branch, dirty, is got out of rather than bailed on.
    ///
    /// The state a session in main leaves behind: `park_main` will not carry your
    /// work to another branch, so main stands on the feature branch, and cutting
    /// the worktree every PR flow needs is impossible while it does. It used to
    /// refuse and tell you to stash. Now the branch and its work move into the very
    /// tree the flow was about to cut.
    #[tokio::test]
    async fn a_pr_worktree_gets_cut_by_moving_mains_branch_into_it() {
        let dir = std::env::temp_dir().join(format!("orchd-prcut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let main = dir.join("repo");
        let run = |at: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(at)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&dir, &["init", "-q", "-b", "develop", "repo"]);
        run(&main, &["config", "user.email", "t@t"]);
        run(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join("f.txt"), "base\n").unwrap();
        run(&main, &["add", "-A"]);
        run(&main, &["commit", "-qm", "base"]);
        // Main on the PR's branch, with a change nobody committed — the exact state.
        run(&main, &["switch", "-qc", "feature/theirs"]);
        std::fs::write(main.join("f.txt"), "edited in main\n").unwrap();

        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7795,"upstream_ref":"origin/develop"}}"#,
            main.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let ws = ensure_pr_worktree(&app, 4242, "feature/theirs")
            .await
            .expect("the flow has to get itself out of this");
        assert_eq!(ws, "pr-4242");

        let tree = app.cfg.worktree_path("pr-4242");
        assert_eq!(crate::git::current_branch(&tree).unwrap(), "feature/theirs");
        assert_eq!(crate::git::current_branch(&main).unwrap(), "develop");
        assert_eq!(
            std::fs::read_to_string(tree.join("f.txt")).unwrap(),
            "edited in main\n",
            "the uncommitted work did not travel with its branch"
        );
        // And the daemon agrees about who holds it, or the next flow looks in main.
        assert_eq!(worktree_holding(&app, "feature/theirs").await.as_deref(), Some("pr-4242"));
    }

    /// The race a relocation would otherwise lose.
    ///
    /// Relocating resumes under the *same id*, so the record the old watcher wakes
    /// up to find is a **live session at the far end** — and settling it there would
    /// mark it exited and, because `release_main` keys on the id, hand main's claim
    /// straight back out from under it. Two real ptys, because the pty is the
    /// identity that tells the two apart and a mock would be asserting the fix
    /// against itself.
    /// What the daemon will and will not adopt from a `WorktreeCreate` hook.
    ///
    /// Every rejection here falls back to the daemon cutting its own tree, so being
    /// strict costs nothing and being lax means adopting a directory chosen by a
    /// script that answered the wrong question.
    #[test]
    fn only_a_real_absolute_dot_free_directory_is_adopted_from_a_hook() {
        let dir = std::env::temp_dir().join(format!("orch-hookpath-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let shown = dir.to_string_lossy().into_owned();

        // The contract: only the path on stdout, and a trailing newline is normal.
        assert_eq!(usable_hook_path(&format!("{shown}\n")), Some(dir.clone()));
        assert_eq!(usable_hook_path(&format!("  {shown}  ")), Some(dir.clone()));

        // Nothing said at all, which is a hook that declined.
        assert_eq!(usable_hook_path(""), None);
        assert_eq!(usable_hook_path("   \n"), None);
        // Relative: ambiguous against a cwd the hook does not control.
        assert_eq!(usable_hook_path("wt/name"), None);
        // Dot segments: Claude Code refuses these rather than normalising, because
        // they can climb out of the repository.
        assert_eq!(usable_hook_path(&format!("{shown}/../elsewhere")), None);
        assert_eq!(usable_hook_path(&format!("{shown}/./here")), None);
        // Absolute, dot-free, and simply not there: a hook that failed while
        // exiting zero.
        assert_eq!(usable_hook_path("/nonexistent/orchd/worktree"), None);
        // A file rather than a directory, same reason.
        let f = dir.join("afile");
        std::fs::write(&f, "x").unwrap();
        assert_eq!(usable_hook_path(&f.to_string_lossy()), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A workspace record outlives the directory it names, and the PR flows key on
    /// that record. The run this cost opened in `$HOME`: the tree had been removed,
    /// `worktree_holding` handed its name back anyway, `ensure_pr_worktree` took the
    /// early return, and the spawn was aimed at a path that was not there.
    #[tokio::test]
    async fn a_worktree_whose_directory_is_gone_does_not_hold_a_branch() {
        use crate::config::Config;
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-holding-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7794}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let here = dir.join("here");
        std::fs::create_dir_all(&here).unwrap();
        let gone = dir.join("gone");
        let _ = std::fs::remove_dir_all(&gone);

        app.register_worktree("here", here, Some("feature/here".into())).await;
        app.register_worktree("gone", gone, Some("feature/gone".into())).await;

        let (name, path) = recorded_worktree_for(&app, "feature/here").await.expect("standing");
        assert_eq!((name.as_str(), path.is_dir()), ("here", true));

        // The one that matters: the record survives its directory, and it comes back
        // with the path so the flow rebuilds the tree where it stood rather than
        // cutting a second one for the same branch.
        let (name, path) = recorded_worktree_for(&app, "feature/gone").await.expect("recorded");
        assert_eq!(name, "gone");
        assert!(!path.is_dir(), "the tree is gone; the record is not");
        assert!(path.ends_with("gone"), "and it names where it stood: {}", path.display());

        // And it is still where that branch is being worked on, which is a different
        // question and the one the busy guard asks.
        assert_eq!(
            worktree_holding(&app, "feature/gone").await.as_deref(),
            Some("gone"),
            "a deleted tree does not free the branch for a second agent"
        );
    }

    /// The record has to be visible to a hook before the process that fires it
    /// exists. Asserted on the helper rather than on a spawn, because the thing
    /// that made this a bug — a real agent booting faster than the insert — is
    /// exactly what a test cannot reproduce.
    #[tokio::test]
    async fn the_record_is_in_the_map_before_the_process_starts() {
        use crate::config::Config;
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-insert-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7792}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let id = Uuid::new_v4();
        let session = Session::new(id, MAIN.to_string(), dir.clone(), Kind::Interactive);
        let cmd = ["cat".to_string()];
        let spawned = insert_and_spawn(&app, id, session, &cmd, &dir, &[], &[])
            .await
            .expect("spawn cat");

        let inner = app.inner.read().await;
        let s = inner.sessions.get(&id).expect("the record is there");
        assert!(s.pty.is_some(), "the handle is hung on the record afterwards");
        assert_eq!(s.pid, spawned.pid);
        drop(inner);
        let _ = spawned.handle.kill();
    }

    /// And a spawn that never happened leaves nothing behind: the record would
    /// otherwise sit in the rail as a session with no process, holding its
    /// workspace against the next attempt.
    #[tokio::test]
    async fn a_refused_spawn_takes_its_record_back_out() {
        use crate::config::Config;
        use crate::state::AppState;

        let dir = std::env::temp_dir().join(format!("orchd-refused-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":"{}","port":7793}}"#,
            dir.display()
        ))
        .unwrap();
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let id = Uuid::new_v4();
        let session = Session::new(id, MAIN.to_string(), dir.clone(), Kind::Interactive);
        let cmd = ["orchd-no-such-binary-ever".to_string()];
        let err = insert_and_spawn(&app, id, session, &cmd, &dir, &[], &[]).await;

        assert!(err.is_err(), "a missing binary is a failed spawn");
        assert!(
            app.inner.read().await.sessions.get(&id).is_none(),
            "the record must not outlive the attempt"
        );
    }

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

    /// Reviewing a PR whose worktree you tore down must not be refused on the name.
    ///
    /// Teardown writes a recovery record naming `pr-<n>`, and the old check refused
    /// on exactly that — for good, since "rename" is impossible for a name derived
    /// from the PR number, leaving deleting the conversation as the only way out.
    /// `fix-pr` and `triage` never had the check, so one PR answered two ways.
    ///
    /// Asserted as "not *this* refusal" rather than success: reaching a real spawn
    /// would need a repo, a branch and `claude`. The failure here is the missing
    /// branch, which is the next thing the path legitimately trips on.
    #[tokio::test]
    async fn reviewing_a_pr_is_not_refused_because_its_worktree_was_torn_down() {
        let dir = std::env::temp_dir().join(format!("orchd-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = crate::config::Config::parse(&format!(
            r#"{{"main_checkout":{:?}}}"#,
            dir.to_string_lossy()
        ))
        .expect("parse");
        let app = crate::state::AppState::new(cfg, "t".into(), crate::window::Chrome::None);
        {
            let mut inner = app.inner.write().await;
            let id = uuid::Uuid::new_v4();
            let mut s = Session::new(id, "pr-4".into(), dir.join("pr-4"), Kind::Interactive);
            s.had_a_turn = true;
            s.set_state(State::Archived { resumable: true });
            // Exactly what `worktree::archive` writes when a pr-4 tree is torn down.
            s.recovery = Some(ArchiveState::Recoverable {
                name: "pr-4".into(),
                branch: "feature/x".into(),
                head_sha: "abc1234".into(),
            });
            inner.sessions.insert(id, s);
        }
        let err = format!(
            "{:#}",
            spawn_command_session(&app, 4, "feature/x", "resolve")
                .await
                .expect_err("no repo here, so it cannot get as far as a session")
        );
        assert!(
            !err.contains("already used") && !err.contains("interleave"),
            "refused on the reused name again: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fix-pr guard used to ask `is_busy()` while the spawn enforces `is_live`,
    /// so a session sitting idle on the branch passed a guard whose refusal reads
    /// "live session" and was only stopped later, once the run looked started. Both
    /// now read this.
    #[tokio::test]
    async fn an_idle_session_on_the_branch_still_makes_it_busy() {
        let dir = std::env::temp_dir().join(format!("orchd-busy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = crate::config::Config::parse(&format!(
            r#"{{"main_checkout":{:?}}}"#,
            dir.to_string_lossy()
        ))
        .expect("parse");
        let app = crate::state::AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let id = uuid::Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            inner.workspaces.insert(
                "pr-4".into(),
                crate::model::Workspace {
                    id: "pr-4".into(),
                    path: dir.join("pr-4"),
                    kind: crate::model::WorkspaceKind::Worktree { name: "pr-4".into() },
                    branches: ["feature".to_string()].into_iter().collect(),
                    processes: Vec::new(),
                    occupant: None,
                    tree: Default::default(),
                },
            );
            let mut s = Session::new(id, "pr-4".into(), dir.join("pr-4"), Kind::Interactive);
            // Idle, not mid-turn — the case the two rules disagreed on. Its pid is
            // this test process, because `live_sessions_in` also wants it alive.
            s.set_state(State::YourTurn {
                since: std::time::SystemTime::now(),
                reason: TurnReason::TurnComplete,
            });
            s.pid = Some(std::process::id());
            inner.sessions.insert(id, s);
        }
        assert_eq!(branch_busy(&app, "feature").await.as_deref(), Some("pr-4"));

        // Archived is the other side of the same rule: nothing is running there.
        {
            let mut inner = app.inner.write().await;
            inner
                .sessions
                .get_mut(&id)
                .unwrap()
                .set_state(State::Archived { resumable: true });
        }
        assert_eq!(branch_busy(&app, "feature").await, None);
        // A branch no worktree holds is never busy.
        assert_eq!(branch_busy(&app, "other").await, None);

        let _ = std::fs::remove_dir_all(&dir);
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

/// What the daemon needs the new worktree to have checked out.
///
/// The repo's `WorktreeCreate` hook decides its own branch and base, so whatever it
/// produces has to be put onto this afterwards. Naming the two shapes rather than
/// passing a branch and a nullable base keeps the "is this a new branch or an
/// existing one" question answered once, at the call site that knows.
pub(crate) enum Want<'a> {
    /// A branch to cut, from this base. A plain new worktree, or a fork from its
    /// parent's HEAD.
    New { branch: &'a str, base: &'a str },
    /// A branch that already exists somewhere, local or on origin. A PR's head ref.
    Existing { branch: &'a str },
}

/// Create a worktree the repo's way if it has one, ours otherwise.
///
/// **The repo's `WorktreeCreate` hook first, the daemon's own creation behind it.**
/// A repo that declares the event is stating how worktrees are made here, and
/// Claude Code treats the hook as the whole mechanism: it reads the request on
/// stdin and prints the path it made. So the daemon runs it and adopts what it
/// produced, which is how a repo's fetching, its layout and its post-create work
/// reach a tree the daemon asked for.
///
/// **Then the branch is put right.** The hook chooses its own base, and the
/// monorepo's hardcodes `upstream/develop`. That is wrong for a PR worktree pinned
/// to a head ref and wrong for a fork cut from its parent, so [`Want`] is applied to
/// the tree afterwards. The branch the hook made is left behind; that is the price
/// of letting it own creation, and it is a stale ref rather than lost work.
///
/// **Every failure falls through to the daemon's own creation**, which is the path
/// that ran before any of this existed. Creating a worktree is the daemon's most
/// load-bearing operation and a repo script is not allowed to be the reason it
/// cannot happen. A tree the hook made but we could not use is *removed* first: it
/// usually sits at the very path and branch the fallback is about to ask for, so
/// leaving it there turned an adoption failure into a hard spawn failure with a git
/// error about a branch nobody asked about.
///
/// Returns the path the worktree actually landed at, which is the hook's choice when
/// the hook made it. Callers must use that rather than the path they passed in.
pub(crate) async fn create_worktree(
    app: &Arc<AppState>,
    name: &str,
    path: &std::path::Path,
    want: Want<'_>,
) -> Result<std::path::PathBuf> {
    // Owned up front: every git call below goes to a blocking thread, because each
    // one can fetch and a fetch against an unreachable remote parks a runtime worker
    // for as long as git waits.
    let main = app.cfg.main_checkout.clone();
    let branch = match &want {
        Want::New { branch, .. } | Want::Existing { branch } => branch.to_string(),
    };
    let base = match &want {
        Want::New { base, .. } => Some(base.to_string()),
        Want::Existing { .. } => None,
    };

    if let Some(made) = hook_cut_worktree(app, name).await {
        let (m, tree, b, ba) = (main.clone(), made.clone(), branch.clone(), base.clone());
        let applied = tokio::task::spawn_blocking(move || match ba {
            Some(base) => crate::git::checkout_new_branch(&m, &tree, &b, &base),
            None => crate::git::checkout_existing_branch(&m, &tree, &b),
        })
        .await
        .context("putting the hook's worktree on its branch panicked")?;

        match applied {
            Ok(()) => return Ok(made),
            Err(e) => {
                tracing::warn!(
                    name,
                    "the repo's WorktreeCreate made {} but it could not be put on the \
                     branch this needs, so the daemon is cutting its own: {e:#}",
                    made.display()
                );
                // Out of the way before the fallback asks for the same path, and very
                // likely the same branch name.
                let (m, t) = (main.clone(), made.clone());
                let _ = tokio::task::spawn_blocking(move || crate::git::worktree_remove(&m, &t)).await;
            }
        }
    }

    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || match base {
        Some(base) => crate::git::worktree_add_new(&main, &p, &branch, &base),
        None => crate::git::worktree_add_existing(&main, &p, &branch),
    })
    .await
    .context("the worktree add panicked")??;
    Ok(path.to_path_buf())
}

/// Run the repo's `WorktreeCreate` hooks and return the tree the first usable one
/// made.
///
/// `None` for every way this can decline: no hook declared, a non-zero exit, empty
/// output, or a path that is not a directory. The caller cuts its own on `None`, so
/// none of these is an error worth raising.
async fn hook_cut_worktree(app: &Arc<AppState>, name: &str) -> Option<std::path::PathBuf> {
    // `name` is the key the monorepo's hook reads, and the only one the contract is
    // observed to carry.
    let payload = serde_json::json!({
        "hook_event_name": "WorktreeCreate",
        "cwd": app.cfg.main_checkout.to_string_lossy(),
        "name": name,
    });
    for out in crate::worktree::run_repo_hooks(app, "WorktreeCreate", payload).await {
        let said = String::from_utf8_lossy(&out.stdout);
        match usable_hook_path(&said) {
            Some(made) => {
                tracing::info!(name, "the repo's WorktreeCreate hook made {}", made.display());
                return Some(made);
            }
            None => tracing::warn!(
                name,
                "the repo's WorktreeCreate hook emitted an unusable path {:?}",
                said.trim()
            ),
        }
    }
    None
}

/// The worktree path a `WorktreeCreate` hook printed, if it is one the daemon can
/// use.
///
/// Checked the way Claude Code checks it, and for its reasons: a relative path is
/// ambiguous against a working directory the hook does not control; dot segments can
/// climb out of the repository, and Claude Code refuses them outright rather than
/// normalising, so a hook that emits them is one written against a different
/// contract; and a path that is not a directory is a hook that failed while exiting
/// zero. The hook's contract is that only the path goes to stdout, so trailing
/// newlines are trimmed and nothing else is parsed out of it.
fn usable_hook_path(said: &str) -> Option<std::path::PathBuf> {
    let said = said.trim();
    if said.is_empty() {
        return None;
    }
    let made = std::path::PathBuf::from(said);
    if !made.is_absolute() {
        return None;
    }
    if made.components().any(|c| {
        matches!(
            c,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return None;
    }
    made.is_dir().then_some(made)
}

