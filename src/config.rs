use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// Everything the daemon needs to know about the machine it runs on.
///
/// Managed processes are config rather than hardcoded on purpose, so a heavier
/// stack is a set of values rather than a special case baked into the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The privileged checkout. Worktrees live inside it at [`Config::worktrees_dir`].
    pub main_checkout: PathBuf,
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
    /// by default; a shell is opened on demand instead. The default is the two a
    /// acme checkout wants (a build watcher and `docker compose`), both
    /// `autostart:false`; edit them in the settings panel.
    #[serde(default = "default_main_processes")]
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
    /// Explicit rather than auto-detected from whether a token happens to resolve.
    /// Auto-detection would let an expired token silently remove an option from
    /// every triage run, leaving "triage did not propose a story" indistinguishable
    /// from "the daemon hid it". Defaults to Shortcut; set it to `none` for a repo
    /// with no tracker.
    #[serde(default = "default_tracker")]
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
    /// that is unclear. Defaults to Dutch; set it to `English` (or any language)
    /// in the settings panel.
    #[serde(default = "default_output_language")]
    pub output_language: String,
    /// The PR poll is one query per period, negligible against 5000 points/hour
    /// (§6). The review poll shells out to [`Config::reviews_command`], whose cost
    /// is that command's business, bounded by [`Config::review_timeout_seconds`].
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    /// Which forge the repo lives on. Only GitHub is implemented; the field
    /// exists so a second platform is a config choice rather than a rebuild.
    #[serde(default)]
    pub forge: ForgeKind,
    /// How long the review-queue command may run before the poller gives up on
    /// it. `mise run reviews` walks every open PR, so it needs a generous ceiling
    /// but must not hang the poller forever.
    #[serde(default = "default_review_timeout")]
    pub review_timeout_seconds: u64,
    /// The command whose JSON output feeds the review-queue pane (the shape in
    /// `docs/reviews-json.md`). An argv, run under `timeout` in the main checkout.
    ///
    /// Defaults to `mise run reviews --json`. Empty means "no review queue here",
    /// and the pane says so rather than reading as a broken command — clear the
    /// field in settings for a repo with no such task.
    #[serde(default = "default_reviews_command")]
    pub reviews_command: Vec<String>,
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
    /// A command run in every worktree the daemon cuts itself, right after
    /// creation and before the session spawns. An argv, run with cwd set to the
    /// new worktree.
    ///
    /// This is the seam that makes a repo's worktree setup a first-class thing
    /// rather than an accident of who created the tree. Claude Code's own
    /// `WorktreeCreate` hook fires only for `claude --worktree`, so a PR worktree
    /// or a resumed one — both cut by the daemon with plain `git worktree add` —
    /// silently skips whatever that hook does. acme's, for one, writes the
    /// `claudeMdExcludes` file that stops rules double-loading, and its PR
    /// worktrees were missing it. Point this at a script that does the
    /// creation-time setup and every daemon-cut worktree gets it.
    ///
    /// Empty by default: a plain checkout needs nothing here, and it never runs on
    /// the `claude --worktree` path, where the repo's own `WorktreeCreate` already
    /// did the work — running both would double it.
    #[serde(default)]
    pub worktree_setup: Vec<String>,
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

/// A clean relative in-main worktrees subdir, or `None` when the configured
/// value is unusable and the default should stand in.
///
/// The whole model breaks if worktrees are not under main — the container
/// mapping, the changed-files exclude and path attribution all assume it — so an
/// absolute path, one climbing out with `..`, or one that normalises to nothing
/// (`""`, `"."`, `"./"`) is refused. `.` components are dropped, so `./worktrees`
/// and `worktrees` mean the same thing; without that the exclude prefix would be
/// `./worktrees/` while git porcelain emits `worktrees/…`, and the §2 sibling
/// leak the exclude prevents would silently reopen.
fn normalize_worktrees_subdir(sub: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in sub.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            // Absolute (RootDir/Prefix) or climbing (ParentDir): not in-main.
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Replace only `main_checkout` in a config file's raw JSON, leaving every other
/// key — and a slim `{ main_checkout }` shape — untouched.
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

/// The subset of [`Config`] the settings panel reads and writes.
///
/// A distinct struct so the editable surface is explicit: a POST from the panel
/// can set these seven and nothing else — not the port, the token paths, or the
/// forge. Field names match the `config.json` keys they persist to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub output_language: String,
    pub tracker: Tracker,
    pub upstream_ref: String,
    pub upstream_remote: String,
    pub reviews_command: Vec<String>,
    pub main_processes: Vec<ManagedSpec>,
    pub worktree_setup: Vec<String>,
}

