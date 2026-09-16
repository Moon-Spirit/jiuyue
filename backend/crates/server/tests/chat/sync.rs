//! Per-Device sync cursors: offline catch-up and multi-device convergence.
//!
//! The reconnect tests in `reconnect.rs` prove a **connection** can be repaired.
//! These prove the stronger, product-level claim of CONTEXT.md: a **Device** — a
//! `sessions` row, not a socket and not a User — resumes its own position across a
//! closed laptop, a fresh process and a second device.
//!
//! Everything runs against a real PostgreSQL and real WebSocket clients. The
//! cursor is asserted from the schema (`sync_cursors`), never from a mock, because
//! the persistence *is* the behaviour under test.

use std::time::Duration;

use jiuyue_chat::ChatService;
use jiuyue_contract::{MessageView, ResyncReason, SyncCursor};

use crate::support::{
    TestApp, TestSocket, assert_exact_sequence_set, connect_socket, count_rows, create_direct,
    device_cursors_for_user, expect_ack, expect_new_message, expect_resync, expect_sync_state,
    get_with_token, login, next_envelope, next_event_within, register, repair_after, resume,
    send_event, send_message, session_id, sync_cursor, wait_for_device_cursor,
};

/// Report a consumed position, then disconnect, proving the report was processed.
///
/// A frame and a dropped socket race: the server might see EOF before it drains the
/// text frame. Sending a `Resume` after the cursor, and reading its answer, makes
/// the ordering explicit — frames are handled in order on one connection — so the
/// teardown checkpoint is guaranteed to see the report.
async fn report_cursor_and_disconnect(
    mut socket: TestSocket,
    conversation_id: &str,
    last_seq: i64,
) {
    send_event(&mut socket, sync_cursor(conversation_id, last_seq)).await;
    send_event(&mut socket, resume(0, None)).await;
    let answer = expect_resync(&mut socket).await;
    assert_eq!(
        answer.reason,
        ResyncReason::Fresh,
        "the ordering handshake must answer `fresh`"
    );
    drop(socket);
}

/// Send one Message from Bob and read Alice's copy, so a burst cannot overflow the
/// sender's bounded control queue.
async fn send_and_receive(
    bob_socket: &mut TestSocket,
    alice_socket: &mut TestSocket,
    conversation_id: &str,
    client_msg_id: &str,
    body: &str,
) -> MessageView {
    send_event(
        bob_socket,
        send_message(conversation_id, client_msg_id, body),
    )
    .await;
    let ack = expect_ack(bob_socket).await;
    let delivered = expect_new_message(alice_socket).await;
    assert_eq!(delivered.message.id, ack.message.id);
    delivered.message
}

/// Two Devices of one account both receive a new Message, exactly once each.
///
/// This is the multi-device fan-out requirement: the registry pushes to every live
/// connection of a Participant, and the per-Device path must not turn one Message
/// into two deliveries on one Device.
#[tokio::test]
async fn two_devices_of_one_account_each_receive_a_new_message_exactly_once() {
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
        send_message(&conversation.id, "cmid-both", "hello both devices"),
    )
    .await;
    let ack = expect_ack(&mut bob_socket).await;

    let first = expect_new_message(&mut device_one).await;
    let second = expect_new_message(&mut device_two).await;
    assert_eq!(first.message.id, ack.message.id, "Device 1 got the Message");
    assert_eq!(
        second.message.id, ack.message.id,
        "Device 2 got the Message"
    );
    println!(
        "[bob] seq={} id={} pushed to both Alice devices",
        ack.message.seq, ack.message.id
    );

    // Exactly once each: a second copy would be a duplicate delivery on one Device.
    assert!(
        next_event_within(&mut device_one, Duration::from_millis(750))
            .await
            .is_none(),
        "Device 1 must not receive the Message twice"
    );
    assert!(
        next_event_within(&mut device_two, Duration::from_millis(750))
            .await
            .is_none(),
        "Device 2 must not receive the Message twice"
    );

    assert_eq!(
        count_rows(app.pool(), "messages").await,
        1,
        "one Message, one stored row"
    );
    println!(
        "[psql] SELECT COUNT(*) FROM messages -> {}",
        count_rows(app.pool(), "messages").await
    );

    server.abort();
    app.cleanup().await;
}

