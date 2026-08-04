//! Integration tests — TDD Section 1a/1b: Connect and Join
//!
//! Test → rubric mapping:
//!   test_join_duplicate_member_name          → TDD 1b.3
//!   test_join_rejects_rejoining_own_room      → TDD 1b.7
//!   test_stale_session_after_restart          → TDD 1a.3 (handler half)
//!   test_connect_after_server_restart_then_join → TDD 1a.2

mod common;

use std::sync::Arc;
use tempfile::TempDir;
use toaster_talk_server::{
    error::Error,
    rooms::RoomStore,
    state::AppState,
    tools::{connect::handle_connect, join::handle_join_room_token, send::handle_send},
};
use tokio::sync::RwLock;

/// Rubric 1b.3: a second session joining an already-active member name (a
/// different session, so no "rejoining my own room" exemption applies) must be
/// rejected.
#[tokio::test]
async fn test_join_duplicate_member_name() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let room = common::create_room(&store, "room1").await;

    // First agent joins as "alice".
    let first_session = common::join(&app, &store, &room, "alice").await;

    // A second, independent session tries to join as "alice" — must be rejected.
    let second_session = handle_connect(&app, None).await.unwrap().session_token;
    let result = handle_join_room_token(&app, &store, second_session, room, "alice".into()).await;
    assert!(
        matches!(result.unwrap_err(), Error::MemberNameTaken(_)),
        "duplicate name from a different session must be rejected"
    );

    // First session must still be active.
    let state = app.read().await;
    assert!(
        state.sessions.contains_key(&first_session),
        "first session must remain active after rejected duplicate"
    );
}

/// Rubric 1b.7: a session that has already joined a room is rejected — not a
/// silent no-op — if it tries to join that same room again.
#[tokio::test]
async fn test_join_rejects_rejoining_own_room() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    let room = common::create_room(&store, "room1").await;
    let session = handle_connect(&app, None).await.unwrap().session_token;
    handle_join_room_token(&app, &store, session.clone(), room.clone(), "alice".into())
        .await
        .unwrap();

    let result =
        handle_join_room_token(&app, &store, session.clone(), room, "alice-again".into()).await;
    assert!(
        matches!(result.unwrap_err(), Error::AlreadyJoinedRoom(_)),
        "rejoining the same room with the same session must be rejected, not a no-op"
    );

    // The original membership must remain untouched.
    let state = app.read().await;
    assert!(
        state
            .session_memberships(&session)
            .unwrap()
            .iter()
            .any(|(_, mn)| mn == "alice"),
        "the original membership must survive a rejected rejoin"
    );
}

/// Rubric 1a.3 (handler half): after a server restart, any tool call using a
/// pre-restart session token returns a descriptive `InvalidSessionToken` error.
/// This covers the handler; the rubric's And-clause — that the host is signalled to
/// reconnect promptly rather than left hanging (the transport answering an unknown
/// session with a 404 reinitialize signal) — is covered at the transport layer by
/// `tests/streamable_transport.rs`.
#[tokio::test]
async fn test_stale_session_after_restart() {
    let dir = TempDir::new().unwrap();
    let store_path = dir.path().join("rooms.yaml");
    let config = common::test_config();

    // Obtain a session token before the "restart".
    let stale_session = {
        let store = Arc::new(RwLock::new(RoomStore::load(store_path.clone()).unwrap()));
        let app = AppState::new();
        let room = common::create_room(&store, "room1").await;
        common::join(&app, &store, &room, "alice").await
        // app dropped — in-memory state gone, simulating restart
    };

    // Fresh AppState after restart — stale_session is unknown.
    let fresh_app = AppState::new();
    let fresh_store = Arc::new(RwLock::new(RoomStore::load(store_path).unwrap()));

    let result = handle_send(
        &fresh_app,
        &config,
        &fresh_store,
        stale_session,
        "room1".into(),
        "bob".into(),
        "hello".into(),
        None,
        None,
        None,
    )
    .await;

    assert!(
        matches!(result.unwrap_err(), Error::InvalidSessionToken),
        "tool call with a pre-restart session token must return InvalidSessionToken"
    );
}

/// Rubric 1a.2: after a server restart, an agent can reconnect — `connect` mints
/// a fresh session exactly as it would on a first connect (there is no
/// distinction) — and then recovers its room via `join` using the same room
/// token. Room state is persisted to disk and survives the restart; only
/// in-memory session state is lost.
#[tokio::test]
async fn test_connect_after_server_restart_then_join() {
    let dir = TempDir::new().unwrap();
    let store_path = dir.path().join("rooms.yaml");

    // --- Before restart ---
    let room_token = {
        let store = Arc::new(RwLock::new(RoomStore::load(store_path.clone()).unwrap()));
        common::create_room(&store, "persistent-room").await
        // store dropped here — written to disk
    };

    // --- Simulate restart: fresh AppState, RoomStore reloaded from disk ---
    let fresh_app = AppState::new();
    let fresh_store = Arc::new(RwLock::new(RoomStore::load(store_path).unwrap()));

    let session = handle_connect(&fresh_app, None)
        .await
        .unwrap()
        .session_token;
    let result = handle_join_room_token(
        &fresh_app,
        &fresh_store,
        session.clone(),
        room_token,
        "qa".into(),
    )
    .await;

    assert!(
        result.is_ok(),
        "join must succeed after server restart: {:?}",
        result.unwrap_err()
    );
    assert!(
        fresh_app.read().await.sessions.contains_key(&session),
        "session must be registered in fresh AppState"
    );
}
