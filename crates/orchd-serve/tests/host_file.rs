//! What the host opens when it has no file yet.
//!
//! Its own file rather than a test beside the others, because `ORCHD_CONFIG_DIR`
//! is process-global and cargo runs the tests in one binary in parallel: two tests
//! pointing that variable at two directories is a race whose loser reads the
//! wrong config. One binary per checkout-shaped fixture is the cheap way out.
// A test binary, so a panic is how a failure is reported. `clippy.toml`'s
// `allow-*-in-tests` covers `#[test]` functions and `#[cfg(test)]` modules, and
// the helpers in an integration crate are neither.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

/// A git checkout, with nothing in the config dir but what a test writes.
mod common;
use common::scratch_repo;

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

    let repo = scratch_repo(&root, "only", None);
    std::fs::write(
        cfg.join("config.json"),
        format!(r#"{{"main_checkout":{:?}}}"#, repo.to_string_lossy()),
    )
    .unwrap();
    assert_eq!(
        orchd_serve::host::remembered_checkouts(),
        vec![repo.clone()]
    );

    /* **A first write that is not about checkouts keeps the fallback.** Turning the
    usage ping off on an install with no host file wrote `checkouts: []`, which is
    an answer — and the next launch opened nothing instead of `config.json`'s
    checkout. */
    orchd_serve::host::set_usage_ping(false).unwrap();
    assert_eq!(
        orchd_serve::host::remembered_checkouts(),
        vec![repo.clone()],
        "turning the ping off forgot the configured checkout"
    );
    std::fs::remove_file(cfg.join("host.json")).unwrap();

    // And once the file exists it is the answer, config or no config.
    orchd_serve::host::remember_checkouts(&[]);
    assert!(
        orchd_serve::host::remembered_checkouts().is_empty(),
        "the fallback outlived the file"
    );

    /* **A hand-set key survives every write of the checkout list.** The writer
    rebuilds `HostFile` and serialises the whole struct, so a field it does not
    set would be written back as its default — which is how `see_through_window`
    would turn itself off on the next add or close, with nothing to see but a
    board that stopped being see-through. */
    let file = cfg.join("host.json");
    std::fs::write(&file, r#"{"checkouts":[],"see_through_window":true}"#).unwrap();
    assert!(
        orchd_serve::host::see_through_window(),
        "the key did not read back"
    );
    orchd_serve::host::remember_checkouts(std::slice::from_ref(&repo));
    assert!(
        orchd_serve::host::see_through_window(),
        "recording the checkouts dropped the key"
    );

    // And absent is off: a see-through window is a compositor feature, so nothing
    // may turn it on for a machine that never asked.
    std::fs::write(&file, r#"{"checkouts":[]}"#).unwrap();
    assert!(!orchd_serve::host::see_through_window());

    /* **The usage ping is the opposite: absent is on**, and turning it off from
    Settings must stick through the writes that follow. Those writes rebuild the
    file, so a dropped key would turn the ping back on behind the person who
    turned it off. */
    assert!(orchd_serve::host::usage_ping(), "absent must be on");
    orchd_serve::host::remember_checkouts(std::slice::from_ref(&repo));
    orchd_serve::host::set_usage_ping(false).unwrap();
    assert!(
        !orchd_serve::host::usage_ping(),
        "the checkbox did not write"
    );
    assert_eq!(
        orchd_serve::host::remembered_checkouts(),
        vec![repo.clone()],
        "turning the ping off dropped the checkouts"
    );
    orchd_serve::host::remember_checkouts(&[]);
    assert!(
        !orchd_serve::host::usage_ping(),
        "recording the checkouts turned the ping back on"
    );
    orchd_serve::host::set_usage_ping(true).unwrap();
    assert!(orchd_serve::host::usage_ping());

    /* **A file that will not parse is moved aside, not read past.** The writer
    starts from `read_host_file().unwrap_or_default()`, so a torn file read as
    "none" was overwritten by the next add or close — the list and every hand-set
    key gone, with the only copy replaced by defaults. */
    std::fs::write(&file, r#"{"checkouts":["/a"],"see_through"#).unwrap();
    assert_eq!(
        orchd_serve::host::remembered_checkouts(),
        vec![repo.clone()],
        "an unreadable host file falls back to the configured checkout"
    );
    orchd_serve::host::remember_checkouts(std::slice::from_ref(&repo));
    assert_eq!(
        std::fs::read_to_string(cfg.join("host.json.corrupt")).unwrap(),
        r#"{"checkouts":["/a"],"see_through"#,
        "the unreadable file was overwritten rather than kept"
    );

    let _ = std::fs::remove_dir_all(&root);
}