/// A Device that was away converges on exactly the server's set.
///
/// Alice consumes seq 1..=3, reports it, and disappears. Five Messages arrive while
/// she is gone. On return she is handed her persisted cursor and repairs forward
/// with the existing bounded walk; the merged result is seq 1..=8, once each.
#[tokio::test]
async fn a_device_returns_after_an_absence_and_converges_on_exactly_the_server_set() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;

    // Alice is online and consumes seq 1..=3.
    let mut held: Vec<MessageView> = Vec::new();
    for index in 0..3 {
        held.push(
            send_and_receive(
                &mut bob_socket,
                &mut alice_socket,
                &conversation.id,
                &format!("before-{index}"),
                "before",
            )
            .await,
        );
    }
    let cursor = held.last().map(|message| message.seq).expect("three held");
    assert_eq!(cursor, 3);

    // She reports the position and the socket goes away. The report is persisted by
    // the teardown checkpoint — the row is in PostgreSQL before she returns.
    report_cursor_and_disconnect(alice_socket, &conversation.id, cursor).await;
    wait_for_device_cursor(app.pool(), &alice.user.id, &conversation.id, cursor).await;
    println!(
        "[psql] sync_cursors for alice's device -> {:?}",
        device_cursors_for_user(app.pool(), &alice.user.id, &conversation.id).await
    );

    // Five Messages arrive while she is away.
    for index in 3..8 {
        send_event(
            &mut bob_socket,
            send_message(&conversation.id, &format!("during-{index}"), "during"),
        )
        .await;
        expect_ack(&mut bob_socket).await;
    }

    // She returns on a fresh socket, with no in-memory cursor at all.
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    let state = expect_sync_state(&mut alice_socket).await;
    assert_eq!(
        state.cursors,
        vec![SyncCursor {
            conversation_id: conversation.id.clone(),
            last_seq: cursor,
        }],
        "the server must hand the Device back its own persisted position"
    );
    println!("[alice] SyncState -> {:?}", state.cursors);

    // The existing forward walk is the repair primitive: exactly what was missed.
    let repaired = repair_after(&app, &conversation.id, cursor, &alice.tokens.access_token).await;
    assert_eq!(
        repaired
            .iter()
            .map(|message| message.seq)
            .collect::<Vec<_>>(),
        (4..=8).collect::<Vec<_>>(),
        "the repair must return exactly the Messages sent during the outage"
    );
    println!(
        "[alice] repair after={cursor} -> seqs {:?}",
        repaired
            .iter()
            .map(|message| message.seq)
            .collect::<Vec<_>>()
    );

    held.extend(repaired);
    assert_exact_sequence_set(held, 8);
    assert_eq!(
        count_rows(app.pool(), "messages").await,
        8,
        "the server holds exactly the eight sent Messages"
    );
    println!(
        "[psql] SELECT COUNT(*) FROM messages -> {}",
        count_rows(app.pool(), "messages").await
    );

    server.abort();
    app.cleanup().await;
}

