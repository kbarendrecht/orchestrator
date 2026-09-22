//! Respawn sessions in place, so they run the `claude` installed now.
//!
//! **What this replaces is quitting the app.** Every spawn resolves the agent
//! through a fresh `mise env` (`docs/traps/performance.md`), so a respawn picks up
//! a `mise up` on its own — and the only way to get one was to restart the whole
//! app and let `auto_resume` bring each session back. That respawned the drawer's
//! shells and processes too, and every other checkout, for an upgrade that
//! concerns none of them.
//!
//! **Asking queues; one watcher restarts.** The routes set
//! [`crate::model::Session::restart_queued`] and return at once, and
//! [`run_due`] respawns the flagged sessions that are safe to interrupt, one at a
//! time. Three reasons for the split. A session mid-turn cannot be restarted
//! without losing the turn, so "restart all" is necessarily "restart each when
//! it is ready", and the queue is what remembers the rest. A restart is a kill,
//! a grace window and a spawn, so doing them inside a request would hold it open
//! for seconds per session. And one sequential loop is the serialisation: two
//! restarts of one id cannot overlap because nothing else starts one.
//!
//! **The respawn is [`crate::spawn::relocate_session`] into the session's own
//! workspace.** It already kills gracefully, waits for the pty to be gone,
//! resumes the same id, forks if the resume dies, and puts back the title, the
//! name and the pass. A transcript "moved" to where it already is moves nowhere
//! (`store::relocate_file` checks), and main's claim is only released when a
//! session *leaves* main — so the same call with the same workspace is a restart.

use std::sync::Arc;

use anyhow::Result;

use crate::model::SessionId;
use crate::state::AppState;

/// How long a respawned session has to prove it stayed up — the swap's number,
/// for the swap's reason: a `--resume` that finds nothing exits at once, and the
/// fork fallback hangs on noticing that.
const GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// What asking for a restart did.
#[derive(Debug, Default, serde::Serialize, PartialEq)]
pub struct Queued {
    /// Sessions now waiting to be respawned.
    pub queued: usize,
    /// Of those, how many can go straight away. The rest go when their turn ends,
    /// and the page says so rather than leaving a count that looks like a failure.
    pub now: usize,
}

/// Which sessions a restart is for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Which {
    /// One session, asked for from its row.
    One(SessionId),
    /// Every live agent session in this checkout.
    All,
    /// The ones an upgrade left behind — see `Session::agent_stale`. What the
    /// agent bar asks for: a session opened after the upgrade is on the new build
    /// already, and respawning it would cost its scrollback for nothing.
    Stale,
}

/// Flag sessions for a respawn, and wake the watcher.
///
/// Drawer shells and managed processes are not sessions, so they are never in
/// any of these — `mise up` does not change them and restarting them would cost a
/// running stack for nothing.
///
/// A session that is not live is refused by name rather than skipped when it was
/// asked for by id: "restart" on an archived row is a resume, and saying so is
/// more use than a count of zero.
pub async fn queue(app: &Arc<AppState>, which: Which) -> Result<Queued> {
    let only = match which {
        Which::One(id) => Some(id),
        Which::All | Which::Stale => None,
    };
    let out = {
        let mut inner = app.inner.write().await;
        if let Some(id) = only {
            let s = inner
                .sessions
                .get(&id)
                .ok_or_else(|| crate::state::no_such_session(id))?;
            if !running(s) {
                anyhow::bail!(
                    "session {} is not running — resume it from the archive instead",
                    crate::model::short_id(&id)
                );
            }
        }
        let mut out = Queued::default();
        for s in inner.sessions.values_mut() {
            if only.is_some_and(|id| id != s.id)
                || !running(s)
                || (which == Which::Stale && !s.agent_stale)
            {
                continue;
            }
            s.restart_queued = true;
            out.queued += 1;
            if s.state.safe_to_restart() {
                out.now += 1;
            }
        }
        out
    };
    tracing::info!(
        queued = out.queued,
        now = out.now,
        "restart asked for; the rest go when their turn ends"
    );
    app.notify().await;
    Ok(out)
}

