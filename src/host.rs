//! The host: the page, the window, and the list of checkouts.
//!
//! **The host is not a daemon.** A daemon manages exactly one checkout — its
//! sessions, its worktrees, its git, its PRs — and knows nothing about any other.
//! The host serves the one page, owns the one window, and knows which checkouts
//! are open. Those were the same thing while there was one checkout, and `orchd`
//! was built that way; `multirepo.md` has the argument for separating them and the
//! measurements behind it.
//!
//! Nothing here knows what a session is, and that is the boundary to keep. A host
//! that learned the session model would be the conflation coming back.
//!
//! **What this module is today**: the routes, the window handle and a checkout
//! list with one entry, mounted into the daemon's own router by [`crate::start`].
//! One process, one port, one token — so nothing behaves differently yet, and the
//! routes now have one owner instead of sitting among the session API. What comes
//! next is the same module served by the app for N child daemons, at which point
//! each child gets its own port and mints its own token and this list grows.
//!
//! Two things the eventual shape needs that are visible here already. The page
//! carries the checkout list rather than fetching it, because a page cannot ask
//! for a token it has not been given — the same reason `GET /` has never been
//! token-gated. And `/api/host/checkouts` exists beside that substitution rather
//! than instead of it, because a checkout added after the page loaded, or one that
//! restarted and minted a new token, cannot be in a page that was already served.

use axum::extract::{Path as UrlPath, State};
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::window::WindowControl;

/// One open checkout, as the page needs to see it.
///
/// `token` is in here because the page holds one per checkout: every call names
/// the daemon it is for, and each daemon compares the token against its own. It is
/// the same value as the host's while one process serves both.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/snapshot.d.ts"))]
pub struct Checkout {
    /// Canonical path, which is also the identity: `main_checkout` is resolved in
    /// [`crate::config::Config::parse`], and comparing an unresolved path against
    /// a resolved one silently matches nothing.
    pub path: String,
    /// The last path component, for a row and a log line. Not a key — two
    /// checkouts can share a leaf.
    pub name: String,
    /// Where its daemon answers.
    pub port: u16,
    /// What that daemon wants in `x-orch-token`.
    pub token: String,
    /// False for a checkout whose daemon is down. Always true while the only
    /// daemon is the process serving this page: a host cannot outlive itself.
    pub live: bool,
}

/// What the host owns.
pub struct Host {
    /// The page's token. One per host; a child daemon mints its own and reports
    /// it, which is why [`Checkout`] carries one rather than the page assuming
    /// this value everywhere.
    pub token: String,
    /// The port the page is served on, for the Host and Origin rules.
    pub port: u16,
    /// The native window, when there is one.
    ///
    /// **Here rather than on `AppState`**, which is where it lived: the titlebar
    /// drives the window over authenticated HTTP, and the process that serves the
    /// page is the one that can hold a Tauri handle. A checkout's daemon is about
    /// to stop being that process, and a handle it could never use is a field that
    /// reads as "this daemon might have a window".
    window: RwLock<Option<Arc<dyn WindowControl>>>,
    /// Every open checkout. One entry today.
    checkouts: RwLock<Vec<Checkout>>,
    /// Which key the app's own chords wear, and whether the page draws its own
    /// titlebar. Told to the page, never sniffed.
    chrome: crate::window::Chrome,
}

impl Host {
    pub fn new(token: String, port: u16, chrome: crate::window::Chrome) -> Arc<Self> {
        Arc::new(Host {
            token,
            port,
            window: RwLock::new(None),
            checkouts: RwLock::new(Vec::new()),
            chrome,
        })
    }

    /// Give the host its native window. Called once, by whichever process has one.
    pub async fn attach_window(&self, control: Arc<dyn WindowControl>) {
        *self.window.write().await = Some(control);
    }

    /// Add or replace a checkout's entry, keyed on the path.
    ///
    /// Replace rather than push, because a restarted daemon is the same checkout
    /// on a new port with a new token, and two rows for one path is the shape
    /// where the page picks whichever it happened to render.
    pub async fn record(&self, checkout: Checkout) {
        let mut open = self.checkouts.write().await;
        match open.iter_mut().find(|c| c.path == checkout.path) {
            Some(existing) => *existing = checkout,
            None => open.push(checkout),
        }
    }

    pub async fn checkouts(&self) -> Vec<Checkout> {
        self.checkouts.read().await.clone()
    }
}

