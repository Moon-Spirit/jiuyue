//! Reconnect and repair: the acceptance scenarios for ticket #12.
//!
//! A chat that silently drops Messages while a laptop sleeps is broken, and the
//! failure is invisible. Every test here is therefore framed around proving that
//! **nothing was skipped**: a client reconnects against a **real Axum server** and
//! a **real PostgreSQL**, repairs over the REST forward cursor (ADR-0003), and the
//! final Message set is compared to the sent set as an exact, duplicate-free
//! collection.
//!
//! The connection-level half — a hole in the envelope sequence `s` — is exercised
//! in `realtime.rs` (`a_connection_level_gap_is_filled_from_the_replay_buffer`);
//! here the emphasis is the Conversation-layer repair that a reconnect always
//! triggers, because a per-connection sequence does not carry across sockets.

use std::collections::{BTreeMap, BTreeSet};

use jiuyue_contract::{MessageAck, MessageList, MessageView};

use crate::support::{
    TestApp, TestSocket, connect_socket, create_direct, expect_ack, expect_new_message,
    get_with_token, next_envelope, register, repair_after, send_event, send_message,
};

/// Send one Message over a socket and wait for its acknowledgement.
///
/// Reading the ack also drains the sender's own `NewMessage` echo, so a long burst
/// of sends cannot overflow the bounded control queue.
async fn send_one(
    socket: &mut TestSocket,
    conversation_id: &str,
    client_msg_id: &str,
    body: &str,
) -> MessageAck {
    send_event(socket, send_message(conversation_id, client_msg_id, body)).await;
    expect_ack(socket).await
}

/// Assert a client's Message set, after idempotent application, is exactly `1..=count`.
///
/// The list may contain the same Message more than once — a repair re-delivers
/// Messages the client already holds — so this applies exactly what the client
/// must: key on Message ID and treat a repeat as a no-op. It returns the surviving
/// ids so a caller can also assert the raw delivery shape.
fn assert_exact_sequence_set(messages: Vec<MessageView>, count: i64) -> BTreeSet<String> {
    let mut by_id: BTreeMap<String, MessageView> = BTreeMap::new();
    for message in messages {
        by_id.insert(message.id.clone(), message);
    }

    let mut merged: Vec<MessageView> = by_id.into_values().collect();
    merged.sort_by_key(|message| message.seq);

    let ids: BTreeSet<String> = merged.iter().map(|message| message.id.clone()).collect();
    assert_eq!(
        merged.len(),
        count as usize,
        "the client must hold exactly the {count} sent Messages"
    );
    assert_eq!(
        ids.len(),
        count as usize,
        "no Message may appear twice after idempotent application"
    );
    assert_eq!(
        merged.iter().map(|message| message.seq).collect::<Vec<_>>(),
        (1..=count).collect::<Vec<_>>(),
        "the repaired set must cover seq 1..={count} with no hole"
    );

    ids
}

/// Disconnect: drop the client socket with no close handshake (a sleeping laptop).
fn sever(socket: TestSocket) {
    drop(socket);
}

#[tokio::test]
async fn a_reconnecting_client_recovers_every_message_exactly_once() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    let opening = next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;
    println!(
        "[alice] connected; opening envelope s={} ({:?})",
        opening.sequence(),
        opening.event()
    );

    // Three Messages reach Alice while she is online; she holds seq 1..=3.
    let mut held: Vec<MessageView> = Vec::new();
    for index in 0..3 {
        let ack = send_one(
            &mut bob_socket,
            &conversation.id,
            &format!("before-{index}"),
            "before",
        )
        .await;
        let delivered = expect_new_message(&mut alice_socket).await;
        assert_eq!(delivered.message.id, ack.message.id);
        println!(
            "[bob] before-{index} -> ack seq={} id={}",
            ack.message.seq, ack.message.id
        );
        println!(
            "[alice] received NewMessage seq={} id={}",
            delivered.message.seq, delivered.message.id
        );
        held.push(delivered.message);
    }
    let cursor = held
        .last()
        .map(|message| message.seq)
        .expect("three were held");
    assert_eq!(cursor, 3, "Alice consumed seq 1..=3");
    println!("[alice] cursor = {cursor}");

    // Alice goes offline for real: the socket is severed, no close frame.
    println!("--- alice socket severed (no close handshake) ---");
    sever(alice_socket);

    // Five more Messages arrive while she is gone.
    for index in 0..5 {
        let ack = send_one(
            &mut bob_socket,
            &conversation.id,
            &format!("during-{index}"),
            "during",
        )
        .await;
        println!(
            "[bob] during-{index} while alice offline -> ack seq={} id={}",
            ack.message.seq, ack.message.id
        );
    }

    // She reconnects (no page refresh) and repairs forward from her cursor.
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let reopened = next_envelope(&mut alice_socket).await;
    println!(
        "[alice] reconnected; opening envelope s={} ({:?})",
        reopened.sequence(),
        reopened.event()
    );
    let repaired = repair_after(&app, &conversation.id, cursor, &alice.tokens.access_token).await;
    println!(
        "[alice] repair after={cursor} -> seqs {:?}",
        repaired
            .iter()
            .map(|message| message.seq)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        repaired
            .iter()
            .map(|message| message.seq)
            .collect::<Vec<_>>(),
        (4..=8).collect::<Vec<_>>(),
        "the repair must return exactly the five Messages sent during the outage"
    );

    let repaired_ids: BTreeSet<String> =
        repaired.iter().map(|message| message.id.clone()).collect();
    assert_eq!(
        repaired_ids.len(),
        repaired.len(),
        "a single repair walk must not repeat a Message"
    );

    held.extend(repaired);
    assert_exact_sequence_set(held, 8);
    println!("[alice] FINAL set = seq 1..=8, every Message exactly once");

    server.abort();
    app.cleanup().await;
}

