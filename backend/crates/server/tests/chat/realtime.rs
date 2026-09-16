//! Socket-level behaviour: authentication, the connection sequence, and fan-out.
//!
//! These drive a **real WebSocket client** against a **real server**, so the
//! handshake, the framing and the registry are all exercised end to end.

use std::time::Duration;

use jiuyue_contract::{PROTOCOL_VERSION, ResyncReason, ServerEvent};
use tokio_tungstenite::connect_async;

use crate::support::{
    TestApp, connect_socket, create_direct, expect_ack, expect_new_message, expect_resync,
    next_envelope, next_event, register, resume, send_event, send_message,
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

#[tokio::test]
async fn a_first_connection_resumes_fresh() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    // The opening heartbeat is s=1.
    next_envelope(&mut socket).await;

    // A client with nothing consumed sends the wake-up handshake with `last_seq: 0`.
    send_event(&mut socket, resume(0, None)).await;

    let answer = expect_resync(&mut socket).await;
    assert_eq!(
        answer.reason,
        ResyncReason::Fresh,
        "a first connection missed nothing at the connection layer"
    );
    assert_eq!(answer.replayed, 0);

    server.abort();
    app.cleanup().await;
}

/// A hole in the connection sequence is filled from the bounded replay buffer.
///
/// This is the server half of "a dropped message mid-stream triggers resync": a
/// client that consumed s=1 and then notices it is behind asks to resume from 1,
/// naming the connection the heartbeat told it about, and the server re-sends
/// s=2..s=4 verbatim before answering `Replayed`.
#[tokio::test]
async fn a_connection_level_gap_is_filled_from_the_replay_buffer() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut socket = connect_socket(&ws_url, &alice.tokens.access_token).await;

    let opening = next_envelope(&mut socket).await; // s=1: opening Ping
    let connection_id = match opening.event() {
        ServerEvent::Ping(ping) => ping
            .connection_id
            .expect("a server heartbeat names the connection it belongs to"),
        other => panic!("expected the opening heartbeat, got {other:?}"),
    };
    println!("[client] opening {opening:?} (connection_id={connection_id})");

    send_event(&mut socket, resume(0, None)).await;
    let fresh = expect_resync(&mut socket).await; // s=2: Resync{Fresh}
    assert_eq!(fresh.reason, ResyncReason::Fresh);
    println!(
        "[client] Resume{{last_seq:0}} -> Resync{{reason:{:?}}}",
        fresh.reason
    );

    // A real send makes s=3 the ack and s=4 the sender's own NewMessage echo.
    send_event(
        &mut socket,
        send_message(&conversation.id, "cmid-gap", "lost then replayed"),
    )
    .await;
    let ack = expect_ack(&mut socket).await;
    let echo = expect_new_message(&mut socket).await;
    assert_eq!(ack.message.id, echo.message.id);
    println!(
        "[server] sent ack s=3 (id={}) and NewMessage s=4 (id={})",
        ack.message.id, echo.message.id
    );

    // The client claims it consumed only up to s=1 (as if s=2..s=4 were dropped),
    // naming the connection those sequences came from.
    println!(
        "[client] simulated hole after s=1: Resume{{last_seq:1, connection_id:{connection_id}}}"
    );
    send_event(&mut socket, resume(1, Some(connection_id))).await;

    // The server replays s=2, s=3 and s=4 verbatim, in order...
    let replayed_fresh = next_envelope(&mut socket).await;
    assert_eq!(replayed_fresh.sequence(), 2);
    assert!(
        matches!(replayed_fresh.event(), ServerEvent::Resync(answer) if answer.reason == ResyncReason::Fresh),
        "the replayed s=2 must be the original Resync, not a new one"
    );
    println!("[client] replayed s=2 {:?}", replayed_fresh.event());

    let replayed_ack = next_envelope(&mut socket).await;
    assert_eq!(replayed_ack.sequence(), 3);
    assert!(
        matches!(replayed_ack.event(), ServerEvent::MessageAck(acked) if acked.message.id == ack.message.id),
        "the replayed ack must be the original Message"
    );
    println!("[client] replayed s=3 {:?}", replayed_ack.event());

    let replayed_echo = next_envelope(&mut socket).await;
    assert_eq!(replayed_echo.sequence(), 4);
    assert!(
        matches!(replayed_echo.event(), ServerEvent::NewMessage(message) if message.message.id == echo.message.id)
    );
    println!("[client] replayed s=4 {:?}", replayed_echo.event());

    // ...and only then answers that the connection is caught up.
    let answer = expect_resync(&mut socket).await;
    assert_eq!(answer.reason, ResyncReason::Replayed);
    assert_eq!(answer.replayed, 3, "three envelopes were replayed");
    println!(
        "[client] final Resync reason={:?} replayed={}",
        answer.reason, answer.replayed
    );

    server.abort();
    app.cleanup().await;
}

/// A position the buffer cannot prove asks for a Conversation repair.
///
/// Three shapes are unprovable: a sequence beyond anything this connection sent, a
/// position that names a *different* connection, and the reconnect handshake that
/// names no connection at all. Each must answer `Unavailable` rather than letting
/// the client believe it is caught up.
#[tokio::test]
async fn a_resume_position_the_buffer_cannot_prove_asks_for_a_repair() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut socket = connect_socket(&ws_url, &alice.tokens.access_token).await;

    let opening = next_envelope(&mut socket).await;
    let connection_id = match opening.event() {
        ServerEvent::Ping(ping) => ping
            .connection_id
            .expect("a server heartbeat names the connection it belongs to"),
        other => panic!("expected the opening heartbeat, got {other:?}"),
    };

    for (last_seq, named, why) in [
        (9_999_u64, Some(connection_id), "beyond this connection"),
        (1, Some(connection_id + 1), "a different connection"),
        (1, None, "the reconnect handshake"),
    ] {
        send_event(&mut socket, resume(last_seq, named)).await;

        let answer = expect_resync(&mut socket).await;
        assert_eq!(
            answer.reason,
            ResyncReason::Unavailable,
            "{why} must require a Conversation repair, not a silent continue"
        );
        assert_eq!(answer.replayed, 0);
    }

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn the_server_heartbeat_fires_on_the_configured_interval() {
    // Milliseconds instead of the production 30 seconds; everything else is real.
    let app = TestApp::start_with_heartbeat(Duration::from_millis(50)).await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut socket = connect_socket(&ws_url, &alice.tokens.access_token).await;

    let opening = next_envelope(&mut socket).await;
    assert_eq!(opening.sequence(), 1);
    assert!(matches!(opening.event(), ServerEvent::Ping(_)));

    // With a 50 ms period the next heartbeat is due almost immediately. A server
    // that only pinged on connect would leave this read to time out.
    let beat = next_envelope(&mut socket).await;
    assert_eq!(
        beat.sequence(),
        2,
        "the heartbeat keeps the connection sequence moving"
    );
    match beat.event() {
        ServerEvent::Ping(ping) => assert_eq!(ping.seq, beat.sequence()),
        other => panic!("the second envelope must be a heartbeat, got {other:?}"),
    }

    server.abort();
    app.cleanup().await;
}
