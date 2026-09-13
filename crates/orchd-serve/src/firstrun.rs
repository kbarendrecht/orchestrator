//! What opening a project needs to know: the recents, whether a folder is a
//! checkout, and what its settings should be.
//!
//! **No server and no page any more.** This was a second application — its own
//! axum server on its own port, its own router and guard, a `BootstrapHost` trait
//! for the two things needing a window, and an HTML page with a copy of the SPA's
//! palette and a titlebar that had to learn the macOS window-drag rule a second
//! time. The host serves the board with no checkouts open instead, and
//! `web/js/open.js` is the screen; `host::validate` and `host::detect` are the
//! routes. What is left here is the part that was always pure: read a list, judge
//! a folder, look at a repo, write a config.
//!
//! Recents live in the config dir, so `ORCHD_CONFIG_DIR` relocates them with
//! everything else — which is what lets a test point the whole list at a temp dir.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use orchd::config::Config;

/// A project opened before, newest first. The path is absolute; the name is its
/// last component, which is what a person recognises the checkout by.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "serve.d.ts")
)]
pub struct RecentProject {
    pub path: String,
    pub name: String,
    /// Milliseconds since the epoch of the last open. The page renders "2 hours
    /// ago" from it; stored as a number so it needs no locale.
    pub last_opened_ms: u64,
}

/// What a valid checkout looks like to the open screen: enough to confirm the
/// choice before the daemon is asked to start on it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProjectInfo {
    pub path: String,
    pub name: String,
}

fn recent_file_in(dir: &Path) -> PathBuf {
    dir.join("recent.json")
}

/// The last component of a path, as a display name. `orchestrator` for
/// `~/development/orchestrator`. Falls back to the whole path if there is no
/// component (the filesystem root), which no real checkout is.
fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The recent-projects list, newest first. Missing or corrupt file reads as empty
/// rather than failing — a first run has no list, and a garbled one should not keep
/// the window shut.
pub fn recent_projects() -> Vec<RecentProject> {
    match Config::config_dir() {
        Ok(dir) => recent_projects_in(&dir),
        Err(_) => Vec::new(),
    }
}

