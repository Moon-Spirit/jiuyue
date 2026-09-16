//! History pagination: the cursor walk on the Sequence Number.
//!
//! Every page travels over the **real Axum router** into a **real database**, and
//! the assertions are about the property that actually breaks in production: a
//! cursor anchored on `seq` neither duplicates nor skips a Message when the
//! Conversation keeps growing between two page fetches.
//!
//! Some tests seed rows directly. That is deliberate: pagination is the behaviour
//! under test, the atomic allocator has its own concurrency coverage in
//! `messaging.rs`, and seeding is the only way to build a large Conversation (the
//! clamp test needs more rows than the page ceiling) or a genuine `seq` gap.

use axum::http::StatusCode;
use jiuyue_contract::{MAX_MESSAGE_PAGE_SIZE, MessageList};

use crate::support::{
    TestApp, collect_acks, connect_socket, create_direct, error_code, get_with_token,
    next_envelope, register, send_event, send_message,
};

/// Seed `count` Messages with contiguous `seq` 1..=count, straight into the schema.
///
/// Bypasses the socket send path on purpose (see the module docs). The generated
/// ids are 26 Crockford characters (`lpad` of an uppercase hex number) so every
/// `messages` CHECK constraint still holds, and `next_seq` is advanced so the
/// allocator stays coherent for any later real send.
async fn seed_messages(app: &TestApp, conversation_id: &str, sender_id: &str, count: i64) {
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, seq, sender_id, client_msg_id, body) \
         SELECT lpad(upper(to_hex(g)), 26, '0'), $1, g, $2, 'seed-' || g, 'seeded ' || g \
         FROM generate_series(1, $3) AS g",
    )
    .bind(conversation_id)
    .bind(sender_id)
    .bind(count)
    .execute(app.pool())
    .await
    .expect("seeding messages must succeed");

    sqlx::query("UPDATE conversations SET next_seq = $2 WHERE id = $1")
        .bind(conversation_id)
        .bind(count)
        .execute(app.pool())
        .await
        .expect("the seeded conversation's next_seq must advance");
}

/// Fetch one history page over real HTTP and decode it.
async fn fetch_page(app: &TestApp, conversation_id: &str, query: &str, token: &str) -> MessageList {
    let path = if query.is_empty() {
        format!("/conversations/{conversation_id}/messages")
    } else {
        format!("/conversations/{conversation_id}/messages?{query}")
    };

    let (status, body) = get_with_token(app, &path, token).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a history page must load, got {status}: {body}"
    );

    serde_json::from_value(body).expect("the response must match MessageList")
}

/// The Sequence Numbers of a page, in the order the page reports them.
fn seqs(page: &MessageList) -> Vec<i64> {
    page.messages.iter().map(|message| message.seq).collect()
}

#[tokio::test]
async fn paging_backwards_yields_every_message_exactly_once_in_order() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;
    seed_messages(&app, &conversation.id, &alice.user.id, 25).await;

    let token = alice.tokens.access_token.clone();
    let mut pages: Vec<MessageList> = Vec::new();
    let mut cursor: Option<i64> = None;

    loop {
        let query = match cursor {
            Some(before) => format!("before={before}&limit=10"),
            None => "limit=10".to_owned(),
        };
        let page = fetch_page(&app, &conversation.id, &query, &token).await;

        // A page is always ascending, whatever cursor produced it.
        let mut ascending = seqs(&page);
        let before_sort = ascending.clone();
        ascending.sort_unstable();
        assert_eq!(ascending, before_sort, "a page must be ascending by seq");

        cursor = page.next_before;
        let more = page.has_more;
        pages.push(page);
        if !more {
            break;
        }
        assert!(
            cursor.is_some(),
            "has_more implies a next_before cursor was returned"
        );
    }

    assert_eq!(pages.len(), 3, "25 Messages at 10 per page is three pages");
    assert_eq!(seqs(&pages[0]), (16..=25).collect::<Vec<_>>());
    assert_eq!(seqs(&pages[1]), (6..=15).collect::<Vec<_>>());
    assert_eq!(seqs(&pages[2]), (1..=5).collect::<Vec<_>>());

    let mut union: Vec<i64> = pages.iter().flat_map(seqs).collect();
    union.sort_unstable();
    assert_eq!(
        union,
        (1..=25).collect::<Vec<_>>(),
        "the union of every page must be the whole history, each Message once"
    );

    app.cleanup().await;
}

