//! Integration tests — per-request host cap enforced via X-Host-Cap-Secs header,
//! and server-side max-timeout clamping.
//!
//! The axum middleware on POST /mcp reads the header and injects it into a
//! task-local. Tests simulate that by scoping REQUEST_HOST_CAP directly, which
//! exercises the same code path the middleware uses.
//!
//! Test → rubric mapping:
//!   test_receive_timeout_is_capped_by_host_cap     → TDD 8.6
//!   test_send_with_reply_is_capped_by_host_cap     → TDD 8.6 (And-clause)
//!   test_receive_cap_signalled_in_response         → TDD 9.4
//!   test_receive_no_cap_on_uncapped_session        → TDD 8.7
//!   test_server_side_max_clamps_receive_timeout    → TDD 8.8
//!   test_server_side_max_clamps_await_reply_secs   → TDD 8.8

mod common;

use rmcp::handler::server::tool::Parameters;
use std::{sync::Arc, time::Instant};
use tempfile::TempDir;
use toaster_talk_server::{
    config::Config,
    host_cap,
    server::{ReceiveParams, SendParams, ToasterServer},
    state::AppState,
    tools::{connect::handle_connect, join::handle_join_room_token},
};

/// Connect and join `room_token` as `member`, returning the session token.
async fn connect_and_join(
    app: &toaster_talk_server::state::SharedAppState,
    store: &Arc<tokio::sync::RwLock<toaster_talk_server::rooms::RoomStore>>,
    room_token: String,
    member: &str,
) -> String {
    let session = handle_connect(app, None).await.unwrap().session_token;
    handle_join_room_token(app, store, session.clone(), room_token, member.to_string())
        .await
        .unwrap();
    // The MCP tool params carry the raw wire string.
    session.as_str().to_owned()
}

// ── TDD Rubric 8.6 ────────────────────────────────────────────────────────────

/// Rubric 8.6: when X-Host-Cap-Secs is set on the request, all receive calls
/// capped to that value regardless of the timeout_secs requested.
/// A 3600s receive with X-Host-Cap-Secs: 1 must return in ~1s.
#[tokio::test]
async fn test_receive_timeout_is_capped_by_host_cap() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = Arc::new(common::test_config());

    let room = common::create_room(&store, "room1").await;
    let session = connect_and_join(&app, &store, room, "alice").await;

    let server = ToasterServer::new(app, store, config);

    let start = Instant::now();
    host_cap::REQUEST_HOST_CAP
        .scope(Some(1), async {
            server
                .receive(Parameters(ReceiveParams {
                    session_token: session,
                    timeout_secs: Some(3600),
                    room: None,
                }))
                .await
                .unwrap();
        })
        .await;
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 10,
        "receive must be capped to 1s (X-Host-Cap-Secs) but ran for {elapsed:?}"
    );
}

/// Rubric 8.6 (And-clause): the host cap shortens an in-flight send-with-reply just
/// as it does a receive. A send awaiting a reply for 3600s under X-Host-Cap-Secs: 1
/// must return in ~1s and signal `timeout: capped` (not `elapsed`) — the message
/// having already been delivered.
#[tokio::test]
async fn test_send_with_reply_is_capped_by_host_cap() {
    use toaster_talk_server::server::SendParams;

    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = Arc::new(common::test_config());

    let room = common::create_room(&store, "room1").await;
    let alice = connect_and_join(&app, &store, room.clone(), "alice").await;
    // bob exists as a recipient but never replies, so only the cap ends the wait.
    let _bob = connect_and_join(&app, &store, room, "bob").await;

    let server = ToasterServer::new(app, store, config);

    let start = Instant::now();
    let result = host_cap::REQUEST_HOST_CAP
        .scope(Some(1), async {
            server
                .send(Parameters(SendParams {
                    session_token: alice,
                    room_name: "room1".into(),
                    to: "bob".into(),
                    message: "ping".into(),
                    ttl_secs: None,
                    await_reply_secs: Some(3600),
                }))
                .await
                .unwrap()
        })
        .await;
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 10,
        "send-with-reply must be capped to 1s (X-Host-Cap-Secs) but ran for {elapsed:?}"
    );

    let text = result.content[0].as_text().unwrap().text.as_str();
    assert!(
        text.contains("timeout: capped"),
        "capped send-with-reply must set timeout: capped; got: {text}"
    );
    assert!(
        !text.contains("timeout: elapsed"),
        "capped send-with-reply must not set timeout: elapsed; got: {text}"
    );
}

// ── TDD Rubric 9.4 ────────────────────────────────────────────────────────────

