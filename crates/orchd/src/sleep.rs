//! Quit agents idle at their prompt, and resume them later.
//!
//! **What it buys is memory.** Each agent holds 300 to 500 MB at its prompt, more
//! with its MCP servers. Two clocks decide: a checkout folded away on the rail for
//! `sleep_folded_after_minutes` puts all its agents to sleep, and one agent idle
//! for `sleep_idle_after_minutes` sleeps on its own. The row stays and says
//! sleeping; an unfold or Resume in its pane brings it back.
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

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

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

/// Quit every session that may go now: all of a checkout folded long enough, and
/// any one idle at its prompt past `sleep_idle_after_minutes`.
///
/// Called by the watcher on every snapshot, like [`crate::restart::run_due`], so
/// a busy session goes the moment its `Stop` lands, and by a one-minute tick,
/// because an idle session sends no snapshots. Costs a read lock when nothing is
/// due; the `ps` for the child check runs only when something is, and its answer
/// is reused for [`CHILDREN_FRESH`].
pub async fn run_due(app: &Arc<AppState>) {
    let settings = app.settings();
    let fold_due = match *app.fold.lock().unwrap_or_else(|e| e.into_inner()) {
        Fold::Folded(at) => {
            settings.sleep_folded_after_minutes > 0
                && at.elapsed() >= wait(settings.sleep_folded_after_minutes)
        }
        Fold::Open | Fold::Unknown => false,
    };
    let idle_after =
        (settings.sleep_idle_after_minutes > 0).then(|| wait(settings.sleep_idle_after_minutes));
    if !fold_due && idle_after.is_none() {
        return;
    }
    let due = |s: &Session, now: SystemTime| {
        sleepable(s) && (fold_due || idle_after.is_some_and(|w| idle_for(s, now) >= w))
    };
    let pids: Vec<u32> = {
        let now = SystemTime::now();
        let inner = app.inner.read().await;
        inner
            .sessions
            .values()
            .filter(|s| due(s, now))
            .filter_map(|s| s.pid)
            .collect()
    };
    if pids.is_empty() {
        return;
    }
    // No opinion is "awake": a `ps` that fails must never be what kills work.
    let Some(working) = children_at_work(pids).await else {
        return;
    };
    let quitting: Vec<_> = {
        let now = SystemTime::now();
        let mut inner = app.inner.write().await;
        inner
            .sessions
            .values_mut()
            .filter(|s| due(s, now) && s.pid.is_some_and(|p| !working.contains(&p)))
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
        folded = fold_due,
        "quitting sessions idle at their prompt; an unfold or a click on the row resumes them"
    );
    for pty in quitting {
        tokio::spawn(async move { pty.kill_gracefully().await });
    }
    // Persists the flag before the exits land, so a crash in between still knows
    // which sessions are owed a resume.
    app.notify().await;
}

/// How long a session has been left alone: since its state last changed or
/// input last went to it, whichever is later. A keystroke into the prompt box
/// fires no hook, so the pty's own clock is what keeps a draft awake.
fn idle_for(s: &Session, now: SystemTime) -> Duration {
    let input = s.pty.as_ref().and_then(|h| h.last_input());
    let last = input.map_or(s.state_since, |i| i.max(s.state_since));
    now.duration_since(last).unwrap_or_default()
}

/// Whether a session may be quit now, whatever asked.
///
/// `had_a_turn`, because the exit watcher forgets a turnless session outright and
/// there would be nothing to resume. Not one queued for a restart: that respawn
/// would race the quit. **Nothing left running in the background**: a monitor, a
/// background agent or a scheduled wake-up (`left_kinds`), or a shell such as a
/// dev server (`background_shell`) — a quit kills them all, and a `/loop` would
/// never fire again. A finished task wakes the agent into a new `Stop`, which
/// is what keeps both lists current.
fn sleepable(s: &Session) -> bool {
    !s.asleep
        && s.pass.is_none()
        && s.had_a_turn
        && !s.restart_queued
        && s.left_kinds.is_empty()
        && !s.background_shell
        && s.state.safe_to_restart()
        && s.pty.as_ref().is_some_and(|h| h.is_alive())
}

