//! A vendored skill's directory has to be the command string that types it.
//!
//! **An integration test because the two halves are in different crates now.**
//! `skills::VENDORED` is `orchd-repo`'s and the command constants are `orchd`'s,
//! and the assertion is about the pair — so it lives where both are visible
//! rather than in either one. `docs/crate-split.md` calls this the shape to
//! expect once per crate: a unit test that was really a pair test.

use orchd_repo::skills::VENDORED;

/// Each is typed as `/orchd:<command> <pr>` from the command string the run
/// carries, so the directory it is written to has to *be* that string. A mismatch
/// is `Unknown command` on the run's first turn and nothing before it.
#[test]
fn a_skill_is_named_after_the_command_that_types_it() {
    for command in [
        orchd::fix_pr::COMMAND,
        orchd::spawn::RESOLVE_RUN_COMMAND,
        orchd::triage::COMMAND,
        orchd::triage::TRIAGE_COMMAND,
        orchd::story::COMMAND,
        orchd::spawn::HANDLE_REVIEW_COMMAND,
    ] {
        assert!(
            VENDORED.iter().any(|(name, _)| *name == command),
            "no vendored skill directory called {command}"
        );
    }
}
