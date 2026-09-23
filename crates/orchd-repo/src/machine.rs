//! What the daemon needs from the machine, said once at boot.
//!
//! Every check here answers a question that otherwise gets answered *late*, by a
//! failure that does not name its own cause:
//!
//! - No `claude` on PATH: every spawned session exits instantly, leaving a
//!   workspace record for a worktree nobody ever saw open.
//! - No interpreter for `reviews_command`: the pane blames the review command for
//!   a binary the command never mentions — the same shape as the GNU `timeout`
//!   trap that `proc::run_bounded` exists to avoid.
//! - A tracker configured but not declared in the repo's `.mcp.json`: Claude Code
//!   drops a pending MCP server **silently**, so the tool is simply not there and
//!   the story pass burns its whole timeout in the middle of a review.
//!
//! **Warnings, never refusals.** A daemon that will not start because `gh` is
//! missing is worse than one whose PR pane is empty, and the person running it may
//! not want the half that is absent. Each warning names the thing, what stops
//! working, and nothing else — a boot log nobody reads is one that cried wolf.

use crate::config::Config;
use serde::Serialize;
use std::path::Path;

/// Something missing, and what it costs.
///
/// **In the snapshot as well as the log, which was the whole gap.** Every one of
/// these used to be a `tracing::warn!` in `lib.rs` and nothing else, so a person
/// whose `gh` is missing read `unavailable` in the PR pane with the cause in a
/// file they do not have open — and the module's own docs above say that is the
/// case it exists for. A launcher-started app makes it worse: there is no terminal
/// the log could have appeared in.
///
/// **Exported to `repo.d.ts`, not `snapshot.d.ts`**, the way `TokenSource` beside
/// it is. ts-rs writes a file per crate run, so a type in *this* crate declaring
/// `snapshot.d.ts` truncates that file to whatever this crate alone exports — the
/// whole daemon's types gone, and `check-web` catching it only because it
/// regenerates and diffs.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(
    any(test, feature = "test-util"),
    derive(ts_rs::TS),
    ts(export, export_to = "repo.d.ts")
)]
pub struct Warning {
    /// The thing that is not there.
    pub what: String,
    /// What stops working because of it.
    pub cost: String,
}

/// Is this executable on `PATH`?
///
/// `which` rather than a shell builtin: the daemon's other probes already use it
/// (`update`, `api`), and every command it spawns is deliberately POSIX.
/// A `which` that cannot run at all answers "yes" — a preflight that produces a
/// false alarm on a machine it cannot inspect is worse than one that says nothing.
fn on_path(exe: &str) -> bool {
    std::process::Command::new("which")
        .arg(exe)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(true)
}

