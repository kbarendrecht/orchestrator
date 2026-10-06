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
    let mut response = image_response(kind, bytes);
    let headers = response.headers_mut();
    // A screenshot an agent just rewrote is the one you are opening it to see.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// An image answer, sandboxed: a CSP that forbids scripts (an SVG opened in a tab
/// of its own runs them) and no type sniffing. Shared by both image routes, so
/// the two cannot drift apart on the one thing that makes serving them safe.
fn image_response(kind: &'static str, bytes: Vec<u8>) -> Response {
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
    let mut response = image_response(kind, bytes);
    let headers = response.headers_mut();
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
///
/// **The page's own folder may start with a dot.** A plan an agent writes to
/// `.plan/` was refused as its own first request, and the pane stayed blank. You
/// opened that file, so the folder holding it is no secret; a dot below it, and
/// every dot anywhere else, is still refused, and `.git` never gets that far
/// because the open refuses it.
fn servable(rel: &str, grant: &PreviewGrant, ignored: impl Fn(&str) -> bool) -> bool {
    let dir = Path::new(&grant.page).parent().unwrap_or(Path::new(""));
    let own = Path::new(rel)
        .strip_prefix(dir)
        .ok()
        .filter(|_| !dir.as_os_str().is_empty())
        .and_then(|rest| rest.to_str());
    let checked = own.unwrap_or(rel);
    if rel.is_empty()
        || rel
            .split('/')
            .any(|c| c.is_empty() || c == ".." || c == ".")
        || checked
            .split('/')
            .any(|c| c.is_empty() || c.starts_with('.'))
    {
        return false;
    }
    if !ignored(rel) {
        return true;
    }
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

/// A file dropped on a session's terminal: the bytes, and the name it had.
#[derive(Deserialize)]
pub struct DropQuery {
    pub session: uuid::Uuid,
    pub name: String,
}

/// The largest file a drop carries. A screenshot or a log, not a disk image.
pub const DROP_MAX: usize = 50 * 1024 * 1024;

/// Save a dropped file into the session's scratchpad and say where.
///
/// **Only when the webview gave no path.** A drop from a Linux file manager
/// carries the file's own path and the page types that; a Mac webview hands the
/// page the file's bytes and no path at all. Those bytes land in the scratchpad
/// because it is the one folder outside the workspace the agent may read without
/// asking, and the page types that path instead.
pub async fn drop_file(
    State(app): State<Arc<AppState>>,
    Query(q): Query<DropQuery>,
    body: axum::body::Bytes,
) -> ApiResult<serde_json::Value> {
    let cwd = {
        let inner = app.inner.read().await;
        inner.sessions.get(&q.session).map(|s| s.cwd.clone())
    };
    let Some(cwd) = cwd else {
        return Err(anyhow::anyhow!("no session {}", q.session).into());
    };
    let session = q.session;
    let path = crate::proc::run_blocking("saving a dropped file", move || {
        use std::os::unix::fs::MetadataExt;
        // Claude Code's own folder is named for the uid, and HOME is the user's.
        let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
        let uid = std::fs::metadata(&home)?.uid();
        let mut at = drop_path(Path::new("/tmp"), uid, &cwd, session, &q.name, 0);
        std::fs::create_dir_all(at.parent().unwrap_or(Path::new("/tmp")))?;
        // A second drop of `screenshot.png` must not overwrite the first, which
        // the agent may still be reading.
        let mut n = 0;
        while at.exists() && n < 1000 {
            n += 1;
            at = drop_path(Path::new("/tmp"), uid, &cwd, session, &q.name, n);
        }
        std::fs::write(&at, &body)?;
        anyhow::Ok(at)
    })
    .await??;
    Ok(Json(serde_json::json!({ "path": path })))
}

/// Where a dropped file goes: this session's scratchpad, under the name it had,
/// stripped of any folder, with `-n` before the extension for the `n`th copy.
fn drop_path(
    base: &Path,
    uid: u32,
    cwd: &Path,
    session: uuid::Uuid,
    name: &str,
    n: u32,
) -> std::path::PathBuf {
    let leaf = Path::new(name)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let clean: String = leaf
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .collect();
    let clean = clean.trim_start_matches('.');
    let clean = if clean.is_empty() { "dropped" } else { clean };
    let file = if n == 0 {
        clean.to_string()
    } else {
        match clean.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() => format!("{stem}-{n}.{ext}"),
            _ => format!("{clean}-{n}"),
        }
    };
    base.join(format!("claude-{uid}"))
        .join(crate::config::transcript_slug(cwd))
        .join(session.to_string())
        .join("scratchpad")
        .join(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dropped file lands where the scratchpad reader looks, under a name with
    /// no folder in it, and a second copy does not take the first one's place.
    #[test]
    fn a_dropped_file_lands_in_the_scratchpad_by_its_own_name() {
        let id = uuid::Uuid::parse_str("f8452082-4dc5-4a96-836f-02f7ee3a2250").unwrap();
        let cwd = Path::new("/home/me/dev/repo");
        let base = Path::new("/tmp");
        let at = drop_path(base, 501, cwd, id, "Screen Shot.png", 0);
        assert!(
            in_scratchpad(&at, &[base.to_path_buf()], cwd, id),
            "{}",
            at.display()
        );
        assert!(at.ends_with("scratchpad/Screen Shot.png"));
        assert!(drop_path(base, 501, cwd, id, "Screen Shot.png", 2)
            .ends_with("scratchpad/Screen Shot-2.png"));
        assert!(drop_path(base, 501, cwd, id, "../../etc/passwd", 0).ends_with("scratchpad/passwd"));
        assert!(drop_path(base, 501, cwd, id, ".env", 0).ends_with("scratchpad/env"));
        assert!(drop_path(base, 501, cwd, id, "", 0).ends_with("scratchpad/dropped"));
    }

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

    /// A page in a dot folder is served with what sits beside it, and nothing
    /// with a dot of its own, there or anywhere else.
    #[test]
    fn a_page_in_a_dot_folder_is_served() {
        let g = grant(".plan/plan.html", true);
        let ignored = |p: &str| p.starts_with(".plan/");
        assert!(servable(".plan/plan.html", &g, ignored), "the page itself");
        assert!(servable(".plan/plan.css", &g, ignored), "beside it");
        for bad in [
            ".plan/.env",
            ".plan/../.env",
            ".other/x.html",
            ".env",
            ".plan/a/.git/config",
        ] {
            assert!(!servable(bad, &g, ignored), "{bad}");
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
