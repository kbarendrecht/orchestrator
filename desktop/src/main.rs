//! Orchestrator as a desktop application.
//!
//! This is a window and a lifecycle, nothing more. The daemon it shows runs in
//! this same process — `orchd::start` binds a loopback port and the webview is
//! pointed at it — so there is no sidecar to supervise, no port to agree on
//! ahead of time, and no second process to leave behind on a crash.
//!
//! Deliberately absent: Tauri commands. The page is served from
//! `http://127.0.0.1:<port>`, which Tauri classifies as a *remote* origin, and
//! opening IPC to it means granting `http://127.0.0.1:*` — every port, any
//! page — the right to close the window. The UI already has an authenticated
//! channel to the daemon, so window control rides on that instead and this
//! crate exposes no IPC surface at all.

// No console window on Windows for a release build.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use orchd::window::{Chrome, ResizeEdge, WindowCmd, WindowControl};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// The daemon, once started. Held so the exit hook can tear it down.
static SERVER: OnceLock<Mutex<Option<orchd::Server>>> = OnceLock::new();

/// Whether the window is showing the board yet.
///
/// **The splash must not be remembered as the window's geometry.** It is 520x340
/// and centred, and `remember_window` fires on its `Resized`/`Moved` events like
/// any other — so it wrote that size into `window.json`, and because 520x340 is
/// below the board's own minimum, the next launch *rejected* the file and fell
/// back to the default size. The remembered size was being destroyed on every
/// boot by the loading screen, and the position with it.
static BOARD_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The first-run bootstrap server, while one is up. Aborted once a project is
/// committed and the window has moved to the daemon.
static BOOTSTRAP: OnceLock<Mutex<Option<tokio::task::AbortHandle>>> = OnceLock::new();

/// The async runtime handle, so the repo switcher can start a bootstrap server long
/// after `setup` has returned — it is raised from a window command, not from boot.
static RT: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Set by `WindowCmd::Restart`, read once the window is down.
///
/// The replacement is started from the exit path rather than from the command,
/// because the point of a restart here is the *sessions*: they are this process's
/// children and they have to be gone before their successors are spawned, or two
/// `claude` processes share a worktree. `shutdown` is what makes that true.
static RESTART: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How long a replacement waits for the process it is replacing.
///
/// `shutdown` kills the sessions and waits on them, so this is the tail of that,
/// not a guess about startup. Bounded because the alternative is a window that
/// never appears: if the old process is somehow still there, saying so beats
/// waiting in silence.
const HANDOFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// macOS keeps its real traffic lights over a transparent titlebar; everywhere
/// else the window is frameless and the SPA draws its own controls.
const CHROME: Chrome = if cfg!(target_os = "macos") {
    Chrome::Overlay
} else {
    Chrome::Custom
};

/// WSLg's virtual GPU mis-renders WebKitGTK's accelerated compositing layers as
/// stray white tiles — a white box on top of the UI, and white smears left
/// behind as hover states repaint. Forcing the software paint path removes both.
/// The cost is GPU-accelerated compositing, which does not matter for a terminal
/// board; the app already treats software rendering as the correct fallback.
///
/// Gated to WSL so a real Linux desktop with a working GPU keeps compositing, and
/// skipped when either variable is already set so the choice stays overridable
/// from the environment. Must run before GTK/WebKit start their web process,
/// hence the very top of `main`.
#[cfg(target_os = "linux")]
fn wsl_render_workaround() {
    let under_wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/version")
            .map(|v| v.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false);
    if !under_wsl {
        return;
    }
    for var in ["WEBKIT_DISABLE_DMABUF_RENDERER", "WEBKIT_DISABLE_COMPOSITING_MODE"] {
        if std::env::var_os(var).is_none() {
            std::env::set_var(var, "1");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn wsl_render_workaround() {}

/// Marker written around the PATH the login shell prints, so an rc file that
/// greets you does not become part of it.
const PATH_MARK: &str = "__ORCHD_PATH__";

/// Set once the PATH has been adopted, so a self-restart does not pay for it
/// again.
const ADOPTED: &str = "ORCHD_ADOPTED_LOGIN_PATH";

/// Give the daemon the PATH the user's shell would have given it.
///
/// **An app started from a launcher does not inherit your shell's environment.**
/// On macOS LaunchServices hands over `/usr/bin:/bin:/usr/sbin:/sbin`, and on
/// Linux a desktop entry gets the systemd user manager's. Neither holds Homebrew,
/// mise or `~/.local/bin`, so `gh`, `node` and `claude` are absent — and nothing
/// says "PATH" anywhere the user is looking. What they see is a PR pane reporting
/// no credential, a review queue reading unavailable, and sessions that die on
/// spawn. Measured on a colleague's Mac, from the `.app` this project now writes
/// itself, which is why it is worth doing rather than documenting.
///
/// Skipped when started from a terminal, because then the environment already is
/// the shell's. `-lic` rather than `-lc`: a login shell reads `.zprofile`, but
/// most people set PATH in `.zshrc`, which only an *interactive* shell reads.
///
/// Never fatal, and never destructive: the entries already present are kept, and a
/// shell that cannot be run leaves the process exactly as it was.
///
/// Called from the top of `main`, and it has to stay there: `set_var` is
/// process-global and unsound beside other threads, so this runs before the
/// runtime, the daemon and every pty exist.
fn adopt_login_path() {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() || std::env::var_os(ADOPTED).is_some() {
        return;
    }
    /* **The cache is here because this is on the critical path of the window.**
       A `.zshrc` that activates a tool manager costs one to three seconds, and
       the window cannot open until it answers. So a remembered answer is used
       when there is one, and the shell is asked again *afterwards* only to
       rewrite the file for next time.

       Why the refresh cannot apply itself: `set_var` is process-global and
       unsound beside other threads, which is why this whole function runs before
       the runtime, the daemon and every pty exist. A background refresh that
       called it would be exactly the thing that ordering exists to prevent. So
       the refresh writes the file and nothing else, and a changed rc file takes
       effect on the *next* launch. One launch of lag on a file most people edit
       once a year, against seconds off every start. */
    if let Some(cached) = cached_login_path() {
        apply_login_path(&cached);
        tracing::info!("adopted the remembered login PATH; refreshing it for next time");
        // Not a tokio task: there is no runtime yet, and it must not become one
        // — see above. A bare thread, detached, doing file IO and no more.
        std::thread::spawn(|| {
            if let Some(fresh) = ask_login_path() {
                write_cached_login_path(&fresh);
            }
        });
        return;
    }
    // First launch on this machine, or the cache was cleared. Pay for it once.
    let Some(theirs) = ask_login_path() else {
        return;
    };
    apply_login_path(&theirs);
    write_cached_login_path(&theirs);
    tracing::info!("adopted the login shell's PATH");
}

/// Put a PATH into this process, keeping what it already had.
fn apply_login_path(theirs: &str) {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let merged = merge_paths(theirs, &current);
    std::env::set_var("PATH", &merged);
    std::env::set_var(ADOPTED, "1");
}

/// Where the remembered PATH lives. Beside the config, so `ORCHD_CONFIG_DIR`
/// moves it with everything else durable.
fn login_path_cache() -> Option<std::path::PathBuf> {
    orchd::config::Config::config_dir()
        .ok()
        .map(|d| d.join("login-path"))
}

/// The remembered PATH, if it still looks like one.
///
/// Sanity-checked rather than trusted: a truncated or hand-edited file would
/// otherwise put junk in front of every lookup the daemon makes, and the failure
/// would be "`claude` not found" with no hint where it came from.
fn cached_login_path() -> Option<String> {
    let raw = std::fs::read_to_string(login_path_cache()?).ok()?;
    usable_path(&raw)
}

/// Does this file's contents look like a PATH worth adopting?
///
/// Its own function so the rule can be tested without a config dir. A truncated
/// or hand-edited cache would otherwise go in front of every lookup the daemon
/// makes, and the symptom would be "`claude` not found" with nothing pointing at
/// a file nobody remembers writing.
fn usable_path(raw: &str) -> Option<String> {
    let path = raw.trim();
    (!path.is_empty() && path.contains('/')).then(|| path.to_string())
}

fn write_cached_login_path(path: &str) {
    let Some(file) = login_path_cache() else {
        return;
    };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&file, format!("{path}\n")) {
        tracing::debug!("could not remember the login PATH: {e}");
    }
}

/// Ask the user's shell what PATH it would have given us.
///
/// `-lic` rather than `-lc`: a login shell reads `.zprofile`, but most people set
/// PATH in `.zshrc`, which only an *interactive* shell reads.
fn ask_login_path() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let argv = [
        shell,
        "-lic".into(),
        format!("printf '{PATH_MARK}%s{PATH_MARK}' \"$PATH\""),
    ];
    // Bounded, because an rc file that waits for something would otherwise hold
    // the window shut. Five seconds is a slow shell, not a hung one.
    let out = match orchd::proc::run_bounded(std::path::Path::new(&home), 5, &argv, "login shell") {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!("could not ask the login shell for PATH: {e:#}");
            return None;
        }
    };
    let said = String::from_utf8_lossy(&out.stdout);
    match path_between_marks(&said) {
        Some(p) => Some(p.to_string()),
        None => {
            tracing::warn!("the login shell printed no PATH; keeping the one we were given");
            None
        }
    }
}

