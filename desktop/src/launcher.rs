//! The launcher entry the app writes for itself: a `.desktop` file on Linux,
//! an `.app` bundle on macOS.
//!
//! Split out of `main.rs` for readability only. `main` calls
//! [`refresh_launcher_entry`] on every launch and [`install_desktop_entry`] for
//! the `--install-desktop-entry` one-shot; `relaunch` and `request_restart`
//! read [`launcher_target`].

use anyhow::{Context, Result};

/// The reverse-DNS id the launcher entry and the `.app` bundle are keyed on.
///
/// The same id the `.deb`, the AppImage and the `.dmg` carry, so an entry written
/// here is *replaced* by a later package install rather than listed beside it.
const APP_ID: &str = "dev.orchd.orchestrator";
const APP_NAME: &str = "Orchestrator";

/// The path a launcher entry should point at, which is not always `current_exe`.
///
/// `current_exe` resolves symlinks, so a mise install hands back the
/// version-pinned path (`…/installs/orchestrator/2026.9.0/orchestrator-desktop`)
/// rather than the `latest` symlink beside it. Writing that into an entry breaks
/// the launcher at the next `mise up`, and it breaks in the one way the app cannot
/// repair, because it never starts to notice.
pub(crate) fn launcher_target() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("finding this executable")?;
    // The daemon's, because the push guard's hook has the same problem with the
    // same answer, and two copies of this rule is how one of them would keep the
    // version-pinned path.
    Ok(orchd::update::stable_exe(&exe))
}

/// Write `bytes` unless the file already holds exactly that.
///
/// Every launch calls the refresh below, so this is what keeps it from rewriting an
/// entry that is already right: a changed path is the only thing that writes.
fn write_if_changed(path: &std::path::Path, bytes: &[u8]) -> Result<bool> {
    if std::fs::read(path).is_ok_and(|cur| cur == bytes) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// Is this binary a build output rather than something installed?
///
/// A path predicate so it can be tested; the caller adds `debug_assertions`,
/// which catches the same case for a debug build wherever it sits.
fn in_build_tree(exe: &std::path::Path) -> bool {
    let path = exe.to_string_lossy();
    path.contains("/target/debug/") || path.contains("/target/release/")
}

/// Does this install carry a launcher entry of its own?
///
/// The `.deb` writes one to `/usr/share/applications`, the AppImage is its own,
/// and a `.dmg` install *is* a bundle. Writing a second entry for any of them is
/// how a launcher ends up listing the app twice, so the refresh does nothing there
/// and the flag stays available for the person who wants it anyway.
fn packaged() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return true; // Cannot tell: do nothing rather than guess wrong.
    };
    if std::env::var_os("APPIMAGE").is_some() {
        return true;
    }
    let path = exe.to_string_lossy();
    path.contains("/Contents/MacOS/") || path.starts_with("/usr/")
}

/// Write the launcher entry when it is missing or points somewhere else.
///
/// Runs on every launch, and writes on almost none of them. It exists because the
/// install methods that ship no entry — `mise`, `ubi`, a tarball — are also the
/// ones whose path *moves*: mise installs each version to its own directory, so an
/// entry written once is stale after one upgrade. Best effort throughout: a
/// launcher entry is a convenience, and no failure here is worth keeping the window
/// shut over.
pub(crate) fn refresh_launcher_entry() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // A build tree is not an install. Without this, `cargo run -p
    // orchestrator-desktop` writes an entry pointing at `target/debug` — and
    // because every entry shares one id, that entry *shadows* the real install's
    // on the same machine, which is a worse failure than having no entry at all.
    // `--install-desktop-entry` still obeys you here; only the automatic write
    // declines.
    if cfg!(debug_assertions) || in_build_tree(&exe) || packaged() {
        return;
    }
    match install_desktop_entry() {
        Ok(Some(path)) => tracing::info!("wrote the launcher entry at {}", path.display()),
        Ok(None) => {}
        Err(e) => tracing::warn!("could not write the launcher entry: {e:#}"),
    }
}

/// Write a launcher entry for the binary that is running.
///
/// `Ok(None)` when one was already there and current, which is the usual answer.
///
/// Only for the installs that ship no packaging of their own — `mise`, `ubi`, a
/// tarball unpacked by hand. It points at [`launcher_target`] rather than at the
/// running binary, so a mise upgrade does not leave a dead entry behind.
#[cfg(target_os = "linux")]
pub(crate) fn install_desktop_entry() -> Result<Option<std::path::PathBuf>> {
    let exe = launcher_target()?;
    let home = std::env::var("HOME").context("HOME is not set")?;
    // An empty `XDG_DATA_HOME` means unset, per the spec — and `env::var` hands it
    // back as `Ok("")`, which is how a test run wrote `applications/…` into the
    // working directory instead of under HOME.
    let data = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("{home}/.local/share"));

    for (px, bytes) in [
        (32u32, &include_bytes!("../icons/32x32.png")[..]),
        (128, &include_bytes!("../icons/128x128.png")[..]),
        (256, &include_bytes!("../icons/128x128@2x.png")[..]),
    ] {
        let dir = std::path::PathBuf::from(&data)
            .join("icons/hicolor")
            .join(format!("{px}x{px}"))
            .join("apps");
        write_if_changed(&dir.join(format!("{APP_ID}.png")), bytes)?;
    }

    let apps = std::path::PathBuf::from(&data).join("applications");
    let file = apps.join(format!("{APP_ID}.desktop"));
    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={APP_NAME}\n\
         Comment=Session board for parallel Claude work\n\
         Exec={exe}\n\
         Icon={APP_ID}\n\
         Terminal=false\n\
         Categories=Development;\n\
         Keywords=claude;sessions;orchestrator;\n",
        exe = exe.display()
    );
    if !write_if_changed(&file, entry.as_bytes())? {
        return Ok(None);
    }

    // Best effort: most desktops notice the file on their own, and a missing
    // `update-desktop-database` is not a failure worth reporting.
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&apps)
        .status();
    Ok(Some(file))
}

