//! Integration tests — per-call receive batch cap (`max_receive_batch`), driven
//! through the real MCP tool handlers on `ToasterServer` so the rendered
//! `remaining` field is exercised end to end.
//!
//! Test → rubric mapping:
//!   test_receive_round_trip_drains_backlog_in_capped_batches → TDD 3.11, 3.12
//!   test_send_with_reply_reports_remaining_past_cap          → TDD 4.5

mod common;

use rmcp::handler::server::tool::Parameters;
use std::{sync::Arc, time::Duration};
use tempfile::TempDir;
use toaster_talk_server::{
    config::Config,
    host_cap,
    server::{ReceiveParams, SendParams, ToasterServer},
    state::{AppState, SessionToken},
};

fn text_of(r: &rmcp::model::CallToolResult) -> String {
    r.content[0].as_text().unwrap().text.clone()
}

/// Bodies of the delivered messages, in response order (each body is one line).
fn bodies(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == "body: |")
        .map(|(i, _)| lines[i + 1].trim().to_string())
        .collect()
}

async fn send(
    server: &ToasterServer,
    from: &SessionToken,
    to: &str,
    body: &str,
    await_reply: Option<u64>,
) -> String {
    let r = host_cap::REQUEST_HOST_CAP
        .scope(None, async {
            server
                .send(Parameters(SendParams {
                    session_token: from.as_str().to_owned(),
                    room_name: "room1".into(),
                    to: to.into(),
                    message: body.into(),
                    ttl_secs: None,
                    await_reply_secs: await_reply,
                }))
                .await
                .unwrap()
        })
        .await;
    text_of(&r)
}

async fn receive(server: &ToasterServer, session: &SessionToken, timeout_secs: u64) -> String {
    let r = tokio::time::timeout(
        Duration::from_secs(5),
        host_cap::REQUEST_HOST_CAP.scope(None, async {
            server
                .receive(Parameters(ReceiveParams {
                    session_token: session.as_str().to_owned(),
                    timeout_secs: Some(timeout_secs),
                    room: None,
                }))
                .await
                .unwrap()
        }),
    )
    .await
    .expect("receive with a queued backlog must return without blocking");
    text_of(&r)
}

/// Full round-trip: join → send a backlog larger than the cap → receive in
/// capped batches. Each batch is oldest-first, `remaining` counts down, and a
/// long-timeout receive returns at once while a remainder is queued.
#[tokio::test]
async fn test_receive_round_trip_drains_backlog_in_capped_batches() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = Arc::new(Config {
        max_receive_batch: 3,
        ..common::test_config()
    });
    let room = common::create_room(&store, "room1").await;
    let alice = common::join(&app, &store, &room, "alice").await;
    let bob = common::join(&app, &store, &room, "bob").await;
    let server = ToasterServer::new(app, store, config);

    for i in 0..7 {
        send(&server, &bob, "alice", &format!("m{i}"), None).await;
    }

    let mut seen = Vec::new();
    for expected_remaining in [4, 1, 0] {
        let text = receive(&server, &alice, 3600).await;
        assert!(text.contains("timeout: none"), "got: {text}");
        assert!(
            text.contains(&format!("remaining: {expected_remaining}\n")),
            "expected remaining: {expected_remaining}; got: {text}"
        );
        seen.extend(bodies(&text));
    }
    let expected: Vec<String> = (0..7).map(|i| format!("m{i}")).collect();
    assert_eq!(
        seen, expected,
        "every message delivered once, in FIFO order"
    );

    // Drained: an empty receive reports remaining: 0 alongside elapsed.
    let text = receive(&server, &alice, 0).await;
    assert!(
        text.contains("timeout: elapsed") && text.contains("remaining: 0"),
        "got: {text}"
    );
}

/// Send-with-reply drains with the same cap; the response carries `remaining`
/// when more same-room messages are queued, and they reach the next receive.
#[tokio::test]
async fn test_send_with_reply_reports_remaining_past_cap() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = Arc::new(Config {
        max_receive_batch: 2,
        ..common::test_config()
    });
    let room = common::create_room(&store, "room1").await;
    let alice = common::join(&app, &store, &room, "alice").await;
    let bob = common::join(&app, &store, &room, "bob").await;
    let server = ToasterServer::new(app, store, config);

    for body in ["r0", "r1", "r2"] {
        send(&server, &bob, "alice", body, None).await;
    }
    let text = tokio::time::timeout(
        Duration::from_secs(5),
        send(&server, &alice, "bob", "question", Some(3600)),
    )
    .await
    .expect("queued same-room messages satisfy the reply-wait at once");
    assert!(text.contains("remaining: 1\n"), "got: {text}");
    assert_eq!(bodies(&text), ["r0", "r1"]);

    let text = receive(&server, &alice, 0).await;
    assert_eq!(bodies(&text), ["r2"]);
    assert!(text.contains("remaining: 0"), "got: {text}");
}
