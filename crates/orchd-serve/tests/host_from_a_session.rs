//! An app started from one of its own session panes.
//!
//! Its own file because `ORCHD_CONFIG_DIR` is process-global — see `host_file.rs`.
// A test binary, so a panic is how a failure is reported. `clippy.toml`'s
// `allow-*-in-tests` covers `#[test]` functions and `#[cfg(test)]` modules, and
// the helpers in an integration crate are neither.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::scratch_root;

/// A session's `ORCHD_CONFIG_DIR` is its checkout's directory, and the host
/// takes the root above it, so the restart reads the host file that lists every
/// project rather than one listing only the checkout it was started from.
#[test]
fn a_host_started_from_a_session_reads_the_root_host_file() {
    let root = scratch_root("fromasession");
    let child = root.join("checkouts").join("app-1a2b");
    std::fs::create_dir_all(&child).unwrap();
    std::env::set_var("ORCHD_CONFIG_DIR", &child);

    orchd_serve::host::take_the_hosts_config_dir();
    assert_eq!(
        std::env::var_os("ORCHD_CONFIG_DIR").unwrap(),
        root.as_os_str()
    );

    orchd_serve::host::remember_checkouts(&[root.join("app")]);
    assert!(
        root.join("host.json").exists(),
        "the host file is the root's"
    );
    assert!(
        !child.join("host.json").exists(),
        "a checkout's dir got a host file"
    );

    // A root set on purpose stays where it was put.
    orchd_serve::host::take_the_hosts_config_dir();
    assert_eq!(
        std::env::var_os("ORCHD_CONFIG_DIR").unwrap(),
        root.as_os_str()
    );
}
