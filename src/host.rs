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
//! **What this module owns**: the page and its assets, the window handle, the set
//! of open checkouts, and the four commands that change that set — add, close,
//! reopen and the folder dialog. Each open checkout is a child `orchd` on its own
//! port with its own token, and [`HostFile`] is what makes the set survive a
//! launch. A solo `orchd` mounts this into its own router and is its own host,
//! which is why nothing here assumes an app.
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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::window::WindowControl;

/// How long a daemon has to stay up for its next death to count as a new problem.
///
/// A start is ~1.3 s on a real repo, including a network fetch, so anything that
/// dies inside this window died *of starting* — a config this build refuses, an
/// untrusted `mise.toml`, a checkout whose directory went away. Past it, the
/// daemon plainly could start, and whatever killed it is worth one more try.
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);

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
    /// What to call this checkout on screen. Not a key — see [`Checkout::path`].
    ///
    /// Usually the last path component. **The parent segment is prefixed when two
    /// open checkouts share a leaf** (`work/app` beside `play/app`), because the
    /// leaf alone is the name in the rail header, the identity chip and every
    /// message that says which checkout an action lands in — and two rows reading
    /// `app` make all three useless. Only on a collision: a longer name in a 290px
    /// rail is a cost most installs should not pay.
    ///
    /// Computed over the whole set by [`name_the_set`], so it changes when the set
    /// does: closing the checkout that collided gives the other its short name
    /// back.
    pub name: String,
    /// Where its daemon answers.
    pub port: u16,
    /// What that daemon wants in `x-orch-token`.
    pub token: String,
    /// False for a checkout whose daemon is down. Always true while the only
    /// daemon is the process serving this page: a host cannot outlive itself.
    pub live: bool,
    /// The repository this checkout's daemon polls, `owner/name`, when it has one.
    ///
    /// The key [`Host::add_checkout`] refuses a second checkout on — see
    /// [`polled_repo`] for why it is this value and not the `Repos` pair. `None`
    /// for a checkout with no matching remote, and two of those are allowed:
    /// refusing them would refuse every local-only checkout after the first.
    pub repo: Option<String>,
    /// The other open checkout polling the same repository, when there is one.
    ///
    /// **A clash `add` could not see.** `add` derives the identity from whatever
    /// config exists; the daemon derives it from the real `upstream_remote` and
    /// reports it, which is the only authoritative answer. A disagreement between
    /// the two is named here rather than tolerated in silence — the two daemons
    /// cannot see each other's fix runs, and nothing else would ever say so.
    pub clash: Option<String>,
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
    /// **A `std::sync::Mutex`, not tokio's**, and that is a decision rather than
    /// habit: the observer thread in [`crate::child`] is a plain `std::thread` —
    /// it has to be, because it owns a blocking `wait()` — and it is the thing
    /// that reports a checkout down. An async lock would need a runtime handle
    /// smuggled onto that thread. Every critical section here is a map or vector
    /// update with no `await` in it, so a blocking lock is both correct and
    /// cheaper.
    window: Mutex<Option<Arc<dyn WindowControl>>>,
    /// Every open checkout, in the order they were opened.
    checkouts: Mutex<Vec<Checkout>>,
    /// The child processes behind them, keyed on the checkout path.
    ///
    /// Separate from [`Self::checkouts`] because they have different lifetimes: a
    /// row survives its daemon dying (that is what `live: false` is for, and what
    /// `reopen` acts on), while the handle does not.
    children: Mutex<HashMap<PathBuf, Arc<crate::child::Child>>>,
    /// Which checkouts have already spent their one restart.
    ///
    /// Bounded to one retry: a first death is worth a free recovery, and a
    /// checkout that kills its daemon twice is a checkout to look at rather than
    /// to keep restarting — every restart runs `auto_resume`, so a crash loop
    /// respawns agents nobody asked for.
    ///
    /// **Cleared by a long life, not by a successful start.** It used to be
    /// cleared whenever a start reached its ready line, which is every start — so
    /// the bound could never bite and a daemon that died on boot was restarted
    /// forever. A daemon that ran for [`HEALTHY_UPTIME`] and then died is a
    /// different event from one that died at once, and that is the distinction
    /// worth keeping: the first gets its retry back, the second does not.
    retried: Mutex<HashMap<PathBuf, bool>>,
    /// When each checkout's current daemon started, so a death can be told from a
    /// failure to start.
    started: Mutex<HashMap<PathBuf, Instant>>,
    /// Which key the app's own chords wear, and whether the page draws its own
    /// titlebar. Told to the page, never sniffed.
    chrome: crate::window::Chrome,
    /// Whether the window behind the page is see-through, so the pane can say what
    /// lowering the opacity will do. Read once, with the port — the flag is a
    /// property of the window that was built, and that window outlives the page.
    see_through: bool,
    /// Announces the checkout list whenever it changes.
    ///
    /// **Because the page's substituted copy goes stale the moment anything
    /// happens.** A checkout added, closed, restarted on a new port with a new
    /// token, or gone down — none of those can be in a page that was already
    /// served, and the alternative to a socket is reloading the page, which takes
    /// every terminal in every checkout down with it.
    ///
    /// A `broadcast` of the whole list rather than a delta, for the reason the
    /// snapshot socket gives: the list is small, whole state cannot half-apply,
    /// and a dropped message costs freshness rather than correctness.
    changes: tokio::sync::broadcast::Sender<Vec<Checkout>>,
}

impl Host {
    pub fn new(token: String, port: u16, chrome: crate::window::Chrome) -> Arc<Self> {
        Arc::new(Host {
            token,
            port,
            window: Mutex::new(None),
            checkouts: Mutex::new(Vec::new()),
            children: Mutex::new(HashMap::new()),
            retried: Mutex::new(HashMap::new()),
            started: Mutex::new(HashMap::new()),
            chrome,
            see_through: see_through_window(),
            // Small: a subscriber that falls this far behind is a page that has
            // stopped reading, and the list it eventually gets is the current one.
            changes: tokio::sync::broadcast::channel(16).0,
        })
    }