/// The PATH between the two markers, if the shell printed both.
fn path_between_marks(said: &str) -> Option<&str> {
    let (_, rest) = said.split_once(PATH_MARK)?;
    let (path, _) = rest.split_once(PATH_MARK)?;
    (!path.trim().is_empty()).then_some(path)
}

/// The shell's PATH first, then anything we already had that it did not mention.
///
/// Order matters and theirs wins: a tool manager puts its shims in front on
/// purpose, and taking the version the user gets in a terminal is the whole point.
/// Nothing is dropped, because the inherited PATH is what the packaged install
/// relies on to find its own `orch`.
fn merge_paths(theirs: &str, ours: &std::ffi::OsStr) -> std::ffi::OsString {
    let mut merged: Vec<std::path::PathBuf> = std::env::split_paths(theirs).collect();
    for p in std::env::split_paths(ours) {
        if !merged.contains(&p) {
            merged.push(p);
        }
    }
    std::env::join_paths(merged).unwrap_or_else(|_| ours.to_os_string())
}

/// The launcher identity, shared with every bundle this project ships.
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
fn launcher_target() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("finding this executable")?;
    // The daemon's, because the push guard's hook has the same problem with the
    // same answer, and two copies of this rule is how one of them would keep the
    // version-pinned path.
    Ok(orchd::self_update::stable_exe(&exe))
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
fn refresh_launcher_entry() {
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
fn install_desktop_entry() -> Result<Option<std::path::PathBuf>> {
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
fn install_desktop_entry() -> Result<Option<std::path::PathBuf>> {
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
fn install_desktop_entry() -> Result<Option<std::path::PathBuf>> {
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

/// Where a log line can be read back from, when nobody is looking at a terminal.
///
/// **An app launched from Finder or a desktop launcher has no stdout**, so every
/// line the daemon writes goes nowhere. That is not a small gap: it is why a
/// colleague reporting a slow start could not send anything to look at, and why
/// the timing this module now records would have been invisible to the only
/// people who can see the problem. So the same lines also go to a file next to
/// the config, which is the one place both halves of the app already agree on.
///
/// One generation is kept. A restart is the interesting case to compare against
/// and it would otherwise overwrite itself, while an unbounded log on a machine
/// nobody is watching is the other way to lose the information.
struct LogFile(std::path::PathBuf);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogFile {
    /// Boxed because a file that cannot be opened has to degrade to writing
    /// nowhere. Losing the file log is not worth losing the app over, and it is
    /// the stdout layer that a developer is reading anyway.
    type Writer = Box<dyn std::io::Write>;

    /// Opened per line rather than held. It costs a syscall on a log this
    /// quiet, and it buys a file that is complete after a crash, which is the
    /// one case the log is being read for.
    fn make_writer(&'a self) -> Self::Writer {
        match std::fs::OpenOptions::new().create(true).append(true).open(&self.0) {
            Ok(f) => Box::new(f),
            Err(_) => Box::new(std::io::sink()),
        }
    }
}

fn init_logging() {
    use tracing_subscriber::layer::{Layer, SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "orchd=info,orchestrator_desktop=info".into())
    };
    let stdout = tracing_subscriber::fmt::layer().with_filter(filter());

    // `ORCHD_CONFIG_DIR` moves this with everything else durable, which is what
    // keeps a fixture daemon from writing over the real log.
    let dir = orchd::config::Config::config_dir().ok();
    let path = dir.as_ref().map(|dir| dir.join("orchd.log"));
    let file = path.clone().map(|path| {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::rename(&path, dir.join("orchd.log.1"));
        }
        tracing_subscriber::fmt::layer()
            // No colour: this one is read in an editor, not a terminal.
            .with_ansi(false)
            .with_writer(LogFile(path))
            .with_filter(filter())
    });

    tracing_subscriber::registry().with(stdout).with(file).init();

    // Said once, first, because the whole point of the file is that somebody has
    // to be able to find it without being told by hand.
    match path {
        Some(p) => tracing::info!("logging to {}", p.display()),
        None => tracing::warn!("no config dir — this run leaves no log file behind"),
    }
}

fn main() {
    // Before anything opens a window: a one-shot for the person who wants the
    // entry written now, or written again somewhere the refresh below declines to
    // touch. The launch-time refresh covers the ordinary case.
    if std::env::args().any(|a| a == "--install-desktop-entry") {
        match install_desktop_entry() {
            Ok(Some(path)) => println!("wrote {}", path.display()),
            Ok(None) => println!("already current"),
            Err(e) => {
                eprintln!("could not write the launcher entry: {e:#}");
                std::process::exit(1);
            }
        }
        return;
    }

    wsl_render_workaround();

    init_logging();

    // Held from the first thing `main` does that can be slow, because the phases
    // before the daemon are the ones a person launching from Finder pays for and
    // a person typing `cargo run` does not. `adopt_login_path` is the clearest
    // case: it returns immediately at a terminal and runs somebody's whole zsh
    // config from a launcher.
    let mut phases = orchd::timing::Phases::start();

    // Before the daemon, because everything it looks up — `gh` for the credential,
    // `node` for the review queue, `claude` for a session — is a PATH lookup made
    // from this process's environment.
    adopt_login_path();
    phases.mark("login-path");

    // After the logger so a write is visible, and before the window because the
    // point of it is the launcher: an install that carries no entry writes one
    // here, and a mise upgrade that moved the binary rewrites it.
    refresh_launcher_entry();
    phases.mark("launcher");

    // After the logger, so the wait can say what it is waiting for, and well before
    // `orchd::start`: a replacement must not touch the lock, the port or the hook
    // settings while the process it is replacing still holds them.
    await_handoff();
    // Nearly always zero. Non-zero only on a self-restart, where it is the
    // process being replaced taking its sessions down.
    phases.mark("handoff");
    phases.log("shell start");

    // Tauri owns the main thread, so the async half gets its own runtime. It is
    // never dropped — `App::run` does not return — which is what keeps the
    // daemon's pollers and pty readers alive for the life of the window.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("building the tokio runtime");
    let handle = rt.handle().clone();
    let _ = RT.set(handle.clone());

    let app = with_settings_item(tauri::Builder::default().plugin(tauri_plugin_dialog::init()))
        .setup(move |app| {
            let app_handle = app.handle().clone();
            let rt = handle.clone();

            match orchd::config::Config::existing() {
                // Configured already: straight to a window. A failure here is
                // shown rather than propagated: `?` would surface as a panic
                // from `build()`, and "already running" deserves a sentence in a
                // dialog, not a backtrace nobody launched a GUI to read.
                Some(_) => {
                    if let Err(e) = open(&app_handle, &rt, None) {
                        fail(&app_handle, &format!("{e:#}"));
                    }
                }
                // First run, or a config pointing at a checkout that has since
                // moved. Bring up the open-project window rather than a bare OS
                // dialog — there is no terminal here to read a CLI flag in either.
                None => {
                    if let Err(e) = first_run(&app_handle, &rt) {
                        fail(&app_handle, &format!("{e:#}"));
                    }
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("building the application");

    app.run(|app_handle, event| {
        // Recorded as it happens, not at exit: by the time the app is exiting the
        // window has already been destroyed, and there is nothing left to measure.
        // `Moved` as well as `Resized`, or dragging the window somewhere is never
        // written down and the next launch reopens at the last place it was
        // *resized* — which reads as the position being ignored.
        if let tauri::RunEvent::WindowEvent {
            event: tauri::WindowEvent::Resized(_) | tauri::WindowEvent::Moved(_),
            ..
        } = &event
        {
            remember_window(app_handle);
        }
        if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
            // Hold the loop open just long enough to take the children with us.
            api.prevent_exit();
            shutdown();
            app_handle.cleanup_before_exit();
            if RESTART.load(std::sync::atomic::Ordering::SeqCst) {
                relaunch();
            }
            // `None` is a user closing the window; `Some` came from an
            // `exit(n)` of ours, and a failed start that reports success is a
            // lie to whatever launched us.
            std::process::exit(code.unwrap_or(0));
        }
    });
}

/// The id the Settings item is recognised by in [`with_settings_item`].
#[cfg(target_os = "macos")]
const SETTINGS_ITEM: &str = "settings";

/// Put `Settings…` in the macOS application menu, where the platform says it lives.
///
/// The SPA has its own gear and no chord for it, so on a Mac the menu bar was the
/// one place a user looks first and found nothing. Only there: everywhere else this
/// window is frameless and carries no menu bar at all.
///
/// Built by amending [`Menu::default`] rather than by writing the whole menu out,
/// so About, Services, Hide and Quit stay whatever Tauri makes them. The app
/// submenu is its first item, and About plus its separator are that submenu's
/// first two, which is why the pair goes in at 2: About, separator, `Settings…`,
/// separator, Services, as every other Mac application reads.
///
/// A failure anywhere here loses the item, not the menu: the window matters more
/// than the shortcut to a panel that has a button.
#[cfg(target_os = "macos")]
fn with_settings_item(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};

    builder
        .menu(|app| {
            let menu = Menu::default(app)?;
            let items = menu.items()?;
            let Some(app_menu) = items.first().and_then(|i| i.as_submenu()) else {
                tracing::warn!("no application submenu to hang Settings on");
                return Ok(menu);
            };
            let item = MenuItem::with_id(app, SETTINGS_ITEM, "Settings\u{2026}", true, Some("Cmd+,"))?;
            app_menu.insert(&PredefinedMenuItem::separator(app)?, 2)?;
            app_menu.insert(&item, 2)?;
            Ok(menu)
        })
        .on_menu_event(|app, event| {
            if event.id() != SETTINGS_ITEM {
                return;
            }
            let Some(win) = app.get_webview_window("main") else { return };
            /* The page is a remote origin, so there is no IPC to call: this crate
               exposes none on purpose (see the module docs). `eval` is the whole
               channel, and the name is `web/app.js`'s. Guarded because the same
               window shows the splash and the first-run page, neither of which
               has an SPA in it. */
            if let Err(e) = win.eval("window.orchSettings && window.orchSettings()") {
                tracing::warn!("could not open settings from the menu: {e}");
            }
        })
}

/// No menu bar off macOS: the window is frameless and the SPA draws its own chrome.
#[cfg(not(target_os = "macos"))]
fn with_settings_item(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    builder
}

/// First run: no config, so bring up the open-project window instead of a native
/// dialog fired at nothing.
///
/// A small HTTP bootstrap server ([`orchd::firstrun`]) serves the page and the JSON
/// it calls; the window loads it, and choosing a project comes back through
/// [`TauriBootstrap`], which hands off to [`boot_daemon`]. HTTP rather than Tauri
/// IPC so the flow is the same shape as the daemon SPA and can be tested headlessly.
fn first_run(app_handle: &AppHandle, rt: &tokio::runtime::Handle) -> Result<()> {
    let host: Arc<dyn orchd::firstrun::BootstrapHost> = Arc::new(TauriBootstrap {
        app: app_handle.clone(),
    });
    let serving = rt
        .block_on(orchd::firstrun::serve(host))
        .context("starting the first-run server")?;
    let url = serving.url().parse().context("the bootstrap URL")?;
    // Kept so the daemon boot can stop it once a project is committed.
    *BOOTSTRAP.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(serving.task.abort_handle());
    // The open-project page wants the full board size; it is the window you work in.
    build_window(app_handle, WebviewUrl::External(url), board_size(), MIN_SIZE, false)
}

/// A splash window smaller than the board it grows into. Just big enough for the
/// wordmark and its dots; `boot_daemon` grows it to the real size when the daemon
/// is up, so the splash reads as a loading card rather than a full empty window.
const SPLASH_SIZE: (f64, f64) = (520.0, 340.0);

/// Below this the three-column grid stops being three columns. Applied to the board
/// and re-applied when the splash grows into it.
const MIN_SIZE: (f64, f64) = (1000.0, 600.0);

/// Navigate the main window to `url`, on the main thread — GTK calls only run there.
/// Best effort: a missing window or an unparseable URL is logged, not fatal.
fn navigate_main(app: &AppHandle, url: String) {
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        let Some(w) = app.get_webview_window("main") else { return };
        match url.parse::<tauri::Url>() {
            Ok(u) => {
                if let Err(e) = w.navigate(u) {
                    tracing::error!("could not navigate to {url}: {e}");
                }
            }
            Err(e) => tracing::error!("bad URL to navigate to ({url}): {e}"),
        }
    });
}

/// The size to open the board at: the one you left it, or 20% up on the 1440x900
/// this started at — three columns and a terminal want the room.
fn board_size() -> (f64, f64) {
    orchd::store::load_window()
        .and_then(|r| r.size())
        .map(|(w, h)| (w as f64, h as f64))
        .unwrap_or((1728.0, 1080.0))
}

/// Configured already: build the window on the splash and boot the daemon.
fn open(app_handle: &AppHandle, rt: &tokio::runtime::Handle, main: Option<std::path::PathBuf>) -> Result<()> {
    let splash = splash_url().context("preparing the splash")?;
    /* A small, centred splash; `boot_daemon` grows it to `board_size` on hand-off.
       **Except where the window cannot be moved afterwards.** A compositor that
       places the window keeps its top-left corner where it put it, so growing a
       520x340 card into a 1728x1080 board leaves the board hanging down and to
       the right of the spot the splash was centred on, and nothing may pull it
       back. Opening at the board's size instead means the one placement the
       compositor makes is the one the board keeps. The splash page is a centred
       flex column, so it is a wordmark in the middle of the window rather than a
       card, which is what a splash looks like anyway. */
    let (size, min) = if can_place_windows() {
        (SPLASH_SIZE, SPLASH_SIZE)
    } else {
        (board_size(), MIN_SIZE)
    };
    build_window(app_handle, WebviewUrl::External(splash), size, min, true)?;
    boot_daemon(app_handle.clone(), rt.clone(), main);
    Ok(())
}

/// Build the one window, on whatever page it opens on.
///
/// Called once. The daemon boot and the first-run commit both `navigate` this
/// window rather than building another — a second window would mean two of every
/// window command and two things for the exit hook to reason about.
fn build_window(
    app_handle: &AppHandle,
    url: WebviewUrl,
    size: (f64, f64),
    min: (f64, f64),
    center: bool,
) -> Result<()> {
    let mut phases = orchd::timing::Phases::start();
    let mut builder = WebviewWindowBuilder::new(app_handle, "main", url)
        .title("Orchestrator")
        /* **The ground is the app's, not the toolkit's white.** A webview paints
           white until a document says otherwise, and there are two moments here
           when nothing has: while the splash is still being fetched, and across the
           navigate to the daemon. Both showed as the page in the top-left corner
           with white filling the rest of the window, because the surface is already
           board-sized while the document is not. `background_color` on this builder
           sets the window *and* the webview, so there is no white to flash. */
        .background_color(tauri::window::Color(0x10, 0x10, 0x10, 0xFF))
        .inner_size(size.0, size.1)
        .min_inner_size(min.0, min.1);
    /* **Where it was, or centred — never left to the window manager.** Only the
       size used to be remembered, so placement was the WM's guess and the window
       turned up somewhere new on every launch. Centring alone is not the answer
       either: on a multi-head desktop the centre of the *virtual* screen is the
       seam between two monitors, which is how a centred splash ends up looking
       like it landed at random. */
    /* Applied *after* the window exists, not through the builder — see below.
       Both halves are the compositor's business where the app cannot place a
       window ([`can_place_windows`]), and asking anyway is not harmless: it is
       what wrote (0,0) into the file that then suppressed the centring. */
    let rec = orchd::store::load_window().unwrap_or_default();
    let restore_to = rec.pos().filter(|_| can_place_windows());
    if restore_to.is_none() && center && can_place_windows() {
        builder = builder.center();
    }

    #[cfg(target_os = "macos")]
    {
        use tauri::TitleBarStyle;
        builder = builder
            .title_bar_style(TitleBarStyle::Overlay)
            .hidden_title(true);
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Frameless. The SPA — and the first-run page — draw the controls and the
        // resize edges; see `Chrome::Custom` and `firstrun.html`.
        builder = builder.decorations(false);
    }

    // `_window` because only the Linux arm reads it: `Ctrl+Shift+Tab` never reaches
    // the SPA on WebKitGTK — GTK's focus chain claims the backward-traversal chord
    // before the page can, while the forward one arrives fine, which is the
    // asymmetry the report describes. Intercept it at the gtk window and re-inject
    // the DOM event the keymap already handles.
    let _window = builder.build().context("opening the window")?;
    /* **`set_position` after the build, rather than the builder's `position`.**
       The builder places the window before its frame exists, so what comes back
       from `outer_position` afterwards is offset by the decoration — and since that
       value is what gets saved, every launch subtracted the frame again and the
       window walked up and left across the desktop until it fell off it. Measured:
       300,150 → 262,91 → 224,32 → 186,-27. Setting and reading through the same
       pair round-trips instead. */
    if let Some((x, y)) = restore_to {
        let _ = _window.set_position(tauri::LogicalPosition::new(x as f64, y as f64));
    }
    // Nothing to rescue where nothing was placed: the check reads a position that
    // is always (0,0) there, and the rescue is a `center()` that does nothing.
    if can_place_windows() {
        ensure_on_screen(&_window);
    } else {
        /* **The state belongs where the size does.** This window already opened at
           the board's size (see `open`), so there is nothing left for the hand-off
           to do — and applying the state there instead is what put the splash in a
           corner: maximising grew the surface under a page that had already
           painted, and the compositor showed that old frame at the origin of the
           new one until the document caught up. Applied before anything is drawn,
           the first frame is the final geometry. */
        restore_state(&_window, &rec);
    }
    #[cfg(target_os = "linux")]
    wire_session_switch_keys(&_window);
    // The window exists here; it is not painted yet. Everything after this is
    // the webview fetching the page and the SPA waiting for its first snapshot,
    // which is the client's own half of the wait and is timed in the page.
    phases.mark("window");
    phases.log("window open");

    // WebKitGTK (WSLg especially) gives the webview no live input region until the
    // native window is resized once: on launch, clicks and the frameless resize
    // edges are dead until something nudges the window — collapsing a drawer is
    // what happens to do it by hand. A DOM repaint is not enough; it has to be a
    // real native resize. So grow the window a pixel and set it straight back,
    // just after it has had a moment to realise. Marshalled onto the main thread
    // because GTK window calls must not come from another one.
    #[cfg(not(target_os = "macos"))]
    {
        let ah = app_handle.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            let grow = ah.clone();
            let _ = ah.run_on_main_thread(move || {
                if let Some(w) = grow.get_webview_window("main") {
                    if let Ok(s) = w.inner_size() {
                        let _ = w.set_size(tauri::PhysicalSize::new(s.width, s.height + 1));
                    }
                }
            });
            std::thread::sleep(std::time::Duration::from_millis(60));
            let back = ah.clone();
            let _ = ah.run_on_main_thread(move || {
                if let Some(w) = back.get_webview_window("main") {
                    if let Ok(s) = w.inner_size() {
                        let _ = w.set_size(tauri::PhysicalSize::new(s.width, s.height - 1));
                    }
                }
            });
        });
    }
    Ok(())
}

