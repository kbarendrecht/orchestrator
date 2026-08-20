use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Everything the daemon needs to know about the machine it runs on.
///
/// Managed processes and test capabilities are config rather than hardcoded on
/// purpose (§7 rule 6) — the capability table has already changed once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The privileged checkout. Worktrees live inside it at `.claude/worktrees/`.
    pub main_checkout: PathBuf,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Managed processes declared for the main workspace. Worktrees declare none
    /// by default; a shell is opened on demand instead.
    #[serde(default)]
    pub main_processes: Vec<ManagedSpec>,
    #[serde(default)]
    pub worktree_processes: Vec<ManagedSpec>,
    /// Upstream ref the diff and worktree bases resolve against.
    #[serde(default = "default_upstream")]
    pub upstream_ref: String,
    /// Remote the PRs live on. Fork workflow: PRs are opened against upstream
    /// while head refs live on origin (§6).
    #[serde(default = "default_upstream_remote")]
    pub upstream_remote: String,
    /// `owner/name` override; derived from the upstream remote when absent.
    #[serde(default)]
    pub repo: Option<String>,
    /// A `0600` file holding a read-only GitHub token, outside the repo.
    #[serde(default)]
    pub github_token_file: Option<PathBuf>,
    /// Which tracker a `story+reply` position files into.
    ///
    /// Explicit, and defaulting to `None`, rather than auto-detected from whether
    /// a token happens to resolve. Auto-detection would let an expired token
    /// silently remove an option from every triage run, leaving "triage did not
    /// propose a story" indistinguishable from "the daemon hid it".
    #[serde(default)]
    pub tracker: Tracker,
    /// A `0600` file holding the Shortcut API token. `ORCHD_SHORTCUT_TOKEN` wins
    /// over it, mirroring the GitHub ladder.
    #[serde(default)]
    pub shortcut_token_file: Option<PathBuf>,
    /// How long the borrowed story-filing agent gets before it is killed.
    ///
    /// The one timeout in this daemon, because it is the one agent whose caller is
    /// a blocking HTTP request rather than a rail entry somebody is watching. Sized
    /// from the skill's real workload — read an epic, search, create, follow up —
    /// so minutes, not seconds.
    #[serde(default = "default_story_timeout")]
    pub story_timeout_seconds: u64,
    /// 288 queries/day is negligible against 5000 points/hour (§6).
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    /// Which forge the repo lives on. Only GitHub is implemented; the field
    /// exists so a second platform is a config choice rather than a rebuild.
    #[serde(default)]
    pub forge: ForgeKind,
    /// How the review queue is classified and ordered. Config, not hardcoded, so
    /// a repo with different labels or priorities does not need a patched daemon.
    /// The default reproduces the buckets the queue shipped with (stopper / prio
    /// / requested-of-you / of-your-team / re-review / other / sidequest).
    #[serde(default)]
    pub review_ranking: crate::reviews::ReviewRanking,
    /// Test capabilities per suite. Config rather than hardcoded, because the
    /// table has already changed once (§7 rule 6).
    #[serde(default)]
    pub capabilities: crate::capability::CapabilityConfig,
    /// Where the auto-updated findings block lives. Defaults to the
    /// orchestrator's own TODO.md.
    #[serde(default)]
    pub todo_path: Option<PathBuf>,
    /// Bring back sessions that were live when the daemon last went down.
    ///
    /// The daemon owns every pty, so a crash — or a reboot — takes every Claude
    /// process with it. This relaunches them with `--resume` so a crash costs
    /// you the scrollback rather than the conversation.
    #[serde(default = "default_auto_resume")]
    pub auto_resume: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedSpec {
    pub name: String,
    pub command: Vec<String>,
    /// Substring that marks a line as an error when parsing health from output.
    #[serde(default)]
    pub failure_patterns: Vec<String>,
    /// Substring that marks the process as healthy again.
    #[serde(default)]
    pub ok_patterns: Vec<String>,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Off by default: `docker compose up` is not something to launch behind
    /// your back when the daemon starts.
    #[serde(default)]
    pub autostart: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    #[default]
    Never,
    OnFailure,
}

fn default_port() -> u16 {
    7777
}

fn default_upstream() -> String {
    "upstream/develop".to_string()
}

fn default_auto_resume() -> bool {
    true
}

fn default_upstream_remote() -> String {
    "upstream".to_string()
}

fn default_poll_seconds() -> u64 {
    300
}

/// Where a `story+reply` position's story goes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tracker {
    /// No tracker. `story+reply` is never offered and would be refused.
    #[default]
    None,
    /// The Shortcut MCP from the repo's own `.mcp.json`.
    Shortcut,
    /// A stub MCP server that speaks the same tool names and records what it was
    /// asked to do. For proving the plumbing without filing a real story.
    Stub,
}

impl Tracker {
    pub fn is_configured(self) -> bool {
        !matches!(self, Tracker::None)
    }
}

fn default_story_timeout() -> u64 {
    300
}