    /// Give the host its native window. Called once, by whichever process has one.
    pub fn attach_window(&self, control: Arc<dyn WindowControl>) {
        *self.window.lock().unwrap() = Some(control);
    }

    /// Add or replace a checkout's entry, keyed on the path.
    ///
    /// Replace rather than push, because a restarted daemon is the same checkout
    /// on a new port with a new token, and two rows for one path is the shape
    /// where the page picks whichever it happened to render.
    pub fn record(&self, checkout: Checkout) {
        let mut open = self.checkouts.lock().unwrap();
        match open.iter_mut().find(|c| c.path == checkout.path) {
            Some(existing) => *existing = checkout,
            None => open.push(checkout),
        }
        name_the_set(&mut open);
        drop(open);
        self.announce();
    }

    pub fn checkouts(&self) -> Vec<Checkout> {
        self.checkouts.lock().unwrap().clone()
    }

    /// Tell every open page what the list is now.
    ///
    /// Called from every path that changes it. A send with no subscribers is not
    /// an error — the app's own window may not have loaded the page yet.
    fn announce(&self) {
        let _ = self.changes.send(self.checkouts());
    }

    /// Open every remembered checkout, each on its own thread.
    ///
    /// **Nothing waits for the slowest one.** A start is a network `git fetch`
    /// away from slow, and opened in series N checkouts hold the window at the
    /// splash for N × that — the 90 s hold the branch this replaces could produce.
    /// Each row appears the moment its daemon answers, and a checkout still
    /// starting is simply not in the list yet.
    ///
    /// A checkout that will not start is logged and skipped, never fatal: one bad
    /// path must not cost the window. Returns when every attempt has finished, so
    /// the caller can hand the window over knowing the list is settled.
    pub fn open_remembered(self: &Arc<Self>, checkouts: &[PathBuf]) {
        let mut opening = Vec::new();
        for checkout in checkouts {
            let host = self.clone();
            let checkout = checkout.clone();
            opening.push(std::thread::spawn(move || {
                if let Err(e) = host.open_checkout(&checkout) {
                    tracing::error!(checkout = %checkout.display(), "could not open: {e:#}");
                }
            }));
        }
        for handle in opening {
            let _ = handle.join();
        }
        // **Back into the order they were asked for.** They start concurrently, so
        // the rows land in the order the daemons happened to answer — and the rail
        // draws them in list order, so an install would re-shuffle its own rail on
        // every launch for no reason a person could see.
        self.reorder(checkouts);
        // After, not before: the sweep skips what is open, and reading that from
        // the rows means it cannot race a start that has not recorded itself yet.
        sweep_checkout_dirs(checkouts);
    }

    /// Put the rows in the order a person dragged them into, and remember it.
    ///
    /// **Server-side, not in the browser.** The host already owns the set and the
    /// order it opens them in, so keeping the order anywhere else would be a
    /// second answer to one question — and the app and a browser tab would
    /// disagree about a rail they are both looking at. `host.json` is the same
    /// file that decides what opens at all.
    ///
    /// Paths it does not name keep their relative order, at the end: a reorder
    /// racing an `add` must not drop the checkout that just arrived.
    pub fn order_checkouts(&self, order: &[PathBuf]) {
        self.reorder(order);
        self.remember();
    }

    /// Put the rows in the given order, keeping any the caller did not name.
    fn reorder(&self, order: &[PathBuf]) {
        let wanted: Vec<String> = order.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        self.checkouts.lock().unwrap().sort_by_key(|c| {
            wanted.iter().position(|w| *w == c.path).unwrap_or(usize::MAX)
        });
        self.announce();
    }

    /// Write the open list to the host file.
    ///
    /// Best effort and logged: a list that cannot be written costs the *next*
    /// launch its checkouts, and refusing the `add` that could not be recorded
    /// would cost this one. Called from `add` and `close`, which are the only two
    /// things that change the set — a restart replaces a daemon, not a row.
    fn remember(&self) {
        let open: Vec<PathBuf> = self.checkouts().into_iter().map(|c| PathBuf::from(c.path)).collect();
        remember_checkouts(&open);
    }

    /// The native window, when one is attached.
    fn window(&self) -> Option<Arc<dyn WindowControl>> {
        self.window.lock().unwrap().clone()
    }

    /// The pid of a checkout's daemon, while there is one.
    ///
    /// **Not on [`Checkout`]**, which is what the page sees and has no use for a
    /// pid. This is for a log line and for a test that needs to kill a daemon the
    /// way a crash would.
    pub fn pid_of(&self, checkout: &Path) -> Option<u32> {
        self.children.lock().unwrap().get(checkout).map(|c| c.pid)
    }

    /// Mark a checkout's row down, keeping the row.
    ///
    /// The row is where `reopen` lives, so dropping it would leave a checkout you
    /// opened with nothing to press. `live: false` is the difference between "this
    /// checkout is gone" and "this checkout's daemon is gone".
    fn mark_down(&self, checkout: &Path) {
        let path = checkout.to_string_lossy();
        if let Some(row) = self.checkouts.lock().unwrap().iter_mut().find(|c| c.path == path) {
            row.live = false;
        }
        self.children.lock().unwrap().remove(checkout);
        self.announce();
    }

    /// Start a checkout's daemon and record it.
    ///
    /// The observer this arms is what makes a death visible, and it is armed by
    /// `child::launch` before it returns — the same rule
    /// `spawn::watch_session_exit` follows for a pty.
    ///
    /// **A death that was not asked for is restarted once.** A death that *was*
    /// asked for is not, which is the whole reason [`crate::child::Child::stop`]
    /// sets its flag before it signals: without that a `close` would restart the
    /// daemon it just stopped, and a restart runs `auto_resume`.
    pub fn open_checkout(self: &Arc<Self>, checkout: &Path) -> anyhow::Result<()> {
        self.open_checkout_with(&crate::child::daemon_binary(), checkout, false)
    }