fn recent_projects_in(dir: &Path) -> Vec<RecentProject> {
    let Ok(raw) = std::fs::read_to_string(recent_file_in(dir)) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// How many to keep. Long enough to cover the repos anyone juggles, short enough
/// that the list stays a glance rather than a history.
const MAX_RECENT: usize = 12;

/// Record that `path` was just opened: move it to the front with a fresh timestamp,
/// drop any older entry for the same path, and cap the list. Best effort — a failure
/// to write the list must never fail an open, so the caller logs and carries on.
pub fn record_recent(path: &Path) -> Result<()> {
    let dir = Config::config_dir()?;
    record_recent_in(&dir, path)
}

fn record_recent_in(dir: &Path, path: &Path) -> Result<()> {
    let path_str = path.to_string_lossy().into_owned();
    let mut list = recent_projects_in(dir);
    list.retain(|r| r.path != path_str);
    list.insert(
        0,
        RecentProject {
            name: name_of(path),
            path: path_str,
            last_opened_ms: now_ms(),
        },
    );
    list.truncate(MAX_RECENT);

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = recent_file_in(dir);
    std::fs::write(&file, serde_json::to_string_pretty(&list)? + "\n")
        .with_context(|| format!("writing {}", file.display()))?;
    Ok(())
}

/// Whether a chosen folder can host a session board, and its display name.
///
/// The two things that make an open fail later if they are wrong now: the folder
/// has to exist, and it has to be a git repository — the whole model is worktrees
/// cut from one checkout. `.git` as a file counts (a linked worktree), though
/// pointing the daemon at a worktree rather than its main checkout is a separate
/// mistake this does not police. Returns a human message, not an error type,
/// because it goes straight to the page.
pub fn validate(path: &Path) -> std::result::Result<ProjectInfo, String> {
    let path = expand_home(path, std::env::var_os("HOME").map(PathBuf::from).as_deref());
    if !path.exists() {
        return Err("No such folder.".into());
    }
    if !path.is_dir() {
        return Err("That is a file, not a folder.".into());
    }
    // Canonical, because this string is what `config.json` gets: a relative path
    // typed into the box would be resolved against whatever the daemon's cwd
    // happened to be on the next launch, and a symlinked one would fail the
    // path comparisons `Config::parse` canonicalises everything else for.
    let path =
        std::fs::canonicalize(&path).map_err(|e| format!("Cannot resolve that folder: {e}"))?;
    if !path.join(".git").exists() {
        return Err("Not a git repository — orchd works on a git checkout.".into());
    }
    Ok(ProjectInfo {
        name: name_of(&path),
        path: path.to_string_lossy().into_owned(),
    })
}

/// `~` and `~/x` mean the home directory, the way the box's own placeholder
/// writes it. A shell expands this; a text field does not, so the page's own
/// example was refused as "No such folder".
fn expand_home(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path.to_path_buf();
    };
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// What orchd worked out about a chosen checkout, for the review step to confirm.
/// Every field is a guess with a default, and the page says where each came from —
/// a wrong one is caught here rather than discovered on the first sweep.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "serve.d.ts")
)]
pub struct Detected {
    pub path: String,
    pub name: String,
    /// The base ref worktrees branch from, `<remote>/<branch>`. The resolved
    /// default (`origin/main`) when the symref is known, else the first remote
    /// branch, else `origin/HEAD` — the daemon's own default.
    pub base_branch: String,
    /// The remote-tracking branches to choose among.
    pub base_branches: Vec<String>,
    /// `owner/name` for PR watching, from the origin remote. `None` off GitHub.
    pub repo: Option<String>,
    /// Where a session's environment comes from: `mise`, `direnv` or `none`,
    /// detected from the files in the checkout.
    pub env_source: String,
    /// Where worktrees are cut. Always the default today; shown so it is not a
    /// surprise later.
    pub worktrees: String,
    /// Long-running processes the repo appears to define — a compose stack, a dev
    /// watch. Offered unchecked: orchd never starts someone's stack behind their
    /// back on first open.
    pub processes: Vec<DetectedProcess>,
}

/// A process orchd guessed the repo runs, and how to run it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "serve.d.ts")
)]
pub struct DetectedProcess {
    /// Short name for the drawer tab.
    pub name: String,
    /// The argv to run.
    pub command: Vec<String>,
    /// How it reads to a person (`pnpm run dev`).
    pub label: String,
    /// The file it was inferred from, shown so a wrong guess is obvious.
    pub source: String,
}

/// Long-running processes a repo appears to define. Best effort and conservative:
/// a compose file means a stack, and a small set of conventional dev scripts in a
/// `package.json` mean a watcher — anything cleverer would guess wrong more than it
/// helped, and these are offered unchecked anyway.
fn detect_processes(path: &Path) -> Vec<DetectedProcess> {
    let mut out = Vec::new();

    for f in [
        "compose.yaml",
        "compose.yml",
        "docker-compose.yml",
        "docker-compose.yaml",
    ] {
        if path.join(f).exists() {
            out.push(DetectedProcess {
                name: "docker".into(),
                command: vec!["docker".into(), "compose".into(), "up".into()],
                label: "docker compose up".into(),
                source: f.into(),
            });
            break; // one compose stack, not one per spelling
        }
    }

    if let Ok(raw) = std::fs::read_to_string(path.join("package.json")) {
        if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&raw) {
            // The package manager the repo uses, from its lockfile.
            let pm = if path.join("pnpm-lock.yaml").exists() {
                "pnpm"
            } else if path.join("yarn.lock").exists() {
                "yarn"
            } else if path.join("bun.lockb").exists() {
                "bun"
            } else {
                "npm"
            };
            // Only the conventional long-running ones — not every script, which is
            // mostly one-shot build and lint tasks nobody wants as a managed pty.
            const WANTED: &[&str] = &[
                "dev",
                "start",
                "watch",
                "serve",
                "build-watch",
                "build:watch",
            ];
            if let Some(scripts) = pkg.get("scripts").and_then(|s| s.as_object()) {
                for name in WANTED {
                    if scripts.contains_key(*name) {
                        out.push(DetectedProcess {
                            name: (*name).into(),
                            command: vec![pm.into(), "run".into(), (*name).into()],
                            label: format!("{pm} run {name}"),
                            source: "package.json".into(),
                        });
                    }
                }
            }
        }
    }

    out
}

