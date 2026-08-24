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
    /// and where a repo's own `worktree-create` hook is most likely to put them,
    /// so a generic checkout needs no setting. A repo that relocates them (a `WorktreeCreate`
    /// hook) points this at the same place, so the daemon still recognises its
    /// own worktrees. Kept relative and in-main on purpose: the container path
    /// mapping, the changed-files exclude, and path attribution all assume
    /// worktrees sit under main.
    #[serde(default = "default_worktrees_subdir")]
    pub worktrees_subdir: PathBuf,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Directories inside a worktree that are allowed to be symlinks *out* of it.
    ///
    /// The editable diff pane is the one endpoint that writes arbitrary bytes to
    /// disk, so it refuses any path that resolves outside the workspace — a
    /// symlink pointing out must not become a write primitive. A repo that
    /// deliberately shares a directory across worktrees (a plan or notes dir
    /// symlinked back to main, say) names it here and writes through it stay
    /// allowed.
    ///
    /// Empty by default, which is the tight answer: a repo that shares nothing
    /// gets no exception at all. Each entry is a path relative to the worktree
    /// root, matched after canonicalisation, so `..` in the *value* buys nothing.
    #[serde(default)]
    pub shared_worktree_paths: Vec<String>,
    /// Managed processes declared for the main workspace. Worktrees declare none
    /// by default; a shell is opened on demand instead. Empty by default — see the
    /// README for the shape and a worked example — and edited in the settings panel.
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
    /// from "the daemon hid it". `none` by default; the implementations live in
    /// `src/tracker/`.
    ///
    /// Its credential is **not** a config key: `ORCHD_TRACKER_TOKEN` in the
    /// daemon's environment, and nowhere else. See `story::resolve_token`.
    #[serde(default = "default_tracker")]
    pub tracker: TrackerKind,
    /// How long the borrowed story-filing agent gets before it is killed.
    ///
    /// The one timeout in this daemon, because it is the one agent whose caller is
    /// a blocking HTTP request rather than a rail entry somebody is watching. Sized
    /// from the skill's real workload — read an epic, search, create, follow up —
    /// so minutes, not seconds.
    #[serde(default = "default_story_timeout")]
    pub story_timeout_seconds: u64,
    /// The language the agent *writes* in — reviewer replies and story text.
    ///
    /// A **fallback**, which is what the name says: the agent matches a thread's
    /// own language first and only reaches for this when that is unclear. Prompts
    /// and code stay English regardless; this is the outward prose only.
    ///
    /// Named `default_language` rather than `output_language` because it is not
    /// only the tracker's, and not a mandate — replies to a PR thread are the
    /// other, larger half of what it governs.
    #[serde(default = "default_language_value")]
    pub default_language: String,
    /// The PR poll is one query per period, negligible against 5000 points/hour
    /// (§6). The review poll shells out to [`Config::reviews_command`], whose cost
    /// is that command's business, bounded by [`Config::review_timeout_seconds`].
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    /// Which forge the repo lives on. Only GitHub is implemented; the field
    /// exists so a second platform is a config choice rather than a rebuild.
    #[serde(default)]
    pub forge: ForgeKind,
    /// How long the review-queue command may run before the poller gives up on it.
    /// Such a command typically walks every open PR, so it needs a generous ceiling
    /// but must not hang the poller forever.
    #[serde(default = "default_review_timeout")]
    pub review_timeout_seconds: u64,
    /// The command whose JSON output feeds the review-queue pane (the shape in
    /// `docs/reviews-json.md`). An argv, run in the main checkout under
    /// `proc::run_bounded` — not coreutils `timeout`, which is GNU and absent on a
    /// Mac, where it failed at the spawn and blamed the review command.
    ///
    /// Defaults to the queue the daemon ships, ejected to the config dir as
    /// `reviews.js` and never overwritten after that — so it works on a fresh
    /// install and the ranking is still yours to edit. Point it at your own task
    /// instead, or clear it: empty means "no review queue here", and the pane says
    /// so rather than reading as a broken command.
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
    /// Shadows the repo's `worktree-create` hook: **make the tree usable.**
    ///
    /// First of the two, run with cwd set to the new worktree, before
    /// [`Self::worktree_setup`]. This is the half about the tree *as a checkout* —
    /// basing it on a freshly fetched upstream, configuring triangular push,
    /// whatever the repo does to a branch before anyone works on it.
    ///
    /// See [`Self::worktree_setup`] for why there are two of these and when they
    /// run at all.
    #[serde(default)]
    pub worktree_init: Vec<String>,
    /// Shadows the repo's `worktree-link` hook: **put the shared files in place.**
    ///
    /// Second of the two, same cwd. This is the half about what the tree *needs
    /// beside the code* — symlinks back to main, a rules-dedup file, generated
    /// config. It runs even if `worktree_init` failed, because the two answer
    /// different questions and a tree that is merely un-based is still worth
    /// linking.
    ///
    /// # Why two
    ///
    /// Claude Code's `WorktreeCreate` hook fires only for `claude --worktree`, so a
    /// PR worktree or a resumed one — both cut by the daemon with plain
    /// `git worktree add` — silently skips whatever the repo does at creation. The
    /// case this was written for wrote a file that stops the repo's rules
    /// double-loading, and the PR worktrees were missing it.
    ///
    /// One command had to cover both concerns, which meant a repo with two hooks
    /// needed a wrapper script to fan back out. Two settings mirror the two hooks,
    /// so each points straight at the script that already exists.
    ///
    /// Empty by default: a plain checkout needs neither. Neither runs on the
    /// `claude --worktree` path, where the repo's own hooks already did the work —
    /// running both would double it.
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
    pub default_language: String,
    pub tracker: TrackerKind,
    pub upstream_ref: String,
    pub upstream_remote: String,
    pub reviews_command: Vec<String>,
    pub main_processes: Vec<ManagedSpec>,
    pub worktree_setup: Vec<String>,
}

