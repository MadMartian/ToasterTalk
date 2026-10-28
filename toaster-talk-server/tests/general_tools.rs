//! Integration tests — TDD Section 5: General Tools
//!
//! Test → rubric mapping:
//!   test_delivered_message_includes_age    → TDD 5.1
//!   test_send_includes_last_seen_for_idle  → TDD 5.2

mod common;

use tempfile::TempDir;
use toaster_talk_server::{
    state::AppState,
    tools::{receive::handle_receive, send::handle_send, Timeout},
};

/// Rubric 5.1: a delivered message includes the time it spent waiting in the queue.
#[tokio::test]
async fn test_delivered_message_includes_age() {
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

    let result = handle_receive(&app, &store, alice, 1, None, None, usize::MAX)
        .await
        .unwrap();
    assert_eq!(result.timeout, Timeout::None);
    assert_eq!(result.messages.len(), 1);

    let age = &result.messages[0].age;
    assert!(!age.is_empty(), "age field must be present");
    assert!(
        age.contains("in queue"),
        "age must describe queue wait time, got: {age}"
    );
}

/// Rubric 5.2: when a recipient has not been seen within the idle threshold,
/// the send response includes a human-readable `last_seen` duration.
#[tokio::test]
async fn test_send_includes_last_seen_for_idle_recipient() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    // idle_threshold_secs = 0 means any elapsed time since last_seen triggers the warning.
    let mut config = common::test_config();
    config.idle_threshold_secs = 0;

    let room = common::create_room(&store, "room1").await;
    let _alice = common::join(&app, &store, &room, "alice").await;
    let bob = common::join(&app, &store, &room, "bob").await;

    // Bob sends to alice. Alice's last_seen was set at join time; with
    // threshold = 0 any elapsed duration exceeds it.
    let result = handle_send(
        &app,
        &config,
        &store,
        bob,
        "room1".into(),
        "alice".into(),
        "hi".into(),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    assert!(
        result.last_seen.is_some(),
        "last_seen must be included in the response when recipient exceeds idle threshold"
    );
}
