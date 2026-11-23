//! Quit agents idle at their prompt, and resume them later.
//!
//! **What it buys is memory.** Each agent holds 300 to 500 MB at its prompt, more
//! with its MCP servers. An agent idle for `sleep_idle_after_minutes` is quit; its
//! row stays and says sleeping, and a press on the row, Resume in its pane, a
//! message or a `ListAgents` call brings it back. Folding a project on the rail is
//! a view and has no part in this.
//!
//! **Only a session at its prompt goes**, by the rule a restart uses
//! ([`crate::model::State::safe_to_restart`]): a busy one goes when its turn ends,
//! and one holding a question or a permission prompt waits for the answer. A pass
//! (a fix run, a review) is left alone, because it ends by itself.
//!
//! **Under memory pressure most of that gives way**, because one session lost is
//! better than a desktop the kernel or oomd takes down. Below
//! [`crate::headroom::FLOOR_MB`] available, one session goes per tick whatever its
//! idle time, cheapest to lose first ([`Shed`]): at its prompt, then holding a
//! question, then with background work, then mid-turn. Each comes back the same
//! way; a cut-off turn needs a "continue", and killed background work stays gone.
//! A pass still stays: its exit settles the run and frees the PR for another.
//!
//! **A quit here is not the end of the session.** The exit watcher reads
//! [`crate::model::Session::asleep`] and skips what a last exit owes a workspace:
//! the drawer's processes keep running and main is not parked on its base, since
//! the conversation is coming back to both.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::model::Session;
use crate::state::AppState;

fn wait(minutes: u32) -> Duration {
    Duration::from_secs(u64::from(minutes) * 60)
}

/// Quit every session idle at its prompt past `sleep_idle_after_minutes`.
///
/// Called by the watcher on every snapshot, like [`crate::restart::run_due`], so
/// a busy session goes the moment its `Stop` lands. Costs a read lock when
/// nothing is due; the `ps` for the child check runs only when something is, and
/// its answer is reused for [`CHILDREN_FRESH`].
pub async fn run_due(app: &Arc<AppState>) {
    let quitting = idle_past(app).await;
    quit(app, quitting).await;
}

/// [`run_due`] for the five-second tick, and under memory pressure the one
/// session cheapest to lose instead, however short its idle.
///
/// A tick because an idle session sends no snapshots and oomd acts after twenty
/// seconds of pressure. **Only the tick sheds, never a snapshot**: the quit's own
/// `notify` is a snapshot and so is every hook, so a shed driven by snapshots
/// picks the next session before the last one has let go of its memory, and
/// empties the rail in milliseconds.
pub async fn run_tick(app: &Arc<AppState>) {
    if app.settings().sleep_idle_after_minutes == 0 {
        return;
    }
    let quitting = if crate::headroom::below_floor() {
        shed_one(app).await
    } else {
        idle_past(app).await
    };
    quit(app, quitting).await;
}

type Pty = Arc<crate::pty::PtyHandle>;

async fn quit(app: &Arc<AppState>, quitting: Vec<Pty>) {
    if quitting.is_empty() {
        return;
    }
    for pty in quitting {
        tokio::spawn(async move { pty.kill_gracefully().await });
    }
    // Persists the flag before the exits land, so a crash in between still knows
    // which sessions are owed a resume.
    app.notify().await;
}

/// Mark every session idle at its prompt past the limit asleep, and hand back
/// their ptys to quit.
async fn idle_past(app: &Arc<AppState>) -> Vec<Pty> {
    let minutes = app.settings().sleep_idle_after_minutes;
    if minutes == 0 {
        return Vec::new();
    }
    let due = |s: &Session, now: SystemTime| sleepable(s) && idle_for(s, now) >= wait(minutes);
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
        return Vec::new();
    }
    // No opinion is "awake": a `ps` that fails must never be what kills work.
    let Some(working) = children_at_work(pids).await else {
        return Vec::new();
    };
    let quitting: Vec<Pty> = {
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
    if !quitting.is_empty() {
        tracing::info!(
            sessions = quitting.len(),
            "quitting sessions idle at their prompt; a press on the row resumes them"
        );
    }
    quitting
}

