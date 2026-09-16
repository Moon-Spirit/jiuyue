//! Group Conversations: creation, membership changes, Roles and dissolution.
//!
//! Every test runs against the **real router**, **real PostgreSQL** and, where
//! delivery is the point, **real WebSocket clients**. Nothing is mocked: the
//! permission rules, the fan-out recipient sets and the `seq` allocator are the
//! behaviour under test and none of them exist in a mock.
//!
//! # Who may see what (the contract these tests pin)
//!
//! | Change                  | Recipients                                        |
//! | ----------------------- | ------------------------------------------------- |
//! | Group created           | every invited member (their own `ConversationCreated`) |
//! | Member joined           | every current Participant                         |
//! | Member left             | remaining Participants **and** the leaver          |
//! | Member removed          | remaining Participants **and** the removed User    |
//! | Role changed            | every current Participant                         |
//! | Ownership transferred   | every current Participant                         |
//! | Group dissolved         | every former Participant                          |
//! | Announcement edited     | every current Participant (their own summary)     |
//!
//! A leaver or removed member appears in the recipients **only** for their own
//! exit event; they are never a recipient of the Group's Messages afterwards, nor
//! of a later announcement edit. That is what these tests assert by draining a
//! socket and demanding silence.

use std::time::Duration;

use axum::http::{Method, StatusCode};
use jiuyue_contract::group::MAX_ANNOUNCEMENT_CHARS;
use jiuyue_contract::{
    AddGroupMembersRequest, ConversationCreated, ConversationKind, MembershipChange, MessageList,
    Role, ServerEvent,
};

use crate::support::{
    TestApp, TestSocket, add_group_members, change_member_role, collect_events_within,
    connect_socket, count_rows, create_direct, create_group, dissolve_group, error_code,
    expect_ack, expect_membership_changed, expect_new_message, expect_rejection, get_with_token,
    leave_group, list_conversations, next_envelope, next_event, next_event_within,
    post_json_with_token, register, remove_group_member, request_json_with_token, send_event,
    send_message, transfer_ownership,
};

/// How long an absence assertion waits before believing nothing will arrive.
const ABSENCE_WINDOW: Duration = Duration::from_millis(300);

/// Register three Users: Alice (the group owner-to-be), Bob and Carol.
async fn three_users(
    app: &TestApp,
) -> (
    jiuyue_contract::AuthSession,
    jiuyue_contract::AuthSession,
    jiuyue_contract::AuthSession,
) {
    let alice = register(app, "alice", "alice@example.com", "secret123").await;
    let bob = register(app, "bob", "bob@example.com", "secret123").await;
    let carol = register(app, "carol", "carol@example.com", "secret123").await;

    (alice, bob, carol)
}

/// A group whose list of Participants contains `conversation_id`.
fn holds(list: &jiuyue_contract::ConversationList, conversation_id: &str) -> bool {
    list.conversations
        .iter()
        .any(|conversation| conversation.id == conversation_id)
}

/// The stored Participant count of a Conversation, read straight from PostgreSQL.
async fn stored_member_count(app: &TestApp, conversation_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM conversation_members WHERE conversation_id = $1",
    )
    .bind(conversation_id)
    .fetch_one(app.pool())
    .await
    .expect("counting members must succeed")
}

#[tokio::test]
async fn a_group_created_with_three_members_appears_in_all_three_conversation_lists() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;

    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    assert_eq!(group.kind, ConversationKind::Group);
    assert!(group.peer.is_none(), "a group names no single peer");
    let summary = group.group.as_ref().expect("a group summary must exist");
    assert_eq!(summary.title, "九月小组");
    assert_eq!(summary.member_count, 3);
    assert_eq!(summary.my_role, Role::Owner);

    for (session, expected_role) in [
        (&alice, Role::Owner),
        (&bob, Role::Member),
        (&carol, Role::Member),
    ] {
        let list = list_conversations(&app, &session.tokens.access_token).await;
        let entry = list
            .conversations
            .iter()
            .find(|conversation| conversation.id == group.id)
            .unwrap_or_else(|| panic!("the group must be in every member's list"));
        assert_eq!(
            entry.group.as_ref().map(|group| group.my_role),
            Some(expected_role),
            "each member sees their own Role"
        );
    }

    assert_eq!(count_rows(app.pool(), "conversations").await, 1);
    assert_eq!(stored_member_count(&app, &group.id).await, 3);

    app.cleanup().await;
}

