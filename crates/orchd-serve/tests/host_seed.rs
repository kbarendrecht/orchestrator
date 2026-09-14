//! What a checkout inherits the first time it gets a directory of its own.
//!
//! **This is issue #17, pinned.** `2a976739` gave each checkout its own
//! `checkouts/<leaf>-<hash>/` and seeded it with one `fs::copy` of `config.json`.
//! Everything else the daemon keeps beside that file stayed at the top level, so an
//! update to v2026.9.14 opened the board with an empty rail: 28 sessions were still
//! on disk, in a `sessions.json` the new daemon no longer read, while the one it did
//! read held `[]`. Nothing was deleted and nothing said so — which is the worst way
//! for a migration to fail, because a user who rebuilds the rail by hand pays for a
//! copy that never happened.
//!
//! So the assertion that matters is not "sessions.json arrives". It is the **unknown
//! file**: the seed is a deny list now, and a state file nobody has written yet has
//! to be carried without anyone remembering to add it here. An allow list passes a
//! test that names the files it already knows, which is exactly how #17 shipped
//! green.
//!
//! Its own binary because `ORCHD_CONFIG_DIR` is process-global — cargo gives each
//! `tests/*.rs` its own process, so nothing here races the other host tests over
//! where every durable thing lands. Inside this binary the cases still share it, and
//! cargo runs them on threads, so [`CONFIG_DIR`] serialises the three that set it.
//! That is measured rather than assumed: written without it, the seed test read the
//! repair test's directory and failed on the assertion it exists for.

// A test binary, so a panic is how a failure is reported. `clippy.toml`'s
// `allow-*-in-tests` covers `#[test]` functions and `#[cfg(test)]` modules, and the
// helpers in an integration crate are neither.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::sync::Mutex;

mod common;
use common::{scratch_repo, scratch_root};

/// Held for the length of any test that sets `ORCHD_CONFIG_DIR`.
///
/// Poisoning is ignored on purpose: a failed test has already been reported, and
/// taking the rest of the binary down with it turns one red line into four.
static CONFIG_DIR: Mutex<()> = Mutex::new(());

fn write(at: &Path, rel: &str, body: &str) {
    let p = at.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, body).unwrap();
}

fn read(at: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(at.join(rel)).ok()
}

