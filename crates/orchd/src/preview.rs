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
    /* **The checkout, not a list of shared directories.** This took
    `shared_worktree_paths` — the per-repo exception list that let a symlink out
    of a worktree stay writable — and that setting is gone: `resolve_in_workspace`
    bounds at the *checkout* now, so a path shared in from main resolves inside
    the bound without anybody naming it (#34). Preview reads rather than writes,
    and the bound it wants is the same one. */
    let (page, shared) = (b.path.clone(), app.cfg.main_checkout.clone());
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
    // The checkout is the bound; see `open` above.
    let shared = app.cfg.main_checkout.clone();
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
    // The checkout is the bound; see `open` above.
    let shared = app.cfg.main_checkout.clone();
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

#[derive(Deserialize)]
pub struct ScratchpadQuery {
    pub session: uuid::Uuid,
    /// Absolute, as the agent printed it.
    pub path: String,
}

/// One image out of a session's Claude Code scratchpad, for the file pane.
///
/// **Outside every workspace, so the workspace bound cannot answer for it.** An
/// agent writes its screenshots to `<tmp>/claude-<uid>/<slug>/<session>/scratchpad`,
/// prints that path, and the path was refused before it was ever underlined. This
/// serves that one folder for the session it belongs to and nothing else: the
/// file is resolved first, links and all, and only then checked against
/// [`in_scratchpad`], so a link inside the scratchpad pointing out of it is a 404.
///
/// The layout is Claude Code's, not a contract. If it moves, these links stop
/// working; they cannot start reading anything else. Images only, with the CSP
/// [`image`] sends, for the reasons given there.
pub async fn scratchpad_image(
    State(app): State<Arc<AppState>>,
    Query(q): Query<ScratchpadQuery>,
) -> Response {
    let kind = content_type(&q.path);
    if !kind.starts_with("image/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let cwd = {
        let inner = app.inner.read().await;
        inner.sessions.get(&q.session).map(|s| s.cwd.clone())
    };
    let Some(cwd) = cwd else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let session = q.session;
    let read = crate::proc::run_blocking("reading a scratchpad image", move || {
        let at = std::fs::canonicalize(&q.path).ok()?;
        /* Both bases resolved the way the file was. On macOS `/tmp` is a link to
        `/private/tmp`, so a resolved file never started with the plain `/tmp`
        and its own image answered 404, caught by `check` on macos-14. */
        let bases: Vec<std::path::PathBuf> = [std::env::temp_dir(), "/tmp".into()]
            .iter()
            .filter_map(|b| std::fs::canonicalize(b).ok())
            .collect();
        if !in_scratchpad(&at, &bases, &cwd, session) {
            return None;
        }
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
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Whether `at`, already resolved, is inside this session's scratchpad:
/// `<tmp>/claude-<digits>/<slug of cwd>/<session>/scratchpad/…`.
///
/// Pure, so the shape is tested rather than trusted. `bases` are the resolved temp
/// folder and the resolved `/tmp`: Claude Code puts it there even on a machine
/// whose `TMPDIR` points elsewhere.
pub fn in_scratchpad(
    at: &Path,
    bases: &[std::path::PathBuf],
    cwd: &Path,
    session: uuid::Uuid,
) -> bool {
    let slug = crate::config::transcript_slug(cwd);
    bases.iter().any(|base| {
        let Ok(rest) = at.strip_prefix(base) else {
            return false;
        };
        let mut parts = rest.components().map(|c| c.as_os_str().to_string_lossy());
        let user = parts.next();
        let ok_user = user
            .as_deref()
            .and_then(|u| u.strip_prefix("claude-"))
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        ok_user
            && parts.next().as_deref() == Some(slug.as_str())
            && parts.next().as_deref() == Some(session.to_string().as_str())
            && parts.next().as_deref() == Some("scratchpad")
            && parts.next().is_some()
    })
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

    /// The one folder a scratchpad link may read: this session's, under the temp
    /// folder, below `scratchpad/`. Each refusal is a path an agent could print.
    #[test]
    fn only_the_sessions_own_scratchpad_is_readable() {
        let id = uuid::Uuid::parse_str("f8452082-4dc5-4a96-836f-02f7ee3a2250").unwrap();
        let other = uuid::Uuid::parse_str("2b0cf0c7-dc0a-4dac-832d-ee69ac9b0478").unwrap();
        let cwd = Path::new("/home/k/dev/scienta/.claude/worktrees/story-53860");
        let tmp = [
            std::path::PathBuf::from("/var/tmp-elsewhere"),
            std::path::PathBuf::from("/tmp"),
        ];
        let tmp = tmp.as_slice();
        let slug = "-home-k-dev-scienta--claude-worktrees-story-53860";
        let at = |s: &str| std::path::PathBuf::from(s);
        let mine = format!("/tmp/claude-1000/{slug}/{id}/scratchpad/kanban.png");
        assert!(
            in_scratchpad(&at(&mine), tmp, cwd, id),
            "its own file, under /tmp"
        );
        let via_tmpdir = format!("/var/tmp-elsewhere/claude-1000/{slug}/{id}/scratchpad/a.png");
        assert!(
            in_scratchpad(&at(&via_tmpdir), tmp, cwd, id),
            "under TMPDIR too"
        );
        assert!(
            !in_scratchpad(&at(&mine), tmp, cwd, other),
            "another session's scratchpad"
        );
        let wrong_slug = format!("/tmp/claude-1000/-home-k-other/{id}/scratchpad/a.png");
        assert!(
            !in_scratchpad(&at(&wrong_slug), tmp, cwd, id),
            "another project"
        );
        let not_scratch = format!("/tmp/claude-1000/{slug}/{id}/tasks/out.png");
        assert!(
            !in_scratchpad(&at(&not_scratch), tmp, cwd, id),
            "beside the scratchpad"
        );
        let bare = format!("/tmp/claude-1000/{slug}/{id}/scratchpad");
        assert!(
            !in_scratchpad(&at(&bare), tmp, cwd, id),
            "the folder itself is no file"
        );
        let user = format!("/tmp/claude-x/{slug}/{id}/scratchpad/a.png");
        assert!(!in_scratchpad(&at(&user), tmp, cwd, id), "not a uid");
        assert!(!in_scratchpad(&at("/etc/passwd"), tmp, cwd, id));
        // macOS: `/tmp` resolves to `/private/tmp`, and the route resolves both
        // the file and its bases, so the two still agree.
        let mac = [std::path::PathBuf::from("/private/tmp")];
        let resolved = format!("/private/tmp/claude-501/{slug}/{id}/scratchpad/kanban.png");
        assert!(
            in_scratchpad(&at(&resolved), &mac, cwd, id),
            "a resolved /tmp"
        );
    }

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