/// The same, as the only thing macOS will show in Finder: an `.app` bundle.
///
/// `~/Applications` rather than `/Applications`, because it needs no password and
/// Spotlight and Finder index it the same. A bundle built locally also carries no
/// quarantine attribute, so it opens on the first double-click — unlike the
/// unsigned `.dmg`, which needs the right-click dance once.
#[cfg(target_os = "macos")]
pub(crate) fn install_desktop_entry() -> Result<Option<std::path::PathBuf>> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let apps = std::path::PathBuf::from(home).join("Applications");
    let (bundle, wrote) = write_app_bundle(&apps, &launcher_target()?)?;
    if !wrote {
        return Ok(None);
    }
    // Best effort, and the same shape as `update-desktop-database`: LaunchServices
    // notices `~/Applications` on its own, eventually, and this is what makes it
    // now.
    let _ = std::process::Command::new(
        "/System/Library/Frameworks/CoreServices.framework/Frameworks/\
         LaunchServices.framework/Support/lsregister",
    )
    .arg("-f")
    .arg(&bundle)
    .status();
    Ok(Some(bundle))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn install_desktop_entry() -> Result<Option<std::path::PathBuf>> {
    anyhow::bail!("a launcher entry is written on Linux and macOS only")
}

/// Write `<apps>/Orchestrator.app` around a binary that lives somewhere else.
///
/// Says whether anything changed, so the launch-time refresh can stay quiet.
///
/// Compiled everywhere, and tested on Linux, because it is ordinary file writing
/// and the machine that can run it is the one machine this repo cannot compile
/// for locally (`objc2-exception-helper` needs a real macOS SDK). Keeping the
/// logic platform-free is what lets it be tested at all.
#[cfg(any(target_os = "macos", test))]
fn write_app_bundle(
    apps: &std::path::Path,
    exec: &std::path::Path,
) -> Result<(std::path::PathBuf, bool)> {
    let bundle = apps.join(format!("{APP_NAME}.app"));
    let contents = bundle.join("Contents");
    let mut wrote = write_if_changed(&contents.join("Info.plist"), info_plist().as_bytes())?;
    wrote |= write_if_changed(&contents.join("Resources/icon.icns"), &icns())?;

    // A script rather than a copy of the binary or a symlink to it. A copy is
    // stale at the next upgrade and doubles the disk cost; a symlink out of a
    // bundle is the shape code-signing rejects, and this bundle is meant to
    // survive being signed one day. The binary stays where its installer put it,
    // so `mise up` still owns it.
    let launcher = contents.join("MacOS").join(APP_NAME);
    let script = format!(
        "#!/bin/sh\n\
         # Written by `orchestrator-desktop --install-desktop-entry`, and rewritten\n\
         # whenever the binary it points at moves.\n\
         exec {} \"$@\"\n",
        orchd::hooks::sh_quote(&exec.to_string_lossy())
    );
    wrote |= write_if_changed(&launcher, script.as_bytes())?;
    // Set every time, not only on a write: a bundle restored from a backup that
    // dropped the bit is a bundle that will not launch, and the fix costs nothing.
    #[cfg(unix)]
    std::fs::set_permissions(
        &launcher,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .with_context(|| format!("making {} executable", launcher.display()))?;
    Ok((bundle, wrote))
}

/// The bundle's `Info.plist`.
///
/// `CFBundleIdentifier` matches the `.dmg`'s, which is deliberate: install the
/// package later and LaunchServices treats it as the same application rather than
/// showing two Orchestrators.
#[cfg(any(target_os = "macos", test))]
fn info_plist() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>{APP_NAME}</string>
  <key>CFBundleDisplayName</key><string>{APP_NAME}</string>
  <key>CFBundleExecutable</key><string>{APP_NAME}</string>
  <key>CFBundleIdentifier</key><string>{APP_ID}</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>10.15</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
"#
    )
}