/// Inspect a checkout and propose its settings. Best effort throughout — anything
/// it cannot read falls back to the daemon's own default rather than failing, so
/// the review always has something to show.
pub fn detect(path: &Path) -> Detected {
    let base_branches = orchd::git::remote_branches(path);
    // A fork layout first — an `upstream` remote beside `origin` — because on one
    // every other guess below is the fork, and the review then pre-filled the
    // fork's default branch and the fork's repo. The daemon's own first write asks
    // the same question (`git::detect_base`), so the review shows what it would do.
    let fork = orchd::git::detect_base(path);
    let remote = fork.as_ref().map(|(_, r)| r.as_str()).unwrap_or("origin");
    let prefix = format!("{remote}/");
    // Prefer the remote's recorded default; then a conventional main/master; then
    // any branch of that remote; then whatever branch there is.
    //
    // Every arm but the last is filtered on `base_branches`, because this fills a
    // `<select>` and pre-selecting a branch the list does not offer shows the
    // picker as blank. The last arm is deliberately *not* filtered: it is only
    // reached when there are no remote branches at all, so there is no list to be
    // absent from, and naming the daemon's own default ref is more use than "".
    let base_branch = fork
        .as_ref()
        .map(|(b, _)| b.clone())
        .filter(|b| base_branches.contains(b))
        .or_else(|| {
            orchd::git::base_checkout_branch(path, &format!("{prefix}HEAD"))
                .map(|b| format!("{prefix}{b}"))
                .filter(|b| base_branches.contains(b))
        })
        .or_else(|| {
            base_branches
                .iter()
                .find(|b| *b == &format!("{prefix}main") || *b == &format!("{prefix}master"))
                .or_else(|| base_branches.iter().find(|b| b.starts_with(&prefix)))
                .cloned()
        })
        .or_else(|| base_branches.first().cloned())
        .unwrap_or_else(|| match &fork {
            Some((b, _)) => b.clone(),
            None => "origin/HEAD".to_string(),
        });

    let repo = orchd::forge::GitHubForge::detect(path, remote)
        .map(|(owner, name)| format!("{owner}/{name}"));

    let env_source = if path.join("mise.toml").exists() || path.join(".mise.toml").exists() {
        "mise"
    } else if path.join(".envrc").exists() {
        "direnv"
    } else {
        "none"
    }
    .to_string();

    Detected {
        path: path.to_string_lossy().into_owned(),
        name: name_of(path),
        base_branch,
        base_branches,
        repo,
        env_source,
        worktrees: ".claude/worktrees".to_string(),
        processes: detect_processes(path),
    }
}

/// The review's answers, sent with the add. Every override is optional — an unset
/// one leaves the daemon's default, and a `None` `repo` means "watch what origin
/// resolves to" rather than "watch nothing".
#[derive(Deserialize, Default)]
pub struct Overrides {
    pub path: String,
    #[serde(default)]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    /// Typed rather than a string, so serde refuses a value the daemon could not
    /// have loaded — the page sends the enum's own spelling, and a mismatch is a
    /// 422 naming the field instead of a config written and then rejected.
    #[serde(default)]
    pub env_source: Option<orchd::config::EnvSourceKind>,
    /// The processes the user ticked in the review, to manage from the start.
    #[serde(default)]
    pub processes: Vec<SelectedProcess>,
}

/// A process the review ticked, to write into `main_processes`.
#[derive(Deserialize, Default)]
pub struct SelectedProcess {
    pub name: String,
    #[serde(default)]
    pub command: Vec<String>,
}

