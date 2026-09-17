//! Typing Indicator (CONTEXT.md: 正在输入): the acceptance scenarios.
//!
//! Everything here runs against a **real PostgreSQL** and **real WebSocket
//! clients**, because the behaviour under test *is* the fan-out: who receives the
//! signal, who must not, how fast, and how long. The ephemeral half is asserted
//! against the schema too — a typing signal must have no table and no history row,
//! not merely a column that happens to be unused.
//!
//! # The arithmetic the throttle test pins down
//!
//! A Typing Indicator is the cheapest event in the contract and so the easiest
//! amplifier. In a 200-Participant Group, one careless client sending per keystroke
//! at 5 keys/s would produce `200 * 5 = 1000` deliveries per second for one typer,
//! on a 2 vCPU / 2 GB box. The server coalesces every signal for one
//! (Conversation, Participant) into at most one fan-out per throttle window, so the
//! same minute costs `200 * ceil(60 / 5) = 2400` deliveries instead of
//! `200 * 300 = 60000` — and it is flat in keystroke rate. The throttle test
//! asserts that bound with a single peer: `N` keystrokes inside one window cost
//! exactly `1` delivery, and a second window costs exactly one more.

use std::collections::HashSet;
use std::time::Duration;

use jiuyue_contract::ServerEvent;
use jiuyue_contract::events::TypingState;
use jiuyue_realtime::TypingLimits;

use crate::support::{
    TestApp, collect_events_within, connect_socket, count_rows, create_direct, create_group,
    expect_typing, login, next_envelope, register, send_event, send_message, typing, typing_tables,
};

/// No `ServerEvent::Typing` may be among the events a socket received.
fn assert_no_typing(events: &[ServerEvent], context: &str) {
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, ServerEvent::Typing(_))),
        "{context} must receive no Typing Indicator, got {events:?}"
    );
}

/// One Participant typing reaches the other Participants and **not** the sender's
/// own other Device.
///
/// Alice has a laptop and a phone (two Devices of one account). When the laptop
/// types, Bob — the other Participant — sees it, and the phone must not: seeing
/// your own typing echoed on your phone is a bug a user notices immediately, and
/// the audience is computed as "the other Participants", so the phone is not even
/// in the fan-out.
#[tokio::test]
async fn typing_reaches_the_other_participants_and_never_echoes_to_the_senders_own_device() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let alice_phone = login(&app, "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;
    let mut laptop = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut laptop).await;
    let mut phone = connect_socket(&ws_url, &alice_phone.tokens.access_token).await;
    next_envelope(&mut phone).await;

    send_event(&mut laptop, typing(&conversation.id, TypingState::Started)).await;

    let seen = expect_typing(&mut bob_socket).await;
    assert_eq!(seen.user_id, alice.user.id, "the event names who is typing");
    assert_eq!(seen.conversation_id, conversation.id);
    assert_eq!(seen.state, TypingState::Started);
    println!(
        "[bob] Typing {{ user: alice, conversation: {}, state: {:?} }}",
        seen.conversation_id, seen.state
    );

    let on_phone = collect_events_within(&mut phone, Duration::from_millis(600)).await;
    assert_no_typing(
        &on_phone,
        "the sender's own second Device (the echo channel)",
    );
    println!(
        "[alice phone] {} event(s) in the window, none of them the sender's own echo",
        on_phone.len()
    );

    server.abort();
    app.cleanup().await;
}

/// In a Group, the indicator says **who** — two Participants typing produce two
/// events, each naming its own User.
#[tokio::test]
async fn a_group_indicator_names_every_participant_who_is_typing() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let carol = register(&app, "carol", "carol@example.com", "secret123").await;
    let group = create_group(&app, &alice.tokens.access_token, "团队", &["bob", "carol"]).await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut carol_socket).await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;

    send_event(&mut alice_socket, typing(&group.id, TypingState::Started)).await;
    send_event(&mut bob_socket, typing(&group.id, TypingState::Started)).await;

    let first = expect_typing(&mut carol_socket).await;
    let second = expect_typing(&mut carol_socket).await;
    assert_eq!(first.conversation_id, group.id);
    assert_eq!(second.conversation_id, group.id);

    let typers: HashSet<String> = [first.user_id.clone(), second.user_id]
        .into_iter()
        .collect();
    let expected: HashSet<String> = [alice.user.id.clone(), bob.user.id.clone()]
        .into_iter()
        .collect();
    assert_eq!(
        typers, expected,
        "a Group indicator must name each typing Participant, not just 'someone'"
    );
    println!(
        "[carol] Typing {{ conversation: {}, users: {:?} }}",
        group.id, typers
    );

    server.abort();
    app.cleanup().await;
}

