//! The per-socket transport loop.
//!
//! [`serve_connection`] is the whole per-socket lifecycle: split the socket, run a
//! writer task over a bounded queue, register in the hub's registry, then pump
//! heartbeats, fanned-out events and client frames until the socket closes. All
//! stateful work — sequencing, replay, the resume handshake — lives in
//! `session`; this module only drives it.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use jiuyue_contract::{ServerEnvelope, ServerEvent};
use tokio::sync::mpsc;

use crate::session::Session;
use crate::{CONTROL_QUEUE_CAPACITY, RealtimeError, RealtimeHub, SEND_QUEUE_CAPACITY};

/// Serve one authenticated connection, logging (not panicking) on failure.
///
/// `user_id` must come from a verified access token; this function never sees the
/// token itself.
pub async fn serve_connection(socket: WebSocket, user_id: String, hub: Arc<RealtimeHub>) {
    if let Err(error) = run_connection(socket, user_id, hub).await {
        tracing::debug!(%error, "realtime connection ended");
    }
}

/// Assign the connection sequence, push the opening heartbeat, then pump frames
/// and fanned-out events until the socket closes.
async fn run_connection(
    socket: WebSocket,
    user_id: String,
    hub: Arc<RealtimeHub>,
) -> Result<(), RealtimeError> {
    let (mut sink, mut stream) = socket.split();
    let (envelope_tx, mut envelope_rx) = mpsc::channel::<ServerEnvelope>(SEND_QUEUE_CAPACITY);

    // Writer task: drains the bounded queue onto the socket and closes the sink
    // when the connection ends. Holding the sink here leaves the read side free.
    let writer = tokio::spawn(async move {
        while let Some(envelope) = envelope_rx.recv().await {
            let text = match serde_json::to_string(&envelope) {
                Ok(text) => text,
                Err(error) => {
                    tracing::error!(%error, "failed to encode a server envelope");
                    continue;
                }
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // The registry holds the only control sender: fan-out reaches this connection
    // through the same bounded queue as any other Device of this User.
    let (control_tx, mut control_rx) = mpsc::channel::<ServerEvent>(CONTROL_QUEUE_CAPACITY);
    let connection_id = hub.registry().register(user_id.clone(), control_tx).await;

    let mut session = Session::new(
        &user_id,
        &hub,
        envelope_tx,
        hub.replay_capacity(),
        connection_id.value(),
    );

    session.send_ping().await?;

    let mut heartbeat = tokio::time::interval(hub.heartbeat_interval());
    // The first interval tick is immediate; the opening heartbeat already fired.
    heartbeat.tick().await;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if session.send_ping().await.is_err() {
                    break;
                }
            }
            incoming = control_rx.recv() => match incoming {
                Some(event) => {
                    if session.send_event(event).await.is_err() {
                        break;
                    }
                }
                // The registry dropped the sender: this connection is over.
                None => break,
            },
            frame = stream.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    if session.handle_frame(text.as_str()).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    tracing::debug!(%error, "realtime receive error");
                    break;
                }
            },
        }
    }

    hub.registry().unregister(&user_id, connection_id).await;

    // Dropping the session drops the writer's sender, ending that task; awaiting it
    // lets the close frame flush before the connection is torn down.
    drop(session);
    let _ = writer.await;

    Ok(())
}
