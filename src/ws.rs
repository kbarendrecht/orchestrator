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
    ws.on_upgrade(move |socket| pty_loop(handle, socket))
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

async fn pty_loop(handle: Arc<PtyHandle>, socket: WebSocket) {
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
                    if tx.send(Message::Binary(handle.snapshot())).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Binary(data))) => {
                    let _ = writer.write(&data);
                }
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ClientMessage>(&text) {
                        Ok(ClientMessage::Resize { rows, cols }) => {
                            let _ = writer.resize(rows, cols);
                        }
                        // Anything else is keystrokes that arrived as text.
                        Err(_) => {
                            let _ = writer.write(text.as_bytes());
                        }
                    }
                }
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                _ => {}
            },
        }
    }
}