    /// The same, with the daemon binary named — see [`crate::child::launch_at`]
    /// for why that split exists.
    pub fn open_checkout_with(
        self: &Arc<Self>,
        exe: &Path,
        checkout: &Path,
        no_resume: bool,
    ) -> anyhow::Result<()> {
        let origin = format!("http://127.0.0.1:{}", self.port);
        // Its own state directory, handed over as `ORCHD_CONFIG_DIR` — the one
        // variable that relocates *every* durable thing at once, which is why two
        // checkouts cannot end up sharing a `sessions.json` or a hook settings
        // file by somebody forgetting one of them.
        let state = ensure_checkout_dir(checkout)?;
        let host = self.clone();
        let exe_again = exe.to_path_buf();
        let child = crate::child::launch_at(
            exe,
            checkout,
            &origin,
            &state,
            no_resume,
            move |path, asked, code| {
            host.mark_down(path);
            if asked {
                return;
            }
            // A daemon that ran a while and then died is not a daemon that will
            // not start. It has earned the free recovery back.
            let lived = host.started.lock().unwrap().remove(path).map(|at| at.elapsed());
            if lived.is_some_and(|d| d >= HEALTHY_UPTIME) {
                host.retried.lock().unwrap().remove(path);
            }
            let spent = host.retried.lock().unwrap().insert(path.to_path_buf(), true);
            if spent == Some(true) {
                tracing::error!(
                    checkout = %path.display(),
                    code = code.unwrap_or(-1),
                    "the checkout's daemon died twice; leaving it down"
                );
                return;
            }
            tracing::warn!(
                checkout = %path.display(),
                code = code.unwrap_or(-1),
                "the checkout's daemon died; restarting it once"
            );
            // A restart resumes: the person asked for an empty start once, when
            // they added the checkout, and a crash is not them asking again.
            if let Err(e) = host.open_checkout_with(&exe_again, path, false) {
                tracing::error!(checkout = %path.display(), "the restart failed: {e:#}");
            }
            },
        )?;

        self.started.lock().unwrap().insert(checkout.to_path_buf(), Instant::now());
        // **The host owns `recent.json`.** A hosted child's config dir is its own
        // checkout directory, so a child writing this would leave one single-entry
        // list per checkout — see the matching arm in `crate::start`. Best effort:
        // a list that cannot be written is not a reason to fail an open.
        if let Err(e) = crate::firstrun::record_recent(checkout) {
            tracing::warn!("could not record the recent checkout: {e:#}");
        }
        self.record(Checkout {
            path: checkout.to_string_lossy().into_owned(),
            name: checkout
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| checkout.to_string_lossy().into_owned()),
            port: child.ready.port,
            token: child.ready.token.clone(),
            live: true,
            // **The child's answer, not the host's guess.** [`add_checkout`]
            // derives the identity from whatever config exists to refuse the
            // ordinary case before a process is spawned; the daemon knows its real
            // `upstream_remote` and reports what it will actually poll. This is the
            // only place the authoritative answer exists, so the row carries it.
            repo: child.ready.repo.clone(),
            clash: None,
        });
        self.note_repo_clash(checkout);
        self.children.lock().unwrap().insert(checkout.to_path_buf(), Arc::new(child));
        Ok(())
    }

    /// Open a checkout the person just chose, or say why not.
    ///
    /// **Three refusals, and each names itself**, because the page shows the
    /// sentence and a refusal nobody can act on is worse than none:
    ///
    ///  - **The same path twice.** One checkout is one row; a second row for one
    ///    path is the shape where the page renders whichever it happened to reach.
    ///  - **Containment, in either direction.** A candidate under an open checkout,
    ///    and an open checkout under the candidate. Both give one object store two
    ///    daemons — and the reversed case is real rather than theoretical, since
    ///    [`crate::firstrun::validate`] accepts any directory where `.git` exists
    ///    and a worktree's `.git` is a file.
    ///  - **A repository already open**, keyed on [`polled_repo`] — see there for
    ///    why that value and not the `Repos` pair.
    ///
    /// The candidate is validated first, which is what resolves it: comparing an
    /// unresolved path against the resolved ones already in the list matches
    /// nothing, and on macOS `/tmp` is a symlink so that is the normal case.
    ///
    /// `resume` is the person's answer to the question [`Added::Ask`] poses, and
    /// `None` means they have not been asked yet.
    pub fn add_checkout(
        self: &Arc<Self>,
        candidate: &Path,
        resume: Option<bool>,
    ) -> std::result::Result<Added, String> {
        let info = crate::firstrun::validate(candidate)?;
        let path = PathBuf::from(&info.path);
        self.vacancy_for(&path)?;

        // Derived before the spawn so an ordinary clash costs no process. The
        // child's own answer replaces this one on the row — it knows its real
        // `upstream_remote` and this does not.
        let wanted = polled_repo(&path);
        if let Some(clash) = self.holder_of_repo(wanted.as_deref(), &path) {
            return Err(format!(
                "{} is already open, and it is a checkout of the same repository ({}).                  Two daemons polling one repository cannot see each other's fix runs.",
                clash.path,
                wanted.unwrap_or_default()
            ));
        }

        // **A close keeps a checkout's records, so a re-add has to ask.** Closing a
        // checkout is a statement about the window; resuming months-old
        // conversations because a path came back is the resurrection this design
        // spent a flag on avoiding. An ordinary app restart still resumes silently
        // — the ask is on this path only, deliberately: a restart is the same
        // window coming back, while an add is a decision you just made.
        let waiting = resumable_sessions(&path);
        let resume = match resume {
            Some(chosen) => chosen,
            None if waiting > 0 => return Ok(Added::Ask { path: info.path, sessions: waiting }),
            None => true,
        };

        self.open_checkout_with(&crate::child::daemon_binary(), &path, !resume)
            .map_err(|e| format!("{e:#}"))?;
        let opened = self
            .checkouts()
            .into_iter()
            .find(|c| c.path == info.path)
            .ok_or_else(|| "the checkout started and then vanished from the list".to_string())?;
        self.remember();
        Ok(Added::Opened(opened))
    }

    /// The path half of [`add_checkout`]'s refusals: same path, and containment
    /// either way. Split out because `reopen` needs the same answer for a row that
    /// is already in the list, minus its own entry.
    fn vacancy_for(&self, path: &Path) -> std::result::Result<(), String> {
        for open in self.checkouts() {
            let other = PathBuf::from(&open.path);
            if other == path {
                return Err(format!("{} is already open.", open.path));
            }
            if path.starts_with(&other) {
                return Err(format!(
                    "That folder is inside {}, which is already open. One git object                      store cannot have two daemons.",
                    open.path
                ));
            }
            if other.starts_with(path) {
                return Err(format!(
                    "That folder contains {}, which is already open. One git object                      store cannot have two daemons.",
                    open.path
                ));
            }
        }
        Ok(())
    }

    /// The open checkout that already polls `repo`, ignoring `except`.
    ///
    /// `None` for `repo` never clashes: a checkout with no matching remote has no
    /// repository identity, and refusing two of those would refuse every
    /// local-only checkout after the first.
    fn holder_of_repo(&self, repo: Option<&str>, except: &Path) -> Option<Checkout> {
        let repo = repo?;
        self.checkouts()
            .into_iter()
            .find(|c| c.repo.as_deref() == Some(repo) && Path::new(&c.path) != except)
    }

    /// Close a checkout: stop its daemon and drop its row.
    ///
    /// **Symmetric, down to the last one.** Any checkout, including the one you are
    /// looking at and including the only one — an empty host is the first-run page,
    /// which `firstrun::serve` already is. A refusal at N=1 would be the one place
    /// symmetry broke, and the switcher's deletion rests on close being symmetric.
    ///
    /// **Session records are left alone.** Closing a checkout is a statement about
    /// the window, not a decision about its conversations, and the transcripts a
    /// record points at are the only remaining copy once a worktree is gone. The
    /// resurrection that worries about is stopped at `add` instead, where a re-add
    /// of a path whose `sessions.json` still holds live records asks first.
    pub fn close_checkout(&self, checkout: &Path) -> bool {
        let stopped = self.stop_checkout(checkout);
        let path = checkout.to_string_lossy();
        {
            let mut open = self.checkouts.lock().unwrap();
            open.retain(|c| c.path != path);
            // The set shrank, so a name that was only long because of a collision
            // gets its short form back.
            name_the_set(&mut open);
            // The other half of a clash goes with it: a warning naming a checkout
            // that is no longer open is a warning nobody can act on.
            for row in open.iter_mut().filter(|c| c.clash.as_deref() == Some(&path)) {
                row.clash = None;
            }
        }
        // A closed checkout that is opened again deserves its free recovery back:
        // the retry count is about a daemon that will not stay up, not about a
        // path you once closed.
        self.retried.lock().unwrap().remove(checkout);
        self.started.lock().unwrap().remove(checkout);
        self.remember();
        self.announce();
        stopped
    }

    /// Start the daemon for a checkout whose row is down.
    ///
    /// The row is what `reopen` acts on, which is why [`Self::mark_down`] keeps it.
    /// Refuses a row that is already live rather than starting a second daemon for
    /// one checkout — the instance lock would refuse that anyway, but it would
    /// refuse it as a failed launch a minute later.
    pub fn reopen_checkout(self: &Arc<Self>, checkout: &Path) -> std::result::Result<(), String> {
        let path = checkout.to_string_lossy().into_owned();
        let row = self.checkouts().into_iter().find(|c| c.path == path);
        match row {
            Some(row) if row.live => Err(format!("{} is already running.", row.path)),
            Some(_) => {
                // The retry the last two deaths spent. A reopen is a person saying
                // to try again, so it is worth the same free recovery a first
                // start gets.
                self.retried.lock().unwrap().remove(checkout);
                self.open_checkout(checkout).map_err(|e| format!("{e:#}"))
            }
            None => Err(format!("{path} is not open.")),
        }
    }

    /// Name the other checkout polling this one's repository, if the daemon's own
    /// answer turned out to clash with a row already open.
    ///
    /// Both rows are marked, because neither is more at fault than the other and a
    /// warning on one of two is a warning you can dismiss by looking at the wrong
    /// one.
    fn note_repo_clash(&self, checkout: &Path) {
        let path = checkout.to_string_lossy().into_owned();
        let mut open = self.checkouts.lock().unwrap();
        let Some(mine) = open.iter().find(|c| c.path == path).and_then(|c| c.repo.clone()) else {
            return;
        };
        let other = open
            .iter()
            .find(|c| c.path != path && c.repo.as_deref() == Some(mine.as_str()))
            .map(|c| c.path.clone());
        let Some(other) = other else { return };
        tracing::warn!(
            checkout = %path,
            other = %other,
            repo = %mine,
            "two open checkouts poll one repository; their fix runs cannot see each other"
        );
        for row in open.iter_mut() {
            if row.path == path {
                row.clash = Some(other.clone());
            } else if row.path == other {
                row.clash = Some(path.clone());
            }
        }
        drop(open);
        self.announce();
    }

    /// Stop a checkout's daemon, keeping its row.
    ///
    /// Every stop path goes through here so the flag is always set before the
    /// signal. Returns whether there was a daemon to stop.
    pub fn stop_checkout(&self, checkout: &Path) -> bool {
        let child = self.children.lock().unwrap().get(checkout).cloned();
        match child {
            Some(child) => {
                child.stop();
                true
            }
            None => false,
        }
    }

    /// Stop every checkout's daemon, concurrently, on one deadline.
    ///
    /// Concurrent because quit must not be N × the grace: each child's own
    /// `shutdown` is the only thing that reaches its sessions, and those run in
    /// parallel with each other already.
    pub fn stop_all(&self) {
        let children: Vec<_> = self.children.lock().unwrap().values().cloned().collect();
        let mut waiting = Vec::new();
        for child in children {
            waiting.push(std::thread::spawn(move || child.stop()));
        }
        for handle in waiting {
            let _ = handle.join();
        }
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
    if !crate::api::origin_ok(origin, host.port, None, false, is_get, token_ok) {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }
    // The page itself is deliberately not token-gated: it is where the token comes
    // from. Everything that changes something is.
    if !is_get && !token_ok {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    next.run(req).await
}

/// What an `add` did, or what it needs answered first.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "added", rename_all = "snake_case")]
pub enum Added {
    /// The checkout is open and its daemon reported ready.
    Opened(Checkout),
    /// This path's `sessions.json` still holds conversations that were live when
    /// it was last closed. Ask, then call again with the answer.
    Ask { path: String, sessions: usize },
}

/// How many of a closed checkout's sessions would come back if it were resumed.
///
/// **Read as JSON rather than through `store::load`**, which resolves its own path
/// from the process-global config dir — this is a *different* checkout's store, and
/// the host has no business relocating a process-wide variable to read one file.
/// The same two conditions `auto_resume` applies, because a number that disagrees
/// with what resuming would actually do is worse than no number: live at the last
/// write, and a conversation behind it.
fn resumable_sessions(checkout: &Path) -> usize {
    let Ok(dir) = checkout_dir(checkout) else { return 0 };
    let Ok(raw) = std::fs::read_to_string(dir.join("sessions.json")) else {
        return 0;
    };
    let Ok(records) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) else {
        return 0;
    };
    records
        .iter()
        .filter(|r| r["was_live"] == true && r["had_a_turn"] == true)
        .count()
}

