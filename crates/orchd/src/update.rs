//! Is the agent binary out of date, and one button to fix it.
//!
//! Claude Code prints its own "update available, run `mise upgrade …`" nag
//! *inside* the session's terminal, which is the wrong place twice over: it is
//! buried in a conversation, and acting on it means leaving the app for a shell.
//! Worse, that nag is the agent's stdout, and this daemon does not parse the
//! agent's stdout (`triage.rs` gives the reason — an agent can say anything and a
//! second source of truth is a worse one). So the fact is fetched from `mise`
//! instead, which is the thing that would perform the upgrade anyway.
//!
//! **The upgrade cannot interrupt a running session**, which is what makes a
//! button safe to offer. A running `claude` keeps executing the image it already
//! loaded, so sessions in flight finish on the old version and every new one gets
//! the new — no restart, no downtime, nothing to coordinate.
//!
//! Not quite for the reason first written here, and the difference is worth
//! keeping: mise does not leave the old versioned directory behind. It **deletes**
//! it, and the live processes read `/proc/<pid>/exe` as
//! `installs/claude-code/2.1.246/claude (deleted)` — they survive on an unlinked
//! inode, not on a directory that is still there. Nothing has broken on this, and
//! the reason to know it is `CLAUDE_CODE_EXECPATH`: anything that re-execs itself
//! by that path after an upgrade is pointing at a file that no longer exists.
//!
//! The visible consequence is smaller and bit a user first: every open session
//! keeps printing *its own* upgrade nag, because that process really is the old
//! build. An upgrade that reported nothing therefore read as an upgrade that did
//! not happen, which is why success says so now.
//!
//! The app has the same two halves, in this file because they are the same
//! shape: a poller that notices a newer build, and one runner that installs it.
//! See the "Upgrading the app" section below for what differs.

use anyhow::Result;
use std::path::Path;
use std::sync::Arc;

use crate::model::{AgentUpdate, Offer, UpdateInfo, UpgradeRun};
use crate::state::AppState;
use orchd_base::install::Install;

// ---------------------------------------------------------------------------
// The agent
// ---------------------------------------------------------------------------

/// Ask mise whether the agent is behind, in the checkout whose config decides it.
///
/// `None` covers every uninteresting answer — up to date, no mise, not a mise
/// project, `claude` not installed by mise at all — because this drives a nudge.
/// Something that cannot answer must be silent rather than shout.
pub fn check(main: &Path) -> Option<AgentUpdate> {
    let tool = agent_providing_tool(main)?;
    // `--json` rather than the table: the human output is columns of padded
    // text, and `{}` for "nothing outdated" is unambiguous where an empty table
    // is not.
    //
    // Named, rather than asking about every tool the checkout pins. Bare
    // `outdated` reaches each tool's own backend — seven registries here, one of
    // them answering 404 and warning about it every time — and the answer is
    // thrown away but for one line. Asked about the agent alone it is a single
    // lookup off mise's cache, which is what makes polling this often affordable.
    let out = mise(
        main,
        &["outdated", "--json", &tool],
        "the agent version check",
    )?;
    parse(&out.stdout, &tool)
}

/// How long a `mise` query may take before it is killed. These reach mise's
/// cache and, for `outdated`, the network once; a minute is generous for either
/// and short enough that a stuck registry does not park the poller for good.
const QUERY_TIMEOUT_SECS: u64 = 60;

/// Run a `mise` query, bounded, and hand back its output only on success.
///
/// Bounded the way network git is (`git::git_net`), and for the same reason: an
/// unbounded child on a dead network is a hang, and this one ran on the update
/// poller with nothing to end it. `None` on a failure of any kind, because every
/// caller treats "cannot answer" as "say nothing".
pub(crate) fn mise(main: &Path, args: &[&str], label: &str) -> Option<std::process::Output> {
    let argv: Vec<String> = std::iter::once("mise".to_string())
        .chain(args.iter().map(|a| (*a).to_string()))
        .collect();
    let out = crate::proc::run_bounded(main, QUERY_TIMEOUT_SECS, &argv, label)
        .map_err(|e| tracing::debug!("{label}: {e:#}"))
        .ok()?;
    out.status.success().then_some(out)
}

/// Which mise tool provides the `claude` a session would actually run.
///
/// Asked rather than guessed, and this is not pedantry — it is the difference
/// between a nudge that clears and one that does not. The same binary is pinned
/// under two names here: `claude-code` (what this repo pins) and `claude` (a
/// parent directory's config). Both were listed as outdated, upgrading one left
/// the other stale, and the bar came straight back asking for a tool whose
/// install is *shadowed on PATH and never executed*.
///
/// `mise which claude` resolves the whole ladder and answers with the real path,
/// whose `installs/<tool>/<version>/` component names the tool. Anything else —
/// no mise, a `claude` from npm — yields `None` and no nudge, which is right: this
/// cannot offer to upgrade something it does not know how to.
fn agent_providing_tool(main: &Path) -> Option<String> {
    let out = mise(
        main,
        &["which", "claude"],
        "resolving the agent's mise tool",
    )?;
    tool_of_install_path(String::from_utf8_lossy(&out.stdout).trim())
}

/// The directory mise installs every version under: `…/installs/<tool>/<version>/`.
///
/// Named once because two functions here read that layout and would otherwise
/// spell it twice: this one and [`stable_exe`], which swaps the version component
/// for `latest`.
const INSTALLS: &str = "installs";

/// The tool name out of a mise install path, e.g. `…/installs/claude-code/2.1/claude`.
///
/// **Only sound for the agent**, whose directory name is the name `mise upgrade`
/// accepts. Do not reach for this to name the *app*'s tool: a backend install
/// (`github:kbarendrecht/orchestrator`) lands in a directory mise spells
/// differently from the tool, which is why [`tool_owning`] asks mise instead of
/// reading the path.
fn tool_of_install_path(path: &str) -> Option<String> {
    let after = path.split(&format!("/{INSTALLS}/")).nth(1)?;
    let tool = after.split('/').next()?;
    (!tool.is_empty()).then(|| tool.to_string())
}

/// Split from [`check`] so the shapes mise emits can be tested without mise.
fn parse(stdout: &[u8], tool: &str) -> Option<AgentUpdate> {
    let v: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    // Absent from `outdated` is the ordinary happy case: the tool is current.
    let entry = v.get(tool)?;
    let current = entry.get("current")?.as_str()?.to_string();
    let latest = entry.get("latest")?.as_str()?.to_string();
    // mise lists a tool because *something* differs; an equal pair would be a
    // nudge offering to upgrade to what is already installed.
    if current == latest {
        return None;
    }
    Some(AgentUpdate {
        tool: tool.to_string(),
        current,
        latest,
    })
}

