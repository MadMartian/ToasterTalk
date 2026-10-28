//! Integration tests for seat-credential contention (flap) detection.
//!
//! Test → rubric mapping:
//!   thrash_freezes_seat_and_dump_shows_it      → TDD 12.6, 13.13
//!   kckm_clears_freeze                         → TDD 12.7
//!   delmb_clears_freeze                        → TDD 12.7

mod common;

use std::sync::Arc;
use tempfile::TempDir;
use toaster_talk_common::parse_quoted_line;
use toaster_talk_server::{
    error::Error,
    rooms::RoomStore,
    state::{AppState, SharedAppState},
    tools::{connect::handle_connect, join::handle_join_seat},
};
use tokio::sync::RwLock;

/// Room + seat created through the real mgmt listener. Returns the socket path
/// and the seat token.
async fn setup(
    dir: &TempDir,
    store: &Arc<RwLock<RoomStore>>,
    app: &SharedAppState,
) -> (std::path::PathBuf, String) {
    let socket = common::test_mgmt_addr(dir);
    common::create_room(store, "flaproom").await;
    common::spawn_mgmt(&socket, store, app).await;
    let created = common::mgmt_request(&socket, "ADDMB \"flaproom\" \"alice\"").await;
    let seat_token = parse_quoted_line(&created)[3].clone();
    (socket, seat_token)
}

/// One agent's reconnect-and-join cycle with a fresh session (default policy:
/// freeze on the 4th supersession within 30 s).
async fn seat_join(
    app: &SharedAppState,
    store: &Arc<RwLock<RoomStore>>,
    seat_token: &str,
) -> Result<(), Error> {
    let session = handle_connect(app, None).await.unwrap().session_token;
    handle_join_seat(app, store, session, seat_token.to_string(), "alice".into())
        .await
        .map(|_| ())
}

/// Drive the seat into the frozen state: initial claim + 4 supersessions.
async fn thrash_until_frozen(
    app: &SharedAppState,
    store: &Arc<RwLock<RoomStore>>,
    seat_token: &str,
) {
    for _ in 0..5 {
        seat_join(app, store, seat_token).await.unwrap();
    }
    assert!(matches!(
        seat_join(app, store, seat_token).await.unwrap_err(),
        Error::SeatContention { .. }
    ));
}

fn alice_dump_row(response: &str) -> serde_yml::Value {
    let body: String = response.lines().skip(1).collect::<Vec<_>>().join("\n");
    let yaml: serde_yml::Value = serde_yml::from_str(&body).expect("DUMP body is YAML");
    yaml["rooms"][0]["members"][0].clone()
}

#[tokio::test]
async fn thrash_freezes_seat_and_dump_shows_it() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let (socket, seat_token) = setup(&dir, &store, &app).await;

    // Before any supersession the seat row shows a quiet history.
    seat_join(&app, &store, &seat_token).await.unwrap();
    let row = alice_dump_row(&common::mgmt_request(&socket, "DUMP").await);
    assert_eq!(row["recent_supersedes"].as_u64(), Some(0));
    assert_eq!(row["last_supersede_secs_ago"].as_str(), Some("never"));
    assert_eq!(row["seat_frozen_secs_remaining"].as_str(), Some("no"));

    for _ in 0..4 {
        seat_join(&app, &store, &seat_token).await.unwrap();
    }
    assert!(matches!(
        seat_join(&app, &store, &seat_token).await.unwrap_err(),
        Error::SeatContention { .. }
    ));

    let response = common::mgmt_request(&socket, "DUMP").await;
    let row = alice_dump_row(&response);
    assert_eq!(row["member_name"].as_str(), Some("alice"));
    assert_eq!(row["recent_supersedes"].as_u64(), Some(4));
    assert!(row["last_supersede_secs_ago"].as_u64().is_some());
    let frozen = row["seat_frozen_secs_remaining"]
        .as_u64()
        .expect("frozen seat shows secs");
    assert!((1..=60).contains(&frozen), "got {frozen}");
    // The dump must never carry a seat token.
    assert!(!response.contains(&seat_token));
}

#[tokio::test]
async fn kckm_clears_freeze() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let (socket, seat_token) = setup(&dir, &store, &app).await;
    thrash_until_frozen(&app, &store, &seat_token).await;

    let kicked = common::mgmt_request(&socket, "KCKM \"flaproom\" \"alice\"").await;
    assert!(kicked.starts_with("+KICKED"), "got: {kicked}");
    seat_join(&app, &store, &seat_token)
        .await
        .expect("KCKM must clear the freeze");
    // A post-clear supersession is accepted too (history reset, not just the freeze).
    seat_join(&app, &store, &seat_token).await.unwrap();
}

#[tokio::test]
async fn delmb_clears_freeze() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let (socket, seat_token) = setup(&dir, &store, &app).await;
    thrash_until_frozen(&app, &store, &seat_token).await;

    let deleted = common::mgmt_request(&socket, "DELMB \"flaproom\" \"alice\"").await;
    assert!(deleted.starts_with("+SEAT_DELETED"), "got: {deleted}");
    assert!(app.read().await.seat_flaps.is_empty());

    // Re-creating the seat regenerates the identical token; it starts unfrozen.
    common::mgmt_request(&socket, "ADDMB \"flaproom\" \"alice\"").await;
    seat_join(&app, &store, &seat_token).await.unwrap();
    seat_join(&app, &store, &seat_token).await.unwrap();
}
