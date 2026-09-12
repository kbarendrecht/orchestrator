//! The `checkouts/` sweep: what it may delete, and what it may not.
//!
//! Its own binary for the reason `host_file.rs` gives — `ORCHD_CONFIG_DIR` is
//! process-global, and cargo runs the tests in one binary in parallel, so one
//! fixture that owns that variable is one binary.
// A test binary, so a panic is how a failure is reported. `clippy.toml`'s
// `allow-*-in-tests` covers `#[test]` functions and `#[cfg(test)]` modules, and
// the helpers in an integration crate are neither.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};

/// A git checkout, with nothing in the config dir but what a test writes.
fn scratch_repo(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    // Canonical, because `Config::parse` resolves `main_checkout` and comparing an
    // unresolved path against a resolved one silently matches nothing.
    let dir = dir.canonicalize().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("git ran");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "test@test"]);
    git(&["config", "user.name", "test"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    dir
}

/// The sweep drops what a daemon rebuilds, and never a conversation.
///
/// **The distinction is the whole rule.** `transcripts/` is the only remaining
/// copy of a conversation once a worktree is gone, and a session record survives
/// because that copy does — so a sweep that took the directory would undo the
/// decision `close` makes on purpose, which is to leave records alone.
#[test]
fn the_sweep_takes_derived_state_and_leaves_conversations() {
    let root = std::env::temp_dir().join(format!("orchd-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let cfg = root.join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::env::set_var("ORCHD_CONFIG_DIR", &cfg);

    // Three checkout directories: one open, one recent, one long untouched.
    let open = scratch_repo(&root, "open");
    let recent = scratch_repo(&root, "recent");
    let old = scratch_repo(&root, "old");
    let dirs: Vec<PathBuf> = [&open, &recent, &old]
        .iter()
        .map(|c| {
            let dir = orchd_serve::host::checkout_dir(c).unwrap();
            std::fs::create_dir_all(dir.join("plugin/skills")).unwrap();
            std::fs::write(dir.join("plugin/skills/SKILL.md"), "x").unwrap();
            std::fs::write(dir.join("hooks.json"), "{}").unwrap();
            std::fs::write(dir.join("window.json"), "{}").unwrap();
            std::fs::create_dir_all(dir.join("transcripts")).unwrap();
            std::fs::write(dir.join("transcripts/a.jsonl"), "{}").unwrap();
            std::fs::write(dir.join("sessions.json"), "[]").unwrap();
            dir
        })
        .collect();

    // Age the third one past the retention. `filetime` is not a dependency here,
    // so the retention is moved instead — one day, and the files are newer than
    // that, so only a backdated directory can qualify.
    orchd_serve::host::remember_checkouts(std::slice::from_ref(&open));
    let file = cfg.join("host.json");
    let mut host_file: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    host_file["checkout_retention_days"] = serde_json::json!(0);
    std::fs::write(&file, host_file.to_string()).unwrap();
    // Zero is off, so nothing goes at all — the same escape hatch worktrees have.
    orchd_serve::host::sweep_checkout_dirs(std::slice::from_ref(&open));
    for dir in &dirs {
        assert!(
            dir.join("hooks.json").exists(),
            "a retention of 0 swept anyway"
        );
    }

    // A retention nothing can be older than: every directory was written a moment
    // ago, so the sweep must take nothing.
    host_file["checkout_retention_days"] = serde_json::json!(60);
    std::fs::write(&file, host_file.to_string()).unwrap();
    orchd_serve::host::sweep_checkout_dirs(std::slice::from_ref(&open));
    for dir in &dirs {
        assert!(
            dir.join("hooks.json").exists(),
            "the sweep took a directory written today"
        );
    }

    // Backdate the third, and only the third goes — and only its derived half.
    let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 24 * 60 * 60);
    set_mtimes(&dirs[2], stale);
    orchd_serve::host::sweep_checkout_dirs(std::slice::from_ref(&open));
    assert!(
        dirs[0].join("hooks.json").exists(),
        "an open checkout was swept"
    );
    assert!(
        dirs[1].join("hooks.json").exists(),
        "a recent checkout was swept"
    );
    assert!(
        !dirs[2].join("hooks.json").exists(),
        "the stale checkout kept its hook settings"
    );
    assert!(
        !dirs[2].join("plugin").exists(),
        "the stale checkout kept its skills copy"
    );
    assert!(!dirs[2].join("window.json").exists());
    assert!(
        dirs[2].join("transcripts/a.jsonl").exists(),
        "the sweep deleted the only copy of a conversation",
    );
    assert!(
        dirs[2].join("sessions.json").exists(),
        "the sweep deleted session records"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Backdate a directory and everything one level inside it.
///
/// **`std::fs`, not `touch -d`.** BSD `touch` has no `-d`: its `-t` takes
/// `[[CC]YY]MMDDhhmm[.SS]`, so the GNU spelling exits with "out of range or
/// illegal time specification" and this test failed on the macos-14 runner alone —
/// the coreutils trap `CLAUDE.md` names, in a test rather than in the daemon.
/// `File::set_times` is `futimens`, which is POSIX and works on a directory fd.
fn set_mtimes(dir: &Path, when: std::time::SystemTime) {
    let stamp = std::fs::FileTimes::new()
        .set_accessed(when)
        .set_modified(when);
    let touch = |path: &Path| {
        let f = std::fs::File::open(path)
            .unwrap_or_else(|e| panic!("opening {} to backdate it: {e}", path.display()));
        f.set_times(stamp)
            .unwrap_or_else(|e| panic!("backdating {}: {e}", path.display()));
    };
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        touch(&entry.path());
    }
    touch(dir);
}