/// Start the daemon off the main thread, navigate the window to it, and stop the
/// first-run server if one was up.
///
/// A `std::thread` rather than `rt.spawn` because `orchd::start` is not required to
/// be `Send` and `block_on` does not ask it to be — the same reason the resize nudge
/// is a thread. An error has no terminal to reach, so it lands in the OS dialog
/// `fail` draws, back on the main thread. Reached from a configured boot and from
/// the first-run commit, so it must not assume the window is on any particular page.
fn boot_daemon(app_handle: AppHandle, rt: tokio::runtime::Handle, main: Option<std::path::PathBuf>) {
    std::thread::spawn(move || {
        let mut phases = orchd::timing::Phases::start();
        let server = match rt.block_on(orchd::start(orchd::StartOptions {
            main_checkout: main,
            // No terminal to complain in, and a stale daemon on the configured port
            // should not be the difference between an app that opens and one that
            // does not.
            fallback_port: true,
            chrome: CHROME,
        })) {
            Ok(s) => s,
            Err(e) => {
                let ah = app_handle.clone();
                let _ = app_handle.run_on_main_thread(move || fail(&ah, &format!("{e:#}")));
                return;
            }
        };
        phases.mark("daemon");
        tracing::info!("serving {} on port {}", server.app.cfg.main_checkout.display(), server.port);
        let url = server.url();

        // Attach the window control before the SPA can call it, and keep the server
        // alive for the life of the process.
        let control: Arc<dyn WindowControl> = Arc::new(TauriWindow { app: app_handle.clone() });
        rt.block_on(server.app.attach_window(control));
        *SERVER.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(server);

        /* Grow from the splash to the board, then hand the window over. GTK calls
           only on the main thread.

           **All of it, only where the splash opened small.** Where the app cannot
           place a window the splash was already board-sized and already in its
           final state (see `open` and `build_window`), so every call here would be
           a no-op or an undo: `set_size` on a maximised window is the one that
           bites, and applying the state this late is what put the splash in a
           corner — the surface grew under a page that had painted, and the
           compositor showed that old frame at the origin of the new one. */
        let ah = app_handle.clone();
        let _ = app_handle.run_on_main_thread(move || {
            let Some(w) = ah.get_webview_window("main") else { return };
            if can_place_windows() {
                // `set_min_size` first, so the larger size is never clamped.
                let (bw, bh) = board_size();
                let _ = w.set_min_size(Some(tauri::LogicalSize::new(MIN_SIZE.0, MIN_SIZE.1)));
                let _ = w.set_size(tauri::LogicalSize::new(bw, bh));
                // Back where it was left, and centred only when there is no such
                // place. Centring unconditionally is what moved the board away from
                // the splash — and on a multi-head desktop, onto the seam between
                // two monitors.
                let rec = orchd::store::load_window().unwrap_or_default();
                match rec.pos() {
                    Some((x, y)) => {
                        let _ = w.set_position(tauri::LogicalPosition::new(x as f64, y as f64));
                    }
                    None => {
                        let _ = w.center();
                    }
                }
                ensure_on_screen(&w);
                // Last, so it grows over the size and place it would otherwise have
                // had: leaving the state then puts the window back where it was.
                restore_state(&w, &rec);
            }
            // From here the geometry is the board's, so it is worth remembering.
            BOARD_UP.store(true, std::sync::atomic::Ordering::SeqCst);
            match url.parse::<tauri::Url>() {
                Ok(u) => {
                    if let Err(e) = w.navigate(u) {
                        tracing::error!("could not navigate to the daemon: {e}");
                    }
                }
                Err(e) => tracing::error!("the daemon's own URL did not parse ({url}): {e}"),
            }
        });
        // The first-run page and its port are dead weight now.
        stop_bootstrap();
        phases.log("daemon ready");
    });
}