// --- the host's own file ----------------------------------------------------

/// What the host remembers between launches.
///
/// **Its own file, beside the checkouts rather than inside one.** Each checkout's
/// `config.json` belongs to its daemon and follows `ORCHD_CONFIG_DIR`; this is the
/// one thing that is the *host's*, and a host that kept its list in a checkout
/// would lose the list with the checkout.
///
/// Deliberately not built in Stage 2, because nothing read it then and a seam with
/// no subscriber is what this refactor exists to remove.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct HostFile {
    /// Every checkout that was open, in the order they were opened. The app opens
    /// these again at launch.
    #[serde(default)]
    pub checkouts: Vec<String>,
    /// How long a closed checkout's derived state is kept, in days. `0` turns the
    /// sweep off, the way it already does for worktrees.
    ///
    /// A setting rather than a constant because the number is a judgement about
    /// your own habits, and this repo has written that judgement down once
    /// already: `worktree_retention_days` defaults to 60 with a docblock arguing
    /// why — "clearly longer than anyone's memory of a branch". This follows it,
    /// so there is one number to learn rather than two.
    #[serde(default = "default_checkout_retention_days")]
    pub checkout_retention_days: u32,
    /// Whether the window is built see-through, so the theme's opacity can show
    /// the desktop behind the board.
    ///
    /// **Here rather than in `config.json`, and off by default, for two separate
    /// reasons.** The window belongs to the host — one window over every checkout —
    /// so a per-checkout file would have as many answers as you have checkouts open
    /// and no rule for which one wins. And a see-through window is a *compositor*
    /// feature: without one the surface behind the board is undefined rather than
    /// the desktop, so a machine that cannot do it would show a board you may not
    /// be able to read, and the pane that turns it off is in that board.
    ///
    /// It is read once, when the window is built, so changing it needs a restart.
    /// The theme's opacity is live and does the moment-to-moment work; this only
    /// decides whether anything is there to see.
    #[serde(default)]
    pub see_through_window: bool,
}

