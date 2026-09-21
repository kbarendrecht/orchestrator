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

/// The binaries a bundle carries, and the first is the bundle's executable.
///
/// All three, not the app alone. `child.rs::daemon_binary` resolves `orchd` in
/// `current_exe().parent()` and a session's `orch` the same way, so a bundle
/// holding only the app is one that opens a window and can do nothing — the same
/// failure v2026.9.14 shipped through the packages (#16). The names match the
/// `.dmg`'s layout deliberately: one shape to reason about, and `app-check` drives
/// `Contents/MacOS/orchestrator-desktop` in both.
#[cfg(any(target_os = "macos", test))]
const BINARIES: [&str; 3] = ["orchestrator-desktop", "orchd", "orch"];

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
    packaged_path(&exe)
}

/// The half of [`packaged`] that is only a path, so it can be tested.
///
/// **A bundle this app wrote is not packaging, and telling the two apart is what
/// keeps the refresh alive.** The app runs from a *copy* inside its own bundle
/// since #24, so `/Contents/MacOS/` is where an ordinary mise install starts rather
/// than proof that an installer owns the tree. Without the first branch the refresh
/// would decline on every launch, and the copies would be pinned to whatever
/// version wrote them. The marker beside the copies is what says which is which.
fn packaged_path(exe: &std::path::Path) -> bool {
    if own_bundle(exe).is_some() {
        return false;
    }
    let path = exe.to_string_lossy();
    path.contains("/Contents/MacOS/") || path.starts_with("/usr/")
}

/// The `.app` this executable runs from, when this app is the one that wrote it.
///
/// `Contents/Resources/source` is the marker and the record in one: it names the
/// install the copies were taken from, which is the only way back to it. A `.dmg`
/// install has no such file, and nor does anything else that puts a binary in a
/// `Contents/MacOS`.
fn own_bundle(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    if macos.file_name()? != std::ffi::OsStr::new("MacOS")
        || contents.file_name()? != std::ffi::OsStr::new("Contents")
    {
        return None;
    }
    source_marker(bundle)
        .is_file()
        .then(|| bundle.to_path_buf())
}

/// Where the record of the source install sits inside a bundle.
fn source_marker(bundle: &std::path::Path) -> std::path::PathBuf {
    bundle.join("Contents/Resources/source")
}

/// The install a bundle's copies came from, as recorded when they were copied.
///
/// The first line only. The rest of the marker is the staleness stamp, which is
/// nobody's business but [`write_app_bundle`]'s.
#[cfg(any(target_os = "macos", test))]
fn recorded_source(bundle: &std::path::Path) -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string(source_marker(bundle)).ok()?;
    let first = text.lines().next()?.trim();
    (!first.is_empty()).then(|| std::path::PathBuf::from(first))
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
        // 512 because a launcher draws the largest size it finds. A 512 left by an
        // earlier install outranks all three above, so without this a changed icon
        // reaches three directories and shows in none.
        (512, &include_bytes!("../icons/icon.png")[..]),
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

/// The install a bundle must take its copies from.
///
/// Not [`launcher_target`] when the app already runs from its own bundle. The
/// running executable is a copy then, `current_exe` points inside the bundle, and
/// resolving from it would copy the bundle onto itself — which pins the app to
/// whatever version last wrote it and makes `mise up` invisible for good. The
/// marker written beside the copies is the way back to the install mise owns.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(
    not(target_os = "macos"),
    expect(
        dead_code,
        reason = "compiled on Linux so it type-checks; only the macOS entry calls it"
    )
)]
fn source_exec() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("finding this executable")?;
    if let Some(source) = own_bundle(&exe).as_deref().and_then(recorded_source) {
        return Ok(source);
    }
    launcher_target()
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
    let (bundle, wrote) = write_app_bundle(&apps, &source_exec()?)?;
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

