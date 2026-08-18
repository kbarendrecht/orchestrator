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

fn main() {
    wsl_render_workaround();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "orchd=info,orchestrator_desktop=info".into()),
        )
        .init();

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
        if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
            // Hold the loop open just long enough to take the children with us.
            api.prevent_exit();
            shutdown();
            app_handle.cleanup_before_exit();
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

    let url = server.url().parse().context("the daemon's own URL")?;
    let mut builder = WebviewWindowBuilder::new(app_handle, "main", WebviewUrl::External(url))
        .title("Orchestrator")
        // 20% up on the 1440x900 this started at: three columns and a terminal
        // want the room, and every desktop this runs on has it.
        .inner_size(1728.0, 1080.0)
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

    let control: Arc<dyn WindowControl> = Arc::new(TauriWindow {
        app: app_handle.clone(),
    });
    rt.block_on(server.app.attach_window(control));

    *SERVER.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(server);
    Ok(())
}

/// Take the daemon's children down before the process goes.
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
