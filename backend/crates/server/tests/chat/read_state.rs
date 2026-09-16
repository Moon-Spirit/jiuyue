//! Read state: the private Read Marker, the public Read Receipt, and the Unread
//! Count (ticket #16).
//!
//! Everything runs against a **real PostgreSQL** and **real WebSocket clients**.
//! The point of the suite is not merely that a badge changes, but that the two
//! read concepts stay apart:
//!
//! - a new Message raises the recipient's Unread Count, and reading clears it;
//! - reading on one Device clears the count on the account's other Devices;
//! - the other Participant observes the public Read Receipt advance;
//! - **a private Read Marker is never delivered to another Participant** — the
//!   peer's socket is drained and asserted to contain no `ReadMarker`;
//! - concurrent sends land on the correct count;
//! - the positions survive a process restart.

use std::time::Duration;

use jiuyue_chat::ChatService;
use jiuyue_contract::{ConversationList, MessageList, ReadReceipt, ServerEvent};
use tokio::task::JoinHandle;

use crate::support::{
    TestApp, TestSocket, collect_acks, collect_events_within, connect_socket, count_rows,
    create_direct, expect_ack, expect_new_message, expect_read_marker, expect_read_receipt,
    get_with_token, login, mark_read, member_read_state, next_envelope, register, send_event,
    send_message, wait_for_unread,
};

/// Open a Direct Conversation with a live socket on each side, opening heartbeats
/// already consumed.
async fn connected_pair(
    app: &TestApp,
    alice_token: &str,
    bob_token: &str,
) -> (JoinHandle<()>, TestSocket, TestSocket) {
    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, alice_token).await;
    let mut bob_socket = connect_socket(&ws_url, bob_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;
    (server, alice_socket, bob_socket)
}

/// A new Message raises the recipient's Unread Count; reading clears it to zero.
#[tokio::test]
async fn a_new_message_raises_the_unread_count_and_reading_clears_it() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (server, mut alice_socket, mut bob_socket) =
        connected_pair(&app, &alice.tokens.access_token, &bob.tokens.access_token).await;

    send_event(
        &mut bob_socket,
        send_message(&conversation.id, "raise-1", "你好"),
    )
    .await;
    let ack = expect_ack(&mut bob_socket).await;
    let delivered = expect_new_message(&mut alice_socket).await;
    assert_eq!(delivered.message.id, ack.message.id);

    // The badge is raised, in the schema and in the Conversation list alike.
    wait_for_unread(app.pool(), &conversation.id, &alice.user.id, 1).await;
    let (marker, receipt, unread) =
        member_read_state(app.pool(), &conversation.id, &alice.user.id).await;
    assert_eq!(unread, 1);
    assert_eq!(marker, 0, "the marker has not advanced yet");
    assert_eq!(receipt, 0, "nor has the public receipt");
    println!("[psql] alice before reading: marker={marker} receipt={receipt} unread={unread}");

    let (status, body) = get_with_token(&app, "/conversations", &alice.tokens.access_token).await;
    assert_eq!(status, axum::http::StatusCode::OK, "conversations: {body}");
    let list: ConversationList = serde_json::from_value(body).expect("ConversationList");
    assert_eq!(list.conversations[0].unread_count, 1);
    println!(
        "[alice] GET /conversations -> unread_count={}",
        list.conversations[0].unread_count
    );

    // Reading the Conversation: the private marker comes back to Alice...
    send_event(&mut alice_socket, mark_read(&conversation.id, 1)).await;
    let marker_event = expect_read_marker(&mut alice_socket).await;
    assert_eq!(marker_event.conversation_id, conversation.id);
    assert_eq!(marker_event.last_read_seq, 1);
    assert_eq!(marker_event.unread_count, 0);
    println!("[alice] ReadMarker -> {marker_event:?}");

    // ...and the public receipt reaches the peer.
    let receipt_event = expect_read_receipt(&mut bob_socket).await;
    assert_eq!(receipt_event.conversation_id, conversation.id);
    assert_eq!(receipt_event.reader_id, alice.user.id);
    assert_eq!(receipt_event.last_read_seq, 1);
    println!("[bob] ReadReceipt -> {receipt_event:?}");

    // The badge is cleared in storage and in the list.
    wait_for_unread(app.pool(), &conversation.id, &alice.user.id, 0).await;
    let (marker, receipt, unread) =
        member_read_state(app.pool(), &conversation.id, &alice.user.id).await;
    assert_eq!(unread, 0);
    assert_eq!(marker, 1);
    assert_eq!(receipt, 1);
    println!("[psql] alice after reading: marker={marker} receipt={receipt} unread={unread}");

    let (_status, body) = get_with_token(&app, "/conversations", &alice.tokens.access_token).await;
    let list: ConversationList = serde_json::from_value(body).expect("ConversationList");
    assert_eq!(list.conversations[0].unread_count, 0);

    server.abort();
    app.cleanup().await;
}