/// Two Devices of one account, one catching up later, converge to identical sets.
///
/// Device A and Device B both hold seq 1..=2. A goes away; three more Messages
/// arrive; B advances live while A repairs from its own persisted cursor. A's set
/// and B's set are asserted to be the same set, in the same order.
#[tokio::test]
async fn two_devices_converge_when_one_catches_up_later() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let alice_second = login(&app, "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut device_a = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut device_b = connect_socket(&ws_url, &alice_second.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut device_a).await;
    next_envelope(&mut device_b).await;
    next_envelope(&mut bob_socket).await;

    // Bob sends seq 1..=2: both Devices of Alice receive both.
    let mut held_a: Vec<MessageView> = Vec::new();
    for index in 0..2 {
        let message = send_and_receive(
            &mut bob_socket,
            &mut device_a,
            &conversation.id,
            &format!("shared-{index}"),
            "shared",
        )
        .await;
        // Device B receives the same Message independently.
        let on_b = expect_new_message(&mut device_b).await;
        assert_eq!(on_b.message.id, message.id);
        held_a.push(message);
    }
    let mut held_b: Vec<MessageView> = held_a.clone();

    // Device A records its position and leaves. Device B records a later one only
    // after it has seen the extra Messages below.
    report_cursor_and_disconnect(device_a, &conversation.id, 2).await;
    wait_for_device_cursor(app.pool(), &alice.user.id, &conversation.id, 2).await;

    // Three more Messages arrive while A is away; B takes them live.
    for index in 2..5 {
        send_event(
            &mut bob_socket,
            send_message(&conversation.id, &format!("during-{index}"), "during"),
        )
        .await;
        expect_ack(&mut bob_socket).await;
        held_b.push(expect_new_message(&mut device_b).await.message);
    }

    // B reports 5 and leaves, so the two Devices' stored positions are genuinely
    // independent: 2 for A, 5 for B.
    report_cursor_and_disconnect(device_b, &conversation.id, 5).await;
    wait_for_device_cursor(app.pool(), &alice.user.id, &conversation.id, 5).await;

    let mut stored = device_cursors_for_user(app.pool(), &alice.user.id, &conversation.id).await;
    stored.sort_unstable();
    assert_eq!(
        stored,
        vec![2, 5],
        "each Device must advance its own cursor independently"
    );
    println!("[psql] SELECT last_seq FROM sync_cursors ... -> {stored:?} (independent per Device)");

    // A returns, is handed cursor 2 (not 5), and repairs only what it missed.
    let mut device_a = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut device_a).await;
    let state = expect_sync_state(&mut device_a).await;
    assert_eq!(state.cursors[0].last_seq, 2);
    let repaired = repair_after(&app, &conversation.id, 2, &alice.tokens.access_token).await;
    held_a.extend(repaired);

    assert_exact_sequence_set(held_a.clone(), 5);
    assert_exact_sequence_set(held_b.clone(), 5);

    let seqs_a: Vec<i64> = held_a.iter().map(|message| message.seq).collect();
    let seqs_b: Vec<i64> = held_b.iter().map(|message| message.seq).collect();
    assert_eq!(seqs_a, vec![1, 2, 3, 4, 5]);
    assert_eq!(
        seqs_a, seqs_b,
        "the two Devices must agree on the set and its order"
    );
    println!("[alice] device A seqs {seqs_a:?} == device B seqs {seqs_b:?}");

    server.abort();
    app.cleanup().await;
}

/// A Device cursor survives a process restart: it lives in PostgreSQL, not memory.
///
/// The proxy for a restart is a brand-new `ChatService` instance over the same
/// database: its in-memory state is empty, so anything it reads came from storage.
#[tokio::test]
async fn a_device_cursor_survives_a_process_restart() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;

    for index in 0..4 {
        send_and_receive(
            &mut bob_socket,
            &mut alice_socket,
            &conversation.id,
            &format!("persist-{index}"),
            "persist",
        )
        .await;
    }

    report_cursor_and_disconnect(alice_socket, &conversation.id, 4).await;
    wait_for_device_cursor(app.pool(), &alice.user.id, &conversation.id, 4).await;

    // A fresh service: no memory of the connection, the hub, or the report.
    let device = session_id(&app, &alice.tokens.access_token).await;
    let restarted = ChatService::new(app.pool().clone());
    let cursors = restarted
        .list_sync_cursors(&device)
        .await
        .expect("a fresh service must read the persisted cursor");
    assert_eq!(
        cursors,
        vec![SyncCursor {
            conversation_id: conversation.id.clone(),
            last_seq: 4,
        }],
        "the cursor must be readable by a service that never saw the report"
    );

    assert_eq!(count_rows(app.pool(), "sync_cursors").await, 1);
    println!(
        "[psql] SELECT COUNT(*) FROM sync_cursors -> {}",
        count_rows(app.pool(), "sync_cursors").await
    );

    server.abort();
    app.cleanup().await;
}

