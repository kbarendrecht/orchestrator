//! Quit the idle agents of a checkout folded away on the rail, and resume them
//! when it unfolds.
//!
//! **What it buys is memory.** Each agent holds 300 to 500 MB at its prompt, and
//! a folded checkout is one you are not looking at. `sleep_folded_after_minutes`
//! is the wait, so a fold to tidy the rail for a moment costs nothing.
//!
//! **The fold is the page's, and the daemon only hears about it.** It lives in the
//! browser beside the rail's other view state, so the page posts `/api/fold`
//! whenever the snapshot's `folded` disagrees with it. That covers a restarted
//! daemon too, which starts at [`Fold::Unknown`].
//!
//! **Only a session at its prompt goes**, by the rule a restart uses
//! ([`crate::model::State::safe_to_restart`]): a busy one goes when its turn ends,
//! and one holding a question or a permission prompt waits for the answer. A pass
//! (a fix run, a review) is left alone, because it ends by itself.
//!
//! **A quit here is not the end of the session.** The exit watcher reads
//! [`crate::model::Session::asleep`] and skips what a last exit owes a workspace:
//! the drawer's processes keep running and main is not parked on its base, since
//! the conversation is coming back to both.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::model::{Session, SessionId};
use crate::state::{AppState, Fold};

/// Take what the page says about the fold.
///
/// Answers the sessions an unfold owes a resume, oldest first; the caller
/// resumes them, because a resume is a spawn and this module sits below that.
/// A fold that was already a fold keeps its first instant, so a page posting
/// it twice does not restart the wait.
pub async fn set_folded(app: &Arc<AppState>, folded: bool) -> Vec<SessionId> {
    let was = {
        let mut fold = app.fold.lock().unwrap_or_else(|e| e.into_inner());
        let was = *fold;
        *fold = match (folded, was) {
            (true, Fold::Folded(at)) => Fold::Folded(at),
            (true, _) => Fold::Folded(Instant::now()),
            (false, _) => Fold::Open,
        };
        was
    };
    if folded {
        if !matches!(was, Fold::Folded(_)) {
            arm(app);
        }
        app.notify().await;
        return Vec::new();
    }
    let owed = {
        let inner = app.inner.read().await;
        let mut owed: Vec<&Session> = inner.sessions.values().filter(|s| s.asleep).collect();
        owed.sort_by_key(|s| s.created_at);
        owed.iter().map(|s| s.id).collect()
    };
    app.notify().await;
    owed
}

/// Whether the page has folded this checkout since `owed` was taken. A resume
/// in progress stops there and leaves the rest asleep.
pub fn folded_again(app: &AppState) -> bool {
    matches!(
        *app.fold.lock().unwrap_or_else(|e| e.into_inner()),
        Fold::Folded(_)
    )
}

/// Wake the watcher when the wait runs out. [`run_due`] decides then, against
/// the fold as it stands, so an unfold in between makes this a no-op.
fn arm(app: &Arc<AppState>) {
    let minutes = app.settings().sleep_folded_after_minutes;
    if minutes == 0 {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(wait(minutes)).await;
        app.notify().await;
    });
}

fn wait(minutes: u32) -> Duration {
    Duration::from_secs(u64::from(minutes) * 60)
}

/// Quit every session that may go now, if the fold has lasted long enough.
///
/// Called by the watcher on every snapshot, like [`crate::restart::run_due`], so
/// a busy session goes the moment its `Stop` lands. Costs a mutex read when
/// nothing is folded, which is the common case.
pub async fn run_due(app: &Arc<AppState>) {
    let since = match *app.fold.lock().unwrap_or_else(|e| e.into_inner()) {
        Fold::Folded(at) => at,
        Fold::Open | Fold::Unknown => return,
    };
    let minutes = app.settings().sleep_folded_after_minutes;
    if minutes == 0 || since.elapsed() < wait(minutes) {
        return;
    }
    let quitting: Vec<_> = {
        let mut inner = app.inner.write().await;
        inner
            .sessions
            .values_mut()
            .filter(|s| sleepable(s))
            .filter_map(|s| {
                s.asleep = true;
                s.pty.clone()
            })
            .collect()
    };
    if quitting.is_empty() {
        return;
    }
    tracing::info!(
        sessions = quitting.len(),
        "quitting idle sessions of a folded checkout; they resume when it unfolds"
    );
    for pty in quitting {
        tokio::spawn(async move { pty.kill_gracefully().await });
    }
    // Persists the flag before the exits land, so a crash in between still knows
    // which sessions an unfold owes.
    app.notify().await;
}