fn default_checkout_retention_days() -> u32 {
    60
}

/// What a checkout's state directory holds that the daemon can rebuild.
///
/// **The sweep may delete only these.** `transcripts/` is the *only* remaining
/// copy of a conversation once a worktree is gone, and a session record survives
/// precisely because its archived transcript does — so deleting the directory
/// would drop both, which contradicts `close` leaving records alone and
/// `worktree::reap_old`'s stated intent ("the tree, never the conversation").
/// `sessions.json` stays for the same reason.
///
/// Nor can the safety be borrowed from that reaper: it is safe because it routes
/// through `teardown`, whose checks refuse a live session, a dirty tree, unpushed
/// work or an attached process. A directory of JSON has no such gate, so the rule
/// here is the narrow one — delete what is regenerated on the next start, and
/// nothing else.
const DERIVED: [&str; 3] = ["plugin", "hooks.json", "window.json"];

fn host_file() -> anyhow::Result<PathBuf> {
    Ok(crate::config::Config::config_dir()?.join("host.json"))
}

/// The host file as it stands, or `None` when there is none to read.
///
/// A file that will not parse reads as absent, deliberately: the cost is one
/// forgotten list, and refusing to start over it would cost the window.
fn read_host_file() -> Option<HostFile> {
    let raw = std::fs::read_to_string(host_file().ok()?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The checkouts to open at launch.
///
/// **Falls back to `config.json`'s `main_checkout` when there is no file**, which
/// is every install that predates this: the app has always opened exactly one
/// checkout, and reading that as an empty list would show a first-run page to
/// somebody who configured a project months ago. The first `add` or `close`
/// writes the file, and from then on it is the answer.
pub fn remembered_checkouts() -> Vec<PathBuf> {
    match read_host_file() {
        Some(file) => file.checkouts.into_iter().map(PathBuf::from).collect(),
        None => crate::config::Config::existing()
            .map(|cfg| vec![cfg.main_checkout])
            .unwrap_or_default(),
    }
}

/// Give every checkout a display name, disambiguating any that collide.
///
/// The leaf, or `<parent>/<leaf>` for a leaf two or more open checkouts share.
/// Over the whole set rather than per row, because "is this name ambiguous" is a
/// question about the set — the same reason the colour band is assigned that way.
///
/// One level of parent only. A pair that collides at two levels
/// (`a/x/app`, `b/x/app`) is rarer than the cost of a name that grows without
/// bound, and the full path is on the row's tooltip either way.
fn name_the_set(checkouts: &mut [Checkout]) {
    let leaf = |p: &str| {
        Path::new(p)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.to_string())
    };
    let mut seen: HashMap<String, usize> = HashMap::new();
    for c in checkouts.iter() {
        *seen.entry(leaf(&c.path)).or_default() += 1;
    }
    for c in checkouts.iter_mut() {
        let short = leaf(&c.path);
        let shared = seen.get(&short).is_some_and(|n| *n > 1);
        let parent = Path::new(&c.path)
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned());
        c.name = match (shared, parent) {
            (true, Some(parent)) => format!("{parent}/{short}"),
            // Nothing to prefix — a checkout at the filesystem root, which no real
            // one is. The short name is still better than nothing.
            _ => short,
        };
    }
}