impl Settings {
    pub fn of(cfg: &Config) -> Self {
        Settings {
            default_language: cfg.default_language.clone(),
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
        obj.insert("default_language".into(), serde_json::to_value(&self.default_language)?);
        obj.insert("tracker".into(), serde_json::to_value(self.tracker)?);
        obj.insert("upstream_ref".into(), serde_json::to_value(&self.upstream_ref)?);
        obj.insert("upstream_remote".into(), serde_json::to_value(&self.upstream_remote)?);
        obj.insert("reviews_command".into(), serde_json::to_value(&self.reviews_command)?);
        obj.insert("main_processes".into(), serde_json::to_value(&self.main_processes)?);
        obj.insert("worktree_setup".into(), serde_json::to_value(&self.worktree_setup)?);
        Ok(serde_json::to_string_pretty(&v)? + "\n")
    }
}

/// The remote's own default branch, no fork assumed.
///
/// `origin/HEAD` rather than a branch name because nothing universal is called
/// `develop` or `main`, and the symref answers for the repo. A fork layout is
/// *detected* on first run rather than assumed — see `git::detect_base` — so a
/// fork user never has to learn there are two keys to set.
fn default_upstream() -> String {
    "origin/HEAD".to_string()
}

fn default_auto_resume() -> bool {
    true
}

fn default_upstream_remote() -> String {
    "origin".to_string()
}

fn default_tracker() -> TrackerKind {
    TrackerKind::None
}

/// The ejected default queue, so the pane works on a fresh install.
///
/// There is no *repo task* every repo has, which is why this used to be empty and
/// a new checkout got no queue at all. The answer is not daemon code — a built-in
/// GraphQL queue with a ranking engine was built and deliberately reverted for
/// being more machinery than anyone wanted to own — but a script the daemon ships
/// and then stops owning: `reviews::eject_default_script` writes it once and never
/// again, so the ranking is yours to edit.
///
/// Empty if the config dir cannot be resolved. That is the honest fallback: the
/// pane reads "not configured" rather than pointing at a path that is not there.
/// `docs/reviews-json.md` has the contract, for replacing it outright.
fn default_reviews_command() -> Vec<String> {
    match crate::reviews::default_script_path() {
        Ok(p) => vec![p.to_string_lossy().into_owned()],
        Err(_) => Vec::new(),
    }
}

/// Empty: a managed process is whatever *this* repo runs long-term, and no two
/// repos agree.
///
/// The drawer is the place they show up, and it stays empty until you declare
/// one. `ManagedSpec` is the shape — a name, an argv, and the output patterns
/// that decide whether it reads as healthy or failing.
fn default_main_processes() -> Vec<ManagedSpec> {
    Vec::new()
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
pub enum TrackerKind {
    /// No tracker. `story+reply` is never offered and would be refused.
    #[default]
    None,
    /// The Shortcut MCP from the repo's own `.mcp.json`.
    Shortcut,
    /// A stub MCP server that speaks the same tool names and records what it was
    /// asked to do. For proving the plumbing without filing a real story.
    Stub,
}

impl TrackerKind {
    pub fn is_configured(self) -> bool {
        !matches!(self, TrackerKind::None)
    }
}

fn default_story_timeout() -> u64 {
    300
}

fn default_language_value() -> String {
    "English".to_string()
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
    /// which are deliberately generic. An old file with a stale `"profile"` key
    /// still loads — serde ignores the unknown field.
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
        // `upstream_ref`, while repo detection reads `upstream_remote`. If they
        // name different remotes the merge-base and the resolved repo drift apart,
        // and the symptom is PRs polled from the wrong repository — not an error,
        // just the wrong answer.
        //
        // The ref wins, rather than warning and carrying on. It names its remote
        // explicitly, whereas `upstream_remote` defaults to `origin` — so a config
        // that pins only `upstream_ref: upstream/develop` is stating a fork layout
        // and merely omitting the half it should not have to repeat. Warning there
        // would nag about a config that is not wrong, and honouring `origin` would
        // silently poll the fork.
        if let Some((ref_remote, _)) = cfg.upstream_ref.split_once('/') {
            if ref_remote != cfg.upstream_remote {
                tracing::info!(
                    "upstream_ref {:?} names remote {:?}; using that rather than \
                     upstream_remote {:?}, so the base fetch and repo detection agree",
                    cfg.upstream_ref,
                    ref_remote,
                    cfg.upstream_remote
                );
                cfg.upstream_remote = ref_remote.to_string();
            }
        }
        /* Resolved once, here, so every path derived from it is resolved too —
           `worktrees_dir`, `worktree_path`, and so the workspace paths that
           `workspace_for_path` matches hook paths against.

           That match is the reason. A `PostToolUse` path goes through
           `canonicalize` before it is attributed (`hooks.rs`, so a shared
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
    /// Those defaults ask nothing of the repo being pointed at: no review-queue
    /// command, no managed processes, no tracker. A checkout that has those turns
    /// them on in the settings panel.
    ///
    /// The base ref is the one exception, because it is the one setting a checkout
    /// can answer for itself: an `upstream` remote beside `origin` is a fork
    /// layout, unmistakably, and guessing wrong there means every diff is measured
    /// against nothing.
    fn default_for(main_checkout: PathBuf) -> Self {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "main_checkout".into(),
            serde_json::to_value(&main_checkout).expect("a path is JSON"),
        );
        // Written into the file rather than left to the default, so what the
        // daemon measures against is visible in `config.json` and editable there.
        // A detected value living only in code would be a base ref nobody could
        // see, which is worse than one they had to type.
        if let Some((base, remote)) = crate::git::detect_base(&main_checkout) {
            tracing::info!(%base, %remote, "first run: detected a fork layout");
            obj.insert("upstream_ref".into(), base.into());
            obj.insert("upstream_remote".into(), remote.into());
        }
        let raw = serde_json::Value::Object(obj).to_string();
        // Cannot fail: every key written here is known-valid.
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
/// `…-repo-.claude-worktrees-x` against a real `…-repo--claude-worktrees-x`
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
            transcript_slug(Path::new("/home/x/dev/monorepo/.claude/worktrees/dfafdf")),
            "-home-x-dev-monorepo--claude-worktrees-dfafdf"
        );
    }

    #[test]
    fn a_bare_config_asks_nothing_of_the_repo_it_points_at() {
        // The defaults used to be one monorepo's: a review-queue task only it had,
        // its build watcher and docker stack, its tracker, its language. A repo
        // without those read as broken rather than as not having them.
        let cfg = Config::parse(r#"{"main_checkout":"/tmp/x"}"#).expect("parse");
        assert!(cfg.main_processes.is_empty(), "no process every repo runs");
        // The review queue is the exception, and it asks nothing of the repo
        // either: the ejected script talks to the forge, not to a repo task.
        assert_eq!(cfg.reviews_command.len(), 1);
        assert!(
            cfg.reviews_command[0].ends_with("reviews.js"),
            "the ejected default, not a repo task: {:?}",
            cfg.reviews_command
        );
        assert_eq!(cfg.tracker, TrackerKind::None);
        assert_eq!(cfg.default_language, "English");
        // `default_for` (the first-run write) goes through the same path, so a
        // fresh install writes the same nothing.
        assert!(Config::default_for(PathBuf::from("/tmp/x")).main_processes.is_empty());
    }

    /// The example in the README has to keep working, because it is what anyone
    /// declaring a build watcher will copy. The recovery line is the part that
    /// cost a debugging session: esbuild's success marker matches none of the
    /// older ones, so without it the rail's `build failing` never cleared after a
    /// fixed compile.
    #[test]
    fn a_declared_watcher_clears_on_the_esbuild_recovery_line() {
        let cfg = Config::parse(
            r#"{"main_checkout":"/tmp/x","main_processes":[{
                 "name":"ng-watch","command":["npx","ng","build","--watch"],
                 "failure_patterns":["Error:","ERROR in"],
                 "ok_patterns":["bundle generation complete"],
                 "autostart":false}]}"#,
        )
        .expect("parse");
        let ng = &cfg.main_processes[0];
        assert!(ng.ok_patterns.iter().any(|p| p == "bundle generation complete"));
        assert!(!ng.autostart);
    }

    #[test]
    fn a_written_key_overrides_the_default() {
        let cfg = Config::parse(
            r#"{"main_checkout":"/tmp/x","port":9000,"default_language":"English",
                "tracker":"none","reviews_command":["mise","run","reviews:mine"]}"#,
        )
        .expect("parse");
        assert_eq!(cfg.port, 9000);
        assert_eq!(cfg.default_language, "English");
        assert_eq!(cfg.tracker, TrackerKind::None);
        assert_eq!(cfg.reviews_command, vec!["mise", "run", "reviews:mine"]);
        // ...but an unmentioned key still comes from the defaults.
        assert_eq!(cfg.upstream_ref, "origin/HEAD");
    }

    /// The hazard the generic default introduced: `upstream_remote` now defaults
    /// to `origin`, so a config pinning only `upstream_ref: upstream/develop` — a
    /// fork layout stated once — would have polled PRs from the fork.
    #[test]
    fn a_ref_naming_its_own_remote_wins_over_the_defaulted_one() {
        let cfg = Config::parse(r#"{"main_checkout":"/tmp/x","upstream_ref":"upstream/develop"}"#)
            .expect("parse");
        assert_eq!(cfg.upstream_remote, "upstream", "taken from the ref, not the default");

        // A bare branch name says nothing about a remote, so the default stands.
        let bare = Config::parse(r#"{"main_checkout":"/tmp/x","upstream_ref":"main"}"#)
            .expect("parse");
        assert_eq!(bare.upstream_remote, "origin");

        // And an explicit pair that agrees is left exactly alone.
        let both = Config::parse(
            r#"{"main_checkout":"/tmp/x","upstream_ref":"fork/trunk","upstream_remote":"fork"}"#,
        )
        .expect("parse");
        assert_eq!(both.upstream_remote, "fork");
    }

    /// The detection has to reach the file a first run writes, not merely exist.
    /// `git::detect_base` is unit-tested; this is the wiring.
    #[test]
    fn a_first_run_adopts_a_fork_layout_and_leaves_a_plain_one_generic() {
        let dir = std::env::temp_dir().join(format!(
            "orchd-firstrun-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git runs")
        };
        run(&["init", "-q", "."]);
        run(&["remote", "add", "origin", "git@github.com:you/monorepo.git"]);

        // `origin` alone is not a fork, so the generic pair stands.
        let plain = Config::default_for(dir.clone());
        assert_eq!(plain.upstream_ref, "origin/HEAD");
        assert_eq!(plain.upstream_remote, "origin");

        run(&["remote", "add", "upstream", "git@github.com:acme/monorepo.git"]);
        let fork = Config::default_for(dir.clone());
        assert_eq!(fork.upstream_ref, "upstream/HEAD");
        assert_eq!(fork.upstream_remote, "upstream");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_config_with_a_profile_key_still_loads() {
        // `profile` was a config key until the six settings became defaults; a file
        // written back then must still load, its stale key ignored.
        let cfg = Config::parse(
            r#"{"main_checkout":"/tmp/x","profile":"monorepo"}"#,
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
            default_language: "English".into(),
            tracker: TrackerKind::None,
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
        assert_eq!(cfg.default_language, "English");
        assert_eq!(cfg.tracker, TrackerKind::None);
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
    fn a_fresh_config_talks_to_github_and_gets_the_ejected_queue() {
        // Both defaults are defensible: GitHub is where the PRs are for most repos
        // and the only impl, and the queue is a script the daemon ships rather than
        // a repo task it hopes exists. It lands *in the file*, so it is visible and
        // replaceable rather than hidden in code.
        let cfg = Config::default_for(PathBuf::from("/tmp/x"));
        assert_eq!(cfg.forge, ForgeKind::GitHub);
        assert!(cfg.reviews_command[0].ends_with("reviews.js"));
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
