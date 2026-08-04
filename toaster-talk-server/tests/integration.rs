//! Integration tests for ToasterTalk MCP Server.
//!
//! Each test maps to one or more TDD rubrics from sdd/TDD.md. Integration tests
//! validate business-level outcomes; they do not test internal implementation
//! details (those belong in unit tests within the source modules).
//!
//! Test → rubric mapping:
//!   test_fire_and_forget_is_one_way                 → TDD 2.3
//!   test_send_with_reply_timeout                    → TDD 4.1
//!   test_send_with_reply_gets_reply                 → TDD 4.2
//!   test_four_member_exchange                       → TDD 7.1
//!   test_cross_room_isolation                       → TDD 7.2
//!   test_mgmt_create_room                           → TDD 10.1
//!   test_mgmt_delete_room                           → TDD 10.2
//!   test_mgmt_list_rooms                            → TDD 10.3
//!   test_room_ttl_sweep_evicts_members              → TDD 10.4
//!   test_mgmt_get_room                              → TDD 10.7
//!   test_receive_slot_preempted_when_handler_abandoned → TDD 9.3
//!   test_send_to_reaped_seat_reports_recipient_gone → TDD 10.13
//!   test_connect_old_session_token_does_not_carry_over_queued_messages → TDD 1a.4
//!   test_roomless_session_is_reaped_after_idle_ttl  → TDD 1a.5
//!   test_roomless_session_is_not_reaped_before_idle_ttl → TDD 1a.5

mod common;

use std::time::{Duration, Instant};
use tempfile::TempDir;
use toaster_talk_server::{
    state::AppState,
    tools::{
        connect::handle_connect,
        join::{handle_join_room_token, handle_join_seat},
        receive::handle_receive,
        send::handle_send,
        Timeout,
    },
    ttl::do_sweep,
};

// ── Member idle-TTL helpers ─────────────────────────────────────────────────

/// Backdate a member's `last_seen` so the idle sweep treats them as inactive.
async fn backdate_last_seen(
    app: &toaster_talk_server::state::SharedAppState,
    member: &str,
    secs: u64,
) {
    let mut state = app.write().await;
    for rs in state.rooms.values_mut() {
        if let Some(m) = rs.members.get_mut(member) {
            m.last_seen = Instant::now()
                .checked_sub(Duration::from_secs(secs))
                .expect("backdated instant underflowed");
            return;
        }
    }
    panic!("member {member} not found");
}

/// Backdate a roomless session's `connect` timestamp so the idle-roomless-session
/// reaper (`AppState::reap_idle_roomless_sessions`) treats it as stale.
async fn backdate_session_connected_at(
    app: &toaster_talk_server::state::SharedAppState,
    session_token: &str,
    secs: u64,
) {
    let mut state = app.write().await;
    let connected_at = state
        .session_connected_at
        .get_mut(session_token)
        .expect("session not found in session_connected_at");
    *connected_at = Instant::now()
        .checked_sub(Duration::from_secs(secs))
        .expect("backdated instant underflowed");
}

/// True if `member` is currently registered in any room.
async fn member_present(app: &toaster_talk_server::state::SharedAppState, member: &str) -> bool {
    app.read()
        .await
        .rooms
        .values()
        .any(|rs| rs.members.contains_key(member))
}

/// Create a reserved seat for `member`, connect (mint a roomless session), and
/// join via the seat token (Seat auth).
async fn join_seat(
    app: &toaster_talk_server::state::SharedAppState,
    store: &std::sync::Arc<tokio::sync::RwLock<toaster_talk_server::rooms::RoomStore>>,
    room_name: &str,
    member: &str,
) -> String {
    let seat_token = {
        let mut s = store.write().await;
        s.create_seat(room_name, member, 0, common::TEST_MASTER_SECRET)
            .await
            .unwrap()
            .0
            .to_string()
    };
    let session_token = handle_connect(app, None).await.unwrap().session_token;
    handle_join_seat(
        app,
        store,
        session_token.clone(),
        seat_token,
        member.to_string(),
    )
    .await
    .unwrap();
    session_token
}

// ── TDD Rubric 2.3 ────────────────────────────────────────────────────────────