/// Whether this platform lets a window know where it is and choose where to be.
///
/// **A Wayland client is neither told its position nor allowed to set one.** The
/// protocol has no window coordinates at all: placement belongs to the
/// compositor, `gtk_window_move` is ignored, and `gtk_window_get_position`
/// answers (0,0) for every window. So `inner_position` returns a real number that
/// is not a position, and `window.json` recorded `"x":0,"y":0` on every save while
/// the window sat wherever the user had dragged it. Restoring it then did nothing,
/// which is "remembering the position is broken" exactly as reported: nothing here
/// was wrong except the assumption that the question can be asked.
///
/// Read the way GDK itself picks a backend: `GDK_BACKEND` wins when it is set,
/// otherwise a `WAYLAND_DISPLAY` in the environment means Wayland. Asking GDK for
/// its display type would be the direct question, and it cannot be asked before
/// the display exists — this one can, and it is the same answer.
#[cfg(target_os = "linux")]
fn can_place_windows() -> bool {
    match std::env::var("GDK_BACKEND") {
        // A comma-separated preference list; the first one that works is used, and
        // wayland-first is the case that matters.
        Ok(b) => !b
            .split(',')
            .next()
            .unwrap_or_default()
            .eq_ignore_ascii_case("wayland"),
        Err(_) => std::env::var_os("WAYLAND_DISPLAY").is_none(),
    }
}

