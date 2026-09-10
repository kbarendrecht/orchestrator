//! A whole second `orchd` for a second repository, and the parent side that
//! supervises it.
//!
//! **Why a child process rather than a second daemon in this one.** See
//! [`orchd::peers`] for the design and the `TODO.md` decision it reverses. The
//! short of it: a daemon is already exactly a one-repository thing, so the
//! cheapest correct way to have two is to have two of them — the way two terminal
//! tabs are two whole shells. Nothing in `orchd` becomes repo-aware, the
//! `config_dir` process global stays correct (each process has its own), and a
//! crash or a self-upgrade restart of one repository leaves the other's sessions
//! alone.
//!
//! **Why this binary re-executes itself.** The bundle ships `orch` and
//! `orchestrator-desktop`; it does not ship `orchd`, so a released install has no
//! daemon binary to spawn. Re-executing ourselves with [`FLAG`] needs no bundle
//! change and cannot drift from the daemon this app embeds, because it *is* it.
//!
//! **Why the token comes back out rather than going in.** The app token opens
//! every API route, and this codebase is deliberate about where it may exist:
//! never on disk, and never in a spawned session's environment — `triage.rs`
//! asserts `ORCHD_TOKEN` is absent from a run's env, and `hooks.rs` writes no
//! token into the settings file. Handing a child its token through the
//! environment would put it in the environment of every session *that* child
//! spawns. So the child mints its own and prints one line for the parent to read,
//! and the parent keeps it in memory.

use anyhow::{Context, Result};
use orchd::peers::Peer;
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// The argument that turns this binary into a headless daemon for one checkout.
pub const FLAG: &str = "--secondary";

/// The board's origin, which a secondary must answer because it serves no page of
/// its own. See [`orchd::config::Config::sibling_origin`].
pub const ORIGIN_FLAG: &str = "--board-origin";

/// How long the parent waits for a child to say which port it came up on.
///
/// Generous because a cold start is child processes rather than CPU — the real
/// monorepo measured 7.8s and 447 of them before the sweep was deferred — and a
/// repository that is merely slow must not be reported as broken. Bounded because
/// the alternative is a rail that never finishes appearing.
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// What this process was asked to be a daemon for, if it was: the checkout and
/// the origin of the board that will be showing it.
pub fn requested() -> Option<(PathBuf, Option<String>)> {
    let args: Vec<String> = std::env::args().collect();
    let checkout = args
        .iter()
        .position(|a| a == FLAG)
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)?;
    let origin = args
        .iter()
        .position(|a| a == ORIGIN_FLAG)
        .and_then(|i| args.get(i + 1))
        .cloned();
    Some((checkout, origin))
}

/// Where a secondary repository keeps its own durable state.
///
/// **Derived from the checkout, not from its position in the list.** That is what
/// keeps [`orchd::instance`]'s lock meaningful: the lock is per config dir, so a
/// checkout that always maps to the same dir cannot end up with two daemons, and
/// reordering the rail does not hand a repository somebody else's sessions.
///
/// The directory name is in there for the human reading `~/.../orchd/repos/`, and
/// the hash is what makes it unique — two checkouts can share a directory name
/// (a fork beside its parent), and a path is not a legal path component.
pub fn config_dir_for(primary: &Path, checkout: &Path) -> PathBuf {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in checkout.to_string_lossy().as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let name = orchd::peers::name_for(checkout);
    // Anything that is not obviously safe in a path component becomes `-`: this
    // ends up inside a shell-quoted hook command and inside the transcript slug.
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    primary.join("repos").join(format!("{safe}-{hash:x}"))
}

/// The line a child prints so its parent can reach it: `ready <port> <token>`.
///
/// Two fields on one line rather than JSON, because JSON here would mean adding
/// `serde` and `serde_json` to this crate for one line of one message between two
/// copies of the same binary. The token is a simple UUID
/// ([`orchd::state::random_token`]) so it carries no separator, and it is taken as
/// the whole remainder anyway.
///
/// `ready` leads so a line that is *not* this — a panic, a warning that beat the
/// logger to stdout — is recognisable rather than parsed as a port.
const READY: &str = "ready";