/// Rubric 2.3: Fire-and-forget is strictly one-way. A fire-and-forget send never
/// returns messages waiting in the sender's own inbox, and never drains them — the
/// waiting message is still delivered by a subsequent `receive`. (Regression guard
/// for the implicit-receive-on-send blackhole: the prior behavior returned the
/// waiting message in the send response and removed it from the inbox, silently
/// losing it whenever the agent did not read that response.)
#[tokio::test]
async fn test_fire_and_forget_is_one_way() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = common::test_config();

    let room = common::create_room(&store, "room1").await;
    let alice = common::join(&app, &store, &room, "alice").await;
    let bob = common::join(&app, &store, &room, "bob").await;

    // Alice sends to bob — alice's message now sits in bob's inbox.
    handle_send(
        &app,
        &config,
        &store,
        alice,
        "room1".into(),
        "bob".into(),
        "ping".into(),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Bob sends to alice (fire-and-forget). Bob's response must NOT include alice's
    // "ping" — fire-and-forget is one-way.
    let result = handle_send(
        &app,
        &config,
        &store,
        bob.clone(),
        "room1".into(),
        "alice".into(),
        "pong".into(),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    assert!(
        result.messages.is_empty(),
        "fire-and-forget send must not return the sender's own inbox messages"
    );

    // And the "ping" must still be waiting for bob — the send did not drain it.
    let recv = handle_receive(&app, &store, bob, 0, None, None)
        .await
        .unwrap();
    assert_eq!(
        recv.messages.len(),
        1,
        "the waiting message must survive a fire-and-forget send and be delivered by receive"
    );
    assert_eq!(recv.messages[0].from, "alice");
    assert_eq!(recv.messages[0].body, "ping");
}

// ── TDD Rubric 4.1 ────────────────────────────────────────────────────────────

/// Rubric 4.1: When send-with-reply is used and no reply arrives within the timeout,
/// the tool returns successfully with an empty message list (not an error).
#[tokio::test]
async fn test_send_with_reply_timeout() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = common::test_config();

    let room = common::create_room(&store, "room1").await;
    let alice = common::join(&app, &store, &room, "alice").await;
    common::join(&app, &store, &room, "bob").await;

    // Alice sends with a zero-second reply window — nobody replies in time.
    let result = handle_send(
        &app,
        &config,
        &store,
        alice,
        "room1".into(),
        "bob".into(),
        "hello".into(),
        None,
        Some(0),
        None,
    )
    .await
    .unwrap();

    assert!(
        result.messages.is_empty(),
        "timed-out send-with-reply must return an empty message list, not an error"
    );
}

// ── TDD Rubric 4.2 ────────────────────────────────────────────────────────────

