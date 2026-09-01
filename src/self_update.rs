//! Upgrading the app to the release it just told you about.
//!
//! The nudge existed long before the button: `start_update_poller` compares the
//! newest tag against `CARGO_PKG_VERSION` and the bar said "Run mise up", which is
//! a instruction to go and do by hand what the app is perfectly able to do itself.
//! This is the other half, and it is deliberately the *same* half the agent bar
//! already has — [`crate::agent_update::run_upgrade`] runs both, so the bounded
//! exec, the captured tail and the reporting have one implementation rather than
//! two that drift.
//!
//! **Only a mise install can be upgraded from inside the app**, and that is not a
//! gap to close later. A `.deb` belongs to apt and wants a password; an AppImage
//! and a `.dmg` are files somebody downloaded, and replacing the binary a process
//! is executing is not something to do behind the user's back. Those installs keep
//! the link to the release, which is what they had.
//!
//! The upgrade also cannot take effect on its own: this process *is* the old build,
//! and mise installs beside it rather than over it. So a finished run says
//! "restart", and the restart is the same [`crate::window::WindowCmd::Restart`] the
//! agent bar offers — which is why `relaunch` resolves the `latest` symlink instead
//! of re-running the exact path it started from.

use std::path::Path;

/// Which mise tool provides the binary that is running, if mise provides it.
///
/// Asked of mise rather than derived from the path, because the two are not the
/// same string: an install of `github:kbarendrecht/orchestrator` lands in
/// `installs/github-kbarendrecht-orchestrator/…`, and `mise upgrade` wants the name
/// with the colon and the slash. `mise ls --json` is keyed by exactly the name mise
/// accepts and carries `install_path` beside it, so the answer is a prefix match
/// rather than a guess about how a backend spells its directory.
///
/// `None` for every install that is not mise's — a `.deb`, an AppImage, a `cargo
/// build` in a checkout — and that is the answer that hides the button.
pub fn providing_tool(main: &Path) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let out = std::process::Command::new("mise")
        .args(["ls", "--json"])
        .current_dir(main)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    tool_owning(&out.stdout, &exe)
}

/// Split from [`providing_tool`] so the shape mise emits can be tested without
/// mise, and so the prefix rule is stated once.
///
/// The longest matching `install_path` wins for the same reason
/// `workspace_for_path` takes the longest workspace: nothing stops one tool's
/// install directory from sitting inside another's, and the specific one is the
/// owner.
fn tool_owning(stdout: &[u8], exe: &Path) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let mut best: Option<(usize, String)> = None;
    for (tool, versions) in v.as_object()? {
        for entry in versions.as_array().into_iter().flatten() {
            let Some(at) = entry.get("install_path").and_then(|p| p.as_str()) else {
                continue;
            };
            if !exe.starts_with(at) {
                continue;
            }
            if best.as_ref().is_none_or(|(len, _)| at.len() > *len) {
                best = Some((at.len(), tool.clone()));
            }
        }
    }
    best.map(|(_, tool)| tool)
}