/// The icon set as one `.icns`, built from the PNGs the Linux entry already uses.
///
/// Assembled here rather than committed as a fourth icon file, and rather than
/// shelling `iconutil`, which exists only on the platform this cannot be built on.
/// The format is a header and a run of typed chunks, and every type below takes a
/// PNG payload: `ic11` is 16pt at 2x, `ic07` 128, `ic08` 256, `ic09` 512.
#[cfg(any(target_os = "macos", test))]
fn icns() -> Vec<u8> {
    let parts: [(&[u8; 4], &[u8]); 4] = [
        (b"ic11", &include_bytes!("../icons/32x32.png")[..]),
        (b"ic07", &include_bytes!("../icons/128x128.png")[..]),
        (b"ic08", &include_bytes!("../icons/128x128@2x.png")[..]),
        (b"ic09", &include_bytes!("../icons/icon.png")[..]),
    ];
    let total: usize = 8 + parts.iter().map(|(_, png)| png.len() + 8).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&(total as u32).to_be_bytes());
    for (kind, png) in parts {
        out.extend_from_slice(kind);
        out.extend_from_slice(&((png.len() + 8) as u32).to_be_bytes());
        out.extend_from_slice(png);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("orchd-entry-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The fault this prevents costs an upgrade, not a launch: mise installs each
    /// version to its own directory, so an entry naming the resolved path is dead
    /// the moment `mise up` removes it.
    #[test]
    fn the_bundle_execs_the_binary_where_it_actually_lives() {
        let d = scratch("bundle");
        let exe = d.join("mise/installs/orchestrator/latest/orchestrator-desktop");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "x").unwrap();

        let (bundle, wrote) = write_app_bundle(&d.join("Applications"), &exe).unwrap();
        assert!(wrote, "a bundle that did not exist is a write");
        assert!(bundle.join("Contents/Info.plist").is_file());
        assert!(bundle.join("Contents/Resources/icon.icns").is_file());

        let launcher = bundle.join("Contents/MacOS/Orchestrator");
        let script = std::fs::read_to_string(&launcher).unwrap();
        assert!(script.starts_with("#!/bin/sh\n"), "{script}");
        assert!(
            script.contains(&format!("exec '{}'", exe.display())),
            "the path is quoted, because a home directory can hold a space: {script}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&launcher).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "a bundle that cannot be executed will not launch");
        }
    }

    /// The refresh runs on every launch, so "nothing changed" has to be free and
    /// silent. Only a moved binary may rewrite anything.
    #[test]
    fn writing_the_same_bundle_twice_reports_no_change() {
        let d = scratch("idempotent");
        let apps = d.join("Applications");
        let exe = d.join("bin/orchestrator-desktop");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "x").unwrap();

        assert!(write_app_bundle(&apps, &exe).unwrap().1);
        assert!(!write_app_bundle(&apps, &exe).unwrap().1, "second call changes nothing");

        let moved = d.join("bin2/orchestrator-desktop");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::write(&moved, "x").unwrap();
        assert!(write_app_bundle(&apps, &moved).unwrap().1, "a moved binary is a rewrite");
    }

    /// Not a format test for its own sake: Finder shows nothing at all for an icns
    /// whose declared length disagrees with its bytes, and there is no error.
    #[test]
    fn the_icns_declares_the_length_it_actually_has() {
        let icns = icns();
        assert_eq!(&icns[..4], b"icns");
        assert_eq!(u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize, icns.len());

        // Walk the chunks the way the loader does, and land exactly on the end.
        let mut at = 8;
        let mut kinds = Vec::new();
        while at < icns.len() {
            let len = u32::from_be_bytes(icns[at + 4..at + 8].try_into().unwrap()) as usize;
            kinds.push(String::from_utf8_lossy(&icns[at..at + 4]).to_string());
            assert!(len >= 8 && at + len <= icns.len(), "chunk at {at} runs past the end");
            assert_eq!(&icns[at + 8..at + 12], b"\x89PNG", "every type here takes a PNG");
            at += len;
        }
        assert_eq!(at, icns.len());
        assert_eq!(kinds, ["ic11", "ic07", "ic08", "ic09"]);
    }

    #[test]
    fn a_binary_in_a_build_tree_writes_no_entry_of_its_own() {
        assert!(in_build_tree(std::path::Path::new("/home/me/src/orchestrator/target/debug/orchestrator-desktop")));
        assert!(in_build_tree(std::path::Path::new("/home/me/src/orchestrator/target/release/orchestrator-desktop")));
        assert!(!in_build_tree(std::path::Path::new(
            "/home/me/.local/share/mise/installs/orchestrator/latest/orchestrator-desktop"
        )));
        assert!(!in_build_tree(std::path::Path::new("/usr/bin/orchestrator-desktop")));
    }

    /// A bundle id that drifts from the `.dmg`'s is two Orchestrators in the
    /// launcher, and nothing says so at the time.
    #[test]
    fn the_bundle_id_is_the_one_the_packages_ship() {
        let conf = include_str!("../tauri.conf.json");
        assert!(
            conf.contains(&format!("\"identifier\": \"{APP_ID}\"")),
            "tauri.conf.json and APP_ID disagree"
        );
        assert!(info_plist().contains(APP_ID));
        assert!(info_plist().contains(env!("CARGO_PKG_VERSION")));
    }
}
