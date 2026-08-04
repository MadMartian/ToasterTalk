//! Integration tests — TDD Section 6: Message TTL
//!
//! Test → rubric mapping:
//!   test_sweep_discards_expired_messages       → TDD 6.2
//!   test_expired_message_filtered_at_receive   → TDD 6.3

mod common;

use std::time::{Duration, Instant};
use tempfile::TempDir;
use toaster_talk_server::{
    state::AppState,
    tools::{receive::handle_receive, send::handle_send, Timeout},
    ttl::do_sweep,
};

/// Rubric 6.2: the TTL sweeper discards expired messages before they can be delivered.
#[tokio::test]
async fn test_sweep_discards_expired_messages() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = common::test_config();

    let room = common::create_room(&store, "room1").await;
    let alice = common::join(&app, &store, &room, "alice").await;
    let bob = common::join(&app, &store, &room, "bob").await;

    handle_send(
        &app,
        &config,
        &store,
        bob,
        "room1".into(),
        "alice".into(),
        "hello".into(),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Backdate the message's expires_at so the sweeper considers it expired.
    {
        let state = app.read().await;
        for room_state in state.rooms.values() {
            if let Some(member) = room_state.members.get("alice") {
                let mut q = member.inbox.queue.lock().await;
                for msg in q.iter_mut() {
                    msg.expires_at = Instant::now() - Duration::from_secs(1);
                }
            }
        }
    }

    do_sweep(&app, &store, 1800, 0).await;

    // Alice's inbox must be empty after the sweep.
    let result = handle_receive(&app, &store, alice, 0, None, None)
        .await
        .unwrap();
    assert_eq!(result.timeout, Timeout::Elapsed);
    assert!(
        result.messages.is_empty(),
        "sweeper must discard expired messages before delivery"
    );
}

/// Rubric 6.3: a message sent with an explicit TTL is not delivered after that TTL elapses,
/// even without a sweep cycle — the receive handler filters it out inline.
#[tokio::test]
async fn test_expired_message_filtered_at_receive() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = common::test_config();

    let room = common::create_room(&store, "room1").await;
    let alice = common::join(&app, &store, &room, "alice").await;
    let bob = common::join(&app, &store, &room, "bob").await;

    // Send with an explicit TTL of 1 second.
    handle_send(
        &app,
        &config,
        &store,
        bob,
        "room1".into(),
        "alice".into(),
        "hello".into(),
        Some(1),
        None,
        None,
    )
    .await
    .unwrap();

    // Backdate expires_at without running the sweeper, simulating elapsed TTL.
    {
        let state = app.read().await;
        for room_state in state.rooms.values() {
            if let Some(member) = room_state.members.get("alice") {
                let mut q = member.inbox.queue.lock().await;
                for msg in q.iter_mut() {
                    msg.expires_at = Instant::now() - Duration::from_secs(1);
                }
            }
        }
    }

    // Receive must filter the expired message without needing a sweep.
    let result = handle_receive(&app, &store, alice, 0, None, None)
        .await
        .unwrap();
    assert_eq!(result.timeout, Timeout::Elapsed);
    assert!(
        result.messages.is_empty(),
        "expired message must be filtered at receive time, not delivered"
    );
}