/// Write the first-run `config.json` from the review's answers, before the daemon
/// starts and reads it.
///
/// **Merged into the file that is there, never written over it.** This runs on
/// every open — a fresh checkout, a recent, a switch — and it used to build the
/// object from scratch, so a checkout that had moved was re-picked and every
/// hand-tuned key (`reviews_command`, `worktree_setup`, `main_processes`, the notes)
/// was gone with no copy kept. Only the keys the review answered change; the
/// previous file is kept beside it as `config.json.bak`.
///
/// **Into the checkout's own state directory**, which is where a reviewed setting
/// has to land. The root `config.json` this used to write is one checkout's:
/// `host::checkout_dir` gives every checkout its own `ORCHD_CONFIG_DIR`, and the
/// root file is only ever *copied* into one of them, once, when it happens to name
/// that checkout. So a second checkout's review had nowhere to go, which is half of
/// why the review step only ever ran on a fresh install.
///
/// Slim on purpose — only `main_checkout` and the fields that differ from a plain
/// default are written, the same shape a hand-written config takes, so the file
/// stays readable. Each value is validated by parsing the whole config before it is
/// written, so a bad override is a caught error rather than a daemon that refuses to
/// start. Never fatal to the caller: if this fails, the daemon's own `load_or_init`
/// still writes a sensible default for the checkout — the review's edits are just
/// lost, which is better than no window.
pub fn write_config_in(dir: &Path, path: &Path, ov: &Overrides) -> Result<()> {
    write_config_to(&dir.join("config.json"), path, ov)
}