#[tokio::test]
async fn a_group_needs_at_least_three_participants() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;

    let (status, body) = post_json_with_token(
        &app,
        "/conversations/group",
        &serde_json::json!({ "title": "两人不够", "member_usernames": ["bob"] }),
        &alice.tokens.access_token,
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "got: {body}");
    assert_eq!(error_code(&body), "VALIDATION_FAILED");
    assert_eq!(count_rows(app.pool(), "conversations").await, 0);

    app.cleanup().await;
}

#[tokio::test]
async fn a_member_who_leaves_stops_receiving_group_messages_and_cannot_post() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;
    next_envelope(&mut carol_socket).await;

    // Carol leaves of her own accord.
    let (status, body) = leave_group(&app, &carol.tokens.access_token, &group.id).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "leaving must succeed: {body}"
    );

    // The leaver hears about her own exit; so do the Participants who remain.
    let on_carol = expect_membership_changed(&mut carol_socket).await;
    assert_eq!(
        on_carol.change,
        MembershipChange::Left {
            user_id: carol.user.id.clone()
        }
    );
    let on_bob = expect_membership_changed(&mut bob_socket).await;
    assert_eq!(
        on_bob.change,
        MembershipChange::Left {
            user_id: carol.user.id.clone()
        }
    );

    assert_eq!(stored_member_count(&app, &group.id).await, 2);

    // Bob posts; only Bob and Alice are in the fan-out now.
    send_event(
        &mut bob_socket,
        send_message(&group.id, "cmid-after-leave", "carol is gone"),
    )
    .await;
    let ack = expect_ack(&mut bob_socket).await;
    let on_alice = expect_new_message(&mut alice_socket).await;
    assert_eq!(on_alice.message.id, ack.message.id);

    // Carol's socket is silent: she is not in the fan-out any more.
    assert!(
        next_event_within(&mut carol_socket, ABSENCE_WINDOW)
            .await
            .is_none(),
        "a member who left must not keep receiving group traffic"
    );

    // And she cannot post: the send is refused as a non-participant.
    send_event(
        &mut carol_socket,
        send_message(&group.id, "cmid-after-leave-post", "let me back in"),
    )
    .await;
    let rejection = expect_rejection(&mut carol_socket).await;
    assert_eq!(rejection.code, jiuyue_contract::ErrorCode::NotAParticipant);

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn a_removed_member_stops_receiving_group_messages_and_cannot_post() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;
    next_envelope(&mut carol_socket).await;

    remove_group_member(&app, &alice.tokens.access_token, &group.id, &carol.user.id).await;

    let on_carol = expect_membership_changed(&mut carol_socket).await;
    assert_eq!(
        on_carol.change,
        MembershipChange::Removed {
            user_id: carol.user.id.clone()
        }
    );
    assert_eq!(stored_member_count(&app, &group.id).await, 2);

    send_event(
        &mut bob_socket,
        send_message(&group.id, "cmid-after-remove", "carol was removed"),
    )
    .await;
    expect_ack(&mut bob_socket).await;

    assert!(
        next_event_within(&mut carol_socket, ABSENCE_WINDOW)
            .await
            .is_none(),
        "a removed member must not keep receiving group traffic"
    );

    send_event(
        &mut carol_socket,
        send_message(&group.id, "cmid-removed-post", "still here?"),
    )
    .await;
    let rejection = expect_rejection(&mut carol_socket).await;
    assert_eq!(rejection.code, jiuyue_contract::ErrorCode::NotAParticipant);

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn a_non_admin_cannot_remove_another_member() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let path = format!("/conversations/{}/members/{}", group.id, carol.user.id);
    let (status, body) = crate::support::request_json_with_token(
        &app,
        axum::http::Method::DELETE,
        &path,
        None,
        &bob.tokens.access_token,
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");
    assert_eq!(error_code(&body), "FORBIDDEN");
    assert_eq!(stored_member_count(&app, &group.id).await, 3);

    app.cleanup().await;
}

#[tokio::test]
async fn an_admin_cannot_remove_the_owner_but_can_remove_a_member() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    change_member_role(
        &app,
        &alice.tokens.access_token,
        &group.id,
        &bob.user.id,
        Role::Admin,
    )
    .await;

    let path = format!("/conversations/{}/members/{}", group.id, alice.user.id);
    let (status, body) = crate::support::request_json_with_token(
        &app,
        axum::http::Method::DELETE,
        &path,
        None,
        &bob.tokens.access_token,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");
    assert_eq!(error_code(&body), "FORBIDDEN");

    // The same admin may remove an ordinary member.
    remove_group_member(&app, &bob.tokens.access_token, &group.id, &carol.user.id).await;
    assert_eq!(stored_member_count(&app, &group.id).await, 2);

    app.cleanup().await;
}