/// Write the checkouts to open next launch.
///
/// **The rows, not what started.** A checkout whose daemon refused to start keeps
/// its row and stays in the file, because dropping it would mean one bad boot
/// silently forgets a checkout you opened on purpose. Only `close` takes one out.
///
/// Best effort and logged: a list that cannot be written costs the *next* launch
/// its checkouts, and refusing the `add` that could not be recorded would cost
/// this one.
pub fn remember_checkouts(checkouts: &[PathBuf]) {
    // Read first, so a hand-set `checkout_retention_days` survives every add and
    // close. A writer that rebuilds the file from what it knows is how one setting
    // nobody touched disappears.
    let mut file = read_host_file().unwrap_or_default();
    file.checkouts = checkouts.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let write = || -> anyhow::Result<()> {
        let path = host_file()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&file)? + "\n")?;
        Ok(())
    };
    if let Err(e) = write() {
        tracing::error!("could not record the open checkouts: {e:#}");
    }
}

/// The retention the host file names, in days.
/// Whether the window should be built see-through. See [`HostFile::see_through_window`].
pub fn see_through_window() -> bool {
    read_host_file().is_some_and(|f| f.see_through_window)
}

fn checkout_retention_days() -> u32 {
    read_host_file().map_or_else(default_checkout_retention_days, |f| f.checkout_retention_days)
}

/// Drop the regenerable state of checkouts nobody has opened in a long time.
///
/// `<config dir>/checkouts/` is append-only otherwise: a full skills-plugin copy
/// and a hook settings file per checkout ever tried, and every one of those is
/// rewritten on the next start of the daemon that owns it.
///
/// **It never touches a conversation** — see [`DERIVED`] for what that rules out
/// and why. A directory left holding only `transcripts/` and `sessions.json` is
/// the expected outcome, and the log says how much was left and where.
///
/// Age is the directory's most recent write, not its creation: a checkout you
/// worked in last week is recent however long ago it was first opened.
pub fn sweep_checkout_dirs(open: &[PathBuf]) {
    let days = checkout_retention_days();
    if days == 0 {
        return;
    }
    let Ok(root) = crate::config::Config::config_dir().map(|d| d.join("checkouts")) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let keep: Vec<PathBuf> = open.iter().filter_map(|p| checkout_dir(p).ok()).collect();
    let cutoff = std::time::Duration::from_secs(u64::from(days) * 24 * 60 * 60);
    let mut swept = 0usize;
    let mut kept = 0usize;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || keep.contains(&dir) {
            continue;
        }
        if last_write(&dir).is_none_or(|age| age < cutoff) {
            continue;
        }
        let mut removed_any = false;
        for name in DERIVED {
            let target = dir.join(name);
            let gone = if target.is_dir() {
                std::fs::remove_dir_all(&target)
            } else if target.exists() {
                std::fs::remove_file(&target)
            } else {
                continue;
            };
            match gone {
                Ok(()) => removed_any = true,
                Err(e) => tracing::warn!("could not remove {}: {e}", target.display()),
            }
        }
        if removed_any {
            swept += 1;
        }
        // What is left is the half that is nobody's to delete automatically.
        if dir.join("transcripts").exists() || dir.join("sessions.json").exists() {
            kept += 1;
        }
    }
    if swept > 0 {
        tracing::info!(
            "swept the regenerable state of {swept} checkout(s) unopened for {days} days; \
             {kept} still hold conversations, in {}",
            root.display()
        );
    }
}

/// How long ago anything under `dir` was last written.
///
/// One level deep, which is where every file the daemon writes lives; a
/// transcripts archive underneath it does not change the answer, because a
/// checkout whose daemon has not run has not written one either.
fn last_write(dir: &Path) -> Option<std::time::Duration> {
    let newest = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .chain(std::fs::metadata(dir).ok().and_then(|m| m.modified().ok()))
        .max()?;
    newest.elapsed().ok()
}

/// The repository a checkout's daemon would poll, `owner/name`.
///
/// **The key [`Host::add_checkout`] refuses a second checkout on, and it is the
/// polled repository rather than the `Repos` pair.** `Repos` is
/// `{ upstream, fork }`, and `upstream_remote` defaults to `origin` — so a parent
/// clone on defaults derives `{acme/mono, acme/mono}` while a fork checkout
/// derives `{acme/mono, you/mono}`. Unequal pairs, so a rule keyed on the pair
/// would allow both, and both would then poll `acme/mono`. That is the hazard
/// itself: "one live fix run per PR" reads *this* daemon's `automation.json` and
/// `branch_busy` reads *this* daemon's workspaces, so two force-pushing runs
/// against one head ref is reachable and neither guard can see the other.
///
/// **A guess, deliberately.** The authoritative answer needs that checkout's own
/// `upstream_remote`, which lives in the `config.json` the host may be about to
/// create — so this reads whatever config already exists (a re-add has one) and
/// falls back to the default remote. The daemon re-derives with the real value and
/// reports it on its ready line. This one refuses the ordinary case before a
/// process is spawned; that one is the answer the row carries.
///
/// `None` when no remote gives it one, which is an ordinary local-only checkout.
pub fn polled_repo(checkout: &Path) -> Option<String> {
    let cfg = checkout_dir(checkout)
        .ok()
        .and_then(|dir| crate::config::Config::existing_at(&dir.join("config.json")));
    if let Some(repo) = cfg.as_ref().and_then(|c| c.repo.clone()) {
        return Some(repo);
    }
    let remote = cfg.as_ref().map_or("origin", |c| c.upstream_remote.as_str());
    let url = crate::forge::remote_url(checkout, remote)?;
    crate::forge::repo_from_remote(&url).map(|(o, n)| format!("{o}/{n}"))
}

