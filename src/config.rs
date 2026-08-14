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
    /// 288 queries/day is negligible against 5000 points/hour (§6).
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    /// `mise run reviews` walks every open PR, so it needs a generous ceiling
    /// but must not hang the poller forever.
    #[serde(default = "default_review_timeout")]
    pub review_timeout_seconds: u64,
    /// Whether sessions the daemon spawns write transcripts.
    ///
    /// This is decided here rather than inherited, because inheriting makes the
    /// daemon behave differently depending on what launched it: a shell inside
    /// a Claude Code session carries `CLAUDE_CODE_CHILD_SESSION`, which turns
    /// transcript saving off in every child. Silently losing transcripts breaks
    /// resume (§2) and leaves the teardown preflight with nothing to copy.
    ///
    /// Off is useful while developing the daemon itself — the throwaway
    /// sessions it spawns do not then litter your real session history.
    #[serde(default = "default_persist")]
    pub persist_transcripts: bool,
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

fn default_persist() -> bool {
    true
}

fn default_upstream_remote() -> String {
    "upstream".to_string()
}

fn default_poll_seconds() -> u64 {
    300
}

fn default_review_timeout() -> u64 {
    240
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
    pub fn load_or_init(main_checkout: Option<PathBuf>) -> Result<Self> {
        let path = Self::path()?;
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut cfg: Config = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            if let Some(main) = main_checkout {
                cfg.main_checkout = main;
            }
            return Ok(cfg);
        }

        let main = main_checkout.context(
            "no config yet — pass --main <path to the main checkout> on first run",
        )?;
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
                    command: vec![
                        "mise".into(),
                        "run".into(),
                        "silent:exec:toolbox".into(),
                        "ng".into(),
                        "build".into(),
                        "--watch".into(),
                    ],
                    // Angular error blocks. The first matching line becomes the
                    // summary shown in the rail.
                    failure_patterns: vec![
                        "Error:".into(),
                        "ERROR in".into(),
                        "error TS".into(),
                        "✘ [ERROR]".into(),
                    ],
                    ok_patterns: vec![
                        "Build at:".into(),
                        "successfully".into(),
                        "watching for file changes".into(),
                    ],
                    restart: RestartPolicy::Never,
                    autostart: false,
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
            review_timeout_seconds: default_review_timeout(),
            persist_transcripts: default_persist(),
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
/// Returns `(set, unset)`.
pub fn transcript_env(persist: bool) -> (Vec<(String, String)>, Vec<&'static str>) {
    if persist {
        (
            vec![(
                "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE".to_string(),
                "1".to_string(),
            )],
            vec!["CLAUDE_CODE_CHILD_SESSION"],
        )
    } else {
        // Set explicitly rather than left to chance, so "off" means off even
        // when the daemon was launched from a plain terminal.
        (
            vec![("CLAUDE_CODE_CHILD_SESSION".to_string(), "1".to_string())],
            vec!["CLAUDE_CODE_FORCE_SESSION_PERSISTENCE"],
        )
    }
}

/// Claude Code keys its transcript directory by working directory, slugging the
/// absolute path by replacing every `/` with `-`. Verified against a real run.
pub fn transcript_dir_for(cwd: &Path) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let slug = cwd.to_string_lossy().replace('/', "-");
    Ok(PathBuf::from(home).join(".claude/projects").join(slug))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_on_clears_the_child_marker() {
        let (set, unset) = transcript_env(true);
        assert!(unset.contains(&"CLAUDE_CODE_CHILD_SESSION"));
        assert!(set
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE" && v == "1"));
    }

    #[test]
    fn persistence_off_sets_the_marker_rather_than_hoping_for_it() {
        // "Off" must mean off even when the daemon was launched from a plain
        // terminal that carries no marker of its own.
        let (set, unset) = transcript_env(false);
        assert!(set
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_CHILD_SESSION" && v == "1"));
        assert!(unset.contains(&"CLAUDE_CODE_FORCE_SESSION_PERSISTENCE"));
    }

    #[test]
    fn the_two_directions_never_agree() {
        // Whatever launched the daemon, exactly one of the two vars is set.
        let (on_set, on_unset) = transcript_env(true);
        let (off_set, off_unset) = transcript_env(false);
        for (k, _) in &on_set {
            assert!(off_unset.contains(&k.as_str()), "{k} not cleared when off");
        }
        for (k, _) in &off_set {
            assert!(on_unset.contains(&k.as_str()), "{k} not cleared when on");
        }
    }

    #[test]
    fn a_config_without_the_field_persists() {
        // Existing config files predate the flag; the safe reading is on.
        let cfg: Config = serde_json::from_str(
            r#"{"main_checkout":"/tmp","port":7777,"upstream_ref":"upstream/develop"}"#,
        )
        .expect("parse");
        assert!(cfg.persist_transcripts);
    }
}
