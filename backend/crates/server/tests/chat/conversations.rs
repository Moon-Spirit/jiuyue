//! The Conversation lifecycle: opening a Direct Conversation is idempotent.
//!
//! The acceptance criterion — "starting a direct chat twice does not create a
//! second conversation" — is asserted here both sequentially and concurrently,
//! against the real UNIQUE constraint on the canonical pair key.

use axum::http::StatusCode;
use futures_util::future::join_all;
use jiuyue_contract::ConversationList;
use serde_json::json;

use crate::support::{
    TestApp, count_rows, create_direct, error_code, get_with_token, get_without_token,
    post_json_with_token, register,
};

#[tokio::test]
async fn opening_the_same_direct_conversation_twice_yields_one_conversation() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;

    let first = create_direct(&app, &alice.tokens.access_token, "bob").await;
    // A differently-cased handle resolves to the same User, and therefore to the
    // same Conversation.
    let second = create_direct(&app, &alice.tokens.access_token, "BOB").await;

    assert_eq!(
        first.id, second.id,
        "opening the direct conversation twice must not create a second one"
    );
    assert_eq!(
        first.peer.as_ref().map(|peer| peer.username.as_str()),
        Some("bob")
    );
    assert_eq!(
        count_rows(app.pool(), "conversations").await,
        1,
        "exactly one conversation row must exist"
    );
    assert_eq!(
        count_rows(app.pool(), "conversation_members").await,
        2,
        "both users must be participants of the one conversation"
    );

    // The other participant sees the same Conversation from their side.
    let (status, body) = get_with_token(&app, "/conversations", &bob.tokens.access_token).await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    let list: ConversationList = serde_json::from_value(body).expect("ConversationList");
    assert_eq!(list.conversations.len(), 1);
    assert_eq!(list.conversations[0].id, first.id);
    assert_eq!(
        list.conversations[0]
            .peer
            .as_ref()
            .map(|peer| peer.username.as_str()),
        Some("alice")
    );

    app.cleanup().await;
}

#[tokio::test]
async fn concurrent_opens_of_the_same_pair_yield_one_conversation() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;

    // Several opens race on the UNIQUE direct_key. The losers must read the
    // winner's row instead of inserting, so every response names one Conversation.
    let token = alice.tokens.access_token.as_str();
    let opened = join_all((0..6).map(|_| create_direct(&app, token, "bob"))).await;

    let first_id = opened[0].id.as_str();
    assert!(
        opened.iter().all(|summary| summary.id == first_id),
        "every concurrent open must return the same conversation, got: {:?}",
        opened
            .iter()
            .map(|summary| summary.id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(count_rows(app.pool(), "conversations").await, 1);
    assert_eq!(count_rows(app.pool(), "conversation_members").await, 2);

    app.cleanup().await;
}

#[tokio::test]
async fn opening_a_conversation_with_an_unknown_handle_is_rejected() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;

    let (status, body) = post_json_with_token(
        &app,
        "/conversations/direct",
        &json!({ "peer_username": "nobody" }),
        &alice.tokens.access_token,
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "got: {body}");
    assert_eq!(error_code(&body), "USER_NOT_FOUND");
    assert_eq!(count_rows(app.pool(), "conversations").await, 0);

    app.cleanup().await;
}

#[tokio::test]
async fn you_cannot_open_a_conversation_with_yourself() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;

    let (status, body) = post_json_with_token(
        &app,
        "/conversations/direct",
        &json!({ "peer_username": "alice" }),
        &alice.tokens.access_token,
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "got: {body}");
    assert_eq!(error_code(&body), "VALIDATION_FAILED");
    assert_eq!(count_rows(app.pool(), "conversations").await, 0);

    app.cleanup().await;
}

#[tokio::test]
async fn listing_conversations_requires_authentication() {
    let app = TestApp::start().await;

    let (status, body) = get_without_token(&app, "/conversations").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "got: {body}");
    assert_eq!(error_code(&body), "UNAUTHENTICATED");

    app.cleanup().await;
}