/// The cask and the package are both named this, and the name is the argument.
///
/// One constant because the two channels were published together and a rename
/// would have to move both — see CLAUDE.md's *Releases*.
const PACKAGE: &str = "orchestrator";

/// What an upgrade runs, and what to call it.
///
/// `argv` and `shown` are two different strings on purpose. An apt upgrade runs
/// `pkexec sh -c …` because it needs root, and showing that to somebody is showing
/// them the plumbing — what they would type is `sudo apt …`, and that is what the
/// bar and the tooltip say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    argv: Vec<String>,
    shown: String,
}

/// The command that upgrades this install, when one can be run from in here.
///
/// Returned rather than executed so the deadline, the cwd and the reporting all
/// live with the caller. Run in the main checkout, because that is the config mise
/// resolves the tool version from.
///
/// **Three channels can be upgraded and three cannot**, and the split is not about
/// effort. mise installs *beside* the running build; apt replaces the binary and
/// Linux keeps this process on its old inode; Homebrew replaces the bundle. All
/// three survive being upgraded under a running app, and all three need the restart
/// the bar already offers. A `.dmg`, an AppImage and a tarball have no installer to
/// ask at all — there is nothing to run, so nothing is offered.
fn plan(install: Install, tool: Option<&str>) -> Option<Plan> {
    // mise first, and by the tool rather than by the install: it is the one
    // channel that cannot be read off a path, and `app_providing_tool` has
    // already asked mise itself.
    if let Some(tool) = tool {
        return Some(Plan {
            argv: vec!["mise".into(), "upgrade".into(), tool.into()],
            shown: format!("mise upgrade {tool}"),
        });
    }
    match install {
        Install::Homebrew => Some(Plan {
            argv: vec![
                "brew".into(),
                "upgrade".into(),
                "--cask".into(),
                PACKAGE.into(),
            ],
            shown: format!("brew upgrade --cask {PACKAGE}"),
        }),
        // **`apt-get update` is not optional.** `--only-upgrade` can only install
        // what the package lists already carry, and the release this bar is
        // nudging about reached the apt repository minutes ago — so without the
        // refresh the upgrade succeeds at doing nothing.
        //
        // `pkexec` because every apt path needs root and this app has no terminal
        // to type a password into. `sh -c` because pkexec runs one command and
        // this is two.
        Install::Apt => Some(Plan {
            argv: vec![
                "pkexec".into(),
                "sh".into(),
                "-c".into(),
                format!("apt-get update && apt-get install -y --only-upgrade {PACKAGE}"),
            ],
            shown: format!("sudo apt install --only-upgrade {PACKAGE}"),
        }),
        Install::AppImage | Install::MacBundle | Install::Tarball | Install::Checkout => None,
    }
}

/// What the bar may offer for this install.
///
/// The daemon decides this, rather than the page deriving it from `tool` being
/// `None`: the page cannot tell a cask from a `.dmg`, and when it tried it told
/// every non-mise install to run `mise up`.
///
/// `have_pkexec` is the one runtime fact that turns a button into advice. An apt
/// install on a machine with no `pkexec` can still be upgraded — just not from in
/// here — so the command is named rather than offered.
fn offer_for(install: Install, tool: Option<&str>, have_pkexec: bool) -> Offer {
    match plan(install, tool) {
        Some(p) if install == Install::Apt && !have_pkexec && tool.is_none() => {
            Offer::Advice { command: p.shown }
        }
        Some(p) => Offer::Button { command: p.shown },
        None => Offer::LinkOnly,
    }
}

/// Is this executable on `PATH`?
///
/// Read rather than spawned, unlike `machine::on_path`: this runs on a tokio
/// worker inside the release poller, where `std::process::Command` is the trap
/// `docs/traps/portability.md` opens with. A missing `PATH` answers "no", which
/// degrades an apt install to advice rather than to a button that cannot work.
fn on_path(exe: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(exe).is_file())
}

/// How long an upgrade may take before it is killed and reported as failed.
///
/// `mise upgrade` fetches and unpacks, so this is minutes rather than seconds —
/// but bounded, because the alternative is a bar that says "Upgrading…" forever
/// with no way to find out otherwise.
const UPGRADE_TIMEOUT_SECS: u64 = 300;

/// Which install an upgrade belongs to.
///
/// One runner, two subjects. They differ in which slot the report lands in and in
/// whether the check that found the update is worth re-running, and in nothing
/// else — so the bounded exec, the captured tail and the reporting stay one
/// implementation. That reporting is the part already got wrong once (a successful
/// run used to clear itself, which read as a button that did nothing), and a second
/// copy is how one of them would get the fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    /// Claude Code, which every session spawns fresh.
    Agent,
    /// This app. Nothing about the running process changes: mise installs beside
    /// it, so the report is "restart" rather than "done".
    App,
}

impl Subject {
    /// Where this subject's run is reported. Named once, because the two slots
    /// are the only thing that differs between the two upgrade routes and every
    /// place that reached for one by hand was a place the other could be picked
    /// by mistake.
    fn run_slot(self, inner: &mut crate::state::Inner) -> &mut Option<UpgradeRun> {
        match self {
            Subject::Agent => &mut inner.upgrade_run,
            Subject::App => &mut inner.self_upgrade_run,
        }
    }

    /// What to upgrade, and to which version, if there is anything to offer.
    ///
    /// `Err` is the refusal the route reports verbatim: no update found, or an
    /// install with no installer to ask.
    fn offer(self, inner: &crate::state::Inner) -> std::result::Result<(Plan, Version), String> {
        match self {
            Subject::Agent => {
                let u = inner
                    .agent_update
                    .clone()
                    .ok_or("no agent update to install — refresh the check first")?;
                Ok((
                    Plan {
                        argv: vec!["mise".into(), "upgrade".into(), u.tool.clone()],
                        shown: format!("mise upgrade {}", u.tool),
                    },
                    Version {
                        from: u.current,
                        to: u.latest,
                    },
                ))
            }
            Subject::App => {
                let u = inner
                    .update
                    .clone()
                    .ok_or("no release to install — the check has not found one")?;
                /* **Decided again here, not read out of the snapshot.** What the
                bar carries is a sentence for a person; what this needs is an
                argv. Both come from the same [`plan`], so a button the page
                offered is a plan this can build — and a press cannot outlive
                the fact that made it, because the fact is re-read. */
                let install = Install::of_running();
                let p = plan(install, u.tool.as_deref()).ok_or(
                    "this install has no installer to ask — \
                     download the release, or install through mise, Homebrew or apt",
                )?;
                if p.argv.first().is_some_and(|a| a == PKEXEC) && !on_path(PKEXEC) {
                    // Refused in front of the run rather than reported after it:
                    // spawning a missing binary fails as "No such file", which is
                    // a sentence about `pkexec` rather than about what to do.
                    return Err(format!(
                        "no pkexec on this machine, so the password cannot be asked for — \
                         run `{}` in a terminal",
                        p.shown
                    ));
                }
                Ok((
                    p,
                    Version {
                        from: u.current,
                        to: u.latest,
                    },
                ))
            }
        }
    }
}

