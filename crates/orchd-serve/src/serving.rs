//! The one way a server in this crate is put on a socket.
//!
//! **Three servers bind a loopback port here** — the daemon, the host, and the
//! bootstrap server the first-run page runs on — and each spelled out its own
//! `axum::serve`. The third one forgot `TCP_NODELAY`, which is the fault this
//! crate has already paid for once and cannot see: axum defaults it to `None`
//! (`serve.rs` only calls `set_nodelay` when told to), so a small write waits for
//! an ACK that waits for the peer's delayed-ACK timer — the classic ~40 ms per
//! round trip, reported as typing lag on macOS, where the delayed-ACK behaviour
//! is more eager. Loopback makes it look like it cannot matter, and on Linux it
//! mostly does not.
//!
//! Nothing about a forgotten `set_nodelay` fails a test or shows in a log. So
//! there is one call, and a fourth server gets it by construction.

use axum::Router;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Serve `router` on `listener` until it stops, naming the server in the log.
///
/// The handle is the caller's: every `Serving` in this crate aborts it on drop.
pub fn spawn(what: &'static str, listener: TcpListener, router: Router) -> JoinHandle<()> {
    tokio::spawn(async move {
        #[expect(
            clippy::disallowed_methods,
            reason = "this is the one call the lint exists to funnel everything else into"
        )]
        if let Err(e) = axum::serve(listener, router).tcp_nodelay(true).await {
            tracing::error!("{what} stopped serving: {e:#}");
        }
    })
}
