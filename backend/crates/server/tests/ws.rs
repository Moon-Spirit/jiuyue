//! Integration test for the realtime endpoint.
//!
//! Binds a real TCP listener, serves the real application router and connects a
//! real WebSocket client — no mocks. The upgrade handshake, framing limits and
//! envelope serialisation are all exercised end to end.

use futures_util::StreamExt;
use jiuyue_contract::{PROTOCOL_VERSION, ServerEnvelope, ServerEvent};
use jiuyue_server::{AppState, Config, app};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;

/// Bind the real router to an ephemeral port and return its address.
async fn serve_app() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the listener must bind");

    let address = listener
        .local_addr()
        .expect("the listener must report its address");

    let server = tokio::spawn(async move {
        axum::serve(listener, app(AppState::new(Config::default())))
            .await
            .expect("the server must run");
    });

    (format!("ws://{address}/ws"), server)
}

/// Connect and return the first text frame the server sends.
async fn first_frame(url: &str) -> String {
    let (mut socket, _response) = connect_async(url).await.expect("the client must connect");

    let frame = socket
        .next()
        .await
        .expect("a frame must arrive")
        .expect("the frame must be valid");

    frame
        .into_text()
        .expect("the frame must be text")
        .to_string()
}

#[tokio::test]
async fn ws_sends_a_ping_envelope_on_connect() {
    let (url, server) = serve_app().await;

    let text = first_frame(&url).await;

    // The wire shape is part of the contract: single-letter keys and an
    // adjacently tagged event.
    let raw: serde_json::Value =
        serde_json::from_str(&text).expect("the frame must be a JSON object");
    assert_eq!(raw["v"], PROTOCOL_VERSION);
    assert_eq!(raw["s"], 1);
    assert_eq!(raw["e"]["t"], "Ping");
    assert_eq!(raw["e"]["d"]["seq"], 1);

    let envelope: ServerEnvelope =
        serde_json::from_str(&text).expect("the frame must decode as an envelope");
    assert_eq!(envelope.version(), PROTOCOL_VERSION);
    assert_eq!(envelope.sequence(), 1);
    assert!(
        envelope.timestamp_ms() > 0,
        "the server must stamp a real timestamp"
    );

    match envelope.event() {
        ServerEvent::Ping(ping) => {
            assert_eq!(ping.seq, envelope.sequence());
            assert_eq!(ping.time_ms, envelope.timestamp_ms());
        }
    }

    server.abort();
}

#[tokio::test]
async fn sequence_is_per_connection_not_global() {
    let (url, server) = serve_app().await;

    let first = first_frame(&url).await;
    let second = first_frame(&url).await;

    let first: ServerEnvelope = serde_json::from_str(&first).expect("first envelope must decode");
    let second: ServerEnvelope =
        serde_json::from_str(&second).expect("second envelope must decode");

    assert_eq!(
        first.sequence(),
        1,
        "a fresh connection starts its own sequence"
    );
    assert_eq!(
        second.sequence(),
        1,
        "the sequence is per connection, not global"
    );

    server.abort();
}
