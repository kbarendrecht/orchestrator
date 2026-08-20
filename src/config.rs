use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// Everything the daemon needs to know about the machine it runs on.
///
/// Managed processes and test capabilities are config rather than hardcoded on
/// purpose (§7 rule 6) — the capability table has already changed once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The privileged checkout. Worktrees live inside it at [`Config::worktrees_dir`].
    pub main_checkout: PathBuf,
    /// A baked-in bundle of settings this config is merged *over*, so a machine
    /// only writes what is machine-specific (and whatever it wants to override).
    /// `default` is empty; `acme` supplies that stack's processes, test
    /// capabilities, tracker, upstream refs and review ranking. See
    /// [`Config::parse_with_profile`].
    #[serde(default)]
    pub profile: Profile,
    /// Where worktrees live, relative to `main_checkout`. Defaults to
    /// `.claude/worktrees`, which is both Claude Code's own `--worktree` default
    /// and where acme's `worktree-create` hook puts them — so a generic
    /// checkout needs no setting. A repo that relocates them (a `WorktreeCreate`
    /// hook) points this at the same place, so the daemon still recognises its
    /// own worktrees. Kept relative and in-main on purpose: the container path
    /// mapping, the changed-files exclude, and path attribution all assume
    /// worktrees sit under main.
    #[serde(default = "default_worktrees_subdir")]
    pub worktrees_subdir: PathBuf,
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
    /// The language the agent *writes* in — reviewer replies and story text.
    /// Prompts and code stay English; this is only the outward prose. The agent
    /// still matches a thread's own language first and falls back to this when
    /// that is unclear. Defaults to English; the `acme` profile sets Dutch.
    #[serde(default = "default_output_language")]
    pub output_language: String,
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

fn default_worktrees_subdir() -> PathBuf {
    PathBuf::from(".claude/worktrees")
}

/// Merge `over` onto `base`: two objects merge key-by-key (recursively), and
/// anything else — a scalar or an array — replaces wholesale. Arrays are not
/// element-merged on purpose: "my `main_processes`" should mean exactly the list
/// written, not the profile's list with edits spliced in.
fn deep_merge(base: &mut serde_json::Value, over: serde_json::Value) {
    use serde_json::Value::{Null, Object};
    match (base, over) {
        (Object(b), Object(o)) => {
            for (k, v) in o {
                deep_merge(b.entry(k).or_insert(Null), v);
            }
        }
        (b, o) => *b = o,
    }
}

/// Replace only `main_checkout` in a config file's raw JSON, leaving the rest —
/// including a slim profile-based shape — untouched.
fn rewrite_main_checkout(path: &Path, raw: &str, main: &Path) -> Result<()> {
    let mut v: serde_json::Value = serde_json::from_str(raw)?;
    let obj = v
        .as_object_mut()
        .context("config.json is not a JSON object")?;
    obj.insert(
        "main_checkout".into(),
        serde_json::Value::String(main.to_string_lossy().into_owned()),
    );
    std::fs::write(path, serde_json::to_string_pretty(&v)? + "\n")?;
    Ok(())
}

/// Portable default: the remote's own default branch, whatever it is named. A
/// fork workflow (PRs against an `upstream/develop`) sets its own — the `acme`
/// profile does.
fn default_upstream() -> String {
    "origin/HEAD".to_string()
}

fn default_auto_resume() -> bool {
    true
}

