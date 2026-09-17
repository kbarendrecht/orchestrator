use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

use crate::model::*;
use crate::pty::{PtyHandle, DEFAULT_SIZE};
use crate::state::AppState;

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
async fn spawn_session_confirmed(
    app: &Arc<AppState>,
    workspace: &str,
    pass: Option<Pass>,
    resume: Option<Source>,
    grace: std::time::Duration,
) -> Result<SessionId> {
    let id = spawn_session(app, workspace, pass, resume).await?;
    let handle = {
        let inner = app.inner.read().await;
        inner.sessions.get(&id).and_then(|s| s.pty.clone())
    };
    let Some(handle) = handle else {
        bail!(
            "session {} was gone before it could be confirmed",
            crate::model::short_id(&id)
        );
    };
    if let Ok(code) = tokio::time::timeout(grace, handle.wait()).await {
        bail!(
            "session {} exited immediately, code {code}",
            crate::model::short_id(&id)
        );
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
    let (src_cwd, src_workspace, handle, title, name, created_at, pass) = {
        let inner = app.inner.read().await;
        let s = inner
            .sessions
            .get(&id)
            .with_context(|| format!("unknown session {}", crate::model::short_id(&id)))?;
        (
            s.cwd.clone(),
            s.workspace.clone(),
            s.pty.clone(),
            s.title.clone(),
            s.name.clone(),
            s.created_at,
            s.pass.clone(),
        )
    };

    // The pty holds the transcript open, and it is the process that decides when
    // the last turn is flushed. Waiting for it to actually be gone is what makes
    // the move below a move of a file nobody is writing to.
    if let Some(h) = handle {
        // Bounded, and it escalates. This was `kill(); wait().await`, which against
        // an agent that traps `SIGHUP` — Node does — never returned: the swap held
        // its own HTTP request open forever.
        h.kill_gracefully().await;
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

    // Its recorded pass, not `None`: relocating must not quietly turn a run into a
    // session the guard table counts differently.
    let resumed = spawn_session_confirmed(
        app,
        dest_workspace,
        pass.clone(),
        Some(Source::Resume(id)),
        grace,
    )
    .await;

    match resumed {
        Ok(id) => {
            restore_after_relocate(app, id, title, name, created_at).await;
            Ok(Relocated {
                id,
                degraded: false,
            })
        }
        Err(e) => {
            tracing::warn!(session = %id, "the resume in {dest_workspace} did not stay up, forking instead: {e:#}");
            let forked =
                spawn_session_confirmed(app, dest_workspace, pass, Some(Source::Fork(id)), grace)
                    .await
                    .with_context(|| {
                        format!(
                            "neither resuming nor forking {} into {dest_workspace} stayed up; \
                     it is closed but its conversation is intact and resumable",
                            crate::model::short_id(&id)
                        )
                    })?;
            restore_after_relocate(app, forked, title, name, created_at).await;
            Ok(Relocated {
                id: forked,
                degraded: true,
            })
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
///
/// **The headroom check lives here because this is the only thing every spawned
/// agent goes through.** It was written at two of the four callers and worded
/// differently at each, so the rail's new-worktree button, the fork path and the
/// story filer started an agent on a box with no room while a run and an
/// interactive session were refused. Managed processes and drawer shells go
/// straight to [`PtyHandle::spawn`] and are deliberately not covered: a shell is
/// not what exhausts a machine.
pub(crate) async fn insert_and_spawn(
    app: &Arc<AppState>,
    id: SessionId,
    session: Session,
    cmd: &[String],
    cwd: &std::path::Path,
    env: &[(String, String)],
    unset: &[&str],
) -> Result<crate::pty::Spawned> {
    crate::headroom::check().map_err(|why| anyhow::anyhow!("not starting a session: {why}"))?;
    {
        let mut inner = app.inner.write().await;
        /* The bar reports the *last* attempt, the way `pr_error` reports the last
        poll, so a new one clears it here rather than at each of the ten sites that
        set `had_a_turn` — nine of which would have been the site somebody forgot.
        A press that fails again puts it straight back, ~50ms later. */
        inner.agent_error = None;
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

/// What a resume or a fork carries over from the record it continues.
#[derive(Default)]
struct Carried {
    /// A resume comes back at an empty prompt, so whether the turn behind it was
    /// interrupted is the one thing it cannot re-derive. A fork starts a fresh
    /// direction, so it is never interrupted.
    interrupted: bool,
    /// Carries across both: a resumed conversation already had its turns, and a
    /// fork replays the parent's — so both open on a real conversation, not an
    /// empty pane, and reading it fresh would say otherwise until the next
    /// `Working`.
    had_a_turn: bool,
    /// Carried first, read second. A resume or a fork continues a conversation
    /// that already knows what it is about, and the tree it comes back to may have
    /// been swapped since, so asking git would quietly rewrite the conversation's
    /// own history to match whatever is checked out now, which is the mismatch the
    /// field exists to catch. Only a genuinely new session asks the tree.
    branch: Option<String>,
    /// The undelivered arrival notice rides along for the same reason: a session
    /// moved while it was not running is told when auto-resume brings it back, and
    /// that resume is exactly this path.
    notice: Option<String>,
    /// A name you typed survives a resume. The record is rebuilt under the same
    /// id, so unless it is carried it reverts to the workspace default — which is
    /// exactly what auto-resume on a restart did, since it resumes through this
    /// path and never through `restore_after_relocate`. The title needs no carry:
    /// it is re-read from the transcript. Resume only, not fork: a fork is a new
    /// conversation and earns its own name (the degraded-relocate fork is the one
    /// exception, and it gets the name back through `restore_after_relocate`).
    name: Option<String>,
    /// Who spawned this session, and whether that spawn cut the worktree.
    ///
    /// **The one fact on the record with no other home.** Everything else a resume
    /// rebuilds either persists and is restored, or heals itself from disk — a
    /// title from the transcript, a branch from the tree. This pair is a *link*
    /// between two conversations, and nothing outside the record knows it.
    ///
    /// `api::discard_spawned` is the reader: it refuses to let a session discard
    /// one it did not spawn. Lost, an agent that restarts can no longer undo the
    /// child it created, and the refusal says the opposite of what happened.
    /// `SessionRecord` persists both and `restore` puts them back at boot;
    /// auto-resume discarded them a moment later, exactly as it did
    /// [`Self::created_at`]. Carried on a fork too: a fork of a spawned session is
    /// still that parent's to undo.
    spawned_by: Option<SessionId>,
    spawn_cut_worktree: bool,
    /// That this conversation began as a fork of another.
    ///
    /// The rail draws a badge from it and `store` persists it, so a restart kept
    /// it and the respawn right after threw it away — a forked row quietly stopped
    /// reading as forked. Set directly for a *new* fork; carried here for a resume
    /// of one that already was.
    forked_from: Option<SessionId>,
    /// When this conversation actually began, which a resume otherwise loses.
    ///
    /// `Session::new` stamps it with *now*, and a resume rebuilds the record under
    /// the same id — so an hours-old conversation came back claiming to have
    /// started this second. That is not bookkeeping: `claim_stale_warning` refuses
    /// to warn about a file rewritten *before* the session started, on the grounds
    /// that a session which never saw the old contents cannot be working from
    /// them, and after a restart every session says it started just now. So the
    /// one session that needs the warning is the one that stops getting it.
    /// `SessionRecord` persists this and `restore` puts it back at boot; auto-resume
    /// discarded it a moment later. `None` for a fresh session, which really did
    /// start now.
    created_at: Option<std::time::SystemTime>,
}

impl Carried {
    fn from(prev: Option<&Session>, fork: bool) -> Self {
        let Some(prev) = prev else {
            return Self::default();
        };
        Carried {
            interrupted: !fork && prev.interrupted,
            had_a_turn: prev.had_a_turn,
            branch: prev.branch.clone(),
            notice: prev.arrival_notice.clone(),
            name: if fork { None } else { prev.name.clone() },
            spawned_by: prev.spawned_by,
            spawn_cut_worktree: prev.spawn_cut_worktree,
            // A *new* fork is not itself forked from anything the parent records —
            // `spawn_session` sets that from the `Source`. This carries the fact
            // for a resume of a session that was already a fork.
            forked_from: if fork { None } else { prev.forked_from },
            // Resume only. A fork is a new conversation and started when it was
            // forked, so the files you rewrote before it existed are news to it.
            created_at: (!fork).then_some(prev.created_at),
        }
    }

    /// Put what travelled onto the record that continues the conversation.
    ///
    /// **The other half of [`Carried::from`], and the reason it is a method.** A
    /// field named there and not here is carried and then thrown away a moment
    /// later, and the only sign is a behaviour that quietly stops working after a
    /// restart — `created_at`, `spawned_by`, `spawn_cut_worktree` and `forked_from`
    /// were each lost exactly that way, one at a time, over four separate fixes.
    /// The read and the write were a destructuring of nine names and ten
    /// assignments thirty lines apart; now adding a field to one side fails to
    /// compile until it is on the other.
    ///
    /// The branch is **returned rather than set**, because it is the one carried
    /// value with a fallback: only a genuinely new session asks git, and that
    /// question is a `spawn_blocking` the caller owns. Returned rather than left on
    /// the struct so it cannot be the field somebody forgets.
    fn apply(self, session: &mut Session) -> Option<String> {
        session.interrupted = self.interrupted;
        session.had_a_turn = self.had_a_turn;
        session.arrival_notice = self.notice;
        session.name = self.name;
        if let Some(began) = self.created_at {
            session.created_at = began;
        }
        session.spawned_by = self.spawned_by;
        session.spawn_cut_worktree = self.spawn_cut_worktree;
        session.forked_from = self.forked_from;
        self.branch
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
    pass: Option<Pass>,
    resume: Option<Source>,
) -> Result<SessionId> {
    let id = match resume {
        Some(Source::Resume(prev)) => prev,
        // A fork is a second conversation, so it needs an id of its own.
        Some(Source::Fork(_)) | None => Uuid::new_v4(),
    };
    // Main is exclusive, and the claim is taken before anything is created so a
    // refusal costs no worktree and no pty. The claim is *given back* here rather
    // than in the body, because everything below this line is fallible — the env
    // source, the transcript read, `claude` itself — and a claim left behind by a
    // spawn that never happened names a session that does not exist. Every reader
    // live-filters the occupant and so recovers, which is why this was invisible;
    // it is closed at the one place that knows the spawn failed.
    if workspace == MAIN {
        app.claim_main(id).await?;
    }
    let out = spawn_session_with_id(app, workspace, pass, resume, id).await;
    if out.is_err() && workspace == MAIN {
        app.release_main(id).await;
    }
    out
}

/// [`spawn_session`], with main's claim already taken and given back for it.
async fn spawn_session_with_id(
    app: &Arc<AppState>,
    workspace: &str,
    pass: Option<Pass>,
    resume: Option<Source>,
    id: SessionId,
) -> Result<SessionId> {
    // The centre pane is empty until this returns, so this is the number people
    // mean by "the terminal takes ages to appear". Three of the phases below are
    // somebody else's program: the env source, the transcript on disk, and
    // `claude` itself.
    let mut phases = crate::timing::Phases::start();

    let path = app
        .workspace_path(workspace)
        .await
        .with_context(|| format!("unknown workspace {workspace}"))?;

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
    cmd.extend(crate::launch::session_flags()?);

    phases.mark("claim");

    // What the previous record hands the session that continues it, read before
    // the insert below replaces it: a resume keeps the id, so the record of what
    // the conversation was doing is about to be overwritten. One read for the lot;
    // these used to be four separate lock acquisitions on the same record.
    let carried = match resume {
        Some(Source::Resume(prev)) | Some(Source::Fork(prev)) => {
            let fork = matches!(resume, Some(Source::Fork(_)));
            let inner = app.inner.read().await;
            Carried::from(inner.sessions.get(&prev), fork)
        }
        None => Carried::default(),
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
    // started, so nothing else is appending to that file.
    //
    // **Any pin, not only one that disagrees.** It used to be left alone when it
    // matched the cwd, on the reading that such a session is correctly isolated.
    // That reading is gone with the delegated arm (`spawn_worktree_session`): the
    // daemon does not want Claude Code's isolation at all now, because it refuses
    // writes as well as git — an agreeing pin is what made a scratch dir shared
    // into the tree unwritable from either side of its symlink — and the isolation
    // the daemon does want is [`crate::guard::isolation`], on the agent's Bash.
    // Sessions cut by the old arm carry a pin that agrees, so this is the only
    // thing that ever releases them.
    // Off the runtime, all of it: finding the transcript walks
    // `~/.claude/projects`, and reading the pin reads the whole file — which the
    // note below says is megabytes of turns. One hop for the lot, since the pin is
    // the only reason the file is opened.
    if resume.is_some() {
        let (at, id_) = (path.clone(), id);
        let cleared = crate::proc::run_blocking("clearing the worktree pin", move || {
            // The id scan as a fallback, because the case that breaks the cheap slug
            // lookup is this one: a relocation whose `move_transcript` failed leaves
            // the file under the old tree's slug, and that is a conversation that is
            // *more* likely to be carrying a stale pin, not less.
            let t = crate::store::transcript_file(id_, &at, None)
                .or_else(|| crate::store::find_transcript(id_))?;
            let pin = crate::store::worktree_pin(&t)?;
            Some((pin.clone(), crate::store::clear_worktree_pin(id_, &at, &t)))
        })
        .await
        .unwrap_or(None);
        match cleared {
            Some((pin, Ok(()))) => tracing::info!(
                session = %id,
                "cleared the worktree pin on {}; the session runs in {}",
                pin.display(),
                path.display()
            ),
            // Not fatal: the arrival notice still says it in words, and a
            // session that comes back isolated is what happened before this.
            Some((_, Err(e))) => {
                tracing::warn!(session = %id, "could not clear the worktree pin: {e:#}")
            }
            None => {}
        }
    }

    // The whole transcript is read to find the last `worktree-state` record, and
    // a transcript is megabytes of turns.
    phases.mark("transcript");

    let mut session = Session::new(id, workspace.to_string(), path.clone(), pass);
    let carried_branch = carried.apply(&mut session);
    // Off the runtime like every other git call on this path; only a new session
    // asks (see `Carried::branch`).
    session.branch = match carried_branch {
        Some(b) => Some(b),
        None => {
            let at = path.clone();
            crate::proc::run_blocking("reading the session's branch", move || {
                crate::git::current_branch(&at).ok()
            })
            .await
            .unwrap_or(None)
        }
    };
    if let Some(Source::Fork(prev)) = resume {
        session.forked_from = Some(prev);
    }

    // A resume rebuilds the environment from nothing, so the run's own credential
    // has to be re-handed here as well as at a fresh spawn: through the same seam,
    // because this is the path that forgot it once. See [`post_token_for`].
    let post = post_token_for(app, &session.pass).await;
    // Off the runtime. `session_env` asks the checkout's own env source, which is a
    // bounded child process (`mise env`, `direnv export`) that `run_bounded` polls
    // with `thread::sleep` for up to five seconds — on *every* spawn. Parked on a
    // tokio worker that is the whole board freezing while a session starts.
    let (env, unset) = {
        let (app, at, tok) = (app.clone(), path.clone(), session.ask_token.clone());
        let post = post.clone();
        crate::proc::run_blocking("reading the session environment", move || {
            run_env(&app.cfg, &at, id, Some(&tok), post.as_deref(), &[])
        })
        .await?
    };
    // Read per spawn on purpose (`env_source`), and it is a bounded run of mise
    // or direnv in a fresh worktree, so it is a cold one.
    phases.mark("env");
    let spawned = insert_and_spawn(app, id, session, &cmd, &path, &env, &unset).await?;
    phases.mark("pty");
    // The claim belongs to the record, so it is settled once the record is in.
    // Until this insert the map still described whatever stood here under this id,
    // and a relocation reuses the id — `reclaim_main` has what that let the
    // outgoing session's watcher do to the incoming session's claim. After it the
    // pty guard in `watch_session_exit` turns that watcher away, so this is the
    // last moment the window is open.
    if workspace == MAIN {
        app.reclaim_main(id).await;
    }

    started(app, id, spawned.handle).await;
    phases.log(&format!(
        "session {} start in {workspace}",
        crate::model::short_id(&id)
    ));
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
            said.push(format!(
                "its uncommitted changes could NOT be carried in ({e})"
            ));
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
                if more > 0 {
                    format!(", and {more} more")
                } else {
                    String::new()
                },
            ));
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("fork could not list the parent's untracked files: {e:#}"),
    }
    (!said.is_empty()).then(|| said.join(". "))
}

/// Create a worktree and start a session in it.
///
/// **The daemon cuts the tree, wherever this repo keeps them, and starts a plain
/// session in it.** `create_worktree` runs the repo's own `WorktreeCreate` first
/// and adopts the tree it prints, so nothing the repo does at creation is
/// reimplemented or skipped — it bases on a freshly fetched upstream and
/// configures triangular push exactly as it did before. The hook is invoked with
/// **cwd = main** (`hook_cut_worktree`), which is what its no-nesting rule wants,
/// and the daemon cuts its own tree only when the repo has no such hook or its
/// tree cannot be put on the branch this needs.
///
/// **`claude --worktree` used to do the cutting at Claude Code's own layout, and
/// that is what changed.** It pins worktree isolation into the transcript, and the
/// pin refuses *writes* as well as git: the monorepo shares a `.plan` scratch dir
/// into every tree as a relative symlink, and a pinned session could write neither
/// the link (a "raw dot segment" it will not resolve) nor the shared checkout
/// behind it (isolated). Measured across 119 worktree transcripts: 49 carried the
/// pin and every one of them came from that arm, while the 70 the daemon cut
/// carried none. The isolation the daemon actually needs is narrower and is its
/// own — main's branch and occupant are what `claim_main`, `park_main`,
/// `switch_main_to_pr` and `branch_busy` read — so `guard::isolation`
/// enforces that on the agent's Bash and says nothing about writes.
///
/// One thing it gives up, and it is not the naming: [`crate::names`] generates in
/// the same shape Claude Code did, from word lists mined out of the names it had
/// already made here. What is gone is that Claude Code locked and removed its own
/// tree;
/// `worktree::teardown` owns both anyway, and `git::worktree_remove` keeps its
/// stale-lock retry for a repo that locks its own.
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

        /* Two refusals, and both are about the present: a name a live workspace
        already holds, and a directory already sitting on disk.

        §2's "worktree names must be unique over time" is deliberately **not**
        enforced. Its reason was that the projects directory is keyed by path, so
        reusing an archived name would interleave two conversations' transcripts,
        and that reason is false: a transcript is keyed by session uuid, so
        sharing a directory slug gets you two files rather than one interleaved
        one. This is the third place that same belief had been written down, and
        `ensure_pr_worktree` had already outvoted it in practice — it reuses
        `pr-<n>` for every run on a PR, which is how one of them came to hold
        five conversations.

        The real hazard of reusing a name is a resume landing in a tree that was
        cut again for something else. `worktree::branch_drift` says so on the
        resume, where the answer is known, instead of refusing a creation that is
        usually fine. */
        let inner = app.inner.read().await;
        if inner.workspaces.contains_key(name) {
            bail!("a workspace named {name} already exists");
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
    // Minted here rather than taken off the record, because the record cannot exist
    // yet: without a name the workspace is only known once `SessionStart` reports
    // the cwd, so `Session::new` happens after the pty. The token has to be in the
    // environment the pty is *given*, so it is generated first and written onto the
    // session below.
    let ask_token = crate::secret::random_token();

    // Where the daemon looks for worktrees decides who creates this one — **unless
    // this is a fork**, which the daemon always cuts itself. `claude --worktree`
    // takes a name and nothing else: it cannot be told to branch from the parent,
    // and it does not report the path until `SessionStart`, so neither half of
    // carrying the parent's files is possible through it. `create_worktree` still
    // runs the repo's `WorktreeCreate`, so the only thing a fork gives up is Claude
    // Code choosing the name.
    // The tree a fork is cut from and carries the work of. `None` when there is no
    // parent tree left — a fork is deliberately cheaper than a resume, so a
    // conversation whose worktree is long gone can still be forked; it just comes
    // back on the base branch with nothing carried, exactly as it did before.
    let parent_tree = match fork {
        Some(prev) => {
            let cwd = app
                .inner
                .read()
                .await
                .sessions
                .get(&prev)
                .map(|s| s.cwd.clone());
            cwd.filter(|p| p.exists())
        }
        None => None,
    };
    // What travelled, in words, for the arrival notice below.
    let mut carried: Option<String> = None;

    /* A name is always required now: the daemon cuts the tree, so it has to know
    the path up front. An unnamed request gets one in Claude Code's own shape
    (`crate::names`), which is what that arm was pleasant for — `wt-ca12db78`
    is a row you find by position, `federated-seeking-quasar` is one you can say.
    Checked against the two things the *named* path refuses above, because a
    generated name never reaches those checks: a live workspace holding the id,
    and a directory a torn-down one left behind. `wt-<8 hex>` stays as the
    fallback for the case that cannot happen, since a spawn that fails to invent
    a name is worse than an ugly one. */
    /* **The one number the spare pool is sold on.** Nothing in the daemon timed
    this before: `slow git` only prints a git exec that crosses 300ms on its own,
    and the dearest half of a cut is the repo's `WorktreeCreate` hook, which is a
    `sh -c` and therefore invisible to it. So "is the pool actually helping" had no
    answer in production, on a feature whose entire justification is latency.
    Logged with `claimed` or `cut` beside it, because the two arms are the
    comparison.

    **Started above the claim, not below it.** It bracketed only the cut at first,
    which made the claimed arm report `took_ms=0` — true, and worthless, because
    the claim's own four probes are the thing the warm arm actually pays. Measured
    on the monorepo they are 200-520ms, not the 47-81ms a hand-timed `git status`
    on a warm tree suggested. */
    let tree_began = std::time::Instant::now();

    /* **A pre-cut worktree, when there is one and this request can take it.**
    Only the plain case: a caller-supplied name has to be that name, and a fork is
    cut from its parent's HEAD rather than from the base every spare sits on. The
    spare keeps its own generated name, which is what makes the claim free — see
    `crate::spare` for why that is sound and what it measures first. */
    let claimed = match (name, fork) {
        (None, None) => crate::spare::claim(app).await,
        _ => None,
    };

    let owned_name = match (name, &claimed) {
        (Some(_), _) => None,
        (None, Some((spare, _))) => Some(spare.clone()),
        (None, None) => {
            let held = {
                let inner = app.inner.read().await;
                inner
                    .workspaces
                    .keys()
                    .cloned()
                    .collect::<std::collections::HashSet<_>>()
            };
            Some(
                crate::names::candidates()
                    .find(|c| !held.contains(c) && !app.cfg.worktree_path(c).exists())
                    .unwrap_or_else(|| format!("wt-{}", &id.simple().to_string()[..8])),
            )
        }
    };
    let name = name.or(owned_name.as_deref());

    let (spawn_cwd, cmd, made_at) = {
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
        /* A claimed spare is already all of this: cut by `create_worktree`, put
        through both hooks, registered, and measured clean at the base moments ago
        by `spare::claim`. So neither the cut nor the hooks run again — running
        them would re-pay the 4.4 seconds the pool exists to remove, and
        `worktree_init` over a tree that is already based is not a repair. */
        let path = match &claimed {
            Some((_, at)) => at.clone(),
            None => {
                // The repo's own `WorktreeCreate` if it has one, ours if not. The
                // path can come back different: the hook chooses where it puts
                // things.
                let path = crate::worktree::create_worktree(
                    app,
                    name,
                    &path,
                    crate::worktree::Want::New {
                        branch: &branch,
                        base: &base,
                    },
                    Board::Loud,
                )
                .await?;
                // Both worktree hooks on top, `worktree_init` then
                // `worktree_setup`, for a repo whose setup does not hang off the
                // `WorktreeCreate` the line above just ran. Configured per repo,
                // and nothing at all when unset.
                crate::worktree::run_worktree_hooks(app, &path, Board::Loud).await;
                path
            }
        };
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
    tracing::info!(
        took_ms = tree_began.elapsed().as_millis(),
        how = if claimed.is_some() { "claimed" } else { "cut" },
        "worktree ready"
    );

    let mut cmd = cmd;
    if let Some(prev) = fork {
        cmd.push("--resume".into());
        cmd.push(prev.to_string());
        cmd.push("--fork-session".into());
    }
    cmd.extend(["--session-id".to_string(), id.to_string()]);
    cmd.extend(crate::launch::session_flags()?);
    // After the tree exists, because the environment is read in the directory the
    // session will run in — a fresh worktree is a fresh path, and `mise` answers
    // per directory.
    // Off the runtime — see the note in `spawn_session`; this is a child process
    // on the spawn path too.
    // Through `run_env`, the seam every other spawner uses, with no post token: a
    // worktree session carries no `Pass`, so it has nothing to post. Spelled as the
    // same call rather than `session_env` directly, because the difference between
    // the two is one variable an agent reports missing hours later — which is how
    // the resume path lost `ORCH_POST_TOKEN` once.
    let (env, unset) = {
        let (app, at, tok) = (app.clone(), spawn_cwd.clone(), ask_token.clone());
        crate::proc::run_blocking("reading the session environment", move || {
            run_env(&app.cfg, &at, id, Some(&tok), None, &[])
        })
        .await?
    };

    /* The path is always known here now, because the daemon cut the tree. The
    `None` arm is kept rather than made unreachable-by-construction: nothing in
    this spawner reaches it any more, and `hooks::session_start`'s adoption of a
    `PENDING_WORKTREE` row with it — that pair existed for `claude --worktree`,
    which reported its path only at `SessionStart`. */
    let (workspace, cwd) = match name {
        Some(name) => {
            // Where the tree actually is, which is the repo hook's answer when it
            // has one and `worktree_path` when it does not.
            let at = made_at.unwrap_or_else(|| app.cfg.worktree_path(name));
            app.register_worktree(name, at.clone(), Some(format!("worktree-{name}")))
                .await;
            (name.to_string(), at)
        }
        None => (PENDING_WORKTREE.to_string(), app.cfg.main_checkout.clone()),
    };

    /* **The branch the conversation is about, recorded now rather than at the next
    sweep.** `spawn_session` reads this for every other kind of session; this
    spawner did not, so a worktree session's record said `branch: None` until a
    reconcile of its workspace happened to run.

    That is not cosmetic. `api::to_carry` matches on it to decide which
    conversation travels with a branch, so a swap pressed before that sweep
    silently left the conversation behind — the tree's branch moved into main
    and the agent that had been working on it stayed put, with no error anywhere.
    Caught by the swap e2e flows failing about one run in three; invisible to
    every unit test, and easy to read as a slow resume.

    Read from the tree rather than assumed to be `worktree-<name>`, because
    `create_worktree` may have adopted a tree the repo's own `WorktreeCreate`
    hook put somewhere else, on a branch of its choosing. */
    let branch = {
        let at = cwd.clone();
        crate::proc::run_blocking("reading the worktree's branch", move || {
            crate::git::current_branch(&at).ok()
        })
        .await
        .unwrap_or(None)
    };

    // `cwd` is cloned because the arrival notice below names it.
    let mut session = Session::new(id, workspace, cwd.clone(), None);
    session.branch = branch;
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

    started(app, id, spawned.handle).await;
    // Top the pool back up, after the pty rather than before it: a cut competing
    // with the agent's own boot for the disk is the one moment it must not. Spawned,
    // so this request does not wait for it — and a no-op when the pool is already
    // full or turned off.
    crate::spare::refill_soon(app);
    Ok(id)
}

/// One run over a PR, in the shape every such spawn shares.
///
/// Four spawns — fix-pr, `/resolve`, the resolve run and the two posting runs —
/// each built the `claude --session-id --settings` argv, rendered a prompt to the
/// config dir and set `pending_prompt` for the `SessionStart` hook to type. They
/// drifted apart in the one place it mattered: two of them set the prompt *after*
/// `spawn_session` under a fresh lock, which is the window `insert_and_spawn`
/// documents — a hook landing in it took the prompt and dropped it. [`spawn_run`]
/// consumes this and sets everything on the record before the process exists.
pub struct RunSpec {
    /// The `Pass` command the session carries.
    pub command: String,
    /// What the `SessionStart` hook types: one line, `/orchd:<command> <pr>`.
    pub pending: String,
    /// Whether the run takes decisions over the ask channel, and so is handed
    /// `ORCH_ASK_TOKEN`. A run with nothing to ask is given the narrower surface.
    pub asks: bool,
    /// Variables the run needs beyond the checkout's own and the daemon's.
    pub extra_env: Vec<(String, String)>,
}

impl RunSpec {
    /// The session record for this run, as it goes into the map. Pure, so the one
    /// property the whole struct exists for — the prompt is on the record before
    /// the spawn — is testable without a `claude` to spawn.
    fn session(&self, id: SessionId, workspace: &str, path: PathBuf, pr: u64) -> Session {
        let mut session = Session::new(
            id,
            workspace.to_string(),
            path,
            Some(Pass {
                pr,
                command: self.command.clone(),
            }),
        );
        session.pending_prompt = Some(self.pending.clone());
        session
    }
}

/// The environment a run is handed, and nothing more.
///
/// Extracted because this is the seam that decides *which* credential a run holds,
/// and it used to be `app.token`: the whole API, handed to the pass whose input is
/// other people's review comments. `ask` is `None` for a run with nobody to ask,
/// `post` is `Some` only for a run that posts proposals. Both go in the environment
/// rather than the prompt, because prompt text lands in a transcript and a pty
/// buffer.
/// The proposals credential a session with this pass is handed, minted and recorded,
/// or `None` for one with nothing to post.
///
/// Its own function because the *resume* path is where this rule was forgotten
/// once: `spawn_session` rebuilds a session's environment from nothing, so a
/// resumed review run came back able to ask questions and unable to post its
/// proposals, reporting `ORCH_POST_TOKEN is absent from this environment` after it
/// had read every thread. Re-minted rather than persisted, like the ask token: the
/// value is only ever compared against the record this write updates.
pub(crate) async fn post_token_for(app: &Arc<AppState>, pass: &Option<Pass>) -> Option<String> {
    match pass {
        Some(Pass { pr, command }) if Pass::posts_proposals(command) => {
            Some(app.mint_post_token(*pr).await)
        }
        _ => None,
    }
}

pub(crate) fn run_env(
    cfg: &crate::config::Config,
    cwd: &Path,
    id: SessionId,
    ask: Option<&str>,
    post: Option<&str>,
    extra: &[(String, String)],
) -> (Vec<(String, String)>, Vec<&'static str>) {
    #[expect(
        clippy::disallowed_methods,
        reason = "this is the one call the lint funnels every spawner into"
    )]
    let (mut env, unset) = crate::launch::session_env(cfg, cwd, id, ask);
    if let Some(token) = post {
        env.push(("ORCH_POST_TOKEN".to_string(), token.to_string()));
    }
    env.extend(extra.iter().cloned());
    (env, unset)
}

/// Start a run over a PR in `workspace`: an ordinary session, typed one line.
///
/// Headed, not `-p`: a run you can watch, answer and take over mid-flight. The
/// guard tables decide whether a run may start; none of them depended on the run
/// being invisible. The line the `SessionStart` hook types is a skill invocation
/// (`/orchd:<command> <pr>`), so nothing is written into the checkout being
/// driven.
///
/// **`id` is the caller's to mint, and that is the whole reason it is a parameter
/// rather than a `Uuid::new_v4()` here.** Every run has a record of the daemon's
/// own beside the session — `automation.by_pr`, a posting run's proposals — and
/// each caller used to write it *after* this returned. A `claude` that dies at
/// once, on a bad `--settings` or the version gate, is reaped before that write
/// lands, and [`watch_session_exit`] then goes looking for a run to settle and
/// finds nothing: a posting run's "exited without posting" warning read the
/// *previous* run's proposals and stayed silent, and `fix_pr::start` carried a
/// compensating re-check for exactly this. So the caller mints the id, writes its record under
/// it, and takes the record back out if this returns `Err` — the same ordering
/// [`insert_and_spawn`] documents for the session record itself.
pub(crate) async fn spawn_run(
    app: &Arc<AppState>,
    workspace: &str,
    pr: u64,
    id: SessionId,
    spec: RunSpec,
) -> Result<SessionId> {
    let path = app
        .workspace_path(workspace)
        .await
        .context("worktree vanished")?;

    let mut cmd = vec![
        "claude".to_string(),
        "--session-id".to_string(),
        id.to_string(),
    ];
    // Through `session_flags` like every other spawn: it carries `--settings` and
    // the vendored skill's `--plugin-dir`, and that flag is per *invocation*, so a
    // run that built its own argv would be the one session on the board without it.
    cmd.extend(crate::launch::session_flags()?);

    let session = spec.session(id, workspace, path.clone(), pr);
    // Minted with the session so the same value goes into the environment and onto
    // the record: the agent reads `ORCH_ASK_TOKEN`, and `/ask`/`/wait` check it
    // against `session.ask_token`.
    let ask = spec.asks.then(|| session.ask_token.clone());
    // Narrow credentials, no broad one: asks are authenticated against this
    // session, proposals against this PR. Neither opens anything else, which is
    // what keeps "the daemon owns outward writes" an API rule rather than a
    // sentence in a prompt this run's own input could argue with.
    let post = post_token_for(app, &session.pass).await;
    // Off the runtime — see the note in `spawn_session`.
    let (env, unset) = {
        let (app, at, extra) = (app.clone(), path.clone(), spec.extra_env.clone());
        crate::proc::run_blocking("reading the session environment", move || {
            run_env(&app.cfg, &at, id, ask.as_deref(), post.as_deref(), &extra)
        })
        .await?
    };

    let spawned = insert_and_spawn(app, id, session, &cmd, &path, &env, &unset).await?;

    started(app, id, spawned.handle).await;
    Ok(id)
}

/// Start a headless `fix-pr` run pinned to the PR's head branch.
///
/// §8 writes this as a headless `--worktree` run, but `--worktree`
/// always cuts a fresh branch from `upstream/develop` while the same section
/// requires a worktree "pinned to that PR's head branch". The branch wins: the
/// worktree is created here and the run happens inside it.
pub async fn spawn_fix_pr_session(
    app: &Arc<AppState>,
    pr: u64,
    head_ref: &str,
    id: SessionId,
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

    /* **The instructions are a skill, and what the prompt substituted the
    environment carries.** `/orchd:fix-pr` is one typed line, so the six values
    `commands/fix-pr.md` had rendered into it need another way in — and for this
    run that way is not a context route. A fix run force-pushes unattended and
    is deliberately given no ask token, so a route would have meant handing it a
    credential to read four values the daemon is already building an environment
    for. `triage` went the other way for the opposite reason: it has a post token
    already and its context is a fetch the daemon would otherwise repeat.

    Being a skill is also what makes it work when *you* type `/orchd:fix-pr 42`
    in a checkout the daemon never started: the file names a fallback for each
    variable, which a prompt file rendered per run could not have. */
    let (login, base_ref) = {
        let inner = app.inner.read().await;
        (
            inner.viewer.clone(),
            inner.pr(pr).map(|p| p.base_ref.clone()),
        )
    };
    let mut extra_env = vec![
        // Parallel runs collide on ports and docker resource names, so each gets
        // its own compose project and port base (§8).
        ("COMPOSE_PROJECT_NAME".to_string(), format!("orchd-pr-{pr}")),
        (
            "ORCHD_PORT_BASE".to_string(),
            (20000 + (pr % 1000) * 20).to_string(),
        ),
        (crate::skills::VAR_PR.to_string(), pr.to_string()),
        // Not `upstream_ref` as configured: the run rebases onto the PR's *own*
        // base when the poller knows it, which is what `rebase_target` decides.
        (
            crate::skills::VAR_UPSTREAM.to_string(),
            rebase_target(
                &app.cfg.upstream_ref,
                &app.cfg.upstream_remote,
                base_ref.as_deref(),
            ),
        ),
        (
            crate::skills::VAR_UPSTREAM_REMOTE.to_string(),
            app.cfg.upstream_remote.clone(),
        ),
    ];
    /* Omitted rather than empty when the poller has not run yet, and that turns a
    refusal into a degradation: rendering the prompt *failed* the spawn here
    ("no GitHub login yet"), where the skill asks `gh api user` for it. */
    if let Some(login) = login {
        extra_env.push((crate::skills::VAR_LOGIN.to_string(), login));
    }

    // Headed, not `-p`: a run you can watch, answer and take over mid-flight, the
    // same shape as /resolve. The guard table is what decides whether the run may
    // start (§8); it never depended on the run being invisible.
    let spec = RunSpec {
        command: Pass::FIX_PR.to_string(),
        pending: format!("/orchd:{} {pr}", Pass::FIX_PR),
        // No ask token: a fix run has nothing to ask, which is the narrower surface
        // 942d01b chose on purpose.
        asks: false,
        extra_env,
    };
    spawn_run(app, &workspace, pr, id, spec).await
}

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
pub(crate) fn rebase_target(
    upstream_ref: &str,
    upstream_remote: &str,
    base_ref: Option<&str>,
) -> String {
    match base_ref {
        Some(b) if !b.is_empty() => format!("{upstream_remote}/{b}"),
        _ => upstream_ref.to_string(),
    }
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
/// branch is a choice `AppState::worktree_holding` should never have to make.
///
/// Deliberately not folded into [`AppState::worktree_holding`], which answers "is anyone
/// working on this branch". That one must keep saying yes for a live session whose
/// tree was deleted under it, or a fix run would start beside it.
async fn recorded_worktree_for(
    app: &Arc<AppState>,
    head_ref: &str,
) -> Option<(String, std::path::PathBuf)> {
    let ws = app.worktree_holding(head_ref).await?;
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
    let ws = app.worktree_holding(head_ref).await?;
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
    /* **This moves main's branch, and possibly a worktree's, so it is a swap.**
    `AppState::swapping`'s own doc has the rule: every swap involves main, so two
    are never independent — and this one reclaims the base from a worktree
    (`git::release_branch`) as well, which is a second pair a swap of that tree
    would race.

    **Skipped, not refused, and that is not a style choice.** This runs from a
    detached exit watcher: there is no request to answer and nobody to read a
    refusal, so waiting would park main long after the session that prompted it,
    against state the swap has since changed. A skip costs nothing that is not
    already recoverable — the branch stays in main until the next session there
    closes, and the swap holding the lock is itself moving main. */
    let Ok(_swap) = app.swapping.try_lock() else {
        tracing::info!("main stays where it is: a swap is moving it");
        return;
    };
    let path = app.cfg.main_checkout.clone();
    let base_ref = app.cfg.upstream_ref.clone();
    let exclude = app.cfg.worktrees_subdir_str();

    /* **The base has to be free, and main is the only checkout that may hold it.**
    Git allows one checkout per branch, so a worktree sitting on `develop` makes
    "main goes back to base" impossible — not refused, *impossible* — and every
    flow that needs main on base is blocked until somebody notices. A swap is how
    it happens: main resting on base, a worktree swapped in, and base goes out as
    the exchange. Reported as `fatal: 'develop' is already used by worktree at …`
    from four calls deep, days later.

    Reclaimed rather than prevented at the swap, because pressing swap twice has
    to stay the undo — the exchange is the feature. So the branch comes home at
    the moment it is needed, and only from a tree **nobody is working in**: the
    content does not move (`release_branch` cuts at the same commit), but a name
    changing under a live agent is a surprise the log cannot undo. A tree that is
    busy leaves main where it is, and says so.

    **Asked in the same breath as "can main park at all", and that order is the
    whole of it.** Releasing first and finding out afterwards renames a
    worktree's branch for a park that then does not happen — `park_on_base`
    returns `Ok(None)` on a dirty main, silently and by design, so a session
    closed after editing files in main would have undone the swap for nothing
    and said only that the tree "is on worktree-w now". Nothing moves unless
    everything can. */
    let plan = {
        let (at, base_ref, exclude) = (path.clone(), base_ref.clone(), exclude.clone());
        tokio::task::spawn_blocking(move || {
            let base = crate::git::base_checkout_branch(&at, &base_ref)?;
            // Already there, or carrying work: `park_on_base` would do nothing, so
            // there is nothing to clear the way for either.
            if crate::git::current_branch(&at).ok()? == base {
                return None;
            }
            if !crate::git::is_clean_excluding(&at, Some(&exclude)).ok()? {
                return None;
            }
            let holder = crate::git::holder_of_branch(&at, &base)
                .ok()
                .flatten()
                .filter(|h| h != &at);
            Some((base, holder))
        })
        .await
        .ok()
        .flatten()
    };
    // Nothing parkable: dirty, already on base, or no base ref fetched yet.
    let Some((base, holder)) = plan else {
        return;
    };
    let holder = holder.map(|tree| (tree, base.clone()));
    if let Some((tree, base)) = holder {
        let ws = app.workspace_for_path(&tree).await;
        let busy = match &ws {
            Some(id) => !app.live_sessions_in(id).await.is_empty(),
            // No workspace of ours: a tree the daemon does not manage, and moving a
            // branch in it is not the daemon's business at all.
            None => true,
        };
        if busy {
            tracing::warn!(
                "main stays off {base}: {} has it checked out{}",
                tree.display(),
                match ws {
                    Some(_) => " and a session is live there",
                    None => ", and it is not a worktree this daemon manages",
                }
            );
            return;
        }
        let stem = format!(
            "worktree-{}",
            tree.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        let at = tree.clone();
        match tokio::task::spawn_blocking(move || crate::git::release_branch(&at, &stem)).await {
            Ok(Ok(fresh)) => {
                /* The record follows the branch, because `reconcile` only ever
                *adds* to a workspace's set: left in, that tree would go on
                claiming the base for good, and two workspaces claiming it is
                what `AppState::worktree_holding` and the snapshot's PR lookup both read.
                `move_out_of_main` does the same for the same reason. */
                if let Some(id) = &ws {
                    app.forget_branch(id, &base).await;
                }
                tracing::info!(
                    "{} was on {base} and main needs it; it is on {fresh} now",
                    tree.display()
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    "main stays off {base}: {} could not let go of it: {e:#}",
                    tree.display()
                );
                return;
            }
            Err(e) => {
                tracing::warn!("releasing {base} panicked: {e}");
                return;
            }
        }
    }

    /* Whatever main holds, not only a branch `open_pr(main)` put there. It used to
    park on provenance — a mark set by that one flow — so a swapped-in branch or
    a hand-checkout stayed in main for good, on the reasoning that parking it
    would undo the swap. But nobody is working it: the last session just left, and
    a branch parked in main with no session on it blocks every PR flow that needs
    main on base, which is the thing that goes wrong far away from here. The
    branch is not lost either — it is still a branch, and `move_branch_out` is how
    it gets a tree if you want one.

    `park_on_base` re-checks the dirty tree itself, which is not a duplicate
    worth removing: the plan above was read before the base was reclaimed, and
    this is the check that runs against the tree as it stands now. */
    let moved = tokio::task::spawn_blocking(move || {
        match crate::git::park_on_base(&path, &base, Some(&exclude)) {
            Ok(was) => was,
            Err(e) => {
                tracing::warn!("main could not go back to {base}: {e:#}");
                None
            }
        }
    })
    .await
    .ok()
    .flatten();

    if let Some(was) = moved {
        // Said out loud: the checkout under every worktree just changed, and the
        // rail showing `develop` with no explanation is a worse surprise than a
        // line in the log.
        tracing::info!(from = %was, "the last session in main closed; main is back on its base");
        let _ = app.reconcile(MAIN).await;
    }
}

/// The worktree for a PR's head branch, created if absent.
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
        /* **And it moves main, so it is a swap.** `AppState::swapping` exists
        because every move of main's branch decides who travels from state read
        before anything moves, and a second one taken in that window moves a
        branch without its conversation. Three handlers took the lock and this
        arm did not, so a swap and an `open PR` could each move main at once.

        The lock is taken only once main really holds the branch, and the read is
        then repeated under it: taking it up front would refuse an ordinary
        worktree cut — which touches main not at all — for the length of any swap.
        Refused rather than queued, like the swap: the second one would run on the
        strength of what you saw before the first. */
        let mut moved_out = false;
        if main_is_on(app, head_ref).await? {
            let _swap = app.swapping.try_lock().map_err(|_| {
                anyhow::anyhow!(
                    "the main checkout is on #{pr}'s branch and a swap is already moving it; \
                     give it a moment and look at the rail before asking again"
                )
            })?;
            // Re-read under the lock: the swap this refuses may have just finished,
            // and then main is no longer the tree that holds the branch.
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
                // Main gave the branch away, and `reconcile` only adds.
                app.forget_branch(MAIN, head_ref).await;
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
                crate::worktree::run_worktree_hooks(app, &path, Board::Loud).await;
                moved_out = true;
            }
        }
        if !moved_out {
            // The repo's own `WorktreeCreate` if it has one, ours if not. It cuts
            // from a base of its own, so the tree is then put on the PR's head ref.
            path = crate::worktree::create_worktree(
                app,
                &name,
                &path,
                crate::worktree::Want::Existing { branch: head_ref },
                Board::Loud,
            )
            .await?;
            // Only when we actually cut it. Skipped when the tree was already there,
            // since setup ran when it was first created.
            crate::worktree::run_worktree_hooks(app, &path, Board::Loud).await;
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
            crate::model::short_id(&held)
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

    // The pane must be right about what is checked out the moment it changes.
    let _ = app.reconcile(MAIN).await;
    Ok(MAIN.to_string())
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

/// Drop terminal escapes, so what is left is what a person would have read.
///
/// The destination is a bar, not a terminal: a raw CSI in the middle of a sentence
/// is line noise, and an OSC title sequence would swallow the sentence after it.
/// Handles the two shapes anything prints on its way out — `ESC [ … <@-~>` and
/// `ESC ] … <BEL|ESC \>` — and drops a lone `ESC` with whatever followed it.
fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                for c in chars.by_ref() {
                    if c == '\u{7}' || c == '\u{1b}' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The ring buffer, once the reader has had its chance at it.
///
/// **`wait()` resolves on the child being reaped, not on its output arriving.** The
/// pty reader is its own blocking thread, so a program that prints a line and exits
/// at once is a race: about one run in six, the snapshot is still empty when the
/// exit watcher reads it, and the bar then says the agent "exited at once (1) and
/// said nothing" about an agent that said exactly what was wrong. Reproduced by
/// running `an_agent_that_dies_at_once_reports_what_it_said` six times; `pty.rs`'s
/// own test already polls for this and the daemon did not.
///
/// Bounded and early-exiting, so the ordinary case — output already there — waits
/// for nothing at all, and the case that waits the full budget is an agent that
/// really did print nothing, where a fifth of a second before a message nobody is
/// blocking on is invisible.
async fn drained(handle: &Arc<PtyHandle>) -> Vec<u8> {
    const PATIENCE: std::time::Duration = std::time::Duration::from_millis(200);
    let deadline = std::time::Instant::now() + PATIENCE;
    loop {
        let seen = handle.snapshot();
        if !seen.is_empty() || std::time::Instant::now() >= deadline {
            return seen;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// The one line of a dead agent's output worth putting in front of a person.
///
/// **The first line with anything in it, not the last.** A program that cannot
/// start says why in its first sentence and then spends four more telling you how
/// to fix it — the shadowing npm shim that prompted this opens with `Error: claude
/// native binary not installed.` and ends on `Or reinstall without
/// --ignore-scripts` — so the tail is the least useful part of the buffer.
///
/// Bounded, because the bar is one line tall and the ring holds ~3600 of them.
fn agent_complaint(buf: &[u8]) -> Option<String> {
    const MAX: usize = 200;
    let text = String::from_utf8_lossy(buf);
    let clean = strip_escapes(&text);
    let line = clean.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(if line.chars().count() > MAX {
        line.chars().take(MAX).collect::<String>() + "\u{2026}"
    } else {
        line.to_string()
    })
}

/// The tail every spawn shares: arm the one exit observer, re-check the agent
/// version, and tell the page.
///
/// **One call because it was four copies and one of them was short.** The story
/// filer armed the watcher and notified but never asked `update::refresh_detached`,
/// so a spawn that was in every other way a run of its own quietly skipped the
/// version check. A tail that is spelled out at each site is a tail that drifts at
/// one of them, and the one it drifts at is whichever was written last.
///
/// Not folded into [`insert_and_spawn`], which would make it unforgettable, for
/// one reason: `spawn_session` does `reclaim_main` between the insert and this,
/// and that ordering is load-bearing — see the comment there.
pub(crate) async fn started(app: &Arc<AppState>, id: SessionId, handle: Arc<PtyHandle>) {
    watch_session_exit(app.clone(), id, handle);
    crate::update::refresh_detached(app);
    app.notify().await;
}

/// The one observer of a session's pty exit: it settles the record and dispatches
/// whatever that session's pass owes on its way out. Every spawner arms it,
/// `triage::spawn_posting_run` included — a review session that had its own
/// watcher never reached the hand-off below, so `fix_pr_on_exit` was set, the
/// session was killed, and no run ever started.
pub(crate) fn watch_session_exit(app: Arc<AppState>, id: SessionId, handle: Arc<PtyHandle>) {
    tokio::spawn(async move {
        let code = handle.wait().await;
        /* What this session *was*, read while the lock is already held and acted on
        below. A run's end is owed to the module that started it — a fix run's
        verdict to `fix_pr`, the review's hand-off to the same — and this is the
        only place that learns a pty is over, which is why the news is *published*
        here rather than dispatched by name. `state::RunObserver` says why that
        inversion is the shape and a hook on `RunSpec` is not. */
        let mut ended: Option<crate::state::RunExit> = None;
        // The run's own account is the daemon's record rather than a feature's, so
        // it is closed here: nothing else would ever notice that the thing working
        // through it had stopped, and threads left `pending` then read as imminent
        // for as long as the daemon runs.
        // A run whose success is "proposals arrived", not "exited zero": an agent
        // can finish cleanly having posted nothing, and that is the failure the
        // user would otherwise stare at an empty overlay wondering about.
        let mut posting_for: Option<u64> = None;
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
                    if let Some(pass) = &s.pass {
                        if Pass::posts_proposals(&pass.command) {
                            posting_for = Some(pass.pr);
                        }
                        ended = Some(crate::state::RunExit {
                            session: id,
                            pass: pass.clone(),
                            // Only a review that said so. Every other way one ends
                            // — you closed it, it fell over reading, you killed it
                            // mid-cards — leaves the flag false and hands on
                            // nothing, which is the whole difference between this
                            // and a run that trips behind you.
                            hand_off: s.fix_pr_on_exit,
                        });
                    }
                    // Last chance to find the conversation. A session closed
                    // between two `Stop`s can be carrying a transcript path
                    // Claude Code reported and never wrote to, and once it is
                    // archived nothing else goes looking.
                    crate::store::pin_transcript(s.id, &s.cwd, &mut s.transcript_path);
                    let ws = s.workspace.clone();
                    // A session that never had a turn is an empty pane, not a
                    // conversation: keeping its row and header file would only
                    // offer a resume that exits instantly and a fork that dies in a
                    // fresh worktree. A session started as a pass is exempt — a run
                    // that never got going is tracked as `Exhausted`, not deleted
                    // (§8).
                    if s.pass.is_none() && !s.had_a_turn {
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
            /* A pane you opened and closed and an agent that cannot start are the
            same exit to everything above: turnless, so the row, the transcript and
            the tree all go. The difference is *why*, and without it the second case
            is completely silent — a `claude` shadowed on PATH by a broken install
            took every press of the new-worktree button and left nothing on screen,
            no row, no pane, no message.

            Two conditions, and both are needed. A non-zero code is the failure; and
            `stopped_deliberately` excludes the kill button, which ends a session by
            `SIGKILL` and so produces a failure code of exactly this shape. What the
            agent printed is the useful half — it is the binary's own sentence about
            itself — so the tail of the ring buffer travels with the report. */
            if code != 0 && !handle.stopped_deliberately() {
                let said = agent_complaint(&drained(&handle).await);
                let mut inner = app.inner.write().await;
                inner.agent_error = Some(match said {
                    Some(line) => format!("the agent exited at once ({code}): {line}"),
                    None => format!("the agent exited at once ({code}) and said nothing"),
                });
            }
            /* And the tree it opened in, which is the half that leaked. Forgetting
            the row left a worktree on disk with nothing pointing at it: 32 of the
            61 trees on the machine this was written for, and the retention timer
            could not date any of them because the record that dates a tree is
            the one just deleted. A session and the worktree cut for it are one
            thing, so they end together.

            Through the ordinary preflight, which is what makes it safe without a
            second set of rules: a tree holding uncommitted or unpushed work
            refuses, and so does one a *second* session is still live in — the
            case that matters, since two conversations can share a worktree.

            Not gated on `spawn_cut_worktree`: that flag is only ever set on the
            agent-spawn path, so every worktree session you start from the rail
            has it false, which is exactly the population this is for. The
            preflight is the authorisation instead. Same switch as the timer:
            `0` means the daemon never removes a worktree by itself. */
            if app.cfg.worktree_retention_days > 0 {
                if let Some(ws) = workspace.as_deref() {
                    if ws != MAIN && ws != PENDING_WORKTREE {
                        match crate::worktree::teardown(&app, ws).await {
                            Ok(_) => tracing::info!(
                                workspace = %ws,
                                "removed the worktree it opened in, having left nothing behind"
                            ),
                            // Debug: a tree that carries work is a correct refusal,
                            // and the row is gone either way.
                            Err(e) => tracing::debug!(workspace = %ws, "tree left in place: {e:#}"),
                        }
                    }
                }
            }
        }
        if let Some(pr) = posting_for {
            if !app.inner.read().await.proposals.contains_key(&pr) {
                tracing::warn!(pr, session = %id, "the run exited without posting proposals");
            }
        }
        app.release_main(id).await;
        /* And the news, after the claim is given back rather than before it.
        The hand-off is the reason: `fix_pr::start` refuses while a live session
        holds the branch, and that is only true once the record says `Exited` and
        main has let go. The verdict half does not care either way. */
        if let Some(exit) = ended {
            app.run_ended(exit).await;
        }
        if let Some(ws) = workspace {
            // The drawer's processes belong to whoever was working here. Only once
            // the workspace is empty, though: a worktree holds one session at a time
            // now, but the swap's relocation briefly leaves the incoming one live
            // beside the outgoing, and killing the shells then would pull them out
            // from under the session that is staying.
            if app.live_sessions_in(&ws).await.is_empty() {
                // Through `stop_managed`, like every other path that means stop
                // this: it runs the configured `stop_command` and escalates past a
                // trapped `SIGHUP`, neither of which a bare `kill` here did.
                let leaving = app.processes_to_stop(&ws).await;
                for (name, pty) in &leaving {
                    crate::managed::stop_managed(&app, &ws, name, pty).await;
                }
                if !leaving.is_empty() {
                    tracing::info!(
                        workspace = %ws,
                        "stopped {} process(es) with the last session",
                        leaving.len()
                    );
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

/// Resolve the worktree a session actually landed in.
///
/// `claude --worktree` reports the real path at `SessionStart`; until then the
/// daemon only knows where it asked for the worktree to be created.
pub fn worktree_name_of(path: &Path, worktrees_dir: &Path) -> Option<String> {
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
        assert_eq!(
            rebase_target("origin/main", "origin", Some("")),
            "origin/main"
        );
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
        // `env_source: none`: this pins what the *daemon* puts in, and a real
        // source would add whatever the machine running the test exports. The port
        // is named because `ORCH_URL` below is read back against it.
        let (app, dir) =
            crate::testutil::app_with("agent-env", r#""port":7794,"env_source":"none""#);

        let id = Uuid::new_v4();
        #[expect(
            clippy::disallowed_methods,
            reason = "asserting on what the seam wraps"
        )]
        let (env, _) = crate::launch::session_env(&app.cfg, &dir, id, Some("ask-tok"));
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
        let (app, _dir) = crate::testutil::app("managed");

        let spec = crate::config::ManagedSpec {
            name: "quick".into(),
            // POSIX, and it ends on its own without printing a health line — so
            // only the exit can move it off `Starting`.
            command: vec!["sh".into(), "-c".into(), "exit 3".into()],
            failure_patterns: Vec::new(),
            ok_patterns: Vec::new(),
            restart: crate::config::RestartPolicy::Never,
            autostart: true,
            stop_command: Vec::new(),
        };
        let id = crate::managed::start_managed(&app, MAIN, &spec)
            .await
            .expect("started");

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
        let dir = crate::testutil::scratch("prcut");
        let main = dir.join("repo");
        let run = crate::testutil::git;
        run(&dir, &["init", "-q", "-b", "develop", "repo"]);
        run(&main, &["config", "user.email", "t@t"]);
        run(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join("f.txt"), "base\n").unwrap();
        run(&main, &["add", "-A"]);
        run(&main, &["commit", "-qm", "base"]);
        // Main on the PR's branch, with a change nobody committed — the exact state.
        run(&main, &["switch", "-qc", "feature/theirs"]);
        std::fs::write(main.join("f.txt"), "edited in main\n").unwrap();

        let app = crate::testutil::app_at(&main, r#""upstream_ref":"origin/develop""#);

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
        assert_eq!(
            app.worktree_holding("feature/theirs").await.as_deref(),
            Some("pr-4242")
        );
    }

    /// The race a relocation would otherwise lose.
    ///
    /// Relocating resumes under the *same id*, so the record the old watcher wakes
    /// up to find is a **live session at the far end** — and settling it there would
    /// mark it exited and, because `release_main` keys on the id, hand main's claim
    /// straight back out from under it. Two real ptys, because the pty is the
    /// identity that tells the two apart and a mock would be asserting the fix
    /// against itself.
    /// A workspace record outlives the directory it names, and the PR flows key on
    /// that record. The run this cost opened in `$HOME`: the tree had been removed,
    /// `AppState::worktree_holding` handed its name back anyway, `ensure_pr_worktree` took the
    /// early return, and the spawn was aimed at a path that was not there.
    #[tokio::test]
    async fn a_worktree_whose_directory_is_gone_does_not_hold_a_branch() {
        let (app, dir) = crate::testutil::app("holding");

        let here = dir.join("here");
        std::fs::create_dir_all(&here).unwrap();
        let gone = dir.join("gone");
        let _ = std::fs::remove_dir_all(&gone);

        app.register_worktree("here", here, Some("feature/here".into()))
            .await;
        app.register_worktree("gone", gone, Some("feature/gone".into()))
            .await;

        let (name, path) = recorded_worktree_for(&app, "feature/here")
            .await
            .expect("standing");
        assert_eq!((name.as_str(), path.is_dir()), ("here", true));

        // The one that matters: the record survives its directory, and it comes back
        // with the path so the flow rebuilds the tree where it stood rather than
        // cutting a second one for the same branch.
        let (name, path) = recorded_worktree_for(&app, "feature/gone")
            .await
            .expect("recorded");
        assert_eq!(name, "gone");
        assert!(!path.is_dir(), "the tree is gone; the record is not");
        assert!(
            path.ends_with("gone"),
            "and it names where it stood: {}",
            path.display()
        );

        // And it is still where that branch is being worked on, which is a different
        // question and the one the busy guard asks.
        assert_eq!(
            app.worktree_holding("feature/gone").await.as_deref(),
            Some("gone"),
            "a deleted tree does not free the branch for a second agent"
        );
    }

    /// The stop command runs, and it runs *before* the pty dies.
    ///
    /// The order is the entire fix: `docker compose exec` leaves the container-side
    /// process running when its client is killed, so a kill-then-stop would be
    /// talking to a client that is already gone. Proven by having the stop command
    /// write a file and then asserting the pty was still alive when it ran — the
    /// script records the client's own liveness, so a reordering fails this.
    #[tokio::test]
    async fn the_stop_command_runs_before_the_client_is_killed() {
        use crate::config::{Config, ManagedSpec, RestartPolicy};

        let dir = crate::testutil::scratch("stopcmd");
        // Built by hand rather than through `testutil::app`, because the managed
        // process below is a `ManagedSpec` value rather than something spellable
        // in the config JSON.
        let mut cfg: Config = serde_json::from_str(&format!(
            r#"{{"main_checkout":{:?}}}"#,
            dir.to_string_lossy()
        ))
        .unwrap();
        cfg.main_processes = vec![ManagedSpec {
            name: "watcher".into(),
            // Stands in for the exec client: writes its own pid, then waits to be
            // killed.
            command: vec![
                "sh".into(),
                "-c".into(),
                format!("echo $$ > {}/client.pid; sleep 30", dir.display()),
            ],
            failure_patterns: Vec::new(),
            ok_patterns: Vec::new(),
            restart: RestartPolicy::Never,
            autostart: false,
            // Reads that pid and records whether the client was still alive when
            // this ran. `kill -0` asks without signalling.
            stop_command: vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "if kill -0 $(cat {d}/client.pid) 2>/dev/null;                      then echo alive > {d}/stopped; else echo dead > {d}/stopped; fi",
                    d = dir.display()
                ),
            ],
        }];
        let app = AppState::new(cfg, "t".into(), crate::window::Chrome::None);

        let spec = app.cfg.main_processes[0].clone();
        crate::managed::start_managed(&app, MAIN, &spec)
            .await
            .expect("started");
        let pty = {
            let inner = app.inner.read().await;
            inner.workspaces[MAIN].processes[0]
                .pty
                .clone()
                .expect("a pty")
        };
        assert!(pty.is_alive(), "the stand-in client is running");
        // Its pid file, which the stop command reads.
        for _ in 0..40 {
            if dir.join("client.pid").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        crate::managed::stop_managed(&app, MAIN, "watcher", &pty).await;

        let recorded =
            std::fs::read_to_string(dir.join("stopped")).expect("the stop command ran at all");
        assert_eq!(
            recorded.trim(),
            "alive",
            "it must run before the kill, not after"
        );
        // And the client is gone afterwards, stop command or not.
        for _ in 0..40 {
            if !pty.is_alive() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(!pty.is_alive(), "the pty is killed after the stop command");
    }

    /// The record has to be visible to a hook before the process that fires it
    /// exists. Asserted on the helper rather than on a spawn, because the thing
    /// that made this a bug — a real agent booting faster than the insert — is
    /// exactly what a test cannot reproduce.
    #[tokio::test]
    async fn the_record_is_in_the_map_before_the_process_starts() {
        let (app, dir) = crate::testutil::app("insert");

        let id = Uuid::new_v4();
        let session = Session::new(id, MAIN.to_string(), dir.clone(), None);
        let cmd = ["cat".to_string()];
        let spawned = insert_and_spawn(&app, id, session, &cmd, &dir, &[], &[])
            .await
            .expect("spawn cat");

        let inner = app.inner.read().await;
        let s = inner.sessions.get(&id).expect("the record is there");
        assert!(
            s.pty.is_some(),
            "the handle is hung on the record afterwards"
        );
        assert_eq!(s.pid, spawned.pid);
        drop(inner);
        let _ = spawned.handle.kill();
    }

    /// And a spawn that never happened leaves nothing behind: the record would
    /// otherwise sit in the rail as a session with no process, holding its
    /// workspace against the next attempt.
    #[tokio::test]
    async fn a_refused_spawn_takes_its_record_back_out() {
        let (app, dir) = crate::testutil::app("refused");

        let id = Uuid::new_v4();
        let session = Session::new(id, MAIN.to_string(), dir.clone(), None);
        let cmd = ["orchd-no-such-binary-ever".to_string()];
        let err = insert_and_spawn(&app, id, session, &cmd, &dir, &[], &[]).await;

        assert!(err.is_err(), "a missing binary is a failed spawn");
        assert!(
            !app.inner.read().await.sessions.contains_key(&id),
            "the record must not outlive the attempt"
        );
    }

    #[tokio::test]
    async fn a_stale_watcher_does_not_settle_the_session_that_replaced_it() {
        use crate::pty::PtyHandle;

        let (app, dir) = crate::testutil::app("stale");

        let pty = |()| {
            PtyHandle::spawn(
                &["cat".to_string()],
                std::path::Path::new("/tmp"),
                &[],
                &[],
                (24, 80),
            )
            .unwrap()
        };
        let (old, new) = (pty(()), pty(()));

        // The session as the relocation leaves it: same id, holding the *new* pty,
        // live, and owning main.
        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), dir.clone(), None);
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
        assert!(
            s.state.is_live(),
            "a stale watcher exited the live session: {:?}",
            s.state
        );
        assert_eq!(
            inner.workspaces.get(MAIN).and_then(|w| w.occupant),
            Some(id),
            "a stale watcher released the main claim the resume had taken"
        );
        drop(inner);

        let _ = new.handle.kill();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An agent that cannot start has to say so, because nothing else is left to.
    ///
    /// The row, the transcript and the worktree all go with a turnless session, so
    /// the whole failure was silent: a `claude` shadowed on PATH by a broken
    /// install took every press of the new-worktree button and put nothing at all
    /// on screen.
    #[tokio::test]
    async fn an_agent_that_dies_at_once_reports_what_it_said() {
        use crate::pty::PtyHandle;

        let (app, dir) = crate::testutil::app("agenterr");
        let spawned = PtyHandle::spawn(
            &[
                "sh".to_string(),
                "-c".to_string(),
                // The shape the real one had: a first line that says what is wrong
                // and three more telling you how to fix it.
                "echo 'Error: claude native binary not installed.'; echo; \
                 echo 'Run the postinstall manually'; exit 1"
                    .to_string(),
            ],
            std::path::Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .unwrap();

        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), dir.clone(), None);
            s.pty = Some(spawned.handle.clone());
            /* Deliberately *not* `set_state(Working)`: that latches `had_a_turn`,
            which is the whole condition being tested. A session that never had a
            turn is one that never reached `Working`, and writing it the other way
            made this test pass through the branch it was written to exercise. */
            inner.sessions.insert(id, s);
        }
        watch_session_exit(app.clone(), id, spawned.handle.clone());
        // A condition rather than a sleep: what is being waited for is the watcher
        // having settled, and a fixed sleep trades flakiness for slowness and gets
        // both. The race underneath it is `drained`'s.
        for _ in 0..200 {
            if app.inner.read().await.agent_error.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let inner = app.inner.read().await;
        assert!(
            !inner.sessions.contains_key(&id),
            "a turnless session is still forgotten; this changes what is said, not what is kept"
        );
        let said = inner.agent_error.clone().expect("the failure is reported");
        assert!(
            said.contains("claude native binary not installed"),
            "the agent's own first line is the part that says what to fix: {said}"
        );
        drop(inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ending a session yourself is not a fault, and it exits like one.
    ///
    /// `kill_gracefully` escalates to `SIGKILL`, so the code a killed session
    /// leaves is indistinguishable from a crash — which is why the handle records
    /// that the stop came from us.
    #[tokio::test]
    async fn killing_a_turnless_session_reports_nothing() {
        use crate::pty::PtyHandle;

        let (app, dir) = crate::testutil::app("agentkill");
        let spawned = PtyHandle::spawn(
            &["cat".to_string()],
            std::path::Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .unwrap();

        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(id, MAIN.to_string(), dir.clone(), None);
            s.pty = Some(spawned.handle.clone());
            // Turnless, like the test above and for the same reason: this is the
            // branch that reports, so the kill has to arrive inside it to be excluded.
            inner.sessions.insert(id, s);
        }
        watch_session_exit(app.clone(), id, spawned.handle.clone());
        spawned.handle.kill_gracefully().await;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let inner = app.inner.read().await;
        assert!(
            inner.agent_error.is_none(),
            "a session you ended reads as a fault: {:?}",
            inner.agent_error
        );
        drop(inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_agent_complaint_is_its_first_line_without_escapes() {
        // A clear, a home, and a title sequence — what anything writes to a pty
        // before it says a word.
        let buf = b"\x1b[2J\x1b[H\x1b]0;a title\x07Error: not installed.\r\nhow to fix it\r\n";
        assert_eq!(
            agent_complaint(buf).as_deref(),
            Some("Error: not installed."),
            "the first line with anything in it, and no escapes left in it"
        );
        assert_eq!(agent_complaint(b"").as_deref(), None, "said nothing");
        assert_eq!(
            agent_complaint(b"\r\n   \r\n").as_deref(),
            None,
            "whitespace is saying nothing"
        );
        let long = agent_complaint(&vec![b'x'; 400]).expect("a long line still answers");
        assert!(
            long.chars().count() <= 201,
            "the bar is one line tall: {}",
            long.chars().count()
        );
    }

    /// The prompt is on the record the spawn inserts, not set afterwards. Two of
    /// the four run spawns used to set it under a fresh lock after `spawn_session`
    /// returned, and a `SessionStart` landing in between took nothing and dropped
    /// it: a run that never received its instructions.
    #[test]
    fn a_run_spec_puts_the_prompt_on_the_record_before_the_spawn() {
        let spec = RunSpec {
            command: Pass::FIX_PR.to_string(),
            pending: "/orchd:fix-pr 7".to_string(),
            asks: true,
            extra_env: Vec::new(),
        };
        let id = Uuid::new_v4();
        let s = spec.session(id, "pr-7", PathBuf::from("/tmp"), 7);
        assert_eq!(s.pending_prompt.as_deref(), Some("/orchd:fix-pr 7"));
        assert_eq!(s.id, id);
        assert!(matches!(
            &s.pass,
            Some(Pass { pr: 7, command }) if command == Pass::FIX_PR
        ));
    }

    /// Every field a resume carries actually lands on the record that continues it.
    ///
    /// **The one test the four historical losses would each have failed.**
    /// `created_at`, `spawned_by`, `spawn_cut_worktree` and `forked_from` all
    /// persisted, were restored at boot, and were then thrown away by the respawn a
    /// moment later — each found by a behaviour that had quietly stopped working
    /// (a stale-file warning that never fired, a spawned child that could no longer
    /// be undone, a forked row that stopped reading as forked), never by a test.
    /// Checked against deliberate breakage: dropping any assignment in
    /// [`Carried::apply`] fails this.
    #[test]
    fn a_resume_carries_every_field_it_says_it_does() {
        let parent = Uuid::new_v4();
        let began = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let mut prev = Session::new(Uuid::new_v4(), "wt".into(), PathBuf::from("/tmp"), None);
        prev.interrupted = true;
        prev.had_a_turn = true;
        prev.branch = Some("feature/x".into());
        prev.arrival_notice = Some("moved while you were away".into());
        prev.name = Some("the one you named".into());
        prev.spawned_by = Some(parent);
        prev.spawn_cut_worktree = true;
        prev.forked_from = Some(parent);
        prev.created_at = began;

        let mut next = Session::new(Uuid::new_v4(), "wt".into(), PathBuf::from("/tmp"), None);
        let branch = Carried::from(Some(&prev), false).apply(&mut next);

        assert_eq!(branch.as_deref(), Some("feature/x"), "the branch");
        assert!(next.interrupted, "the interrupted turn");
        assert!(next.had_a_turn, "that it is a conversation at all");
        assert_eq!(
            next.arrival_notice.as_deref(),
            Some("moved while you were away")
        );
        assert_eq!(next.name.as_deref(), Some("the one you named"));
        assert_eq!(next.spawned_by, Some(parent), "who spawned it");
        assert!(next.spawn_cut_worktree, "whether that spawn cut the tree");
        assert_eq!(next.forked_from, Some(parent), "that it was forked");
        assert_eq!(next.created_at, began, "when the conversation began");
    }

    /// A fork is a new conversation, so four of those deliberately do *not* travel.
    #[test]
    fn a_fork_starts_its_own_conversation() {
        let parent = Uuid::new_v4();
        let began = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let mut prev = Session::new(Uuid::new_v4(), "wt".into(), PathBuf::from("/tmp"), None);
        prev.interrupted = true;
        prev.name = Some("the one you named".into());
        prev.forked_from = Some(parent);
        prev.created_at = began;
        prev.had_a_turn = true;

        let mut next = Session::new(Uuid::new_v4(), "wt".into(), PathBuf::from("/tmp"), None);
        Carried::from(Some(&prev), true).apply(&mut next);

        assert!(!next.interrupted, "a fork opens at a fresh prompt");
        assert!(next.name.is_none(), "a fork earns its own name");
        assert!(
            next.forked_from.is_none(),
            "spawn_session sets this from the Source"
        );
        assert_ne!(next.created_at, began, "a fork started when it was forked");
        // And the one that does: a fork replays the parent's turns, so it opens on
        // a conversation rather than an empty pane.
        assert!(next.had_a_turn);
    }

    /// What the exit watcher published, for the test below.
    ///
    /// A `static` because [`crate::state::RunObserver`] is a plain `fn`: the
    /// observer is installed once per process and outlives every call, so there is
    /// nothing for a closure to capture.
    static PUBLISHED: std::sync::Mutex<Vec<crate::state::RunExit>> =
        std::sync::Mutex::new(Vec::new());

    /// A review that asked for its checks to be handed on says so when its pty ends.
    ///
    /// The review spawner used to arm a watcher of its own that never reached the
    /// hand-off, so the flag stayed set and nothing started. What is asserted is the
    /// *published* exit, since settling one is `orchd-serve`'s now
    /// ([`crate::state::RunObserver`]) — and the payload is the stronger assertion
    /// anyway: the old test could only watch a refusal take the flag back.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_review_exit_acts_on_its_hand_off_flag() {
        use crate::pty::PtyHandle;

        let (app, dir) = crate::testutil::app("handoff-exit");

        let pty = PtyHandle::spawn(
            &["cat".to_string()],
            std::path::Path::new("/tmp"),
            &[],
            &[],
            (24, 80),
        )
        .unwrap();
        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.write().await;
            let mut s = Session::new(
                id,
                "wt".to_string(),
                dir.clone(),
                Some(Pass {
                    pr: 4242,
                    command: Pass::REVIEW.to_string(),
                }),
            );
            s.pty = Some(pty.handle.clone());
            s.set_state(State::Working);
            s.fix_pr_on_exit = true;
            inner.sessions.insert(id, s);
        }

        app.observe_runs(|_app, exit| {
            Box::pin(async move {
                PUBLISHED.lock().expect("the recorder").push(exit);
            })
        });
        PUBLISHED.lock().expect("the recorder").clear();

        watch_session_exit(app.clone(), id, pty.handle.clone());
        let _ = pty.handle.kill();

        let mut published = None;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            if !app.inner.read().await.sessions[&id].state.is_live() {
                published = PUBLISHED.lock().expect("the recorder").pop();
                break;
            }
        }
        let exit = published.expect("the exit was never published");
        assert_eq!(exit.session, id);
        assert_eq!(exit.pass.pr, 4242);
        assert_eq!(exit.pass.command, Pass::REVIEW);
        assert!(exit.hand_off, "the review asked for the hand-off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Main goes back to base when the last session leaves it, whatever put the
    /// branch there — and it takes the base back off a worktree that is sitting on
    /// it, because git allows one checkout per branch and that tree is the reason
    /// the switch is impossible rather than merely refused.
    ///
    /// Driven against a real checkout: the interesting half is git's own refusal,
    /// which no hand-built state can produce.
    #[tokio::test]
    async fn park_returns_main_to_base_and_takes_the_base_back_to_do_it() {
        let dir = crate::testutil::scratch("park");
        let git = |args: &[&str], at: &std::path::Path| {
            crate::testutil::git(at, args);
        };
        git(&["init", "-q", "-b", "main", "main"], &dir);
        // Canonical because `scratch` is, which is what makes this fixture behave
        // like a real checkout on a Mac — see that docblock.
        let repo = dir.join("main");
        git(&["config", "user.email", "t@t"], &repo);
        git(&["config", "user.name", "t"], &repo);
        std::fs::write(repo.join("f.txt"), "base\n").unwrap();
        git(&["add", "-A"], &repo);
        git(&["commit", "-qm", "base"], &repo);
        git(&["branch", "feature/x"], &repo);

        // `origin/main` resolves to the local `main` branch as the base — no remote
        // needed, since only the branch part is used for a non-HEAD ref.
        let app = crate::testutil::app_at(&repo, r#""upstream_ref":"origin/main""#);

        let on = |b: &str| git(&["switch", "-q", b], &repo);
        let branch = || crate::git::current_branch(&repo).unwrap();

        // Whatever put it there. This used to need an `open_pr(main)` mark, so a
        // swapped-in branch or a hand-checkout stayed in main for good — and a
        // branch nobody is working blocks every flow that needs main on base.
        on("feature/x");
        park_main(&app).await;
        assert_eq!(
            branch(),
            "main",
            "the last session left; main goes back to base"
        );

        // Already there: nothing to do and nothing said.
        park_main(&app).await;
        assert_eq!(branch(), "main");

        // --- and now the case that could not be fixed from inside main ---
        //
        // A worktree holding base is what a swap leaves behind when main was resting
        // on base, and `git switch main` in main then fails outright.
        let tree = repo.join(".claude/worktrees/w");
        git(
            &["worktree", "add", "-q", tree.to_str().unwrap(), "feature/x"],
            &repo,
        );
        git(&["switch", "-q", "-c", "feature/x-2"], &repo);
        git(&["switch", "-q", "main"], &tree);
        assert_eq!(crate::git::current_branch(&tree).unwrap(), "main");
        // Registered, because "is anybody working in there" is asked of the
        // workspace, and an unmanaged directory is deliberately left alone.
        app.register_worktree("w", tree.clone(), Some("main".into()))
            .await;

        assert!(
            crate::git::switch_branch(&repo, "main").is_err(),
            "git must refuse a branch checked out elsewhere",
        );

        /* **Nothing moves unless everything can.** A dirty main cannot park —
        `park_on_base` says so by doing nothing — so taking the base off the
        worktree first would undo a swap for a park that never happens, and say
        only that the tree "is on worktree-w now". Asserted before the happy
        path, because the happy path would hide it. */
        std::fs::write(repo.join("f.txt"), "editing in main\n").unwrap();
        park_main(&app).await;
        assert_eq!(branch(), "feature/x-2", "a dirty main does not park");
        assert_eq!(
            crate::git::current_branch(&tree).unwrap(),
            "main",
            "and nothing was taken off the worktree for it",
        );
        git(&["checkout", "-q", "--", "f.txt"], &repo);

        park_main(&app).await;
        assert_eq!(branch(), "main", "main took its base back");
        assert_eq!(
            crate::git::current_branch(&tree).unwrap(),
            "worktree-w",
            "the tree keeps its content and gets a name of its own",
        );
        /* And the *record* followed the branch. `reconcile` only ever adds to a
        workspace's set, so a base left in there is claimed by two workspaces
        for good — which `AppState::worktree_holding` and the snapshot's PR lookup both
        read. Asserted here because the git side passing says nothing about it. */
        assert!(
            !app.inner.read().await.workspaces["w"]
                .branches
                .iter()
                .any(|b| b == "main"),
            "the worktree still claims the base it gave up",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fix-pr guard used to ask `is_busy()` while the spawn enforces `is_live`,
    /// so a session sitting idle on the branch passed a guard whose refusal reads
    /// "live session" and was only stopped later, once the run looked started. Both
    /// now read this.
    #[tokio::test]
    async fn an_idle_session_on_the_branch_still_makes_it_busy() {
        let dir = crate::testutil::scratch("busy");
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
                    kind: crate::model::WorkspaceKind::Worktree {
                        name: "pr-4".into(),
                    },
                    branches: ["feature".to_string()].into_iter().collect(),
                    processes: Vec::new(),
                    occupant: None,
                    tree: Default::default(),
                    banked: None,
                },
            );
            let mut s = Session::new(id, "pr-4".into(), dir.join("pr-4"), None);
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
