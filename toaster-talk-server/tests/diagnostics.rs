//! Integration tests for the `DUMP` diagnostic command (TDD section 13).
mod common;

use common::{create_room, join, make_app, make_store, mgmt_request, spawn_mgmt};
use tempfile::TempDir;
use toaster_talk_server::tools::connect::handle_connect;

/// Split a `+DUMP` response into its YAML body (everything after the header line).
fn dump_body(response: &str) -> String {
    let mut lines = response.lines();
    assert_eq!(
        lines.next(),
        Some("+DUMP"),
        "DUMP response must start with the +DUMP header; got: {response}"
    );
    lines.collect::<Vec<_>>().join("\n")
}

/// TDD 13.2/13.3/13.4/13.5/13.8/13.11: the dump includes the room, member, and
/// session/inbox fields, and the body is well-formed YAML.
#[tokio::test]
async fn dump_includes_all_live_state_as_valid_yaml() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = make_app();
    let room_token = create_room(&store, "diag-room").await;
    let session = join(&app, &store, &room_token, "alice").await;

    let socket = common::test_mgmt_addr(&dir);
    spawn_mgmt(&socket, &store, &app).await;

    let response = mgmt_request(&socket, "DUMP").await;
    let body = dump_body(&response);

    // TDD 13.8: body parses as well-formed YAML.
    let yaml: serde_yml::Value =
        serde_yml::from_str(&body).expect("DUMP body must be well-formed YAML");

    // TDD 13.2: the room and member appear.
    let room = &yaml["rooms"][0];
    assert_eq!(room["name"].as_str(), Some("diag-room"));
    assert_eq!(room["member_count"].as_u64(), Some(1));
    let member = &room["members"][0];
    assert_eq!(member["member_name"].as_str(), Some("alice"));

    // TDD 13.5: session is shown and round-trips through the session map.
    assert_eq!(member["session_in_map"].as_bool(), Some(true));

    // TDD 13.x: a room-token member shows its auth method and a numeric idle
    // countdown (it is mortal, not exempt).
    assert_eq!(member["auth_method"].as_str(), Some("room_token"));
    assert!(
        member["idle_eviction_secs"].as_u64().is_some(),
        "a room-token member must show a numeric idle countdown, got: {:?}",
        member["idle_eviction_secs"]
    );

    // TDD 13.11: the shared inbox is rendered once, under `sessions`, not nested
    // under the room row.
    assert!(room["members"][0].get("inbox").is_none());
    let sessions = yaml["sessions"].as_sequence().unwrap();
    assert_eq!(sessions.len(), 1);
    let s = &sessions[0];
    assert_eq!(
        s["memberships"][0].as_str(),
        Some("diag-room/alice"),
        "session entry must cross-reference its room membership"
    );

    // TDD 13.12: every session shows its connect age.
    assert!(
        s["connected_secs_ago"].as_u64().is_some(),
        "session entry must show connected_secs_ago"
    );
    let inbox = &s["inbox"];
    assert!(inbox["queue_depth"].as_u64().is_some());
    assert!(inbox["receive_active"].as_bool().is_some());
    assert!(inbox["active_gen"].as_u64().is_some());
    assert!(inbox["gen_counter"].as_u64().is_some());
    assert!(inbox["abort_handle_present"].as_bool().is_some());
    assert!(inbox["arc_strong_count"].as_u64().is_some());

    // TDD 13.7: a healthy snapshot is clearly OK.
    let invariants = yaml["invariants"].as_sequence().unwrap();
    assert!(invariants
        .iter()
        .any(|i| i.as_str().is_some_and(|s| s.starts_with("OK"))));

    // TDD 13.6: the full session token never appears — only its 8-char prefix.
    assert!(
        !body.contains(session.as_str()),
        "full session token must not appear in the dump"
    );
    assert!(
        body.contains(&session.as_str()[..8]),
        "redacted session prefix should appear in the dump"
    );
    // TDD 13.6: the room token never appears in any form.
    assert!(
        !body.contains(&room_token),
        "room token must never appear in the dump"
    );
}