/// Rubric 4.2: When send-with-reply is used and the recipient sends a reply before
/// the timeout, the sender receives the reply in the send response.
#[tokio::test]
async fn test_send_with_reply_gets_reply() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = common::test_config();

    let room = common::create_room(&store, "room1").await;
    let alice = common::join(&app, &store, &room, "alice").await;
    let bob = common::join(&app, &store, &room, "bob").await;

    let app2 = app.clone();
    let store2 = store.clone();
    let config2 = common::test_config();

    // Spawn bob: receive alice's message then send a reply.
    let bob_handle = tokio::spawn(async move {
        let recv = handle_receive(&app2, &store2, bob.clone(), 5, None, None)
            .await
            .unwrap();
        assert!(
            !recv.messages.is_empty(),
            "bob must receive alice's message"
        );
        handle_send(
            &app2,
            &config2,
            &store2,
            bob,
            "room1".into(),
            "alice".into(),
            "pong".into(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    });

    // Alice sends to bob and waits up to 5 seconds for a reply.
    let result = handle_send(
        &app,
        &config,
        &store,
        alice,
        "room1".into(),
        "bob".into(),
        "ping".into(),
        None,
        Some(5),
        None,
    )
    .await
    .unwrap();

    bob_handle.await.unwrap();

    assert_eq!(result.messages.len(), 1, "alice must receive bob's reply");
    assert_eq!(result.messages[0].from, "bob");
    assert_eq!(result.messages[0].body, "pong");
}

// ── TDD Rubric 7.1 ────────────────────────────────────────────────────────────

/// Rubric 7.1: Four members (A, B, C, D) in the same room can exchange messages
/// in all permitted directions: A→C, B→D, C→A, D→B.
#[tokio::test]
async fn test_four_member_exchange() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = common::test_config();

    let room = common::create_room(&store, "room1").await;
    let a = common::join(&app, &store, &room, "A").await;
    let b = common::join(&app, &store, &room, "B").await;
    let c = common::join(&app, &store, &room, "C").await;
    let d = common::join(&app, &store, &room, "D").await;

    // A→C and B→D
    handle_send(
        &app,
        &config,
        &store,
        a.clone(),
        "room1".into(),
        "C".into(),
        "from-A".into(),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    handle_send(
        &app,
        &config,
        &store,
        b.clone(),
        "room1".into(),
        "D".into(),
        "from-B".into(),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let c_recv = handle_receive(&app, &store, c.clone(), 1, None, None)
        .await
        .unwrap();
    assert_eq!(c_recv.timeout, Timeout::None);
    assert_eq!(c_recv.messages[0].from, "A");
    assert_eq!(c_recv.messages[0].body, "from-A");

    let d_recv = handle_receive(&app, &store, d.clone(), 1, None, None)
        .await
        .unwrap();
    assert_eq!(d_recv.timeout, Timeout::None);
    assert_eq!(d_recv.messages[0].from, "B");
    assert_eq!(d_recv.messages[0].body, "from-B");

    // C→A and D→B
    handle_send(
        &app,
        &config,
        &store,
        c,
        "room1".into(),
        "A".into(),
        "from-C".into(),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    handle_send(
        &app,
        &config,
        &store,
        d,
        "room1".into(),
        "B".into(),
        "from-D".into(),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let a_recv = handle_receive(&app, &store, a, 1, None, None)
        .await
        .unwrap();
    assert_eq!(a_recv.timeout, Timeout::None);
    assert_eq!(a_recv.messages[0].from, "C");
    assert_eq!(a_recv.messages[0].body, "from-C");

    let b_recv = handle_receive(&app, &store, b, 1, None, None)
        .await
        .unwrap();
    assert_eq!(b_recv.timeout, Timeout::None);
    assert_eq!(b_recv.messages[0].from, "D");
    assert_eq!(b_recv.messages[0].body, "from-D");
}

// ── TDD Rubric 7.2 ────────────────────────────────────────────────────────────

/// Rubric 7.2: Members in separate rooms cannot exchange messages. Attempting to
/// send across room boundaries results in an error.
///
/// Note: the current implementation returns `MemberNotFound` because the recipient
/// is simply not present in the sender's room. A future enhancement (tracked via
/// the `CrossRoomMessage` error variant) may return a more specific error message.
#[tokio::test]
async fn test_cross_room_isolation() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = common::test_config();

    let room1 = common::create_room(&store, "room1").await;
    let room2 = common::create_room(&store, "room2").await;

    let a = common::join(&app, &store, &room1, "A").await;
    let b = common::join(&app, &store, &room1, "B").await;
    let c = common::join(&app, &store, &room2, "C").await;
    let d = common::join(&app, &store, &room2, "D").await;

    // A→C must fail (C is in room2, A is in room1)
    let r1 = handle_send(
        &app,
        &config,
        &store,
        a,
        "room1".into(),
        "C".into(),
        "cross".into(),
        None,
        None,
        None,
    )
    .await;
    assert!(r1.is_err(), "A→C cross-room send must fail");

    // B→D must fail
    let r2 = handle_send(
        &app,
        &config,
        &store,
        b,
        "room1".into(),
        "D".into(),
        "cross".into(),
        None,
        None,
        None,
    )
    .await;
    assert!(r2.is_err(), "B→D cross-room send must fail");

    // C→A must fail
    let r3 = handle_send(
        &app,
        &config,
        &store,
        c,
        "room2".into(),
        "A".into(),
        "cross".into(),
        None,
        None,
        None,
    )
    .await;
    assert!(r3.is_err(), "C→A cross-room send must fail");

    // D→B must fail
    let r4 = handle_send(
        &app,
        &config,
        &store,
        d,
        "room2".into(),
        "B".into(),
        "cross".into(),
        None,
        None,
        None,
    )
    .await;
    assert!(r4.is_err(), "D→B cross-room send must fail");
}

// ── TDD Rubric 10.1 ───────────────────────────────────────────────────────────

/// Rubric 10.1: Creating a room via the management CLI writes a token file and
/// registers the room with the server.
#[tokio::test]
async fn test_mgmt_create_room() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let socket_path = common::test_mgmt_addr(&dir);

    common::spawn_mgmt(&socket_path, &store, &app).await;

    let response = common::mgmt_request(&socket_path, "ADDRM \"test-room\"").await;

    assert!(
        response.starts_with("+ROOM_CREATED"),
        "expected +ROOM_CREATED, got: {response}"
    );

    // Room must be registered in the store.
    assert!(
        store.read().await.get_by_name("test-room").is_some(),
        "room must be registered in the store"
    );

    // Response must include the token (format: +ROOM_CREATED "<name>" <token> <expires>)
    // so the CLI can write the token file on its own host.
    let parts: Vec<&str> = response.splitn(4, ' ').collect();
    assert!(parts.len() >= 3, "response must include name and token");
    assert_eq!(parts[1], "\"test-room\"", "room name must be double-quoted");
    let token_in_response = parts[2];
    let store_read = store.read().await;
    let room = store_read.get_by_name("test-room").unwrap();
    assert_eq!(
        room.token, token_in_response,
        "token in response must match the stored room token"
    );
}

// ── TDD Rubric 10.2 ───────────────────────────────────────────────────────────

/// Rubric 10.2: Deleting a room via the management CLI unregisters it,
/// invalidates its token, and evicts any connected members.
#[tokio::test]
async fn test_mgmt_delete_room() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let socket_path = common::test_mgmt_addr(&dir);

    // Pre-create a room and connect a member so we can verify eviction.
    let room_token = common::create_room(&store, "to-delete").await;
    let session = common::join(&app, &store, &room_token, "alice").await;

    common::spawn_mgmt(&socket_path, &store, &app).await;

    let response = common::mgmt_request(&socket_path, "DELRM \"to-delete\"").await;

    assert!(
        response.starts_with("+ROOM_DELETED"),
        "expected +ROOM_DELETED, got: {response}"
    );

    // Token must be invalidated — verify_token returns None.
    assert!(
        store.read().await.verify_token(&room_token).is_none(),
        "room token must be invalidated after deletion"
    );

    // Alice's session must be evicted from AppState.
    assert!(
        !app.read().await.sessions.contains_key(&session),
        "alice's session must be evicted when her room is deleted"
    );
}

// ── TDD Rubric 10.3 ───────────────────────────────────────────────────────────

/// Rubric 10.3: Listing rooms via the management CLI returns room metadata
/// without exposing sensitive token information.
#[tokio::test]
async fn test_mgmt_list_rooms() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let socket_path = common::test_mgmt_addr(&dir);

    common::create_room(&store, "room-alpha").await;
    common::create_room(&store, "room-beta").await;

    common::spawn_mgmt(&socket_path, &store, &app).await;

    let response = common::mgmt_request(&socket_path, "LSRM").await;

    let lines: Vec<&str> = response.lines().collect();

    // First line must be the count.
    assert!(
        lines[0].starts_with("*2"),
        "expected count *2, got: {}",
        lines[0]
    );

    // Both room names must appear.
    assert!(response.contains("room-alpha"), "room-alpha must appear");
    assert!(response.contains("room-beta"), "room-beta must appear");

    // Room tokens must NOT appear in the listing.
    let store_read = store.read().await;
    for room in store_read.list() {
        assert!(
            !response.contains(room.token.as_str()),
            "room token must not be exposed in LIST-ROOMS output"
        );
    }
}

