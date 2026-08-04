//! Integration tests — streamable-HTTP transport layer (TDD 1.6).
//!
//! These tests exercise the rmcp `StreamableHttpService` together with the
//! `map_session_401_to_404` response layer the server wires onto `/mcp`. The
//! handler-level test (`connect::test_stale_session_after_restart`) proves the
//! handler returns `InvalidSessionToken` for a stale token, but it cannot see the
//! transport: rmcp's stateful transport rejects a request bearing an unknown
//! session id with HTTP 401 *before* the handler runs, and MCP hosts read 401 as a
//! connection fault and stall (the post-restart "hang"). The server stays stateful
//! (to keep the server-push channel available) and instead maps that session-401 to
//! 404 — the transport spec's "reinitialize" signal, which the classic SSE
//! transport already returns for an unknown session.
//!
//! Test → rubric mapping:
//!   test_unknown_session_is_mapped_from_401_to_404   → TDD 1.6 (transport)
//!   test_ok_response_passes_through_unchanged        → TDD 1.6 (guard: only 401 is rewritten)

mod common;

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use toaster_talk_server::{http_transport::map_session_401_to_404, server::ToasterServer};

const RECEIVE_CALL: &str = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"receive","arguments":{"session_token":"stale-token-from-before-restart","timeout_secs":3600}}}"#;

/// Build a streamable-HTTP service wrapping a real ToasterServer in the server's
/// actual (stateful) configuration.
fn stateful_service(
    dir: &tempfile::TempDir,
) -> StreamableHttpService<ToasterServer, LocalSessionManager> {
    let store = common::make_store(dir);
    let app = toaster_talk_server::state::AppState::new();
    let config = Arc::new(common::test_config());
    StreamableHttpService::new(
        move || {
            Ok(ToasterServer::new(
                app.clone(),
                store.clone(),
                config.clone(),
            ))
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(), // stateful_mode: true
    )
}

fn tool_call_with_unknown_session() -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-session-id", "00000000-0000-0000-0000-000000000000")
        .body(Body::from(RECEIVE_CALL))
        .unwrap()
}

/// Rubric 1.6 (transport): a tool call carrying an unknown/stale session id — what
/// a client holds after the server restarts — is rejected by the stateful transport
/// with 401, and the server's response layer rewrites that to 404 so the host
/// re-initializes instead of hanging. Asserting both halves pins the cause and the
/// cure in one place.
#[tokio::test]
async fn test_unknown_session_is_mapped_from_401_to_404() {
    let dir = tempfile::TempDir::new().unwrap();
    let svc = stateful_service(&dir);

    // Raw transport behaviour: unknown session → 401 (the cause of the hang).
    let raw = svc.handle(tool_call_with_unknown_session()).await;
    assert_eq!(
        raw.status(),
        StatusCode::UNAUTHORIZED,
        "stateful transport rejects an unknown session id with 401 (the hang's cause)"
    );

    // After the server's /mcp response layer: 401 → 404 (the cure).
    let mapped = map_session_401_to_404(raw.map(Body::new)).await;
    assert_eq!(
        mapped.status(),
        StatusCode::NOT_FOUND,
        "the server must rewrite the session-401 to 404 so the host reinitializes instead of hanging"
    );

    let bytes = axum::body::to_bytes(mapped.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes);
    assert!(
        body.contains("reinitialize"),
        "the 404 body should tell the client to reinitialize; got: {body}"
    );
}

/// Rubric 1.6 (guard): the layer rewrites *only* session-401 responses. A normal
/// 200 (or any non-401) response passes through untouched, so the mapping cannot
/// mask successful tool results.
#[tokio::test]
async fn test_ok_response_passes_through_unchanged() {
    let ok = axum::response::Response::builder()
        .status(StatusCode::OK)
        .body(Body::from("data: {\"jsonrpc\":\"2.0\"}"))
        .unwrap();

    let passed = map_session_401_to_404(ok).await;
    assert_eq!(
        passed.status(),
        StatusCode::OK,
        "a non-401 response must pass through the layer unchanged"
    );
}