/// Rubric 9.4: when a receive times out because of the host cap, the response
/// signals this with `timeout: capped` (not `timeout: elapsed`). The behavioural
/// guidance for handling `capped` lives in the tool description, not the response.
#[tokio::test]
async fn test_receive_cap_signalled_in_response() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = Arc::new(common::test_config());

    let room = common::create_room(&store, "room1").await;
    let session = connect_and_join(&app, &store, room, "alice").await;

    let server = ToasterServer::new(app, store, config);

    let result = host_cap::REQUEST_HOST_CAP
        .scope(Some(1), async {
            server
                .receive(Parameters(ReceiveParams {
                    session_token: session,
                    timeout_secs: Some(3600),
                    room: None,
                }))
                .await
                .unwrap()
        })
        .await;

    let text = result.content[0].as_text().unwrap().text.as_str();

    assert!(
        text.contains("timeout: capped"),
        "capped response must set timeout: capped; got: {text}"
    );
    assert!(
        !text.contains("timeout: elapsed"),
        "capped response must not set timeout: elapsed; got: {text}"
    );
    // Behavioural guidance ("call receive again") belongs in the tool description,
    // not the response body — the response carries only the state signal.
    assert!(
        !text.contains("note:"),
        "host-cap instructions must not appear in the response body; got: {text}"
    );
}

// ── TDD Rubric 8.7 ────────────────────────────────────────────────────────────

/// Rubric 8.7: a session without X-Host-Cap-Secs must not be affected by another
/// session's cap. Host A (cap=1s) and host B (no cap) run concurrently;
/// host B's receive must time out only at its own requested timeout.
#[tokio::test]
async fn test_receive_no_cap_on_uncapped_session() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();
    let config = Arc::new(common::test_config());

    let room = common::create_room(&store, "room1").await;

    let _session_a = connect_and_join(&app, &store, room.clone(), "host-a").await;
    let session_b = connect_and_join(&app, &store, room, "host-b").await;

    let server = ToasterServer::new(app, store, config);

    // Host B requests a 0s timeout with no cap header — should time out immediately with no cap note.
    let result = host_cap::REQUEST_HOST_CAP
        .scope(None, async {
            server
                .receive(Parameters(ReceiveParams {
                    session_token: session_b,
                    timeout_secs: Some(0),
                    room: None,
                }))
                .await
                .unwrap()
        })
        .await;

    let text = result.content[0].as_text().unwrap().text.as_str();

    assert!(
        text.contains("timeout: elapsed"),
        "uncapped response must set timeout: elapsed; got: {text}"
    );
    assert!(
        !text.contains("timeout: capped"),
        "host B must not see timeout: capped; got: {text}"
    );
}

// ── TDD Rubric 8.8 ────────────────────────────────────────────────────────────

/// Rubric 8.8: when the server-configured max_receive_timeout_secs is lower than
/// the requested timeout_secs, the server silently clamps the wait to the max.
#[tokio::test]
async fn test_server_side_max_clamps_receive_timeout() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    // Config with max_receive_timeout_secs = 0 — any requested timeout is clamped to 0.
    let config = Arc::new(Config {
        max_receive_timeout_secs: 0,
        ..common::test_config()
    });

    let room = common::create_room(&store, "room1").await;
    let session = connect_and_join(&app, &store, room, "alice").await;

    let server = ToasterServer::new(app, store, config);

    let start = Instant::now();
    // Request 3600s but the server max is 0 — must return immediately.
    host_cap::REQUEST_HOST_CAP
        .scope(None, async {
            server
                .receive(Parameters(ReceiveParams {
                    session_token: session,
                    timeout_secs: Some(3600),
                    room: None,
                }))
                .await
                .unwrap();
        })
        .await;

    assert!(
        start.elapsed().as_secs() < 5,
        "server-side clamp must cap receive to 0s"
    );
}

/// Rubric 8.8: when the server-configured max_await_reply_secs is lower than
/// the requested await_reply_secs, the server silently clamps the wait.
#[tokio::test]
async fn test_server_side_max_clamps_await_reply_secs() {
    let dir = TempDir::new().unwrap();
    let store = common::make_store(&dir);
    let app = AppState::new();

    // Config with max_await_reply_secs = 0 — any requested wait is clamped to 0.
    let config = Arc::new(Config {
        max_await_reply_secs: 0,
        ..common::test_config()
    });

    let room = common::create_room(&store, "room1").await;
    let alice = connect_and_join(&app, &store, room.clone(), "alice").await;
    let _bob = connect_and_join(&app, &store, room, "bob").await;

    let server = ToasterServer::new(app, store, config);

    let start = Instant::now();
    // Request 3600s reply wait but the server max is 0 — must return immediately.
    host_cap::REQUEST_HOST_CAP
        .scope(None, async {
            server
                .send(Parameters(SendParams {
                    session_token: alice,
                    room_name: "room1".into(),
                    to: "bob".into(),
                    message: "ping".into(),
                    ttl_secs: None,
                    await_reply_secs: Some(3600),
                }))
                .await
                .unwrap();
        })
        .await;

    assert!(
        start.elapsed().as_secs() < 5,
        "server-side clamp must cap await_reply_secs to 0s"
    );
}
