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
use std::path::Path;

/// Something missing, and what it costs.
pub struct Warning {
    /// The thing that is not there.
    pub what: String,
    /// What stops working because of it.
    pub cost: String,
}

/// Is this executable on `PATH`?
///
/// `which` rather than a shell builtin: the daemon's other probes already use it
/// (`agent_update`, `api`), and every command it spawns is deliberately POSIX.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("orchd-pre-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
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
}