/// Fast input is throttled to a bounded rate on the **server**.
///
/// `SIGNALS` signals sent as fast as the socket accepts them cost one fan-out per
/// throttle window, not one per signal: the burst inside the first window costs
/// exactly one delivery, and the burst inside the second window exactly one more.
/// A per-keystroke fan-out would deliver `2 * SIGNALS`; the bound is `2 * peers`.
#[tokio::test]
async fn fast_typing_collapses_to_one_fan_out_per_throttle_window() {
    const SIGNALS: usize = 200;
    const THROTTLE: Duration = Duration::from_millis(2_000);

    let app = TestApp::start_with_typing(
        Duration::from_secs(30),
        TypingLimits::new(Duration::from_millis(4_000), THROTTLE),
    )
    .await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;

    for _ in 0..SIGNALS {
        send_event(
            &mut alice_socket,
            typing(&conversation.id, TypingState::Started),
        )
        .await;
    }

    let first_window = collect_events_within(&mut bob_socket, Duration::from_millis(500)).await;
    let first = first_window
        .iter()
        .filter(|event| matches!(event, ServerEvent::Typing(_)))
        .count();
    assert_eq!(
        first, 1,
        "{SIGNALS} keystrokes inside one window must cost one fan-out, got {first}"
    );

    // The next window is the only other broadcast the burst can earn.
    tokio::time::sleep(THROTTLE + Duration::from_millis(200)).await;
    for _ in 0..SIGNALS {
        send_event(
            &mut alice_socket,
            typing(&conversation.id, TypingState::Started),
        )
        .await;
    }

    let second_window = collect_events_within(&mut bob_socket, Duration::from_millis(500)).await;
    let second = second_window
        .iter()
        .filter(|event| matches!(event, ServerEvent::Typing(_)))
        .count();
    assert_eq!(
        second, 1,
        "a refresh past the throttle window is one more fan-out, got {second}"
    );

    println!(
        "[bob] {} signals (2 bursts of {SIGNALS}) -> {} fan-outs; naive per-signal would be {}",
        SIGNALS * 2,
        first + second,
        SIGNALS * 2
    );

    server.abort();
    app.cleanup().await;
}

/// The indicator expires on its own, so a stop that arrives after the bound cannot
/// cancel a newer start.
///
/// The TTL is shortened to milliseconds so the test watches the real rule instead
/// of waiting ten seconds. After the bound the server has forgotten the typer, so
/// the late stop is a no-op; a start after that is a fresh indicator.
#[tokio::test]
async fn the_indicator_expires_on_its_own_without_a_stop() {
    let ttl = Duration::from_millis(300);
    let app = TestApp::start_with_typing(
        Duration::from_secs(30),
        TypingLimits::new(ttl, Duration::from_millis(150)),
    )
    .await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;

    send_event(
        &mut alice_socket,
        typing(&conversation.id, TypingState::Started),
    )
    .await;
    assert_eq!(
        expect_typing(&mut bob_socket).await.state,
        TypingState::Started
    );

    // Wait past the bound: the receiver would have cleared the indicator on its
    // own, and the server has forgotten the typer.
    tokio::time::sleep(ttl + Duration::from_millis(200)).await;

    send_event(
        &mut alice_socket,
        typing(&conversation.id, TypingState::Stopped),
    )
    .await;
    let late = collect_events_within(&mut bob_socket, Duration::from_millis(400)).await;
    assert_no_typing(&late, "a late stop for an expired indicator");
    println!("[bob] no stop arrived: the indicator had already expired on its own after {ttl:?}");

    // A new start after expiry is a fresh indicator, not a throttled repeat.
    send_event(
        &mut alice_socket,
        typing(&conversation.id, TypingState::Started),
    )
    .await;
    let again = expect_typing(&mut bob_socket).await;
    assert_eq!(again.state, TypingState::Started);
    println!(
        "[bob] a start after expiry appears again (state {:?})",
        again.state
    );

    server.abort();
    app.cleanup().await;
}

