//! The send path: delivery, idempotency, ordering and membership.
//!
//! Every Message here travels over a **real WebSocket** into a **real database**.
//! The sequence asserts the two properties a client depends on: a Message is
//! acknowledged with the idempotency key it was sent with, and the Sequence Number
//! is unique and gapless even when two connections send at once.

use axum::http::StatusCode;
use jiuyue_contract::{ErrorCode, MessageList, ServerEvent};
use std::time::Duration;

use crate::support::{
    TestApp, collect_acks, connect_socket, count_rows, create_direct, error_code, expect_ack,
    expect_new_message, expect_rejection, get_with_token, next_envelope, next_event_within,
    register, send_event, send_message,
};

#[tokio::test]
async fn a_sent_message_reaches_the_peers_live_socket() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    // Consume the opening heartbeat first: registration is complete once it arrives,
    // so the send below cannot race the registration of Bob's connection.
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;

    send_event(
        &mut alice_socket,
        send_message(&conversation.id, "cmid-first", "你好，Bob"),
    )
    .await;

    // The sender is acknowledged with the key it sent, so it can reconcile the
    // optimistic bubble it rendered before the server saw the Message.
    let ack = expect_ack(&mut alice_socket).await;
    assert_eq!(ack.client_msg_id, "cmid-first");
    assert_eq!(ack.message.body, "你好，Bob");
    assert_eq!(ack.message.conversation_id, conversation.id);
    assert_eq!(ack.message.sender_id, alice.user.id);
    assert_eq!(ack.message.seq, 1, "the first Message takes seq 1");

    // The recipient's live socket receives it with no refresh.
    let delivered = expect_new_message(&mut bob_socket).await;
    assert_eq!(delivered.message.id, ack.message.id);
    assert_eq!(delivered.message.body, "你好，Bob");
    assert_eq!(delivered.message.sender_id, alice.user.id);

    // The sender's own connection is a Participant too, so it also receives the
    // new-message event (this is the same fan-out that reaches its other Devices).
    let echoed = expect_new_message(&mut alice_socket).await;
    assert_eq!(
        echoed.message.id, ack.message.id,
        "the sender receives NewMessage as well as the ack"
    );

    assert_eq!(count_rows(app.pool(), "messages").await, 1);

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn replaying_a_client_message_id_does_not_write_twice_and_the_ack_is_stable() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut socket).await;

    // The first send is stored...
    send_event(
        &mut socket,
        send_message(&conversation.id, "cmid-retry", "第一次"),
    )
    .await;
    let first = expect_ack(&mut socket).await;

    // ...and the retry — same idempotency key — returns the very same Message.
    send_event(
        &mut socket,
        send_message(&conversation.id, "cmid-retry", "第一次"),
    )
    .await;
    let second = expect_ack(&mut socket).await;

    assert_eq!(
        first.message.id, second.message.id,
        "a replayed send must return the original Message"
    );
    assert_eq!(first.message.seq, second.message.seq);
    assert_eq!(
        count_rows(app.pool(), "messages").await,
        1,
        "the retry must not write a second row"
    );

    server.abort();
    app.cleanup().await;
}

/// A replay is a no-op on the wire, not just in storage.
///
/// The sender still gets its stable ack — that is often the whole reason it
/// retried — but the peer must see exactly one `NewMessage` for the Message, so no
/// client has to de-duplicate by Message id.
#[tokio::test]
async fn replaying_a_client_message_id_does_not_broadcast_a_second_new_message() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;

    // First send: the peer is pushed the Message once.
    send_event(
        &mut alice_socket,
        send_message(&conversation.id, "cmid-replay", "hello from A"),
    )
    .await;
    let first_ack = expect_ack(&mut alice_socket).await;
    let first_push = expect_new_message(&mut bob_socket).await;
    assert_eq!(first_push.message.id, first_ack.message.id);

    // The exact same send again: same Conversation, same Client Message ID, same body.
    send_event(
        &mut alice_socket,
        send_message(&conversation.id, "cmid-replay", "hello from A"),
    )
    .await;
    let replay_ack = expect_ack(&mut alice_socket).await;

    assert_eq!(
        replay_ack.message.id, first_ack.message.id,
        "the replayed send must acknowledge the original Message"
    );
    assert_eq!(replay_ack.message.seq, first_ack.message.seq);

    // ...but nothing new is pushed to the peer.
    let unexpected = next_event_within(&mut bob_socket, Duration::from_millis(750)).await;
    assert!(
        unexpected.is_none(),
        "a replayed send must not fan out a second NewMessage, got {unexpected:?}"
    );

    assert_eq!(
        count_rows(app.pool(), "messages").await,
        1,
        "the replay must not write a second row"
    );

    server.abort();
    app.cleanup().await;
}