/// The interpreter a script's shebang asks for, as a bare command name.
///
/// Derived rather than assumed: the shipped review queue is `#!/usr/bin/env node`,
/// but `reviews_command` is *yours* to point anywhere, and hardcoding `node` would
/// check the wrong thing the moment somebody points it at a shell or Python script.
///
/// `None` for a binary, an unreadable file, or no shebang — all of which mean
/// "nothing to check", not "broken".
fn interpreter_of(script: &Path) -> Option<String> {
    use std::io::Read;
    // Bounded: this may be pointed at anything, including something enormous.
    let mut head = [0u8; 256];
    let mut f = std::fs::File::open(script).ok()?;
    let n = f.read(&mut head).ok()?;
    let line = head[..n].split(|b| *b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim_end_matches('\r');
    let rest = line.strip_prefix("#!")?.trim();
    let mut parts = rest.split_whitespace();
    let first = parts.next()?;
    // `#!/usr/bin/env node` names the interpreter in the *second* word.
    let exe = if Path::new(first).file_name().is_some_and(|f| f == "env") {
        parts.next()?
    } else {
        first
    };
    Some(Path::new(exe).file_name()?.to_string_lossy().into_owned())
}

/// Does the repo declare an MCP server by this name?
///
/// `None` when there is no readable `.mcp.json` at all, which is a different
/// finding from "the file is there and your tracker is not in it" — the first is
/// usually "you have not set this up yet", the second is a typo.
fn declares_mcp_server(main: &Path, name: &str) -> Option<bool> {
    let raw = std::fs::read_to_string(main.join(".mcp.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(v.get("mcpServers")?.get(name).is_some())
}

/// Whether this directory is a git work tree.
///
/// Asked of the directory rather than of `.git`, because both shapes are legitimate:
/// a plain checkout has a `.git` directory, a worktree has a `.git` *file*, and a
/// repository marked `core.bare` has a perfectly good `.git` and still refuses every
/// working-tree command. Only git can tell them apart, so git is asked.
///
/// Failure to run answers `true`. A preflight that cries wolf on a machine it cannot
/// inspect is worse than one that says nothing — the same rule [`on_path`] follows.
/// What asking git about a directory answered.
///
/// **"git said no" and "git could not run" are different questions**, and this
/// used to return `bool` for both. A failing `git` therefore read as "that
/// directory is not a work tree", which is a wrong conclusion drawn from a true
/// observation — the shape this whole module exists to remove. It is not
/// hypothetical: a Mac running the app under Rosetta has a `git` that cannot load
/// `libxcrun`, and every call fails with an architecture error the user then does
/// not see, because the daemon has already translated it into a sentence about
/// their checkout.
enum WorkTree {
    Yes,
    No,
    /// git ran and failed, or could not be run at all. Carries what it said,
    /// because that text is the only thing that names the real cause.
    Unusable(String),
}

fn is_work_tree(dir: &Path) -> WorkTree {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &dir.to_string_lossy(),
            "rev-parse",
            "--is-inside-work-tree",
        ])
        .output();
    let out = match out {
        Ok(out) => out,
        Err(e) => return WorkTree::Unusable(format!("git could not be run: {e}")),
    };
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        let said = said.trim();
        /* git's own refusal, which really is about the directory. Anything else —
        a loader error, a missing library, a translated architecture — is about
        the *machine*, and saying "not a work tree" about it sends somebody to
        look at their repository. */
        return if said.contains("not a git repository") {
            WorkTree::No
        } else {
            WorkTree::Unusable(said.lines().next().unwrap_or("git failed").to_string())
        };
    }
    if String::from_utf8_lossy(&out.stdout).trim() == "false" {
        WorkTree::No
    } else {
        WorkTree::Yes
    }
}

/// What is running under Rosetta: nothing, this app, or only what it starts.
///
/// **Three answers, because the remedy differs and the old one had a single
/// answer with the wrong remedy attached.** #18 came from a Mac where all three
/// binaries were arm64 and `git` still failed with `need 'x86_64'`: the app was
/// native and the processes it started were not. The warning said "this app is
/// running under Rosetta" and sent the reporter to Finder ▸ Get Info on an
/// install that has no `.app` bundle at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Translation {
    /// Nothing is translated, or this is not an Apple Silicon Mac.
    None,
    /// This process is translated, and so is everything it starts.
    App,
    /// This process is native and the processes it starts are not.
    Children,
}

/// Whether a process this app *starts* is being translated.
///
/// **`sysctl` the command, not `sysctlbyname` the call**, which is the one
/// decision here worth writing down. The FFI version is one call and needs
/// `unsafe`; this module is a growing list of *checks*, and opting it out of the
/// workspace's `unsafe_code` deny would hand that allowance to every check added
/// after this one. Three modules opt out today and each names the calls it makes.
/// One exec on a start that already makes eleven, on macOS only, is the cheaper
/// trade.
///
/// **And the spawn is what makes the answer useful rather than a bug.**
/// `sysctl.proc_translated` is *per process*, so asking it through a child
/// answers for the child — which is the process class that actually matters here,
/// because the failure is always a child (`git`) and never this process. What the
/// spawn cannot tell you is whether *this* process is translated too, and
/// [`translation_from`] gets that from the architecture this binary was built for.
///
/// `sysctl.proc_translated` does not exist on an Intel Mac, where `sysctl` exits
/// non-zero — read as "not translated", which is correct there.
#[cfg(target_os = "macos")]
fn child_translated() -> bool {
    std::process::Command::new("sysctl")
        .args(["-n", "sysctl.proc_translated"])
        .output()
        .is_ok_and(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "1")
}

#[cfg(not(target_os = "macos"))]
fn child_translated() -> bool {
    false
}

