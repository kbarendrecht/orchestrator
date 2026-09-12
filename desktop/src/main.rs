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

mod launcher;
mod login_path;

use launcher::{install_desktop_entry, launcher_target, refresh_launcher_entry};
use login_path::adopt_login_path;

/// The daemon, once started. Held so the exit hook can tear it down.
static SERVER: OnceLock<Mutex<Option<orchd_serve::host::Serving>>> = OnceLock::new();

/// Whether the window is showing the board yet.
///
/// **The splash must not be remembered as the window's geometry.** It is 520x340
/// and centred, and `remember_window` fires on its `Resized`/`Moved` events like
/// any other — so it wrote that size into `window.json`, and because 520x340 is
/// below the board's own minimum, the next launch *rejected* the file and fell
/// back to the default size. The remembered size was being destroyed on every
/// boot by the loading screen, and the position with it.
static BOARD_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set by the first open that boots the daemon, and never cleared: a second open
/// before the board is up would start `orchd::start` twice, and the second one
/// fails on the instance lock — which `fail` turns into an exit of the whole app,
/// right after a boot that had worked. A double-click on "Open project" did that.
static BOOTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The first-run bootstrap server, while one is up. Aborted once a project is
/// committed and the window has moved to the daemon.
static BOOTSTRAP: OnceLock<Mutex<Option<tokio::task::AbortHandle>>> = OnceLock::new();

/// The async runtime handle, so the first-run page can boot a daemon long after
/// `setup` has returned — the checkout is picked by a person, at their own pace.
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