/// Where one checkout's durable state lives.
///
/// `<config dir>/checkouts/<leaf>-<hash>`. **The hash is over the checkout path
/// alone**, so the directory is a function of the checkout and nothing else: two
/// hosts pointed at one checkout agree on where its `sessions.json` is, which is
/// the property the instance lock will need when it is re-keyed. The leaf is there
/// for the person reading a bug report by eye; it is not the key, because two
/// checkouts can share a leaf (a fork beside its parent, `web/app` beside
/// `mobile/app`).
///
/// FNV-1a rather than anything stronger: this is a filename, not a signature.
pub fn checkout_dir(checkout: &Path) -> anyhow::Result<PathBuf> {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in checkout.to_string_lossy().as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let leaf = checkout.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // Anything not obviously safe in a path component becomes `-`: this ends up
    // inside a shell-quoted hook command and inside a transcript slug.
    let safe: String = leaf
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    Ok(crate::config::Config::config_dir()?.join("checkouts").join(format!("{safe}-{hash:x}")))
}

/// Make a checkout's state directory, seeding it from the old single config once.
///
/// **The layout move is a one-shot here rather than a `migrate.rs` rule**, and the
/// reason is worth keeping: that table's `apply` is
/// `fn(&mut Map<String, Value>) -> bool` and `config_file` ends in one `fs::write`,
/// so a rule there can rewrite keys inside one file and cannot create a directory
/// or write a sibling. Its shape would have been wrong too — `main_checkout` stays
/// at the root of every per-checkout file, so a rule keyed on that key would
/// re-fire on every start of every daemon, forever.
///
/// The shape recognised here is a **location**: this checkout has no directory yet.
/// It then **copies** the old `<config dir>/config.json` rather than moving it, and
/// only when that file names *this* checkout — a copy carries `main_checkout`, so
/// seeding a second checkout from it would hand that daemon the wrong tree. The
/// root file is left where it is, so an older build still finds its config and a
/// downgrade keeps working; that is the same trade `store::OnDiskKind` and the
/// tracker names already make.
fn ensure_checkout_dir(checkout: &Path) -> anyhow::Result<PathBuf> {
    let dir = checkout_dir(checkout)?;
    if dir.exists() {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir)?;
    let root = crate::config::Config::config_dir()?.join("config.json");
    let names_this_checkout = crate::config::Config::existing_at(&root)
        .is_some_and(|cfg| cfg.main_checkout == checkout);
    if names_this_checkout {
        match std::fs::copy(&root, dir.join("config.json")) {
            Ok(_) => tracing::info!(
                checkout = %checkout.display(),
                "copied the existing config into {}",
                dir.display()
            ),
            // Not fatal: the daemon writes a default and the user has lost their
            // settings, which is bad — but refusing to start a checkout at all is
            // worse, and the root file is still there to copy by hand.
            Err(e) => tracing::error!("could not seed {}: {e}", dir.display()),
        }
    }
    Ok(dir)
}

/// A host serving on its own port.
///
/// Held by the caller for as long as the page should be reachable; dropping it
/// aborts the server task. The children are **not** in here — they are the host's,
/// because a restart replaces the child and not the server.
pub struct Serving {
    pub host: Arc<Host>,
    task: tokio::task::JoinHandle<()>,
}

impl Serving {
    /// The URL that authenticates: the token is a query parameter exactly once, on
    /// the initial navigation, so it never has to be typed and never appears in a
    /// link anybody else could follow.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/?token={}", self.host.port, self.host.token)
    }
}

/// A fresh page token.
///
/// Here rather than at the call site so both hosts mint it the same way, and
/// because a host that let its caller choose could be handed a value from a config
/// file — the one place a token must never come from.
pub fn mint_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bind a loopback port and serve the page on it.
///
/// `port` of 0 takes an ephemeral one, which is what a test wants and what the app
/// will want too: the page's own URL is handed to the webview, so nothing needs to
/// predict it. The `Host` is rebuilt with the port it actually got, because the
/// Host and Origin rules compare against it — a guard checking a port nothing is
/// listening on refuses everything, and says `bad host` while doing it.
pub async fn serve(
    token: String,
    port: u16,
    chrome: crate::window::Chrome,
) -> anyhow::Result<Serving> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let bound = listener.local_addr()?.port();
    let host = Host::new(token, bound, chrome);
    let router = router(host.clone());
    let task = tokio::spawn(async move {
        // `TCP_NODELAY` for the same reason the daemon sets it: a keystroke is one
        // small frame, and Nagle plus a delayed ACK is ~40 ms per round trip. The
        // host serves no pty, but it serves the page that opens them.
        if let Err(e) = axum::serve(listener, router).tcp_nodelay(true).await {
            tracing::error!("the host stopped serving: {e:#}");
        }
    });
    tracing::info!(port = bound, "the host is serving the page");
    Ok(Serving { host, task })
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
        .route("/api/host/checkout", post(add_checkout))
        .route("/api/host/checkout/close", post(close_checkout))
        .route("/api/host/checkout/reopen", post(reopen_checkout))
        .route("/api/host/checkout/order", post(order_checkouts))
        .route("/ws/host", get(host_socket))
        .route("/api/host/recent", get(recent))
        .route("/api/host/pick", post(pick))
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
fn page(host: &Arc<Host>, template: &str) -> String {
    let checkouts = serde_json::to_string(&host.checkouts()).unwrap_or_else(|_| "[]".into());
    template
        .replace("__ORCH_TOKEN__", &host.token)
        .replace("__ORCH_CHROME__", host.chrome.as_str())
        // Whether the opacity control has anything behind it. A slider that
        // silently does nothing is worse than one that says why it cannot.
        .replace("__ORCH_SEE_THROUGH__", if host.see_through { "yes" } else { "no" })
        .replace("__ORCH_CHECKOUTS__", &checkouts)
        // ⌘ on a Mac, Ctrl elsewhere. Told rather than sniffed — the host knows at
        // compile time, and `navigator.platform` is both deprecated and a lie
        // under a webview.
        .replace("__ORCH_PLATFORM__", if cfg!(target_os = "macos") { "mac" } else { "other" })
}

async fn index(State(host): State<Arc<Host>>) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store, must-revalidate")],
        Html(page(&host, INDEX)),
    )
        .into_response()
}

/// The review-overlay preview page, substituted the same way, because the module
/// graph reads `window.__ORCH__` at import time.
async fn review_preview(State(host): State<Arc<Host>>) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store, must-revalidate")],
        Html(page(&host, REVIEW_PREVIEW)),
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
        "palette.js" => include_str!("../web/js/palette.js"),
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
    Json(json!({ "checkouts": host.checkouts() }))
}