/// Fold the two observations into the one that has a remedy.
///
/// `arch` is [`std::env::consts::ARCH`] — what this binary was *built* for, which
/// is the half a spawned `sysctl` cannot report. An `aarch64` build cannot be
/// translated, so a translated child under one means the x86_64 preference was
/// set for this app rather than chosen by the kernel for it.
///
/// **Where that preference comes from is settled, and it is this repo's own
/// fault.** The first version of this guessed "inherited from whatever launched
/// it" and told people to use `arch -arm64`; #18's launch record disproved both.
/// The `.app` bundle `--install-desktop-entry` writes has a `/bin/sh` script as
/// its `CFBundleExecutable`, so LaunchServices had no Mach-O to read an
/// architecture from and defaulted to x86_64 first — while the application that
/// launched it was itself arm64. `launcher.rs` declares
/// `LSArchitecturePriority` now, which is why the remedy below is to rewrite the
/// bundle rather than to change how it is started.
fn translation_from(arch: &str, child_translated: bool) -> Translation {
    if !child_translated {
        return Translation::None;
    }
    if arch == "x86_64" {
        Translation::App
    } else {
        Translation::Children
    }
}

/// The warning for a translation state, or `None` when there is nothing to say.
///
/// Its own function so the wording is testable: the defect this replaces was
/// never in the detection, it was in the sentence, and a sentence no test reads
/// is a sentence that can say anything.
pub(crate) fn translation_warning(t: Translation) -> Option<Warning> {
    match t {
        Translation::None => None,
        Translation::App => Some(Warning {
            what: "this app is running under Rosetta on an Apple Silicon Mac".into(),
            cost: "every process it starts inherits x86_64, so `git` fails to load \
                   `libxcrun` and nothing that reads the repository works; run the arm64 \
                   build — for an app bundle, Finder ▸ Get Info ▸ uncheck \"Open using \
                   Rosetta\""
                .into(),
        }),
        Translation::Children => Some(Warning {
            what: "this app is native, but every process it starts runs under Rosetta".into(),
            cost: "`git` fails to load `libxcrun`, so nothing that reads the repository \
                   works; the cause is usually an app bundle written before the \
                   architecture was declared in it — run \
                   `orchestrator-desktop --install-desktop-entry` to rewrite it, then \
                   launch again"
                .into(),
        }),
    }
}

/// Whether Claude Code has been trusted in this directory.
///
/// `Some(false)` is the only actionable answer: trust is recorded per directory in
/// `~/.claude.json`, so an absent file, an unreadable one or a shape that has changed
/// all mean "cannot tell" and answer `None`. Read rather than inferred, because the
/// alternative — spawning a session to see whether it survives — is the failure this
/// exists to describe.
///
/// The key is the directory as Claude Code spells it, which is the absolute path.
fn trust_accepted(dir: &Path) -> Option<bool> {
    let home = std::env::var("HOME").ok()?;
    trust_accepted_in(Path::new(&home), dir)
}

