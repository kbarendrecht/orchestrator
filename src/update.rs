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
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;

use crate::state::AppState;

// ---------------------------------------------------------------------------
// The agent
// ---------------------------------------------------------------------------

/// A newer agent build than the one installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export, export_to = "../web/snapshot.d.ts"))]
pub struct AgentUpdate {
    /// The mise tool name to upgrade — `claude-code` or `claude`, whichever this
    /// checkout pins. Carried rather than assumed so the button upgrades the tool
    /// that actually provides the binary.
    pub tool: String,
    pub current: String,
    pub latest: String,
}

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
    let out = mise(main, &["outdated", "--json", &tool], "the agent version check")?;
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
    let out = mise(main, &["which", "claude"], "resolving the agent's mise tool")?;
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
    Some(AgentUpdate { tool: tool.to_string(), current, latest })
}

/// The command the upgrade button runs.
///
/// Returned rather than executed so the deadline, the cwd and the reporting all
/// live with the caller. Run in the main checkout, because that is the config mise
/// resolves the tool version from.
fn upgrade_argv(tool: &str) -> Vec<String> {
    vec!["mise".into(), "upgrade".into(), tool.into()]
}

/// An upgrade the daemon is running, or the failure it left behind.
///
/// The run used to be a process in main's drawer, which was the wrong home twice:
/// the drawer is *this workspace's* processes, and upgrading the agent belongs to
/// no workspace — so from any worktree the run was invisible, and main's drawer
/// grew a tab that was not a process of main's at all. It reports through the same
/// bar that offered the button instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export, export_to = "../web/snapshot.d.ts"))]
pub struct UpgradeRun {
    /// The version being installed. Carried so the bar can say it even after the
    /// check that found it has been refreshed away.
    pub to: String,
    pub running: bool,
    /// The tail of the output, for a run that failed. Empty while it runs, and
    /// empty on success — which, with `running` false, is how the bar tells the two
    /// finished states apart.
    ///
    /// Success used to clear the run outright, on the reasoning that the nudge
    /// going away *is* the report. It is not: the sessions you have open go on
    /// printing Claude Code's own upgrade notice, because they really are still the
    /// old build, so a bar that vanishes silently against a terminal that still
    /// says "update available" reads as a button that did nothing. It is reported,
    /// and dismissed like any other.
    pub tail: String,
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
    /// install mise did not make and so cannot name in `mise upgrade`.
    fn offer(self, inner: &crate::state::Inner) -> std::result::Result<(String, Version), String> {
        match self {
            Subject::Agent => {
                let u = inner
                    .agent_update
                    .clone()
                    .ok_or("no agent update to install — refresh the check first")?;
                Ok((u.tool, Version { from: u.current, to: u.latest }))
            }
            Subject::App => {
                let u = inner
                    .update
                    .clone()
                    .ok_or("no release to install — the check has not found one")?;
                let tool = u
                    .tool
                    .clone()
                    .ok_or("this build was not installed by mise, so it cannot upgrade itself")?;
                Ok((tool, Version { from: u.current, to: u.latest }))
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
    let (tool, version) = {
        let mut inner = app.inner.write().await;
        if subject.run_slot(&mut inner).as_ref().is_some_and(|r| r.running) {
            return Err("that upgrade is already running".to_string());
        }
        let (tool, version) = subject.offer(&inner)?;
        *subject.run_slot(&mut inner) = Some(UpgradeRun {
            to: version.to.clone(),
            running: true,
            tail: String::new(),
        });
        (tool, version)
    };
    app.notify().await;
    run_upgrade(app.clone(), tool, version.to.clone(), subject);
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
    tool: String,
    to: String,
    subject: Subject,
) {
    tokio::spawn(async move {
        let main = app.cfg.main_checkout.clone();
        let argv = upgrade_argv(&tool);
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
                Some(tail(&text, 12))
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
                Some(text) => tracing::warn!("upgrading {tool} failed: {text}"),
                None => tracing::info!("upgraded {tool}"),
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
    let mut inner = app.inner.write().await;
    if inner.agent_update != next {
        inner.agent_update = next;
        drop(inner);
        app.notify().await;
    }
    Ok(())
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
// **Only a mise install can be upgraded from inside the app**, and that is not a
// gap to close later. A `.deb` belongs to apt and wants a password; an AppImage
// and a `.dmg` are files somebody downloaded, and replacing the binary a process
// is executing is not something to do behind the user's back. Those installs keep
// the link to the release, which is what they had.
//
// The upgrade also cannot take effect on its own: this process *is* the old build,
// and mise installs beside it rather than over it. So a finished run says
// "restart", and the restart is the same [`crate::window::WindowCmd::Restart`] the
// agent bar offers — which is why `relaunch` resolves the `latest` symlink instead
// of re-running the exact path it started from.

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

/// A release newer than what is running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export, export_to = "../web/snapshot.d.ts"))]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub url: String,
    /// The mise tool that installed this binary, when one did.
    ///
    /// What decides whether the bar can offer a button at all: `Some` is an install
    /// the app can upgrade itself (`mise upgrade <tool>`), `None` is a `.deb`, an
    /// AppImage, a `.dmg` or a checkout, where the honest offer is the release link
    /// it already had. Resolved by `update::app_providing_tool` at check time,
    /// off-thread, because it shells mise.
    pub tool: Option<String>,
}

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
                let token = crate::forge::resolve_token(tf.as_deref()).ok().map(|t| t.value);
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
                // did not make, and it is what leaves the bar as the link it was.
                let tool = if newer {
                    let main = app.cfg.main_checkout.clone();
                    tokio::task::spawn_blocking(move || app_providing_tool(&main))
                        .await
                        .unwrap_or(None)
                } else {
                    None
                };
                let next = newer.then(|| UpdateInfo {
                    current: cur.clone(),
                    latest: tag.trim_start_matches('v').to_string(),
                    url,
                    tool,
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

    /// What the bar shows when an upgrade fails is the *end* of the output, and
    /// mise pads its errors with blank lines — so a naive last-N-lines would hand
    /// the bar an empty string and the failure would read as no reason at all.
    #[test]
    fn the_reported_tail_is_the_last_lines_that_say_something() {
        let noisy = "fetching\n\nunpacking\n\nmise ERROR no version set\nmise ERROR see --verbose\n\n";
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
        assert_eq!(upgrade_argv(&u.tool), vec!["mise", "upgrade", "claude-code"]);
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
        assert_eq!(parse(br#"{"bun":{"current":"1.0","latest":"1.1"}}"#, "claude-code"), None);
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
        assert_eq!(parse(br#"{"claude-code":{"requested":"latest"}}"#, "claude-code"), None);
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
            tool_owning(ls.as_bytes(), Path::new("/i/tools/orchestrator/1.0/orchestrator-desktop")).as_deref(),
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