/// Reading on Device A clears the Unread Count on Device B of the same account.
#[tokio::test]
async fn reading_on_one_device_clears_the_unread_count_on_the_accounts_other_device() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    // A second login is a second Device (a second `sessions` row).
    let alice_second = login(&app, "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut device_one = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut device_two = connect_socket(&ws_url, &alice_second.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut device_one).await;
    next_envelope(&mut device_two).await;
    next_envelope(&mut bob_socket).await;

    send_event(
        &mut bob_socket,
        send_message(&conversation.id, "multi-1", "两台设备"),
    )
    .await;
    let ack = expect_ack(&mut bob_socket).await;

    // Both Devices of Alice receive the Message, so both badges are raised.
    let first = expect_new_message(&mut device_one).await;
    let second = expect_new_message(&mut device_two).await;
    assert_eq!(first.message.id, ack.message.id);
    assert_eq!(second.message.id, ack.message.id);
    wait_for_unread(app.pool(), &conversation.id, &alice.user.id, 1).await;
    println!(
        "[alice] both devices see seq {} and a shared unread of 1",
        ack.message.seq
    );

    // Device one reads. The marker is per **User**, so the server echoes it to
    // every Device of the account — including device two, which clears its badge
    // without ever having opened the Conversation.
    send_event(&mut device_one, mark_read(&conversation.id, 1)).await;

    let on_reader = expect_read_marker(&mut device_one).await;
    let on_other = expect_read_marker(&mut device_two).await;
    assert_eq!(on_reader.unread_count, 0);
    assert_eq!(on_other.unread_count, 0);
    assert_eq!(on_other.last_read_seq, 1);
    println!("[alice/device1] ReadMarker -> {on_reader:?}");
    println!("[alice/device2] ReadMarker -> {on_other:?}");

    // The sender observes the peer's public receipt, while the private marker
    // never left Alice's account.
    let receipt = expect_read_receipt(&mut bob_socket).await;
    assert_eq!(receipt.reader_id, alice.user.id);
    assert_eq!(receipt.last_read_seq, 1);
    println!("[bob] ReadReceipt -> {receipt:?}");

    wait_for_unread(app.pool(), &conversation.id, &alice.user.id, 0).await;

    server.abort();
    app.cleanup().await;
}

/// The sender observes the peer's public Read Receipt position advancing.
#[tokio::test]
async fn the_sender_observes_the_peers_read_receipt_position_advancing() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (server, mut alice_socket, mut bob_socket) =
        connected_pair(&app, &alice.tokens.access_token, &bob.tokens.access_token).await;

    for (index, seq) in [(0, 1_i64), (1, 2_i64)] {
        send_event(
            &mut bob_socket,
            send_message(&conversation.id, &format!("receipt-{index}"), "读我"),
        )
        .await;
        expect_ack(&mut bob_socket).await;
        expect_new_message(&mut alice_socket).await;

        // Alice reads one Message at a time; Bob watches the receipt move.
        send_event(&mut alice_socket, mark_read(&conversation.id, seq)).await;
        let receipt = expect_read_receipt(&mut bob_socket).await;
        assert_eq!(receipt.reader_id, alice.user.id);
        assert_eq!(
            receipt.last_read_seq, seq,
            "the receipt must name the position Alice acknowledged"
        );
        println!("[bob] ReadReceipt after alice read seq {seq} -> {receipt:?}");
    }

    server.abort();
    app.cleanup().await;
}

