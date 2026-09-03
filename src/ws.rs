use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures::{sink::SinkExt, stream::StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::model::{State as SessionState, TurnReason};
use crate::pty::PtyHandle;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct WsQuery {
    pub token: String,
    #[serde(default)]
    pub target: Option<String>,
}

/// Browsers cannot set headers on a WebSocket handshake, so the token rides in
/// the query string. It never leaves the loopback interface (§12).
fn authorised(app: &AppState, q: &WsQuery) -> bool {
    q.token == app.token
}

// ---------------------------------------------------------------------------
// State stream
// ---------------------------------------------------------------------------

pub async fn events(
    State(app): State<Arc<AppState>>,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !authorised(&app, &q) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    ws.on_upgrade(move |socket| events_loop(app, socket))
}

async fn events_loop(app: Arc<AppState>, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let mut sub = app.events.subscribe();

    // The SPA is stateless and disposable: it gets a full snapshot on connect
    // rather than replaying a delta log (§1).
    if let Ok(json) = serde_json::to_string(&app.snapshot().await) {
        if tx.send(Message::Text(json)).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            msg = sub.recv() => match msg {
                Ok(json) => {
                    if tx.send(Message::Text(json)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Snapshots are whole, so a dropped one costs nothing but
                    // freshness; send the current state and carry on.
                    if let Ok(json) = serde_json::to_string(&app.snapshot().await) {
                        if tx.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            },
            incoming = rx.next() => match incoming {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Pty attach
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Resize { rows: u16, cols: u16 },
}

pub async fn pty(
    State(app): State<Arc<AppState>>,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if !authorised(&app, &q) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    let Some(target) = q.target.clone() else {
        return (StatusCode::BAD_REQUEST, "missing target").into_response();
    };
    let Some(handle) = resolve(&app, &target).await else {
        return (StatusCode::NOT_FOUND, "no such pty").into_response();
    };
    // A session's pane, as opposed to the drawer's shells: only a session has a turn
    // to interrupt. Parsed from the target rather than looked up again, since
    // `resolve` has just proved this one exists.
    let session = target
        .strip_prefix("session:")
        .and_then(|rest| Uuid::parse_str(rest).ok());
    ws.on_upgrade(move |socket| pty_loop(app, session, target, handle, socket))
}

/// Targets are named and enumerated — there is no endpoint that runs an
/// arbitrary command (§12).
async fn resolve(app: &Arc<AppState>, target: &str) -> Option<Arc<PtyHandle>> {
    let inner = app.inner.read().await;
    if let Some(rest) = target.strip_prefix("session:") {
        let id = Uuid::parse_str(rest).ok()?;
        return inner.sessions.get(&id).and_then(|s| s.pty.clone());
    }
    if let Some(rest) = target.strip_prefix("proc:") {
        return inner
            .workspaces
            .values()
            .flat_map(|w| w.processes.iter())
            .find(|p| p.id == rest)
            .and_then(|p| p.pty.clone());
    }
    None
}

/// Whether these bytes are you telling the agent to stop.
///
/// `ESC` and `^C` both do it in Claude Code's TUI, and the pane is the only way
/// either reaches the pty.
///
/// **A *bare* escape, not an escape anywhere in the chunk.** `0x1b` is the first
/// byte of every escape sequence a terminal sends — arrow keys are `ESC [ A`,
/// bracketed paste opens `ESC [ 200 ~`, and a TUI with mouse reporting on emits one
/// per movement. Matching on "contains" would have read all of that as an
/// interrupt, so pressing Up mid-turn, or just moving the mouse across the pane,
/// would have declared the turn over. Alt+key is `ESC` plus the key, and is
/// correctly not one either.
///
/// `^C` needs no such care: `0x03` is not a byte any sequence carries.
fn is_interrupt(data: &[u8]) -> bool {
    data == [0x1b] || data.contains(&0x03)
}

/// Move a session off `Working` because you just cut its turn short.
///
/// **The only signal there is.** Claude Code's `Stop` hook does not fire on a user
/// interrupt, and the `idle_prompt` notification did not arrive either — so without
/// this the session sits in `Working` until it happens to finish a *later* turn, and
/// a conversation you abandoned mid-thought sits there for good. The pane is where
/// the escape is typed, so the pane is where the daemon can know.
///
/// Only out of `Working`: an escape at the prompt dismisses something, an escape at
/// a question cancels it (`api::rewind` refuses for exactly these reasons), and
/// neither is a turn ending.
async fn note_interrupt(app: &Arc<AppState>, id: Uuid) {
    {
        let mut inner = app.inner.write().await;
        match inner.sessions.get_mut(&id) {
            Some(s) if matches!(s.state, SessionState::Working) => {
                s.set_state(SessionState::YourTurn {
                    since: std::time::SystemTime::now(),
                    reason: TurnReason::Interrupted,
                })
            }
            _ => return,
        }
    }
    app.notify().await;
}

#[cfg(test)]
mod tests {
    use super::is_interrupt;

    /// The distinction the whole detector turns on: `0x1b` leads every escape
    /// sequence a terminal sends, so anything looser than "the chunk *is* an
    /// escape" reads ordinary typing as the end of a turn.
    #[test]
    fn only_a_bare_escape_or_a_ctrl_c_counts() {
        assert!(is_interrupt(b"\x1b"), "the escape key");
        assert!(is_interrupt(b"\x03"), "^C");

        // Every one of these arrives while you are watching a turn, and none of
        // them means stop.
        for ordinary in [
            &b"\x1b[A"[..],       // up arrow
            b"\x1b[B",            // down
            b"\x1b[200~hi\x1b[201~", // bracketed paste
            b"\x1b[M   ",         // a mouse report, one per movement
            b"\x1bb",             // alt+b
            b"hello",
            b"",
        ] {
            assert!(!is_interrupt(ordinary), "{ordinary:?} is not an interrupt");
        }
    }
}

async fn pty_loop(
    app: Arc<AppState>,
    session: Option<Uuid>,
    target: String,
    handle: Arc<PtyHandle>,
    socket: WebSocket,
) {
    // The one place a pane going deaf becomes a line somebody can read. The SPA
    // reconnects a dropped pty socket on its own, but "typing does nothing" used to
    // leave no trace at either end (#7); this names the client attaching and, below,
    // why it left.
    tracing::info!(%target, "pty client attached");
    let (mut tx, mut rx) = socket.split();

    // Subscribe before replaying, so output produced during the replay is
    // queued rather than lost.
    let mut sub = handle.subscribe();
    let snapshot = handle.snapshot();
    if !snapshot.is_empty() && tx.send(Message::Binary(snapshot)).await.is_err() {
        return;
    }

    let writer = handle.clone();
    let exit = handle.clone();
    // Why the loop ended, for the detach line below. Defaults to the client going,
    // which is every reconnect, a tab close and a window shutdown; the process
    // ending overrides it.
    let mut reason = "client left";
    loop {
        tokio::select! {
            // The broadcast Sender lives in the PtyHandle, so `recv()` never
            // errors on its own — without this the socket would outlive the
            // process it is attached to, and a drawer full of dead shells would
            // keep as many open sockets as it had corpses.
            _ = exit.wait() => {
                // The reader thread may still be draining the pty when the
                // child is reaped, so give it a moment before taking what is
                // left; otherwise the last lines of output are lost.
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                while let Ok(bytes) = sub.try_recv() {
                    if tx.send(Message::Binary(bytes.to_vec())).await.is_err() {
                        break;
                    }
                }
                reason = "process exited";
                break;
            }
            out = sub.recv() => match out {
                Ok(bytes) => {
                    if tx.send(Message::Binary(bytes.to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // A client too slow to keep up resyncs from the ring buffer
                    // rather than receiving a torn stream.
                    //
                    // **Resubscribe first, or the resync duplicates output.** After
                    // `Lagged`, tokio leaves the receiver at the *oldest retained*
                    // chunk — so the next several `recv`s replay bytes the snapshot
                    // below already carries, and the pane shows them twice.
                    // Resubscribing drops that backlog and starts from now, which is
                    // the same subscribe-then-snapshot order the initial attach uses.
                    sub = sub.resubscribe();
                    if tx.send(Message::Binary(handle.snapshot())).await.is_err() {
                        break;
                    }
                }
                Err(_) => { reason = "process exited"; break }
            },
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Binary(data))) => {
                    let _ = writer.write(&data);
                    if let Some(id) = session.filter(|_| is_interrupt(&data)) {
                        note_interrupt(&app, id).await;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ClientMessage>(&text) {
                        Ok(ClientMessage::Resize { rows, cols }) => {
                            let _ = writer.resize(rows, cols);
                        }
                        // Anything else is keystrokes that arrived as text.
                        Err(_) => {
                            let _ = writer.write(text.as_bytes());
                            if let Some(id) = session.filter(|_| is_interrupt(text.as_bytes())) {
                                note_interrupt(&app, id).await;
                            }
                        }
                    }
                }
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            },
        }
    }
    tracing::info!(%target, reason, "pty client detached");
}