/// Sending a Message clears the indicator and tells the other Participants.
///
/// The sender's client may have hit Enter without sending a stop, so the server
/// clears the sender's typing state as part of accepting the Message. The sender's
/// own other Device must not receive that stop, exactly as it did not receive the
/// start.
#[tokio::test]
async fn sending_a_message_clears_the_indicator() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let alice_phone = login(&app, "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;
    let mut laptop = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut laptop).await;
    let mut phone = connect_socket(&ws_url, &alice_phone.tokens.access_token).await;
    next_envelope(&mut phone).await;

    send_event(&mut laptop, typing(&conversation.id, TypingState::Started)).await;
    assert_eq!(
        expect_typing(&mut bob_socket).await.state,
        TypingState::Started
    );

    send_event(
        &mut laptop,
        send_message(&conversation.id, "cmid-typing-cleared", "你好"),
    )
    .await;

    // The Message arrives first, then the stop; `expect_typing` skips the Message.
    let cleared = expect_typing(&mut bob_socket).await;
    assert_eq!(cleared.user_id, alice.user.id);
    assert_eq!(cleared.conversation_id, conversation.id);
    assert_eq!(
        cleared.state,
        TypingState::Stopped,
        "sending must clear the indicator for the other Participants"
    );
    println!(
        "[bob] Typing {{ user: alice, state: {:?} }} after the Message",
        cleared.state
    );

    let on_phone = collect_events_within(&mut phone, Duration::from_millis(500)).await;
    assert_no_typing(&on_phone, "the sender's own second Device on clear-on-send");

    assert_eq!(count_rows(app.pool(), "messages").await, 1);

    server.abort();
    app.cleanup().await;
}

/// A non-Participant neither receives an indicator nor can produce one.
///
/// Carol shares nothing with Alice and Bob: Alice's typing never reaches her, and
/// when Carol tries to claim she is typing in a Conversation she is not in, the
/// server drops the signal rather than fanning it out.
#[tokio::test]
async fn a_non_participant_neither_receives_nor_produces_a_typing_indicator() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let carol = register(&app, "carol", "carol@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut carol_socket).await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;

    // Alice's indicator reaches Bob, never Carol.
    send_event(
        &mut alice_socket,
        typing(&conversation.id, TypingState::Started),
    )
    .await;
    assert_eq!(expect_typing(&mut bob_socket).await.user_id, alice.user.id);

    let for_carol = collect_events_within(&mut carol_socket, Duration::from_millis(500)).await;
    assert_no_typing(&for_carol, "a User with no shared Conversation");
    println!("[carol] no Typing event for a Conversation she is not in");

    // Carol claims she is typing in Alice and Bob's Conversation. The signal is
    // refused, so Bob is told nothing.
    send_event(
        &mut carol_socket,
        typing(&conversation.id, TypingState::Started),
    )
    .await;
    let from_carol = collect_events_within(&mut bob_socket, Duration::from_millis(500)).await;
    assert_no_typing(&from_carol, "a non-Participant's forged signal");
    println!("[bob] no Typing event from carol's forged signal (dropped, not fanned out)");

    server.abort();
    app.cleanup().await;
}

/// A typing signal never reaches storage or history, and never reappears after a
/// reconnect.
///
/// Asserted against the database, not the absence of a column: there is no table
/// whose name looks like typing storage, and the `messages` and
/// `conversation_members` counts are exactly what the Message and membership
/// actions produced — a typing signal added nothing. A brand-new connection for a
/// Participant that already saw the indicator receives nothing, because replay is
/// scoped to a connection and this is a different one (ADR-0013).
#[tokio::test]
async fn a_typing_signal_never_reaches_storage_or_history() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    let bob = register(&app, "bob", "bob@example.com", "secret123").await;
    let conversation = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;

    send_event(
        &mut alice_socket,
        typing(&conversation.id, TypingState::Started),
    )
    .await;
    assert_eq!(
        expect_typing(&mut bob_socket).await.state,
        TypingState::Started
    );
    send_event(
        &mut alice_socket,
        typing(&conversation.id, TypingState::Stopped),
    )
    .await;
    assert_eq!(
        expect_typing(&mut bob_socket).await.state,
        TypingState::Stopped
    );

    // Structural: nothing to write to, and nothing that was written.
    let tables = typing_tables(app.pool()).await;
    assert!(
        tables.is_empty(),
        "a Typing Indicator must have no storage at all; found {tables:?}"
    );
    assert_eq!(
        count_rows(app.pool(), "messages").await,
        0,
        "a typing signal must never become a history entry"
    );
    assert_eq!(
        count_rows(app.pool(), "conversation_members").await,
        2,
        "a typing signal must not touch membership"
    );
    println!(
        "[psql] typing tables: {:?}; messages: 0; conversation_members: 2",
        tables
    );

    // A reconnect is a new connection: the per-connection replay cannot carry an
    // old typing event, and it was never persisted, so it cannot appear.
    let mut reconnected = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut reconnected).await;
    let after_reconnect = collect_events_within(&mut reconnected, Duration::from_millis(400)).await;
    assert_no_typing(&after_reconnect, "a reconnected client");
    println!(
        "[bob reconnect] {} event(s) after reconnect, no stale indicator",
        after_reconnect.len()
    );

    server.abort();
    app.cleanup().await;
}
