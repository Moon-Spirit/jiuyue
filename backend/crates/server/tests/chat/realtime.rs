//! Socket-level behaviour: authentication, the connection sequence, and fan-out.
//!
//! These drive a **real WebSocket client** against a **real server**, so the
//! handshake, the framing and the registry are all exercised end to end.

use jiuyue_contract::{PROTOCOL_VERSION, ServerEvent};
use tokio_tungstenite::connect_async;

use crate::support::{
    TestApp, connect_socket, create_direct, expect_ack, expect_new_message, next_envelope,
    next_event, register, send_event, send_message,
};

#[tokio::test]
async fn an_unauthenticated_socket_is_rejected() {
    let app = TestApp::start().await;
    let (ws_url, server) = app.serve_websocket().await;

    let anonymous = connect_async(ws_url.clone()).await;
    assert!(
        anonymous.is_err(),
        "a connection with no token must be refused, not silently accepted"
    );

    let forged = connect_async(format!("{ws_url}?token=not-a-real-token")).await;
    assert!(
        forged.is_err(),
        "a connection with an unverifiable token must be refused"
    );

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn an_authenticated_socket_receives_a_ping_envelope_on_connect() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut socket = connect_socket(&ws_url, &alice.tokens.access_token).await;

    let envelope = next_envelope(&mut socket).await;

    // The wire shape is part of the contract: single-letter keys and an
    // adjacently tagged event.
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
        other => panic!("the first event must be a heartbeat, got {other:?}"),
    }

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn the_connection_sequence_is_per_connection_not_global() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut first = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut second = connect_socket(&ws_url, &alice.tokens.access_token).await;

    let first = next_envelope(&mut first).await;
    let second = next_envelope(&mut second).await;

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
    app.cleanup().await;
}

#[tokio::test]
async fn a_new_conversation_reaches_the_peers_live_socket() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    // Registration is complete once the opening heartbeat arrives.
    next_envelope(&mut bob_socket).await;

    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    match next_event(&mut bob_socket).await {
        ServerEvent::ConversationCreated(created) => {
            assert_eq!(created.conversation.id, conversation.id);
            assert_eq!(
                created
                    .conversation
                    .peer
                    .as_ref()
                    .map(|peer| peer.username.as_str()),
                Some("alice"),
                "Bob sees Alice as the peer"
            );
        }
        other => panic!("expected ConversationCreated, got {other:?}"),
    }

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn fanout_reaches_every_device_of_the_sender() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    // Two logins for the same User are two Devices with two independent sockets.
    let mut device_one = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut device_two = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut device_one).await;
    next_envelope(&mut device_two).await;

    send_event(
        &mut device_one,
        send_message(&conversation.id, "cmid-multi", "hello from my phone"),
    )
    .await;

    let ack = expect_ack(&mut device_one).await;

    // The other Device receives the Message even though it sent nothing.
    let on_other_device = expect_new_message(&mut device_two).await;
    assert_eq!(on_other_device.message.id, ack.message.id);
    assert_eq!(on_other_device.message.body, "hello from my phone");

    server.abort();
    app.cleanup().await;
}
