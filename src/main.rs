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

    // Ctrl-C takes the children with it, same as closing the desktop window.
    tokio::signal::ctrl_c().await?;
    println!();
    server.shutdown().await;
    Ok(())
}