/// How long a `ps` answer about child processes is reused.
const CHILDREN_FRESH: Duration = Duration::from_secs(30);

/// The last answer, by when it was taken. Process-wide, because `run_due` runs on
/// every snapshot and a session kept awake by a child would otherwise cost a
/// `ps` per snapshot for as long as it stays that way.
static CHILDREN: std::sync::Mutex<Option<Checked>> = std::sync::Mutex::new(None);

/// One `ps` answer: when, which agents it looked at, and which of them had work.
/// Reused only for agents it looked at: one it did not would read as idle.
struct Checked {
    at: Instant,
    asked: HashSet<u32>,
    working: HashSet<u32>,
}

/// Which of `agents` have a child that is doing work, or `None` when `ps` could
/// not say.
///
/// **No hook fires for a `! command`**, so a foreground `! npm run dev` leaves the
/// session looking idle at its prompt. Its process is the tell: an agent's MCP
/// servers start with it, and anything started a minute or more after them —
/// a `! command`, a tool still running — is work. A restarted MCP server reads
/// as work too, which only keeps a session awake; it can never kill one.
///
/// `ps -A -o pid=,ppid=,etime=` is POSIX, on Linux and on macOS's BSD `ps`.
async fn children_at_work(agents: Vec<u32>) -> Option<HashSet<u32>> {
    if let Some(c) = CHILDREN.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if c.at.elapsed() < CHILDREN_FRESH && agents.iter().all(|a| c.asked.contains(a)) {
            return Some(c.working.clone());
        }
    }
    let out = crate::proc::run_blocking("reading the agents' child processes", || {
        std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,etime="])
            .output()
    })
    .await
    .ok()?
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let rows = parse_ps(&String::from_utf8_lossy(&out.stdout));
    let working: HashSet<u32> = agents
        .iter()
        .copied()
        .filter(|a| child_at_work(&rows, *a))
        .collect();
    *CHILDREN.lock().unwrap_or_else(|e| e.into_inner()) = Some(Checked {
        at: Instant::now(),
        asked: agents.into_iter().collect(),
        working: working.clone(),
    });
    Some(working)
}

/// `(pid, ppid, age)` per line of `ps -o pid=,ppid=,etime=`; a line that does
/// not parse is left out.
fn parse_ps(text: &str) -> Vec<(u32, u32, Duration)> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let age = parse_etime(it.next()?)?;
            Some((pid, ppid, age))
        })
        .collect()
}

/// `[[dd-]hh:]mm:ss`, the one `etime` format both `ps`es print.
fn parse_etime(t: &str) -> Option<Duration> {
    let (days, rest) = match t.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, t),
    };
    let parts: Vec<u64> = rest
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let secs = match parts.as_slice() {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some(Duration::from_secs(days * 86_400 + secs))
}