/// **The privacy assertion**: a private Read Marker never reaches another
/// Participant.
///
/// Alice reads; her private marker is echoed to her *other* Device, and the peer
/// is drained for a window. The peer must receive the public receipt and **no**
/// `ReadMarker` at all — the private read position has no path to another User.
#[tokio::test]
async fn a_private_read_marker_never_reaches_another_participant() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let alice_second = login(&app, "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut alice_other = connect_socket(&ws_url, &alice_second.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut alice_other).await;
    next_envelope(&mut bob_socket).await;

    // Bob sends two Messages and consumes his own acks (and echoes).
    for index in 0..2 {
        send_event(
            &mut bob_socket,
            send_message(&conversation.id, &format!("privacy-{index}"), "私有已读"),
        )
        .await;
    }
    let acks = collect_acks(&mut bob_socket, 2).await;
    let last_seq = acks.last().expect("two acks").message.seq;
    println!("[bob] stored two Messages, last seq={last_seq}");

    // Alice reads on one Device.
    send_event(&mut alice_socket, mark_read(&conversation.id, last_seq)).await;

    // Her marker is private *to her account*: the other Device gets it...
    let marker = expect_read_marker(&mut alice_other).await;
    assert_eq!(marker.last_read_seq, last_seq);
    println!("[alice/device2] ReadMarker -> {marker:?}");

    // ...and the peer gets only the public receipt, never the marker.
    let seen = collect_events_within(&mut bob_socket, Duration::from_millis(750)).await;
    println!("[bob] events observed -> {seen:?}");

    assert!(
        seen.iter()
            .any(|event| matches!(event, ServerEvent::ReadReceipt(_))),
        "the peer must observe the public Read Receipt"
    );
    assert!(
        seen.iter()
            .all(|event| !matches!(event, ServerEvent::ReadMarker(_))),
        "PRIVACY: a private Read Marker must never reach another Participant, got {seen:?}"
    );

    let receipt = seen
        .iter()
        .find_map(|event| match event {
            ServerEvent::ReadReceipt(receipt) => Some(receipt.clone()),
            _ => None,
        })
        .expect("the peer saw a receipt");
    assert_eq!(receipt.reader_id, alice.user.id);
    assert_eq!(receipt.last_read_seq, last_seq);

    server.abort();
    app.cleanup().await;
}

/// The peer-facing read model returns only public receipts, never the caller's own
/// private marker.
#[tokio::test]
async fn the_message_page_exposes_only_the_peers_public_receipt() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (server, mut alice_socket, mut bob_socket) =
        connected_pair(&app, &alice.tokens.access_token, &bob.tokens.access_token).await;

    send_event(
        &mut bob_socket,
        send_message(&conversation.id, "page-1", "第一页"),
    )
    .await;
    expect_ack(&mut bob_socket).await;
    expect_new_message(&mut alice_socket).await;

    // Both sides read, so both have a private marker AND a public receipt at 1.
    send_event(&mut alice_socket, mark_read(&conversation.id, 1)).await;
    expect_read_marker(&mut alice_socket).await;
    expect_read_receipt(&mut bob_socket).await;

    send_event(&mut bob_socket, mark_read(&conversation.id, 1)).await;
    expect_read_marker(&mut bob_socket).await;
    expect_read_receipt(&mut alice_socket).await;

    // Alice fetches the page: it carries Bob's receipt and does not carry Alice's
    // own marker (a user's own private position is not a shipment on the page).
    let path = format!("/conversations/{}/messages", conversation.id);
    let (status, body) = get_with_token(&app, &path, &alice.tokens.access_token).await;
    assert_eq!(status, axum::http::StatusCode::OK, "messages: {body}");
    let page: MessageList = serde_json::from_value(body).expect("MessageList");

    assert_eq!(
        page.read_receipts,
        vec![ReadReceipt {
            conversation_id: conversation.id.clone(),
            reader_id: bob.user.id.clone(),
            last_read_seq: 1,
        }],
        "the page must carry the peer's public receipt, and only that"
    );
    println!(
        "[alice] message page read_receipts -> {:?}",
        page.read_receipts
    );

    // The caller's own marker is still 1 in storage, proving the page did not
    // expose it as if it were a receipt.
    let (marker, _, _) = member_read_state(app.pool(), &conversation.id, &alice.user.id).await;
    assert_eq!(marker, 1);

    server.abort();
    app.cleanup().await;
}

