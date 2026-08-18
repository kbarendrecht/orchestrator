use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Two independent questions per suite, per workspace (§7): does the result
/// reflect this workspace's code, and does running it mutate state someone else
/// owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Suite {
    Static,
    Unit,
    Integration,
    #[serde(rename = "e2e")]
    E2E,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// The result reflects this workspace's code.
    Verified,
    /// Reflects this workspace, but a cached artifact may be out of date.
    Stale,
    /// Meaningless here — never act on it.
    Untrusted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Isolation {
    Isolated,
    /// Requires a global lock before running: two runs taking `main:instances`
    /// would fight over one instances dir (§7 rule 2).
    SharedResource { resource: String },
}

/// Capabilities are **config, not hardcoded** — they have already changed once
/// (§7 rule 6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteSpec {
    pub suite: Suite,
    pub command: Vec<String>,
    /// Trust when run from a worktree. Main is always `Verified`.
    #[serde(default = "verified")]
    pub worktree_trust: Trust,
    #[serde(default = "isolated")]
    pub isolation: Isolation,
    /// Lockfiles whose drift from main makes this suite `Stale` (§7 rule 3).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Run inside this container. Set it and the reported command becomes the
    /// `docker exec` form with the mapped working directory.
    #[serde(default)]
    pub container: Option<String>,
}

fn verified() -> Trust {
    Trust::Verified
}

