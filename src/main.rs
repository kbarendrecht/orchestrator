//! Headless orchd: the daemon with a terminal instead of a window.
//!
//! Everything of substance is in the library, which the desktop shell embeds.
//! This is the entry point for running it in a terminal and pointing a browser
//! at it — still the fastest way to debug the daemon itself.

use anyhow::Result;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "orchd=info".into()),
        )
        .init();

    let main_checkout = std::env::args()
        .skip_while(|a| a != "--main")
        .nth(1)
        .map(PathBuf::from)
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p));

    let server = orchd::start(orchd::StartOptions {
        main_checkout,
        // A busy port here means another orchd is already running, and saying
        // so beats quietly starting a second one somewhere else.
        fallback_port: false,
        // A browser tab draws its own chrome.
        chrome: orchd::window::Chrome::None,
    })
    .await?;

    println!("orchd  {}", server.url());
    println!("main   {}", server.app.cfg.main_checkout.display());

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