/// Everywhere else a window is placed by the application.
#[cfg(not(target_os = "linux"))]
fn can_place_windows() -> bool {
    true
}

/// Which display the window is on, as far as the display server will say.
///
/// **This is the one placement question Wayland answers.** A client is never told
/// where it is and may not move itself, but the compositor does tell a surface
/// which output it is being shown on, and GTK passes that through — so "which
/// screen was it on" is recordable even where "where was it" is not.
#[cfg(target_os = "linux")]
fn monitor_of(win: &tauri::WebviewWindow) -> Option<orchd::store::MonitorRef> {
    use gtk::prelude::*;
    let gw = win.gtk_window().ok()?;
    let display = gtk::gdk::Display::default()?;
    let m = display.monitor_at_window(&gw.window()?)?;
    let g = m.geometry();
    Some(orchd::store::MonitorRef {
        name: m.model().map(|s| s.to_string()).unwrap_or_default(),
        x: g.x(),
        y: g.y(),
        width: g.width(),
        height: g.height(),
    })
}

/// Elsewhere the position already says which screen, so there is nothing to record.
#[cfg(not(target_os = "linux"))]
fn monitor_of(_win: &tauri::WebviewWindow) -> Option<orchd::store::MonitorRef> {
    None
}

/// Put the window back the way it was left: maximised, fullscreen, or neither.
///
/// **Fullscreen is the only way to name a screen on Wayland.** `xdg_toplevel` has
/// no coordinates, so nothing can move a window to a monitor — but
/// `set_fullscreen` takes an output, and GTK exposes it as
/// `fullscreen_on_monitor`. That is why a window left fullscreen can come back on
/// the display it was on, and a merely *maximised* one lands wherever the
/// compositor puts it. It is a protocol limit, not an oversight.
fn restore_state(win: &tauri::WebviewWindow, rec: &orchd::store::WindowRecord) {
    if rec.is("fullscreen") {
        #[cfg(target_os = "linux")]
        if fullscreen_on_recorded_monitor(win, rec) {
            return;
        }
        let _ = win.set_fullscreen(true);
    } else if rec.is("maximized") {
        let _ = win.maximize();
    }
}

