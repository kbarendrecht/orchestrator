//! A workspace's HTML, served to the file pane's preview frame.
//!
//! **The page cannot render HTML itself, and that is deliberate.** It carries the
//! app token on `window.__ORCH__`, no CSP applies to it, and ESLint refuses every
//! HTML sink for that reason (`tools/eslint.config.mjs`). So a preview is an
//! iframe with `sandbox="allow-scripts"` and no `allow-same-origin`: its scripts
//! run in an opaque origin that can read neither the page nor the token, and every
//! API call it makes arrives with `Origin: null`, which [`crate::api::guard`]
//! refuses.
//!
//! **The frame loads from here rather than from `srcdoc`**, so the page's own
//! `<link href="app.css">` and `<script src="app.js">` resolve. The token is in
//! the *path* for the same reason: a relative URL keeps the path and drops the
//! query. The frame's scripts can read it off `location`, so it grants nothing
//! the app token does — only reads, only in one workspace, and only files that
//! pass [`servable`].

use axum::extract::{Json, Path as AxPath, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

use crate::api::{refuse, ApiError, ApiResult};
use crate::model::PreviewGrant;
use crate::state::AppState;

/// Largest file the route serves. A page's assets, not a video archive.
const MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Deserialize)]
pub struct OpenBody {
    pub workspace: String,
    pub path: String,
}

#[derive(Serialize)]
pub struct Opened {
    /// The page builds the URL, `/preview/<token>/<path>`, because it already
    /// encodes paths segment by segment and the daemon would only do it again.
    pub token: String,
}

/// Mint (or reuse) the token that lets the frame read this page's workspace.
pub async fn open(State(app): State<Arc<AppState>>, Json(b): Json<OpenBody>) -> ApiResult<Opened> {
    let lower = b.path.to_ascii_lowercase();
    if !(lower.ends_with(".html") || lower.ends_with(".htm")) {
        refuse!("only an .html file can be previewed");
    }
    let Some(root) = app.workspace_path(&b.workspace).await else {
        refuse!("unknown workspace {}", b.workspace);
    };
    let (page, shared) = (b.path.clone(), app.cfg.shared_worktree_paths.clone());
    let page_ignored = crate::proc::run_blocking("checking a page to preview", move || {
        let at = crate::edit::resolve_in_workspace(&root, &page, &shared)?;
        if !at.is_file() {
            anyhow::bail!("{page} is not a file");
        }
        anyhow::Ok(crate::search::is_ignored(&root, &page))
    })
    .await??;

    let grant = PreviewGrant {
        workspace: b.workspace,
        page: b.path,
        page_ignored,
    };
    let mut previews = app.previews.lock().unwrap_or_else(|e| e.into_inner());
    // One token per page, so opening the same file again is not a new entry.
    let token = previews
        .iter()
        .find(|(_, g)| **g == grant)
        .map(|(t, _)| t.clone())
        .unwrap_or_else(|| {
            let t = crate::secret::random_token();
            previews.insert(t.clone(), grant);
            t
        });
    Ok(Json(Opened { token }))
}

/// One file of a previewed page.
///
/// A refusal is a bare `404` with no reason: the reader is a script in a sandbox,
/// and telling it *why* a path was refused tells it what exists.
pub async fn serve(
    State(app): State<Arc<AppState>>,
    AxPath((token, rel)): AxPath<(String, String)>,
) -> Response {
    let grant = {
        let previews = app.previews.lock().unwrap_or_else(|e| e.into_inner());
        previews.get(&token).cloned()
    };
    let Some(grant) = grant else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(root) = app.workspace_path(&grant.workspace).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let shared = app.cfg.shared_worktree_paths.clone();
    let path = rel.clone();
    let read = crate::proc::run_blocking("serving a preview file", move || {
        if !servable(&path, &grant, |p| crate::search::is_ignored(&root, p)) {
            return None;
        }
        let at = crate::edit::resolve_in_workspace(&root, &path, &shared).ok()?;
        let md = std::fs::metadata(&at).ok()?;
        if !md.is_file() || md.len() > MAX_BYTES {
            return None;
        }
        std::fs::read(&at).ok()
    })
    .await;
    let Ok(Some(bytes)) = read else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&rel)),
    );
    /* **Sandboxed by the response too, not only by the frame.** Opened in a tab of
    its own, a preview URL would otherwise run with this daemon's origin, where a
    `fetch` sends a same-origin Origin and the guard lets it through. */
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox allow-scripts"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // An edit and a reload should show the edit.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    /* A `<script type="module">` loads in CORS mode, and from an opaque origin it
    asks as `null`. `*` rather than `null`, because a sandboxed frame is not the
    only opaque origin and this carries no credentials anyway. */
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    response
}

