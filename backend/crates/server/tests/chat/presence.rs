//! Presence and last-seen: the acceptance scenarios.
//!
//! Everything here runs against a **real PostgreSQL** and **real WebSocket
//! clients**. Presence is per **User** while a Sync Cursor is per Device
//! (CONTEXT.md), so the first scenario is the one that catches the mistake that
//! makes a phone and a laptop flicker each other offline. The last-seen instant is
//! asserted from the schema (`user_presence`), never from a hub's memory, because
//! the persistence *is* half of the behaviour under test.
//!
//! The dead-connection scenario is honest about what it can and cannot do: a
//! client that is killed sends a FIN and the server sees EOF at once, so the
//! interesting case is the **half-open** socket, which the test models by keeping
//! the socket open and simply never speaking again. That is a shape only the
//! server-side deadline can end, and the test shortens the heartbeat so it watches
//! the bound in milliseconds instead of ninety seconds.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use jiuyue_chat::ChatService;
use jiuyue_contract::PresenceStatus;
use jiuyue_realtime::{LastSeenStore, RealtimeHub};

use crate::support::{
    TestApp, connect_socket, create_direct, error_code, expect_presence, get_with_token,
    get_without_token, heartbeat, list_presence, login, next_envelope, next_event_within, register,
    send_event, stored_last_seen, wait_for_last_seen,
};

/// A User is online while *any* of their Devices is, and offline only when the
/// last one goes.
///
/// Alice's laptop and phone are two Devices (two `sessions` rows) of one account.
/// Bob — who shares a Direct Conversation with her — sees exactly one `online` when
/// her first Device arrives, nothing when the second does, nothing when the first
/// leaves, and one `offline` when the last one does. Getting the axis wrong (per
/// Device instead of per User) fails the middle assertions.
#[tokio::test]
async fn one_device_closing_keeps_the_account_online_and_the_last_one_goes_offline() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let alice_phone = login(&app, "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;

    // Alice's first Device: she becomes reachable, so Bob is told.
    let mut laptop = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut laptop).await;

    let online = expect_presence(&mut bob_socket).await;
    assert_eq!(online.user_id, alice.user.id);
    assert_eq!(online.status, PresenceStatus::Online);
    assert_eq!(
        online.last_seen_ms, None,
        "a reachable User has no past instant to show"
    );
    println!("[bob] Presence {{ alice -> {:?} }}", online.status);

    // Her second Device connects. The account was already online, so nothing
    // changed and nobody is told — a phone must not re-announce a laptop.
    let mut phone = connect_socket(&ws_url, &alice_phone.tokens.access_token).await;
    next_envelope(&mut phone).await;
    assert!(
        next_event_within(&mut bob_socket, Duration::from_millis(750))
            .await
            .is_none(),
        "a second Device must not re-announce an account that is already online"
    );
    println!("[bob] second Device of alice connected: no presence event (still online)");

    // One of two Devices leaving leaves the account reachable.
    drop(laptop);
    assert!(
        next_event_within(&mut bob_socket, Duration::from_millis(750))
            .await
            .is_none(),
        "closing one of two Devices must not report the account offline"
    );
    println!("[bob] alice's laptop closed: no presence event (her phone is still connected)");

    // The last Device leaving is the transition, and it carries the instant.
    drop(phone);
    let offline = expect_presence(&mut bob_socket).await;
    assert_eq!(offline.user_id, alice.user.id);
    assert_eq!(offline.status, PresenceStatus::Offline);
    assert!(
        offline.last_seen_ms.is_some(),
        "an offline change must say when the User was last reachable"
    );
    println!(
        "[bob] Presence {{ alice -> {:?}, last_seen_ms: {:?} }}",
        offline.status, offline.last_seen_ms
    );

    server.abort();
    app.cleanup().await;
}