/// A long absence is repaired in bounded pages, never one unbounded response.
///
/// Alice consumed seq 1..=30 before a long outage; 230 Messages arrive. The sync
/// frame carries her **position only** (one cursor, no Messages), and the repair is
/// the ordinary forward walk: pages capped at 100, three of them.
#[tokio::test]
async fn a_long_absence_is_repaired_in_bounded_pages() {
    const INITIAL: i64 = 30;
    const DURING: i64 = 230;

    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;

    let mut held: Vec<MessageView> = Vec::new();
    for index in 0..INITIAL {
        held.push(
            send_and_receive(
                &mut bob_socket,
                &mut alice_socket,
                &conversation.id,
                &format!("catchup-initial-{index}"),
                "initial",
            )
            .await,
        );
    }
    report_cursor_and_disconnect(alice_socket, &conversation.id, INITIAL).await;
    wait_for_device_cursor(app.pool(), &alice.user.id, &conversation.id, INITIAL).await;

    for index in 0..DURING {
        send_event(
            &mut bob_socket,
            send_message(&conversation.id, &format!("catchup-{index}"), "while away"),
        )
        .await;
        expect_ack(&mut bob_socket).await;
    }

    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    let state = expect_sync_state(&mut alice_socket).await;

    // The sync frame is a position, not a backlog: one cursor, no Messages.
    assert_eq!(state.cursors.len(), 1, "positions, not the missed set");
    assert_eq!(state.cursors[0].last_seq, INITIAL);
    println!(
        "[alice] SyncState carries {} cursor(s) and no Messages",
        state.cursors.len()
    );

    let token = &alice.tokens.access_token;
    let mut cursor = state.cursors[0].last_seq;
    let mut pages = 0_usize;
    let mut collected: Vec<MessageView> = Vec::new();

    loop {
        let path = format!(
            "/conversations/{}/messages?after={cursor}&limit=100",
            conversation.id
        );
        let (status, body) = get_with_token(&app, &path, token).await;
        assert_eq!(status, axum::http::StatusCode::OK, "page must load: {body}");

        let page: jiuyue_contract::MessageList =
            serde_json::from_value(body).expect("the page must be MessageList");
        assert!(
            page.messages.len() <= 100,
            "a single page must stay within the ceiling"
        );

        pages += 1;
        let has_more = page.has_more;
        let next_after = page.next_after;
        collected.extend(page.messages);

        match (has_more, next_after) {
            (true, Some(next)) if next > cursor => cursor = next,
            _ => break,
        }
    }

    assert_eq!(
        pages, 3,
        "{DURING} Messages at 100 per page is three bounded repair pages"
    );
    held.extend(collected);
    assert_exact_sequence_set(held, INITIAL + DURING);
    println!("[alice] repaired {DURING} missed Messages in {pages} pages of <= 100");

    server.abort();
    app.cleanup().await;
}

/// Repeated catch-ups from the same position are idempotent.
///
/// Two identical forward walks re-deliver the same Messages; applied by Message ID
/// they collapse, and the final set is still exactly seq 1..=5.
#[tokio::test]
async fn repeated_catch_ups_are_idempotent() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;

    let mut held: Vec<MessageView> = Vec::new();
    for index in 0..2 {
        held.push(
            send_and_receive(
                &mut bob_socket,
                &mut alice_socket,
                &conversation.id,
                &format!("idem-{index}"),
                "before",
            )
            .await,
        );
    }
    report_cursor_and_disconnect(alice_socket, &conversation.id, 2).await;
    wait_for_device_cursor(app.pool(), &alice.user.id, &conversation.id, 2).await;

    for index in 2..5 {
        send_event(
            &mut bob_socket,
            send_message(&conversation.id, &format!("idem-during-{index}"), "during"),
        )
        .await;
        expect_ack(&mut bob_socket).await;
    }

    // The same catch-up twice: a client that retries its repair must not duplicate.
    let first = repair_after(&app, &conversation.id, 2, &alice.tokens.access_token).await;
    let second = repair_after(&app, &conversation.id, 2, &alice.tokens.access_token).await;
    assert_eq!(first.len(), 3);
    assert_eq!(second.len(), 3, "a repeat delivers the same three again");

    held.extend(first);
    held.extend(second);
    assert_exact_sequence_set(held, 5);
    println!("[alice] two identical catch-ups collapse to seq 1..=5 exactly once");

    server.abort();
    app.cleanup().await;
}

/// A malformed report is discarded without disturbing the connection or the store.
///
/// Reporting is advisory state for one Device, so bad input must be a no-op — not a
/// dropped socket and not a broken batch.
#[tokio::test]
async fn a_malformed_sync_cursor_does_not_disturb_the_connection() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;

    send_event(&mut alice_socket, sync_cursor("not-a-ulid", 999)).await;
    send_event(&mut alice_socket, sync_cursor(&conversation.id, 0)).await;
    send_event(&mut alice_socket, sync_cursor(&conversation.id, -4)).await;

    // The connection still works: a normal send is acknowledged.
    send_event(
        &mut alice_socket,
        send_message(&conversation.id, "cmid-after-bad-cursor", "still here"),
    )
    .await;
    let ack = expect_ack(&mut alice_socket).await;
    assert_eq!(ack.message.seq, 1);

    // Nothing malformed reached storage, and the socket survives a clean close.
    report_cursor_and_disconnect(alice_socket, &conversation.id, ack.message.seq).await;
    wait_for_device_cursor(app.pool(), &alice.user.id, &conversation.id, 1).await;
    assert_eq!(
        count_rows(app.pool(), "sync_cursors").await,
        1,
        "only the one well-formed report may be stored"
    );

    server.abort();
    app.cleanup().await;
}