/// What quitting a session costs, cheapest first: the order memory pressure
/// takes them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Shed {
    /// At its prompt with nothing running: costs a resume.
    AtPrompt,
    /// Holding a question or a permission prompt, which Claude asks again.
    Asking,
    /// Idle with a dev server, a monitor, a `/loop` or a `! command`: that work
    /// is killed and does not come back.
    Background,
    /// Mid-turn: the turn is cut off.
    Busy,
}

fn shed_cost(s: &Session, child_at_work: bool) -> Shed {
    use crate::model::State;
    let asking = match &s.state {
        State::YourTurn { reason, .. } | State::BuildFailing { covers: reason, .. } => {
            reason.awaits_an_answer()
        }
        State::Error { .. } => false,
        State::Starting | State::Working | State::Exited | State::Archived { .. } => {
            return Shed::Busy;
        }
    };
    if child_at_work || s.background_shell || !s.left_kinds.is_empty() {
        Shed::Background
    } else if asking {
        Shed::Asking
    } else {
        Shed::AtPrompt
    }
}

/// Mark the one session cheapest to lose asleep, longest idle among equals, and
/// hand back its pty to quit.
///
/// **One per call**: each quit frees 300 to 500 MB, so the next tick asks again
/// rather than emptying the rail. A wake is refused until there is room for a
/// session above the floor, so a sleeper is not woken straight back into the
/// pressure that put it to sleep.
async fn shed_one(app: &Arc<AppState>) -> Vec<Pty> {
    let pids: HashSet<u32> = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .values()
            .filter(|s| sheddable(s))
            .filter_map(|s| s.pid)
            .collect()
    };
    if pids.is_empty() {
        return Vec::new();
    }
    // Here no opinion is "nothing at work": it only orders who goes, nothing
    // waiting on `ps` may keep the machine full, and the known background work
    // still ranks; only a `! command` is missed.
    let working = children_at_work(pids.iter().copied().collect())
        .await
        .unwrap_or_default();
    let now = SystemTime::now();
    let mut inner = app.inner.write().await;
    // Only a session `ps` was asked about: one that came up since would read idle.
    let Some(s) = inner
        .sessions
        .values_mut()
        .filter(|s| sheddable(s) && s.pid.is_some_and(|p| pids.contains(&p)))
        .min_by_key(|s| {
            let at_work = s.pid.is_some_and(|p| working.contains(&p));
            (shed_cost(s, at_work), std::cmp::Reverse(idle_for(s, now)))
        })
    else {
        return Vec::new();
    };
    s.asleep = true;
    tracing::warn!(session = %s.id, "memory under the floor, quitting a session; a press on the row resumes it");
    s.pty.clone().into_iter().collect()
}

/// How long a session has been left alone: since its state last changed or
/// input last went to it, whichever is later. A keystroke into the prompt box
/// fires no hook, so the pty's own clock is what keeps a draft awake.
fn idle_for(s: &Session, now: SystemTime) -> Duration {
    let input = s.pty.as_ref().and_then(|h| h.last_input());
    let last = input.map_or(s.state_since, |i| i.max(s.state_since));
    now.duration_since(last).unwrap_or_default()
}

/// Whether a session may be quit at all: a live agent with a conversation to
/// come back to.
///
/// `had_a_turn`, because the exit watcher forgets a turnless session outright and
/// there would be nothing to resume. Not one queued for a restart: that respawn
/// would race the quit. Not a pass: its exit settles the run and frees the PR,
/// so a second run could start on it while this one waits to be woken.
fn sheddable(s: &Session) -> bool {
    !s.asleep
        && s.pass.is_none()
        && s.had_a_turn
        && !s.restart_queued
        && s.pty.as_ref().is_some_and(|h| h.is_alive())
}