// ── TDD Rubric 10.7 ───────────────────────────────────────────────────────────

/// Rubric 10.7: GTRM returns room details including the sorted list of currently
/// connected member names.
#[tokio::test]
async fn test_mgmt_get_room() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let socket_path = common::test_mgmt_addr(&dir);

    let room = common::create_room(&store, "detail-room").await;
    common::join(&app, &store, &room, "alice").await;
    common::join(&app, &store, &room, "bob").await;

    common::spawn_mgmt(&socket_path, &store, &app).await;

    let response = common::mgmt_request(&socket_path, "GTRM \"detail-room\"").await;

    assert!(
        response.starts_with("+ROOM"),
        "expected +ROOM response, got: {response}"
    );
    assert!(
        response.contains("members:2"),
        "member count must be 2, got: {response}"
    );
    assert!(
        response.contains("alice"),
        "alice must appear in GTRM output"
    );
    assert!(response.contains("bob"), "bob must appear in GTRM output");
}

// ── TDD Rubric 10.4 ───────────────────────────────────────────────────────────

/// Rubric 10.4: When a room's TTL elapses, the next sweep cycle deletes the room,
/// invalidates its token, and evicts all connected members.
#[tokio::test]
async fn test_room_ttl_sweep_evicts_members() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let room_token = common::create_room(&store, "expiring-room").await;
    let alice_session = common::join(&app, &store, &room_token, "alice").await;

    // Confirm alice is registered before the sweep.
    assert!(app.read().await.sessions.contains_key(&alice_session));

    // Expire the room by backdating its expiry timestamp.
    {
        let mut s = store.write().await;
        if let Some(room) = s.rooms.get_mut(room_token.as_str()) {
            room.expires_at_secs = 1; // 1 second after the Unix epoch — expired
        }
    }

    // Run one sweep cycle directly (no sleep required).
    do_sweep(&app, &store, 1800, 0).await;

    // Room must be gone from the store.
    assert!(
        store.read().await.verify_token(&room_token).is_none(),
        "expired room must be removed from the room store"
    );

    // Alice's session must be evicted.
    assert!(
        !app.read().await.sessions.contains_key(&alice_session),
        "alice's session must be evicted when her room expires"
    );
}