/// Write `<apps>/Orchestrator.app` around the install `exec` belongs to.
///
/// Says whether anything changed, so the launch-time refresh can stay quiet.
///
/// **This wrote a four-line `sh` stub until #24, and the stub is why macOS refused
/// to manage the window.** A process started through a script has no bundle and no
/// bundle identifier, so Rectangle's Accessibility move and resize did nothing and
/// a drag to the top edge opened Mission Control instead of the tiling preview.
/// The reporter proved the cause by swapping in the `.dmg`'s bundle and changing
/// nothing else, whereupon every chord worked. So the binaries are copied in, and
/// the bundle is real.
///
/// The stub's reasons were real and are paid for rather than dismissed. It cost
/// disk, which is three binaries now. It went stale at `mise up`, which the stamp
/// and the marker below handle. And it kept `mise` the owner of the binary, which
/// it still is: this only ever copies *from* the install, never writes to it.
///
/// One behaviour genuinely changes. The stub always exec'd the newest binary, and
/// a copy cannot: a launch after `mise up` refreshes the copies and then goes on
/// running the old ones, because they are already mapped. The new version arrives
/// at the launch after that.
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
    let macos = contents.join("MacOS");
    let source_dir = exec
        .parent()
        .with_context(|| format!("{} is not in a directory", exec.display()))?;

    let mut wrote = write_if_changed(&contents.join("Info.plist"), info_plist().as_bytes())?;
    wrote |= write_if_changed(&contents.join("Resources/icon.icns"), &icns())?;

    // The stamp decides whether the copies are stale, and it is three `stat`s
    // rather than a hash of the binaries: this runs at every launch, and the
    // launch path is the one a person waiting on a window pays for.
    let stamp = source_stamp(exec, source_dir)?;
    let moved = write_if_changed(&source_marker(&bundle), stamp.as_bytes())?;
    // A missing copy counts too: the stamp alone would call a bundle current after
    // somebody deleted a binary out of it, and `orchd` is the one that matters.
    let incomplete = BINARIES.iter().any(|name| !macos.join(name).exists());
    if moved || incomplete {
        for name in BINARIES {
            copy_executable(&source_dir.join(name), &macos.join(name))?;
        }
        // **An upgrade has to clear the old executable out, not write beside it.**
        // Every bundle written before #24 holds `MacOS/Orchestrator`, the `sh` stub
        // that was the executable then. The plist names the copy now, so the stub
        // is dead weight — but it is dead weight inside the signature, and
        // `codesign` seals everything in `MacOS` as code. Leaving it there is a
        // bundle that may refuse to sign, on exactly the machines that upgrade
        // rather than install fresh.
        if let Ok(entries) = std::fs::read_dir(&macos) {
            for stale in entries.flatten().filter(|e| {
                e.file_name()
                    .to_str()
                    .is_none_or(|name| !BINARIES.contains(&name))
            }) {
                let _ = std::fs::remove_file(stale.path());
            }
        }
        wrote = true;
    }

    // Last, because it seals what is in the bundle: anything written after a
    // signature invalidates it.
    #[cfg(target_os = "macos")]
    if wrote {
        sign_ad_hoc(&bundle);
    }
    Ok((bundle, wrote))
}

/// What the copies were taken from, in a form that changes when they go stale.
///
/// The first line is the record [`recorded_source`] reads back: where the copies
/// came from, which is the one thing a copy cannot work out for itself.
///
/// The rest is the staleness test. The canonical directory catches the ordinary
/// case on its own — mise installs each version beside the last and moves `latest`,
/// so following the symlink is what makes an upgrade visible. The length and mtime
/// catch the case it misses: the same version reinstalled in place.
#[cfg(any(target_os = "macos", test))]
fn source_stamp(exec: &std::path::Path, source_dir: &std::path::Path) -> Result<String> {
    let canonical = std::fs::canonicalize(source_dir).unwrap_or_else(|_| source_dir.to_path_buf());
    let mut out = format!("{}\n{}\n", exec.display(), canonical.display());
    for name in BINARIES {
        let path = source_dir.join(name);
        let meta = std::fs::metadata(&path).with_context(|| {
            format!(
                "{} is missing, so the bundle would not work",
                path.display()
            )
        })?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        out.push_str(&format!("{name} {} {mtime}\n", meta.len()));
    }
    Ok(out)
}

