//! A daily usage ping, so the number of people running the app is a count rather
//! than a guess.
//!
//! **What it sends is the whole of it:** a random install id, the version, the OS
//! and the CPU architecture. Once per UTC day, from the host only — a child daemon
//! never pings, so one install with five checkouts open is one row, not five. The
//! other end is `tools/ping/worker.js`, which stores exactly those four fields and
//! deliberately not the address the request came from.
//!
//! The id is minted here, kept in `ping.json` in the config dir, and means nothing
//! outside it: it ties one day's ping to the next and to no person. Turning the
//! ping off is `"usage_ping": false` in `host.json`, which the host hands in as
//! `allowed` — passed rather than read here, because the host is what calls this
//! and the module graph is a DAG.
//!
//! **Three things never ping**, because each would count something other than a
//! person: a debug build (`cargo run` from a checkout), a test binary, and anything
//! with `CI` set — `app-check` drives the release bundle on a fresh config dir
//! every run, and each of those would be a brand-new install.
//!
//! Best effort in every direction: no network, no `curl`, a Worker that is down —
//! each is a debug line and another try next hour. It can never break a start.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where the ping goes. A `workers.dev` name because there is no domain to put it
/// under; changing it later strands only the builds already shipped.
const URL: &str = "https://orchd-ping.karsbarendrecht-dash-cloudflare-com.workers.dev/ping";

/// What `ping.json` holds between launches.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct State {
    /// The install's id: 32 lowercase hex characters, which is what the Worker
    /// accepts.
    #[serde(default)]
    id: String,
    /// The last UTC day, counted from the epoch, that a ping was accepted.
    #[serde(default)]
    day: u64,
}

/// Start pinging, once a day, for as long as the process runs.
///
/// Hourly rather than once at start, because the app is left open for days and a
/// ping only at launch would count somebody who never restarts it once.
pub fn start(allowed: fn() -> bool) {
    if cfg!(debug_assertions)
        || cfg!(any(test, feature = "test-util"))
        || std::env::var_os("CI").is_some()
    {
        return;
    }
    tokio::spawn(async move {
        loop {
            match tokio::task::spawn_blocking(move || ping_if_due(allowed)).await {
                Ok(Err(e)) => tracing::debug!("the usage ping did not go: {e:#}"),
                Err(e) => tracing::debug!("the usage ping panicked: {e}"),
                Ok(Ok(())) => {}
            }
            tokio::time::sleep(Duration::from_secs(60 * 60)).await;
        }
    });
}

/// Send today's ping, unless it has been sent or the setting says not to.
///
/// Blocking: it reads and writes a file and runs `curl`.
fn ping_if_due(allowed: fn() -> bool) -> anyhow::Result<()> {
    if !allowed() {
        return Ok(());
    }
    let path = orchd::config::Config::config_dir()?.join("ping.json");
    let mut state: State = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let today = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() / 86_400;
    if !due(&mut state, today) {
        return Ok(());
    }
    send(&state.id)?;
    // Written only after the Worker accepted it, so a ping that failed is tried
    // again next hour rather than skipped for the day.
    state.day = today;
    let text = serde_json::to_string_pretty(&state)? + "\n";
    orchd::atomic::write(&path, text.as_bytes())
}

/// Whether a ping is owed today, minting an id first when there is no valid one.
///
/// A malformed id is replaced rather than sent, because the Worker refuses it and
/// a refused ping would otherwise be retried every hour forever.
fn due(state: &mut State, today: u64) -> bool {
    let valid = state.id.len() == 32
        && state
            .id
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !valid {
        state.id = uuid::Uuid::new_v4().simple().to_string();
        state.day = 0;
    }
    state.day != today
}

/// POST one ping. `-f` so a refusal from the Worker is an error, not a success
/// with an error page in it.
fn send(id: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "id": id,
        "version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    });
    let argv: Vec<String> = [
        "curl",
        "-fsS",
        "--max-time",
        "10",
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "--data-binary",
        "@-",
        URL,
    ]
    .map(String::from)
    .into();
    // Bounded above curl's own `--max-time`, as the release check is.
    let out = orchd::proc::run_bounded_with_input(
        Path::new("/"),
        20,
        &argv,
        "the usage ping",
        Some(body.to_string().into_bytes()),
        &[],
        None,
    )?;
    anyhow::ensure!(
        out.status.success(),
        "curl said: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ping_is_owed_once_a_day_and_an_id_is_minted_once() {
        let mut state = State::default();
        assert!(due(&mut state, 20_000), "a first launch pings");
        let id = state.id.clone();
        assert_eq!(id.len(), 32);
        state.day = 20_000;
        assert!(!due(&mut state, 20_000), "the same day does not ping twice");
        assert!(due(&mut state, 20_001), "the next day does");
        assert_eq!(state.id, id, "and keeps the install's id");
    }

    #[test]
    fn an_id_the_worker_would_refuse_is_replaced_rather_than_sent() {
        let mut state = State {
            id: "NOT-AN-ID".into(),
            day: 20_000,
        };
        assert!(due(&mut state, 20_000), "a replaced id is a new install");
        assert!(state
            .id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }
}