/// A hole in the **Conversation** sequence is repaired to an exact set.
///
/// Alice receives seq 1..=5 but the frame carrying seq 3 is dropped in flight, so
/// she holds {1,2,4,5}. Repairing from the last contiguous Sequence Number before
/// the hole (2) must restore {3,4,5} and leave her with {1..5} exactly once.
#[tokio::test]
async fn a_conversation_level_gap_is_repaired_to_an_exact_set() {
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
    for index in 0..5 {
        send_one(
            &mut bob_socket,
            &conversation.id,
            &format!("gap-{index}"),
            "gap",
        )
        .await;
        let delivered = expect_new_message(&mut alice_socket).await;
        if delivered.message.seq == 3 {
            // The frame is lost in transit: Alice never applies it.
            continue;
        }
        held.push(delivered.message);
    }
    assert_eq!(
        held.iter().map(|message| message.seq).collect::<Vec<_>>(),
        vec![1, 2, 4, 5],
        "Alice is missing seq 3"
    );

    // The last contiguous Sequence Number before the hole.
    let cursor = 2;
    let repaired = repair_after(&app, &conversation.id, cursor, &alice.tokens.access_token).await;
    assert_eq!(
        repaired
            .iter()
            .map(|message| message.seq)
            .collect::<Vec<_>>(),
        vec![3, 4, 5],
        "the repair must fill the hole and append what came after it"
    );

    held.extend(repaired);
    assert_eq!(
        held.len(),
        7,
        "the repair re-delivers seq 4 and 5, which Alice already holds"
    );
    // The client keys on Message ID, so the repeats are harmless and the result is
    // still exactly {1,2,3,4,5} — the at-least-once + idempotent contract.
    assert_exact_sequence_set(held, 5);

    server.abort();
    app.cleanup().await;
}

/// A long outage converges over several repair pages.
///
/// 230 Messages accumulate while Alice is offline. The page ceiling is 100, so the
/// repair walk is three pages: she must still end with seq 1..=230, once each.
#[tokio::test]
async fn a_long_outage_converges_over_several_repair_pages() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;

    // Alice goes offline before anything is sent.
    sever(alice_socket);

    const SENT: i64 = 230;
    for index in 0..SENT {
        send_one(
            &mut bob_socket,
            &conversation.id,
            &format!("outage-{index}"),
            "while offline",
        )
        .await;
    }

    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;

    // Walk the forward cursor by hand so the number of pages is observable.
    let token = &alice.tokens.access_token;
    let mut cursor = 0_i64;
    let mut pages = 0_usize;
    let mut collected: Vec<MessageView> = Vec::new();

    loop {
        let path = format!(
            "/conversations/{}/messages?after={cursor}&limit=100",
            conversation.id
        );
        let (status, body) = get_with_token(&app, &path, token).await;
        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "page {pages} must load: {body}"
        );

        let page: MessageList = serde_json::from_value(body).expect("the page must be MessageList");
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
        "230 Messages at 100 per page is three repair pages"
    );
    assert_exact_sequence_set(collected, SENT);

    server.abort();
    app.cleanup().await;
}

/// Repeated reconnects never duplicate anything.
///
/// Each reconnect repairs from the client's held cursor, so the second and third
/// repairs see a caught-up conversation and return nothing new. The final set is
/// still exactly the sent Messages.
#[tokio::test]
async fn repeated_reconnects_do_not_duplicate_anything() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;

    // Four Messages reach Alice while she is online.
    let mut held: Vec<MessageView> = Vec::new();
    for index in 0..4 {
        send_one(
            &mut bob_socket,
            &conversation.id,
            &format!("cycle-before-{index}"),
            "before",
        )
        .await;
        held.push(expect_new_message(&mut alice_socket).await.message);
    }
    assert_eq!(held.len(), 4);

    let mut cursor = held.last().map(|message| message.seq).expect("held");
    let token = alice.tokens.access_token.clone();

    for cycle in 0..3 {
        sever(alice_socket);

        for index in 0..2 {
            send_one(
                &mut bob_socket,
                &conversation.id,
                &format!("cycle-{cycle}-{index}"),
                "during",
            )
            .await;
        }

        alice_socket = connect_socket(&ws_url, &token).await;
        next_envelope(&mut alice_socket).await;

        let repaired = repair_after(&app, &conversation.id, cursor, &token).await;
        assert_eq!(
            repaired.len(),
            2,
            "cycle {cycle} must repair exactly the two new Messages"
        );
        held.extend(repaired);
        cursor = held.last().map(|message| message.seq).expect("held");
    }

    assert_exact_sequence_set(held, 10);

    server.abort();
    app.cleanup().await;
}