/// The half that reads a file, with the home directory handed in.
///
/// Split so the test needs no `set_var`: `HOME` is process-global, the suite runs in
/// threads, and half of what the daemon reads is keyed on it. Setting it "for one
/// test" sets it for whatever else is running at that moment.
fn trust_accepted_in(home: &Path, dir: &Path) -> Option<bool> {
    let raw = std::fs::read_to_string(home.join(".claude.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let entry = v.get("projects")?.get(dir.to_string_lossy().as_ref())?;
    // Present and false is untrusted. Present without the key is a project Claude
    // Code knows and has not recorded a decision for, which is not a refusal.
    entry.get("hasTrustDialogAccepted")?.as_bool()
}

/// Everything worth saying about this machine, in boot order.
///
/// `tracker_server` is the MCP server name the configured tracker needs, or `None`
/// when no tracker is configured. Passed in rather than resolved here, because the
/// caller already built the tracker to write the hook settings.
pub fn check(cfg: &Config, tracker_server: Option<&str>) -> Vec<Warning> {
    let mut out = Vec::new();

    if !on_path("claude") {
        out.push(Warning {
            what: "`claude` is not on PATH".into(),
            cost: "every session will exit the moment it spawns".into(),
        });
    }

    // Named in the README's prerequisites and checked nowhere until now. Nothing
    // here degrades without it: worktrees, branch moves and every diff are `git`.
    if !on_path("git") {
        out.push(Warning {
            what: "`git` is not on PATH".into(),
            cost: "no worktree can be cut and no changed file can be read".into(),
        });
    }

    // The one prerequisite whose failure is silent. `claude` refuses an untrusted
    // directory, the session exits before its first turn, and
    // `watch_session_exit` forgets a turnless session on purpose — so the rail row
    // *disappears* and the new-worktree button reads as doing nothing at all.
    if trust_accepted(&cfg.main_checkout) == Some(false) {
        out.push(Warning {
            what: format!(
                "Claude Code's workspace trust has not been accepted for {}",
                cfg.main_checkout.display()
            ),
            cost: "sessions will die on spawn and their rows will vanish; run `claude` \
                   there once and accept the dialog"
                .into(),
        });
    }

    // A checkout that is not a work tree measures nothing, and says so only in a
    // reconcile warning per poll. Seen for real: a `git filter-repo` run left
    // `core.bare = true` behind, and the panes went stale with no error in the app.
    match is_work_tree(&cfg.main_checkout) {
        WorkTree::Yes => {}
        WorkTree::No => out.push(Warning {
            what: format!("{} is not a git work tree", cfg.main_checkout.display()),
            cost: "the changed-file pane and the divergence strip stay empty, and every \
                   poll fails"
                .into(),
        }),
        // Not about the checkout at all. Say what git said, because that sentence
        // names the real cause and nothing else here can.
        WorkTree::Unusable(said) => out.push(Warning {
            what: format!("git cannot run here: {said}"),
            cost: "nothing that reads the repository works — every pane stays empty and \
                   every poll fails"
                .into(),
        }),
    }

    /* Named before the git warning above would be read, because it *explains* it.
    A translated process runs a translated `git`, and on Apple Silicon that git
    cannot load `libxcrun`. */
    if let Some(w) =
        translation_warning(translation_from(std::env::consts::ARCH, child_translated()))
    {
        out.push(w);
    }

    // Not fatal on its own: the daemon reaches GitHub with `curl`, and only the
    // *credential* ladder ends at `gh auth token`. So this costs the PR pane only
    // when no token is configured another way.
    if !on_path("gh") && cfg.github_token_file.is_none() {
        out.push(Warning {
            what: "`gh` is not on PATH and no github_token_file is set".into(),
            cost: "PRs and the review queue stay empty".into(),
        });
    }

    if let Some(argv0) = cfg.reviews_command.first() {
        let script = Path::new(argv0);
        if !script.exists() && !on_path(argv0) {
            out.push(Warning {
                what: format!("reviews_command `{argv0}` is not there"),
                cost: "the review queue pane reads as unavailable".into(),
            });
        } else if let Some(interp) = interpreter_of(script) {
            if !on_path(&interp) {
                out.push(Warning {
                    what: format!("`{interp}` is not on PATH, and reviews_command needs it"),
                    cost: "the review queue fails at the spawn, blaming the command".into(),
                });
            }
        }
    }

    if let Some(w) = two_installs_warning(
        orchd_base::install::Install::of_running(),
        &orchd_base::install::packages_present(),
    ) {
        out.push(w);
    }

    if let Some(server) = tracker_server {
        match declares_mcp_server(&cfg.main_checkout, server) {
            Some(true) => {}
            Some(false) => out.push(Warning {
                what: format!("the repo's `.mcp.json` declares no `{server}` server"),
                cost: "filing a story will hang until it times out".into(),
            }),
            None => out.push(Warning {
                what: format!("no readable `.mcp.json` in the repo, so `{server}` is not there"),
                cost: "filing a story will hang until it times out".into(),
            }),
        }
    }

    out
}

/// Said when this build is not a package and a package is installed too.
///
/// **The launcher can open either, and that is invisible from inside.** A mise
/// install's entry carries the packages' id, so somebody who switched to the cask
/// kept opening the old mise build, and its bar told them to upgrade it. The
/// launcher now stops writing that entry, which cannot help somebody who starts
/// this build some other way, so this says it where they will see it.
///
/// Only for `Tarball`, which is what a mise install and the bundle it wrote both
/// classify as. A package with a second package beside it is somebody's own
/// arrangement, and a build in a checkout is a developer who knows.
fn two_installs_warning(
    running: orchd_base::install::Install,
    others: &[orchd_base::install::Install],
) -> Option<Warning> {
    if running != orchd_base::install::Install::Tarball || others.is_empty() {
        return None;
    }
    let names: Vec<&str> = others.iter().map(|i| i.name()).collect();
    Some(Warning {
        what: format!(
            "Orchestrator is installed twice: this is a mise or tarball copy, and {} \
             installed it too",
            names.join(" and ")
        ),
        cost: "the launcher can open either one, so an upgrade can seem to do nothing; \
               remove the one you do not use, with `mise uninstall` if it is mise's"
            .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mise build with the cask beside it is told, and nothing else is.
    #[test]
    fn two_installs_are_named_only_from_the_one_that_is_not_a_package() {
        use orchd_base::install::Install;
        let w = two_installs_warning(Install::Tarball, &[Install::Homebrew])
            .expect("mise beside a cask");
        assert!(w.what.contains("Homebrew"), "{}", w.what);
        assert!(two_installs_warning(Install::Tarball, &[]).is_none());
        // The cask itself is the answer, not the problem.
        assert!(two_installs_warning(Install::Homebrew, &[Install::Homebrew]).is_none());
        assert!(two_installs_warning(Install::Checkout, &[Install::Apt]).is_none());
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        crate::testutil::scratch(&format!("pre-{name}"))
    }

    /// The three shapes a checkout can be in, told apart.
    ///
    /// `core.bare` is the one worth a test: the repository is real, `.git` is there
    /// and complete, and every working-tree command still refuses. A `.git` existence
    /// check would call it healthy, which is how a `git filter-repo` run left the
    /// panes stale for a day with no error in the app.
    #[test]
    fn a_bare_repo_and_a_plain_directory_are_both_refused_as_work_trees() {
        let root = tmp("worktree-shapes");
        let repo = root.join("repo");
        let plain = root.join("plain");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git");
        };
        git(&["init", "-q"]);
        assert!(
            matches!(is_work_tree(&repo), WorkTree::Yes),
            "a fresh checkout is a work tree"
        );

        // Real repository, real `.git`, and no work tree.
        git(&["config", "core.bare", "true"]);
        assert!(
            matches!(is_work_tree(&repo), WorkTree::No),
            "core.bare is not a work tree"
        );

        /* Not a repository at all, which is what picking the wrong folder gives
        you — and it must stay `No` rather than joining the arm below, because
        only *this* is a fact about the checkout.

        **The third answer is the point.** This returned `bool`, so a `git` that
        could not run at all read as "that is not a work tree": a wrong
        conclusion drawn from a true observation, which is the failure this
        module exists to prevent. Reported from a Mac running under Rosetta,
        where every git call died loading `libxcrun` and the app answered with a
        sentence about the user's repository. */
        assert!(matches!(is_work_tree(&plain), WorkTree::No));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Trust is only reported when the answer is actually "no".
    ///
    /// Everything else has to be `None`: an absent `~/.claude.json`, a directory
    /// Claude Code has never seen, or a shape that has changed under us. A warning
    /// telling somebody to accept a dialog they already accepted is the kind of
    /// false alarm that teaches people to skip the boot log.
    #[test]
    fn trust_is_unknown_unless_the_file_says_no() {
        let root = tmp("trust");
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let dir = root.join("checkout");
        let shown = dir.to_string_lossy().into_owned();
        let write = |body: &str| std::fs::write(home.join(".claude.json"), body).unwrap();

        // The home directory is a parameter here on purpose: `HOME` is
        // process-global and the suite runs in threads.
        let at = |d: &std::path::Path| trust_accepted_in(&home, d);

        assert_eq!(at(&dir), None, "no file at all is not a refusal");
        write("{ not json");
        assert_eq!(at(&dir), None, "nor is a file we cannot parse");
        write(r#"{"projects":{}}"#);
        assert_eq!(at(&dir), None, "nor a project it has never seen");
        write(&format!(r#"{{"projects":{{"{shown}":{{}}}}}}"#));
        assert_eq!(at(&dir), None, "nor one with no decision recorded");

        write(&format!(
            r#"{{"projects":{{"{shown}":{{"hasTrustDialogAccepted":false}}}}}}"#
        ));
        assert_eq!(at(&dir), Some(false), "this one is the warning");
        write(&format!(
            r#"{{"projects":{{"{shown}":{{"hasTrustDialogAccepted":true}}}}}}"#
        ));
        assert_eq!(at(&dir), Some(true));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_interpreter_comes_off_the_shebang_including_through_env() {
        let d = tmp("shebang");
        let write = |n: &str, body: &str| {
            let p = d.join(n);
            std::fs::write(&p, body).unwrap();
            p
        };
        // The shipped queue's own form.
        assert_eq!(
            interpreter_of(&write("a.js", "#!/usr/bin/env node\nconsole.log(1)\n")).as_deref(),
            Some("node")
        );
        // A direct path names the interpreter in the first word.
        assert_eq!(
            interpreter_of(&write("b.sh", "#!/bin/sh\necho hi\n")).as_deref(),
            Some("sh")
        );
        // `env` with a flag still finds the command after it.
        assert_eq!(
            interpreter_of(&write("c.py", "#!/usr/bin/env python3\nprint(1)\n")).as_deref(),
            Some("python3")
        );
        // No shebang, and a binary, are both "nothing to check" rather than broken.
        assert_eq!(interpreter_of(&write("d.txt", "just text\n")), None);
        assert_eq!(interpreter_of(&d.join("nope")), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_shebang_longer_than_the_buffer_does_not_panic() {
        let d = tmp("long");
        let p = d.join("big.js");
        // No newline at all inside the window: the first "line" is the whole read.
        std::fs::write(&p, "#!/usr/bin/env ".to_string() + &"x".repeat(4000)).unwrap();
        let _ = interpreter_of(&p);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_declared_server_is_distinguished_from_a_missing_file() {
        let d = tmp("mcp");
        // Absent file is "no opinion", which the caller words differently.
        assert_eq!(declares_mcp_server(&d, "shortcut"), None);

        std::fs::write(
            d.join(".mcp.json"),
            r#"{"mcpServers":{"shortcut":{"type":"stdio","command":"x"}}}"#,
        )
        .unwrap();
        assert_eq!(declares_mcp_server(&d, "shortcut"), Some(true));
        // Present, but not the one the tracker needs — the typo case.
        assert_eq!(declares_mcp_server(&d, "linear"), Some(false));

        // Unparseable is treated as unreadable, not as a declaration.
        std::fs::write(d.join(".mcp.json"), "{not json").unwrap();
        assert_eq!(declares_mcp_server(&d, "shortcut"), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_probe_that_cannot_run_does_not_cry_wolf() {
        // `sh` is on every machine this runs on; the point of the assertion is the
        // direction of the default, which is "say nothing" rather than "warn".
        assert!(on_path("sh"));
        assert!(!on_path("orchd-definitely-not-a-real-binary"));
    }

    /// An arm64 build with a translated child is not an app running under Rosetta.
    ///
    /// The whole of #18's second problem, as a table. The detection was right and
    /// the conclusion was wrong: a spawned `sysctl` answers for the *child*, so
    /// "the child is translated" was read as "this app is translated" — and on an
    /// `aarch64` binary that is impossible.
    #[test]
    fn a_native_build_with_a_translated_child_is_not_an_app_under_rosetta() {
        use Translation::{App, Children, None as Native};
        for (arch, child, want) in [
            ("aarch64", false, Native),
            ("x86_64", false, Native),
            ("aarch64", true, Children),
            ("x86_64", true, App),
        ] {
            assert_eq!(
                translation_from(arch, child),
                want,
                "{arch} with child_translated={child}"
            );
        }
    }

    /// Advice that names a thing the reader may not have is advice that costs a day.
    ///
    /// **The sentence is the deliverable here, so the sentence is what is asserted.**
    /// The reporter was sent to Finder ▸ Get Info for an install that is three
    /// binaries under `~/.local/share/mise`, with no bundle to get info on. So the
    /// bundle-only remedy may only appear where the app itself is translated, and
    /// the inherited case has to name what it actually is.
    #[test]
    fn each_translation_warning_names_a_remedy_that_fits_its_case() {
        assert!(translation_warning(Translation::None).is_none());

        let app = translation_warning(Translation::App).expect("a translated app is a warning");
        assert!(
            app.cost.contains("app bundle"),
            "the bundle remedy must say it is for a bundle: {}",
            app.cost
        );

        let kids =
            translation_warning(Translation::Children).expect("translated children are a warning");
        assert!(
            !kids.what.contains("this app is running under Rosetta"),
            "a native app must not be told it is translated: {}",
            kids.what
        );
        /* **The remedy has to be the one that works, and the first one was not.**
        This asserted `arch -arm64` on the theory that the preference came from
        the launching shell. #18's launch record showed it comes from our own
        bundle's missing `LSArchitecturePriority`, with an arm64 app doing the
        launching — so the shell advice would have sent somebody to change
        something that was already correct. */
        assert!(
            kids.cost.contains("--install-desktop-entry"),
            "the remedy must rewrite the bundle, which is where the preference lives: {}",
            kids.cost
        );
        assert!(
            !kids.cost.contains("arch -arm64"),
            "the shell remedy was disproved by #18's launch record: {}",
            kids.cost
        );
        // Both still name the symptom the person actually sees, which is the only
        // reason either warning is read at all.
        for w in [&app, &kids] {
            assert!(w.cost.contains("libxcrun"), "{}", w.cost);
        }
    }
}