/// Whether a session may be quit for the fold now.
///
/// `had_a_turn`, because the exit watcher forgets a turnless session outright and
/// there would be nothing to resume. Not one queued for a restart: that respawn
/// would race the quit.
fn sleepable(s: &Session) -> bool {
    !s.asleep
        && s.pass.is_none()
        && s.had_a_turn
        && !s.restart_queued
        && s.state.safe_to_restart()
        && s.pty.as_ref().is_some_and(|h| h.is_alive())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{State, TurnReason};

    async fn live(app: &Arc<AppState>, dir: &std::path::Path, state: State) -> SessionId {
        let id = uuid::Uuid::new_v4();
        let pty = orchd_base::pty::PtyHandle::spawn(&["cat".to_string()], dir, &[], &[], (24, 80))
            .expect("spawn cat")
            .handle;
        let mut s = Session::new(id, "main".to_string(), dir.to_path_buf(), None);
        s.set_state(state);
        s.had_a_turn = true;
        s.pty = Some(pty);
        app.inner.write().await.sessions.insert(id, s);
        id
    }

    fn idle() -> State {
        State::YourTurn {
            since: std::time::SystemTime::now(),
            reason: TurnReason::TurnComplete,
        }
    }

    fn fold_long_ago(app: &AppState) {
        *app.fold.lock().unwrap() = Fold::Folded(Instant::now() - wait(60));
    }

    /// The rule the feature turns on: after the wait, the idle session goes and
    /// the busy one and the one holding a question stay.
    #[tokio::test]
    async fn only_a_session_at_its_prompt_goes_to_sleep() {
        let (app, dir) = crate::testutil::app("sleep-due");
        let at_prompt = live(&app, &dir, idle()).await;
        let busy = live(&app, &dir, State::Working).await;
        let asking = live(
            &app,
            &dir,
            State::YourTurn {
                since: std::time::SystemTime::now(),
                reason: TurnReason::AskedAQuestion,
            },
        )
        .await;
        fold_long_ago(&app);

        run_due(&app).await;

        let inner = app.inner.read().await;
        assert!(inner.sessions[&at_prompt].asleep);
        assert!(!inner.sessions[&busy].asleep, "a turn is never cut off");
        assert!(
            !inner.sessions[&asking].asleep,
            "a question keeps its session"
        );
    }

    /// A fold younger than the wait, an open checkout, and a wait of `0` all quit
    /// nothing. The second is the half with power: an unfold cancels the timer
    /// only because `run_due` reads the fold again when it wakes.
    #[tokio::test]
    async fn nothing_sleeps_before_the_wait_or_after_an_unfold() {
        let (app, dir) = crate::testutil::app("sleep-wait");
        let id = live(&app, &dir, idle()).await;

        assert!(set_folded(&app, true).await.is_empty());
        run_due(&app).await;
        assert!(
            !app.inner.read().await.sessions[&id].asleep,
            "folded a moment ago"
        );

        fold_long_ago(&app);
        set_folded(&app, false).await;
        run_due(&app).await;
        assert!(!app.inner.read().await.sessions[&id].asleep, "unfolded");

        app.replace_settings(crate::config::Settings {
            sleep_folded_after_minutes: 0,
            ..app.settings()
        });
        fold_long_ago(&app);
        run_due(&app).await;
        assert!(!app.inner.read().await.sessions[&id].asleep, "turned off");
    }

    /// Folding twice keeps the first instant, so a page that posts the fold on
    /// every reconnect does not push the quit back forever.
    #[tokio::test]
    async fn a_second_fold_keeps_the_first_instant() {
        let (app, _dir) = crate::testutil::app("sleep-twice");
        fold_long_ago(&app);
        let first = *app.fold.lock().unwrap();
        set_folded(&app, true).await;
        assert_eq!(*app.fold.lock().unwrap(), first);
    }

    /// An unfold owes a resume to every session the fold quit, oldest first.
    #[tokio::test]
    async fn an_unfold_names_the_sleeping_sessions_oldest_first() {
        let (app, dir) = crate::testutil::app("sleep-wake");
        let first = live(&app, &dir, idle()).await;
        let second = live(&app, &dir, idle()).await;
        let _awake = live(&app, &dir, State::Working).await;
        {
            let mut inner = app.inner.write().await;
            inner.sessions.get_mut(&first).unwrap().asleep = true;
            inner.sessions.get_mut(&second).unwrap().asleep = true;
            inner.sessions.get_mut(&second).unwrap().created_at += Duration::from_secs(1);
        }
        assert_eq!(set_folded(&app, false).await, vec![first, second]);
    }
}