/// The Host, Origin and token rules, on the host's own port.
///
/// The policy comes from [`crate::api`] rather than being restated: `host_allowed`
/// and `origin_ok` are the two functions with the tests around them, and a second
/// spelling of one rule is how the two halves of a guard drift apart. What differs
/// is only which arms can apply — nothing here is a hook, and no agent calls the
/// host, so a request with no Origin passes on the same two grounds a page has: it
/// is a GET, or it carries the token.
async fn guard(State(host): State<Arc<Host>>, req: Request<axum::body::Body>, next: Next) -> Response {
    let headers = req.headers();
    let host_header = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !crate::api::host_allowed(host_header, host.port) {
        return (StatusCode::FORBIDDEN, "bad host").into_response();
    }
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    let token_ok = headers
        .get("x-orch-token")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t == host.token);
    let is_get = req.method() == axum::http::Method::GET;
    if !crate::api::origin_ok(origin, host.port, false, is_get, token_ok) {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }
    // The page itself is deliberately not token-gated: it is where the token comes
    // from. Everything that changes something is.
    if !is_get && !token_ok {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    next.run(req).await
}

pub fn router(host: Arc<Host>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/review-preview", get(review_preview))
        .route("/app.js", get(asset_js))
        .route("/app.css", get(asset_css))
        .route("/js/:file", get(module))
        .route("/vendor/:file", get(vendor))
        .route("/vendor/fonts/:file", get(font))
        .route("/api/host/checkouts", get(checkouts))
        .route("/api/window/resize/:edge", post(window_resize))
        .route("/api/window/:cmd", post(window_cmd))
        .layer(axum::middleware::from_fn_with_state(host.clone(), guard))
        .with_state(host)
}

// --- the page ---------------------------------------------------------------

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");
const REVIEW_PREVIEW: &str = include_str!("../web/review-preview.html");

/// The substitutions every served page gets.
///
/// The token is embedded rather than fetched, so it never exists as a value
/// another origin could ask for. The checkout list rides along for the same
/// reason: the page's first call already has to name a checkout, and a fetch to
/// find out which would need a credential the page does not have yet.
async fn page(host: &Arc<Host>, template: &str) -> String {
    let checkouts = serde_json::to_string(&host.checkouts().await).unwrap_or_else(|_| "[]".into());
    template
        .replace("__ORCH_TOKEN__", &host.token)
        .replace("__ORCH_CHROME__", host.chrome.as_str())
        .replace("__ORCH_CHECKOUTS__", &checkouts)
        // ⌘ on a Mac, Ctrl elsewhere. Told rather than sniffed — the host knows at
        // compile time, and `navigator.platform` is both deprecated and a lie
        // under a webview.
        .replace("__ORCH_PLATFORM__", if cfg!(target_os = "macos") { "mac" } else { "other" })
}

async fn index(State(host): State<Arc<Host>>) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store, must-revalidate")],
        Html(page(&host, INDEX).await),
    )
        .into_response()
}

/// The review-overlay preview page, substituted the same way, because the module
/// graph reads `window.__ORCH__` at import time.
async fn review_preview(State(host): State<Arc<Host>>) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store, must-revalidate")],
        Html(page(&host, REVIEW_PREVIEW).await),
    )
        .into_response()
}

/// Serve a static asset with its real type and no caching.
///
/// `no-store` matters more than it looks: the SPA is baked into the binary with
/// `include_str!`, so a cached bundle silently shadows a rebuilt host and you
/// debug code that is not running. Found exactly that way.
fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store, must-revalidate"),
        ],
        body,
    )
        .into_response()
}

async fn asset_js() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}

async fn asset_css() -> Response {
    asset("text/css; charset=utf-8", APP_CSS)
}

/// The SPA's own ES modules.
///
/// A flat, known set exactly like [`vendor`]: no traversal, and the compiled-in
/// file is the only thing servable. The content type must be a JavaScript one or a
/// `type="module"` script fetches it and then refuses to run it.
///
/// **Each new module needs a line here and a rebuild**, which is why the modules
/// track features rather than being cut finer.
async fn module(UrlPath(file): UrlPath<String>) -> Response {
    if file.contains('/') || file.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad asset").into_response();
    }
    let body = match file.as_str() {
        "core.js" => include_str!("../web/js/core.js"),
        "term.js" => include_str!("../web/js/term.js"),
        "rail.js" => include_str!("../web/js/rail.js"),
        "diff.js" => include_str!("../web/js/diff.js"),
        "review.js" => include_str!("../web/js/review.js"),
        "review-diff.js" => include_str!("../web/js/review-diff.js"),
        "queue.js" => include_str!("../web/js/queue.js"),
        "settings.js" => include_str!("../web/js/settings.js"),
        _ => return (StatusCode::NOT_FOUND, "no such module").into_response(),
    };
    asset("text/javascript; charset=utf-8", body)
}