#[tokio::test]
async fn inserting_between_two_page_fetches_neither_duplicates_nor_skips() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut socket).await;

    // Ten Messages stored through the real send path, seq 1..=10.
    for index in 0..10 {
        send_event(
            &mut socket,
            send_message(&conversation.id, &format!("cmid-{index}"), "original"),
        )
        .await;
    }
    let acks = collect_acks(&mut socket, 10).await;
    assert_eq!(
        acks.iter().map(|ack| ack.message.seq).collect::<Vec<_>>(),
        (1..=10).collect::<Vec<_>>()
    );

    let token = alice.tokens.access_token.clone();

    // Page 1: the four newest Messages.
    let first = fetch_page(&app, &conversation.id, "limit=4", &token).await;
    assert_eq!(seqs(&first), [7, 8, 9, 10]);
    assert_eq!(first.next_before, Some(7));
    assert!(first.has_more);

    // The Conversation grows *between* the two fetches. A `LIMIT/OFFSET` client
    // would now repeat 7..10 on its second page; a `seq` cursor must not.
    for index in 10..13 {
        send_event(
            &mut socket,
            send_message(&conversation.id, &format!("cmid-{index}"), "inserted later"),
        )
        .await;
    }
    collect_acks(&mut socket, 3).await;

    // Page 2, anchored on the first page's cursor, reaches strictly older rows.
    let second = fetch_page(&app, &conversation.id, "before=7&limit=4", &token).await;
    assert_eq!(seqs(&second), [3, 4, 5, 6]);
    assert_eq!(second.next_before, Some(3));
    assert!(second.has_more);

    let third = fetch_page(&app, &conversation.id, "before=3&limit=4", &token).await;
    assert_eq!(seqs(&third), [1, 2]);
    assert!(!third.has_more);
    assert_eq!(third.next_before, None);

    let mut union: Vec<i64> = [&first, &second, &third]
        .into_iter()
        .flat_map(seqs)
        .collect();
    union.sort_unstable();
    assert_eq!(
        union,
        (1..=10).collect::<Vec<_>>(),
        "the three pages must cover the pre-insert history exactly once — no duplicate, no gap"
    );
    assert!(
        !union.contains(&11) && !union.contains(&12) && !union.contains(&13),
        "Messages inserted after the walk started must not leak into older pages"
    );

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn an_absurd_page_size_is_clamped_to_the_ceiling() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;
    seed_messages(
        &app,
        &conversation.id,
        &alice.user.id,
        MAX_MESSAGE_PAGE_SIZE + 1,
    )
    .await;

    let token = alice.tokens.access_token.clone();

    // One row more than the ceiling exists, so a server that honoured the asked
    // limit would return 101 — the clamped one returns exactly the ceiling.
    let page = fetch_page(&app, &conversation.id, "limit=1000000", &token).await;
    assert_eq!(
        page.messages.len() as i64,
        MAX_MESSAGE_PAGE_SIZE,
        "an absurd limit must be clamped to the ceiling, not honoured"
    );
    assert_eq!(seqs(&page), (2..=101).collect::<Vec<_>>());
    assert_eq!(page.next_before, Some(2));
    assert!(page.has_more);

    let older = fetch_page(&app, &conversation.id, "before=2&limit=1000000", &token).await;
    assert_eq!(seqs(&older), [1]);
    assert!(!older.has_more);

    app.cleanup().await;
}

#[tokio::test]
async fn a_cursor_at_a_deleted_or_nonexistent_sequence_degrades_sanely() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;
    seed_messages(&app, &conversation.id, &alice.user.id, 5).await;

    // Punch a hole at seq 3 — a cursor naming a Message that no longer exists.
    sqlx::query("DELETE FROM messages WHERE conversation_id = $1 AND seq = 3")
        .bind(&conversation.id)
        .execute(app.pool())
        .await
        .expect("deleting one seeded Message must succeed");

    let token = alice.tokens.access_token.clone();

    // Cursor on the deleted slot: everything strictly older, no error.
    let on_deleted = fetch_page(&app, &conversation.id, "before=3&limit=10", &token).await;
    assert_eq!(seqs(&on_deleted), [1, 2]);
    assert!(!on_deleted.has_more);
    assert_eq!(on_deleted.next_before, None);

    // Cursor above the newest Message: the most recent page, no error.
    let beyond = fetch_page(&app, &conversation.id, "before=9999&limit=10", &token).await;
    assert_eq!(seqs(&beyond), [1, 2, 4, 5]);
    assert_eq!(beyond.next_before, None);

    // Cursor at the very first Message: nothing is older, an empty page.
    let empty = fetch_page(&app, &conversation.id, "before=1&limit=10", &token).await;
    assert!(empty.messages.is_empty());
    assert!(!empty.has_more);
    assert_eq!(empty.next_before, None);

    app.cleanup().await;
}

#[tokio::test]
async fn a_non_participant_cannot_page_history_with_a_cursor() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let carol = register(&app, "carol", "carol@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;
    seed_messages(&app, &conversation.id, &alice.user.id, 3).await;

    let (status, body) = get_with_token(
        &app,
        &format!(
            "/conversations/{}/messages?before=3&limit=10",
            conversation.id
        ),
        &carol.tokens.access_token,
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");
    assert_eq!(error_code(&body), "NOT_A_PARTICIPANT");

    app.cleanup().await;
}