/// Spelled once, because the writer and the reader are one protocol and drifted
/// apart in a subtler way than a rename: see [`handoff_args`].
const WAIT_FOR_PID: &str = "--wait-for-pid";

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
    for var in [
        "WEBKIT_DISABLE_DMABUF_RENDERER",
        "WEBKIT_DISABLE_COMPOSITING_MODE",
    ] {
        if std::env::var_os(var).is_none() {
            std::env::set_var(var, "1");
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn wsl_render_workaround() {}

/// A poisoned lock here is still readable, and refusing it is worse than reading it.
///
/// Both of these hold an `Option` the code takes whole — the bootstrap server's
/// abort handle and the running daemon — so there is no half-written state for
/// poisoning to protect. `unwrap` instead would mean one panic anywhere in the
/// shell turning "quit" and "restart" into panics of their own, in the process
/// that owns the window and every child daemon. `orchd::host` says the same at
/// more length.
fn poisoned_is_still_usable<T>(poisoned: std::sync::PoisonError<T>) -> T {
    // See `orchd::host`'s `locked`: recovering without a line in the log makes
    // "the shell kept going" a guess.
    tracing::warn!("recovered a poisoned lock; a thread panicked holding it");
    poisoned.into_inner()
}

fn main() {
    // Before anything opens a window: a one-shot for the person who wants the
    // entry written now, or written again somewhere the refresh below declines to
    // touch. The launch-time refresh covers the ordinary case.
    if std::env::args().any(|a| a == "--install-desktop-entry") {
        // The one branch of this binary that is a command line rather than a
        // window, so it is the one place a print reaches a person.
        #[expect(
            clippy::print_stdout,
            clippy::print_stderr,
            reason = "run from a shell, never from the launcher"
        )]
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

    // Both from the library, so a checkout's own child daemon logs the same lines
    // to the same kind of file rather than into a pipe nobody drains.
    orchd::logging::init("orchd=info,orchestrator_desktop=info", true);
    // Right after the logger exists, since the hook writes through it.
    orchd::logging::install_panic_hook();

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
    #[expect(
        clippy::expect_used,
        reason = "no runtime is no app; there is nothing to degrade to"
    )]
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("building the tokio runtime");
    let handle = rt.handle().clone();
    let _ = RT.set(handle.clone());

    #[expect(
        clippy::expect_used,
        reason = "a window that will not build is a launch that has already failed"
    )]
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
            let item =
                MenuItem::with_id(app, SETTINGS_ITEM, "Settings\u{2026}", true, Some("Cmd+,"))?;
            app_menu.insert(&PredefinedMenuItem::separator(app)?, 2)?;
            app_menu.insert(&item, 2)?;
            Ok(menu)
        })
        .on_menu_event(|app, event| {
            if event.id() != SETTINGS_ITEM {
                return;
            }
            let Some(win) = app.get_webview_window("main") else {
                return;
            };
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
/// A small HTTP bootstrap server ([`orchd_serve::firstrun`]) serves the page and the JSON
/// it calls; the window loads it, and choosing a project comes back through
/// [`TauriBootstrap`], which hands off to [`boot_daemon`]. HTTP rather than Tauri
/// IPC so the flow is the same shape as the daemon SPA and can be tested headlessly.
fn first_run(app_handle: &AppHandle, rt: &tokio::runtime::Handle) -> Result<()> {
    let host: Arc<dyn orchd_serve::firstrun::BootstrapHost> = Arc::new(TauriBootstrap {
        app: app_handle.clone(),
    });
    let serving = rt
        .block_on(orchd_serve::firstrun::serve(host))
        .context("starting the first-run server")?;
    let url = serving.url().parse().context("the bootstrap URL")?;
    // Kept so the daemon boot can stop it once a project is committed.
    *BOOTSTRAP
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(poisoned_is_still_usable) = Some(serving.task.abort_handle());
    // The open-project page wants the full board size; it is the window you work in.
    build_window(
        app_handle,
        WebviewUrl::External(url),
        board_size(),
        MIN_SIZE,
        false,
    )
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
        let Some(w) = app.get_webview_window("main") else {
            return;
        };
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
fn open(
    app_handle: &AppHandle,
    rt: &tokio::runtime::Handle,
    main: Option<std::path::PathBuf>,
) -> Result<()> {
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
    /* Read once, here, because `transparent` is a property of the window at the
    moment it is built and no runtime call takes it back. The theme's opacity is
    what moves day to day; this only decides whether there is anything behind the
    board to show. Off unless `host.json` says otherwise — see
    [`orchd_serve::host::HostFile::see_through_window`] for why that is the safe way
    round.

    macOS needs one more thing, and it is in `tauri.conf.json` rather than here:
    `macOSPrivateApi`, without which `transparent` is accepted and does nothing.
    It is on unconditionally because the config cannot read a runtime setting,
    and its one real cost — the Mac App Store refuses an app that uses it — is
    not a cost this app pays: it ships as a dmg. */
    let see_through = orchd_serve::host::see_through_window();
    let mut builder = WebviewWindowBuilder::new(app_handle, "main", url)
        .title("Orchestrator")
        .transparent(see_through)
        /* **The ground is the app's, not the toolkit's white.** A webview paints
        white until a document says otherwise, and there are two moments here
        when nothing has: while the splash is still being fetched, and across the
        navigate to the daemon. Both showed as the page in the top-left corner
        with white filling the rest of the window, because the surface is already
        board-sized while the document is not. `background_color` on this builder
        sets the window *and* the webview, so there is no white to flash.
        A see-through window gives that job up: an opaque ground would be the one
        thing the alpha in `--ground` could never see past. The flash it guards
        against becomes a flash of desktop rather than of white, which is the
        trade the setting asks for. */
        .background_color(tauri::window::Color(
            0x10,
            0x10,
            0x10,
            if see_through { 0x00 } else { 0xFF },
        ))
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

/// Start the host off the main thread, navigate the window to it, and stop the
/// first-run server if one was up.
///
/// **The app is the host, and every checkout is a child process.** It used to run
/// `orchd::start` in this process and point the webview at the daemon; now it
/// serves the page itself and spawns one `orchd` per checkout, which is what makes
/// several checkouts possible at all — `multirepo.md` has the argument and the
/// measured cost (2.7 ms and 9.3 MB for the extra process).
///
/// Two things follow from that and are easy to miss. The **instance lock is the
/// child's**, taken inside its own `orchd::start`, so a second app on the same
/// checkout now surfaces as a child that never reported ready rather than as this
/// process refusing to start. And the checkout comes from the child's own config:
/// this function passes a path when it has one (a first-run commit or a switch) and
/// otherwise lets the daemon read `main_checkout` for itself, which is why the row
/// the host records is read back from the child rather than assumed.
///
/// A `std::thread` rather than `rt.spawn` because this blocks on a child's ready
/// line — the same reason the resize nudge is a thread. An error has no terminal to
/// reach, so it lands in the OS dialog `fail` draws, back on the main thread.
/// Reached from a configured boot and from the first-run commit, so it must not
/// assume the window is on any particular page.
fn boot_daemon(
    app_handle: AppHandle,
    rt: tokio::runtime::Handle,
    main: Option<std::path::PathBuf>,
) {
    std::thread::spawn(move || {
        let mut phases = orchd::timing::Phases::start();
        // The checkouts to open. The one the caller just picked, else everything
        // the host file remembers — read here rather than in a child, because the
        // host needs them to key the rows it is about to record.
        let checkouts: Vec<std::path::PathBuf> = match main {
            Some(p) => vec![p],
            None => orchd_serve::host::remembered_checkouts(),
        };
        if checkouts.is_empty() {
            let ah = app_handle.clone();
            let _ = app_handle.run_on_main_thread(move || {
                fail(&ah, "no checkout is configured, and none was picked")
            });
            return;
        }
        // A port of 0: the page's URL is handed to the webview, so nothing has to
        // predict it, and a stale process on a configured port cannot be the
        // difference between an app that opens and one that does not.
        let serving = match rt.block_on(orchd_serve::host::serve(
            orchd_serve::host::mint_token(),
            0,
            CHROME,
        )) {
            Ok(s) => s,
            Err(e) => {
                let ah = app_handle.clone();
                let _ = app_handle.run_on_main_thread(move || fail(&ah, &format!("{e:#}")));
                return;
            }
        };
        phases.mark("host");

        // Attach the window before the page can call it: the process that serves
        // the page is the one that can hold a Tauri handle, and a child daemon
        // never can.
        let control: Arc<dyn WindowControl> = Arc::new(TauriWindow {
            app: app_handle.clone(),
        });
        serving.host.attach_window(control);

        // **A checkout that will not start is not a failed boot.** Every one is
        // tried, each on its own thread, and one bad path leaves the window open
        // on the rest — with N checkouts, refusing to open at all because of one
        // is how a single stale row costs you the app.
        // A first-run pick is a set the file has never seen, so record it before
        // anything can fail: what to open next launch is the decision the person
        // just made, not the subset that happened to start.
        orchd_serve::host::remember_checkouts(&checkouts);
        serving.host.open_remembered(&checkouts);
        if serving.host.checkouts().is_empty() {
            let ah = app_handle.clone();
            let message = format!(
                "none of the {} remembered checkouts would start; see the log",
                checkouts.len()
            );
            let _ = app_handle.run_on_main_thread(move || fail(&ah, &message));
            return;
        }
        phases.mark("daemon");
        tracing::info!(
            "serving {} checkout(s) on port {}",
            serving.host.checkouts().len(),
            serving.host.port
        );
        let url = serving.url();
        *SERVER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(poisoned_is_still_usable) = Some(serving);

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
            let Some(w) = ah.get_webview_window("main") else {
                return;
            };
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
    let Some(want) = rec.monitor.as_ref() else {
        return false;
    };
    let (Ok(gw), Some(display)) = (win.gtk_window(), gtk::gdk::Display::default()) else {
        return false;
    };
    let mut by_name = Vec::new();
    let mut exact = None;
    for i in 0..display.n_monitors() {
        let Some(m) = display.monitor(i) else {
            continue;
        };
        let g = m.geometry();
        if (g.x(), g.y(), g.width(), g.height()) == (want.x, want.y, want.width, want.height) {
            exact = Some(i);
            break;
        }
        if !want.name.is_empty() && m.model().map(|s| s.to_string()).as_deref() == Some(&want.name)
        {
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
    let Ok(pos) = win.inner_position() else {
        return;
    };
    let Ok(monitors) = win.available_monitors() else {
        return;
    };
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
    if let Some(handle) = BOOTSTRAP
        .get()
        .and_then(|b| b.lock().unwrap_or_else(poisoned_is_still_usable).take())
    {
        handle.abort();
    }
}

/// The URL of the running daemon, if there is one. `None` on first run, before any
/// project has been opened.
fn daemon_url() -> Option<String> {
    SERVER.get().and_then(|s| {
        s.lock()
            .unwrap_or_else(poisoned_is_still_usable)
            .as_ref()
            .map(|serving| serving.url())
    })
}

/// Ask for a restart: verify there is a binary to come back as, then close the
/// window so the exit path tears the daemon down and `relaunch` starts the
/// replacement. Called off the main thread (a window command or a bootstrap request),
/// where `close` belongs — wry panics if it is handled on the main thread.
fn request_restart(app: &AppHandle) -> bool {
    match launcher_target() {
        Ok(exe) if exe.exists() => {
            RESTART.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.close();
            }
            true
        }
        Ok(exe) => {
            tracing::error!(
                "not restarting: {} does not exist, sessions kept",
                exe.display()
            );
            false
        }
        Err(e) => {
            tracing::error!("not restarting: no path to this binary ({e:#}), sessions kept");
            false
        }
    }
}

/// The window-side of the first-run flow, handed to [`orchd_serve::firstrun`]'s HTTP
/// server: the native folder dialog, the daemon boot, and the frameless window
/// commands the page's own titlebar needs.
struct TauriBootstrap {
    app: AppHandle,
}

impl orchd_serve::firstrun::BootstrapHost for TauriBootstrap {
    fn pick(&self) -> Option<std::path::PathBuf> {
        pick_folder(&self.app)
    }

    fn open(&self, path: std::path::PathBuf) -> bool {
        if self.switching() {
            // A daemon is already up and the new project's config is written, so a
            // restart brings the app back on it. The current project's live sessions
            // go with the restart — `auto_resume` brings a project's sessions back
            // when you switch to it again. A refusal is reported, so the caller can
            // put the config back: the daemon stays on the old project.
            tracing::info!("switching project — restarting onto {}", path.display());
            request_restart(&self.app)
        } else if BOOTING.swap(true, std::sync::atomic::Ordering::SeqCst) {
            tracing::warn!("a project is already opening; ignoring a second open");
            false
        } else if let Some(rt) = RT.get() {
            boot_daemon(self.app.clone(), rt.clone(), Some(path));
            true
        } else {
            tracing::error!("no runtime to boot the daemon");
            false
        }
    }

    fn window_cmd(&self, cmd: orchd::window::WindowCmd) {
        if let Err(e) = (TauriWindow {
            app: self.app.clone(),
        })
        .dispatch(cmd)
        {
            tracing::warn!("first-run window command failed: {e}");
        }
    }

    /// **Always false now that the header switcher is gone**, and kept anyway.
    ///
    /// The first-run page is reached only at boot with nothing configured, so no
    /// daemon is ever up while it is on screen. It stays because it is a
    /// *defaulted* trait method: dropping the impl compiles silently, and this is
    /// the only branch of [`BootstrapHost::open`](orchd_serve::firstrun::BootstrapHost::open) that reaches `request_restart` — so if a
    /// flow ever puts this page over a running board again, losing it would mean
    /// every open taking the `BOOTING` branch, which is set by the first boot and
    /// never cleared.
    fn switching(&self) -> bool {
        daemon_url().is_some()
    }

    /// The page's own way back, for the same reason [`BootstrapHost::switching`](orchd_serve::firstrun::BootstrapHost::switching) stays.
    fn cancel(&self) {
        let Some(url) = daemon_url() else { return };
        navigate_main(&self.app, url);
        stop_bootstrap();
    }
}

/// The splash page as a `data:` URL.
///
/// Not the daemon's own page, because at this point the daemon is not up yet, and
/// on first run there is no checkout to start one with. The splash is passive: it
/// draws identity while the daemon starts and needs no IPC, so the markup itself
/// is the whole asset. It used to be written to `<config_dir>/splash.html` on
/// every launch and loaded as `file://`, which left a file behind for a page that
/// is on screen for about a second and had a write to fail on before any window
/// existed.
fn splash_url() -> Result<tauri::Url> {
    // Percent-encoded rather than base64, so there is no dependency and the
    // markup stays readable in a debugger. Every byte outside the unreserved set
    // is encoded: `#` (the CSS colours) would otherwise end the URL as a fragment,
    // and `%` would start an escape.
    let mut encoded = String::with_capacity(splash_html().len() * 2);
    for b in splash_html().bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("data:text/html;charset=utf-8,{encoded}")
        .parse()
        .context("the splash data URL")
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
    let args = handoff_args(std::env::args_os().skip(1), std::process::id());
    match std::process::Command::new(&exe).args(&args).spawn() {
        Ok(child) => tracing::info!(pid = child.id(), "restarting"),
        Err(e) => tracing::error!("could not restart: {e}"),
    }
}

/// The successor's argv: our own, minus `argv[0]`, with **one** handoff pair.
///
/// The stripping is the whole point. This used to be
/// `args_os().skip(1).collect()` plus a push, which **keeps** the pair a previous
/// restart handed us — and [`await_handoff`] reads the *first* occurrence. So the
/// second restart in one app lifetime waited on the already-dead grandparent,
/// returned at once, and then died on the live `flock`: no app left at all, from a
/// path that works perfectly the first time.
///
/// Reading the last occurrence would fix the wait and leave argv growing by a pair
/// per restart, so the old one goes instead. Only the two-token form is handled,
/// because that is the only form this function produces and the only one the reader
/// parses.
fn handoff_args(
    argv: impl IntoIterator<Item = std::ffi::OsString>,
    pid: u32,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    let mut skip_value = false;
    for a in argv {
        if skip_value {
            skip_value = false;
            continue;
        }
        if a == std::ffi::OsStr::new(WAIT_FOR_PID) {
            skip_value = true;
            continue;
        }
        args.push(a);
    }
    args.push(WAIT_FOR_PID.into());
    args.push(pid.to_string().into());
    args
}

/// The pid in a successor's argv, if it is one.
///
/// Split out of [`await_handoff`] so the pairing with [`handoff_args`] is testable:
/// these two are one protocol, and it was wrong in exactly the place where only one
/// half was ever read.
fn handoff_pid<'a>(args: impl IntoIterator<Item = &'a String>) -> Option<u32> {
    let args: Vec<&String> = args.into_iter().collect();
    args.iter()
        .position(|a| *a == WAIT_FOR_PID)
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse::<u32>().ok())
}

/// Wait for the process we are replacing to go, if we are a replacement.
///
/// Before anything binds a port or takes the lock. Both are held until the old
/// process actually exits, and its sessions are its children — so starting early
/// would mean two daemons and, worse, two agents in one worktree.
fn await_handoff() {
    let args: Vec<String> = std::env::args().collect();
    let Some(pid) = handoff_pid(&args) else {
        return;
    };
    let deadline = std::time::Instant::now() + HANDOFF_TIMEOUT;
    while orchd::pty::pid_alive(pid) {
        if std::time::Instant::now() > deadline {
            tracing::warn!(
                pid,
                "the process being replaced is still running; starting anyway"
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    tracing::info!(pid, "the process being replaced is gone");
}

/// Stop every checkout's daemon before the process goes.
///
/// **Each child takes its own sessions down**, in its own `Server::shutdown`, and
/// that is the only thing that can: `portable-pty` calls `setsid`, so every agent
/// is its own process group and no sweep from here would find them. So this asks,
/// concurrently and on one deadline, and a child that will not go is reported
/// rather than pretended about.
///
/// No runtime needed any more, which is the quiet simplification of the child
/// model: stopping a process is a signal and a wait, not an async teardown of state
/// this process holds.
fn shutdown() {
    let Some(serving) = SERVER
        .get()
        .and_then(|s| s.lock().unwrap_or_else(poisoned_is_still_usable).take())
    else {
        return;
    };
    serving.host.stop_all();
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

/// Hand the drag to the compositor, refusing it when AppKit is not on a mouse
/// event.
///
/// **This is the one call in the app that can abort the process.** tao's
/// `drag_window` reads `[NSApp currentEvent]` and passes it to
/// `performWindowDragWithEvent:`, which accepts nothing but a mouse event —
/// anything else raises an Objective-C exception, and an uncaught one takes the
/// process, the daemon and every session with it.
///
/// tao does try to substitute a synthetic mouse-down, and **its guard does not
/// fire**: `tao-0.35.3` compares the event type against `0x15`, which is 21, while
/// `NSEventTypeApplicationDefined` is 15. So the substitution is dead code and
/// essentially every call reaches AppKit with whatever event is current. Read from
/// the vendored source, not inferred — and worth knowing before anyone concludes
/// the page-side guard is redundant.
///
/// The page only asks once the pointer has moved, which keeps the request inside a
/// gesture; but the request crosses HTTP, so it can still arrive after the gesture
/// ended. This is the half that closes it: the check and the call happen together,
/// on the main thread, with no queue between them.
///
/// **On the main thread because both halves need it.** `[NSApp currentEvent]` is
/// only meaningful there, and `dispatch` runs on an axum worker — so a check made
/// here and a call made there would be answering about two different moments.
#[cfg(target_os = "macos")]
fn start_dragging(app: &AppHandle, _window: &tauri::WebviewWindow) -> Result<()> {
    let app = app.clone();
    app.clone()
        .run_on_main_thread(move || {
            if !on_a_mouse_event() {
                // Not an error worth a toast: the gesture is simply over, and the
                // press that started it is not owed a window drag any more.
                tracing::debug!("ignoring a window drag: AppKit is not on a mouse event");
                return;
            }
            let Some(w) = app.get_webview_window("main") else {
                return;
            };
            if let Err(e) = w.start_dragging() {
                tracing::warn!("the window drag was refused: {e}");
            }
        })
        .context("asking the main thread for a window drag")
}

#[cfg(not(target_os = "macos"))]
fn start_dragging(_app: &AppHandle, window: &tauri::WebviewWindow) -> Result<()> {
    // No AppKit, no exception: GTK takes the drag from whichever thread asks.
    window.start_dragging().map_err(Into::into)
}

/// Whether the event AppKit is dispatching right now is a mouse event.
///
/// The full list rather than "left button only": a drag can legitimately begin on
/// a dragged or moved event, and `performWindowDragWithEvent:` accepts any of
/// them. What it must never see is a key, a gesture, or one of AppKit's own
/// synthetic types — [`start_dragging`] has what that costs.
///
/// No `unsafe`: `sharedApplication`, `currentEvent` and `type` are all safe in
/// `objc2-app-kit`, so this crate keeps the workspace's `unsafe_code = deny`.
/// `MainThreadMarker::new` answering `None` means we are not on the main thread,
/// where the question has no meaningful answer — so the drag is refused.
#[cfg(target_os = "macos")]
fn on_a_mouse_event() -> bool {
    use objc2_app_kit::{NSApplication, NSEventType};

    let Some(mtm) = objc2::MainThreadMarker::new() else {
        return false;
    };
    let Some(event) = NSApplication::sharedApplication(mtm).currentEvent() else {
        return false;
    };
    let kind = event.r#type();
    kind == NSEventType::LeftMouseDown
        || kind == NSEventType::LeftMouseUp
        || kind == NSEventType::LeftMouseDragged
        || kind == NSEventType::RightMouseDown
        || kind == NSEventType::RightMouseUp
        || kind == NSEventType::RightMouseDragged
        || kind == NSEventType::OtherMouseDown
        || kind == NSEventType::OtherMouseUp
        || kind == NSEventType::OtherMouseDragged
        || kind == NSEventType::MouseMoved
}

/// Raise the native folder dialog and wait for the answer.
///
/// One spelling for the two seams that need it — the first-run page's `pick` and
/// the host's `add a checkout` — because they ask the same question and a second
/// copy is where the title and the path handling drift apart.
///
/// Called from a request thread, never the GTK main thread: the plugin marshals
/// the dialog to the main thread and calls back here, so blocking on the answer is
/// safe and blocking on the main thread would deadlock.
fn pick_folder(app: &AppHandle) -> Option<std::path::PathBuf> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog()
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

impl WindowControl for TauriWindow {
    /// The same dialog the first-run page raises, on the same terms: this runs on
    /// an axum blocking thread, and the plugin marshals the dialog to the main
    /// thread and calls back here.
    fn pick_folder(&self) -> Option<std::path::PathBuf> {
        pick_folder(&self.app)
    }

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
            WindowCmd::Restart => {
                request_restart(&self.app);
            }
            WindowCmd::StartDrag => start_dragging(&self.app, &w)?,
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
    use std::ffi::OsString;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    /// The pair a previous restart left behind is replaced, not appended to.
    ///
    /// The bug this pins: the successor reads the **first** `--wait-for-pid`, so a
    /// kept pair means the second restart waits on the grandparent, which is
    /// already dead — it starts at once and dies on the lock the parent still
    /// holds.
    #[test]
    fn a_second_restart_waits_for_the_parent_and_not_the_grandparent() {
        let first = handoff_args(os(&["--main", "/repo"]), 111);
        assert_eq!(first, os(&["--main", "/repo", "--wait-for-pid", "111"]));

        let second = handoff_args(first, 222);
        assert_eq!(second, os(&["--main", "/repo", "--wait-for-pid", "222"]));
        assert_eq!(
            second.iter().filter(|a| *a == "--wait-for-pid").count(),
            1,
            "argv grew a pair per restart"
        );

        let read: Vec<String> = second
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            handoff_pid(&read),
            Some(222),
            "the reader took the stale pid"
        );
    }

    /// Everything else the app was started with survives a restart, which is why
    /// the argv is rebuilt rather than replaced.
    #[test]
    fn a_restart_keeps_the_arguments_it_was_started_with() {
        let args = handoff_args(
            os(&["--main", "/repo", "--wait-for-pid", "9", "--flag"]),
            42,
        );
        assert_eq!(
            args,
            os(&["--main", "/repo", "--flag", "--wait-for-pid", "42"])
        );
    }

    /// A plain launch is not a handoff, and must not wait for anything.
    #[test]
    fn an_ordinary_launch_has_no_pid_to_wait_for() {
        let plain: Vec<String> = ["orchestrator-desktop", "--main", "/repo"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(handoff_pid(&plain), None);
        // A flag with nothing after it is a malformed argv, not a pid of 0.
        let truncated: Vec<String> = ["x", "--wait-for-pid"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(handoff_pid(&truncated), None);
    }
}