/// Take a session back out of the queue.
pub async fn cancel(app: &Arc<AppState>, id: SessionId) -> Result<()> {
    let was = app
        .with_session(id, |s| std::mem::replace(&mut s.restart_queued, false))
        .await
        .ok_or_else(|| crate::state::no_such_session(id))?;
    if was {
        app.notify().await;
    }
    Ok(())
}

/// Respawn every flagged session that is safe to interrupt now, one at a time.
///
/// Called by the watcher on every snapshot, which is every state change — so a
/// session queued mid-turn goes the moment its `Stop` lands, with no poll to wait
/// out. It returns when nothing flagged is ready, which is the common case and
/// costs one read lock.
///
/// **Skipped while a swap runs, and held against one while it works.** A swap
/// decides which conversations follow which branch before it moves anything, and
/// a restart landing in the middle would be a session it did not account for.
/// Leaving the queue alone costs nothing: the swap notifies when it lands, and
/// that snapshot is the next look.
pub async fn run_due(app: &Arc<AppState>) {
    loop {
        let Some((id, workspace)) = next_due(app).await else {
            return;
        };
        let Ok(_swap) = app.swapping.try_lock() else {
            return;
        };
        tracing::info!(session = %id, "restarting on the installed claude");
        let outcome = crate::spawn::relocate_session(app, id, &workspace, GRACE).await;
        /* **Cleared whatever happened.** A respawn under the same id replaced the
        record already, so this finds `false` and changes nothing. A fork left the
        old record behind as archived, and a failure left it closed — and a flag on
        either would be a row promising a restart that can never come, since
        neither is live. */
        app.with_session(id, |s| s.restart_queued = false).await;
        match outcome {
            Ok(r) if r.degraded => tracing::warn!(
                session = %id,
                forked = %r.id,
                "the resume did not stay up, so it was forked instead"
            ),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(session = %id, "restart failed, the conversation is resumable: {e:#}")
            }
        }
        app.notify().await;
    }
}

/// Whether there is an agent here to restart.
///
/// **A live pty, not merely a handle.** A session killed a moment ago still holds
/// its handle, and its state is still whatever it was, until the exit watcher
/// lands — so "has a pty" queued a closed session, and the respawn then resumed a
/// conversation you had just closed.
fn running(s: &crate::model::Session) -> bool {
    s.state.is_live() && s.pty.as_ref().is_some_and(|h| h.is_alive())
}