impl Settings {
    pub fn of(cfg: &Config) -> Self {
        Settings {
            output_language: cfg.output_language.clone(),
            tracker: cfg.tracker,
            upstream_ref: cfg.upstream_ref.clone(),
            upstream_remote: cfg.upstream_remote.clone(),
            reviews_command: cfg.reviews_command.clone(),
            main_processes: cfg.main_processes.clone(),
            worktree_setup: cfg.worktree_setup.clone(),
        }
    }

    /// Persist these into `config.json`, touching only their six keys — the same
    /// reparse-the-raw-file reason as [`rewrite_main_checkout`], so a slim
    /// `{ main_checkout }` config stays slim. Takes effect on the next start;
    /// nothing here mutates the running `cfg`.
    pub fn write(&self) -> Result<()> {
        let path = Config::path()?;
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".to_string());
        std::fs::write(&path, self.merge_into(&raw)?)?;
        Ok(())
    }

    /// Set these seven keys on a raw `config.json` string, returning the new file
    /// text. Split from [`Settings::write`] so it is testable without the real
    /// config path.
    pub fn merge_into(&self, raw: &str) -> Result<String> {
        let mut v: serde_json::Value =
            serde_json::from_str(raw).context("config.json is not JSON")?;
        let obj = v
            .as_object_mut()
            .context("config.json is not a JSON object")?;
        obj.insert("output_language".into(), serde_json::to_value(&self.output_language)?);
        obj.insert("tracker".into(), serde_json::to_value(self.tracker)?);
        obj.insert("upstream_ref".into(), serde_json::to_value(&self.upstream_ref)?);
        obj.insert("upstream_remote".into(), serde_json::to_value(&self.upstream_remote)?);
        obj.insert("reviews_command".into(), serde_json::to_value(&self.reviews_command)?);
        obj.insert("main_processes".into(), serde_json::to_value(&self.main_processes)?);
        obj.insert("worktree_setup".into(), serde_json::to_value(&self.worktree_setup)?);
        Ok(serde_json::to_string_pretty(&v)? + "\n")
    }
}

/// The fork-workflow base: PRs against `upstream/develop`. A repo that merges to
/// its origin's default branch sets `upstream_ref`/`upstream_remote` in settings.
fn default_upstream() -> String {
    "upstream/develop".to_string()
}

fn default_auto_resume() -> bool {
    true
}

fn default_upstream_remote() -> String {
    "upstream".to_string()
}

fn default_tracker() -> Tracker {
    Tracker::Shortcut
}

