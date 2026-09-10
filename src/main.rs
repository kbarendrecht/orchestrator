//! Headless orchd: the daemon with a terminal instead of a window.
//!
//! Everything of substance is in the library, which the desktop shell embeds.
//! This is the entry point for running it in a terminal and pointing a browser
//! at it — still the fastest way to debug the daemon itself.

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
    let main_checkout = arg("--main")
        .map(PathBuf::from)
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p));
    // The origin the page is served from, when a host spawned this daemon. One
    // exact string, on the argv, never in a file — `config::Config::host_origin`
    // says why.
    let host_origin = arg("--host-origin");

    let server = orchd::start(orchd::StartOptions {
        main_checkout,
        // A busy port here means another orchd is already running, and saying
        // so beats quietly starting a second one somewhere else. A host-spawned
        // child takes what it can get: it was given a checkout, and the port it
        // ends up on is reported rather than assumed.
        fallback_port: announce,
        // A browser tab draws its own chrome.
        chrome: orchd::window::Chrome::None,
        host_origin,
    })
    .await?;

    if announce {
        // The one line the parent reads, and the only reason a token is ever
        // printed. Flushed, because a parent is blocking on it.
        println!("{}", orchd::child::ready_line(server.port, &server.token));
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