/// Whether `agent` has a child started a minute or more after its oldest one.
fn child_at_work(rows: &[(u32, u32, Duration)], agent: u32) -> bool {
    let ages: Vec<Duration> = rows
        .iter()
        .filter(|(_, ppid, _)| *ppid == agent)
        .map(|(_, _, age)| *age)
        .collect();
    let Some(oldest) = ages.iter().max() else {
        return false;
    };
    ages.iter()
        .any(|age| *age + Duration::from_secs(60) <= *oldest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{State, TurnReason};

    async fn live(app: &Arc<AppState>, dir: &std::path::Path, state: State) -> SessionId {
        let id = uuid::Uuid::new_v4();
        let spawned =
            orchd_base::pty::PtyHandle::spawn(&["cat".to_string()], dir, &[], &[], (24, 80))
                .expect("spawn cat");
        let mut s = Session::new(id, "main".to_string(), dir.to_path_buf(), None);
        s.set_state(state);
        s.had_a_turn = true;
        s.pid = spawned.pid;
        s.pty = Some(spawned.handle);
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

    /// Leave a session alone for `ago`, as far as the idle clock can tell.
    async fn idle_since(app: &Arc<AppState>, id: SessionId, ago: Duration) {
        app.with_session(id, |s| s.state_since = SystemTime::now() - ago)
            .await;
    }

    fn idle_limit(app: &AppState, minutes: u32) {
        app.replace_settings(crate::config::Settings {
            sleep_idle_after_minutes: minutes,
            ..app.settings()
        });
    }

    async fn asleep(app: &Arc<AppState>, id: SessionId) -> bool {
        app.inner.read().await.sessions[&id].asleep
    }

    /// An open checkout's session sleeps once it has been idle past the limit, and
    /// not before; `0` turns it off.
    #[tokio::test]
    async fn an_idle_session_sleeps_past_the_limit_and_not_before() {
        let (app, dir) = crate::testutil::app("sleep-idle");
        *app.fold.lock().unwrap() = Fold::Open;
        let old = live(&app, &dir, idle()).await;
        let recent = live(&app, &dir, idle()).await;
        idle_since(&app, old, wait(61)).await;
        idle_since(&app, recent, wait(59)).await;

        idle_limit(&app, 0);
        run_due(&app).await;
        assert!(!asleep(&app, old).await, "0 never sleeps");

        idle_limit(&app, 60);
        run_due(&app).await;
        assert!(asleep(&app, old).await, "idle past the limit");
        assert!(!asleep(&app, recent).await, "idle under the limit");
    }

    /// The rows that make this safe: anything left running in the background keeps
    /// a session awake, past the limit and in a fold alike.
    #[tokio::test]
    async fn background_work_keeps_a_session_awake() {
        let (app, dir) = crate::testutil::app("sleep-background");
        let shell = live(&app, &dir, idle()).await;
        let monitor = live(&app, &dir, idle()).await;
        let looping = live(&app, &dir, idle()).await;
        app.with_session(shell, |s| s.background_shell = true).await;
        app.with_session(monitor, |s| s.left_kinds = vec!["monitor".into()])
            .await;
        app.with_session(looping, |s| s.left_kinds = vec!["scheduled wake-up".into()])
            .await;
        for id in [shell, monitor, looping] {
            idle_since(&app, id, wait(600)).await;
        }
        fold_long_ago(&app);

        run_due(&app).await;

        for id in [shell, monitor, looping] {
            assert!(!asleep(&app, id).await, "{id} had background work");
        }
    }

    /// A keystroke into the prompt box restarts the idle clock: a draft is not a
    /// session nobody uses.
    #[tokio::test]
    async fn a_keystroke_restarts_the_idle_clock() {
        let (app, dir) = crate::testutil::app("sleep-typing");
        *app.fold.lock().unwrap() = Fold::Open;
        let id = live(&app, &dir, idle()).await;
        idle_since(&app, id, wait(600)).await;
        let pty = app.inner.read().await.sessions[&id].pty.clone().unwrap();
        pty.write(b"half a sentence").unwrap();

        run_due(&app).await;

        assert!(!asleep(&app, id).await);
    }

    #[test]
    fn etime_reads_every_shape_both_ps_print() {
        for (t, secs) in [
            ("00:07", 7),
            ("33:30", 33 * 60 + 30),
            ("01:02:03", 3723),
            ("2-01:02:03", 2 * 86_400 + 3723),
        ] {
            assert_eq!(parse_etime(t), Some(Duration::from_secs(secs)), "{t}");
        }
        assert_eq!(parse_etime("soon"), None);
    }

    /// A child started a minute after the MCP servers is work: what a `! command`
    /// or a running tool looks like. Children all as old as each other are not.
    #[test]
    fn a_child_younger_than_the_mcp_servers_is_work() {
        let rows = parse_ps(
            "  100     1  1:00:00\n  201   100    59:58\n  202   100    59:57\n  300     1  1:00:00\n  301   300    59:58\n  302   300    00:04\n  400     1    10:00\n",
        );
        assert!(!child_at_work(&rows, 100), "MCP servers only");
        assert!(
            child_at_work(&rows, 300),
            "a ! command started an hour later"
        );
        assert!(!child_at_work(&rows, 400), "no children at all");
    }
}