/// The checkout list, pushed whenever it changes.
///
/// **The page cannot learn this any other way.** Its substituted copy is a
/// snapshot of the moment it was served, and a restarted daemon mints a new token
/// — so a page that kept the old one would be refused by the very checkout it is
/// drawing. Reloading would work and would take every terminal down with it.
///
/// Token in the query rather than a header, because a browser cannot set headers
/// on a websocket. Same rule as the daemon's own sockets.
async fn host_socket(
    State(host): State<Arc<Host>>,
    axum::extract::Query(q): axum::extract::Query<WsQuery>,
    ws: axum::extract::WebSocketUpgrade,
) -> Response {
    if q.token != host.token {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    ws.on_upgrade(move |socket| host_socket_loop(host, socket))
}

#[derive(serde::Deserialize)]
struct WsQuery {
    token: String,
}

async fn host_socket_loop(host: Arc<Host>, mut socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message;
    let mut sub = host.changes.subscribe();
    async fn send(socket: &mut axum::extract::ws::WebSocket, list: Vec<Checkout>) -> bool {
        let Ok(text) = serde_json::to_string(&json!({ "checkouts": list })) else {
            // Unserialisable is not the socket's fault; keep it open.
            return true;
        };
        socket.send(Message::Text(text)).await.is_ok()
    }
    // The current list first, so a page that connected after a change is correct
    // without waiting for the next one.
    if !send(&mut socket, host.checkouts()).await {
        return;
    }
    loop {
        tokio::select! {
            msg = sub.recv() => match msg {
                Ok(list) => if !send(&mut socket, list).await { break },
                // The list is whole state, so a dropped message costs freshness
                // and the current one repairs it.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if !send(&mut socket, host.checkouts()).await {
                        break;
                    }
                }
                Err(_) => break,
            },
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            },
        }
    }
}

/// The order the rail was dragged into.
#[derive(serde::Deserialize)]
struct CheckoutOrder {
    paths: Vec<String>,
}

async fn order_checkouts(
    State(host): State<Arc<Host>>,
    Json(body): Json<CheckoutOrder>,
) -> Response {
    let order: Vec<PathBuf> = body.paths.into_iter().map(PathBuf::from).collect();
    host.order_checkouts(&order);
    Json(json!({ "ok": true })).into_response()
}

/// The checkouts opened before, newest first, minus the ones already open.
///
/// **The same list the first-run page offers**, because it is the same question —
/// which checkout do you want — asked from the other side of having one. Filtered
/// here rather than in the page: a row you cannot act on is a row that reads as
/// broken when it refuses.
async fn recent(State(host): State<Arc<Host>>) -> Json<serde_json::Value> {
    let open: Vec<String> = host.checkouts().into_iter().map(|c| c.path).collect();
    let recent: Vec<_> = crate::firstrun::recent_projects()
        .into_iter()
        .filter(|r| !open.contains(&r.path))
        .collect();
    Json(json!({ "recent": recent }))
}

/// Raise the native folder dialog, for the checkout the recents do not list.
///
/// A `POST` because it opens a window, not because it changes anything — and it
/// carries the token like every other mutating route for the same reason.
/// Blocking, on its own thread: the dialog answers when a person answers it.
async fn pick(State(host): State<Arc<Host>>) -> Response {
    let control = host.window();
    let Some(control) = control else {
        // A browser tab, where the page's own text box is the way in. Not an
        // error, and the same sentence every other window route refuses with.
        return refusal("no native window attached");
    };
    let picked = crate::proc::run_blocking("the folder dialog", move || control.pick_folder()).await;
    match picked {
        Ok(Some(path)) => Json(json!({ "path": path.to_string_lossy() })).into_response(),
        // A cancelled dialog is an answer, not a failure.
        Ok(None) => Json(json!({ "path": serde_json::Value::Null })).into_response(),
        Err(e) => refusal(&format!("{e:#}")),
    }
}

/// The one body every checkout command takes: which checkout.
#[derive(serde::Deserialize)]
struct CheckoutPath {
    path: String,
    /// `add` only: the answer to the resume question, absent until it is asked.
    #[serde(default)]
    resume: Option<bool>,
}

/// Open a checkout, on a blocking thread.
///
/// **`spawn_blocking`, because a launch waits for a `ready` line** and that wait is
/// a network `git fetch` away from slow — 1.3 s measured on a real repo. Blocking a
/// tokio worker for that long is what makes every *other* checkout's page go quiet
/// while one of them starts.
async fn add_checkout(
    State(host): State<Arc<Host>>,
    Json(body): Json<CheckoutPath>,
) -> Response {
    let path = PathBuf::from(&body.path);
    let resume = body.resume;
    let added = crate::proc::run_blocking("adding a checkout", move || {
        host.add_checkout(&path, resume)
    })
    .await;
    match added {
        Ok(Ok(added)) => Json(json!({ "ok": true, "result": added })).into_response(),
        Ok(Err(message)) => refusal(&message),
        Err(e) => refusal(&format!("{e:#}")),
    }
}

async fn close_checkout(State(host): State<Arc<Host>>, Json(body): Json<CheckoutPath>) -> Response {
    let path = PathBuf::from(&body.path);
    // Blocking too: a stop waits out the child's own graceful shutdown, which is
    // the only thing that reaches its sessions.
    match crate::proc::run_blocking("closing a checkout", move || host.close_checkout(&path)).await {
        Ok(stopped) => Json(json!({ "ok": true, "stopped": stopped })).into_response(),
        Err(e) => refusal(&format!("{e:#}")),
    }
}

async fn reopen_checkout(State(host): State<Arc<Host>>, Json(body): Json<CheckoutPath>) -> Response {
    let path = PathBuf::from(&body.path);
    let opened =
        crate::proc::run_blocking("reopening a checkout", move || host.reopen_checkout(&path)).await;
    match opened {
        Ok(Ok(())) => Json(json!({ "ok": true })).into_response(),
        Ok(Err(message)) => refusal(&message),
        Err(e) => refusal(&format!("{e:#}")),
    }
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
    let control = host.window();
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
