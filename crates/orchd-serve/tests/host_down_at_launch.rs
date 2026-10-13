//! A remembered checkout whose daemon will not start at launch.
//!
//! Its own file because `ORCHD_CONFIG_DIR` is process-global — see `host_file.rs`.
// A test binary, so a panic is how a failure is reported. `clippy.toml`'s
// `allow-*-in-tests` covers `#[test]` functions and `#[cfg(test)]` modules, and
// the helpers in an integration crate are neither.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;
use common::{daemon_on_path, scratch_root};

/// It keeps a row, down, and stays in the host file.
///
/// **The row is the whole of it.** `reopen` acts on a row, and the host file is
/// written from the rows — so a launch failure that left none took the checkout
/// out of `host.json` on the next add, close or reorder, and one bad boot forgot
/// a checkout somebody opened on purpose.
#[test]
fn a_checkout_that_will_not_start_keeps_a_down_row_and_its_place_in_the_file() {
    let root = scratch_root("downatlaunch");
    let cfg = root.join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::env::set_var("ORCHD_CONFIG_DIR", &cfg);
    daemon_on_path(env!("CARGO_BIN_EXE_orchd"));

    // A directory that is not a checkout: the real daemon refuses it at start,
    // which is the failure a moved or deleted project produces at launch.
    let broken = root.join("not-a-repo");
    std::fs::create_dir_all(&broken).unwrap();
    orchd_serve::host::remember_checkouts(std::slice::from_ref(&broken));

    let host = orchd_serve::host::Host::new("t".into(), 0, orchd::window::Chrome::None);
    host.open_remembered(&orchd_serve::host::remembered_checkouts());

    let rows = host.checkouts();
    assert_eq!(
        rows.len(),
        1,
        "the checkout that failed to start has no row"
    );
    assert_eq!(rows[0].path, broken.to_string_lossy());
    assert!(!rows[0].live, "a daemon that never started reads as live");

    // Any write of the list now — a reorder is the smallest — keeps it.
    host.order_checkouts(&[]);
    assert_eq!(
        orchd_serve::host::remembered_checkouts(),
        vec![broken.clone()],
        "the next write of the host file forgot the checkout"
    );

    let _ = std::fs::remove_dir_all(&root);
}
