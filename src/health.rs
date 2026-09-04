//! Reading a managed process's output, and what that means for the sessions
//! beside it.
//!
//! Two things live here, and they used to live in two other places:
//!
//! - **The parser** — [`verdict`], [`scan_lines`], [`strip_ansi`]. Classifying a
//!   build watcher's output is not spawning anything, so it was only in
//!   `spawn.rs` because `start_managed` happened to be the caller.
//! - **The rule** — [`at_rest`]. "A session at rest is `BuildFailing` if the
//!   build beside it is red, otherwise it is an idle agent" was written twice,
//!   once in `hooks.rs` when a turn ends and once in `spawn.rs` when health
//!   changes. Same rule, two triggers, two copies: the kind of pair where one
//!   gets a fix and the other does not.

use std::time::SystemTime;

use crate::config::ManagedSpec;
use crate::model::{Health, State, TurnReason};

/// What one line of output says about a managed process's health.
///
/// Split out from the scan loop because this is where the whole health model
/// actually lives, and it is the part worth pinning down in tests.
pub fn verdict(spec: &ManagedSpec, raw: &str) -> Option<Health> {
    let line = strip_ansi(raw.trim_end());
    if line.trim().is_empty() {
        return None;
    }
    if spec.failure_patterns.iter().any(|p| line.contains(p)) {
        return Some(Health::Failing {
            summary: line.trim().to_string(),
        });
    }
    if spec.ok_patterns.iter().any(|p| line.contains(p)) {
        return Some(Health::Ok);
    }
    None
}


/// Drain whole lines out of `pending` and return the health verdict they imply.
///
/// Keeps draining past a failure rather than stopping on the first error line.
/// A recovery line can sit in the buffer right after the failure — the esbuild
/// watcher prints `…generation failed.` and then, on the next build, `…generation
/// complete.` — and stopping early left that `complete` unparsed, so a build that
/// was fixed stayed red in the rail until some unrelated output happened to drain
/// it. The first error in a failing run still wins the summary; a later ok line
/// overrides it, which is exactly the recovery that was being lost.
pub fn scan_lines(spec: &ManagedSpec, pending: &mut String) -> Option<Health> {
    let mut changed = None;
    while let Some(nl) = pending.find('\n') {
        let line: String = pending.drain(..=nl).collect();
        match verdict(spec, &line) {
            Some(Health::Failing { summary }) => {
                if !matches!(changed, Some(Health::Failing { .. })) {
                    changed = Some(Health::Failing { summary });
                }
            }
            Some(h) => changed = Some(h),
            None => {}
        }
    }
    // Guard against a single unterminated line growing without bound.
    if pending.len() > 64 * 1024 {
        pending.clear();
    }
    changed
}


/// Strip CSI/OSC escape sequences so pattern matching sees the plain text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // CSI runs until a byte in 0x40..=0x7E.
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC runs until BEL or ST.
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}


/// Where a session that is not working belongs, given the build beside it.
///
/// The one place the red-outranks-ochre rule (§2) is written. Both callers reach
/// it from a different direction — a turn ending, or a process changing health —
/// and each keeps its own guard about *which* sessions to apply it to, because
/// those genuinely differ. What must not differ is this mapping.
pub fn at_rest(build_failure: Option<&str>) -> State {
    match build_failure {
        Some(summary) => State::BuildFailing {
            summary: summary.to_string(),
        },
        // The turn is still finished, so it goes back to being an idle agent
        // rather than to nothing at all.
        None => State::YourTurn {
            since: SystemTime::now(),
            reason: TurnReason::TurnComplete,
        },
    }
}