/// Which code-hosting platform the repo lives on. The read/write seam is
/// `crate::forge`; adding a platform is a new arm here plus a new `Forge` impl,
/// not a change to any caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeKind {
    #[default]
    GitHub,
}

impl Config {
    pub fn config_dir() -> Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".config/orchd"))
    }

    pub fn path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.json"))
    }

    /// Load config, writing a default one on first run so there is something to edit.
    /// The config, if there is a usable one already.
    ///
    /// Usable means more than present: a config naming a checkout that has been
    /// moved or deleted is worse than none, because the failure surfaces later
    /// and further from the cause. The desktop app asks this before starting
    /// and shows a folder picker when the answer is `None`.
    pub fn existing() -> Option<Self> {
        let path = Self::path().ok()?;
        let raw = std::fs::read_to_string(&path).ok()?;
        let cfg: Config = serde_json::from_str(&raw)
            .map_err(|e| tracing::warn!("ignoring unparseable {}: {e}", path.display()))
            .ok()?;
        if !cfg.main_checkout.join(".git").exists() {
            tracing::warn!(
                "{} names {}, which is not a git checkout",
                path.display(),
                cfg.main_checkout.display()
            );
            return None;
        }
        Some(cfg)
    }

    pub fn load_or_init(main_checkout: Option<PathBuf>) -> Result<Self> {
        let path = Self::path()?;
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut cfg: Config = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            if let Some(main) = main_checkout {
                // Remember it. The desktop app reaches here when the recorded
                // checkout has moved and you have just pointed at the new one
                // in a dialog; being asked again on every launch would be the
                // app forgetting an answer you already gave.
                if cfg.main_checkout != main {
                    cfg.main_checkout = main;
                    if let Err(e) = std::fs::write(&path, serde_json::to_string_pretty(&cfg)?) {
                        tracing::warn!(
                            "could not record the new checkout in {}: {e}",
                            path.display()
                        );
                    }
                }
            }
            return Ok(cfg);
        }

        let main = main_checkout
            .context("no config yet — pass --main <path to the main checkout> on first run")?;
        let cfg = Config::default_for(main);
        std::fs::create_dir_all(Self::config_dir()?)?;
        std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)?;
        tracing::info!("wrote default config to {}", path.display());
        Ok(cfg)
    }

    fn default_for(main_checkout: PathBuf) -> Self {
        Config {
            main_checkout,
            port: default_port(),
            // Main declares the two long-running processes the spec names (§2).
            main_processes: vec![
                ManagedSpec {
                    name: "ng-watch".to_string(),
                    // The repo's own task, rather than this daemon's idea of how
                    // to invoke Angular: it is one name to keep in step.
                    command: vec!["mise".into(), "run".into(), "watch".into()],
                    // Angular error blocks. The first matching line becomes the
                    // summary shown in the rail.
                    failure_patterns: vec![
                        "Error:".into(),
                        "ERROR in".into(),
                        "error TS".into(),
                        "✘ [ERROR]".into(),
                        // The esbuild builder's own summary line, in case an error
                        // block ever lands without the ✘ prefix.
                        "bundle generation failed".into(),
                    ],
                    // Every builder has a different "all clear" line, and health is
                    // latched until one is seen: miss the recovery line and a fixed
                    // build stays red in the rail forever. `successfully` covers the
                    // webpack builder (`Compiled successfully.`), but the esbuild one
                    // says `Application bundle generation complete.` instead — which
                    // was the lingering-error bug.
                    ok_patterns: vec![
                        "Build at:".into(),
                        "successfully".into(),
                        "watching for file changes".into(),
                        "bundle generation complete".into(),
                    ],
                    restart: RestartPolicy::Never,
                    // The one process that starts by itself. A build watcher is
                    // no use started by hand five minutes after you needed it,
                    // and unlike `docker compose up` it touches nothing outside
                    // the checkout.
                    autostart: true,
                },
                ManagedSpec {
                    name: "docker".to_string(),
                    command: vec!["docker".into(), "compose".into(), "up".into()],
                    failure_patterns: vec!["exited with code".into(), "Error response".into()],
                    ok_patterns: vec!["Started".into(), "Attaching to".into()],
                    restart: RestartPolicy::Never,
                    autostart: false,
                },
            ],
            worktree_processes: vec![],
            upstream_ref: default_upstream(),
            upstream_remote: default_upstream_remote(),
            repo: None,
            github_token_file: None,
            poll_seconds: default_poll_seconds(),
            forge: ForgeKind::default(),
            // The queue is built in and asks the forge directly, so a fresh
            // checkout needs no setup; the default ranking reproduces the buckets
            // it shipped with.
            review_ranking: Default::default(),
            tracker: Tracker::None,
            shortcut_token_file: None,
            story_timeout_seconds: default_story_timeout(),
            capabilities: Default::default(),
            todo_path: None,
            auto_resume: default_auto_resume(),
        }
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.main_checkout.join(".claude/worktrees")
    }

    pub fn worktree_path(&self, name: &str) -> PathBuf {
        self.worktrees_dir().join(name)
    }

    /// Path of the daemon-owned settings file handed to every spawned session.
    ///
    /// Deliberately not `~/.claude/settings.json` (§3): global config would make
    /// every Claude session on the machine POST to the daemon, unrelated repos
    /// included, and each would pay the hook timeout while the daemon is down.
    /// Verified at spike time that `--settings` *merges* with project and user
    /// settings rather than replacing them, so the repo's own
    /// `worktree-edit-boundary` and `pre-bash` hooks keep firing.
    pub fn hooks_settings_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("hooks.json"))
    }
}