/// Whether a session may be quit for being idle.
///
/// **Nothing left running in the background**: a monitor, a background agent or
/// a scheduled wake-up (`left_kinds`), or a shell such as a dev server
/// (`background_shell`) — a quit kills them all, and a `/loop` would never fire
/// again. A finished task wakes the agent into a new `Stop`, which is what keeps
/// both lists current. The `! command` check is the caller's, since it costs a
/// `ps`.
fn sleepable(s: &Session) -> bool {
    sheddable(s) && shed_cost(s, false) == Shed::AtPrompt
}

/// Which sessions sleep in each *other* open project: the project's folder name,
/// and each sleeper as `ListAgents` would name it without its suffix — its
/// worktree, or the folder name for one in main.
///
/// For the line `ListAgents` gets: a sleeping session has no process, so Claude
/// Code's own list cannot show it, and this daemon only knows its own. The other
/// projects' daemons persist `asleep` in their `sessions.json`, under the host's
/// `checkouts/<key>`, so this **reads** them; it writes nothing that is not its own.
/// Blocking: call it off the runtime. A file that will not read counts as none.
pub fn asleep_elsewhere(own: &std::path::Path) -> Vec<(String, Vec<String>)> {
    let Some(host) = host_dir() else {
        return Vec::new();
    };
    asleep_under(&host, own)
}

fn host_dir() -> Option<std::path::PathBuf> {
    let here = crate::config::Config::config_dir().ok()?;
    Some(crate::instance::host_config_dir(&here))
}

fn read_json(p: std::path::PathBuf) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

/// The other open projects, each with its folder name and its state directory.
fn other_projects(
    host: &std::path::Path,
    own: &std::path::Path,
) -> Vec<(String, std::path::PathBuf)> {
    let Some(open) = read_json(host.join("host.json")) else {
        return Vec::new();
    };
    open.get("checkouts")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str().map(std::path::PathBuf::from))
        .filter(|c| c != own)
        .filter_map(|c| {
            let name = c.file_name()?.to_string_lossy().into_owned();
            Some((
                name,
                host.join("checkouts")
                    .join(crate::instance::checkout_key(&c)),
            ))
        })
        .collect()
}

/// [`asleep_elsewhere`] under a given host config dir, so a test needs no
/// process-wide `ORCHD_CONFIG_DIR`.
fn asleep_under(host: &std::path::Path, own: &std::path::Path) -> Vec<(String, Vec<String>)> {
    other_projects(host, own)
        .into_iter()
        .filter_map(|(project, dir)| {
            let records = read_json(dir.join("sessions.json"))?;
            let names: Vec<String> = records
                .as_array()?
                .iter()
                .filter(|r| r.get("asleep").and_then(|a| a.as_bool()) == Some(true))
                .filter_map(|r| r.get("workspace")?.as_str())
                .map(|ws| {
                    if ws == "main" {
                        project.clone()
                    } else {
                        ws.to_string()
                    }
                })
                .collect();
            (!names.is_empty()).then_some((project, names))
        })
        .collect()
}

/// Each other open project's daemon port, read off the hook settings it wrote:
/// every HTTP hook there names `http://127.0.0.1:<port>/hooks/…`. For waking a
/// sleeper over there; a project whose file will not read is left out.
pub fn other_ports(own: &std::path::Path) -> Vec<(String, u16)> {
    let Some(host) = host_dir() else {
        return Vec::new();
    };
    ports_under(&host, own)
}

fn ports_under(host: &std::path::Path, own: &std::path::Path) -> Vec<(String, u16)> {
    other_projects(host, own)
        .into_iter()
        .filter_map(|(project, dir)| {
            let raw = std::fs::read_to_string(dir.join("hooks.json")).ok()?;
            let rest = raw.split("http://127.0.0.1:").nth(1)?;
            let port = rest.split('/').next()?.parse().ok()?;
            Some((project, port))
        })
        .collect()
}