/// The first red **managed** process in a workspace, if any.
///
/// Managed only: a shell you opened is yours to read, and its exit code is not a
/// statement about the build.
pub fn build_failure_in(inner: &crate::state::Inner, workspace: &str) -> Option<String> {
    inner.workspaces.get(workspace).and_then(|w| {
        w.processes.iter().find_map(|p| match &p.health {
            Health::Failing { summary } if p.is_managed() => Some(summary.clone()),
            _ => None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colour_codes_before_matching() {
        let line = "\x1b[31mError:\x1b[0m TS2304: cannot find name";
        assert_eq!(strip_ansi(line), "Error: TS2304: cannot find name");
    }

    #[test]
    fn strips_osc_titles() {
        assert_eq!(strip_ansi("\x1b]0;title\x07done"), "done");
    }


    fn ng() -> ManagedSpec {
        ManagedSpec {
            name: "ng-watch".into(),
            command: vec!["true".into()],
            failure_patterns: vec![
                "Error:".into(),
                "ERROR in".into(),
                "error TS".into(),
                "✘ [ERROR]".into(),
                "bundle generation failed".into(),
            ],
            ok_patterns: vec![
                "Build at:".into(),
                "successfully".into(),
                "watching for file changes".into(),
                "bundle generation complete".into(),
            ],
            restart: crate::config::RestartPolicy::Never,
            autostart: false,
            stop_command: Vec::new(),
        }
    }

    #[test]
    fn an_angular_error_block_becomes_the_summary() {
        let v = verdict(
            &ng(),
            "\x1b[31mError:\x1b[0m src/app/x.ts:42:7 - error TS2304: nope",
        );
        match v {
            Some(Health::Failing { summary }) => {
                // Colour codes stripped, so the rail shows readable text.
                assert_eq!(summary, "Error: src/app/x.ts:42:7 - error TS2304: nope");
            }
            other => panic!("expected Failing, got {other:?}"),
        }
    }

    #[test]
    fn a_successful_build_line_reads_as_healthy() {
        assert_eq!(verdict(&ng(), "Build at: 2026-08-14"), Some(Health::Ok));
        assert_eq!(
            verdict(&ng(), "watching for file changes..."),
            Some(Health::Ok)
        );
    }

    #[test]
    fn a_recovery_line_after_a_failure_in_the_same_buffer_still_wins() {
        // The bug: scan stopped on the first failure line, so a `complete` sitting
        // right after `failed` in the buffer was never read and the build stayed
        // red until unrelated output happened to drain it.
        let mut buf = "Application bundle generation failed. (0.9 seconds)\n\
                       Application bundle generation complete. (1.1 seconds)\n"
            .to_string();
        assert_eq!(scan_lines(&ng(), &mut buf), Some(Health::Ok));
        assert!(buf.is_empty(), "every whole line should be drained");
    }

    #[test]
    fn the_first_error_of_a_failing_block_is_the_summary() {
        let mut buf = "✘ [ERROR] first\n✘ [ERROR] second\n".to_string();
        match scan_lines(&ng(), &mut buf) {
            Some(Health::Failing { summary }) => assert_eq!(summary, "✘ [ERROR] first"),
            other => panic!("expected Failing, got {other:?}"),
        }
    }

    #[test]
    fn an_incomplete_trailing_line_is_left_for_the_next_read() {
        let mut buf = "watching for file changes...\n✘ [ERROR] half".to_string();
        // The ok line is consumed; the errorless partial stays buffered.
        assert_eq!(scan_lines(&ng(), &mut buf), Some(Health::Ok));
        assert_eq!(buf, "✘ [ERROR] half");
    }

    #[test]
    fn the_esbuild_builder_recovers_and_fails_on_its_own_summary_lines() {
        // The success line carries none of the webpack markers, so before this was
        // recognised a fixed build stayed red in the rail forever.
        assert_eq!(
            verdict(&ng(), "Application bundle generation complete. (1.2 seconds)"),
            Some(Health::Ok)
        );
        match verdict(&ng(), "Application bundle generation failed. (0.9 seconds)") {
            Some(Health::Failing { .. }) => {}
            other => panic!("expected Failing, got {other:?}"),
        }
    }

    #[test]
    fn ordinary_output_says_nothing_either_way() {
        assert_eq!(verdict(&ng(), "compiling 412 files"), None);
        assert_eq!(verdict(&ng(), "   "), None);
    }

}
