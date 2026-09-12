//! Headless orchd: the daemon with a terminal instead of a window.
//!
//! Everything of substance is in the library, which the desktop shell embeds.
//! This is the entry point for running it in a terminal and pointing a browser
//! at it — still the fastest way to debug the daemon itself.
// A command-line binary: printing *is* its output, and `print_stdout` is denied
// across the workspace so the daemon library cannot quietly grow a `println!`
// that no log ever sees.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use anyhow::Result;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    // The same subscriber the app installs, so a daemon in a terminal and a daemon
    // the app spawned leave the same lines in the same place. It used to be a
    // stdout-only `fmt()` here, which is fine at a prompt and invisible to a child
    // process whose stdout the parent reads one line of.
    // A child's stdout is the parent's protocol channel, so the log goes to the
    // file only; a terminal run keeps both.
    let announce = std::env::args().any(|a| a == "--announce");
    orchd::logging::init("orchd=info", !announce);
    orchd::logging::install_panic_hook();

    let arg = |name: &str| std::env::args().skip_while(|a| a != name).nth(1);

    // A host in a terminal, with a child daemon per checkout named after the flag.
    //
    // **The one way to drive the multi-checkout page without a screen.** The app
    // is the other host and it needs a window; `mise run shot` is this repo's
    // answer to "check a UI change when you cannot see the screen", and it needs
    // something serving the page. Everything here is the same library the app
    // calls, so what this serves is what the app serves, minus the window.
    if std::env::args().any(|a| a == "--host") {
        return run_host(
            std::env::args()
                .skip_while(|a| a != "--host")
                .skip(1)
                .collect(),
        )
        .await;
    }
    let main_checkout = arg("--main")
        .map(PathBuf::from)
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p));
    // The origin the page is served from, when a host spawned this daemon. One
    // exact string, on the argv, never in a file — `config::Config::host_origin`
    // says why.
    let host_origin = arg("--host-origin");

    let server = orchd_serve::start(orchd_serve::StartOptions {
        main_checkout,
        // A busy port here means another orchd is already running, and saying
        // so beats quietly starting a second one somewhere else. A host-spawned
        // child takes what it can get: it was given a checkout, and the port it
        // ends up on is reported rather than assumed.
        fallback_port: announce,
        // A browser tab draws its own chrome.
        chrome: orchd::window::Chrome::None,
        host_origin,
        // The host says so when a person chose "start empty" while adding this
        // checkout back. Nothing else passes it; an ordinary restart resumes.
        no_resume: std::env::args().any(|a| a == "--no-resume"),
    })
    .await?;

    if announce {
        // The one line the parent reads, and the only reason a token is ever
        // printed. Flushed, because a parent is blocking on it.
        // The repository this daemon will poll, so the host can refuse a second
        // checkout of it. Derived here rather than by the host because it needs
        // this checkout's own `upstream_remote` and `repo`, which only the daemon
        // that just read that config knows.
        let repo = orchd::resolve_repo(&server.app).map(|(o, n)| format!("{o}/{n}"));
        println!(
            "{}",
            orchd::child::ready_line(server.port, &server.token, repo.as_deref())
        );
        use std::io::Write;
        let _ = std::io::stdout().flush();
        // The second kill switch: a host that was SIGKILLed leaves no signal to
        // catch, only this pipe going away.
        orchd::child::exit_on_stdin_eof(|| std::process::exit(0));
    } else {
        println!("orchd  {}", server.url());
        println!("main   {}", server.app.cfg.main_checkout.display());
    }

    // Ctrl-C takes the children with it, same as closing the desktop window — and
    // so does SIGTERM, which is how anything that is not a keyboard asks a process
    // to stop: `kill`, a systemd unit, a container runtime, a script that started
    // this and is tidying up. Listening for one and not the other meant every
    // managed process survived those, and a `ng build --watch` nobody is watching
    // is a CPU leak with a log file.
    let stopped = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            // A failure to register is not a reason to refuse to run: fall back to
            // Ctrl-C alone, which is what this did before.
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("no SIGTERM handler, Ctrl-C only: {e}");
                    return tokio::signal::ctrl_c().await;
                }
            };
            tokio::select! {
                r = tokio::signal::ctrl_c() => r,
                _ = term.recv() => Ok(()),
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await
        }
    };
    stopped.await?;
    println!();
    server.shutdown().await;
    Ok(())
}

/// Serve the page for N checkouts, each with its own child daemon.
///
/// Every path after `--host` is a checkout. The children are real `orchd`
/// processes found the same way the app finds them, so this exercises the child
/// protocol rather than standing in for it.
///
/// **A real host, writing real state**: each checkout gets its state directory
/// under `ORCHD_CONFIG_DIR` and lands in `recent.json`, the same as under the app.
/// Point that variable somewhere else for a throwaway one.
///
/// The argv names the *starting* set and is not written to `host.json` — this is a
/// one-off, the way `--main` is. An `add` or a `close` during the session does
/// write, because that is a decision rather than an argument.
async fn run_host(checkouts: Vec<String>) -> Result<()> {
    if checkouts.is_empty() {
        anyhow::bail!("--host takes one or more checkout paths");
    }
    let paths: Vec<PathBuf> = checkouts
        .iter()
        .map(|p| {
            let p = PathBuf::from(p);
            std::fs::canonicalize(&p).unwrap_or(p)
        })
        .collect();

    let serving = orchd_serve::host::serve(
        orchd_serve::host::mint_token(),
        // Ephemeral, like the app's: the URL is printed, so nothing has to predict
        // it, and a stale process on a fixed port cannot stop this starting.
        0,
        orchd::window::Chrome::None,
    )
    .await?;
    // Blocking: a launch waits for each child's ready line.
    let host = serving.host.clone();
    let opened = paths.clone();
    tokio::task::spawn_blocking(move || host.open_remembered(&opened)).await?;

    println!("orchd  {}", serving.url());
    for c in serving.host.checkouts() {
        println!(
            "       {} on {} ({})",
            c.name,
            c.port,
            if c.live { "up" } else { "down" }
        );
    }
    tokio::signal::ctrl_c().await?;
    println!();
    serving.host.stop_all();
    Ok(())
}