/// Copy `src` onto `dest`, which may be the executable of the running process.
///
/// **Never `fs::copy` straight onto it.** That truncates the destination in place,
/// and the destination here is the binary the person may have launched this very
/// process from — on macOS that corrupts a running image rather than failing.
/// Write a neighbour and rename: the rename is atomic, and the running process
/// keeps the inode it started from until it exits.
#[cfg(any(target_os = "macos", test))]
fn copy_executable(src: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let dir = dest
        .parent()
        .with_context(|| format!("{} is not in a directory", dest.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dest.with_extension("new");
    std::fs::copy(src, &tmp)
        .with_context(|| format!("copying {} to {}", src.display(), tmp.display()))?;
    // On the copy, not on the destination afterwards: the bit has to be set before
    // the rename, or a launch in that window finds a file it cannot execute.
    #[cfg(unix)]
    std::fs::set_permissions(
        &tmp,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .with_context(|| format!("making {} executable", tmp.display()))?;
    std::fs::rename(&tmp, dest)
        .with_context(|| format!("moving {} into place at {}", tmp.display(), dest.display()))
}

/// Sign the bundle ad hoc, so macOS treats it as a bundle rather than as damage.
///
/// A warning rather than an error, and deliberately: `codesign` belongs to the
/// Xcode command line tools, and a machine without them would otherwise get no
/// launcher entry at all. A bundle written here carries no quarantine attribute, so
/// an unsigned one still opens — this is what keeps the *copies* inside the seal,
/// which is the half the `.dmg` got wrong in #26.
#[cfg(target_os = "macos")]
fn sign_ad_hoc(bundle: &std::path::Path) {
    let out = std::process::Command::new("codesign")
        .args(["--force", "--deep", "--sign", "-"])
        .arg(bundle)
        .output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => tracing::warn!(
            "codesign refused {}: {}",
            bundle.display(),
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::warn!("could not run codesign for {}: {e}", bundle.display()),
    }
}

/// The bundle's `Info.plist`.
///
/// `CFBundleIdentifier` matches the `.dmg`'s, which is deliberate: install the
/// package later and LaunchServices treats it as the same application rather than
/// showing two Orchestrators.
///
/// **`LSArchitecturePriority` was the one key here that was load-bearing rather
/// than cosmetic, and it was missing.** `CFBundleExecutable` was a `/bin/sh`
/// script then — see the writer above, which copies a real binary since #24 — so
/// there was no Mach-O for LaunchServices to read an architecture from. It stays
/// because it is still true and costs nothing, but it is belt and braces now
/// rather than the only answer. With no priority declared it built
/// `BinaryOrderPreference = {x86_64, arm64}`, `/bin/sh` is universal so it ran
/// **translated**, and every process the app started inherited that: `git` then
/// failed to load `libxcrun` and the daemon concluded the checkout was not a work
/// tree. Reported in #18 and diagnosed from the launch record — the app that
/// launched it was itself arm64, so the preference was never inherited from
/// outside. It belongs to this file.
///
/// The value is **this build's own architecture**, not a literal `arm64`: the
/// release is Apple Silicon only, but `--install-desktop-entry` runs on whatever
/// machine built or installed the binary, and naming an architecture the
/// executable is not would recreate the bug pointing the other way.
#[cfg(any(target_os = "macos", test))]
fn info_plist() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let exe = BINARIES[0];
    // Rust and LaunchServices spell it differently, and only these two matter: the
    // macOS targets this ever builds for are `aarch64` and `x86_64`.
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>{APP_NAME}</string>
  <key>CFBundleDisplayName</key><string>{APP_NAME}</string>
  <key>CFBundleExecutable</key><string>{exe}</string>
  <key>CFBundleIdentifier</key><string>{APP_ID}</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>10.15</string>
  <key>LSArchitecturePriority</key><array><string>{arch}</string></array>
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

    /// **A shell-script `CFBundleExecutable` has no architecture, so the plist has
    /// to carry one.**
    ///
    /// Without `LSArchitecturePriority`, LaunchServices built
    /// `BinaryOrderPreference = {x86_64, arm64}` for this bundle, `/bin/sh` is
    /// universal so it ran translated, and every process the app spawned inherited
    /// it — which surfaced as `git` failing to load `libxcrun` and the daemon
    /// reporting that the checkout was not a work tree (#18). Nothing in the app
    /// can observe the key's absence; only the launch record can, which is why it
    /// took a reporter reading `log show` to find.
    ///
    /// Asserted against `std::env::consts::ARCH` rather than a literal, because the
    /// bug reversed is just as bad: an `arm64` priority on an x86_64 build would
    /// translate that one instead.
    #[test]
    fn the_bundle_declares_the_architecture_its_binary_was_built_for() {
        let plist = info_plist();
        let want = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            other => other,
        };
        assert!(
            plist.contains("<key>LSArchitecturePriority</key>"),
            "a shell-script bundle with no architecture priority launches translated: {plist}"
        );
        assert!(
            plist.contains(&format!("<array><string>{want}</string></array>")),
            "the priority must name this build's own architecture ({want}): {plist}"
        );
    }

    /// An install of the shape the bundle is written from: the three binaries the
    /// tarball ships, in one directory, as `mise` and `ubi` leave them.
    fn install(at: &std::path::Path, body: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(at).unwrap();
        for name in BINARIES {
            std::fs::write(at.join(name), format!("{body}-{name}")).unwrap();
        }
        at.join(BINARIES[0])
    }

    /// **The bundle carries the binaries, because a process outside a bundle is a
    /// window macOS will not manage (#24).** This wrote an `sh` stub that exec'd
    /// the install, and a process started that way has no bundle identifier:
    /// Rectangle's Accessibility move did nothing and a drag to the top edge opened
    /// Mission Control instead of tiling. All three, not just the app, because
    /// `daemon_binary` resolves `orchd` beside `current_exe` and finds nothing if
    /// only the app was copied.
    #[test]
    fn the_bundle_carries_the_binaries_the_app_resolves_beside_itself() {
        let d = scratch("bundle");
        let exe = install(&d.join("mise/installs/orchestrator/latest"), "v1");

        let (bundle, wrote) = write_app_bundle(&d.join("Applications"), &exe).unwrap();
        assert!(wrote, "a bundle that did not exist is a write");
        assert!(bundle.join("Contents/Info.plist").is_file());
        assert!(bundle.join("Contents/Resources/icon.icns").is_file());

        for name in BINARIES {
            let copy = bundle.join("Contents/MacOS").join(name);
            assert_eq!(
                std::fs::read_to_string(&copy).unwrap(),
                format!("v1-{name}"),
                "{name} is a copy of the install's own"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&copy).unwrap().permissions().mode();
                assert_eq!(
                    mode & 0o777,
                    0o755,
                    "a bundle that cannot be executed will not launch"
                );
            }
        }
        assert!(
            info_plist().contains(&format!("<string>{}</string>", BINARIES[0])),
            "CFBundleExecutable must name the copy, not the old script"
        );
        assert!(
            !bundle.join("Contents/MacOS/Orchestrator").exists(),
            "the sh stub is what #24 was, and it must not be left beside the copy"
        );
    }

    /// A copy cannot work out where it came from, and it is the only thing that
    /// needs to: without the marker the refresh resolves the *bundle* as the
    /// install, copies it onto itself, and `mise up` never reaches the app again.
    #[test]
    fn the_bundle_records_the_install_it_was_copied_from() {
        let d = scratch("marker");
        let exe = install(&d.join("mise/installs/orchestrator/latest"), "v1");
        let (bundle, _) = write_app_bundle(&d.join("Applications"), &exe).unwrap();

        assert_eq!(recorded_source(&bundle).as_deref(), Some(exe.as_path()));
        assert_eq!(
            own_bundle(&bundle.join("Contents/MacOS").join(BINARIES[0])).as_deref(),
            Some(bundle.as_path()),
            "a bundle this app wrote must be recognised from the executable inside it"
        );
        assert!(!packaged_path(
            &bundle.join("Contents/MacOS").join(BINARIES[0])
        ));
    }

    /// The other half of the rule above. A `.dmg` install has the same shape and
    /// must keep declining the refresh, or every launch rewrites a tree an
    /// installer owns.
    #[test]
    fn a_dmg_install_is_still_packaged() {
        let d = scratch("dmg");
        let exe = d.join("Applications/Orchestrator.app/Contents/MacOS/orchestrator-desktop");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "x").unwrap();

        assert_eq!(own_bundle(&exe), None, "no marker, so not ours");
        assert!(packaged_path(&exe));
    }

    /// The refresh runs on every launch, so "nothing changed" has to be free and
    /// silent. Only a moved or replaced install may rewrite anything.
    #[test]
    fn writing_the_same_bundle_twice_reports_no_change() {
        let d = scratch("idempotent");
        let apps = d.join("Applications");
        let exe = install(&d.join("bin"), "v1");

        assert!(write_app_bundle(&apps, &exe).unwrap().1);
        assert!(
            !write_app_bundle(&apps, &exe).unwrap().1,
            "second call changes nothing"
        );

        // What `mise up` does: a new version directory, and `latest` moved onto it.
        let moved = install(&d.join("bin2"), "v2");
        assert!(
            write_app_bundle(&apps, &moved).unwrap().1,
            "a moved install is a rewrite"
        );
        assert_eq!(
            std::fs::read_to_string(apps.join("Orchestrator.app/Contents/MacOS/orchd")).unwrap(),
            "v2-orchd",
            "the copies follow the install, or the app runs last year's daemon for ever"
        );
    }

    /// **An upgrade from the stub, which is what every existing install is.** The
    /// `sh` script that used to be `CFBundleExecutable` sits in the same directory
    /// the copies land in, and `codesign` seals everything there as code — so a
    /// bundle that keeps it is one that may refuse to sign on the machines that
    /// upgrade rather than install fresh.
    #[test]
    fn an_upgrade_clears_the_stub_the_bundle_used_to_run() {
        let d = scratch("upgrade");
        let apps = d.join("Applications");
        let exe = install(&d.join("bin"), "v1");

        // The bundle as a version before #24 left it.
        let macos = apps.join("Orchestrator.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::write(macos.join("Orchestrator"), "#!/bin/sh\nexec somewhere\n").unwrap();

        write_app_bundle(&apps, &exe).unwrap();
        assert!(
            !macos.join("Orchestrator").exists(),
            "the stub outlived the upgrade and is inside the signature"
        );
        for name in BINARIES {
            assert!(macos.join(name).is_file());
        }
    }

    /// A deleted binary is the failure the stamp alone cannot see, and `orchd` is
    /// the one that matters: without it the window opens and no session starts.
    #[test]
    fn a_bundle_missing_a_binary_is_rewritten() {
        let d = scratch("incomplete");
        let apps = d.join("Applications");
        let exe = install(&d.join("bin"), "v1");
        let (bundle, _) = write_app_bundle(&apps, &exe).unwrap();

        std::fs::remove_file(bundle.join("Contents/MacOS/orchd")).unwrap();
        assert!(
            write_app_bundle(&apps, &exe).unwrap().1,
            "the stamp is unchanged, so only the missing file can report this"
        );
        assert!(bundle.join("Contents/MacOS/orchd").is_file());
    }

    /// **The copy may be the running process's own executable**, and `fs::copy`
    /// truncates in place. Asserted through an open handle: the bytes a process
    /// already mapped must survive the refresh that replaces them.
    #[test]
    fn a_refresh_does_not_write_through_the_running_executable() {
        let d = scratch("inode");
        let src = d.join("src/orchestrator-desktop");
        std::fs::create_dir_all(src.parent().unwrap()).unwrap();
        std::fs::write(&src, "new").unwrap();
        let dest = d.join("dest/orchestrator-desktop");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, "running").unwrap();

        let open = std::fs::File::open(&dest).unwrap();
        copy_executable(&src, &dest).unwrap();

        let mut held = String::new();
        std::io::Read::read_to_string(&mut { open }, &mut held).unwrap();
        assert_eq!(held, "running", "the running image must keep its own inode");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new");
        assert!(
            !dest.with_extension("new").exists(),
            "the neighbour is renamed, not left behind inside a signed bundle"
        );
    }

    /// Not a format test for its own sake: Finder shows nothing at all for an icns
    /// whose declared length disagrees with its bytes, and there is no error.
    #[test]
    fn the_icns_declares_the_length_it_actually_has() {
        let icns = icns();
        assert_eq!(&icns[..4], b"icns");
        assert_eq!(
            u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize,
            icns.len()
        );

        // Walk the chunks the way the loader does, and land exactly on the end.
        let mut at = 8;
        let mut kinds = Vec::new();
        while at < icns.len() {
            let len = u32::from_be_bytes(icns[at + 4..at + 8].try_into().unwrap()) as usize;
            kinds.push(String::from_utf8_lossy(&icns[at..at + 4]).to_string());
            assert!(
                len >= 8 && at + len <= icns.len(),
                "chunk at {at} runs past the end"
            );
            assert_eq!(
                &icns[at + 8..at + 12],
                b"\x89PNG",
                "every type here takes a PNG"
            );
            at += len;
        }
        assert_eq!(at, icns.len());
        assert_eq!(kinds, ["ic11", "ic07", "ic08", "ic09"]);
    }

    #[test]
    fn a_binary_in_a_build_tree_writes_no_entry_of_its_own() {
        assert!(in_build_tree(std::path::Path::new(
            "/home/me/src/orchestrator/target/debug/orchestrator-desktop"
        )));
        assert!(in_build_tree(std::path::Path::new(
            "/home/me/src/orchestrator/target/release/orchestrator-desktop"
        )));
        assert!(!in_build_tree(std::path::Path::new(
            "/home/me/.local/share/mise/installs/orchestrator/latest/orchestrator-desktop"
        )));
        assert!(!in_build_tree(std::path::Path::new(
            "/usr/bin/orchestrator-desktop"
        )));
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