struct Endpoint {
    port: u16,
    token: String,
}

impl Endpoint {
    fn line(&self) -> String {
        format!("{READY} {} {}", self.port, self.token)
    }

    fn parse(line: &str) -> Option<Endpoint> {
        let rest = line.trim().strip_prefix(READY)?.trim_start();
        let (port, token) = rest.split_once(' ')?;
        Some(Endpoint {
            port: port.parse().ok()?,
            token: token.trim().to_string(),
        })
    }
}

/// The child: be a headless daemon for one checkout until told to stop.
///
/// Mirrors `orchd`'s own `main` rather than sharing it, because the two differ in
/// the one way that matters here: this reports its endpoint on stdout and takes
/// its config dir from the environment the parent set.
pub fn run(checkout: PathBuf, board_origin: Option<String>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new().context("the secondary runtime")?;
    rt.block_on(async move {
        let server = orchd::start(orchd::StartOptions {
            main_checkout: Some(checkout),
            /* The board is on the primary's port, so every call it makes to this
               daemon is cross-origin and would otherwise be refused. `None` if the
               flag was not given, which keeps the Origin check as tight as a
               primary's — a secondary started by hand for a look around gets no
               widening it was not told to have. */
            sibling_origin: board_origin,
            /* The primary already holds the configured port, and every secondary
               after the first would collide too. An ephemeral port is right
               because nothing external addresses these — the parent learns it from
               the line below and hands it to the page. A repository that wants a
               fixed one pins `port` in its own `config.json`, which is the whole
               reason each of these has one. */
            fallback_port: true,
            // Serves no page, so it draws no chrome. The titlebar belongs to the
            // primary's page and keeps talking to the primary.
            chrome: orchd::window::Chrome::None,
        })
        .await
        .context("starting the secondary daemon")?;

        let line = Endpoint {
            port: server.port,
            token: server.token.clone(),
        }
        .line();
        println!("{line}");
        // The parent reads a line, so it has to be a line before it blocks on
        // anything. A pipe that is never flushed is a repository that never
        // appears.
        use std::io::Write;
        std::io::stdout().flush().ok();

        tracing::info!(
            "secondary daemon for {} on port {}",
            server.app.cfg.main_checkout.display(),
            server.port
        );

        /* **The parent going is also a reason to stop, and it is the one that
           cannot be assumed polite.** `shutdown` in the shell hangs off Tauri's
           exit hook, which a SIGTERM or a crash never reaches — measured: killing
           the app left this daemon running, with live sessions, holding a checkout
           the next launch would adopt. So rather than trust the ask, watch the
           parent: it holds our stdin and writes nothing down it, so the read
           returns EOF exactly when it is gone. Portable, unlike Linux's
           `PR_SET_PDEATHSIG`, and no polling. */
        let (gone_tx, gone_rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 64];
            loop {
                match std::io::stdin().read(&mut buf) {
                    // EOF, or a pipe error, which means the same thing here.
                    Ok(0) | Err(_) => break,
                    // Nothing is ever sent down it; the pipe is the signal.
                    Ok(_) => {}
                }
            }
            let _ = gone_tx.send(());
        });

        // The parent asks with SIGTERM. Ctrl-C too, for the case where somebody
        // ran this by hand to see what it does.
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::terminate()) {
                Ok(mut term) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = term.recv() => {}
                        _ = gone_rx => tracing::info!("the app that started this repository is gone"),
                    }
                }
                Err(e) => {
                    tracing::warn!("no SIGTERM handler, Ctrl-C only: {e}");
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = gone_rx => {}
                    }
                }
            }
        }
        #[cfg(not(unix))]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = gone_rx => {}
        }

        // The same shutdown the primary does: managed processes stopped, sessions
        // killed and waited for. A repository's agents are this process's children
        // and must be gone before it is.
        server.shutdown().await;
        Ok(())
    })
}

/// A running secondary, from the parent's side.
pub struct Secondary {
    child: std::process::Child,
    pub peer: Peer,
}

// `peer.colour` is written by the shell after every launch has answered; see
// `launch`'s note and `boot_daemon`.


