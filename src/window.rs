//! The seam between the daemon and whatever window is showing it.
//!
//! The SPA is served over http on 127.0.0.1, which Tauri treats as a *remote*
//! origin: `window.__TAURI__` and the whole IPC bridge are gated behind a
//! capability that has to name the origin, port and all. Our port is decided at
//! bind time and can fall back to an ephemeral one, so that origin is not
//! knowable when the capability is compiled in.
//!
//! Rather than fight that, the SPA asks the daemon — same origin, same token,
//! same code path as every other button in the UI — and the daemon asks the
//! window through this trait. The desktop crate implements it against Tauri's
//! Rust window API; headless orchd leaves it unset and the routes 404, which is
//! exactly right for a browser tab that has no window to control.

use serde::Deserialize;

/// What the chrome in the top bar can ask of its window.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowCmd {
    Minimize,
    ToggleMaximize,
    Close,
    /// Hand the drag to the compositor. Sent on mousedown in the top bar, which
    /// is the one command that must arrive while a mouse button is still held.
    StartDrag,
    /// Same, for the invisible strips along the window edges.
    ///
    /// A frameless window loses the WM's resize border along with the rest of
    /// the decorations, so the app has to put it back or the window is stuck at
    /// whatever size it opened.
    StartResize(ResizeEdge),
}

/// Which edge or corner the pointer grabbed.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResizeEdge {
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
    NorthWest,
}

/// Implemented by the desktop shell.
///
/// `Send + Sync` because the caller is an axum handler on some tokio worker,
/// not the UI thread; implementations are responsible for getting themselves
/// back to the main thread if their toolkit demands it.
pub trait WindowControl: Send + Sync {
    fn dispatch(&self, cmd: WindowCmd) -> anyhow::Result<()>;
}

/// How the window wants its chrome drawn, handed to the SPA at page load.
///
/// This is a rendering instruction, not a platform check: the SPA has no
/// business sniffing the user agent to decide where the close button goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chrome {
    /// A browser tab. The OS draws nothing; neither do we.
    None,
    /// Frameless. We draw minimise/maximise/close at the right of the top bar.
    Custom,
    /// macOS with an overlay titlebar: the real traffic lights are floating
    /// over our top-left, so we draw no buttons and inset the left instead.
    Overlay,
}

impl Chrome {
    pub fn as_str(self) -> &'static str {
        match self {
            Chrome::None => "none",
            Chrome::Custom => "custom",
            Chrome::Overlay => "overlay",
        }
    }
}