// ── TDD Rubric 9.3 ────────────────────────────────────────────────────────────

/// Rubric 9.3: When a connection handler is abandoned without being cancelled
/// (the real-world equivalent of an HTTP client dropping the TCP connection
/// while axum keeps the handler future alive), the receive slot must be released
/// so the next receive call can proceed.
///
/// The distinction from rubric 3.5 (`test_receive_slot_released_on_future_cancellation`):
/// - Rubric 3.5 uses `task.abort()` → Tokio cancels the future → `ReceiveGuard` drops → slot released ✓
/// - This test drops the `JoinHandle` without aborting → future keeps running (axum's behaviour
///   on TCP drop) → new receive preempts via `abort_existing_receive()` → slot claimed by new receive ✓
#[tokio::test]
async fn test_receive_slot_preempted_when_handler_abandoned() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let room = common::create_room(&store, "room1").await;
    let session = common::join(&app, &store, &room, "alice").await;

    // Spawn handle_receive and then drop the JoinHandle WITHOUT aborting.
    // Dropping the handle detaches the task — it keeps running in the background,
    // holding the receive slot. This mirrors axum's behaviour when the HTTP
    // client drops the TCP connection: the server-side future is not cancelled.
    let app_clone = app.clone();
    let store_clone = store.clone();
    let session_clone = session.clone();
    let handle = tokio::spawn(async move {
        handle_receive(&app_clone, &store_clone, session_clone, 60, None, None).await
    });
    // Give the task time to claim the receive slot and register its abort handle.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    drop(handle); // detach without cancel — task keeps running in background

    // A new receive must preempt the stale connection: abort the background sub-task,
    // claim a new generation, and return normally.
    let result = handle_receive(&app, &store, session, 0, None, None).await;
    assert!(
        result.is_ok(),
        "receive must preempt abandoned connection and succeed: {result:?}"
    );
    assert_eq!(result.unwrap().timeout, Timeout::Elapsed);
}

// ── Member idle TTL (TDD section 10 / member lifecycle) ──────────────────────

/// A non-seat (room-token-authed) member idle past `member_idle_ttl_secs` is
/// evicted by the sweeper: session invalidated, member removed, room intact.
#[tokio::test]
async fn test_idle_room_token_member_is_evicted() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let room_token = common::create_room(&store, "idle-room").await;
    let alice = common::join(&app, &store, &room_token, "alice").await; // room-token auth

    backdate_last_seen(&app, "alice", 3600).await; // idle for 1h
    do_sweep(&app, &store, 1800, 0).await; // TTL 30m

    assert!(
        !app.read().await.sessions.contains_key(&alice),
        "idle member's session must be invalidated"
    );
    assert!(
        !member_present(&app, "alice").await,
        "idle member must be removed from the room"
    );
    assert!(
        store.read().await.verify_token(&room_token).is_some(),
        "the room itself must remain after an idle-member eviction"
    );
}

/// A session minted by `connect` but never carried through `join` — no room
/// membership at all — is invisible to the per-room idle sweep and would
/// otherwise live forever. Once idle past `member_idle_ttl_secs`, the sweeper's
/// roomless-session reap step removes it (TDD 1a.5).
#[tokio::test]
async fn test_roomless_session_is_reaped_after_idle_ttl() {
    let app = AppState::new();

    let session = handle_connect(&app, None).await.unwrap().session_token;
    assert!(app.read().await.sessions.contains_key(&session));

    backdate_session_connected_at(&app, &session, 3600).await; // idle for 1h
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    do_sweep(&app, &store, 1800, 0).await; // TTL 30m

    assert!(
        !app.read().await.sessions.contains_key(&session),
        "an idle roomless session must be reaped"
    );
    assert!(
        !app.read().await.session_connected_at.contains_key(&session),
        "the reaped session's connected_at entry must be removed too"
    );
}

/// A roomless session younger than the idle TTL is left alone — the reap is
/// idle-time-based, not an immediate connect/join grace-window rejection (TDD
/// 1a.5).
#[tokio::test]
async fn test_roomless_session_is_not_reaped_before_idle_ttl() {
    let app = AppState::new();

    let session = handle_connect(&app, None).await.unwrap().session_token;

    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    do_sweep(&app, &store, 1800, 0).await; // TTL 30m, session is seconds old

    assert!(
        app.read().await.sessions.contains_key(&session),
        "a freshly connected roomless session must not be reaped"
    );
}

