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

/// Claude Code keys its transcript directory by working directory, slugging the
/// absolute path by replacing every `/` with `-`. Verified against a real run.
pub fn transcript_dir_for(cwd: &Path) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let slug = cwd.to_string_lossy().replace('/', "-");
    Ok(PathBuf::from(home).join(".claude/projects").join(slug))
}