#[test]
fn a_new_checkout_dir_inherits_every_state_file_but_the_hosts_own() {
    let _serial = CONFIG_DIR.lock().unwrap_or_else(|e| e.into_inner());
    let root = scratch_root("seed");
    let checkout = scratch_repo(&root, "mono", None);
    let old = root.join("cfg");
    std::fs::create_dir_all(&old).unwrap();
    std::env::set_var("ORCHD_CONFIG_DIR", &old);

    // The old single config dir, as v2026.9.12 left one. `config.json` has to name
    // this checkout or nothing is seeded at all — that guard is what stops a second
    // checkout being handed the first one's tree, and its sessions with it.
    write(
        &old,
        "config.json",
        &serde_json::json!({ "main_checkout": checkout }).to_string(),
    );
    write(&old, "sessions.json", r#"[{"id":"kept"}]"#);
    write(&old, "automation.json", r#"{"35317":"running"}"#);
    write(&old, "stories.json", r#"{"cached":true}"#);
    // A directory, not a file: `fs::copy` alone cannot carry this one, and the
    // resume path reads it.
    write(&old, "transcripts/abc.jsonl", "{}\n");
    /* **The assertion this test exists for.** Nothing knows this file; it stands in
    for whatever the daemon learns to keep next. A seed that lists what it copies
    leaves it behind, silently, which is the fault itself rather than a variant
    of it. */
    write(
        &old,
        "something-nobody-has-written-yet.json",
        r#"{"future":true}"#,
    );
    write(&old, "a-future-directory/inside.txt", "kept");

    // And the ones a checkout must never inherit, each for its own reason.
    write(&old, "host.json", r#"{"checkouts":["/elsewhere"]}"#);
    write(&old, "instance.pid", "12345");
    write(&old, "orchd.log", "an old run");
    write(&old, "orchd.log.1", "an older run");
    write(&old, "checkouts/someone-else/config.json", "{}");
    /* Everything the sweep may delete because the next start rebuilds it — `plugin`,
    `hooks.json` (which names a *port*, and a copied one points a checkout's hooks
    at another checkout's daemon) and `window.json`. Taken from `DERIVED` rather
    than listed again here, so a name added there is denied by this test too and
    the two lists cannot drift apart. `checkouts/` above covers the directory
    case. */
    for name in orchd_serve::host::DERIVED {
        write(&old, name, "derived");
    }

    let dir = orchd_serve::host::ensure_checkout_dir(&checkout).unwrap();

    assert_eq!(
        read(&dir, "sessions.json").as_deref(),
        Some(r#"[{"id":"kept"}]"#),
        "the rail's 28 rows are the whole of #17"
    );
    assert_eq!(
        read(&dir, "automation.json").as_deref(),
        Some(r#"{"35317":"running"}"#)
    );
    assert_eq!(
        read(&dir, "stories.json").as_deref(),
        Some(r#"{"cached":true}"#)
    );
    assert_eq!(
        read(&dir, "transcripts/abc.jsonl").as_deref(),
        Some("{}\n"),
        "a directory has to be walked, not copied"
    );
    assert!(
        read(&dir, "config.json").is_some(),
        "the one file that always worked"
    );

    assert_eq!(
        read(&dir, "something-nobody-has-written-yet.json").as_deref(),
        Some(r#"{"future":true}"#),
        "a file the seed has never heard of must still be carried — an allow list \
         is what shipped #17"
    );
    assert_eq!(
        read(&dir, "a-future-directory/inside.txt").as_deref(),
        Some("kept")
    );

    for denied in ["host.json", "instance.pid", "orchd.log", "orchd.log.1"] {
        assert!(
            read(&dir, denied).is_none(),
            "{denied} is not the checkout's to inherit"
        );
    }
    for name in orchd_serve::host::DERIVED {
        assert!(
            !dir.join(name).exists(),
            "{name} is derived: the next start rebuilds it, so a copy is stale"
        );
    }
    // Recursion: `checkouts/` is the parent of this very directory.
    assert!(
        !dir.join("checkouts").exists(),
        "copying checkouts/ copies the destination"
    );

    // A second call is not a second seed, so a daemon that writes `[]` over a state
    // file is not re-seeded back to the old one on its next start.
    std::fs::write(dir.join("sessions.json"), "[]").unwrap();
    let again = orchd_serve::host::ensure_checkout_dir(&checkout).unwrap();
    assert_eq!(again, dir);
    assert_eq!(
        read(&dir, "sessions.json").as_deref(),
        Some("[]"),
        "the seed is a one-shot, marked in the directory it seeded"
    );
}

#[test]
fn a_config_naming_another_checkout_seeds_nothing() {
    let _serial = CONFIG_DIR.lock().unwrap_or_else(|e| e.into_inner());
    let root = scratch_root("seed-other");
    let mine = scratch_repo(&root, "mine", None);
    let theirs = scratch_repo(&root, "theirs", None);
    let old = root.join("cfg");
    std::fs::create_dir_all(&old).unwrap();
    std::env::set_var("ORCHD_CONFIG_DIR", &old);

    // The old config belongs to `theirs`, so its sessions do too. Seeding `mine`
    // from it would hand this daemon another checkout's tree *and* another
    // checkout's conversations — which is the reason the guard is on the copy
    // rather than only on `config.json`.
    write(
        &old,
        "config.json",
        &serde_json::json!({ "main_checkout": theirs }).to_string(),
    );
    write(&old, "sessions.json", r#"[{"id":"theirs"}]"#);

    let dir = orchd_serve::host::ensure_checkout_dir(&mine).unwrap();
    assert!(
        read(&dir, "sessions.json").is_none(),
        "another checkout's rail"
    );
    assert!(
        read(&dir, "config.json").is_none(),
        "another checkout's tree"
    );
}

/// The two defaults of [`HostFile`] have to be the same value.
///
/// `derive(Default)` gave `checkout_retention_days` zero while serde's default gave
/// 60, and `remember_checkouts` writes `unwrap_or_default()` — so the first
/// `host.json` a machine wrote pinned retention to 0 and the documented number never
/// applied. Asserted whole rather than field by field: a new field that reintroduces
/// the gap fails this without anyone having to remember to extend it.
#[test]
fn the_two_defaults_agree() {
    let from_serde: orchd_serve::host::HostFile = serde_json::from_str("{}").unwrap();
    assert_eq!(from_serde, orchd_serve::host::HostFile::default());
    assert_eq!(
        from_serde.checkout_retention_days, 60,
        "the documented default"
    );
}

/// A machine that already updated to v2026.9.14 is repaired, not written off.
///
/// **The half of #17 a fix for fresh installs does not reach.** Those machines have
/// a checkout directory that exists, holding the one `config.json` the old seed
/// copied and a `sessions.json` the new daemon wrote as `[]`. Keying the one-shot on
/// "the directory exists" put every one of them permanently past their only chance
/// to be seeded, so the marker is what separates *seeded* from *present*.
///
/// What it may overwrite is the narrow part: absent, or an empty container. `[]` is
/// what the empty rail wrote and it has nothing to lose. A file with content in it
/// is left alone and named in a warning, because choosing between two versions of
/// somebody's work is not a migration's to do.
#[test]
fn a_checkout_dir_left_half_seeded_is_filled_in_without_losing_anything() {
    let _serial = CONFIG_DIR.lock().unwrap_or_else(|e| e.into_inner());
    let root = scratch_root("seed-repair");
    let checkout = scratch_repo(&root, "mono", None);
    let old = root.join("cfg");
    std::fs::create_dir_all(&old).unwrap();
    std::env::set_var("ORCHD_CONFIG_DIR", &old);

    write(
        &old,
        "config.json",
        &serde_json::json!({ "main_checkout": checkout }).to_string(),
    );
    write(&old, "sessions.json", r#"[{"id":"the-28-rows"}]"#);
    write(&old, "automation.json", r#"{"35317":"running"}"#);
    write(&old, "stories.json", r#"{"old":true}"#);

    // The state v2026.9.14 left behind: the directory is there, `config.json` came
    // across, the rail was rewritten empty, and something real has been written
    // since.
    let dir = orchd_serve::host::checkout_dir(&checkout).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    write(
        &dir,
        "config.json",
        &serde_json::json!({ "main_checkout": checkout }).to_string(),
    );
    write(&dir, "sessions.json", "[]");
    write(&dir, "stories.json", r#"{"filed":"since the update"}"#);

    orchd_serve::host::ensure_checkout_dir(&checkout).unwrap();

    assert_eq!(
        read(&dir, "sessions.json").as_deref(),
        Some(r#"[{"id":"the-28-rows"}]"#),
        "an empty rail has nothing to lose, so the old one comes back"
    );
    assert_eq!(
        read(&dir, "automation.json").as_deref(),
        Some(r#"{"35317":"running"}"#),
        "absent, so it is simply carried"
    );
    assert_eq!(
        read(&dir, "stories.json").as_deref(),
        Some(r#"{"filed":"since the update"}"#),
        "written since the update, so the daemon must not choose for you"
    );

    // And it does not run again: the rail is the user's from here.
    std::fs::write(dir.join("sessions.json"), "[]").unwrap();
    orchd_serve::host::ensure_checkout_dir(&checkout).unwrap();
    assert_eq!(
        read(&dir, "sessions.json").as_deref(),
        Some("[]"),
        "a repaired checkout is seeded, and a seeded one is never seeded again"
    );
}
