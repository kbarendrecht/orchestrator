# First-boot experience

Replace the immediate OS folder picker with a flow the app owns: a splash while
the daemon wakes, a window to open a project, and a one-screen review of what
orchd detected before it starts. Design is the artifact built 2026-09-03 (splash,
open-project, review & confirm), in orchd's own dark palette and frameless chrome.

## Why

Today first boot (`Config::existing()` is `None`) calls `pick_checkout`, which
fires the native folder picker straight away; cancel quits. And `open()` starts
the daemon *before* building the window, so a normal boot shows nothing for the
~1.4s the daemon takes. Both are replaced here.

## Architecture

**One window, an HTTP bootstrap.** At first boot the daemon cannot start —
`orchd::start` needs a checkout. So a tiny **bootstrap HTTP server** (axum, the same
pattern the daemon SPA already uses) serves the first-run page on an ephemeral port,
and the window loads `http://127.0.0.1:<port>/`. The page calls JSON endpoints
(recents, validate, open) with `fetch`; the native folder dialog goes through Tauri
via a `/api/pick` endpoint that calls `app_handle.dialog()`. On commit the bootstrap
server stops, the daemon starts, and the window **navigates to the daemon URL**.

Chosen over Tauri IPC deliberately: the HTTP+fetch flow is **testable headlessly**
with Playwright (like `term-e2e`), where Tauri IPC only runs in the real WebKitGTK
window and needs a WebDriver harness we do not have. It also avoids Tauri v2's
capabilities system and lines up with the eventual "daemon without a checkout" shape
behind the repo switcher.

**Splash for every boot.** `open()` is restructured to build the window on the
splash first, start the daemon on a background task, and navigate when it is up —
which also fixes the blank gap on normal boots.

## What already exists to reuse

- `Config::load_or_init(Some(main))` writes a first-run `config.json` from
  `Config::default_for(main)`; a slim-diff writer already exists (config.rs ~434).
- `resolve_repo` / `forge::remote_url` / `forge::repo_from_remote` — repo detection.
- `default_upstream` = `origin/HEAD`, `default_upstream_remote` = `origin`.
- Detection facts: base branch from `origin/HEAD`, GitHub repo from the origin
  remote, worktrees `.claude/worktrees`, tracker `None`, env `mise`.

## Phases

### Phase 1 — Window-first + splash (foundation, independent value)
- Restructure `open()`: build the window on a bundled **splash** asset immediately;
  start the daemon on a background task; navigate to its URL when up.
- Config-exists path shows the splash during daemon start.
- No behaviour change to first-run yet (still `pick_checkout`), just the splash
  behind it.

### Phase 2 — Open-project screen (no config)
- Bootstrap shows the welcome: recent projects, Browse, path field.
- Tauri commands: `recent_projects()`, `pick_folder()`, `validate_path()`.

### Phase 3 — Review & confirm
- Command `detect_settings(path) -> Detected`: reuse `remote_url`/`repo_from_remote`,
  `git branch -r` for the base-branch list, `mise.toml`/`.envrc` probes,
  `compose.yaml`/`package.json` probes.
- Bootstrap renders the review; user edits. Rows: base branch, GitHub repo
  (editable, watched automatically — no off switch in v1), agent environment,
  worktrees, issue tracker; opt-in detected processes; collapsed More options.

### Phase 4 — Commit + persist
- `open_project(path, overrides)`: write a slim `config.json` (main_checkout +
  non-default overrides only, via a new `Config::write_first_run` beside the slim
  writer), record the recent project, start the daemon, return the URL to navigate
  to. Recent projects live in the global config dir (`~/.config/orchd/recent.json`,
  cross-repo).

## Decisions settled

- PR watching is automatic when the origin is on GitHub. The review's repo field
  only names *which* repo (maps to `config.repo`); there is **no off toggle** in
  v1 — the daemon has no flag for it. Add later if wanted.
- Choosing a folder always shows the review; a **recent** project skips it and goes
  straight to the board.
- Processes found in the repo are offered **unchecked** — orchd never auto-starts
  someone's docker stack on first open.

## Verification

- Detection, config-write, recent-list: unit tests in orchd against real git
  fixtures — runs everywhere.
- The Tauri flow (native picker, navigate, frameless chrome): a real-window
  hands-on step; parts are drivable, the dialog and navigate want the actual window.

## Status

- [x] Phase 1 — window-first + splash. `open()` builds the window on a `file://`
      splash (`splash_url`/`splash_html`, written to the config dir) and starts the
      daemon on a `std::thread`, navigating the window to it when up. Splash is
      passive, so `file://` needs no IPC; the interactive screens (2–4) will want a
      real asset or IPC bridge. Verified: compiles, `navigate()` exists in tauri
      2.11, splash renders. Not yet verified: the live splash→board swap on the real
      window (a hands-on run).
- [x] Phase 2 — open-project screen. `orchd::firstrun` holds the recents list,
      folder validation, and the bootstrap HTTP router (`BootstrapHost` trait +
      `serve`), all unit-tested (recents, validate, and the router contract via
      tower oneshot). `src/firstrun.html` is the page. The desktop crate splits
      `open()` into `build_window` + `boot_daemon`, adds `first_run` (serves the
      bootstrap, loads it in the window) and `TauriBootstrap` (native dialog, daemon
      boot, frameless window commands). Choosing a project starts the daemon on it
      with defaults and navigates. Verified: Rust tests + a headless browser drive of
      the real page (8/8 interactions). Not yet verified: the live native dialog and
      the daemon hand-off on the real window. Detected-settings review is Phase 3, so
      an open currently uses defaults.
- [x] Phase 3 — review & confirm. `firstrun::detect` reads a checkout (base-branch
      candidates via `git::remote_branches`, the resolved default via
      `base_checkout_branch`, GitHub repo via `forge::repo_from_remote`, env from
      `mise.toml`/`.envrc`); `/api/detect` serves it. The review screen in
      `firstrun.html` shows base branch, GitHub repo, agent environment, worktrees
      and tracker, edited then confirmed. A recent project skips the review. The
      **processes found** card is included: `detect_processes` offers a compose stack
      and conventional `package.json` scripts (package manager from the lockfile),
      unchecked; ticking one writes it into `main_processes` with `autostart: true`.
      Tested: `detect`, `detect_processes`, `write_config` with processes, the detect
      route, and the page's detect→review→confirm flow (19/19).
- [x] Phase 4 — commit + persist (folded into Phase 3). `firstrun::write_config`
      writes a slim, validated `config.json` (main_checkout + only the non-default
      overrides) before the daemon reads it; `/api/open` calls it, records the
      recent, and boots. Non-fatal: a write failure loses the edits, not the open.
      Unit-tested (`write_config_is_slim_and_validated`).

## Left to verify on the real window

The whole flow is tested headlessly (Rust + a browser drive of the real page), but
three things only the real WebKitGTK window can confirm: the **native folder
dialog** (`/api/pick`), the **splash→board hand-off** after commit, and the
**frameless chrome** (drag, resize, min/close) on the bootstrap page. A hands-on
launch when at the PC settles all three.