/// The idle window slides on activity: a member idle past the TTL who then performs
/// any activity (here, a `receive`) is NOT evicted by the next sweep — the TTL
/// measures inactivity, not session age, so a working agent is never reaped.
#[tokio::test]
async fn test_activity_slides_idle_window() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let room_token = common::create_room(&store, "slide-room").await;
    let alice = common::join(&app, &store, &room_token, "alice").await;

    // Alice has been idle well past the TTL...
    backdate_last_seen(&app, "alice", 3600).await;
    // ...but she performs activity now — a 0s receive refreshes her last_seen.
    handle_receive(&app, &store, alice.clone(), 0, None, None)
        .await
        .unwrap();

    do_sweep(&app, &store, 1800, 0).await;

    assert!(
        member_present(&app, "alice").await,
        "activity must slide the idle window — a member seen just now must not be reaped"
    );
    assert!(app.read().await.sessions.contains_key(&alice));
}

/// The idle window re-anchors at the *completion* of a blocking call, not just its
/// start. A member parked in a long `receive` is exempt while parked; when the call
/// returns, the idle clock must restart from that moment — so a member who just
/// finished a legitimate long wait is granted a fresh window, not reaped immediately.
#[tokio::test]
async fn test_idle_window_reanchors_on_blocking_call_completion() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let room_token = common::create_room(&store, "reanchor-room").await;
    let session = common::join(&app, &store, &room_token, "alice").await;

    // Start a receive that parks for ~1s (it stamps last_seen at its start).
    let app2 = app.clone();
    let store2 = store.clone();
    let session2 = session.clone();
    let recv =
        tokio::spawn(async move { handle_receive(&app2, &store2, session2, 1, None, None).await });
    tokio::time::sleep(Duration::from_millis(50)).await; // let it park

    // While she is parked, artificially backdate her last_seen far past the TTL.
    // Without completion re-anchoring, this stale stamp would survive the call and
    // get her reaped on the next sweep.
    backdate_last_seen(&app, "alice", 100_000).await;

    // Let the receive run to completion (its 1s timeout elapses).
    recv.await.unwrap().unwrap();

    // The completion must have re-stamped last_seen to ~now, so the sweep spares her.
    do_sweep(&app, &store, 1800, 0).await;
    assert!(
        member_present(&app, "alice").await,
        "a blocking call's completion must re-anchor the idle window — the member \
         must not be reaped right after finishing a legitimate long receive"
    );
    assert!(app.read().await.sessions.contains_key(&session));
}

/// With the reaper disabled (`seat_idle_ttl_secs == 0`), a seat-token-authed member
/// is never idle-evicted, however long it sits idle — the pre-reaper behavior.
#[tokio::test]
async fn test_seat_member_is_never_idle_evicted_when_reaper_disabled() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    common::create_room(&store, "seat-room").await;
    let alice = join_seat(&app, &store, "seat-room", "alice").await; // seat auth

    backdate_last_seen(&app, "alice", 100_000).await; // idle well past any TTL
    do_sweep(&app, &store, 1800, 0).await;

    assert!(
        app.read().await.sessions.contains_key(&alice),
        "seat-authed member must remain after the idle sweep"
    );
    assert!(member_present(&app, "alice").await);
}

/// The dead-seat reaper (ANTI-PATTERNS #10): with `seat_idle_ttl_secs > 0`, a seat
/// member idle past that threshold with NO active receive is reaped — session
/// invalidated, member removed — so its inbox stops black-holing messages. This is
/// the fix for the silent-message-loss defect.
#[tokio::test]
async fn test_dead_seat_is_reaped_when_reaper_enabled() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    common::create_room(&store, "reap-room").await;
    let alice = join_seat(&app, &store, "reap-room", "alice").await; // seat auth

    backdate_last_seen(&app, "alice", 3600).await; // dead for 1h, not receiving
    do_sweep(&app, &store, 1800, 1800).await; // seat TTL 30m → reap

    assert!(
        !app.read().await.sessions.contains_key(&alice),
        "a dead seat past the seat idle TTL must have its session invalidated"
    );
    assert!(
        !member_present(&app, "alice").await,
        "a dead seat past the seat idle TTL must be removed from the room"
    );
}