/// The workspace a `ListAgents` name points at: the name less its two-character
/// suffix (`striped-rambling-hippo-3a`), and `main` when that is the project's
/// own folder name, which is what Claude Code calls a session in main. `None`
/// for anything without the suffix — a ref like `[3c0ef9]` names a process, and
/// a sleeper has none.
pub fn workspace_of_name(name: &str, checkout: &std::path::Path) -> Option<String> {
    let base = base_of_name(name)?;
    let project = checkout.file_name()?.to_string_lossy();
    Some(if base == project {
        "main".to_string()
    } else {
        base.to_string()
    })
}

/// A `ListAgents` name less its two-character suffix: what [`asleep_elsewhere`]
/// lists a sleeper as.
pub fn base_of_name(name: &str) -> Option<&str> {
    let (base, suffix) = name.rsplit_once('-')?;
    (suffix.len() == 2 && suffix.chars().all(|c| c.is_ascii_hexdigit()) && !base.is_empty())
        .then_some(base)
}

/// The line `ListAgents` is given, or `None` when there is nothing to say.
pub fn list_note(
    woken: usize,
    waking: usize,
    elsewhere: &[(String, Vec<String>)],
) -> Option<String> {
    let plural = |n: usize| if n == 1 { "session" } else { "sessions" };
    let mut lines = Vec::new();
    if woken > 0 {
        lines.push(format!(
            "orchd woke {woken} sleeping {} in this project for this list.",
            plural(woken)
        ));
    }
    if waking > 0 {
        lines.push(format!(
            "{waking} more {} still waking; list again in a few seconds.",
            if waking == 1 { "is" } else { "are" }
        ));
    }
    if !elsewhere.is_empty() {
        let each: Vec<String> = elsewhere
            .iter()
            .map(|(project, names)| format!("{project}: {}", names.join(", ")))
            .collect();
        lines.push(format!(
            "Also asleep in other projects, not listed: {}. A SendMessage to one by that name wakes it.",
            each.join("; ")
        ));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
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
    use crate::model::{SessionId, State, TurnReason};

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

    /// The rule the feature turns on: past the limit, the idle session goes and
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
        for id in [at_prompt, busy, asking] {
            idle_since(&app, id, wait(600)).await;
        }

        run_due(&app).await;

        let inner = app.inner.read().await;
        assert!(inner.sessions[&at_prompt].asleep);
        assert!(!inner.sessions[&busy].asleep, "a turn is never cut off");
        assert!(
            !inner.sessions[&asking].asleep,
            "a question keeps its session"
        );
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

    /// Under the floor the limit does not count: one session goes per pass,
    /// cheapest to lose first, and the longest idle among equals.
    #[tokio::test]
    async fn under_memory_pressure_the_cheapest_session_goes_first() {
        let (app, dir) = crate::testutil::app("sleep-pressure");
        let busy = live(&app, &dir, State::Working).await;
        let background = live(&app, &dir, idle()).await;
        app.with_session(background, |s| s.background_shell = true)
            .await;
        let asking = live(
            &app,
            &dir,
            State::YourTurn {
                since: SystemTime::now(),
                reason: TurnReason::AskedAQuestion,
            },
        )
        .await;
        let newer = live(&app, &dir, idle()).await;
        let older = live(&app, &dir, idle()).await;
        for (id, ago) in [
            (busy, 600),
            (background, 600),
            (asking, 600),
            (newer, 1),
            (older, 5),
        ] {
            idle_since(&app, id, wait(ago)).await;
        }
        idle_limit(&app, 60);

        let order = [older, newer, asking, background, busy];
        for n in 0..order.len() {
            let quitting = shed_one(&app).await;
            quit(&app, quitting).await;
            for (k, id) in order.iter().enumerate() {
                assert_eq!(asleep(&app, *id).await, k <= n, "pass {n}, session {k}");
            }
        }
    }

    /// The rows that make this safe: anything left running in the background keeps
    /// a session awake, however long it has been idle.
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

    /// The line `ListAgents` gets says only what is true, and nothing at all when
    /// nothing was woken and nobody sleeps elsewhere.
    #[test]
    fn the_list_note_says_what_happened_and_nothing_more() {
        assert_eq!(list_note(0, 0, &[]), None);
        let note = list_note(
            2,
            1,
            &[
                (
                    "scienta".into(),
                    vec!["dense-nudging-hollow".into(), "scienta".into()],
                ),
                ("werkenbij".into(), vec!["fix-x".into()]),
            ],
        )
        .unwrap();
        assert!(
            note.contains("woke 2 sleeping sessions in this project"),
            "{note}"
        );
        assert!(note.contains("1 more is still waking"), "{note}");
        assert!(
            note.contains("scienta: dense-nudging-hollow, scienta; werkenbij: fix-x"),
            "{note}"
        );
        assert_eq!(
            list_note(1, 0, &[]).as_deref(),
            Some("orchd woke 1 sleeping session in this project for this list.")
        );
    }

    /// Other projects' sleepers are counted from their own records, this project
    /// and a project with none are left out, and a closed project is not read.
    #[test]
    fn other_projects_sleepers_are_counted_from_their_records() {
        let host = std::env::temp_dir().join(format!("orchd-sleepers-{}", uuid::Uuid::new_v4()));
        let (own, other, quiet, closed) = (
            std::path::Path::new("/w/orchestrator"),
            std::path::Path::new("/w/scienta"),
            std::path::Path::new("/w/werkenbij"),
            std::path::Path::new("/w/old"),
        );
        let write = |checkout: &std::path::Path, records: &str| {
            let dir = host
                .join("checkouts")
                .join(crate::instance::checkout_key(checkout));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("sessions.json"), records).unwrap();
        };
        write(own, r#"[{"asleep":true,"workspace":"w"}]"#);
        write(
            other,
            r#"[{"asleep":true,"workspace":"dense"},{"asleep":true,"workspace":"main"},{"asleep":false,"workspace":"x"},{}]"#,
        );
        write(quiet, r#"[{"asleep":false,"workspace":"q"}]"#);
        write(closed, r#"[{"asleep":true,"workspace":"c"}]"#);
        std::fs::write(
            host.join("host.json"),
            r#"{"checkouts":["/w/orchestrator","/w/scienta","/w/werkenbij"]}"#,
        )
        .unwrap();

        assert_eq!(
            asleep_under(&host, own),
            vec![(
                "scienta".to_string(),
                vec!["dense".to_string(), "scienta".to_string()]
            )],
            "a sleeper in main goes by the project's folder name"
        );
        let hooks = host
            .join("checkouts")
            .join(crate::instance::checkout_key(other));
        std::fs::write(
            hooks.join("hooks.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"http","url":"http://127.0.0.1:41749/hooks/stop"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(
            ports_under(&host, own),
            vec![("scienta".to_string(), 41749)]
        );
        let _ = std::fs::remove_dir_all(&host);
    }

    /// A `ListAgents` name points at a worktree, or at main by the project's
    /// folder name; a ref or a name with no suffix points at nothing.
    #[test]
    fn a_list_name_points_at_its_workspace() {
        let checkout = std::path::Path::new("/w/scienta");
        for (name, want) in [
            ("striped-rambling-hippo-3a", Some("striped-rambling-hippo")),
            ("scienta-49", Some("main")),
            ("[3c0ef9]", None),
            ("plain", None),
            ("ends-in-word", None),
            ("-3a", None),
        ] {
            assert_eq!(workspace_of_name(name, checkout).as_deref(), want, "{name}");
        }
    }
}