/// Concurrent sends all land on the same Unread Count, without a lost update.
#[tokio::test]
async fn concurrent_sends_land_on_the_correct_unread_count() {
    const BURST: usize = 5;

    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    // One extra Device per additional send, so the writes really are concurrent:
    // a single connection handles its own frames in order.
    let mut bob_tokens = vec![bob.tokens.access_token.clone()];
    for _ in 1..BURST {
        bob_tokens.push(
            login(&app, "bob@example.com", "secret123")
                .await
                .tokens
                .access_token,
        );
    }
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;

    let mut sends = Vec::new();
    for (index, token) in bob_tokens.into_iter().enumerate() {
        let mut socket = connect_socket(&ws_url, &token).await;
        next_envelope(&mut socket).await;
        let conversation_id = conversation.id.clone();
        sends.push(tokio::spawn(async move {
            send_event(
                &mut socket,
                send_message(&conversation_id, &format!("burst-{index}"), "并发"),
            )
            .await;
            expect_ack(&mut socket).await;
        }));
    }

    for send in sends {
        send.await.expect("the concurrent send task must finish");
    }

    assert_eq!(
        count_rows(app.pool(), "messages").await as usize,
        BURST,
        "every concurrent send must have stored exactly one Message"
    );
    wait_for_unread(app.pool(), &conversation.id, &alice.user.id, BURST as i64).await;
    let (marker, _, unread) = member_read_state(app.pool(), &conversation.id, &alice.user.id).await;
    assert_eq!(
        unread, BURST as i64,
        "N concurrent sends must produce N, not a lost update"
    );
    assert_eq!(marker, 0, "nothing has been read yet");
    println!("[psql] alice unread after {BURST} concurrent sends -> {unread} (marker={marker})");

    // A read settles it back to zero.
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    send_event(&mut alice_socket, mark_read(&conversation.id, BURST as i64)).await;
    let settled = expect_read_marker(&mut alice_socket).await;
    assert_eq!(settled.unread_count, 0);
    wait_for_unread(app.pool(), &conversation.id, &alice.user.id, 0).await;

    server.abort();
    app.cleanup().await;
}

/// The two positions and the count survive a process restart: they are rows, not
/// memory.
///
/// A fresh [`ChatService`] is the proxy for a restart — its in-memory state is
/// empty, so anything it reads came from PostgreSQL.
#[tokio::test]
async fn read_state_survives_a_process_restart() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (server, mut alice_socket, mut bob_socket) =
        connected_pair(&app, &alice.tokens.access_token, &bob.tokens.access_token).await;

    for index in 0..2 {
        send_event(
            &mut bob_socket,
            send_message(&conversation.id, &format!("restart-{index}"), "持久化"),
        )
        .await;
        expect_ack(&mut bob_socket).await;
        expect_new_message(&mut alice_socket).await;
    }
    send_event(&mut alice_socket, mark_read(&conversation.id, 2)).await;
    expect_read_marker(&mut alice_socket).await;
    wait_for_unread(app.pool(), &conversation.id, &alice.user.id, 0).await;

    // A brand-new service over the same database: no memory of the hub, the
    // sockets or the read.
    let restarted = ChatService::new(app.pool().clone());

    let state = restarted
        .read_state(&alice.user.id, &conversation.id)
        .await
        .expect("a fresh service must read the persisted read state")
        .expect("alice is a participant");
    assert_eq!(state.read_marker_seq, 2);
    assert_eq!(state.read_receipt_seq, 2);
    assert_eq!(state.unread_count, 0);
    println!("[restart] alice read state -> {state:?}");

    // The peer's public receipt is readable from the fresh service too.
    let receipts = restarted
        .list_receipts(&bob.user.id, &conversation.id)
        .await
        .expect("a fresh service must read the persisted receipt");
    assert_eq!(
        receipts,
        vec![ReadReceipt {
            conversation_id: conversation.id.clone(),
            reader_id: alice.user.id.clone(),
            last_read_seq: 2,
        }]
    );
    println!("[restart] bob sees alice's receipt -> {receipts:?}");

    // The persisted marker still guards the count: a further Message raises it.
    send_event(
        &mut bob_socket,
        send_message(&conversation.id, "restart-after", "重启之后"),
    )
    .await;
    expect_ack(&mut bob_socket).await;
    wait_for_unread(app.pool(), &conversation.id, &alice.user.id, 1).await;
    println!("[restart] a new Message after the restart raised the unread count to 1");

    server.abort();
    app.cleanup().await;
}