/// A socket that stops speaking without closing is degraded to offline.
///
/// This is the half-open connection ADR-0013 describes seen from the server: the
/// socket is still open and the server is still writing to it, so nothing at the
/// byte level says the peer is gone. The client heartbeat is the only evidence
/// there is, and the server holds a connection to a deadline **only after the peer
/// has beaten once** — which is what keeps the sweep additive for older clients.
#[tokio::test]
async fn a_silent_connection_degrades_to_offline_within_the_deadline() {
    // 50 ms heartbeats put the deadline at 3 × 50 ms instead of 3 × 30 s, so the
    // test watches the real rule without waiting ninety seconds for it.
    let app = TestApp::start_with_heartbeat(Duration::from_millis(50)).await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;

    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;

    let online = expect_presence(&mut bob_socket).await;
    assert_eq!(online.status, PresenceStatus::Online);

    // One heartbeat — this is what puts Alice on the deadline at all — and then
    // silence. Her socket is deliberately never closed: the point is that the
    // server gives up on a peer that is still, as far as TCP is concerned, there.
    send_event(&mut alice_socket, heartbeat()).await;
    println!("[alice] one client heartbeat sent, then silence (socket still open)");

    let offline = expect_presence(&mut bob_socket).await;
    assert_eq!(offline.user_id, alice.user.id);
    assert_eq!(offline.status, PresenceStatus::Offline);
    assert!(offline.last_seen_ms.is_some());
    println!(
        "[bob] alice's silent socket swept to {:?} (deadline = 3 x 50 ms = 150 ms)",
        offline.status
    );

    // Nothing had to be closed for the server to reach this conclusion.
    drop(alice_socket);
    server.abort();
    app.cleanup().await;
}

/// The REST read answers "online now" and, when offline, when the User was last
/// reachable — and never a stale instant for someone who is reachable.
#[tokio::test]
async fn a_presence_read_reports_online_and_the_last_seen_instant_offline() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;

    let online = list_presence(&app, &alice.tokens.access_token, &[&bob.user.id]).await;
    assert_eq!(online.presences.len(), 1);
    assert_eq!(online.presences[0].user_id, bob.user.id);
    assert_eq!(online.presences[0].status, PresenceStatus::Online);
    assert_eq!(
        online.presences[0].last_seen_ms, None,
        "a reachable User is not shown a last-seen instant"
    );
    println!(
        "[alice] GET /presence -> bob {:?} (last_seen_ms: None)",
        online.presences[0].status
    );

    // Bob leaves. The teardown checkpoint is what makes the instant durable, so
    // wait for the row rather than assuming the read raced it.
    drop(bob_socket);
    let stamped = wait_for_last_seen(app.pool(), &bob.user.id).await;
    println!("[psql] user_presence.last_seen_at for bob -> {stamped} ms since epoch");

    let offline = list_presence(&app, &alice.tokens.access_token, &[&bob.user.id]).await;
    assert_eq!(offline.presences.len(), 1);
    assert_eq!(offline.presences[0].status, PresenceStatus::Offline);
    let last_seen = offline.presences[0]
        .last_seen_ms
        .expect("an offline User must carry the instant they were last reachable");
    assert_eq!(last_seen, stamped, "the read must show the stored instant");
    println!(
        "[alice] GET /presence -> bob {:?} last_seen_ms={last_seen}",
        offline.presences[0].status
    );

    server.abort();
    app.cleanup().await;
}