/// Rubric 10.13, the sender-facing half: once a dead seat is reaped and removed from
/// the room, a peer sending to it is promptly told the recipient is gone rather than
/// black-holing into an undrained inbox. The steady-state error is `MemberNotFound`
/// (the member is gone from the room) — NOT `RecipientEvicted`, which only arises in
/// the close-race. Verified live over the transport in MANUAL-TEST MT-4; this locks
/// it deterministically.
#[tokio::test]
async fn test_send_to_reaped_seat_reports_recipient_gone() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = common::test_config();

    let room_token = common::create_room(&store, "reap-send-room").await;
    let _alice_session = join_seat(&app, &store, "reap-send-room", "alice").await; // recipient (seat)
    let bob = common::join(&app, &store, &room_token, "bob").await; // sender

    // Reap alice: dead seat, not receiving, idle past the seat TTL.
    backdate_last_seen(&app, "alice", 3600).await;
    do_sweep(&app, &store, 1800, 1800).await;
    assert!(
        !member_present(&app, "alice").await,
        "alice must be reaped before the send"
    );

    // Bob sends to the now-gone alice — it must fail-closed, not black-hole.
    let err = handle_send(
        &app,
        &config,
        &store,
        bob,
        "reap-send-room".into(),
        "alice".into(),
        "hello".into(),
        None,
        None,
        None,
    )
    .await
    .expect_err("send to a reaped seat must fail, not silently enqueue");
    assert!(
        err.to_string().contains("not found in room"),
        "a send to a reaped seat must fail-closed as MemberNotFound, got: {err}"
    );
}

/// The seat reaper must never reap a *live* seat: a seat parked in a blocking
/// `receive` is skipped even when idle past the seat TTL and the reaper is enabled.
/// This is why a short seat TTL is safe — a working seat agent holds a receive.
#[tokio::test]
async fn test_parked_seat_is_not_reaped() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    common::create_room(&store, "parked-seat-room").await;
    let alice = join_seat(&app, &store, "parked-seat-room", "alice").await;

    // Park the seat in a long receive so its inbox is receive-active.
    let app_clone = app.clone();
    let store_clone = store.clone();
    let session = alice.clone();
    let handle = tokio::spawn(async move {
        handle_receive(&app_clone, &store_clone, session, 3600, None, None).await
    });
    tokio::time::sleep(Duration::from_millis(20)).await; // let it claim the slot

    backdate_last_seen(&app, "alice", 100_000).await; // idle by the clock...
    do_sweep(&app, &store, 1800, 15).await; // ...reaper enabled (15s) but wait is live

    assert!(
        member_present(&app, "alice").await,
        "a seat parked in a live receive must not be reaped, however idle the clock says"
    );
    assert!(app.read().await.sessions.contains_key(&alice));
    handle.abort();
}

/// Reconnect queue-transfer (ANTI-PATTERNS #11), SEAT path: a message queued for a
/// seat member while it is away must survive the member's reconnect (`join`'s
/// seat-token supersede) and be delivered to the new session — not silently
/// dropped with the old inbox.
#[tokio::test]
async fn test_seat_reconnect_transfers_queued_messages() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = common::test_config();

    let room_token = common::create_room(&store, "xfer-seat-room").await;
    let sender = common::join(&app, &store, &room_token, "sender").await;

    // Seat holder connects (session 1) using a reusable seat token.
    let seat_token = {
        let mut s = store.write().await;
        s.create_seat("xfer-seat-room", "receiver", 0, common::TEST_MASTER_SECRET)
            .await
            .unwrap()
            .0
            .to_string()
    };
    let session1 = handle_connect(&app, None).await.unwrap().session_token;
    handle_join_seat(
        &app,
        &store,
        session1,
        seat_token.clone(),
        "receiver".into(),
    )
    .await
    .unwrap();

    // A message is queued for the seat holder (who is not currently receiving).
    handle_send(
        &app,
        &config,
        &store,
        sender,
        "xfer-seat-room".into(),
        "receiver".into(),
        "carry-me".into(),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Seat holder reconnects (join supersede). The message must carry over.
    let session2 = handle_connect(&app, None).await.unwrap().session_token;
    handle_join_seat(
        &app,
        &store,
        session2.clone(),
        seat_token,
        "receiver".into(),
    )
    .await
    .unwrap();

    let result = handle_receive(&app, &store, session2, 0, None, None)
        .await
        .unwrap();
    assert_eq!(
        result.messages.len(),
        1,
        "seat reconnect must carry over the message queued while away"
    );
    assert_eq!(result.messages[0].body, "carry-me");
    assert_eq!(result.messages[0].from, "sender");
}

