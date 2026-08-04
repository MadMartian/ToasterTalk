//! Integration tests for seat token management commands (ADDMB, DELMB, LSMB).
//!
//! Test → rubric mapping:
//!   test_addmb_creates_seat_and_returns_token  → TDD 12.1
//!   test_lsmb_lists_seats                      → TDD 12.3
//!   test_delmb_deletes_seat_and_evicts_session → TDD 12.2
//!   test_delmb_seat_token_rejected_on_connect  → TDD 12.4

mod common;

use tempfile::TempDir;
use toaster_talk_common::parse_quoted_line;
use toaster_talk_server::{
    error::Error,
    state::AppState,
    tools::{connect::handle_connect, join::handle_join_seat},
};

// ── TDD Rubric 12.1 ───────────────────────────────────────────────────────────

/// Rubric 12.1: ADDMB creates a seat token for a member and returns it so the CLI
/// can write the seat token file.
#[tokio::test]
async fn test_addmb_creates_seat_and_returns_token() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let socket_path = common::test_mgmt_addr(&dir);

    common::create_room(&store, "seatroom").await;
    common::spawn_mgmt(&socket_path, &store, &app).await;

    let response = common::mgmt_request(&socket_path, "ADDMB \"seatroom\" \"alice\"").await;

    assert!(
        response.starts_with("+SEAT_CREATED"),
        "expected +SEAT_CREATED, got: {response}"
    );

    // Seat must be registered in the store under that member name.
    let store_read = store.read().await;
    let seats = store_read
        .list_seats("seatroom")
        .expect("room must exist for list_seats");
    assert!(
        seats.iter().any(|s| s.member_name == "alice"),
        "alice's seat must be registered after ADDMB"
    );
}

// ── TDD Rubric 12.3 ───────────────────────────────────────────────────────────

/// Rubric 12.3: LSMB lists all registered seats for a room without exposing tokens.
#[tokio::test]
async fn test_lsmb_lists_seats() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let socket_path = common::test_mgmt_addr(&dir);

    common::create_room(&store, "seatroom").await;
    common::spawn_mgmt(&socket_path, &store, &app).await;

    common::mgmt_request(&socket_path, "ADDMB \"seatroom\" \"alice\"").await;
    common::mgmt_request(&socket_path, "ADDMB \"seatroom\" \"bob\"").await;

    let response = common::mgmt_request(&socket_path, "LSMB \"seatroom\"").await;

    assert!(
        response.starts_with("*2"),
        "expected count *2, got: {response}"
    );
    assert!(
        response.contains("alice"),
        "alice must appear in LSMB output"
    );
    assert!(response.contains("bob"), "bob must appear in LSMB output");
    // Each data row carries labelled metadata only (no raw token values).
    assert!(
        response.contains("created:") && response.contains("expires:"),
        "LSMB rows must carry labelled created/expires metadata, got: {response}"
    );
}

// ── TDD Rubric 12.2 ───────────────────────────────────────────────────────────

/// Rubric 12.2: DELMB deletes the seat and evicts any active session for that
/// member, marking it superseded so the agent learns to reconnect.
#[tokio::test]
async fn test_delmb_deletes_seat_and_evicts_session() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let socket_path = common::test_mgmt_addr(&dir);

    common::create_room(&store, "seatroom").await;
    common::spawn_mgmt(&socket_path, &store, &app).await;

    // Create the seat via the mgmt socket and capture the returned seat token.
    let created = common::mgmt_request(&socket_path, "ADDMB \"seatroom\" \"alice\"").await;
    // +SEAT_CREATED "seatroom" "alice" <seat_token> <expires>
    let fields = parse_quoted_line(&created);
    let seat_token = fields[3].clone();

    // Connect, then join with the seat token to establish an active session.
    let session = handle_connect(&app, None).await.unwrap().session_token;
    handle_join_seat(&app, &store, session.clone(), seat_token, "alice".into())
        .await
        .unwrap();
    assert!(app.read().await.sessions.contains_key(&session));

    // Delete the seat — the active session must be superseded.
    let deleted = common::mgmt_request(&socket_path, "DELMB \"seatroom\" \"alice\"").await;
    assert!(
        deleted.starts_with("+SEAT_DELETED"),
        "expected +SEAT_DELETED, got: {deleted}"
    );

    let app_read = app.read().await;
    assert!(
        !app_read.sessions.contains_key(&session),
        "alice's session must be evicted after DELMB"
    );
    assert!(
        app_read.superseded_sessions.contains(&session),
        "alice's session must be marked superseded for one-time delivery"
    );
}

// ── TDD Rubric 12.4 ───────────────────────────────────────────────────────────

/// Rubric 12.4: A deleted seat token is rejected on a subsequent connect attempt.
#[tokio::test]
async fn test_delmb_seat_token_rejected_on_connect() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    common::create_room(&store, "seatroom").await;

    // Create the seat directly and capture its token.
    let seat_token = {
        let mut store_write = store.write().await;
        let (token, _) = store_write
            .create_seat("seatroom", "alice", 0, common::TEST_MASTER_SECRET)
            .await
            .unwrap();
        token.to_string()
    };

    // First join succeeds.
    let session = handle_connect(&app, None).await.unwrap().session_token;
    handle_join_seat(&app, &store, session, seat_token.clone(), "alice".into())
        .await
        .expect("join with a valid seat token must succeed");

    // Operator deletes the seat.
    store
        .write()
        .await
        .delete_seat("seatroom", "alice")
        .await
        .unwrap();

    // The (now deleted) seat token must be rejected. A fresh session, since the
    // first is now the active holder in that room.
    let session2 = handle_connect(&app, None).await.unwrap().session_token;
    let result = handle_join_seat(&app, &store, session2, seat_token, "alice".into()).await;
    assert!(
        matches!(result, Err(Error::InvalidSeatToken)),
        "deleted seat token must be rejected with InvalidSeatToken, got: {result:?}"
    );
}