/// `…/installs/<tool>/<version>/<file>` → `…/installs/<tool>/latest/<file>`.
///
/// Only when that path really exists, so a layout this does not understand keeps
/// the resolved path it came with. Pure, and tested, because the fault it prevents
/// is invisible until an upgrade weeks later.
pub fn stable_exe(exe: &std::path::Path) -> std::path::PathBuf {
    let parts: Vec<_> = exe.components().collect();
    // <installs>/<tool>/<version>/<file>: the version is two components from the
    // end, and `installs` two before that.
    let Some(version_at) = parts.len().checked_sub(2) else {
        return exe.to_path_buf();
    };
    let installs_at = match version_at.checked_sub(2) {
        Some(i) => i,
        None => return exe.to_path_buf(),
    };
    if parts[installs_at].as_os_str() != std::ffi::OsStr::new("installs") {
        return exe.to_path_buf();
    }
    let mut latest = std::path::PathBuf::new();
    for (n, c) in parts.iter().enumerate() {
        if n == version_at {
            latest.push("latest");
        } else {
            latest.push(c.as_os_str());
        }
    }
    if latest.exists() {
        latest
    } else {
        exe.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fault this prevents costs an upgrade, not a launch: mise installs each
    /// version in its own directory, so a path written into the launcher entry or
    /// into the push guard's hook is dead the moment `mise up` removes it. Seen
    /// twice — a `.desktop` file naming a version that was gone, and a `PreToolUse`
    /// hook whose `orch` had been replaced, which made the guard fail *open* and
    /// print four errors into a session.
    #[test]
    fn a_mise_install_path_resolves_to_the_latest_symlink() {
        let d = std::env::temp_dir().join(format!("orchd-stable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let versioned = d.join("installs/orchestrator/2026.9.0");
        let latest = d.join("installs/orchestrator/latest");
        std::fs::create_dir_all(&versioned).unwrap();
        std::fs::create_dir_all(&latest).unwrap();
        std::fs::write(versioned.join("orchestrator-desktop"), "x").unwrap();
        std::fs::write(latest.join("orchestrator-desktop"), "x").unwrap();

        assert_eq!(
            stable_exe(&versioned.join("orchestrator-desktop")),
            latest.join("orchestrator-desktop")
        );
    }

    #[test]
    fn a_path_with_no_latest_beside_it_is_left_alone() {
        let d = std::env::temp_dir().join(format!("orchd-nolatest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let versioned = d.join("installs/orchestrator/2026.9.0");
        std::fs::create_dir_all(&versioned).unwrap();
        let exe = versioned.join("orchestrator-desktop");
        std::fs::write(&exe, "x").unwrap();
        assert_eq!(stable_exe(&exe), exe, "no `latest` means nothing to prefer");

        // And anything that is not a mise layout, including paths short enough to
        // underflow the component arithmetic.
        for p in ["/usr/bin/orch", "/x", "/"] {
            assert_eq!(stable_exe(Path::new(p)), Path::new(p));
        }
    }

    /// The real shape, trimmed: mise keys by the name it accepts, which for a
    /// backend install is not what the directory is called.
    const LS: &str = r#"{
      "node": [
        {"version":"22.1.0","install_path":"/home/me/.local/share/mise/installs/node/22.1.0","installed":true,"active":true}
      ],
      "github:kbarendrecht/orchestrator": [
        {"version":"2026.9.2","install_path":"/home/me/.local/share/mise/installs/github-kbarendrecht-orchestrator/2026.9.2","installed":true,"active":true}
      ]
    }"#;

    #[test]
    fn the_tool_is_the_name_mise_accepts_not_the_directory_it_used() {
        let exe = Path::new(
            "/home/me/.local/share/mise/installs/github-kbarendrecht-orchestrator/2026.9.2/orchestrator-desktop",
        );
        assert_eq!(
            tool_owning(LS.as_bytes(), exe).as_deref(),
            Some("github:kbarendrecht/orchestrator"),
            "`mise upgrade` wants the colon-and-slash name, not the directory"
        );
    }

    #[test]
    fn a_binary_mise_did_not_install_has_no_tool() {
        for exe in [
            "/usr/bin/orchestrator-desktop",
            "/home/me/src/orchestrator/target/release/orchestrator-desktop",
            "/tmp/.mount_Orches/usr/bin/orchestrator-desktop",
        ] {
            assert_eq!(
                tool_owning(LS.as_bytes(), Path::new(exe)),
                None,
                "{exe} is not mise's, so there is no button to offer"
            );
        }
    }

    /// Nested install directories are possible, and the owner is the specific one.
    #[test]
    fn the_longest_matching_install_path_wins() {
        let ls = r#"{
          "outer": [{"install_path":"/i/tools","installed":true}],
          "inner": [{"install_path":"/i/tools/orchestrator/1.0","installed":true}]
        }"#;
        assert_eq!(
            tool_owning(ls.as_bytes(), Path::new("/i/tools/orchestrator/1.0/orchestrator-desktop")).as_deref(),
            Some("inner")
        );
    }

    #[test]
    fn nonsense_from_mise_is_no_tool_rather_than_a_panic() {
        assert_eq!(tool_owning(b"not json", Path::new("/x")), None);
        assert_eq!(tool_owning(b"[]", Path::new("/x")), None);
        assert_eq!(tool_owning(b"{\"t\":[{}]}", Path::new("/x")), None);
    }
}