/// TDD 1a.4: rotating a session via `connect`'s `old_session_token` invalidates
/// the old session (and its room membership) immediately, and does NOT carry over
/// queued messages or the room membership — recovery goes back through `join`.
/// This replaces the old (pre-connect/join-split) room-token reconnect-carryover
/// test: that specific path no longer exists, since `connect` never touches room
/// state at all now (see PLAN.multi-receive.md's skeleton-key decision).
#[tokio::test]
async fn test_connect_old_session_token_does_not_carry_over_queued_messages() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = common::test_config();

    let room_token = common::create_room(&store, "xfer-rt-room").await;
    let sender = common::join(&app, &store, &room_token, "sender").await;
    let session1 = common::join(&app, &store, &room_token, "receiver").await;

    handle_send(
        &app,
        &config,
        &store,
        sender,
        "xfer-rt-room".into(),
        "receiver".into(),
        "carry-me".into(),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Rotate via old_session_token — this invalidates session1 and its room
    // membership wholesale; no room state or queued messages transfer.
    let session2 = handle_connect(&app, Some(session1.clone()))
        .await
        .unwrap()
        .session_token;
    assert!(
        app.read()
            .await
            .session_memberships(&session2)
            .unwrap()
            .is_empty(),
        "the rotated session must be roomless"
    );
    assert!(
        !app.read()
            .await
            .rooms
            .get(room_token.as_str())
            .unwrap()
            .members
            .contains_key("receiver"),
        "the old room membership must not survive rotation"
    );

    // Re-join the same room under a fresh name — the carried-away message is gone;
    // this is a brand-new membership with an empty inbox.
    handle_join_room_token(
        &app,
        &store,
        session2.clone(),
        room_token,
        "receiver2".into(),
    )
    .await
    .unwrap();
    let result = handle_receive(&app, &store, session2, 0, None, None)
        .await
        .unwrap();
    assert!(
        result.messages.is_empty(),
        "connect's old_session_token path must not carry over queued messages (TDD 1a.4)"
    );
}

/// A member actively parked in `receive` is not idle-evicted while the wait is
/// live — its `last_seen` was stamped when the wait began.
#[tokio::test]
async fn test_parked_receiver_is_not_idle_evicted() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let room_token = common::create_room(&store, "parked-room").await;
    let session = common::join(&app, &store, &room_token, "alice").await;

    // Park alice in a long receive so her inbox is receive-active.
    let app_clone = app.clone();
    let store_clone = store.clone();
    let handle = tokio::spawn(async move {
        handle_receive(&app_clone, &store_clone, session, 3600, None, None).await
    });
    tokio::time::sleep(Duration::from_millis(20)).await; // let her claim the slot

    backdate_last_seen(&app, "alice", 3600).await; // idle by the clock...
    do_sweep(&app, &store, 1800, 0).await; // ...but the wait is live → skipped

    assert!(
        member_present(&app, "alice").await,
        "a member parked in a live receive must not be idle-evicted"
    );
    handle.abort();
}

/// Exemption is by auth method, not name: a room-token member whose name collides
/// with a reserved seat is still idle-evicted (anti-squat rubric).
#[tokio::test]
async fn test_room_token_member_colliding_with_seat_name_is_evicted() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let room_token = common::create_room(&store, "collide-room").await;
    // A seat named "alice" exists...
    {
        let mut s = store.write().await;
        s.create_seat("collide-room", "alice", 0, common::TEST_MASTER_SECRET)
            .await
            .unwrap();
    }
    // ...but alice connects via the unprivileged room token (RoomToken auth).
    let alice = common::join(&app, &store, &room_token, "alice").await;

    backdate_last_seen(&app, "alice", 3600).await;
    do_sweep(&app, &store, 1800, 0).await;

    assert!(
        !member_present(&app, "alice").await,
        "a room-token member must be idle-evicted even if a same-named seat exists"
    );
    assert!(!app.read().await.sessions.contains_key(&alice));
}

/// Room expiry caps every member regardless of auth: a seat-authed member (idle or
/// not) is evicted when its room expires.
#[tokio::test]
async fn test_room_expiry_evicts_seat_member() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let room_token = common::create_room(&store, "expiring-seat-room").await;
    let alice = join_seat(&app, &store, "expiring-seat-room", "alice").await; // exempt from idle TTL

    // Expire the room.
    {
        let mut s = store.write().await;
        if let Some(room) = s.rooms.get_mut(room_token.as_str()) {
            room.expires_at_secs = 1;
        }
    }
    do_sweep(&app, &store, 1800, 0).await;

    assert!(
        !member_present(&app, "alice").await,
        "room expiry must evict even a seat-authed member"
    );
    assert!(
        !app.read().await.sessions.contains_key(&alice),
        "the seat member's session must be invalidated on room expiry"
    );
}