fn default_upstream_remote() -> String {
    "origin".to_string()
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

fn default_output_language() -> String {
    "English".to_string()
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

/// A named bundle of baked-in config a machine's `config.json` is merged over.
///
/// `default` is empty — a fresh checkout gets serde defaults. `acme` carries
/// that stack's processes, capabilities, tracker, upstream refs and review
/// ranking, so its many machines write only `{ main_checkout, profile }` plus
/// whatever they override. Adding a profile is a new arm here and a JSON file in
/// `src/profiles/`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    #[default]
    Default,
    acme,
}

impl Profile {
    /// The preset JSON this profile merges under a machine's config. `default` is
    /// empty; the rest are baked in with `include_str!`.
    fn preset(self) -> serde_json::Value {
        match self {
            Profile::Default => serde_json::json!({}),
            // Parsed from a checked-in file, so a malformed preset is a build-time
            // artefact we ship, not a runtime surprise; `expect` states that.
            Profile::acme => serde_json::from_str(include_str!("profiles/acme.json"))
                .expect("baked-in acme profile is valid JSON"),
        }
    }
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
        let cfg = Config::parse_with_profile(&raw)
            .map_err(|e| tracing::warn!("ignoring unparseable {}: {e:#}", path.display()))
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
            let mut cfg = Config::parse_with_profile(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            if let Some(main) = main_checkout {
                // Remember it. The desktop app reaches here when the recorded
                // checkout has moved and you have just pointed at the new one
                // in a dialog; being asked again on every launch would be the
                // app forgetting an answer you already gave.
                if cfg.main_checkout != main {
                    cfg.main_checkout = main.clone();
                    // Rewrite only `main_checkout` in the *raw* JSON, not the
                    // merged Config — re-serializing the whole thing would expand
                    // a slim profile-based config back to every field.
                    if let Err(e) = rewrite_main_checkout(&path, &raw, &main) {
                        tracing::warn!("could not record the new checkout in {}: {e:#}", path.display());
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

    /// Parse a `config.json` string, merged over its `profile`'s preset.
    ///
    /// The machine's keys win (deep object merge; arrays replace whole), so a
    /// config writes only what differs from the profile. `profile` is read from
    /// the raw JSON first, since it selects the base everything else merges onto;
    /// an unknown value falls back to `default` rather than failing the load.
    pub fn parse_with_profile(raw: &str) -> Result<Config> {
        let user: serde_json::Value = serde_json::from_str(raw).context("config is not JSON")?;
        let profile = match user.get("profile").and_then(|p| p.as_str()) {
            Some("acme") => Profile::acme,
            None | Some("default") => Profile::Default,
            Some(other) => {
                tracing::warn!("unknown profile {other:?}; using the default profile");
                Profile::Default
            }
        };
        let mut effective = profile.preset();
        deep_merge(&mut effective, user);
        // Pin the resolved profile so an unknown string cannot fail deserialize.
        if let Some(obj) = effective.as_object_mut() {
            obj.insert("profile".into(), serde_json::to_value(profile)?);
        }
        serde_json::from_value(effective).context("applying config over its profile")
    }

    fn default_for(main_checkout: PathBuf) -> Self {
        Config {
            main_checkout,
            profile: Profile::Default,
            worktrees_subdir: default_worktrees_subdir(),
            port: default_port(),
            // No managed processes on a fresh checkout: the daemon does not know
            // what a foreign repo runs, and autostarting a task that does not
            // exist is worse than starting nothing. A stack that has them (see
            // the `acme` profile) supplies its own; a first run can add them
            // to config.json.
            main_processes: vec![],
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
            output_language: default_output_language(),
            capabilities: Default::default(),
            todo_path: None,
            auto_resume: default_auto_resume(),
        }
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.main_checkout.join(self.safe_worktrees_subdir())
    }

    pub fn worktree_path(&self, name: &str) -> PathBuf {
        self.worktrees_dir().join(name)
    }

    /// The subdir as a git-porcelain-relative prefix (forward slashes), for the
    /// changed-files exclude in `git::status`. Ends with `/` so it matches a
    /// directory prefix rather than a sibling whose name merely starts the same.
    pub fn worktrees_subdir_str(&self) -> String {
        let s = self
            .safe_worktrees_subdir()
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect::<Vec<_>>()
            .join("/");
        format!("{s}/")
    }

    /// The configured subdir, or the default when it is not a relative in-main
    /// path. The whole model breaks if worktrees are not under main — the
    /// container mapping, the changed-files exclude and path attribution all
    /// assume it — so an absolute path or one climbing out with `..` is refused
    /// rather than trusted, the same tolerant-but-loud stance as `Config::existing`.
    fn safe_worktrees_subdir(&self) -> PathBuf {
        let sub = &self.worktrees_subdir;
        if sub.is_absolute() || sub.components().any(|c| matches!(c, Component::ParentDir)) {
            tracing::warn!(
                "worktrees_subdir {} is not a relative in-main path; using {}",
                sub.display(),
                default_worktrees_subdir().display()
            );
            return default_worktrees_subdir();
        }
        sub.clone()
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
    fn the_acme_ng_watch_recognises_the_esbuild_recovery_line() {
        // Its success line matches none of the older markers, so without this the
        // rail's `build failing` never cleared after a fixed compile. ng-watch is
        // a acme-profile process now, not a generic default.
        let cfg = Config::parse_with_profile(
            r#"{"main_checkout":"/tmp/x","profile":"acme"}"#,
        )
        .expect("parse");
        let ng = cfg
            .main_processes
            .iter()
            .find(|p| p.name == "ng-watch")
            .expect("ng-watch comes from the acme profile");
        assert!(ng.ok_patterns.iter().any(|p| p == "bundle generation complete"));
        assert!(!ng.autostart, "started by hand, matching the real acme config");
    }

    #[test]
    fn the_acme_profile_supplies_the_stacks_settings() {
        // A acme machine writes almost nothing; the preset fills the rest.
        let cfg = Config::parse_with_profile(
            r#"{"main_checkout":"/tmp/x","profile":"acme"}"#,
        )
        .expect("parse");
        assert_eq!(cfg.profile, Profile::acme);
        assert_eq!(cfg.review_ranking.coverage, crate::reviews::Coverage::AllOpen);
        assert_eq!(cfg.tracker, Tracker::Shortcut);
        assert_eq!(cfg.upstream_ref, "upstream/develop");
        assert_eq!(cfg.output_language, "Dutch");
        assert_eq!(cfg.main_processes.len(), 2);
        assert_eq!(cfg.capabilities.suites.len(), 4);
    }

    #[test]
    fn a_machine_key_overrides_the_profile() {
        // Deep-merge, config wins: a acme machine can still turn coverage down
        // or move the port without abandoning the profile.
        let cfg = Config::parse_with_profile(
            r#"{"main_checkout":"/tmp/x","profile":"acme","port":9000,
                "review_ranking":{"coverage":"requested"}}"#,
        )
        .expect("parse");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.review_ranking.coverage, crate::reviews::Coverage::Requested);
        // ...but an unmentioned key still comes from the profile.
        assert_eq!(cfg.tracker, Tracker::Shortcut);
    }

    #[test]
    fn the_default_profile_is_generic() {
        let cfg = Config::parse_with_profile(r#"{"main_checkout":"/tmp/x"}"#).expect("parse");
        assert_eq!(cfg.profile, Profile::Default);
        assert!(cfg.main_processes.is_empty(), "no processes assumed");
        assert_eq!(cfg.tracker, Tracker::None);
        // Portable base ref: the remote's default branch, not acme's fork.
        assert_eq!(cfg.upstream_ref, "origin/HEAD");
        assert_eq!(cfg.upstream_remote, "origin");
        assert_eq!(cfg.output_language, "English", "outward prose defaults to English");
        assert!(Config::default_for(PathBuf::from("/tmp/x")).main_processes.is_empty());
    }

    #[test]
    fn an_unknown_profile_falls_back_to_default() {
        let cfg = Config::parse_with_profile(
            r#"{"main_checkout":"/tmp/x","profile":"acme"}"#,
        )
        .expect("parse");
        assert_eq!(cfg.profile, Profile::Default);
    }

    #[test]
    fn deep_merge_recurses_objects_and_replaces_arrays() {
        let mut base = serde_json::json!({"a":{"x":1,"y":2},"list":[1,2,3]});
        deep_merge(&mut base, serde_json::json!({"a":{"y":9,"z":3},"list":[4]}));
        assert_eq!(base, serde_json::json!({"a":{"x":1,"y":9,"z":3},"list":[4]}));
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
    fn the_default_worktrees_dir_is_claude_worktrees_under_main() {
        let cfg = Config::default_for(PathBuf::from("/repo"));
        assert_eq!(cfg.worktrees_dir(), PathBuf::from("/repo/.claude/worktrees"));
        assert_eq!(cfg.worktree_path("inv"), PathBuf::from("/repo/.claude/worktrees/inv"));
        assert_eq!(cfg.worktrees_subdir_str(), ".claude/worktrees/");
    }

    #[test]
    fn a_custom_subdir_moves_the_dir_and_the_exclude_prefix() {
        let mut cfg = Config::default_for(PathBuf::from("/repo"));
        cfg.worktrees_subdir = PathBuf::from(".worktrees");
        assert_eq!(cfg.worktrees_dir(), PathBuf::from("/repo/.worktrees"));
        assert_eq!(cfg.worktrees_subdir_str(), ".worktrees/");
    }

    #[test]
    fn a_subdir_outside_main_falls_back_to_the_default() {
        // The container mapping, the exclude and path attribution all assume
        // worktrees sit under main, so an absolute or climbing path is refused.
        let mut cfg = Config::default_for(PathBuf::from("/repo"));
        cfg.worktrees_subdir = PathBuf::from("/tmp/elsewhere");
        assert_eq!(cfg.worktrees_dir(), PathBuf::from("/repo/.claude/worktrees"));
        cfg.worktrees_subdir = PathBuf::from("../escape");
        assert_eq!(cfg.worktrees_dir(), PathBuf::from("/repo/.claude/worktrees"));
    }

    #[test]
    fn an_old_config_without_a_worktrees_subdir_gets_the_default() {
        let cfg: Config = serde_json::from_str(r#"{"main_checkout":"/repo"}"#).expect("parse");
        assert_eq!(cfg.worktrees_subdir, PathBuf::from(".claude/worktrees"));
    }

    #[test]
    fn a_fresh_config_gets_the_default_forge_and_ranking() {
        // A new checkout anywhere gets a working queue with no setup.
        let cfg = Config::default_for(PathBuf::from("/tmp/x"));
        assert_eq!(cfg.forge, ForgeKind::GitHub);
        assert!(!cfg.review_ranking.rules.is_empty());
    }
}