/// The oldest flagged session that is safe to interrupt, and where it lives.
///
/// Oldest first, so a burst of `Stop`s restarts in the order the sessions were
/// opened rather than in hash order — the rail is ordered that way, and a restart
/// that jumps around the list is harder to follow than one that walks it.
async fn next_due(app: &Arc<AppState>) -> Option<(SessionId, String)> {
    let inner = app.inner.read().await;
    inner
        .sessions
        .values()
        .filter(|s| s.restart_queued && running(s) && s.state.safe_to_restart())
        .min_by_key(|s| s.created_at)
        .map(|s| (s.id, s.workspace.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Session, State, TurnReason};

    /// A session with a real pty behind it, in `state`. `cat` stands in for the
    /// agent: the queue only asks whether a pty exists, and a restart is the e2e
    /// flow's to drive, because only there is a `claude` to respawn.
    async fn live(app: &Arc<AppState>, dir: &std::path::Path, state: State) -> SessionId {
        let id = uuid::Uuid::new_v4();
        let pty = orchd_base::pty::PtyHandle::spawn(&["cat".to_string()], dir, &[], &[], (24, 80))
            .expect("spawn cat")
            .handle;
        let mut s = Session::new(id, "main".to_string(), dir.to_path_buf(), None);
        s.set_state(state);
        s.pty = Some(pty);
        app.inner.write().await.sessions.insert(id, s);
        id
    }

    fn turn(reason: TurnReason) -> State {
        State::YourTurn {
            since: std::time::SystemTime::now(),
            reason,
        }
    }

    /// The rule the whole feature turns on, row by row.
    ///
    /// **The two questions are the half with power.** A finished turn and a
    /// working one are obvious either way; what a careless rule gets wrong is a
    /// session waiting on your answer, which *looks* idle and would lose the
    /// question on a `--resume`.
    #[test]
    fn a_session_is_safe_to_restart_only_at_its_prompt() {
        for (state, want) in [
            (turn(TurnReason::TurnComplete), true),
            (turn(TurnReason::Ready), true),
            (turn(TurnReason::Interrupted), true),
            (turn(TurnReason::AskedAQuestion), false),
            (turn(TurnReason::NeedsPermission), false),
            (State::Working, false),
            (State::Starting, false),
            (
                State::BuildFailing {
                    summary: String::new(),
                },
                true,
            ),
            (
                State::Error {
                    message: String::new(),
                },
                true,
            ),
            (State::Exited, false),
            (State::Archived { resumable: true }, false),
        ] {
            assert_eq!(state.safe_to_restart(), want, "{state:?}");
        }
    }

    /// "Restart all" flags every live session and says how many go now.
    #[tokio::test]
    async fn the_queue_takes_every_live_session_and_counts_the_ready_ones() {
        let (app, dir) = crate::testutil::app("restart-queue");
        let idle = live(&app, &dir, turn(TurnReason::TurnComplete)).await;
        let busy = live(&app, &dir, State::Working).await;
        let asking = live(&app, &dir, turn(TurnReason::AskedAQuestion)).await;

        let got = queue(&app, Which::All).await.unwrap();
        assert_eq!(got, Queued { queued: 3, now: 1 });

        let inner = app.inner.read().await;
        for id in [idle, busy, asking] {
            assert!(inner.sessions[&id].restart_queued, "{id} is queued");
        }
    }

    /// The one it may restart first is the one at its prompt, and never the one
    /// holding a question.
    #[tokio::test]
    async fn next_due_skips_a_session_that_is_not_safe_yet() {
        let (app, dir) = crate::testutil::app("restart-due");
        let _asking = live(&app, &dir, turn(TurnReason::AskedAQuestion)).await;
        let _busy = live(&app, &dir, State::Working).await;
        queue(&app, Which::All).await.unwrap();
        assert!(next_due(&app).await.is_none(), "nothing is ready");

        let idle = live(&app, &dir, turn(TurnReason::TurnComplete)).await;
        queue(&app, Which::One(idle)).await.unwrap();
        assert_eq!(next_due(&app).await.map(|(id, _)| id), Some(idle));
    }

    /// The bar's restart takes only what the upgrade left behind.
    #[tokio::test]
    async fn stale_takes_only_the_sessions_on_an_old_build() {
        let (app, dir) = crate::testutil::app("restart-stale");
        let old = live(&app, &dir, turn(TurnReason::TurnComplete)).await;
        let new = live(&app, &dir, turn(TurnReason::TurnComplete)).await;
        app.with_session(old, |s| s.agent_stale = true).await;

        assert_eq!(
            queue(&app, Which::Stale).await.unwrap(),
            Queued { queued: 1, now: 1 }
        );
        let inner = app.inner.read().await;
        assert!(inner.sessions[&old].restart_queued);
        assert!(
            !inner.sessions[&new].restart_queued,
            "a session already on the new build is left alone"
        );
    }

    /// A session that is not running is refused by id, not silently counted.
    #[tokio::test]
    async fn a_session_with_no_pty_is_refused_by_name() {
        let (app, dir) = crate::testutil::app("restart-dead");
        let id = uuid::Uuid::new_v4();
        let mut s = Session::new(id, "main".to_string(), dir.clone(), None);
        s.set_state(State::Archived { resumable: true });
        app.inner.write().await.sessions.insert(id, s);

        let err = queue(&app, Which::One(id)).await.unwrap_err().to_string();
        assert!(err.contains("not running"), "{err}");
        assert!(!app.inner.read().await.sessions[&id].restart_queued);
    }

    #[tokio::test]
    async fn cancel_takes_it_back_out() {
        let (app, dir) = crate::testutil::app("restart-cancel");
        let id = live(&app, &dir, State::Working).await;
        queue(&app, Which::One(id)).await.unwrap();
        cancel(&app, id).await.unwrap();
        assert!(!app.inner.read().await.sessions[&id].restart_queued);
    }
}
