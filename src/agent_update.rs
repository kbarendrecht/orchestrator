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
//! button safe to offer. `mise` installs into a versioned directory and repoints;
//! a running `claude` keeps executing the image it already loaded. So sessions in
//! flight finish on the old version and every new one gets the new — no restart,
//! no downtime, nothing to coordinate.

use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::process::Command;

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
    let tool = providing_tool(main)?;
    // `--json` rather than the table: the human output is columns of padded
    // text, and `{}` for "nothing outdated" is unambiguous where an empty table
    // is not.
    let out = Command::new("mise")
        .args(["outdated", "--json"])
        .current_dir(main)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse(&out.stdout, &tool)
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
fn providing_tool(main: &Path) -> Option<String> {
    let out = Command::new("mise")
        .args(["which", "claude"])
        .current_dir(main)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    tool_of_install_path(String::from_utf8_lossy(&out.stdout).trim())
}

/// The tool name out of a mise install path, e.g. `…/installs/claude-code/2.1/claude`.
fn tool_of_install_path(path: &str) -> Option<String> {
    let after = path.split("/installs/").nth(1)?;
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
pub fn upgrade_argv(tool: &str) -> Vec<String> {
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
    /// The tail of the output, for a run that failed. Empty while it runs, and a
    /// success has none: it clears the run outright, because the nudge going away
    /// *is* the report and a bar saying "done" would be announcing no news.
    pub tail: String,
}

/// How long an upgrade may take before it is killed and reported as failed.
///
/// `mise upgrade` fetches and unpacks, so this is minutes rather than seconds —
/// but bounded, because the alternative is a bar that says "Upgrading…" forever
/// with no way to find out otherwise.
const UPGRADE_TIMEOUT_SECS: u64 = 300;

/// Run the upgrade, then say what happened.
///
/// Detached: the button answers immediately, and the bar follows the state through
/// the snapshot. `run_bounded` captures rather than streams, so there is no live
/// output to show — what a failure needs is the *end* of it, which is what a
/// captured tail is.
pub fn run_upgrade(app: std::sync::Arc<crate::state::AppState>, tool: String, to: String) {
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
        if let Err(e) = refresh(&app).await {
            tracing::warn!("re-checking the agent version after an upgrade failed: {e:#}");
        }

        {
            let mut inner = app.inner.write().await;
            inner.upgrade_run = match &failure {
                Some(text) => {
                    tracing::warn!("upgrading {tool} failed: {text}");
                    Some(UpgradeRun {
                        to,
                        running: false,
                        tail: text.clone(),
                    })
                }
                None => {
                    tracing::info!("upgraded {tool}");
                    None
                }
            };
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
/// Six hours, matching the daemon's own release check: Claude Code ships often,
/// but nothing here is urgent — the nag exists because the version is *usually*
/// a little behind, not because being behind breaks anything.
pub fn start_poller(app: std::sync::Arc<crate::state::AppState>) {
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(6 * 60 * 60);
        loop {
            let main = app.cfg.main_checkout.clone();
            // Off-thread: `mise outdated` reaches the network to learn the latest
            // version, and the runtime must not wait on it.
            if let Ok(next) = tokio::task::spawn_blocking(move || check(&main)).await {
                let mut inner = app.inner.write().await;
                if inner.agent_update != next {
                    inner.agent_update = next;
                    drop(inner);
                    app.notify().await;
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Check once, now, and publish the answer. The refresh button's other half.
pub async fn refresh(app: &std::sync::Arc<crate::state::AppState>) -> Result<()> {
    let main = app.cfg.main_checkout.clone();
    let next = tokio::task::spawn_blocking(move || check(&main)).await?;
    let mut inner = app.inner.write().await;
    if inner.agent_update != next {
        inner.agent_update = next;
        drop(inner);
        app.notify().await;
    }
    Ok(())
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
}