/// The two versions an upgrade moves between, for the route's own answer.
pub struct Version {
    pub from: String,
    pub to: String,
}

/// Start an upgrade for `subject`, or say why not.
///
/// One implementation for both buttons. Each takes the run slot under the same
/// lock that checked it, so a second press cannot race past the refusal — two
/// `mise upgrade`s of one tool race over the same install directory.
pub async fn start_upgrade(
    app: &Arc<AppState>,
    subject: Subject,
) -> std::result::Result<Version, String> {
    let (plan, version) = {
        let mut inner = app.inner.write().await;
        if subject
            .run_slot(&mut inner)
            .as_ref()
            .is_some_and(|r| r.running)
        {
            return Err("that upgrade is already running".to_string());
        }
        let (plan, version) = subject.offer(&inner)?;
        *subject.run_slot(&mut inner) = Some(UpgradeRun {
            to: version.to.clone(),
            running: true,
            tail: String::new(),
        });
        (plan, version)
    };
    app.notify().await;
    run_upgrade(app.clone(), plan, version.to.clone(), subject);
    Ok(version)
}

/// Put a finished run's report away, or say why not.
///
/// Daemon-side rather than a flag in the SPA, because the report is: a bar
/// dismissed in one window and back on the next reload is the same bar arguing
/// with you. Refuses while the run is going — there is nothing to dismiss yet, and
/// clearing it would leave the button enabled beside a running `mise upgrade`.
pub async fn dismiss(app: &Arc<AppState>, subject: Subject) -> std::result::Result<(), String> {
    {
        let mut inner = app.inner.write().await;
        let slot = subject.run_slot(&mut inner);
        if slot.as_ref().is_some_and(|r| r.running) {
            return Err("the upgrade is still running".to_string());
        }
        *slot = None;
    }
    app.notify().await;
    Ok(())
}

/// Run the upgrade, then say what happened.
///
/// Detached: the button answers immediately, and the bar follows the state through
/// the snapshot. `run_bounded` captures rather than streams, so there is no live
/// output to show — what a failure needs is the *end* of it, which is what a
/// captured tail is.
fn run_upgrade(
    app: std::sync::Arc<crate::state::AppState>,
    plan: Plan,
    to: String,
    subject: Subject,
) {
    tokio::spawn(async move {
        let main = app.cfg.main_checkout.clone();
        let argv = plan.argv.clone();
        let privileged = plan.argv.first().is_some_and(|a| a == PKEXEC);
        let done = tokio::task::spawn_blocking(move || {
            crate::proc::run_bounded(&main, UPGRADE_TIMEOUT_SECS, &argv, "agent upgrade")
        })
        .await;

        let failure: Option<String> = match done {
            Err(e) => Some(format!("the upgrade task panicked: {e}")),
            Ok(Err(e)) => Some(format!("{e:#}")),
            Ok(Ok(out)) if !out.status.success() => {
                // stderr first: mise says what went wrong there, and its stdout is
                // progress noise. Both, because a tool that fails quietly on one of
                // them would otherwise report nothing at all.
                let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
                if text.trim().is_empty() {
                    text = String::from_utf8_lossy(&out.stdout).into_owned();
                }
                Some(explain(privileged, &plan.shown, &tail(&text, 12)))
            }
            Ok(Ok(_)) => None,
        };

        // Asked either way, and before the bar is updated: the check is what decides
        // whether the nudge stays, so a failure that actually installed something is
        // reported by the version rather than by our guess about the exit code.
        //
        // Only for the agent. The app's own nudge compares the newest release
        // against the version *this process* was built as, and a successful upgrade
        // does not change that — it is still true until the restart, and re-checking
        // would only cost a request to say so again.
        if subject == Subject::Agent {
            if let Err(e) = refresh(&app).await {
                tracing::warn!("re-checking the agent version after an upgrade failed: {e:#}");
            }
        }

        {
            let mut inner = app.inner.write().await;
            match &failure {
                Some(text) => tracing::warn!("`{}` failed: {text}", plan.shown),
                None => tracing::info!("ran `{}`", plan.shown),
            }
            // Reported rather than cleared: see `tail`. An empty tail on a
            // finished run is the success.
            *subject.run_slot(&mut inner) = Some(UpgradeRun {
                to,
                running: false,
                tail: failure.unwrap_or_default(),
            });
        }
        app.notify().await;
    });
}

/// The program that asks for the password, named once because three places test
/// for it.
const PKEXEC: &str = "pkexec";

/// Put a sentence in front of a failure the raw output does not explain.
///
/// **The failure this exists for is the one an apt install hits most.** `pkexec`
/// needs a polkit *authentication agent* to draw the password prompt, and a GNOME
/// or KDE session has one while a bare X session, an ssh login and WSL do not.
///
/// It has two spellings and the substring is what both share. Measured here, from
/// a daemon-spawned `pkexec true` with no controlling terminal — exit 127, and:
/// `Error creating textual authentication agent: … ('/dev/tty'): No such device or
/// address`. The graphical one is `Error executing command as another user: No
/// authentication agent found`. Both are true and neither says what to do, so the
/// bar leads with what to do and keeps the original underneath: the tail is what
/// the link's `title` shows, and a reason that is thrown away is a reason nobody
/// can act on.
fn explain(privileged: bool, shown: &str, tail: &str) -> String {
    if privileged && tail.contains("authentication agent") {
        return format!(
            "no password prompt could be shown, so nothing was installed — \
             run `{shown}` in a terminal\n{tail}"
        );
    }
    tail.to_string()
}