#[derive(Deserialize)]
pub struct ImageQuery {
    pub workspace: String,
    pub path: String,
}

/// One image, for the file pane's `<img>`.
///
/// **Here rather than in `/api/file`**, which answers JSON with text in it and
/// refuses a binary file on purpose. An `<img>` cannot send the app token, and it
/// needs none: this is a GET, which the guard serves without one, the same as the
/// text the pane already reads.
///
/// **Images only, and sandboxed.** The page is trusted, so this has none of
/// [`servable`]'s rules; what it must not be is a way to serve a workspace's HTML
/// from this daemon's origin. An SVG opened in a tab of its own runs its scripts,
/// so every answer carries a CSP that forbids them, and any other type is a 404.
pub async fn image(State(app): State<Arc<AppState>>, Query(q): Query<ImageQuery>) -> Response {
    let kind = content_type(&q.path);
    if !kind.starts_with("image/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(root) = app.workspace_path(&q.workspace).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let shared = app.cfg.shared_worktree_paths.clone();
    let read = crate::proc::run_blocking("reading an image", move || {
        let at = crate::edit::resolve_in_workspace(&root, &q.path, &shared).ok()?;
        let md = std::fs::metadata(&at).ok()?;
        if !md.is_file() || md.len() > MAX_BYTES {
            return None;
        }
        std::fs::read(&at).ok()
    })
    .await;
    let Ok(Some(bytes)) = read else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // A screenshot an agent just rewrote is the one you are opening it to see.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Whether a preview may read `rel`, before it is resolved on disk.
///
/// **Not the whole workspace**, because the frame runs the page's scripts and a
/// script can send what it reads anywhere — a CSP cannot stop a frame navigating
/// itself to `https://…?secret`. So a dot anywhere in the path is refused (`.env`,
/// `.git`, `..`), and so is anything git ignores, which is where a repo keeps what
/// must not be committed. Unless the page is ignored itself: that is build output,
/// `dist/index.html`, and its own directory is served, or it could not load its
/// own bundle.
fn servable(rel: &str, grant: &PreviewGrant, ignored: impl Fn(&str) -> bool) -> bool {
    if rel.is_empty() || rel.split('/').any(|c| c.is_empty() || c.starts_with('.')) {
        return false;
    }
    if !ignored(rel) {
        return true;
    }
    let dir = Path::new(&grant.page).parent().unwrap_or(Path::new(""));
    grant.page_ignored && Path::new(rel).starts_with(dir)
}

/// The type a browser needs to use the file. `nosniff` means a wrong or missing
/// one is refused rather than guessed, so the common web types are all here.
fn content_type(rel: &str) -> &'static str {
    let ext = Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(page: &str, page_ignored: bool) -> PreviewGrant {
        PreviewGrant {
            workspace: "wt".into(),
            page: page.into(),
            page_ignored,
        }
    }

    #[test]
    fn a_dot_anywhere_is_refused() {
        let g = grant("site/index.html", false);
        let none = |_: &str| false;
        assert!(servable("site/app.css", &g, none));
        assert!(
            servable("assets/logo.png", &g, none),
            "the rest of the workspace"
        );
        for bad in [
            ".env",
            "site/.env",
            ".git/config",
            "../x",
            "site/../.env",
            "a//b",
            "",
        ] {
            assert!(!servable(bad, &g, none), "{bad}");
        }
    }

    #[test]
    fn an_ignored_file_is_refused_unless_the_page_is_build_output() {
        let ignored = |p: &str| p.starts_with("dist/") || p == "secrets.json";
        let tracked = grant("site/index.html", false);
        assert!(!servable("secrets.json", &tracked, ignored));
        assert!(!servable("dist/app.js", &tracked, ignored));

        let built = grant("dist/index.html", true);
        assert!(servable("dist/app.js", &built, ignored), "its own bundle");
        assert!(servable("dist/chunks/a.js", &built, ignored));
        assert!(
            !servable("secrets.json", &built, ignored),
            "not every ignored file"
        );
    }
}