/// TDD 13.1: producing a dump mutates nothing — the member stays connected and
/// a second identical dump returns the same membership.
#[tokio::test]
async fn dump_is_read_only() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = make_app();
    let room_token = create_room(&store, "diag-room").await;
    let session = join(&app, &store, &room_token, "alice").await;

    let socket = common::test_mgmt_addr(&dir);
    spawn_mgmt(&socket, &store, &app).await;

    let first = dump_body(&mgmt_request(&socket, "DUMP").await);
    let second = dump_body(&mgmt_request(&socket, "DUMP").await);

    // The member is still registered after dumping (session not invalidated).
    {
        let app_read = app.read().await;
        let memberships = app_read
            .session_memberships(&session)
            .expect("session must still be valid after a read-only dump");
        assert!(memberships
            .iter()
            .any(|(rt, mn)| rt.as_str() == room_token && mn == "alice"));
    }

    // Two consecutive dumps describe the same membership.
    let a: serde_yml::Value = serde_yml::from_str(&first).unwrap();
    let b: serde_yml::Value = serde_yml::from_str(&second).unwrap();
    assert_eq!(a["rooms"][0]["name"], b["rooms"][0]["name"]);
    assert_eq!(
        a["rooms"][0]["members"][0]["member_name"],
        b["rooms"][0]["members"][0]["member_name"]
    );
}

/// TDD 13.11: a session belonging to two rooms renders its shared inbox exactly
/// once, cross-referencing both room memberships.
#[tokio::test]
async fn dump_groups_shared_inbox_once_per_multi_room_session() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = make_app();
    let room_a = create_room(&store, "room-a").await;
    let room_b = create_room(&store, "room-b").await;

    let session = {
        let session_token = toaster_talk_server::tools::connect::handle_connect(&app, None)
            .await
            .unwrap()
            .session_token;
        toaster_talk_server::tools::join::handle_join_room_token(
            &app,
            &store,
            session_token.clone(),
            room_a,
            "alice-a".into(),
        )
        .await
        .unwrap();
        toaster_talk_server::tools::join::handle_join_room_token(
            &app,
            &store,
            session_token.clone(),
            room_b,
            "alice-b".into(),
        )
        .await
        .unwrap();
        session_token
    };
    let _ = session;

    let socket = common::test_mgmt_addr(&dir);
    spawn_mgmt(&socket, &store, &app).await;

    let body = dump_body(&mgmt_request(&socket, "DUMP").await);
    let yaml: serde_yml::Value = serde_yml::from_str(&body).unwrap();

    assert_eq!(yaml["rooms"].as_sequence().unwrap().len(), 2);
    let sessions = yaml["sessions"].as_sequence().unwrap();
    assert_eq!(sessions.len(), 1, "one session, rendered once");
    let memberships = sessions[0]["memberships"].as_sequence().unwrap();
    assert_eq!(memberships.len(), 2);
}

/// TDD 13.12: a session that never got past `connect` (no `join` yet) is
/// distinguishable in the dump from a genuinely stuck one by its connect age —
/// `memberships: []` alone can't tell the two apart.
#[tokio::test]
async fn dump_shows_connected_age_for_roomless_session() {
    let dir = TempDir::new().unwrap();
    let store = make_store(&dir);
    let app = make_app();

    handle_connect(&app, None).await.unwrap();

    let socket = common::test_mgmt_addr(&dir);
    spawn_mgmt(&socket, &store, &app).await;

    let body = dump_body(&mgmt_request(&socket, "DUMP").await);
    let yaml: serde_yml::Value = serde_yml::from_str(&body).unwrap();

    let sessions = yaml["sessions"].as_sequence().unwrap();
    assert_eq!(sessions.len(), 1);
    let s = &sessions[0];
    assert_eq!(
        s["memberships"].as_sequence().unwrap().len(),
        0,
        "a bare connect has no room memberships"
    );
    assert!(
        s["connected_secs_ago"].as_u64().is_some(),
        "a roomless session must still show how long it has been connected"
    );
}