fn default_reviews_command() -> Vec<String> {
    ["mise", "run", "reviews", "--json"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// The two processes a acme main checkout wants, both started by hand: the
/// Angular build watcher and the `docker compose` stack. The former runs through
/// the repo's toolbox wrapper; the patterns are what the health dot reads.
fn default_main_processes() -> Vec<ManagedSpec> {
    vec![
        ManagedSpec {
            name: "ng-watch".to_string(),
            command: ["mise", "run", "silent:exec:toolbox", "ng", "build", "--watch"]
                .into_iter()
                .map(String::from)
                .collect(),
            failure_patterns: [
                "Error:",
                "ERROR in",
                "error TS",
                "✘ [ERROR]",
                "bundle generation failed",
                // The runner's own failures, which none of the compiler patterns
                // above catch: mise prefixes them `mise ERROR`, so a task that
                // dies before Angular ever prints anything used to leave the
                // watch looking healthy.
                "mise ERROR",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            // Matching is case-sensitive and Angular writes "Watching for file
            // changes", so the lowercase spelling here never fired; the esbuild
            // line below is what actually reports a healthy watch today. The two
            // webpack-era strings stay for an older builder.
            ok_patterns: [
                "bundle generation complete",
                "Watching for file changes",
                "Build at:",
                "successfully",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            restart: RestartPolicy::Never,
            autostart: false,
        },
        ManagedSpec {
            name: "docker".to_string(),
            command: ["docker", "compose", "up"]
                .into_iter()
                .map(String::from)
                .collect(),
            failure_patterns: ["exited with code", "Error response"]
                .into_iter()
                .map(String::from)
                .collect(),
            ok_patterns: ["Started", "Attaching to"]
                .into_iter()
                .map(String::from)
                .collect(),
            restart: RestartPolicy::Never,
            autostart: false,
        },
    ]
}

fn default_poll_seconds() -> u64 {
    300
}

fn default_review_timeout() -> u64 {
    240
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
    "Dutch".to_string()
}

/// Which code-hosting platform the repo lives on. The read/write seam is
/// `crate::forge`; adding a platform is a new arm here plus a new `Forge` impl,
/// not a change to any caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeKind {
    /// Spelled out rather than left to `rename_all`, which would make this
    /// `git_hub`: nobody writes that, and the obvious `"github"` would then be
    /// an unknown variant — a hard parse error that reads as "no config at all"
    /// (the desktop app re-shows the folder picker). The alias keeps a config
    /// already written with the generated spelling loading.
    #[default]
    #[serde(rename = "github", alias = "git_hub")]
    GitHub,
}

/// Where state lives under a given home, with the platform split in one place.
///
/// Split out from [`Config::config_dir`] with `home` injected so it is testable:
/// the alternative is a test that mutates `HOME`, which is process-global and
/// races every other test in a parallel suite.
fn default_config_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/orchd")
    } else {
        home.join(".config/orchd")
    }
}

impl Config {
    /// Everything durable hangs off here: the config, the session store, the
    /// automation and story records, the hook settings, the instance lock.
    ///
    /// `ORCHD_CONFIG_DIR` moves the lot, which is what makes the review fixture
    /// (`tools/fixture-pr.mjs`) safe to point a daemon at. Without it a fixture
    /// run writes throwaway sessions into the real `sessions.json`, rewrites
    /// `main_checkout` to a scratch clone, and — because `todo_path` defaults to
    /// this repo's own TODO.md — puts the live-findings block of a fake repo into
    /// a tracked file. Overriding `HOME` would relocate all of it for free and is
    /// the wrong lever: `claude` reads its credentials from there, so every
    /// session the fixture daemon spawned would come up unauthenticated.
    ///
    /// An empty value is ignored rather than honoured, because `PathBuf::from("")`
    /// is a relative path and the state would land wherever the daemon was started.
    ///
    /// The default is per-platform: `~/.config/orchd` is a Linux convention, and a
    /// Mac keeps application state under `~/Library/Application Support`, where a
    /// Mac user would actually look for it. Nothing is migrated between the two
    /// because there is nothing to migrate — the app has never run on macOS, so no
    /// `~/.config/orchd` exists there to find. Anyone who prefers one spelling on
    /// either platform sets `ORCHD_CONFIG_DIR`.
    ///
    /// **That macOS path contains a space**, which is not merely cosmetic: any
    /// place a path from here reaches a shell has to quote it. See `sh_quote` in
    /// `hooks.rs` — the push guard's hook is a shell string, and an unquoted path
    /// there means the guard silently stops existing.
    pub fn config_dir() -> Result<PathBuf> {
        if let Some(dir) = std::env::var_os("ORCHD_CONFIG_DIR").filter(|d| !d.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
        /* Never the real one from a test binary. `AppState::persist` writes the
           whole session set on every state change, so any test that built an
           `AppState` and touched a session **overwrote the developer's own
           `sessions.json`** with the one record it had invented — silently, and on
           every `cargo test`. Found by watching five real records become one.

           A temp dir keyed to the process rather than a no-op, so `save`/`load`
           still round-trip honestly; and after the `ORCHD_CONFIG_DIR` check, so a
           test that wants a specific dir can still say so. */
        #[cfg(test)]
        {
            let dir = std::env::temp_dir().join(format!("orchd-test-cfg-{}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            return Ok(dir);
        }
        #[cfg(not(test))]
        {
            let home = PathBuf::from(std::env::var("HOME").context("HOME is not set")?);
            Ok(default_config_dir(&home))
        }
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
        let cfg = Config::parse(&raw)
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
            let mut cfg = Config::parse(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            if let Some(main) = main_checkout {
                // Remember it. The desktop app reaches here when the recorded
                // checkout has moved and you have just pointed at the new one
                // in a dialog; being asked again on every launch would be the
                // app forgetting an answer you already gave.
                if cfg.main_checkout != main {
                    cfg.main_checkout = main.clone();
                    // Rewrite only `main_checkout` in the *raw* JSON, not the
                    // parsed Config — re-serializing the whole thing would expand
                    // a slim `{ main_checkout }` file back to every field.
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

    /// Parse a `config.json` string into a [`Config`].
    ///
    /// Everything unset falls back to the `#[serde(default = …)]` attributes,
    /// which now carry the values a acme checkout wants. An old file with a
    /// stale `"profile"` key still loads — serde ignores the unknown field.
    pub fn parse(raw: &str) -> Result<Config> {
        let mut cfg: Config = serde_json::from_str(raw).context("config is not JSON")?;
        // Sanitise once, here, so every accessor can trust the field and the
        // warning fires at load rather than on every hook event.
        cfg.worktrees_subdir = match normalize_worktrees_subdir(&cfg.worktrees_subdir) {
            Some(clean) => clean,
            None => {
                tracing::warn!(
                    "worktrees_subdir {} is not a relative in-main path; using {}",
                    cfg.worktrees_subdir.display(),
                    default_worktrees_subdir().display()
                );
                default_worktrees_subdir()
            }
        };
        // The base fetch (`git::fetch_upstream`) splits its remote out of
        // `upstream_ref`, while repo detection reads `upstream_remote`; if they
        // name different remotes the merge-base and the resolved repo drift apart
        // with nothing to say so. They are equal in every profile and default, so
        // a mismatch is a hand-edit slip worth surfacing.
        if let Some((ref_remote, _)) = cfg.upstream_ref.split_once('/') {
            if ref_remote != cfg.upstream_remote {
                tracing::warn!(
                    "upstream_ref {:?} uses remote {:?}, but upstream_remote is {:?}; \
                     the base fetch and repo detection will disagree",
                    cfg.upstream_ref,
                    ref_remote,
                    cfg.upstream_remote
                );
            }
        }
        /* Resolved once, here, so every path derived from it is resolved too —
           `worktrees_dir`, `worktree_path`, and so the workspace paths that
           `workspace_for_path` matches hook paths against.

           That match is the reason. A `PostToolUse` path goes through
           `canonicalize` before it is attributed (`hooks.rs`, so a `.plan/`
           symlink lands in the right pane), and comparing a resolved path against
           an unresolved workspace root simply fails: the edit is attributed to no
           workspace and quietly never reaches the changed-files pane. Only the
           `--main` argument was resolved before this, so a checkout named in
           `config.json` was not.

           Latent on Linux, where `$HOME` rarely contains a symlink, and much less
           so on macOS: `/tmp`, `/var` and therefore `$TMPDIR` are all symlinks
           into `/private`.

           Falling back to the value as written is deliberate — a path that does
           not resolve yet is `validate`'s complaint to make, with the checkout it
           actually names, not something this silently rewrites. */
        cfg.main_checkout =
            std::fs::canonicalize(&cfg.main_checkout).unwrap_or(cfg.main_checkout);
        Ok(cfg)
    }

    /// The config a first run writes: only the checkout, everything else its serde
    /// default. Built *through* `parse` rather than by hand so a first run and the
    /// same file parsed from disk can never diverge — the field defaults live in
    /// one place (the `#[serde(default = …)]` attributes), not two.
    ///
    /// Those defaults are now acme's (Dutch, Shortcut, `upstream/develop`, the
    /// two managed processes, `mise run reviews`); a checkout that wants otherwise
    /// edits them in the settings panel. No special-casing here.
    fn default_for(main_checkout: PathBuf) -> Self {
        let raw = serde_json::json!({ "main_checkout": main_checkout }).to_string();
        // Cannot fail: the JSON is a single known-valid key.
        Self::parse(&raw).expect("the default config is valid")
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        // The field is sanitised in `parse`, so it is a clean
        // relative in-main path here.
        self.main_checkout.join(&self.worktrees_subdir)
    }

    pub fn worktree_path(&self, name: &str) -> PathBuf {
        self.worktrees_dir().join(name)
    }

    /// The subdir as a git-porcelain-relative prefix (forward slashes), for the
    /// changed-files exclude in `git::status`. Ends with `/` so it matches a
    /// directory prefix rather than a sibling whose name merely starts the same.
    pub fn worktrees_subdir_str(&self) -> String {
        let s = self
            .worktrees_subdir
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect::<Vec<_>>()
            .join("/");
        format!("{s}/")
    }

    /// Whether worktrees live where `claude --worktree` puts them.
    ///
    /// That command has no flag for the location — it always writes to
    /// `<repo>/.claude/worktrees/<name>` — so it can only be trusted to create a
    /// worktree the daemon will then find when the two agree. Anywhere else, the
    /// daemon cuts the worktree itself (`spawn::spawn_worktree_session`).
    pub fn worktrees_subdir_is_claude_default(&self) -> bool {
        self.worktrees_subdir == default_worktrees_subdir()
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
    fn the_default_ng_watch_recognises_the_esbuild_recovery_line() {
        // Its success line matches none of the older markers, so without this the
        // rail's `build failing` never cleared after a fixed compile. ng-watch is
        // a built-in default now, present without any profile.
        let cfg = Config::parse(r#"{"main_checkout":"/tmp/x"}"#).expect("parse");
        let ng = cfg
            .main_processes
            .iter()
            .find(|p| p.name == "ng-watch")
            .expect("ng-watch is a default process");
        assert!(ng.ok_patterns.iter().any(|p| p == "bundle generation complete"));
        assert!(!ng.autostart, "started by hand, matching the real acme config");
    }

    #[test]
    fn a_bare_config_gets_the_acme_defaults() {
        // The six settings the acme profile used to carry are the defaults now,
        // so a checkout writes only `main_checkout` and gets the lot.
        let cfg = Config::parse(r#"{"main_checkout":"/tmp/x"}"#).expect("parse");
        assert_eq!(cfg.reviews_command, vec!["mise", "run", "reviews", "--json"]);
        assert_eq!(cfg.tracker, Tracker::Shortcut);
        assert_eq!(cfg.upstream_ref, "upstream/develop");
        assert_eq!(cfg.upstream_remote, "upstream");
        assert_eq!(cfg.output_language, "Dutch");
        assert_eq!(cfg.main_processes.len(), 2);
        // `default_for` (the first-run write) goes through the same path.
        assert_eq!(Config::default_for(PathBuf::from("/tmp/x")).main_processes.len(), 2);
    }

    #[test]
    fn a_written_key_overrides_the_default() {
        let cfg = Config::parse(
            r#"{"main_checkout":"/tmp/x","port":9000,"output_language":"English",
                "tracker":"none","reviews_command":["mise","run","reviews:mine"]}"#,
        )
        .expect("parse");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.output_language, "English");
        assert_eq!(cfg.tracker, Tracker::None);
        assert_eq!(cfg.reviews_command, vec!["mise", "run", "reviews:mine"]);
        // ...but an unmentioned key still comes from the defaults.
        assert_eq!(cfg.upstream_ref, "upstream/develop");
    }

    #[test]
    fn an_old_config_with_a_profile_key_still_loads() {
        // `profile` was a config key until the six settings became defaults; a file
        // written back then must still load, its stale key ignored.
        let cfg = Config::parse(
            r#"{"main_checkout":"/tmp/x","profile":"acme"}"#,
        )
        .expect("parse");
        assert_eq!(cfg.main_checkout, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn the_forge_is_spelled_the_way_a_person_would_write_it() {
        // `rename_all = "snake_case"` would make this `git_hub`, so `"github"`
        // was an unknown variant — and a deserialize error here reads as "no
        // config", sending the desktop app back to the folder picker.
        let cfg = Config::parse(
            r#"{"main_checkout":"/tmp/x","forge":"github"}"#,
        )
        .expect("`github` must parse");
        assert_eq!(cfg.forge, ForgeKind::GitHub);
        // What `default_for` writes into a first-run config.json.
        let written = serde_json::to_value(ForgeKind::GitHub).unwrap();
        assert_eq!(written, serde_json::json!("github"));
        // A config written with the old generated spelling still loads.
        assert!(Config::parse(
            r#"{"main_checkout":"/tmp/x","forge":"git_hub"}"#
        )
        .is_ok());
    }

    #[test]
    fn writing_settings_touches_only_its_keys_and_round_trips() {
        // A slim config must stay slim: merging settings sets the editable keys
        // and leaves everything else (here, just main_checkout) alone.
        let s = Settings {
            output_language: "English".into(),
            tracker: Tracker::None,
            upstream_ref: "origin/main".into(),
            upstream_remote: "origin".into(),
            reviews_command: vec!["gh".into(), "pr".into()],
            main_processes: vec![],
            worktree_setup: vec![".claude/hooks/worktree-setup".into()],
        };
        let out = s.merge_into(r#"{"main_checkout":"/tmp/x","port":8080}"#).expect("merge");
        let cfg = Config::parse(&out).expect("re-parse");
        assert_eq!(cfg.main_checkout, PathBuf::from("/tmp/x"), "untouched key kept");
        assert_eq!(cfg.port, 8080, "untouched key kept");
        assert_eq!(cfg.output_language, "English");
        assert_eq!(cfg.tracker, Tracker::None);
        assert_eq!(cfg.upstream_ref, "origin/main");
        assert_eq!(cfg.reviews_command, vec!["gh", "pr"]);
        assert!(cfg.main_processes.is_empty());
        assert_eq!(cfg.worktree_setup, vec![".claude/hooks/worktree-setup"]);
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
    fn a_config_sets_its_reviews_command() {
        let cfg: Config = serde_json::from_str(
            r#"{"main_checkout":"/tmp","reviews_command":["mise","run","reviews","--json"],
                "review_timeout_seconds":120}"#,
        )
        .expect("parse");
        assert_eq!(cfg.reviews_command, vec!["mise", "run", "reviews", "--json"]);
        assert_eq!(cfg.review_timeout_seconds, 120);
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
        let cfg = Config::parse(
            r#"{"main_checkout":"/repo","worktrees_subdir":".worktrees"}"#,
        )
        .unwrap();
        assert_eq!(cfg.worktrees_dir(), PathBuf::from("/repo/.worktrees"));
        assert_eq!(cfg.worktrees_subdir_str(), ".worktrees/");
    }

    #[test]
    fn a_subdir_that_normalises_to_nothing_or_has_a_dot_is_cleaned_at_parse() {
        let dir = |json: &str| Config::parse(json).unwrap().worktrees_dir();
        let prefix = |json: &str| Config::parse(json).unwrap().worktrees_subdir_str();
        // `""`, `"."` and `"./"` all normalise to nothing → the default, so the
        // worktrees dir never collapses onto main and the exclude prefix is never
        // `/` (which matches no porcelain path — the §2 sibling leak).
        for empty in [r#"{"main_checkout":"/repo","worktrees_subdir":""}"#,
                      r#"{"main_checkout":"/repo","worktrees_subdir":"."}"#,
                      r#"{"main_checkout":"/repo","worktrees_subdir":"./"}"#] {
            assert_eq!(dir(empty), PathBuf::from("/repo/.claude/worktrees"), "{empty}");
            assert_eq!(prefix(empty), ".claude/worktrees/", "{empty}");
        }
        // A leading `./` is dropped, so `./wt` and `wt` mean the same thing and
        // the exclude prefix matches the paths git actually reports.
        let c = r#"{"main_checkout":"/repo","worktrees_subdir":"./wt"}"#;
        assert_eq!(dir(c), PathBuf::from("/repo/wt"));
        assert_eq!(prefix(c), "wt/");
    }

    #[test]
    fn only_the_claude_default_subdir_delegates_worktree_creation() {
        // `claude --worktree` always writes to `.claude/worktrees/`, so it can
        // only create a worktree the daemon will find when the two agree.
        let default = |sub: &str| {
            Config::parse(&format!(
                r#"{{"main_checkout":"/repo","worktrees_subdir":"{sub}"}}"#
            ))
            .unwrap()
            .worktrees_subdir_is_claude_default()
        };
        assert!(default(".claude/worktrees"));
        assert!(!default(".worktrees"));
        // A refused subdir falls back to the default, so it delegates again.
        assert!(default("/tmp/elsewhere"));
    }

    #[test]
    fn a_subdir_outside_main_falls_back_to_the_default() {
        // The container mapping, the exclude and path attribution all assume
        // worktrees sit under main, so an absolute or climbing path is refused.
        let dir = |sub: &str| {
            Config::parse(&format!(
                r#"{{"main_checkout":"/repo","worktrees_subdir":"{sub}"}}"#
            ))
            .unwrap()
            .worktrees_dir()
        };
        assert_eq!(dir("/tmp/elsewhere"), PathBuf::from("/repo/.claude/worktrees"));
        assert_eq!(dir("../escape"), PathBuf::from("/repo/.claude/worktrees"));
        assert_eq!(dir("wt/../../escape"), PathBuf::from("/repo/.claude/worktrees"));
    }

    #[test]
    fn an_old_config_without_a_worktrees_subdir_gets_the_default() {
        let cfg: Config = serde_json::from_str(r#"{"main_checkout":"/repo"}"#).expect("parse");
        assert_eq!(cfg.worktrees_subdir, PathBuf::from(".claude/worktrees"));
    }

    #[test]
    fn a_fresh_config_gets_the_default_forge_and_review_command() {
        // Talks to GitHub, and the review queue defaults to `mise run reviews`;
        // clear the command in settings for a repo with no such task.
        let cfg = Config::default_for(PathBuf::from("/tmp/x"));
        assert_eq!(cfg.forge, ForgeKind::GitHub);
        assert_eq!(cfg.reviews_command, vec!["mise", "run", "reviews", "--json"]);
    }

    /// A checkout reached through a symlink has to resolve to the real path, or
    /// hook attribution silently stops working: `PostToolUse` resolves the edited
    /// path, and comparing that against an unresolved workspace root matches
    /// nothing, so the edit never reaches the changed-files pane. Only `--main`
    /// was resolved before; a `config.json` checkout was not.
    ///
    /// macOS makes this ordinary rather than exotic — `/tmp`, `/var` and `$TMPDIR`
    /// are symlinks into `/private`.
    #[test]
    fn a_checkout_reached_through_a_symlink_resolves_to_the_real_path() {
        let base = std::env::temp_dir().join(format!(
            "orchd-symlink-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real-checkout");
        std::fs::create_dir_all(&real).expect("mkdir");
        let link = base.join("via-link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let cfg = Config::parse(&format!(
            r#"{{"main_checkout":"{}"}}"#,
            link.to_string_lossy()
        ))
        .expect("parse");

        let expected = std::fs::canonicalize(&real).expect("canonicalize");
        assert_eq!(
            cfg.main_checkout, expected,
            "the symlink must be resolved, or hook paths match no workspace"
        );
        // And the derived paths inherit it, which is the point — those are what
        // `workspace_for_path` compares against.
        assert!(cfg.worktrees_dir().starts_with(&expected));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_checkout_that_does_not_exist_is_left_as_written() {
        // `validate` is what complains about a missing checkout, and it should
        // name the path the user wrote rather than something rewritten here.
        let cfg = Config::parse(r#"{"main_checkout":"/nope/not/here"}"#).expect("parse");
        assert_eq!(cfg.main_checkout, Path::new("/nope/not/here"));
    }

    #[test]
    fn state_lands_where_the_platform_keeps_it() {
        let dir = default_config_dir(Path::new("/home/someone"));
        if cfg!(target_os = "macos") {
            assert_eq!(
                dir,
                Path::new("/home/someone/Library/Application Support/orchd"),
                "a Mac keeps application state in Library, not ~/.config"
            );
            // The space is one path component, not two. Anything that hands this
            // to a shell must quote it (`hooks::sh_quote`).
            assert_eq!(dir.file_name().unwrap(), "orchd");
            assert!(dir.to_string_lossy().contains("Application Support"));
        } else {
            assert_eq!(dir, Path::new("/home/someone/.config/orchd"));
        }
    }
}