impl Secondary {
    /// Ask it to stop the way the primary's own shutdown asks: SIGTERM, which is
    /// what [`run`] waits for, then wait — its sessions have to be gone before we
    /// go, or the next launch adopts worktrees with live agents in them.
    ///
    /// Not `kill_gracefully`: that is for a *pty child*, and reaches for the
    /// process group because a HUP-ignoring grandchild needs reaching. This child
    /// is our own, it handles SIGTERM, and its own `shutdown` is what takes its
    /// grandchildren down.
    pub fn stop(mut self) {
        let pid = self.child.id();
        orchd::proc::ask_to_stop(&self.child);
        // Bounded: a repository that will not shut down must not stop the app from
        // closing. `try_wait` in a loop rather than `wait`, so the deadline is real.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Ok(None) => {
                    tracing::warn!("secondary daemon {pid} did not stop in time; killing it");
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
                Err(e) => {
                    tracing::warn!("could not wait for secondary daemon {pid}: {e}");
                    return;
                }
            }
        }
    }
}

/// Start a secondary daemon for `checkout` and wait until it says where it is.
///
/// `primary_config_dir` is this app's own state directory, which the child's
/// hangs off — see [`config_dir_for`].
pub fn launch(
    primary_config_dir: &Path,
    checkout: &Path,
    board_origin: &str,
) -> Result<Secondary> {
    let exe = std::env::current_exe().context("finding this binary to re-execute")?;
    let dir = config_dir_for(primary_config_dir, checkout);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating the state directory {}", dir.display()))?;

    let mut child = std::process::Command::new(&exe)
        .arg(FLAG)
        .arg(checkout)
        .arg(ORIGIN_FLAG)
        .arg(board_origin)
        /* Everything durable follows this, which is what makes a second daemon
           safe rather than a second daemon fighting over `sessions.json` and the
           hook settings file. Set here rather than inherited, because the parent's
           own value is the one thing it must *not* be. */
        .env("ORCHD_CONFIG_DIR", &dir)
        /* **Piped and never written to.** This is the child's parent-death
           watch: it reads stdin and sees EOF the moment this process ends,
           however it ends. `Secondary` keeps the `Child`, and the `Child` keeps
           the write end — so it must never be `take`n. */
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // Inherited: the child's tracing goes to its own `orchd.log` (it follows
        // `ORCHD_CONFIG_DIR` too), and anything it writes before the logger exists
        // is worth seeing beside ours.
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning a daemon for {}", checkout.display()))?;

    let stdout = child.stdout.take().context("the child's stdout")?;
    let (tx, rx) = std::sync::mpsc::channel();
    /* **Scanned for the ready line, not read as the first line.** `tracing` writes
       to stdout, so the child's own boot log shares this pipe — its very first line
       is `logging to …`, which is what a first-line reader gets instead of an
       endpoint. That is why [`READY`] is a prefix rather than a bare `port token`:
       this can recognise its one line among everything else on the way past.

       **Drained for the child's whole life**, not just until that line. A pipe
       nobody reads fills, and the write that fills it blocks; dropping the read end
       instead is worse, since Rust ignores SIGPIPE and `println!` *panics* on a
       failed write, so the child would abort at some unrelated later moment. This
       thread reads to EOF, which is the child exiting. */
    std::thread::spawn(move || {
        let mut ready = false;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if ready {
                // Dropped rather than forwarded: the child keeps its own
                // `orchd.log` under its own config dir, and echoing every line of
                // it into ours would double every repository's log.
                continue;
            }
            if let Some(endpoint) = Endpoint::parse(&line) {
                let _ = tx.send(endpoint);
                ready = true;
                continue;
            }
            // Still waiting, so anything it says is diagnostic: this is the window
            // in which a repository fails to open, and its own logger may not have
            // a file yet.
            if !line.trim().is_empty() {
                tracing::debug!("secondary: {}", line.trim());
            }
        }
        // Dropping `tx` here is what turns "exited before it was ready" into a
        // disconnect rather than a wait for the full timeout.
    });

    /* **Every failure from here kills the child.** Dropping a `std::process::Child`
       does not: it reaps nothing and signals nothing, so the first version of this
       left a fully-booted daemon running with nobody holding it — a second daemon
       on a checkout, which is the exact thing the instance lock exists to stop, and
       invisible because the log line said the repository was *not* open. */
    let endpoint = match rx.recv_timeout(READY_TIMEOUT) {
        Ok(endpoint) => endpoint,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let _ = child.wait();
            anyhow::bail!(
                "the daemon for {} exited before it was ready; see {}",
                checkout.display(),
                dir.join("orchd.log").display()
            );
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            orchd::proc::ask_to_stop(&child);
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "the daemon for {} did not come up within {}s; see {}",
                checkout.display(),
                READY_TIMEOUT.as_secs(),
                dir.join("orchd.log").display()
            );
        }
    };

    Ok(Secondary {
        child,
        /* Colourless for now: [`orchd::peers::colours_for`] cannot answer for one
           checkout, so the shell sets it once every repository that came up is
           known. A placeholder rather than an `Option`, because every reader wants
           a colour and the window between the two is inside one function. */
        peer: Peer::new(
            checkout.to_path_buf(),
            endpoint.port,
            endpoint.token,
            orchd::peers::PALETTE[0],
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lock that stops two daemons sharing a checkout is per config dir, so
    /// the mapping from checkout to dir has to be a function of the checkout and
    /// nothing else — not of its position in `extra_checkouts`, which is editable.
    #[test]
    fn a_checkout_always_maps_to_the_same_state_directory() {
        let primary = Path::new("/home/me/.config/orchd");
        let repo = Path::new("/home/me/development/platform");
        assert_eq!(config_dir_for(primary, repo), config_dir_for(primary, repo));
        assert!(config_dir_for(primary, repo).starts_with(primary.join("repos")));
    }

    /// Two checkouts can share a directory name — a fork beside its parent — and
    /// sharing a state directory would mean sharing sessions and an instance lock.
    #[test]
    fn two_checkouts_with_one_name_get_two_directories() {
        let primary = Path::new("/cfg");
        let a = config_dir_for(primary, Path::new("/a/orchestrator"));
        let b = config_dir_for(primary, Path::new("/b/orchestrator"));
        assert_ne!(a, b);
        // Still readable: the name a person recognises is in both.
        assert!(a.to_string_lossy().contains("orchestrator"));
    }

    /// The component ends up in shell-quoted hook commands and in a transcript
    /// slug, so a checkout whose name carries a separator must not become two.
    #[test]
    fn an_awkward_directory_name_stays_one_path_component() {
        let dir = config_dir_for(Path::new("/cfg"), Path::new("/x/we ird:name"));
        let tail = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!tail.contains(' '), "{tail}");
        assert!(!tail.contains('/'), "{tail}");
        assert!(!tail.contains(':'), "{tail}");
    }

    /// Hand-parsed, so it gets a test: this is the only thing the two processes
    /// say to each other, and a token read as a port is a repository that silently
    /// never appears.
    #[test]
    fn the_endpoint_line_round_trips_and_refuses_anything_else() {
        let sent = Endpoint { port: 41234, token: "0a1b2c3d4e5f".into() };
        let got = Endpoint::parse(&sent.line()).expect("round trip");
        assert_eq!(got.port, 41234);
        assert_eq!(got.token, "0a1b2c3d4e5f");

        // A line that is not ours — a warning that beat the logger to stdout, a
        // panic — must not parse as an endpoint.
        assert!(Endpoint::parse("WARN something happened").is_none());
        assert!(Endpoint::parse("ready 41234").is_none(), "no token");
        assert!(Endpoint::parse("ready notaport tok").is_none());
        assert!(Endpoint::parse("").is_none());
    }

    #[test]
    fn the_flags_are_the_ones_launch_writes() {
        // `requested` reads the real argv, so this pins the contract rather than
        // the parse: both sides of the re-exec are in this file and a rename on one
        // of them alone would be silent — the child would start as a *primary*,
        // take the main instance lock and refuse.
        assert_eq!(FLAG, "--secondary");
        assert_eq!(ORIGIN_FLAG, "--board-origin");
    }
}
