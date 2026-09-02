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

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
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
                // moved. Ask, rather than dying with a CLI flag in the message
                // — there is no terminal here to read it in.
                None => pick_checkout(app_handle, rt),
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("building the application");

    app.run(|app_handle, event| {
        // Recorded as it happens, not at exit: by the time the app is exiting the
        // window has already been destroyed, and there is nothing left to measure.
        if let tauri::RunEvent::WindowEvent {
            event: tauri::WindowEvent::Resized(_),
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

/// Ask for the main checkout, then open on it.
///
/// The callback form on purpose: `blocking_pick_folder` deadlocks against the
/// event loop when called from the main thread, and `setup` is the main thread.
fn pick_checkout(app_handle: AppHandle, rt: tokio::runtime::Handle) {
    use tauri_plugin_dialog::DialogExt;

    app_handle
        .clone()
        .dialog()
        .file()
        .set_title("Choose the main checkout")
        .pick_folder(move |picked| {
            let Some(picked) = picked else {
                // Cancelled at the first prompt: there is nothing to show, so
                // leaving a blank window open would only be confusing.
                tracing::info!("no checkout chosen — exiting");
                app_handle.exit(0);
                return;
            };
            let path = match picked.simplified().into_path() {
                Ok(p) => p,
                Err(e) => return fail(&app_handle, &format!("that is not a local folder: {e}")),
            };
            if let Err(e) = open(&app_handle, &rt, Some(path)) {
                fail(&app_handle, &format!("{e:#}"));
            }
        });
}

/// Start the daemon and show it.
fn open(app_handle: &AppHandle, rt: &tokio::runtime::Handle, main: Option<std::path::PathBuf>) -> Result<()> {
    // The daemon logs its own phases; this brackets them with the window, which
    // is the part a person is actually waiting for and which nothing else times.
    let mut phases = orchd::timing::Phases::start();
    let server = rt
        .block_on(orchd::start(orchd::StartOptions {
            main_checkout: main,
            // No terminal to complain in, and a stale daemon on the configured
            // port should not be the difference between an app that opens and
            // one that does not.
            fallback_port: true,
            chrome: CHROME,
        }))
        .context("starting the daemon")?;

    phases.mark("daemon");
    tracing::info!("serving {} on port {}", server.app.cfg.main_checkout.display(), server.port);

    let size = orchd::store::load_window()
        .map(|(w, h)| (w as f64, h as f64))
        .unwrap_or((1728.0, 1080.0));
    let url = server.url().parse().context("the daemon's own URL")?;
    let mut builder = WebviewWindowBuilder::new(app_handle, "main", WebviewUrl::External(url))
        .title("Orchestrator")
        // The size you left it at, or 20% up on the 1440x900 this started at:
        // three columns and a terminal want the room, and every desktop this runs
        // on has it.
        .inner_size(size.0, size.1)
        // Below this the three-column grid stops being three columns.
        .min_inner_size(1000.0, 600.0);

    #[cfg(target_os = "macos")]
    {
        use tauri::TitleBarStyle;
        builder = builder
            .title_bar_style(TitleBarStyle::Overlay)
            .hidden_title(true);
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Frameless. The SPA draws the controls and the resize edges; see
        // `Chrome::Custom`.
        builder = builder.decorations(false);
    }

    builder.build().context("opening the window")?;
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

    let control: Arc<dyn WindowControl> = Arc::new(TauriWindow {
        app: app_handle.clone(),
    });
    rt.block_on(server.app.attach_window(control));

    *SERVER.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(server);
    Ok(())
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
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    // Not while maximised or fullscreen: that size belongs to the screen, not to
    // the window, and restoring into it means every launch opens full-screen with
    // nothing to un-maximise back to.
    if win.is_maximized().unwrap_or(false) || win.is_fullscreen().unwrap_or(false) {
        return;
    }
    let Ok(scale) = win.scale_factor() else {
        return;
    };
    if let Ok(size) = win.inner_size() {
        let logical = size.to_logical::<f64>(scale);
        orchd::store::save_window(logical.width.round() as u32, logical.height.round() as u32);
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
            // process that will spawn their replacements.
            WindowCmd::Restart => {
                RESTART.store(true, std::sync::atomic::Ordering::SeqCst);
                w.close()?
            }
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