#[tokio::test]
async fn only_the_owner_can_transfer_ownership() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (status, body) = post_json_with_token(
        &app,
        &format!("/conversations/{}/transfer", group.id),
        &serde_json::json!({ "user_id": carol.user.id }),
        &bob.tokens.access_token,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");
    assert_eq!(error_code(&body), "FORBIDDEN");

    // The owner may.
    let info = transfer_ownership(&app, &alice.tokens.access_token, &group.id, &bob.user.id).await;
    let roles: Vec<(String, Role)> = info
        .members
        .iter()
        .map(|member| (member.user_id.clone(), member.role))
        .collect();
    assert!(roles.contains(&(bob.user.id.clone(), Role::Owner)));
    assert!(roles.contains(&(alice.user.id.clone(), Role::Admin)));

    app.cleanup().await;
}

#[tokio::test]
async fn only_the_owner_can_dissolve_the_group() {
    let app = TestApp::start().await;
    let (alice, bob, _carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (status, body) = dissolve_group(&app, &bob.tokens.access_token, &group.id).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");
    assert_eq!(error_code(&body), "FORBIDDEN");
    assert_eq!(count_rows(app.pool(), "conversations").await, 1);

    let (status, _) = dissolve_group(&app, &alice.tokens.access_token, &group.id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(count_rows(app.pool(), "conversations").await, 0);
    assert_eq!(count_rows(app.pool(), "conversation_members").await, 0);

    app.cleanup().await;
}

#[tokio::test]
async fn ownership_transfer_takes_effect_immediately_for_permission_checks() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    transfer_ownership(&app, &alice.tokens.access_token, &group.id, &bob.user.id).await;

    // Alice is an admin now: she may invite, but may no longer dissolve or
    // transfer, and she may no longer remove an admin.
    let (status, body) = dissolve_group(&app, &alice.tokens.access_token, &group.id).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");

    let (status, body) = post_json_with_token(
        &app,
        &format!("/conversations/{}/transfer", group.id),
        &serde_json::json!({ "user_id": carol.user.id }),
        &alice.tokens.access_token,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");

    // The new owner may remove the former owner (an admin).
    remove_group_member(&app, &bob.tokens.access_token, &group.id, &alice.user.id).await;
    assert_eq!(stored_member_count(&app, &group.id).await, 2);

    app.cleanup().await;
}

#[tokio::test]
async fn the_owner_cannot_leave_while_others_remain() {
    let app = TestApp::start().await;
    let (alice, bob, _carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (status, body) = leave_group(&app, &alice.tokens.access_token, &group.id).await;
    assert_eq!(status, StatusCode::CONFLICT, "got: {body}");
    assert_eq!(error_code(&body), "CONFLICT");
    assert_eq!(stored_member_count(&app, &group.id).await, 3);

    // Transfer first, then the former owner may leave as an ordinary admin.
    transfer_ownership(&app, &alice.tokens.access_token, &group.id, &bob.user.id).await;
    let (status, body) = leave_group(&app, &alice.tokens.access_token, &group.id).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "got: {body}");
    assert_eq!(stored_member_count(&app, &group.id).await, 2);

    app.cleanup().await;
}

#[tokio::test]
async fn transferring_to_a_member_who_already_left_is_refused() {
    let app = TestApp::start().await;
    let (alice, bob, _carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (status, _) = leave_group(&app, &bob.tokens.access_token, &group.id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = post_json_with_token(
        &app,
        &format!("/conversations/{}/transfer", group.id),
        &serde_json::json!({ "user_id": bob.user.id }),
        &alice.tokens.access_token,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "got: {body}");
    assert_eq!(error_code(&body), "CONFLICT");

    // Ownership did not move: Alice is still the owner.
    let info = crate::support::group_info(&app, &alice.tokens.access_token, &group.id).await;
    let alice_role = info
        .members
        .iter()
        .find(|member| member.user_id == alice.user.id)
        .map(|member| member.role);
    assert_eq!(alice_role, Some(Role::Owner));

    app.cleanup().await;
}

#[tokio::test]
async fn dissolving_a_group_other_people_are_in_removes_it_for_everyone() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;
    next_envelope(&mut carol_socket).await;

    let (status, _) = dissolve_group(&app, &alice.tokens.access_token, &group.id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    for socket in [&mut bob_socket, &mut carol_socket] {
        let change = expect_membership_changed(socket).await;
        assert_eq!(change.change, MembershipChange::Dissolved);
        assert_eq!(change.conversation_id, group.id);
    }

    assert_eq!(count_rows(app.pool(), "conversations").await, 0);
    assert_eq!(count_rows(app.pool(), "conversation_members").await, 0);
    assert_eq!(count_rows(app.pool(), "messages").await, 0);

    for session in [&alice, &bob, &carol] {
        let list = list_conversations(&app, &session.tokens.access_token).await;
        assert!(
            !holds(&list, &group.id),
            "a dissolved group must leave every conversation list"
        );
    }

    // A Group action on the now-missing Conversation is a plain not-found.
    let path = format!("/conversations/{}", group.id);
    let (status, _) = get_with_token(&app, &path, &alice.tokens.access_token).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn inviting_a_member_reaches_the_existing_members_and_the_new_one() {
    let app = TestApp::start().await;
    let (alice, bob, _carol) = three_users(&app).await;
    let dave = register(&app, "dave", "dave@example.com", "secret123").await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    let mut dave_socket = connect_socket(&ws_url, &dave.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;
    next_envelope(&mut dave_socket).await;

    add_group_members(&app, &alice.tokens.access_token, &group.id, &["dave"]).await;

    // The member already in the group sees the join.
    let on_bob = expect_membership_changed(&mut bob_socket).await;
    match on_bob.change {
        MembershipChange::Joined { member } => {
            assert_eq!(member.user_id, dave.user.id);
            assert_eq!(member.role, Role::Member);
        }
        other => panic!("expected Joined, got {other:?}"),
    }

    // The new member gains the Conversation (their list updates without a refresh).
    loop {
        if let ServerEvent::ConversationCreated(created) =
            crate::support::next_event(&mut dave_socket).await
        {
            assert_eq!(created.conversation.id, group.id);
            assert_eq!(
                created
                    .conversation
                    .group
                    .as_ref()
                    .map(|group| group.member_count),
                Some(4)
            );
            break;
        }
    }

    assert_eq!(stored_member_count(&app, &group.id).await, 4);
    assert_eq!(
        list_conversations(&app, &dave.tokens.access_token)
            .await
            .conversations
            .len(),
        1
    );

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn a_role_change_reaches_every_participant() {
    let app = TestApp::start().await;
    let (alice, bob, _carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;

    change_member_role(
        &app,
        &alice.tokens.access_token,
        &group.id,
        &bob.user.id,
        Role::Admin,
    )
    .await;

    let change = expect_membership_changed(&mut bob_socket).await;
    assert_eq!(
        change.change,
        MembershipChange::RoleChanged {
            user_id: bob.user.id.clone(),
            role: Role::Admin
        }
    );
    assert_eq!(change.actor_id, alice.user.id);

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn a_non_member_cannot_read_group_info_or_invite() {
    let app = TestApp::start().await;
    let (alice, _bob, _carol) = three_users(&app).await;
    let dave = register(&app, "dave", "dave@example.com", "secret123").await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let path = format!("/conversations/{}", group.id);
    let (status, body) = get_with_token(&app, &path, &dave.tokens.access_token).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");
    assert_eq!(error_code(&body), "NOT_A_PARTICIPANT");

    let (status, body) = post_json_with_token(
        &app,
        &format!("/conversations/{}/members", group.id),
        &serde_json::json!({ "member_usernames": ["dave"] }),
        &dave.tokens.access_token,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");

    app.cleanup().await;
}

#[tokio::test]
async fn inviting_an_existing_member_is_a_conflict() {
    let app = TestApp::start().await;
    let (alice, _bob, _carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (status, body) = post_json_with_token(
        &app,
        &format!("/conversations/{}/members", group.id),
        &serde_json::json!({ "member_usernames": ["bob"] }),
        &alice.tokens.access_token,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "got: {body}");
    assert_eq!(error_code(&body), "CONFLICT");
    assert_eq!(stored_member_count(&app, &group.id).await, 3);

    app.cleanup().await;
}

#[tokio::test]
async fn group_messages_carry_the_same_per_conversation_seq_guarantee_as_direct() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;
    next_envelope(&mut carol_socket).await;

    // Three Participants, three Messages: `seq` is allocated per Conversation,
    // exactly as it is for a Direct Conversation, and every Participant sees the
    // same sequence. One send races another deliberately (the two sends are
    // issued before either ack is read).
    send_event(&mut bob_socket, send_message(&group.id, "cmid-1", "one")).await;
    send_event(
        &mut alice_socket,
        send_message(&group.id, "cmid-3", "three"),
    )
    .await;
    send_event(&mut carol_socket, send_message(&group.id, "cmid-2", "two")).await;

    let bob_ack = expect_ack(&mut bob_socket).await;
    let alice_ack = expect_ack(&mut alice_socket).await;
    let carol_ack = expect_ack(&mut carol_socket).await;

    let mut seqs: Vec<i64> = vec![
        bob_ack.message.seq,
        alice_ack.message.seq,
        carol_ack.message.seq,
    ];
    seqs.sort_unstable();
    assert_eq!(
        seqs,
        vec![1, 2, 3],
        "three Messages must take seq 1, 2 and 3 with no hole and no repeat"
    );

    // A history page reads them back in order, which is the durable authority.
    let (status, body) = get_with_token(
        &app,
        &format!("/conversations/{}/messages", group.id),
        &alice.tokens.access_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    let page: jiuyue_contract::MessageList =
        serde_json::from_value(body).expect("the page must be a MessageList");

    // The same invariant the Direct tests use: idempotently applied, the set is
    // exactly seq 1..=3, once each. Nothing about a Group changes the promise.
    crate::support::assert_exact_sequence_set(page.messages.clone(), 3);

    server.abort();
    app.cleanup().await;
}

#[tokio::test]
async fn a_direct_conversation_is_not_a_group() {
    let app = TestApp::start().await;
    let alice = register(&app, "alice", "alice@example.com", "secret123").await;
    register(&app, "bob", "bob@example.com", "secret123").await;
    let direct = create_direct(&app, &alice.tokens.access_token, "bob").await;

    let path = format!("/conversations/{}", direct.id);
    let (status, body) = get_with_token(&app, &path, &alice.tokens.access_token).await;
    assert_eq!(status, StatusCode::CONFLICT, "got: {body}");
    assert_eq!(error_code(&body), "CONFLICT");

    app.cleanup().await;
}

/// The acceptance run, end to end, with the observed event sets printed.
///
/// Run with `--nocapture` to see the transcript: three real Users on three real
/// sockets, a group created, one member leaving, and the silence that follows for
/// her socket while the others keep receiving.
#[tokio::test]
async fn acceptance_three_users_on_real_sockets() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;
    next_envelope(&mut carol_socket).await;

    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;
    println!(
        "[setup] alice created group {} with bob and carol",
        group.id
    );

    for (who, socket) in [
        ("alice", &mut alice_socket),
        ("bob", &mut bob_socket),
        ("carol", &mut carol_socket),
    ] {
        loop {
            if let ServerEvent::ConversationCreated(created) =
                crate::support::next_event(socket).await
            {
                println!(
                    "[create] {who} received ConversationCreated(id={}, members={:?}, my_role={:?})",
                    created.conversation.id,
                    created
                        .conversation
                        .group
                        .as_ref()
                        .map(|group| group.member_count),
                    created
                        .conversation
                        .group
                        .as_ref()
                        .map(|group| group.my_role),
                );
                break;
            }
        }
    }

    send_event(
        &mut alice_socket,
        send_message(&group.id, "cmid-1", "hello group"),
    )
    .await;
    let ack = expect_ack(&mut alice_socket).await;
    let on_bob = expect_new_message(&mut bob_socket).await;
    let on_carol = expect_new_message(&mut carol_socket).await;
    println!(
        "[message] alice seq={} -> bob seq={} carol seq={}",
        ack.message.seq, on_bob.message.seq, on_carol.message.seq
    );

    let (status, _) = leave_group(&app, &carol.tokens.access_token, &group.id).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    println!("[leave] carol left the group (HTTP 204)");

    println!(
        "[leave] alice received {:?}",
        expect_membership_changed(&mut alice_socket).await.change
    );
    println!(
        "[leave] bob received {:?}",
        expect_membership_changed(&mut bob_socket).await.change
    );
    println!(
        "[leave] carol received {:?}",
        expect_membership_changed(&mut carol_socket).await.change
    );

    send_event(
        &mut bob_socket,
        send_message(&group.id, "cmid-2", "carol is gone"),
    )
    .await;
    expect_ack(&mut bob_socket).await;
    let on_alice = expect_new_message(&mut alice_socket).await;
    println!("[message] bob -> alice seq={}", on_alice.message.seq);

    let carol_traffic = collect_events_within(&mut carol_socket, ABSENCE_WINDOW).await;
    println!(
        "[message] carol received {} events after leaving (expected 0)",
        carol_traffic.len()
    );
    assert!(carol_traffic.is_empty());

    send_event(
        &mut carol_socket,
        send_message(&group.id, "cmid-3", "let me back in"),
    )
    .await;
    let rejection = expect_rejection(&mut carol_socket).await;
    println!("[post] carol's send was rejected: {:?}", rejection.code);
    assert_eq!(rejection.code, jiuyue_contract::ErrorCode::NotAParticipant);

    server.abort();
    app.cleanup().await;
}

/// `AddGroupMembersRequest` is part of the contract; this proves it decodes the
/// body the helper sends, keeping the request shape honest in the test binary.
#[test]
fn the_invite_request_shape_is_the_contract() {
    let request: AddGroupMembersRequest = serde_json::from_str(r#"{"member_usernames":["bob"]}"#)
        .expect("the invite body must decode");

    assert_eq!(request.member_usernames, vec!["bob".to_owned()]);
}

// ---------------------------------------------------------------------------
// The announcement (ticket #15)
// ---------------------------------------------------------------------------

/// `PATCH /conversations/{id}/announcement` with a raw JSON announcement value.
async fn update_announcement(
    app: &TestApp,
    token: &str,
    conversation_id: &str,
    announcement: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let path = format!("/conversations/{conversation_id}/announcement");
    request_json_with_token(
        app,
        Method::PATCH,
        &path,
        Some(&serde_json::json!({ "announcement": announcement })),
        token,
    )
    .await
}

/// The stored announcement, read straight from PostgreSQL.
///
/// Asserting the column and not the response is the point: "nothing was stored"
/// must be true of the database, not merely of what the server echoed back.
async fn stored_announcement(app: &TestApp, conversation_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT announcement FROM conversations WHERE id = $1")
        .bind(conversation_id)
        .fetch_one(app.pool())
        .await
        .expect("reading the stored announcement must succeed")
}

/// Read until the next `ConversationCreated` arrives, ignoring everything else.
///
/// The announcement fan-out reuses this event, so this is the reader for it.
async fn expect_conversation_created(socket: &mut TestSocket) -> ConversationCreated {
    loop {
        if let ServerEvent::ConversationCreated(created) = next_event(socket).await {
            return created;
        }
    }
}

/// Whether any of `events` is a `NewMessage` carrying `id`.
fn carries_new_message(events: &[ServerEvent], id: &str) -> bool {
    events.iter().any(|event| match event {
        ServerEvent::NewMessage(message) => message.message.id == id,
        _ => false,
    })
}

/// The owner edits the announcement; every Participant sees the new text live,
/// each with **their own** Role, and the database holds what was fanned out.
#[tokio::test]
async fn an_announcement_edit_reaches_every_participant_live() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;
    next_envelope(&mut carol_socket).await;

    let (status, body) = update_announcement(
        &app,
        &alice.tokens.access_token,
        &group.id,
        serde_json::json!("周六下午三点线上会议"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the owner may edit: {body}");

    for (who, socket, expected_role) in [
        ("alice", &mut alice_socket, Role::Owner),
        ("bob", &mut bob_socket, Role::Member),
        ("carol", &mut carol_socket, Role::Member),
    ] {
        let created = expect_conversation_created(socket).await;
        assert_eq!(created.conversation.id, group.id, "{who} sees this group");

        let view = created
            .conversation
            .group
            .as_ref()
            .expect("a group summary must exist");
        assert_eq!(
            view.announcement.as_deref(),
            Some("周六下午三点线上会议"),
            "{who} must see the new text without reloading"
        );
        assert_eq!(view.my_role, expected_role, "{who} keeps their own Role");
    }

    assert_eq!(
        stored_announcement(&app, &group.id).await.as_deref(),
        Some("周六下午三点线上会议")
    );

    server.abort();
    app.cleanup().await;
}

/// A member without the capability is refused, and the refusal is a distinct
/// `FORBIDDEN` — not a validation failure and not a silent success.
#[tokio::test]
async fn a_member_may_not_edit_the_announcement_and_nothing_is_stored() {
    let app = TestApp::start().await;
    let (alice, bob, _carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;

    let (status, body) = update_announcement(
        &app,
        &bob.tokens.access_token,
        &group.id,
        serde_json::json!("bob was here"),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "got: {body}");
    assert_eq!(
        error_code(&body),
        "FORBIDDEN",
        "a permission refusal is distinguishable from a bad body"
    );
    assert_eq!(
        stored_announcement(&app, &group.id).await,
        None,
        "a refused edit must store nothing"
    );

    // The refusal did not reach the Participants either: nothing was fanned out.
    assert!(
        next_event_within(&mut alice_socket, ABSENCE_WINDOW)
            .await
            .is_none(),
        "a refused edit must not notify anyone"
    );

    server.abort();
    app.cleanup().await;
}

/// An announcement over the cap is refused before the write, so the previous
/// value survives untouched.
#[tokio::test]
async fn an_over_length_announcement_is_refused_and_nothing_is_stored() {
    let app = TestApp::start().await;
    let (alice, _bob, _carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (status, _) = update_announcement(
        &app,
        &alice.tokens.access_token,
        &group.id,
        serde_json::json!("短公告"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stored_announcement(&app, &group.id).await.as_deref(),
        Some("短公告")
    );

    let over = "九".repeat(MAX_ANNOUNCEMENT_CHARS + 1);
    let (status, body) = update_announcement(
        &app,
        &alice.tokens.access_token,
        &group.id,
        serde_json::json!(over),
    )
    .await;

    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "got: {body}");
    assert_eq!(error_code(&body), "VALIDATION_FAILED");
    assert_eq!(
        body["error"]["fields"][0]["field"], "announcement",
        "the failure names the offending field: {body}"
    );
    assert_eq!(
        stored_announcement(&app, &group.id).await.as_deref(),
        Some("短公告"),
        "an over-length announcement must store nothing"
    );

    app.cleanup().await;
}

/// A User who was removed is no longer a Participant, so an announcement edit
/// does not reach them — the fan-out names the recipients, not the group's history.
#[tokio::test]
async fn a_removed_member_receives_no_announcement_update() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut bob_socket).await;
    next_envelope(&mut carol_socket).await;

    remove_group_member(&app, &alice.tokens.access_token, &group.id, &carol.user.id).await;

    // Carol hears about her own removal; that is the last thing she may receive.
    let removal = expect_membership_changed(&mut carol_socket).await;
    assert_eq!(
        removal.change,
        MembershipChange::Removed {
            user_id: carol.user.id.clone()
        }
    );

    let (status, _) = update_announcement(
        &app,
        &alice.tokens.access_token,
        &group.id,
        serde_json::json!("只给还在群里的人看"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Bob is still a Participant, so he sees it.
    let on_bob = expect_conversation_created(&mut bob_socket).await;
    assert_eq!(
        on_bob
            .conversation
            .group
            .as_ref()
            .and_then(|group| group.announcement.as_deref()),
        Some("只给还在群里的人看")
    );

    // Carol is not: her socket stays silent.
    assert!(
        next_event_within(&mut carol_socket, ABSENCE_WINDOW)
            .await
            .is_none(),
        "a removed member must not receive an announcement update"
    );

    server.abort();
    app.cleanup().await;
}

/// Clearing has one stored representation: `null` stores SQL `NULL`.
#[tokio::test]
async fn clearing_an_announcement_stores_null() {
    let app = TestApp::start().await;
    let (alice, _bob, _carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (status, _) = update_announcement(
        &app,
        &alice.tokens.access_token,
        &group.id,
        serde_json::json!("先发一条"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = update_announcement(
        &app,
        &alice.tokens.access_token,
        &group.id,
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "clearing must succeed: {body}");
    assert_eq!(
        stored_announcement(&app, &group.id).await,
        None,
        "clearing stores NULL, never the empty string"
    );

    app.cleanup().await;
}

/// One Message, one delivery per socket — the durable fan-out is exactly-once
/// per Member even though the transport is at-least-once.
#[tokio::test]
async fn a_group_message_reaches_every_online_member_exactly_once() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;
    next_envelope(&mut carol_socket).await;

    send_event(
        &mut alice_socket,
        send_message(&group.id, "cmid-once", "everyone once"),
    )
    .await;
    let ack = expect_ack(&mut alice_socket).await;
    let id = ack.message.id.clone();
    assert_eq!(ack.message.seq, 1);

    // Each socket — the sender's included — receives the stored Message once.
    for socket in [&mut alice_socket, &mut bob_socket, &mut carol_socket] {
        let delivered = expect_new_message(socket).await;
        assert_eq!(
            delivered.message.id, id,
            "every Member receives the stored Message"
        );
        assert_eq!(delivered.message.seq, 1);
    }

    // And no socket receives it a second time.
    for socket in [&mut alice_socket, &mut bob_socket, &mut carol_socket] {
        let tail = collect_events_within(socket, ABSENCE_WINDOW).await;
        assert!(
            !carries_new_message(&tail, &id),
            "a Message must be delivered exactly once per socket"
        );
    }

    server.abort();
    app.cleanup().await;
}

/// The acceptance run for ticket #15, end to end, with the observed events printed.
///
/// Run with `--nocapture` to see the transcript: three real Users on three real
/// sockets, one group message reaching all three, an announcement edit reaching
/// all three live, and a member's edit refused with a distinguishable error.
#[tokio::test]
async fn acceptance_group_message_announcement_and_permission_refusal() {
    let app = TestApp::start().await;
    let (alice, bob, carol) = three_users(&app).await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    let mut carol_socket = connect_socket(&ws_url, &carol.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;
    next_envelope(&mut carol_socket).await;

    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;
    println!(
        "[setup] alice=owner created group {} with bob=member, carol=member",
        group.id
    );

    // One group Message reaches all three, exactly once, on one Sequence Number.
    send_event(
        &mut alice_socket,
        send_message(&group.id, "cmid-acceptance-1", "群消息：大家好"),
    )
    .await;
    let ack = expect_ack(&mut alice_socket).await;
    let on_alice = expect_new_message(&mut alice_socket).await;
    let on_bob = expect_new_message(&mut bob_socket).await;
    let on_carol = expect_new_message(&mut carol_socket).await;

    let one_id = |delivered: &jiuyue_contract::NewMessage| delivered.message.id == ack.message.id;
    println!(
        "[message] seq={} alice={} bob={} carol={}",
        ack.message.seq,
        one_id(&on_alice),
        one_id(&on_bob),
        one_id(&on_carol)
    );
    assert!(
        one_id(&on_alice) && one_id(&on_bob) && one_id(&on_carol),
        "every online member must receive the same stored Message exactly once"
    );
    assert_eq!(
        ack.message.seq, 1,
        "the Conversation's Sequence Number orders it"
    );

    // The owner edits the announcement; all three see the new text live.
    let (status, _) = update_announcement(
        &app,
        &alice.tokens.access_token,
        &group.id,
        serde_json::json!("周六下午三点线上会议"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for (who, socket, expected_role) in [
        ("alice", &mut alice_socket, Role::Owner),
        ("bob", &mut bob_socket, Role::Member),
        ("carol", &mut carol_socket, Role::Member),
    ] {
        let created = expect_conversation_created(socket).await;
        let view = created
            .conversation
            .group
            .as_ref()
            .expect("a group summary must exist");
        println!(
            "[announcement] {who} ({expected_role:?}) received announcement={:?} my_role={:?}",
            view.announcement, view.my_role
        );
        assert_eq!(view.announcement.as_deref(), Some("周六下午三点线上会议"));
        assert_eq!(view.my_role, expected_role);
    }

    // A member without permission is refused, distinguishably.
    let (status, body) = update_announcement(
        &app,
        &bob.tokens.access_token,
        &group.id,
        serde_json::json!("bob 想改公告"),
    )
    .await;
    println!(
        "[refusal] bob's announcement edit -> HTTP {status}, code={}",
        error_code(&body)
    );
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "FORBIDDEN");
    println!(
        "[refusal] stored announcement unchanged: {:?}",
        stored_announcement(&app, &group.id).await
    );
    assert_eq!(
        stored_announcement(&app, &group.id).await.as_deref(),
        Some("周六下午三点线上会议")
    );

    server.abort();
    app.cleanup().await;
}

/// A Member who joins after earlier Messages were sent reads them from history,
/// in Sequence Number order — the late join does not lose the conversation's past.
#[tokio::test]
async fn a_member_who_joins_later_reads_earlier_messages_from_history() {
    let app = TestApp::start().await;
    let (alice, bob, _carol) = three_users(&app).await;
    let dave = register(&app, "dave", "dave@example.com", "secret123").await;
    let group = create_group(
        &app,
        &alice.tokens.access_token,
        "九月小组",
        &["bob", "carol"],
    )
    .await;

    let (ws_url, server) = app.serve_websocket().await;
    let mut alice_socket = connect_socket(&ws_url, &alice.tokens.access_token).await;
    let mut bob_socket = connect_socket(&ws_url, &bob.tokens.access_token).await;
    next_envelope(&mut alice_socket).await;
    next_envelope(&mut bob_socket).await;

    for (client_msg_id, body) in [("cmid-h1", "第一条"), ("cmid-h2", "第二条")] {
        send_event(
            &mut alice_socket,
            send_message(&group.id, client_msg_id, body),
        )
        .await;
        expect_ack(&mut alice_socket).await;
        expect_new_message(&mut bob_socket).await;
    }

    // Dave arrives after both Messages were written.
    add_group_members(&app, &alice.tokens.access_token, &group.id, &["dave"]).await;

    let (status, body) = get_with_token(
        &app,
        &format!("/conversations/{}/messages", group.id),
        &dave.tokens.access_token,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the new Member may read history: {body}"
    );

    let page: MessageList = serde_json::from_value(body).expect("the page must be a MessageList");
    assert_eq!(
        page.messages
            .iter()
            .map(|message| message.seq)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "history is ordered by the Conversation's Sequence Number"
    );
    assert_eq!(
        page.messages
            .iter()
            .map(|message| message.body.as_str())
            .collect::<Vec<_>>(),
        vec!["第一条", "第二条"],
        "a late joiner sees the Messages sent before they joined"
    );

    server.abort();
    app.cleanup().await;
}
