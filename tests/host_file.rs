//! What the host opens when it has no file yet.
//!
//! Its own file rather than a test beside the others, because `ORCHD_CONFIG_DIR`
//! is process-global and cargo runs the tests in one binary in parallel: two tests
//! pointing that variable at two directories is a race whose loser reads the
//! wrong config. One binary per checkout-shaped fixture is the cheap way out.

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
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "test@test"]);
    git(&["config", "user.name", "test"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    dir
}

/// With no host file, the host opens what `config.json` names.
///
/// **Every install that predates the file is this case**, and reading it as an
/// empty list would show a first-run page to somebody who configured a project
/// months ago.
#[test]
fn a_missing_host_file_falls_back_to_the_configured_checkout() {
    let root = std::env::temp_dir().join(format!("orchd-hostfile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let cfg = root.join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::env::set_var("ORCHD_CONFIG_DIR", &cfg);

    let repo = scratch_repo(&root, "only");
    std::fs::write(
        cfg.join("config.json"),
        format!(r#"{{"main_checkout":{:?}}}"#, repo.to_string_lossy()),
    )
    .unwrap();
    assert_eq!(orchd::host::remembered_checkouts(), vec![repo.clone()]);

    // And once the file exists it is the answer, config or no config.
    orchd::host::remember_checkouts(&[]);
    assert!(orchd::host::remembered_checkouts().is_empty(), "the fallback outlived the file");

    /* **A hand-set key survives every write of the checkout list.** The writer
       rebuilds `HostFile` and serialises the whole struct, so a field it does not
       set would be written back as its default — which is how `see_through_window`
       would turn itself off on the next add or close, with nothing to see but a
       board that stopped being see-through. */
    let file = cfg.join("host.json");
    std::fs::write(&file, r#"{"checkouts":[],"see_through_window":true}"#).unwrap();
    assert!(orchd::host::see_through_window(), "the key did not read back");
    orchd::host::remember_checkouts(std::slice::from_ref(&repo));
    assert!(orchd::host::see_through_window(), "recording the checkouts dropped the key");

    // And absent is off: a see-through window is a compositor feature, so nothing
    // may turn it on for a machine that never asked.
    std::fs::write(&file, r#"{"checkouts":[]}"#).unwrap();
    assert!(!orchd::host::see_through_window());

    let _ = std::fs::remove_dir_all(&root);
}