/// The last `n` non-empty lines, which is what a failure is actually in.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Poll for a newer agent, forever.
///
/// Hourly. It was six hours, to match the daemon's own release check, and that is
/// the wrong comparison: the agent prints its own nag in the pane the moment it
/// knows, so a slow poll does not mean "you hear about it later", it means **the
/// app disagrees with the terminal inside it** — the session says upgrade and the
/// bar that exists to offer that button is not there. Affordable now that the
/// check names one tool instead of every tool in the checkout.
pub fn start_agent_poller(app: std::sync::Arc<crate::state::AppState>) {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(60 * 60);
        loop {
            if let Err(e) = refresh(&app).await {
                tracing::warn!("checking the agent version failed: {e:#}");
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Check in the background, and never make the caller wait for it.
///
/// For the spawn paths: starting a session is the moment the agent's version
/// matters, because this one runs whatever is installed now and prints its own
/// "update available" nag into the pane the moment it thinks so. A poll alone
/// leaves the bar that offers the upgrade button missing while the terminal
/// inside the app is asking for it. Failures are debug-level: this is a nudge,
/// and one that could not be refreshed is not worth a warning per session.
pub fn refresh_detached(app: &std::sync::Arc<crate::state::AppState>) {
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = refresh(&app).await {
            tracing::debug!("re-checking the agent version after a spawn failed: {e:#}");
        }
    });
}

/// Check once, now, and publish the answer. The poller's tick and the refresh
/// button's other half.
pub async fn refresh(app: &std::sync::Arc<crate::state::AppState>) -> Result<()> {
    let main = app.cfg.main_checkout.clone();
    // Off-thread: `mise outdated` reaches the network to learn the latest
    // version, and the runtime must not wait on it.
    let next = tokio::task::spawn_blocking(move || check(&main)).await?;
    /* **Scoped, and the scope is load-bearing.** This used to drop the guard only
    on the branch that notifies, which was harmless while nothing followed it. The
    look below takes the state lock itself, so a guard still held on the unchanged
    path — the ordinary one — is a daemon waiting on its own write lock forever:
    every request after the next spawn hung, and the e2e flows saw it as the
    daemon going away. */
    let changed = {
        let mut inner = app.inner.write().await;
        let changed = inner.agent_update != next;
        if changed {
            inner.agent_update = next;
        }
        changed
    };
    if changed {
        app.notify().await;
    }
    // The same tick asks which sessions an upgrade left behind: a refresh runs
    // when an upgrade ends, which is exactly when that answer changes.
    mark_stale(app, Look::Resolve).await;
    Ok(())
}

/// How hard [`mark_stale`] looks.
#[derive(Clone, Copy, PartialEq)]
pub enum Look {
    /// Only whether each session's file still exists. A `stat` per session.
    Exists,
    /// Also what a new spawn in each session's tree would run now, which needs
    /// that tree's environment and so a `mise env` per distinct tree.
    Resolve,
}

/// Flag the sessions running an older Claude Code than a new one would get.
///
/// **Two signals, because upgrades happen two ways.** mise **deletes** the old
/// versioned directory (the note at the top of this file), so a session whose
/// recorded file is gone is on a build that no longer exists — a `stat`, cheap
/// enough to run every minute, and it is what catches a `mise up` in a shell.
/// An installer that repoints a symlink and keeps the old file (the native
/// installer's `versions/`) leaves that file in place, and only resolving `claude`
/// again the way a spawn would sees it: that is the `Resolve` look, run on the
/// hourly poll, after every spawn and after an upgrade from the bar.
///
/// **Resolved in each session's own tree.** A spawn reads the environment of the
/// directory it runs in (`launch::session_env`), and a worktree may pin a
/// different tool than main — so asking main alone would call a session stale
/// that a respawn would put straight back on the same build.
///
/// One-way within a session's life: a session never becomes current again
/// without being respawned, so only `false → true` is written, and the respawn's
/// fresh record is what clears it.
pub async fn mark_stale(app: &Arc<AppState>, look: Look) {
    let live: Vec<(
        crate::model::SessionId,
        std::path::PathBuf,
        std::path::PathBuf,
    )> = {
        let inner = app.inner.read().await;
        inner
            .sessions
            .values()
            .filter(|s| s.state.is_live() && !s.agent_stale)
            .filter_map(|s| Some((s.id, s.cwd.clone(), s.agent_exe.clone()?)))
            .collect()
    };
    if live.is_empty() {
        return;
    }
    let cfg = app.cfg.clone();
    let stale = crate::proc::run_blocking("asking which sessions run an old claude", move || {
        let mut now: std::collections::HashMap<std::path::PathBuf, Option<std::path::PathBuf>> =
            std::collections::HashMap::new();
        live.into_iter()
            .filter(|(_, cwd, exe)| {
                if !exe.exists() {
                    return true;
                }
                if look == Look::Exists {
                    return false;
                }
                let current = now.entry(cwd.clone()).or_insert_with(|| {
                    /* **The checkout's own variables, not `launch::session_env`.**
                    That is refused here by `clippy.toml`, and rightly: it builds a
                    *spawn's* environment, and one built outside `spawn` is how a run
                    lost its credential. This spawns nothing — it asks which file
                    `claude` names, and that is decided by the PATH the checkout
                    exports. The one other PATH change `session_env` makes is to put
                    the app's own directory first, which holds `orch` and never
                    `claude`, so the two cannot disagree about the agent. */
                    let env = crate::env_source::read(cfg.env_source, cwd);
                    orchd_base::pty::which("claude", cwd, &env, &[]).ok()
                });
                // A tree where `claude` cannot be found says nothing about this
                // session, so it is not called stale on the strength of that.
                current.as_ref().is_some_and(|c| c != exe)
            })
            .map(|(id, _, _)| id)
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    if stale.is_empty() {
        return;
    }
    {
        let mut inner = app.inner.write().await;
        for id in &stale {
            if let Some(s) = inner.sessions.get_mut(id) {
                s.agent_stale = true;
            }
        }
    }
    tracing::info!(
        sessions = stale.len(),
        "sessions are running an older claude than is installed"
    );
    app.notify().await;
}

/// The cheap look, every minute. See [`mark_stale`] for why a minute and why only
/// a `stat`.
pub fn start_stale_poller(app: Arc<AppState>) {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(60);
        loop {
            tokio::time::sleep(interval).await;
            mark_stale(&app, Look::Exists).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Upgrading the app
// ---------------------------------------------------------------------------

// Upgrading the app to the release it just told you about.
//
// The nudge existed long before the button: `start_update_poller` compares the
// newest tag against `CARGO_PKG_VERSION` and the bar said "Run mise up", which is
// a instruction to go and do by hand what the app is perfectly able to do itself.
// This is the other half, and it is deliberately the *same* half the agent bar
// already has — [`run_upgrade`] runs both, so the bounded
// exec, the captured tail and the reporting have one implementation rather than
// two that drift.
//
// **Three installs can be upgraded from inside the app: mise, Homebrew and apt.**
// It was mise alone, which was right while mise was the only channel with an
// installer behind it — and wrong the week a cask and an APT repository shipped,
// because the bar then told both of them to "Run mise up". [`plan`] says what each
// one runs and why the other three are offered nothing.
//
// The upgrade cannot take effect on its own whichever channel it came from: this
// process *is* the old build. mise installs beside it, apt replaces the binary
// while Linux keeps this process on its old inode, and Homebrew replaces the
// bundle — so all three finish with "restart", and the restart is
// [`crate::window::WindowCmd::Restart`], which is why `relaunch` resolves the
// `latest` symlink instead of re-running the exact path it started from. The
// agent bar used to offer that same restart and does not any more: an agent
// upgrade needs its *sessions* respawned, not the app, and `restart.rs` does that
// in place.
//
// **One thing only macOS does**: a cask upgrade swaps `Orchestrator.app`
// underneath a process that is running out of it, where mise never touches the
// build in use. Nothing here can prevent that; what it can do is not leave the old
// process running longer than it must, which is what the "restart to run it" the
// bar already shows is for.

/// Which mise tool provides the binary that is running, if mise provides it.
///
/// Asked of mise rather than derived from the path, because the two are not the
/// same string: an install of `github:kbarendrecht/orchestrator` lands in
/// `installs/github-kbarendrecht-orchestrator/…`, and `mise upgrade` wants the name
/// with the colon and the slash. `mise ls --json` is keyed by exactly the name mise
/// accepts and carries `install_path` beside it, so the answer is a prefix match
/// rather than a guess about how a backend spells its directory.
///
/// `None` for every install that is not mise's — a `.deb`, an AppImage, a `cargo
/// build` in a checkout — and that is the answer that hides the button.
pub fn app_providing_tool(main: &Path) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    // Bounded, through the same helper the agent half uses.
    let out = mise(main, &["ls", "--json"], "listing mise's installs")?;
    tool_owning(&out.stdout, &exe)
}

/// Split from [`app_providing_tool`] so the shape mise emits can be tested without
/// mise, and so the prefix rule is stated once.
///
/// The longest matching `install_path` wins for the same reason
/// `workspace_for_path` takes the longest workspace: nothing stops one tool's
/// install directory from sitting inside another's, and the specific one is the
/// owner.
///
/// The sibling question, "which tool directory is this path under", is
/// [`tool_of_install_path`], and it is the wrong one to ask here — see its note.
fn tool_owning(stdout: &[u8], exe: &Path) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let mut best: Option<(usize, String)> = None;
    for (tool, versions) in v.as_object()? {
        for entry in versions.as_array().into_iter().flatten() {
            let Some(at) = entry.get("install_path").and_then(|p| p.as_str()) else {
                continue;
            };
            if !exe.starts_with(at) {
                continue;
            }
            if best.as_ref().is_none_or(|(len, _)| at.len() > *len) {
                best = Some((at.len(), tool.clone()));
            }
        }
    }
    best.map(|(_, tool)| tool)
}

/// Drop the ` (deleted)` Linux appends to a readlink of an unlinked binary.
///
/// Only ever a whole-string suffix, because `/proc/self/exe` is one link and the
/// suffix is on its target. A path that does not carry it comes back untouched.
fn strip_deleted(exe: &std::path::Path) -> std::path::PathBuf {
    match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(clean) => std::path::PathBuf::from(clean),
        None => exe.to_path_buf(),
    }
}

/// `…/installs/<tool>/<version>/<file>` → `…/installs/<tool>/latest/<file>`.
///
/// Only when that path really exists, so a layout this does not understand keeps
/// the resolved path it came with. Pure, and tested, because the fault it prevents
/// is invisible until an upgrade weeks later.
///
/// Reads the same [`INSTALLS`] layout as [`tool_of_install_path`] and answers a
/// different question about it: that one names the tool, this one re-points the
/// version. Neither can stand in for the other.
///
/// The input is `current_exe`, which after a self-upgrade is the worst-case shape:
/// mise removes the versioned directory this process was started from, and Linux
/// then answers `readlink /proc/self/exe` with the old path plus a literal
/// ` (deleted)` suffix. Left on, that suffix rode through the swap into
/// `…/latest/orchestrator-desktop (deleted)`, which never exists, so the guard
/// handed back the *deleted* path unchanged and `relaunch` spawned it and got
/// `ENOENT` — the app went away and did not come back. So strip the suffix before
/// anything else, and let the fallback return the cleaned path rather than the
/// tombstone.
pub fn stable_exe(exe: &std::path::Path) -> std::path::PathBuf {
    let exe = strip_deleted(exe);
    let exe = exe.as_path();
    let parts: Vec<_> = exe.components().collect();
    // <installs>/<tool>/<version>/<file>: the version is two components from the
    // end, and `installs` two before that.
    let Some(version_at) = parts.len().checked_sub(2) else {
        return exe.to_path_buf();
    };
    let installs_at = match version_at.checked_sub(2) {
        Some(i) => i,
        None => return exe.to_path_buf(),
    };
    if parts[installs_at].as_os_str() != std::ffi::OsStr::new(INSTALLS) {
        return exe.to_path_buf();
    }
    let mut latest = std::path::PathBuf::new();
    for (n, c) in parts.iter().enumerate() {
        if n == version_at {
            latest.push("latest");
        } else {
            latest.push(c.as_os_str());
        }
    }
    if latest.exists() {
        latest
    } else {
        exe.to_path_buf()
    }
}

// ---------------------------------------------------------------------------
// Noticing a newer app release
// ---------------------------------------------------------------------------

/// Notice a newer GitHub release than the running build.
///
/// The release lives on the fork (`kbarendrecht/orchestrator`), which is where
/// the app itself ships from — distinct from the *monorepo's* upstream that the
/// PR poller watches. Checks on launch and every hour; a found update sits
/// in the snapshot as a dismissible nudge, and `mise up` is what installs it.
///
/// **Hourly is one request an hour**, against 5000 with a token and 60 without,
/// and the only other work — `app_providing_tool`, which shells `mise ls --json` —
/// runs only once a newer tag has actually been found. Six hours was the old
/// interval and its cost was the same; what it bought was being most of a working
/// day behind a release that was already out.
pub fn start_release_poller(app: Arc<AppState>) {
    // The repo the binary is released from, not the monorepo it hosts.
    const RELEASE_REPO: (&str, &str) = ("kbarendrecht", "orchestrator");
    let current = env!("CARGO_PKG_VERSION").to_string();
    let token_file = app.cfg.github_token_file.clone();
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(60 * 60);
        loop {
            let cur = current.clone();
            // Rides the same token ladder the PR poller uses. The repo is public,
            // so an unauthenticated read would usually work — but GitHub rate-limits
            // those by IP at 60/hour, shared with everything else on the machine,
            // and a token lifts it to 5000. Resolved per poll, off-thread, so a
            // rotated token is picked up and a slow `gh auth token` never blocks
            // the runtime. A missing token is not an error: the nudge just waits.
            let tf = token_file.clone();
            if let Ok(Some((tag, url))) = tokio::task::spawn_blocking(move || {
                let token = crate::forge::resolve_token(tf.as_deref())
                    .ok()
                    .map(|t| t.value);
                crate::forge::latest_release(RELEASE_REPO.0, RELEASE_REPO.1, token.as_deref())
            })
            .await
            {
                let newer = match (parse_semver(&tag), parse_semver(&cur)) {
                    (Some(latest), Some(running)) => latest > running,
                    _ => false,
                };
                // Only when there is something to offer, and off-thread because it
                // shells mise. `None` is the ordinary answer for every install mise
                // did not make, and the install kind below is what decides what to
                // say about those.
                let tool = if newer {
                    let main = app.cfg.main_checkout.clone();
                    tokio::task::spawn_blocking(move || app_providing_tool(&main))
                        .await
                        .unwrap_or(None)
                } else {
                    None
                };
                /* Both touch the filesystem — `Install::of_running` probes the
                Caskroom and may read a bundle marker, `on_path` stats a
                directory per `PATH` entry — so they go off the runtime with the
                mise call rather than beside it. */
                let tool_for_offer = tool.clone();
                let offer = if newer {
                    tokio::task::spawn_blocking(move || {
                        offer_for(
                            Install::of_running(),
                            tool_for_offer.as_deref(),
                            on_path(PKEXEC),
                        )
                    })
                    .await
                    .unwrap_or(Offer::LinkOnly)
                } else {
                    Offer::LinkOnly
                };
                let next = newer.then(|| UpdateInfo {
                    current: cur.clone(),
                    latest: tag.trim_start_matches('v').to_string(),
                    url,
                    tool,
                    offer,
                });
                let mut inner = app.inner.write().await;
                if inner.update != next {
                    inner.update = next;
                    drop(inner);
                    app.notify().await;
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// `v1.2.3` / `1.2.3` / `1.2.3-rc1` → `(1, 2, 3)`. Prerelease and build metadata
/// are dropped: good enough to answer "is there a newer release", which is all
/// the nudge asks. Anything unparseable is `None` and simply never nags.
fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// mise deletes the directory it upgraded away from, and a `stat` is enough to
    /// see that. This is the look that runs every minute, so it is the one that
    /// catches a `mise up` done in a shell.
    #[tokio::test]
    async fn a_session_whose_binary_is_gone_is_stale_and_one_whose_is_not_is_not() {
        let (app, dir) = crate::testutil::app("stale-gone");
        let gone = dir.join("claude-old");
        let kept = dir.join("claude-kept");
        std::fs::write(&gone, "").unwrap();
        std::fs::write(&kept, "").unwrap();
        let mut ids = Vec::new();
        for exe in [&gone, &kept] {
            let id = uuid::Uuid::new_v4();
            let mut s = crate::model::Session::new(id, "main".into(), dir.clone(), None);
            s.set_state(crate::model::State::Working);
            s.agent_exe = Some(exe.clone());
            app.inner.write().await.sessions.insert(id, s);
            ids.push(id);
        }
        std::fs::remove_file(&gone).unwrap();

        mark_stale(&app, Look::Exists).await;
        let inner = app.inner.read().await;
        assert!(
            inner.sessions[&ids[0]].agent_stale,
            "its build no longer exists"
        );
        assert!(
            !inner.sessions[&ids[1]].agent_stale,
            "its build is still there"
        );
    }

    /// Each channel runs its own installer, and the apt one carries the two things
    /// it is easiest to leave out: the refresh, and the program that asks for the
    /// password.
    #[test]
    fn every_channel_that_can_be_upgraded_runs_its_own_installer() {
        let mise = plan(Install::Tarball, Some("github:kbarendrecht/orchestrator")).expect("mise");
        assert_eq!(
            mise.argv,
            vec!["mise", "upgrade", "github:kbarendrecht/orchestrator"]
        );

        let brew = plan(Install::Homebrew, None).expect("brew");
        assert_eq!(brew.argv, vec!["brew", "upgrade", "--cask", "orchestrator"]);
        assert_eq!(brew.shown, "brew upgrade --cask orchestrator");

        let apt = plan(Install::Apt, None).expect("apt");
        assert_eq!(apt.argv.first().map(String::as_str), Some(PKEXEC));
        let script = apt.argv.last().expect("the script");
        assert!(
            script.contains("apt-get update") && script.contains("--only-upgrade orchestrator"),
            "the lists have to be refreshed before the upgrade can see the release: {script}"
        );
        // What a person would type, which is not what is run.
        assert_eq!(apt.shown, "sudo apt install --only-upgrade orchestrator");
        assert!(!apt.shown.contains(PKEXEC));
    }

    /// The three with no installer behind them, and the point of them: an install
    /// this cannot upgrade is offered nothing rather than offered `mise up`.
    #[test]
    fn a_downloaded_file_is_offered_nothing() {
        for install in [
            Install::MacBundle,
            Install::AppImage,
            Install::Tarball,
            Install::Checkout,
        ] {
            assert_eq!(plan(install, None), None, "{install:?}");
            assert_eq!(
                offer_for(install, None, true),
                Offer::LinkOnly,
                "{install:?}"
            );
        }
    }

    /// The shape the page branches on, asserted here because the page cannot be
    /// asked from a unit test.
    ///
    /// `renderUpdate` reads `offer.kind` and `offer.command`; `snapshot.d.ts` says
    /// so because `ts_rs` reads the same serde attribute, and `check-web` regenerates
    /// and diffs it. What neither covers is the JSON actually put on the wire, which
    /// is this.
    #[test]
    fn the_page_gets_a_tagged_offer_with_the_command_beside_it() {
        assert_eq!(
            serde_json::to_value(Offer::Button {
                command: "brew upgrade --cask orchestrator".into()
            })
            .expect("serialises"),
            serde_json::json!({"kind": "button", "command": "brew upgrade --cask orchestrator"})
        );
        assert_eq!(
            serde_json::to_value(Offer::LinkOnly).expect("serialises"),
            serde_json::json!({"kind": "link_only"})
        );
    }

    /// An apt install on a machine with no `pkexec` can still be upgraded — just
    /// not from in here. Naming the command is the difference between a bar that
    /// helps and a button that cannot work.
    #[test]
    fn apt_without_pkexec_is_advice_rather_than_a_button() {
        assert_eq!(
            offer_for(Install::Apt, None, false),
            Offer::Advice {
                command: "sudo apt install --only-upgrade orchestrator".into()
            }
        );
        assert_eq!(
            offer_for(Install::Apt, None, true),
            Offer::Button {
                command: "sudo apt install --only-upgrade orchestrator".into()
            }
        );
        // mise's own button never wants pkexec, whatever the packaging under it
        // looks like to the path rules.
        assert!(matches!(
            offer_for(Install::Apt, Some("t"), false),
            Offer::Button { .. }
        ));
    }

    /// The failure an apt install hits on every machine with no desktop session.
    /// `pkexec`'s own words are true and useless, so the bar leads with what to do
    /// and keeps them underneath.
    #[test]
    fn a_missing_polkit_agent_is_explained_rather_than_quoted() {
        // Both spellings, because the daemon's own spawn produces the second one:
        // no controlling terminal, so pkexec cannot even fall back to asking in
        // text. Measured, and quoted in `explain`'s note.
        for raw in [
            "Error executing command as another user: No authentication agent found.",
            "Error creating textual authentication agent: Error opening current \
             controlling terminal for the process (`/dev/tty'): No such device or address",
        ] {
            let said = explain(true, "sudo apt install --only-upgrade orchestrator", raw);
            assert!(
                said.starts_with("no password prompt could be shown"),
                "{said}"
            );
        }
        let raw = "Error executing command as another user: No authentication agent found.";
        let said = explain(true, "sudo apt install --only-upgrade orchestrator", raw);
        assert!(
            said.contains("sudo apt install --only-upgrade orchestrator"),
            "it has to name what to run instead: {said}"
        );
        assert!(said.contains(raw), "the original reason is not thrown away");

        // Any other failure is reported as it stands: an explanation invented for
        // an error it does not fit is worse than the error.
        let other = "E: Could not get lock /var/lib/dpkg/lock-frontend";
        assert_eq!(explain(true, "x", other), other);
        // And nothing is explained for a run that never asked for a password.
        assert_eq!(explain(false, "x", raw), raw);
    }

    /// What the bar shows when an upgrade fails is the *end* of the output, and
    /// mise pads its errors with blank lines — so a naive last-N-lines would hand
    /// the bar an empty string and the failure would read as no reason at all.
    #[test]
    fn the_reported_tail_is_the_last_lines_that_say_something() {
        let noisy =
            "fetching\n\nunpacking\n\nmise ERROR no version set\nmise ERROR see --verbose\n\n";
        assert_eq!(
            tail(noisy, 2),
            "mise ERROR no version set\nmise ERROR see --verbose"
        );
        // Shorter than asked for is the whole of it, not padding.
        assert_eq!(tail("only this\n", 12), "only this");
        assert_eq!(tail("\n\n", 4), "");
    }

    /// Both spellings, because this machine really has both — and the shadowed
    /// one must not be what gets reported. Driving it taught this: upgrading
    /// `claude-code` left `claude` listed as outdated, and the bar came back
    /// offering to upgrade an install that is never executed.
    const BOTH_SPELLINGS: &[u8] = br#"{
      "bun": {"name":"bun","requested":"latest","current":"1.3.14","latest":"1.4.0"},
      "claude-code": {"name":"claude-code","requested":"latest","current":"2.1.232",
        "bump":null,"latest":"2.1.240",
        "source":{"type":"mise.toml","path":"/x/mise.toml"}},
      "claude": {"name":"claude","requested":"latest","current":"2.1.232","latest":"2.1.240"}
    }"#;

    #[test]
    fn reads_the_shape_mise_actually_emits() {
        let u = parse(BOTH_SPELLINGS, "claude-code").expect("an update");
        assert_eq!(u.tool, "claude-code");
        assert_eq!(u.current, "2.1.232");
        assert_eq!(u.latest, "2.1.240");
        // The agent is always mise's, so the tool decides the plan whatever the
        // app itself was installed by.
        let p = plan(Install::Apt, Some(&u.tool)).expect("a plan");
        assert_eq!(p.argv, vec!["mise", "upgrade", "claude-code"]);
        assert_eq!(p.shown, "mise upgrade claude-code");
    }

    #[test]
    fn only_the_tool_that_provides_the_binary_is_reported() {
        // `claude-code` current, the shadowed `claude` entry still behind: the
        // answer is silence, because the binary a session runs is up to date.
        let upgraded = br#"{"claude":{"current":"2.1.232","latest":"2.1.240"}}"#;
        assert_eq!(
            parse(upgraded, "claude-code"),
            None,
            "a stale entry that PATH never reaches must not nag"
        );
        // And the other way round, when `claude` is the one in use.
        assert_eq!(parse(upgraded, "claude").expect("an update").tool, "claude");
    }

    #[test]
    fn the_tool_comes_out_of_the_install_path() {
        assert_eq!(
            tool_of_install_path("/home/x/.local/share/mise/installs/claude-code/latest/claude")
                .as_deref(),
            Some("claude-code")
        );
        assert_eq!(
            tool_of_install_path("/home/x/.local/share/mise/installs/claude/2.1.240/claude")
                .as_deref(),
            Some("claude")
        );
        // Not a mise install — an npm global, say. Nothing to offer.
        assert_eq!(tool_of_install_path("/usr/local/bin/claude"), None);
        assert_eq!(tool_of_install_path(""), None);
    }

    #[test]
    fn nothing_outdated_is_silence() {
        // What mise prints when everything is current, and the case that must not
        // produce a nudge.
        assert_eq!(parse(b"{}", "claude-code"), None);
        // Other tools behind, the agent not mentioned: also nothing to say.
        assert_eq!(
            parse(
                br#"{"bun":{"current":"1.0","latest":"1.1"}}"#,
                "claude-code"
            ),
            None
        );
    }

    #[test]
    fn a_version_that_did_not_move_is_not_an_update() {
        // Defensive: mise listing a tool whose versions match would otherwise
        // become "upgrade 2.1.240 to 2.1.240".
        let raw = br#"{"claude-code":{"current":"2.1.240","latest":"2.1.240"}}"#;
        assert_eq!(parse(raw, "claude-code"), None);
    }

    #[test]
    fn unparseable_output_says_nothing_rather_than_failing() {
        assert_eq!(parse(b"not json", "claude-code"), None);
        assert_eq!(parse(b"", "claude-code"), None);
        // Present but missing the fields the nudge needs.
        assert_eq!(
            parse(br#"{"claude-code":{"requested":"latest"}}"#, "claude-code"),
            None
        );
    }

    /// The fault this prevents costs an upgrade, not a launch: mise installs each
    /// version in its own directory, so a path written into the launcher entry or
    /// into the push guard's hook is dead the moment `mise up` removes it. Seen
    /// twice — a `.desktop` file naming a version that was gone, and a `PreToolUse`
    /// hook whose `orch` had been replaced, which made the guard fail *open* and
    /// print four errors into a session.
    #[test]
    fn a_mise_install_path_resolves_to_the_latest_symlink() {
        let d = std::env::temp_dir().join(format!("orchd-stable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let versioned = d.join("installs/orchestrator/2026.9.0");
        let latest = d.join("installs/orchestrator/latest");
        std::fs::create_dir_all(&versioned).unwrap();
        std::fs::create_dir_all(&latest).unwrap();
        std::fs::write(versioned.join("orchestrator-desktop"), "x").unwrap();
        std::fs::write(latest.join("orchestrator-desktop"), "x").unwrap();

        assert_eq!(
            stable_exe(&versioned.join("orchestrator-desktop")),
            latest.join("orchestrator-desktop")
        );
    }

    /// The shape a self-upgrade actually hands this: mise removed the versioned
    /// directory, so `current_exe` reads back the old path with ` (deleted)` on the
    /// end. The suffix must not ride through into the `latest` path, or the swap
    /// resolves to a file that never exists and `relaunch` spawns a corpse.
    #[test]
    fn a_deleted_suffix_is_stripped_before_the_latest_swap() {
        let d = std::env::temp_dir().join(format!("orchd-deleted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let latest = d.join("installs/orchestrator/latest");
        std::fs::create_dir_all(&latest).unwrap();
        std::fs::write(latest.join("orchestrator-desktop"), "x").unwrap();

        // The versioned directory is gone, exactly as after `mise upgrade`.
        let deleted = format!(
            "{}/installs/orchestrator/2026.9.3/orchestrator-desktop (deleted)",
            d.display()
        );
        assert_eq!(
            stable_exe(Path::new(&deleted)),
            latest.join("orchestrator-desktop"),
            "the ` (deleted)` tombstone must resolve to the live `latest` binary"
        );

        // And when there is no `latest` to prefer, the fallback is the cleaned path,
        // not the tombstone — a path that names the file beats one that cannot be run.
        let _ = std::fs::remove_dir_all(&latest);
        assert_eq!(
            stable_exe(Path::new(&deleted)),
            Path::new(&deleted.strip_suffix(" (deleted)").unwrap()),
            "with no `latest`, hand back the file without the suffix"
        );
    }

    #[test]
    fn a_path_with_no_latest_beside_it_is_left_alone() {
        let d = std::env::temp_dir().join(format!("orchd-nolatest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let versioned = d.join("installs/orchestrator/2026.9.0");
        std::fs::create_dir_all(&versioned).unwrap();
        let exe = versioned.join("orchestrator-desktop");
        std::fs::write(&exe, "x").unwrap();
        assert_eq!(stable_exe(&exe), exe, "no `latest` means nothing to prefer");

        // And anything that is not a mise layout, including paths short enough to
        // underflow the component arithmetic.
        for p in ["/usr/bin/orch", "/x", "/"] {
            assert_eq!(stable_exe(Path::new(p)), Path::new(p));
        }
    }

    /// The real shape, trimmed: mise keys by the name it accepts, which for a
    /// backend install is not what the directory is called.
    const LS: &str = r#"{
      "node": [
        {"version":"22.1.0","install_path":"/home/me/.local/share/mise/installs/node/22.1.0","installed":true,"active":true}
      ],
      "github:kbarendrecht/orchestrator": [
        {"version":"2026.9.2","install_path":"/home/me/.local/share/mise/installs/github-kbarendrecht-orchestrator/2026.9.2","installed":true,"active":true}
      ]
    }"#;

    #[test]
    fn the_tool_is_the_name_mise_accepts_not_the_directory_it_used() {
        let exe = Path::new(
            "/home/me/.local/share/mise/installs/github-kbarendrecht-orchestrator/2026.9.2/orchestrator-desktop",
        );
        assert_eq!(
            tool_owning(LS.as_bytes(), exe).as_deref(),
            Some("github:kbarendrecht/orchestrator"),
            "`mise upgrade` wants the colon-and-slash name, not the directory"
        );
    }

    #[test]
    fn a_binary_mise_did_not_install_has_no_tool() {
        for exe in [
            "/usr/bin/orchestrator-desktop",
            "/home/me/src/orchestrator/target/release/orchestrator-desktop",
            "/tmp/.mount_Orches/usr/bin/orchestrator-desktop",
        ] {
            assert_eq!(
                tool_owning(LS.as_bytes(), Path::new(exe)),
                None,
                "{exe} is not mise's, so there is no button to offer"
            );
        }
    }

    /// Nested install directories are possible, and the owner is the specific one.
    #[test]
    fn the_longest_matching_install_path_wins() {
        let ls = r#"{
          "outer": [{"install_path":"/i/tools","installed":true}],
          "inner": [{"install_path":"/i/tools/orchestrator/1.0","installed":true}]
        }"#;
        assert_eq!(
            tool_owning(
                ls.as_bytes(),
                Path::new("/i/tools/orchestrator/1.0/orchestrator-desktop")
            )
            .as_deref(),
            Some("inner")
        );
    }

    #[test]
    fn nonsense_from_mise_is_no_tool_rather_than_a_panic() {
        assert_eq!(tool_owning(b"not json", Path::new("/x")), None);
        assert_eq!(tool_owning(b"[]", Path::new("/x")), None);
        assert_eq!(tool_owning(b"{\"t\":[{}]}", Path::new("/x")), None);
    }
}