/// The environment a spawned Claude session gets, so the outcome never depends
/// on which shell started the daemon.
///
/// Transcripts are always on: resume (§2) and the teardown transcript check both
/// need one, and a session without a transcript costs you the conversation. Set
/// explicitly rather than inherited, because a shell inside a Claude Code session
/// carries `CLAUDE_CODE_CHILD_SESSION`, which turns transcript saving off in every
/// child — so without clearing it the daemon would behave differently depending on
/// what launched it.
///
/// Returns `(set, unset)`.
pub fn transcript_env() -> (Vec<(String, String)>, Vec<&'static str>) {
    (
        vec![(
            "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE".to_string(),
            "1".to_string(),
        )],
        vec!["CLAUDE_CODE_CHILD_SESSION"],
    )
}

/// Claude Code keys its transcript directory by working directory, slugging the
/// absolute path by replacing every `/` **and every `.`** with `-`.
///
/// The dots matter here more than anywhere else: worktrees live under
/// `.claude/worktrees/`, so slugging only the slashes produced
/// `…-acme-.claude-worktrees-x` against a real `…-acme--claude-worktrees-x`
/// — a directory that never exists. Every worktree session therefore looked like
/// it had no transcript, which is what auto-resume and the teardown transcript
/// check both read to decide there was nothing to resume or copy.
pub fn transcript_dir_for(cwd: &Path) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".claude/projects")
        .join(transcript_slug(cwd)))
}

/// The slug half of [`transcript_dir_for`], so the rule can be tested without a
/// test reaching into `HOME` and changing it under every other test.
fn transcript_slug(cwd: &Path) -> String {
    cwd.to_string_lossy().replace(['/', '.'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worktree_slug_collapses_the_dot_as_well_as_the_slashes() {
        // The real directory name for a worktree at `<main>/.claude/worktrees/x`.
        // Slugging only the slashes gave `-.claude-`, which exists nowhere, so
        // every worktree session read as having no transcript.
        assert_eq!(
            transcript_slug(Path::new("/home/x/dev/acme/.claude/worktrees/dfafdf")),
            "-home-x-dev-acme--claude-worktrees-dfafdf"
        );
    }

    #[test]
    fn the_default_watch_recognises_the_esbuild_recovery_line() {
        // Its success line matches none of the older markers, so without this the
        // rail's `build failing` never cleared after a fixed compile.
        let cfg = Config::default_for(PathBuf::from("/tmp/x"));
        let ng = cfg
            .main_processes
            .iter()
            .find(|p| p.name == "ng-watch")
            .expect("ng-watch is a default process");
        assert!(ng
            .ok_patterns
            .iter()
            .any(|p| p == "bundle generation complete"));
    }

    #[test]
    fn persistence_clears_the_child_marker() {
        // The marker is what a daemon launched from inside a Claude session
        // inherits, and it silently turns transcripts off in every child.
        let (set, unset) = transcript_env();
        assert!(unset.contains(&"CLAUDE_CODE_CHILD_SESSION"));
        assert!(set
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE" && v == "1"));
    }

    #[test]
    fn a_config_still_parses_with_the_dropped_flag_in_it() {
        // `persist_transcripts` was a config key until it was always-on; a file
        // written back then must still load.
        let cfg: Config = serde_json::from_str(
            r#"{"main_checkout":"/tmp","port":7777,"upstream_ref":"upstream/develop",
                "persist_transcripts":false}"#,
        )
        .expect("parse");
        assert_eq!(cfg.main_checkout, PathBuf::from("/tmp"));
    }

    #[test]
    fn an_old_config_with_a_reviews_command_still_parses() {
        // `reviews_command` was a config key until the queue became built in; a
        // file written back then must still load, its stale key ignored.
        let cfg: Config = serde_json::from_str(
            r#"{"main_checkout":"/tmp","reviews_command":["mise","run","reviews","--json"],
                "review_timeout_seconds":240}"#,
        )
        .expect("parse");
        assert_eq!(cfg.main_checkout, PathBuf::from("/tmp"));
    }

    #[test]
    fn a_fresh_config_gets_the_default_forge_and_ranking() {
        // A new checkout anywhere gets a working queue with no setup.
        let cfg = Config::default_for(PathBuf::from("/tmp/x"));
        assert_eq!(cfg.forge, ForgeKind::GitHub);
        assert!(!cfg.review_ranking.rules.is_empty());
    }
}