fn write_config_to(file: &Path, path: &Path, ov: &Overrides) -> Result<()> {
    use serde_json::json;
    let previous = std::fs::read_to_string(file).ok();
    let mut obj = match previous
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
    {
        Some(Ok(serde_json::Value::Object(m))) => m,
        None => serde_json::Map::new(),
        // Not JSON, or not an object: nothing in it can be kept by key, so start
        // over. The backup below is what keeps whatever it was.
        Some(_) => {
            tracing::warn!("{} is not a JSON object; replacing it", file.display());
            serde_json::Map::new()
        }
    };
    obj.insert("main_checkout".into(), json!(path.to_string_lossy()));

    if let Some(base) = ov.base_branch.as_deref().filter(|s| !s.is_empty()) {
        obj.insert("upstream_ref".into(), json!(base));
        // `<remote>/<branch>` — the remote is what the push guard and the fetch use.
        if let Some((remote, _)) = base.split_once('/') {
            obj.insert("upstream_remote".into(), json!(remote));
        }
    } else if !obj.contains_key("upstream_ref") {
        // A recent opens with no review, so nothing answered the one question a
        // checkout can answer for itself. `Config::default_for` asked it on a fresh
        // file, but this write comes first and so that path is never reached from
        // the app — without this a fork layout was measured against `origin/HEAD`.
        if let Some((base, remote)) = orchd::git::detect_base(path) {
            tracing::info!(%base, %remote, "detected a fork layout");
            obj.insert("upstream_ref".into(), json!(base));
            obj.insert("upstream_remote".into(), json!(remote));
        }
    }
    if let Some(repo) = ov.repo.as_deref().filter(|s| !s.is_empty()) {
        // Only a repo the remote does *not* already derive is worth pinning. The
        // review pre-fills this field from a remote, and writing that back made an
        // explicit override out of a default — one that kept PR polling on the fork
        // after the base had been pointed at `upstream`.
        let remote = obj
            .get("upstream_remote")
            .and_then(|v| v.as_str())
            .unwrap_or("origin");
        // Through `detect`, which is that pair of calls, and is what filled the
        // field the review is handing back — so the two answers cannot disagree.
        let derived =
            orchd::forge::GitHubForge::detect(path, remote).map(|(o, n)| format!("{o}/{n}"));
        if derived.as_deref() == Some(repo) {
            obj.remove("repo");
        } else {
            obj.insert("repo".into(), json!(repo));
        }
    }
    // Typed on the way in, so an unknown value is refused by serde while the
    // request is being read rather than round-tripped through a string here.
    if let Some(env) = ov.env_source {
        obj.insert("env_source".into(), json!(env));
    }
    // Ticked processes become managed `main_processes`. `autostart` is true because
    // ticking one in the review is the consent the setting's default withholds — the
    // rest of a `ManagedSpec` is left to serde defaults.
    let procs: Vec<_> = ov
        .processes
        .iter()
        .filter(|p| !p.command.is_empty())
        .map(|p| json!({ "name": p.name, "command": p.command, "autostart": true }))
        .collect();
    if !procs.is_empty() {
        obj.insert("main_processes".into(), json!(procs));
    }

    let raw = serde_json::to_string_pretty(&serde_json::Value::Object(obj))? + "\n";
    // The whole thing has to parse, or the daemon would fail to start on it later.
    Config::parse(&raw).context("the first-run config did not validate")?;

    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // One generation back on disk, for the edit above that turns out to be wrong
    // and is only noticed later, by a person. Best effort: a failed backup is not
    // a reason to refuse the open.
    if previous.is_some() {
        let bak = file.with_extension("json.bak");
        if let Err(e) = std::fs::copy(file, &bak) {
            tracing::warn!("could not keep {}: {e}", bak.display());
        }
    }
    std::fs::write(file, raw).with_context(|| format!("writing {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique dir per test, so the recents functions can be exercised through
    /// their dir-taking half with no global `ORCHD_CONFIG_DIR` — the tests run in
    /// parallel, and one process-wide env var would race between them.
    fn tmp(tag: &str) -> PathBuf {
        orchd::testutil::scratch(&format!("firstrun-{tag}"))
    }

    fn git_repo(at: &Path) {
        std::fs::create_dir_all(at.join(".git")).unwrap();
    }

    use orchd::testutil::git as run_git;

    #[test]
    fn validate_wants_a_folder_that_is_a_git_repo() {
        let base = tmp("val");

        let missing = base.join("nope");
        assert!(validate(&missing).is_err(), "a path that does not exist");

        let plain = base.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(validate(&plain).is_err(), "a folder with no .git");

        let repo = base.join("myrepo");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);
        let info = validate(&repo).expect("a git repo validates");
        assert_eq!(info.name, "myrepo", "the name is the last path component");

        // What is handed on is the canonical path: `..` segments and links are
        // resolved, since the string ends up in `config.json`.
        let roundabout = base.join("plain").join("..").join("myrepo");
        let info = validate(&roundabout).expect("a roundabout spelling still validates");
        assert_eq!(
            info.path,
            std::fs::canonicalize(&repo).unwrap().to_string_lossy()
        );
        assert_eq!(info.name, "myrepo");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The placeholder in the box says `~/code/project`, and a text field does not
    /// expand `~` the way a shell does.
    #[test]
    fn a_tilde_means_the_home_directory() {
        let home = Path::new("/home/someone");
        assert_eq!(
            expand_home(Path::new("~/code/x"), Some(home)),
            PathBuf::from("/home/someone/code/x")
        );
        assert_eq!(
            expand_home(Path::new("~"), Some(home)),
            PathBuf::from("/home/someone")
        );
        assert_eq!(
            expand_home(Path::new("/abs/x"), Some(home)),
            PathBuf::from("/abs/x")
        );
        assert_eq!(
            expand_home(Path::new("~user/x"), Some(home)),
            PathBuf::from("~user/x"),
            "not a shell"
        );
        assert_eq!(
            expand_home(Path::new("~/x"), None),
            PathBuf::from("~/x"),
            "no home, no expansion"
        );
    }

    #[test]
    fn recents_round_trip_newest_first_without_duplicates() {
        let dir = tmp("roundtrip");
        assert!(
            recent_projects_in(&dir).is_empty(),
            "a fresh dir has no recents"
        );

        record_recent_in(&dir, Path::new("/a/alpha")).unwrap();
        record_recent_in(&dir, Path::new("/b/bravo")).unwrap();
        let list = recent_projects_in(&dir);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].path, "/b/bravo", "the most recent is first");
        assert_eq!(list[0].name, "bravo");

        // Re-opening alpha moves it to the front and does not duplicate it.
        record_recent_in(&dir, Path::new("/a/alpha")).unwrap();
        let list = recent_projects_in(&dir);
        assert_eq!(list.len(), 2, "the same path is not listed twice");
        assert_eq!(list[0].path, "/a/alpha");
    }

    #[test]
    fn recents_are_capped() {
        let dir = tmp("capped");
        for i in 0..(MAX_RECENT + 5) {
            record_recent_in(&dir, &PathBuf::from(format!("/p/repo{i}"))).unwrap();
        }
        assert_eq!(
            recent_projects_in(&dir).len(),
            MAX_RECENT,
            "the list is bounded"
        );
    }

    #[test]
    fn a_corrupt_recent_file_reads_as_empty() {
        let dir = tmp("corrupt");
        std::fs::write(recent_file_in(&dir), "not json").unwrap();
        assert!(
            recent_projects_in(&dir).is_empty(),
            "garbage does not keep the window shut"
        );
    }

    // --- detection and config writing ---------------------------------------

    #[test]
    fn detect_reads_the_repo_and_environment_it_can() {
        let dir = tmp("detect");
        run_git(&dir, &["init", "-q"]);

        // No remote, no mise/direnv yet: honest fallbacks.
        let d = detect(&dir);
        assert_eq!(d.name, dir.file_name().unwrap().to_string_lossy());
        assert_eq!(d.repo, None, "no remote means no repo to watch");
        assert_eq!(d.env_source, "none");
        assert_eq!(d.worktrees, ".claude/worktrees");
        assert_eq!(
            d.base_branch, "origin/HEAD",
            "nothing fetched, so the daemon default"
        );

        // A GitHub origin becomes the repo to watch.
        run_git(
            &dir,
            &["remote", "add", "origin", "git@github.com:acme/thing.git"],
        );
        assert_eq!(detect(&dir).repo.as_deref(), Some("acme/thing"));

        // A mise.toml is read as the environment source.
        std::fs::write(dir.join("mise.toml"), "[tools]\n").unwrap();
        assert_eq!(detect(&dir).env_source, "mise");
    }

    #[test]
    fn detect_offers_a_compose_stack_and_conventional_scripts() {
        let dir = tmp("procs");
        std::fs::write(dir.join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"scripts":{"dev":"vite","build":"vite build","watch":"tsc -w"}}"#,
        )
        .unwrap();

        let procs = detect_processes(&dir);
        let labels: Vec<_> = procs.iter().map(|p| p.label.as_str()).collect();
        assert!(
            labels.contains(&"docker compose up"),
            "the compose stack is offered"
        );
        assert!(
            labels.contains(&"pnpm run dev"),
            "dev is a long-running script"
        );
        assert!(labels.contains(&"pnpm run watch"), "watch is too");
        assert!(
            !labels.iter().any(|l| l.contains("build\"")),
            "one-shot build is not offered"
        );
        assert!(
            !labels.contains(&"pnpm run build"),
            "a plain build is one-shot, not a managed process"
        );
        // The package manager comes from the lockfile.
        assert!(procs.iter().any(|p| p.command == ["pnpm", "run", "dev"]));
    }

    #[test]
    fn write_config_adds_ticked_processes_as_autostart() {
        let base = tmp("wcp");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);
        let cfg = base.join("config.json");

        write_config_to(
            &cfg,
            &repo,
            &Overrides {
                processes: vec![SelectedProcess {
                    name: "docker".into(),
                    command: vec!["docker".into(), "compose".into(), "up".into()],
                }],
                ..Default::default()
            },
        )
        .unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let procs = v["main_processes"]
            .as_array()
            .expect("main_processes written");
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0]["name"], "docker");
        assert_eq!(
            procs[0]["autostart"], true,
            "a ticked process starts with the daemon"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn write_config_is_slim_and_validated() {
        let base = tmp("wc");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);
        let cfg = base.join("config.json");

        write_config_to(
            &cfg,
            &repo,
            &Overrides {
                base_branch: Some("upstream/develop".into()),
                repo: Some("acme/thing".into()),
                env_source: Some(orchd::config::EnvSourceKind::Direnv),
                ..Default::default()
            },
        )
        .unwrap();

        let written = std::fs::read_to_string(&cfg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["upstream_ref"], "upstream/develop");
        assert_eq!(
            v["upstream_remote"], "upstream",
            "the remote is split off the ref"
        );
        assert_eq!(v["repo"], "acme/thing");
        assert_eq!(v["env_source"], "direnv");
        // Slim: nothing that was not asked for.
        assert!(v.get("poll_seconds").is_none(), "defaults are left out");

        // A bad override never reaches this function at all: the field is typed,
        // so serde refuses the request that carries it. Asserted where the refusal
        // now lives, because a value that cannot be constructed cannot be passed.
        assert!(
            serde_json::from_str::<Overrides>(r#"{"path":"/x","env_source":"nonsense"}"#).is_err(),
            "an unknown env source has to be refused while the request is read"
        );
        // The spellings the page really sends, so a rename of either enum fails
        // here rather than on somebody's first run.
        let ok: Overrides = serde_json::from_str(r#"{"path":"/x","env_source":"mise"}"#)
            .expect("the page's own values");
        assert_eq!(ok.env_source, Some(orchd::config::EnvSourceKind::Mise));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Re-picking a checkout keeps every key the review did not answer, and keeps
    /// the previous file beside it. Building the object from scratch lost them all.
    #[test]
    fn write_config_merges_into_the_existing_file_and_keeps_a_backup() {
        let base = tmp("wc-merge");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git_repo(&repo);
        let cfg = base.join("config.json");
        let old = r#"{"main_checkout":"/somewhere/else","worktree_setup":["mise","trust"],"repo":"acme/thing"}"#;
        std::fs::write(&cfg, old).unwrap();

        write_config_to(&cfg, &repo, &Overrides::default()).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["main_checkout"], repo.to_string_lossy().as_ref());
        assert_eq!(v["worktree_setup"], serde_json::json!(["mise", "trust"]));
        assert_eq!(
            v["repo"], "acme/thing",
            "a key the review did not answer is left alone"
        );
        assert_eq!(
            std::fs::read_to_string(base.join("config.json.bak")).unwrap(),
            old,
            "the previous file is kept"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The review pre-fills `repo` from a remote, so writing it back pinned a
    /// default as an override. Only a repo the remote does not derive is written.
    #[test]
    fn write_config_pins_a_repo_only_when_the_remote_does_not_derive_it() {
        let base = tmp("wc-repo");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(
            &repo,
            &["remote", "add", "origin", "git@github.com:acme/thing.git"],
        );
        let cfg = base.join("config.json");

        let with = |r: &str| Overrides {
            repo: Some(r.into()),
            ..Default::default()
        };
        write_config_to(&cfg, &repo, &with("acme/thing")).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(
            v.get("repo").is_none(),
            "the derived repo is not pinned: {v}"
        );

        write_config_to(&cfg, &repo, &with("other/name")).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["repo"], "other/name");

        // Answering with the derived one again lifts the pin.
        write_config_to(&cfg, &repo, &with("acme/thing")).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(v.get("repo").is_none(), "{v}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A fork layout is what a checkout can answer for itself, and it used to be
    /// answered only by the daemon's own first write — which this write pre-empts.
    /// So the detection lives here too, for the review's pre-fill and for a recent
    /// that opens with no review at all.
    #[test]
    fn a_fork_layout_is_detected_by_the_review_and_by_an_unreviewed_open() {
        let base = tmp("wc-fork");
        let repo = base.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q", "-b", "main"]);
        run_git(
            &repo,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        );
        run_git(
            &repo,
            &["remote", "add", "origin", "git@github.com:fork/thing.git"],
        );
        run_git(
            &repo,
            &["remote", "add", "upstream", "git@github.com:acme/thing.git"],
        );
        run_git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        run_git(
            &repo,
            &["update-ref", "refs/remotes/upstream/develop", "HEAD"],
        );

        let d = detect(&repo);
        assert_eq!(
            d.base_branch, "upstream/develop",
            "the upstream's branch, not the fork's"
        );
        assert_eq!(
            d.repo.as_deref(),
            Some("acme/thing"),
            "the upstream's repo, not the fork's"
        );

        let cfg = base.join("config.json");
        write_config_to(&cfg, &repo, &Overrides::default()).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["upstream_remote"], "upstream");
        assert!(
            v["upstream_ref"].as_str().unwrap().starts_with("upstream/"),
            "an unreviewed open still measures against upstream: {v}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