/// Fullscreen on the display the record names, if it is still attached.
///
/// Matched on geometry first: two of the three monitors this was written on report
/// the same model string, so a name alone is a coin toss between them. The name is
/// the fallback for a desk rearranged since, and only when it is unambiguous —
/// guessing between two screens is what this exists to stop.
#[cfg(target_os = "linux")]
fn fullscreen_on_recorded_monitor(
    win: &tauri::WebviewWindow,
    rec: &orchd::store::WindowRecord,
) -> bool {
    use gtk::prelude::{GtkWindowExt, MonitorExt};
    let Some(want) = rec.monitor.as_ref() else { return false };
    let (Ok(gw), Some(display)) = (win.gtk_window(), gtk::gdk::Display::default()) else {
        return false;
    };
    let mut by_name = Vec::new();
    let mut exact = None;
    for i in 0..display.n_monitors() {
        let Some(m) = display.monitor(i) else { continue };
        let g = m.geometry();
        if (g.x(), g.y(), g.width(), g.height()) == (want.x, want.y, want.width, want.height) {
            exact = Some(i);
            break;
        }
        if !want.name.is_empty() && m.model().map(|s| s.to_string()).as_deref() == Some(&want.name) {
            by_name.push(i);
        }
    }
    let Some(idx) = exact.or_else(|| (by_name.len() == 1).then(|| by_name[0])) else {
        tracing::info!(monitor = %want.name, "the display it was left on is not attached");
        return false;
    };
    let screen = GtkWindowExt::screen(&gw);
    let Some(screen) = screen else { return false };
    gw.fullscreen_on_monitor(&screen, idx);
    true
}

/// Put the window back on a monitor if the place it was told to open is not on one.
///
/// **A remembered position outlives the display it was remembered on.** Unplug the
/// second monitor, or dock somewhere with a different arrangement, and the saved
/// spot is a coordinate nothing can show — the window opens where no mouse can
/// reach it and the app looks like it failed to start. Cheaper to check than to
/// explain, and it only ever fires in that case: a position on any attached
/// monitor is left exactly as it is.
fn ensure_on_screen(win: &tauri::WebviewWindow) {
    let Ok(pos) = win.inner_position() else { return };
    let Ok(monitors) = win.available_monitors() else { return };
    // **An empty list is "cannot tell", not "off-screen".** Failing the other way
    // moves the window on a desktop that simply did not answer — which is the
    // complaint this function exists to fix, caused by the fix.
    if monitors.is_empty() {
        return;
    }
    // The window's own top-left, against each monitor's rectangle. A window mostly
    // off the edge is still reachable; one whose corner is on no monitor at all is
    // the case worth rescuing.
    let visible = monitors.iter().any(|m| {
        let (mp, ms) = (m.position(), m.size());
        pos.x >= mp.x
            && pos.y >= mp.y
            && pos.x < mp.x + ms.width as i32
            && pos.y < mp.y + ms.height as i32
    });
    if !visible {
        tracing::info!(
            x = pos.x,
            y = pos.y,
            "the remembered window position is on no attached monitor — centring instead"
        );
        let _ = win.center();
    }
}

/// Stop the first-run server if one is still running.
fn stop_bootstrap() {
    if let Some(handle) = BOOTSTRAP.get().and_then(|b| b.lock().unwrap().take()) {
        handle.abort();
    }
}

/// The URL of the running daemon, if there is one. `None` on first run, before any
/// project has been opened.
fn daemon_url() -> Option<String> {
    SERVER
        .get()
        .and_then(|s| s.lock().unwrap().as_ref().map(|srv| srv.url()))
}

/// Ask for a restart: verify there is a binary to come back as, then close the
/// window so the exit path tears the daemon down and `relaunch` starts the
/// replacement. Called off the main thread (a window command or a bootstrap request),
/// where `close` belongs — wry panics if it is handled on the main thread.
fn request_restart(app: &AppHandle) {
    match launcher_target() {
        Ok(exe) if exe.exists() => {
            RESTART.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.close();
            }
        }
        Ok(exe) => tracing::error!("not restarting: {} does not exist, sessions kept", exe.display()),
        Err(e) => tracing::error!("not restarting: no path to this binary ({e:#}), sessions kept"),
    }
}

/// Raise the open-project modal over the running board, to switch projects.
///
/// Serves the first-run page again and navigates the window to it. The same
/// `TauriBootstrap` host — so it knows a daemon is already up (`switching`) and a
/// committed project restarts onto it rather than booting a second daemon. Cancel
/// navigates back to the board. Starts the server off `setup`, hence [`RT`].
fn start_switcher(app: &AppHandle) {
    let Some(rt) = RT.get() else {
        return tracing::error!("no runtime to raise the switcher");
    };
    let host: Arc<dyn orchd::firstrun::BootstrapHost> = Arc::new(TauriBootstrap {
        app: app.clone(),
    });
    let app = app.clone();
    rt.spawn(async move {
        let serving = match orchd::firstrun::serve(host).await {
            Ok(s) => s,
            Err(e) => return tracing::error!("could not start the switcher: {e:#}"),
        };
        *BOOTSTRAP.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(serving.task.abort_handle());
        navigate_main(&app, serving.url());
    });
}

/// The window-side of the first-run flow, handed to [`orchd::firstrun`]'s HTTP
/// server: the native folder dialog, the daemon boot, and the frameless window
/// commands the page's own titlebar needs.
struct TauriBootstrap {
    app: AppHandle,
}

impl orchd::firstrun::BootstrapHost for TauriBootstrap {
    fn pick(&self) -> Option<std::path::PathBuf> {
        use tauri_plugin_dialog::DialogExt;
        // Runs on a bootstrap request thread, not the GTK main thread, so blocking
        // on the dialog's answer is safe — the plugin marshals the dialog to the
        // main thread and calls back here.
        let (tx, rx) = std::sync::mpsc::channel();
        self.app
            .dialog()
            .file()
            .set_title("Choose the main checkout")
            .pick_folder(move |picked| {
                let _ = tx.send(picked);
            });
        match rx.recv() {
            Ok(Some(fp)) => fp.simplified().into_path().ok(),
            _ => None,
        }
    }

    fn open(&self, path: std::path::PathBuf) {
        if self.switching() {
            // A daemon is already up and the new project's config is written, so a
            // restart brings the app back on it. The current project's live sessions
            // go with the restart — `auto_resume` brings a project's sessions back
            // when you switch to it again.
            tracing::info!("switching project — restarting onto {}", path.display());
            request_restart(&self.app);
        } else if let Some(rt) = RT.get() {
            boot_daemon(self.app.clone(), rt.clone(), Some(path));
        } else {
            tracing::error!("no runtime to boot the daemon");
        }
    }

    fn window_cmd(&self, cmd: orchd::window::WindowCmd) {
        if let Err(e) = (TauriWindow { app: self.app.clone() }).dispatch(cmd) {
            tracing::warn!("first-run window command failed: {e}");
        }
    }

    fn switching(&self) -> bool {
        daemon_url().is_some()
    }

    fn cancel(&self) {
        // Back to the running board, and drop the switcher server.
        let Some(url) = daemon_url() else { return };
        navigate_main(&self.app, url);
        stop_bootstrap();
    }
}

/// Write the splash page and hand back a `file://` URL to load it from.
///
/// A `file://` asset rather than the daemon's own page, because at this point the
/// daemon is not up yet — and on first run there is no checkout to start one with.
/// The splash is passive: it draws identity while the daemon starts and needs no
/// IPC, so a file is enough. The interactive first-run screens (open a project,
/// review) come later and will want a real asset or IPC bridge instead.
fn splash_url() -> Result<tauri::Url> {
    let dir = orchd::config::Config::config_dir().context("finding the config dir")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("splash.html");
    std::fs::write(&path, splash_html()).with_context(|| format!("writing {}", path.display()))?;
    tauri::Url::from_file_path(&path)
        .map_err(|_| anyhow::anyhow!("splash path is not absolute: {}", path.display()))
}