fn isolated() -> Isolation {
    Isolation::Isolated
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityConfig {
    #[serde(default = "default_suites")]
    pub suites: Vec<SuiteSpec>,
    /// Host `<main>` maps to this path inside the containers.
    #[serde(default = "default_container_root")]
    pub container_root: String,
    /// Class the autoload probe resolves. Anything outside the worktree is a
    /// hard failure, not a warning (§7 rule 4).
    ///
    /// Defaults to composer's own loader, which exists in any composer project
    /// and answers the question directly: which `vendor/` tree is actually in
    /// play. Point it at an application class (`acme\\...`) for the stronger
    /// check of which `src/` is in play.
    #[serde(default = "default_probe_class")]
    pub autoload_probe_class: Option<String>,
}

impl Default for CapabilityConfig {
    fn default() -> Self {
        CapabilityConfig {
            suites: default_suites(),
            container_root: default_container_root(),
            autoload_probe_class: default_probe_class(),
        }
    }
}

fn default_container_root() -> String {
    "/acme".to_string()
}

fn default_probe_class() -> Option<String> {
    Some("Composer\\Autoload\\ClassLoader".to_string())
}

/// The post-WIP table from §7. Config overrides it; this is only the shape.
fn default_suites() -> Vec<SuiteSpec> {
    vec![
        SuiteSpec {
            suite: Suite::Static,
            command: vec!["mise".into(), "run".into(), "static".into()],
            worktree_trust: Trust::Verified,
            isolation: Isolation::Isolated,
            depends_on: vec!["composer.lock".into()],
            container: Some("toolbox".to_string()),
        },
        SuiteSpec {
            suite: Suite::Unit,
            command: vec!["mise".into(), "run".into(), "test:phpunit".into()],
            worktree_trust: Trust::Verified,
            isolation: Isolation::Isolated,
            depends_on: vec!["composer.lock".into()],
            container: Some("toolbox".to_string()),
        },
        SuiteSpec {
            suite: Suite::Integration,
            command: vec!["mise".into(), "run".into(), "test:integration".into()],
            worktree_trust: Trust::Verified,
            isolation: Isolation::Isolated,
            depends_on: vec!["composer.lock".into()],
            container: Some("toolbox".to_string()),
        },
        SuiteSpec {
            suite: Suite::E2E,
            command: vec!["mise".into(), "run".into(), "test:playwright".into()],
            worktree_trust: Trust::Verified,
            // Teardown anchors the instances dir on the main checkout, so this
            // reaches outside the worktree (§7).
            isolation: Isolation::SharedResource {
                resource: "main:instances".into(),
            },
            depends_on: vec!["pnpm-lock.yaml".into(), "composer.lock".into()],
            container: None,
        },
    ]
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Capability {
    pub suite: Suite,
    pub runnable: bool,
    pub trust: Trust,
    pub isolation: Isolation,
    /// The exact invocation for this workspace, already mapped into the
    /// container when the suite declares one (§7 rule 5).
    pub command: Vec<String>,
    /// Why trust is not `Verified`, when it isn't.
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DepDrift {
    pub file: String,
    /// Compared by content hash, never mtime (§7 rule 3).
    pub matches_main: bool,
    pub present: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum AutoloadProbe {
    /// Resolved inside the workspace, which is what it must do.
    Inside { file: String },
    /// Resolved outside — a hard failure, and exactly the regression this
    /// probe exists to catch.
    Outside { file: String },
    /// Could not run: no php, no vendor, or no class configured.
    Skipped { reason: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilityReport {
    pub workspace: String,
    pub is_main: bool,
    pub capabilities: Vec<Capability>,
    pub deps: Vec<DepDrift>,
    pub autoload: AutoloadProbe,
    /// Host path ↔ container path, for every command the daemon builds and
    /// every path it parses back out of test output (§7).
    pub container_path: Option<String>,
    /// Whether §7 **rule 1** would let `/green` act here: every suite runnable
    /// and none `Untrusted`. **Reporting only** — no automation is wired up.
    pub green_eligible: bool,
    pub green_blockers: Vec<String>,
    /// §7 **rule 2**, kept separate from eligibility: a shared resource needs a
    /// global lock before running, it does not make the PR ineligible. The lock
    /// conflicts with another *run* holding it, not with you working in main.
    pub locks_required: Vec<String>,
}

pub fn report(
    cfg: &CapabilityConfig,
    workspace: &str,
    path: &Path,
    main: &Path,
    is_main: bool,
) -> CapabilityReport {
    let deps = if is_main {
        Vec::new()
    } else {
        dep_drift(cfg, path, main)
    };
    let stale: Vec<&DepDrift> = deps.iter().filter(|d| d.present && !d.matches_main).collect();

    let autoload = probe_autoload(cfg, path);

    let capabilities: Vec<Capability> = cfg
        .suites
        .iter()
        .map(|spec| {
            let mut trust = if is_main {
                Trust::Verified
            } else {
                spec.worktree_trust
            };
            let mut note = None;

            // A stale copied autoload or lockfile freezes the result at copy
            // time, so the suite reports Stale rather than a test failure (§7
            // rule 3) — "deps stale, re-link from main" is the actionable form.
            if !is_main {
                let drifted: Vec<&str> = spec
                    .depends_on
                    .iter()
                    .filter(|f| stale.iter().any(|d| &&d.file == f))
                    .map(|s| s.as_str())
                    .collect();
                if !drifted.is_empty() && trust == Trust::Verified {
                    trust = Trust::Stale;
                    note = Some(format!("deps stale — re-link from main ({})", drifted.join(", ")));
                }
            }
            // An autoload resolving outside the workspace makes every PHP suite
            // meaningless here, whatever the lockfiles say.
            if let AutoloadProbe::Outside { file } = &autoload {
                trust = Trust::Untrusted;
                note = Some(format!("autoload resolves outside the workspace: {file}"));
            }

            // Container commands from a worktree always use `docker exec`,
            // never `docker compose exec` — that resolves the wrong compose
            // project from a worktree dir (§7 rule 5).
            let command = match (&spec.container, container_path(cfg, path, main)) {
                (Some(c), Some(workdir)) => container_command(c, &workdir, &spec.command),
                _ => spec.command.clone(),
            };

            Capability {
                suite: spec.suite,
                runnable: !spec.command.is_empty(),
                trust,
                isolation: spec.isolation.clone(),
                command,
                note,
            }
        })
        .collect();

    // §7 rule 1: `/green` may act only if **every** suite it would run is
    // runnable and not Untrusted. Otherwise the PR is NeedsMain — a button,
    // never an auto-run, because occupying main would interrupt your work.
    let mut blockers = Vec::new();
    for c in &capabilities {
        if !c.runnable {
            blockers.push(format!("{:?} has no command configured", c.suite));
        }
        if c.trust == Trust::Untrusted {
            blockers.push(format!(
                "{:?} is untrusted{}",
                c.suite,
                c.note.as_ref().map(|n| format!(" — {n}")).unwrap_or_default()
            ));
        }
    }
    let locks_required: Vec<String> = capabilities
        .iter()
        .filter_map(|c| match &c.isolation {
            Isolation::SharedResource { resource } => Some(format!(
                "{:?} takes the {resource} lock, which conflicts with main occupancy",
                c.suite
            )),
            Isolation::Isolated => None,
        })
        .collect();

    CapabilityReport {
        workspace: workspace.to_string(),
        is_main,
        green_eligible: blockers.is_empty(),
        green_blockers: blockers,
        locks_required,
        capabilities,
        deps,
        autoload,
        container_path: container_path(cfg, path, main),
    }
}

/// Compare lockfiles by content hash, not mtime (§7 rule 3).
fn dep_drift(cfg: &CapabilityConfig, path: &Path, main: &Path) -> Vec<DepDrift> {
    let mut files: Vec<String> = cfg
        .suites
        .iter()
        .flat_map(|s| s.depends_on.iter().cloned())
        .collect();
    files.sort();
    files.dedup();

    files
        .into_iter()
        .map(|f| {
            let here = hash_file(&path.join(&f));
            let there = hash_file(&main.join(&f));
            DepDrift {
                present: here.is_some(),
                matches_main: match (&here, &there) {
                    (Some(a), Some(b)) => a == b,
                    // Absent on both sides is not drift.
                    (None, None) => true,
                    _ => false,
                },
                file: f,
            }
        })
        .collect()
}

fn hash_file(p: &Path) -> Option<u64> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(p).ok()?;
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    Some(h.finish())
}

/// Cheap, and worth keeping even though the WIP fixed the underlying cause —
/// it is the regression detector for exactly this class of bug (§7 rule 4).
fn probe_autoload(cfg: &CapabilityConfig, path: &Path) -> AutoloadProbe {
    let Some(class) = cfg.autoload_probe_class.as_deref() else {
        return AutoloadProbe::Skipped {
            reason: "no autoload_probe_class configured".into(),
        };
    };
    if !path.join("vendor/autoload.php").exists() {
        return AutoloadProbe::Skipped {
            reason: "no vendor/autoload.php".into(),
        };
    }
    let script = format!(
        r#"require "vendor/autoload.php"; echo (new ReflectionClass({class}::class))->getFileName();"#
    );
    let out = Command::new("php")
        .args(["-r", &script])
        .current_dir(path)
        .output();
    let Ok(out) = out else {
        return AutoloadProbe::Skipped {
            reason: "php is not on PATH".into(),
        };
    };
    if !out.status.success() {
        return AutoloadProbe::Skipped {
            reason: String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("php exited non-zero")
                .to_string(),
        };
    }
    let file = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if file.is_empty() {
        return AutoloadProbe::Skipped {
            reason: "php produced no path".into(),
        };
    }
    // Resolve both sides: vendor is a symlink farm, so a lexical prefix test
    // would call a correct resolution wrong.
    let real = std::fs::canonicalize(&file).unwrap_or_else(|_| PathBuf::from(&file));
    let root = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if real.starts_with(&root) {
        AutoloadProbe::Inside { file }
    } else {
        AutoloadProbe::Outside { file }
    }
}

/// Host `<main>/.claude/worktrees/<name>` ↔ container
/// `<container_root>/.claude/worktrees/<name>` (§7).
pub fn container_path(cfg: &CapabilityConfig, path: &Path, main: &Path) -> Option<String> {
    let rel = path.strip_prefix(main).ok()?;
    let rel = rel.to_string_lossy();
    Some(if rel.is_empty() {
        cfg.container_root.clone()
    } else {
        format!("{}/{}", cfg.container_root, rel)
    })
}

/// Container commands from a worktree always use `docker exec <container>`,
/// never `docker compose exec` or `mise run silent:exec:toolbox` — those
/// resolve the wrong compose project from a worktree dir (§7 rule 5).
pub fn container_command(container: &str, workdir: &str, argv: &[String]) -> Vec<String> {
    let mut out = vec![
        "docker".to_string(),
        "exec".to_string(),
        "-w".to_string(),
        workdir.to_string(),
        container.to_string(),
    ];
    out.extend(argv.iter().cloned());
    out
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_a_worktree_path_into_the_container() {
        let cfg = CapabilityConfig::default();
        let main = Path::new("/home/k/development/acme");
        let wt = Path::new("/home/k/development/acme/.claude/worktrees/invoice");
        assert_eq!(
            container_path(&cfg, wt, main).as_deref(),
            Some("/acme/.claude/worktrees/invoice")
        );
        assert_eq!(container_path(&cfg, main, main).as_deref(), Some("/acme"));
    }

    #[test]
    fn container_commands_never_go_through_compose() {
        let cmd = container_command(
            "acme-fpm",
            "/acme/.claude/worktrees/x",
            &["php".to_string(), "-v".to_string()],
        );
        assert_eq!(cmd[0], "docker");
        assert_eq!(cmd[1], "exec");
        // `docker compose exec` resolves the wrong project from a worktree.
        assert!(!cmd.contains(&"compose".to_string()));
        assert!(cmd.ends_with(&["php".to_string(), "-v".to_string()]));
    }

    fn cfg_with(suites: Vec<SuiteSpec>) -> CapabilityConfig {
        CapabilityConfig {
            suites,
            ..Default::default()
        }
    }

    fn spec(suite: Suite, iso: Isolation, deps: &[&str]) -> SuiteSpec {
        SuiteSpec {
            suite,
            command: vec!["true".into()],
            worktree_trust: Trust::Verified,
            isolation: iso,
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            container: None,
        }
    }

    #[test]
    fn a_containerised_suite_reports_the_mapped_docker_exec_form() {
        let (main, wt) = scratch("container");
        let mut s = spec(Suite::Unit, Isolation::Isolated, &[]);
        s.container = Some("toolbox".into());
        s.command = vec!["php".into(), "vendor/bin/phpunit".into()];
        let r = report(&cfg_with(vec![s]), "x", &wt, &main, false);
        let cmd = &r.capabilities[0].command;
        assert_eq!(cmd[0], "docker");
        assert_eq!(cmd[1], "exec");
        // The working directory is the container path, not the host path.
        assert!(cmd.contains(&"/acme/.claude/worktrees/x".to_string()), "{cmd:?}");
        assert!(!cmd.iter().any(|a| a.contains("/tmp")), "leaked a host path: {cmd:?}");
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    fn scratch(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("orchd-cap-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let main = root.join("main");
        let wt = root.join("main/.claude/worktrees/x");
        std::fs::create_dir_all(&wt).unwrap();
        (main, wt)
    }

    #[test]
    fn a_drifted_lockfile_marks_the_suite_stale_not_failing() {
        let (main, wt) = scratch("drift");
        std::fs::write(main.join("composer.lock"), b"AAA").unwrap();
        std::fs::write(wt.join("composer.lock"), b"BBB").unwrap();

        let cfg = cfg_with(vec![spec(Suite::Unit, Isolation::Isolated, &["composer.lock"])]);
        let r = report(&cfg, "x", &wt, &main, false);
        assert_eq!(r.capabilities[0].trust, Trust::Stale);
        assert!(r.capabilities[0].note.as_ref().unwrap().contains("re-link from main"));
        // Still runnable: it is not a test failure, it is a staleness warning.
        assert!(r.capabilities[0].runnable);
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    #[test]
    fn identical_lockfiles_stay_verified() {
        let (main, wt) = scratch("same");
        std::fs::write(main.join("composer.lock"), b"SAME").unwrap();
        std::fs::write(wt.join("composer.lock"), b"SAME").unwrap();
        let cfg = cfg_with(vec![spec(Suite::Unit, Isolation::Isolated, &["composer.lock"])]);
        let r = report(&cfg, "x", &wt, &main, false);
        assert_eq!(r.capabilities[0].trust, Trust::Verified);
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    #[test]
    fn a_shared_resource_needs_a_lock_but_does_not_block_green() {
        // Rule 1 is about trust; rule 2 is about locking. Conflating them would
        // make every workspace with an e2e suite permanently ineligible.
        let (main, wt) = scratch("shared");
        let cfg = cfg_with(vec![spec(
            Suite::E2E,
            Isolation::SharedResource {
                resource: "main:instances".into(),
            },
            &[],
        )]);
        let r = report(&cfg, "x", &wt, &main, false);
        assert!(r.green_eligible, "blockers: {:?}", r.green_blockers);
        assert_eq!(r.locks_required.len(), 1);
        assert!(r.locks_required[0].contains("main:instances"));
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    #[test]
    fn an_untrusted_suite_does_block_green() {
        let (main, wt) = scratch("untrusted");
        let mut sp = spec(Suite::Unit, Isolation::Isolated, &[]);
        sp.worktree_trust = Trust::Untrusted;
        let r = report(&cfg_with(vec![sp]), "x", &wt, &main, false);
        assert!(!r.green_eligible);
        assert!(r.green_blockers[0].contains("untrusted"));
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    #[test]
    fn an_isolated_verified_suite_is_green_eligible() {
        let (main, wt) = scratch("ok");
        let cfg = cfg_with(vec![spec(Suite::Unit, Isolation::Isolated, &[])]);
        let r = report(&cfg, "x", &wt, &main, false);
        assert!(r.green_eligible, "blockers: {:?}", r.green_blockers);
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    #[test]
    fn a_suite_without_a_command_is_not_runnable() {
        let (main, wt) = scratch("nocmd");
        let mut s = spec(Suite::Unit, Isolation::Isolated, &[]);
        s.command.clear();
        let r = report(&cfg_with(vec![s]), "x", &wt, &main, false);
        assert!(!r.capabilities[0].runnable);
        assert!(!r.green_eligible);
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }

    #[test]
    fn main_is_always_verified_and_has_no_drift_to_report() {
        let (main, _) = scratch("main");
        let cfg = cfg_with(vec![spec(Suite::Unit, Isolation::Isolated, &["composer.lock"])]);
        let r = report(&cfg, "main", &main, &main, true);
        assert_eq!(r.capabilities[0].trust, Trust::Verified);
        assert!(r.deps.is_empty());
        let _ = std::fs::remove_dir_all(main.parent().unwrap());
    }
}