/// A restart starts from an empty registry — nobody is stuck online — and the
/// last-seen instant survives it, because it lives in PostgreSQL.
#[tokio::test]
async fn a_restart_leaves_nobody_stuck_online_and_last_seen_survives() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;

    assert!(
        app.realtime().is_online(&alice.user.id).await,
        "the running process must see Alice as reachable before the restart"
    );

    // The restart proxy: a brand-new hub over the same database. Its registry is
    // empty, which is exactly the state a restarted process wakes up in — so the
    // answer to "is Alice online" is no, without any cleanup having to run.
    let restarted_chat = Arc::new(ChatService::new(app.pool().clone()));
    let restarted = RealtimeHub::new(Arc::clone(&restarted_chat), app.pool().clone());
    assert!(
        !restarted.is_online(&alice.user.id).await,
        "a restart must not leave anyone stuck online"
    );

    let after_restart = restarted
        .presence_for(&bob.user.id, std::slice::from_ref(&alice.user.id))
        .await
        .expect("the restarted process must answer a presence read");
    assert_eq!(after_restart.len(), 1);
    assert_eq!(after_restart[0].status, PresenceStatus::Offline);
    println!(
        "[restart] fresh hub reports alice -> {:?} (registry empty), last_seen_ms={:?}",
        after_restart[0].status, after_restart[0].last_seen_ms
    );

    // The durable half: Alice's instant is in the table, readable by a service
    // that never saw her connection.
    drop(alice_socket);
    let stamped = wait_for_last_seen(app.pool(), &alice.user.id).await;
    let store = LastSeenStore::new(app.pool().clone());
    let stored = store
        .last_seen(std::slice::from_ref(&alice.user.id))
        .await
        .expect("a fresh store must read the persisted instant");
    assert_eq!(
        stored.get(&alice.user.id).copied(),
        Some(stamped),
        "the last-seen instant must survive the restart"
    );
    assert_eq!(
        stored_last_seen(app.pool(), &alice.user.id).await,
        Some(stamped)
    );
    println!(
        "[psql] SELECT last_seen_at FROM user_presence -> {stamped} ms (survives the restart)"
    );

    server.abort();
    app.cleanup().await;
}

/// Presence reaches the Participants of a shared Conversation, and nobody else.
///
/// Carol shares nothing with Alice, so she is told nothing live **and** is not
/// allowed to look Alice up: the same rule answers both halves, which is what
/// makes the eventual privacy settings a filter on one seam rather than a second
/// audience. A broadcast-to-everyone implementation would pass the first half of
/// this test and fail the second.
#[tokio::test]
async fn presence_reaches_only_the_participants_of_a_shared_conversation() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let carol = register(&app, "carol", "carol@example.com", "secret123").await;
    // Alice and Bob share a Direct Conversation; Carol shares nothing with Alice.
    create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut carol_socket).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;

    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;

    let online = expect_presence(&mut bob_socket).await;
    assert_eq!(online.user_id, alice.user.id);
    assert_eq!(online.status, PresenceStatus::Online);
    println!(
        "[bob] Presence {{ alice -> {:?} }} (shares a Conversation)",
        online.status
    );

    assert!(
        next_event_within(&mut carol_socket, Duration::from_millis(750))
            .await
            .is_none(),
        "a User with no shared Conversation must not receive a presence change"
    );
    println!("[carol] no presence event for alice (shares no Conversation)");

    // The read is filtered by the same rule, so the endpoint is not a directory.
    let for_carol = list_presence(&app, &carol.tokens.access_token, &[&alice.user.id]).await;
    assert!(
        for_carol.presences.is_empty(),
        "a stranger must not be able to look a User's presence up"
    );
    println!(
        "[carol] GET /presence -> {} (omitted, not visible)",
        for_carol.presences.len()
    );

    let for_bob = list_presence(&app, &bob.tokens.access_token, &[&alice.user.id]).await;
    assert_eq!(for_bob.presences.len(), 1);
    assert_eq!(for_bob.presences[0].status, PresenceStatus::Online);
    println!(
        "[bob] GET /presence -> alice {:?}",
        for_bob.presences[0].status
    );

    server.abort();
    app.cleanup().await;
}

/// The route is authenticated and refuses a query about nobody.
#[tokio::test]
async fn the_presence_route_requires_a_session_and_a_non_empty_list() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;

    let (status, body) =
        get_without_token(&app, "/presence?user_ids=01JABC1234567890ABCDEFGHJ3").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&body), "UNAUTHENTICATED");

    let (status, body) = get_with_token(&app, "/presence", &alice.tokens.access_token).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&body), "VALIDATION_FAILED");

    app.cleanup().await;
}