/// The splash markup: the wordmark, a cluster of pulsing session dots and the
/// tagline, on orchd's own near-black ground. Self-contained — no external fonts,
/// so nothing loads before it paints; the shipped SPA uses IBM Plex, this leans on
/// the system mono while it is only on screen for about a second.
fn splash_html() -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><style>
  html,body{{height:100%;margin:0}}
  body{{background:#101010;color:#D2D2D2;display:flex;flex-direction:column;
    align-items:center;justify-content:center;gap:22px;
    font-family:ui-monospace,'SF Mono',Menlo,Consolas,monospace;
    -webkit-font-smoothing:antialiased;user-select:none;cursor:default}}
  .dots{{display:grid;grid-template-columns:repeat(3,9px);gap:5px}}
  .dots i{{width:9px;height:9px;border-radius:50%;background:#2C2C2C}}
  .dots i.on{{background:#E0A244;animation:p 1.5s ease-in-out infinite}}
  .dots i.on.b{{animation-delay:.25s}} .dots i.on.c{{animation-delay:.5s}}
  @keyframes p{{0%,100%{{opacity:.28}}50%{{opacity:1}}}}
  @media(prefers-reduced-motion:reduce){{.dots i.on{{animation:none;opacity:.85}}}}
  .mark{{font-size:34px;font-weight:600;letter-spacing:.02em}}
  .mark b{{color:#E0A244;font-weight:600}}
  .tag{{font-size:12px;color:#8E8E8E;letter-spacing:.22em;text-transform:uppercase}}
  .boot{{position:fixed;bottom:22px;font-size:11.5px;color:#787878}}
  .ver{{position:fixed;top:16px;right:16px;font-size:11px;color:#787878}}
</style></head><body>
  <span class="ver">v{version}</span>
  <div class="dots"><i class="on"></i><i></i><i class="on c"></i><i></i><i class="on b"></i><i></i><i class="on c"></i><i></i><i class="on"></i></div>
  <div class="mark">orch<b>d</b></div>
  <div class="tag">session orchestrator</div>
  <div class="boot">starting daemon…</div>
</body></html>"#,
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// Make `Ctrl+Shift+Tab` reach the SPA on WebKitGTK.
///
/// `Ctrl+Tab` and `Ctrl+Shift+Tab` are GTK focus-chain accelerators. GTK runs them
/// in the toplevel's own `key-press-event` handler *before* the event is propagated
/// to the focused widget — the webview — so the backward chord is consumed and the
/// page's keydown listener never fires. The forward one happens to survive, which is
/// the exact asymmetry the report names.
///
/// `connect_key_press_event` runs before that default handler, so this sees the
/// chord first. When it is the backward one — Shift+Tab is delivered as the
/// `ISO_Left_Tab` keyval, not `Tab` with a shift bit — it re-injects the very
/// keydown the SPA's keymap already understands and returns `Stop`, which skips
/// GTK's focus move. The forward chord is left untouched precisely because it works.
#[cfg(target_os = "linux")]
fn wire_session_switch_keys(window: &tauri::WebviewWindow) {
    use gtk::prelude::*;
    let gtk_win = match window.gtk_window() {
        Ok(w) => w,
        Err(e) => return tracing::warn!("no gtk window for the previous-session key: {e:#}"),
    };
    let webview = window.clone();
    gtk_win.connect_key_press_event(move |_, ev| {
        use gtk::gdk;
        let ctrl = ev.state().contains(gdk::ModifierType::CONTROL_MASK);
        let shift = ev.state().contains(gdk::ModifierType::SHIFT_MASK);
        let key = ev.keyval();
        let backward = ctrl
            && (key == gdk::keys::constants::ISO_Left_Tab
                || (key == gdk::keys::constants::Tab && shift));
        if backward {
            // isTrusted is false, which the keymap does not check; it reads only
            // key/ctrlKey/shiftKey, and this is the shape it switches on.
            let _ = webview.eval(
                "window.dispatchEvent(new KeyboardEvent('keydown',\
                 {key:'Tab',ctrlKey:true,shiftKey:true}))",
            );
            return gtk::glib::Propagation::Stop;
        }
        gtk::glib::Propagation::Proceed
    });
}

/// Take the daemon's children down before the process goes.
/// Write down how big the window is, so the next launch opens the same way.
///
/// Logical pixels, not physical: the physical size is multiplied by the display's
/// scale factor, and handing that back to `inner_size` grows the window by that
/// factor on every restart.
///
/// Called on every resize. That is a small file written a few times while you drag
/// an edge, which is cheaper than the alternative of not knowing the size when it
/// matters.
fn remember_window(app: &AppHandle) {
    // Nothing before the board: see [`BOARD_UP`].
    if !BOARD_UP.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    /* **The size of a maximised window belongs to the screen, not to the window**,
       so it is not recorded: restoring into it opens every launch full-screen with
       nothing to un-maximise back to. What *is* recorded is that it was maximised,
       over whatever size was last written — otherwise a window left maximised
       comes back at the size it had before, which is "the geometry is not
       remembered" as reported. Fullscreen counts as maximised here: the window
       comes back maximised rather than fullscreen, because a window that reopens
       with no chrome and no way out is worse than one a click puts back. */
    let full = win.is_fullscreen().unwrap_or(false);
    if full || win.is_maximized().unwrap_or(false) {
        let mut rec = orchd::store::load_window().unwrap_or_default();
        rec.state = Some(if full { "fullscreen" } else { "maximized" }.to_string());
        rec.monitor = monitor_of(&win).or(rec.monitor);
        orchd::store::save_window(&rec);
        return;
    }
    let Ok(scale) = win.scale_factor() else {
        return;
    };
    if let Ok(size) = win.inner_size() {
        let logical = size.to_logical::<f64>(scale);
        // The position too, in logical pixels like the size, so a display whose
        // scale factor changes does not move the window on the next launch.
        /* **`inner_position`, to pair with `set_position`.** They must be the same
           reference point or restoring drifts: `set_position` places the client
           area, while `outer_position` reports the frame origin, and under this
           window manager those differ by the decoration — a constant (38,59) here.
           Saving the frame origin and restoring it as the client origin subtracted
           that on every launch, and the window walked off the desktop in four
           starts: 300,150 → 262,91 → 224,32 → 186,-27. */
        /* Only where the answer means something: GTK reports (0,0) for every
           Wayland window, and writing that down turns "the platform does not say"
           into a coordinate the next launch would try to honour. Left out, the
           file carries a size and no position, which `load_window_pos` already
           reads as "no opinion". */
        let at = can_place_windows()
            .then(|| win.inner_position().ok())
            .flatten()
            .map(|p| {
                let l = p.to_logical::<f64>(scale);
                (l.x.round() as i32, l.y.round() as i32)
            });
        orchd::store::save_window(&orchd::store::WindowRecord {
            width: logical.width.round() as u32,
            height: logical.height.round() as u32,
            x: at.map(|(x, _)| x),
            y: at.map(|(_, y)| y),
            state: None,
            monitor: monitor_of(&win),
        });
    }
}

/// Start our own replacement, and tell it what to wait for.
///
/// Tauri's own `restart` spawns and exits, which cannot work here: `instance::acquire`
/// refuses the moment it finds a live holder pid, so the replacement would come up,
/// read this process as the holder and die with "already running" — leaving nothing
/// at all. So the new process is handed our pid and waits for it. The lock file is
/// left behind by `exit`, which is the documented behaviour and exactly what makes
/// the wait sufficient: `acquire` clears a file whose owner is gone.
fn relaunch() {
    // [`stable_exe`], not the path we are running from. A restart is what applies a
    // self-upgrade, and mise installs each version in its own directory: by the time
    // this runs, the path this process was started from may be the version that was
    // just replaced, or gone. The `latest` symlink beside it is the one that means
    // "the build to run now".
    let exe = match launcher_target() {
        Ok(p) => p,
        Err(e) => return tracing::error!("cannot restart, no path to this binary: {e}"),
    };
    // Our own arguments minus argv[0], so a restart keeps whatever it was started
    // with, plus the handoff.
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    args.push("--wait-for-pid".into());
    args.push(std::process::id().to_string().into());
    match std::process::Command::new(&exe).args(&args).spawn() {
        Ok(child) => tracing::info!(pid = child.id(), "restarting"),
        Err(e) => tracing::error!("could not restart: {e}"),
    }
}

/// Wait for the process we are replacing to go, if we are a replacement.
///
/// Before anything binds a port or takes the lock. Both are held until the old
/// process actually exits, and its sessions are its children — so starting early
/// would mean two daemons and, worse, two agents in one worktree.
fn await_handoff() {
    let args: Vec<String> = std::env::args().collect();
    let Some(pid) = args
        .iter()
        .position(|a| a == "--wait-for-pid")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse::<u32>().ok())
    else {
        return;
    };
    let deadline = std::time::Instant::now() + HANDOFF_TIMEOUT;
    while orchd::pty::pid_alive(pid) {
        if std::time::Instant::now() > deadline {
            tracing::warn!(pid, "the process being replaced is still running; starting anyway");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    tracing::info!(pid, "the process being replaced is gone");
}

fn shutdown() {
    let Some(server) = SERVER.get().and_then(|s| s.lock().unwrap().take()) else {
        return;
    };
    // A fresh runtime: the exit hook runs on the main thread and must not
    // depend on the state of the one the daemon has been living on.
    match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt.block_on(server.shutdown()),
        Err(e) => tracing::error!("could not tear down cleanly: {e}"),
    }
}

/// Nothing to show and no page to show it on, so the OS dialog is the only
/// place left to put an error.
fn fail(app_handle: &AppHandle, message: &str) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

    tracing::error!("{message}");
    app_handle
        .dialog()
        .message(message)
        .kind(MessageDialogKind::Error)
        .title("Orchestrator could not start")
        .blocking_show();
    app_handle.exit(1);
}

/// The daemon's window seam, implemented against Tauri.
struct TauriWindow {
    app: AppHandle,
}

impl WindowControl for TauriWindow {
    fn dispatch(&self, cmd: WindowCmd) -> Result<()> {
        let w = self
            .app
            .get_webview_window("main")
            .context("the window is gone")?;

        // Every one of these arrives on an axum worker thread, which is where
        // they belong: tauri-runtime-wry panics outright if `close` is handled
        // on the main thread, and the others post themselves to the event loop
        // regardless of where they are called from.
        match cmd {
            WindowCmd::Minimize => w.minimize()?,
            WindowCmd::ToggleMaximize => {
                if w.is_maximized()? {
                    w.unmaximize()?
                } else {
                    w.maximize()?
                }
            }
            WindowCmd::Close => w.close()?,
            // Closing *is* the restart: the exit path tears the daemon down and
            // takes the sessions with it, and only then is it safe to start the
            // process that will spawn their replacements. So verify there is a
            // binary to come back as *before* committing to the close — a restart
            // that cannot happen must refuse rather than take every live session
            // with it. `relaunch` runs after `shutdown`, too late to change course.
            WindowCmd::Restart => request_restart(&self.app),
            // Raise the open-project modal to switch projects. Off the current
            // thread on purpose — it starts a server and navigates.
            WindowCmd::Switcher => start_switcher(&self.app),
            WindowCmd::StartDrag => w.start_dragging()?,
            // Only `Window` has this, not `WebviewWindow`, so go through the
            // webview to reach it. macOS never asks: it keeps its decorations,
            // and the underlying call is a no-op there anyway.
            WindowCmd::StartResize(edge) => {
                let webview: &tauri::webview::Webview<_> = w.as_ref();
                webview.window().start_resize_dragging(match edge {
                    ResizeEdge::North => tauri_runtime::ResizeDirection::North,
                    ResizeEdge::NorthEast => tauri_runtime::ResizeDirection::NorthEast,
                    ResizeEdge::East => tauri_runtime::ResizeDirection::East,
                    ResizeEdge::SouthEast => tauri_runtime::ResizeDirection::SouthEast,
                    ResizeEdge::South => tauri_runtime::ResizeDirection::South,
                    ResizeEdge::SouthWest => tauri_runtime::ResizeDirection::SouthWest,
                    ResizeEdge::West => tauri_runtime::ResizeDirection::West,
                    ResizeEdge::NorthWest => tauri_runtime::ResizeDirection::NorthWest,
                })?
            }
        }
        Ok(())
    }
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

    /// A remembered PATH goes in front of every binary the daemon looks up, so
    /// junk in that file has to answer "no" rather than "maybe".
    #[test]
    fn a_remembered_path_is_checked_before_it_is_trusted() {
        assert_eq!(
            usable_path("/opt/homebrew/bin:/usr/bin\n").as_deref(),
            Some("/opt/homebrew/bin:/usr/bin"),
            "the trailing newline the writer adds is not part of the value"
        );
        // The shapes a half-written or hand-edited file actually takes.
        assert!(usable_path("").is_none());
        assert!(usable_path("   \n").is_none(), "whitespace is empty");
        assert!(usable_path("no-slashes-here").is_none(), "that is not a path list");
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
    fn the_shell_path_is_read_between_the_marks_and_greetings_are_not() {
        let said = format!("Welcome back!\n{PATH_MARK}/opt/homebrew/bin:/usr/bin{PATH_MARK}");
        assert_eq!(path_between_marks(&said), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(path_between_marks("no markers here"), None, "a shell that failed says nothing");
        assert_eq!(
            path_between_marks(&format!("{PATH_MARK}{PATH_MARK}")),
            None,
            "an empty PATH is a shell that did not answer, not an answer"
        );
    }

    /// The shell's order is the answer, and the inherited entries still have to
    /// survive: a packaged install finds its own `orch` through one of them.
    #[test]
    fn the_shell_path_wins_and_nothing_inherited_is_dropped() {
        let merged = merge_paths(
            "/opt/homebrew/bin:/usr/bin",
            std::ffi::OsStr::new("/usr/bin:/Applications/Orchestrator.app/Contents/MacOS"),
        );
        assert_eq!(
            merged.to_string_lossy(),
            "/opt/homebrew/bin:/usr/bin:/Applications/Orchestrator.app/Contents/MacOS"
        );
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
