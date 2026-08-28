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

/// Write a launcher entry for the binary that is running.
///
/// Only for the installs that ship no packaging of their own — `mise`, `ubi`, a
/// tarball unpacked by hand. It points `Exec` at `current_exe`, so upgrading in
/// place keeps working as long as the path is stable, which is exactly what mise's
/// `latest` symlink gives you.
///
/// XDG only. A macOS `.app` is a directory layout rather than a file to write, and
/// the `.dmg` already is one.
#[cfg(target_os = "linux")]
fn install_desktop_entry() -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;

    let exe = std::env::current_exe().context("finding this executable")?;
    let home = std::env::var("HOME").context("HOME is not set")?;
    // An empty `XDG_DATA_HOME` means unset, per the spec — and `env::var` hands it
    // back as `Ok("")`, which is how a test run wrote `applications/…` into the
    // working directory instead of under HOME.
    let data = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("{home}/.local/share"));

    // Same id as the bundles use, so installing a `.deb` later replaces this entry
    // rather than leaving the launcher showing the app twice.
    const ID: &str = "dev.orchd.orchestrator";
    for (px, bytes) in [
        (32u32, &include_bytes!("../icons/32x32.png")[..]),
        (128, &include_bytes!("../icons/128x128.png")[..]),
        (256, &include_bytes!("../icons/128x128@2x.png")[..]),
    ] {
        let dir = std::path::PathBuf::from(&data)
            .join("icons/hicolor")
            .join(format!("{px}x{px}"))
            .join("apps");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::write(dir.join(format!("{ID}.png")), bytes)?;
    }

    let apps = std::path::PathBuf::from(&data).join("applications");
    std::fs::create_dir_all(&apps).with_context(|| format!("creating {}", apps.display()))?;
    let file = apps.join(format!("{ID}.desktop"));
    std::fs::write(
        &file,
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Orchestrator\n\
             Comment=Session board for parallel Claude work\n\
             Exec={exe}\n\
             Icon={ID}\n\
             Terminal=false\n\
             Categories=Development;\n\
             Keywords=claude;sessions;orchestrator;\n",
            exe = exe.display()
        ),
    )
    .with_context(|| format!("writing {}", file.display()))?;

    // Best effort: most desktops notice the file on their own, and a missing
    // `update-desktop-database` is not a failure worth reporting.
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&apps)
        .status();
    Ok(file)
}

#[cfg(not(target_os = "linux"))]
fn install_desktop_entry() -> anyhow::Result<std::path::PathBuf> {
    anyhow::bail!("--install-desktop-entry is for Linux; macOS ships the .app in the .dmg")
}

fn main() {
    // Before anything opens a window: this is a one-shot, and it is the only way a
    // `mise`/tarball install gets an app you can find in a launcher. The `.deb`
    // and the AppImage carry their own entry, so it exists for the install method
    // that carries nothing.
    if std::env::args().any(|a| a == "--install-desktop-entry") {
        match install_desktop_entry() {
            Ok(path) => println!("wrote {}", path.display()),
            Err(e) => {
                eprintln!("could not write the desktop entry: {e:#}");
                std::process::exit(1);
            }
        }
        return;
    }

    wsl_render_workaround();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "orchd=info,orchestrator_desktop=info".into()),
        )
        .init();

    // After the logger, so the wait can say what it is waiting for, and well before
    // `orchd::start`: a replacement must not touch the lock, the port or the hook
    // settings while the process it is replacing still holds them.
    await_handoff();

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
    let exe = match std::env::current_exe() {
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