/// xterm's own dist files, copied in at build time.
async fn vendor(UrlPath(file): UrlPath<String>) -> Response {
    // No path traversal: only a flat, known set of filenames is served.
    if file.contains('/') || file.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad asset").into_response();
    }
    let body = match file.as_str() {
        "xterm.js" => include_str!("../web/vendor/xterm.js"),
        "xterm.css" => include_str!("../web/vendor/xterm.css"),
        "addon-fit.js" => include_str!("../web/vendor/addon-fit.js"),
        "addon-webgl.js" => include_str!("../web/vendor/addon-webgl.js"),
        // All Prism grammars, dependency-ordered, for diff/open-question
        // highlighting. Vendored whole rather than fetched: the host owns its
        // assets and must work offline, wherever the repo lives.
        "prism.min.js" => include_str!("../web/vendor/prism.min.js"),
        _ => return (StatusCode::NOT_FOUND, "no such asset").into_response(),
    };
    let ct = if file.ends_with(".css") {
        "text/css; charset=utf-8"
    } else {
        "text/javascript; charset=utf-8"
    };
    asset(ct, body)
}

/// Webfonts, baked in like everything else.
///
/// A desktop app that reaches out to fonts.googleapis.com on every launch is one
/// flaky DNS lookup away from rendering in Times New Roman, and it tells a third
/// party when you start work. These are bytes, not text, so they cannot go through
/// [`asset`].
async fn font(UrlPath(file): UrlPath<String>) -> Response {
    if file.contains('/') || file.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad asset").into_response();
    }
    // Plex Sans and Martian Mono ship as variable fonts, so one file covers every
    // weight the UI asks for. Plex Mono is still static per weight.
    let body: &'static [u8] = match file.as_str() {
        "plex-sans.woff2" => include_bytes!("../web/vendor/fonts/plex-sans.woff2"),
        "plex-mono-400.woff2" => include_bytes!("../web/vendor/fonts/plex-mono-400.woff2"),
        "plex-mono-500.woff2" => include_bytes!("../web/vendor/fonts/plex-mono-500.woff2"),
        "plex-mono-600.woff2" => include_bytes!("../web/vendor/fonts/plex-mono-600.woff2"),
        "martian-mono.woff2" => include_bytes!("../web/vendor/fonts/martian-mono.woff2"),
        // Diffs only, and only the one weight they use.
        "jetbrains-mono-400.woff2" => {
            include_bytes!("../web/vendor/fonts/jetbrains-mono-400.woff2")
        }
        _ => return (StatusCode::NOT_FOUND, "no such asset").into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            // Immutable, unlike the SPA: these never change without a rebuild that
            // also changes the filename set.
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
        .into_response()
}

// --- the checkout list ------------------------------------------------------

async fn checkouts(State(host): State<Arc<Host>>) -> Json<serde_json::Value> {
    Json(json!({ "checkouts": host.checkouts().await }))
}

// --- the window -------------------------------------------------------------

/// Drive the window the page is displayed in.
///
/// A mutating route, so it carries the token like every other one — which is the
/// point of doing this over HTTP instead of Tauri's IPC: the page's origin is a
/// localhost URL on a port chosen at bind time, and granting IPC to
/// `http://127.0.0.1:*` would hand window control to anything else that managed to
/// get itself loaded there.
async fn window_cmd(State(host): State<Arc<Host>>, UrlPath(cmd): UrlPath<String>) -> Response {
    match crate::api::parse_window_cmd(&cmd) {
        Some(parsed) => dispatch(&host, parsed).await,
        None => refusal(&format!("no such window command: {cmd}")),
    }
}

/// Resize takes an edge, so it gets its own route rather than bending the command
/// enum into something that serialises from a single word.
async fn window_resize(State(host): State<Arc<Host>>, UrlPath(edge): UrlPath<String>) -> Response {
    match crate::api::parse_resize_edge(&edge) {
        Some(parsed) => dispatch(&host, crate::window::WindowCmd::StartResize(parsed)).await,
        None => refusal(&format!("no such resize edge: {edge}")),
    }
}

/// The same shape `api::ApiError` produces, so the SPA's `call` reads a refusal
/// here exactly as it reads one from a daemon.
fn refusal(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

async fn dispatch(host: &Arc<Host>, cmd: crate::window::WindowCmd) -> Response {
    let control = host.window.read().await.clone();
    let Some(control) = control else {
        // Running in a browser tab. The tab has its own chrome; this is not an
        // error worth a toast, but it is not a success either.
        return refusal("no native window attached");
    };
    match control.dispatch(cmd) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => refusal(&format!("{e:#}")),
    }
}