/// The concurrent variant: two Devices of one sender racing with the same key.
///
/// Exactly one insert wins, so exactly one fan-out happens. The loser acknowledges
/// the winner's Message and pushes nothing.
#[tokio::test]
async fn a_concurrent_duplicate_send_fans_out_exactly_once() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut device_one = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut device_two = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut device_one).await;
    next_envelope(&mut device_two).await;
    next_envelope(&mut bob_socket).await;

    let conversation_id = conversation.id.clone();
    let first = {
        let conversation_id = conversation_id.clone();
        tokio::spawn(async move {
            send_event(
                &mut device_one,
                send_message(&conversation_id, "cmid-race", "racing"),
            )
            .await;
            expect_ack(&mut device_one).await
        })
    };

    let second = tokio::spawn(async move {
        send_event(
            &mut device_two,
            send_message(&conversation_id, "cmid-race", "racing"),
        )
        .await;
        expect_ack(&mut device_two).await
    });

    let (one, two) = tokio::join!(first, second);
    let one = one.expect("the first device task must not panic");
    let two = two.expect("the second device task must not panic");

    assert_eq!(
        one.message.id, two.message.id,
        "both racing devices must be acknowledged with the winning Message"
    );
    assert_eq!(one.message.seq, two.message.seq);

    let pushed = expect_new_message(&mut bob_socket).await;
    assert_eq!(pushed.message.id, one.message.id);

    // Both of Alice's sockets are dropped when the spawned tasks end, so her
    // presence legitimately goes offline in this window; that is a different
    // feature. What this asserts is the thing under test: the racing duplicate
    // must not fan out a *second* NewMessage.
    let unexpected = next_event_within(&mut bob_socket, Duration::from_millis(750)).await;
    assert!(
        !matches!(unexpected, Some(ServerEvent::NewMessage(_))),
        "a racing duplicate must not fan out a second NewMessage, got {unexpected:?}"
    );

    assert_eq!(count_rows(app.pool(), "messages").await, 1);

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn concurrent_sends_produce_unique_gapless_sequence_numbers() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;

    const PER_SENDER: usize = 5;
    let conversation_id = conversation.id.clone();

    // Two independent connection tasks send at the same time. Each reads its own
    // acknowledgements; between them they cover every Sequence Number.
    let alice_task = {
        let conversation_id = conversation_id.clone();
        tokio::spawn(async move {
            for index in 0..PER_SENDER {
                let client_msg_id = format!("alice-{index}");
                send_event(
                    &mut alice_socket,
                    send_message(&conversation_id, &client_msg_id, "from alice"),
                )
                .await;
            }
            collect_acks(&mut alice_socket, PER_SENDER).await
        })
    };

    let bob_task = tokio::spawn(async move {
        for index in 0..PER_SENDER {
            let client_msg_id = format!("bob-{index}");
            send_event(
                &mut bob_socket,
                send_message(&conversation_id, &client_msg_id, "from bob"),
            )
            .await;
        }
        collect_acks(&mut bob_socket, PER_SENDER).await
    });

    let (alice_acks, bob_acks) = tokio::join!(alice_task, bob_task);
    let mut sequences: Vec<i64> = alice_acks
        .expect("the alice task must not panic")
        .into_iter()
        .chain(bob_acks.expect("the bob task must not panic"))
        .map(|ack| ack.message.seq)
        .collect();

    sequences.sort_unstable();
    let expected: Vec<i64> = (1..=(2 * PER_SENDER as i64)).collect();
    assert_eq!(
        sequences, expected,
        "concurrent sends must produce unique, gapless Sequence Numbers"
    );
    assert_eq!(
        count_rows(app.pool(), "messages").await,
        2 * PER_SENDER as i64
    );

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn a_non_participant_cannot_post_to_the_conversation() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let carol = register(&app, "carol", "carol@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut carol_socket).await;

    send_event(
        &mut carol_socket,
        send_message(&conversation.id, "cmid-intruder", "让我进去"),
    )
    .await;

    let rejection = expect_rejection(&mut carol_socket).await;
    assert_eq!(rejection.client_msg_id, "cmid-intruder");
    assert_eq!(rejection.code, ErrorCode::NotAParticipant);
    assert_eq!(
        count_rows(app.pool(), "messages").await,
        0,
        "a non-participant's Message must not be stored"
    );

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn a_non_participant_cannot_read_history() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let carol = register(&app, "carol", "carol@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (status, body) = get_with_token(
        &app,
        &format!("/conversations/{}/messages", conversation.id),
        &carol.tokens.access_token,
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");
    assert_eq!(error_code(&body), "NOT_A_PARTICIPANT");

    app.cleanup().await;
}

#[tokio::test]
async fn an_empty_body_is_rejected_without_storing_a_message() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut socket).await;

    send_event(
        &mut socket,
        send_message(&conversation.id, "cmid-empty", "   "),
    )
    .await;

    let rejection = expect_rejection(&mut socket).await;
    assert_eq!(rejection.code, ErrorCode::ValidationFailed);
    assert_eq!(count_rows(app.pool(), "messages").await, 0);

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn history_returns_the_recent_messages_oldest_first() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut socket).await;

    for (index, body) in ["一", "二", "三"].iter().enumerate() {
        send_event(
            &mut socket,
            send_message(&conversation.id, &format!("cmid-{index}"), body),
        )
        .await;
    }
    let acks = collect_acks(&mut socket, 3).await;
    assert_eq!(
        acks.iter().map(|ack| ack.message.seq).collect::<Vec<_>>(),
        [1, 2, 3]
    );

    let (status, body) = get_with_token(
        &app,
        &format!("/conversations/{}/messages", conversation.id),
        &alice.tokens.access_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    let list: MessageList = serde_json::from_value(body).expect("MessageList");

    assert_eq!(list.messages.len(), 3);
    assert_eq!(
        list.messages
            .iter()
            .map(|message| message.seq)
            .collect::<Vec<_>>(),
        [1, 2, 3],
        "history must be ordered oldest first"
    );
    assert_eq!(
        list.messages
            .iter()
            .map(|message| message.body.as_str())
            .collect::<Vec<_>>(),
        ["一", "二", "三"]
    );

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn sending_to_an_unknown_conversation_is_rejected() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut socket).await;

    send_event(
        &mut socket,
        send_message("01JABC1234567890ABCDEFGHJ9", "cmid-ghost", "hello?"),
    )
    .await;

    let rejection = expect_rejection(&mut socket).await;
    assert_eq!(rejection.code, ErrorCode::NotFound);
    assert_eq!(count_rows(app.pool(), "messages").await, 0);

    server.abort();
    app.cleanup().await;
}
