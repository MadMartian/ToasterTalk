//! Wiring helpers for the streamable-HTTP MCP transport.

use axum::http::{header, StatusCode};

/// Rewrite the streamable-HTTP transport's session-`401` to `404`.
///
/// rmcp answers a request bearing an unknown `Mcp-Session-Id` — the case a client
/// holds after the server restarts — with **HTTP 401** (`Unauthorized: Session not
/// found`). MCP hosts read 401 as an auth/connection fault and stall (the
/// post-restart "hang"), whereas **HTTP 404** is the transport spec's signal to
/// start a fresh session by re-`initialize`-ing. The classic SSE transport already
/// returns 404 for an unknown session; applying this as a `map_response` layer on
/// the `/mcp` route makes the streamable transport agree, so a restarted server
/// cleanly prompts a reconnect instead of hanging — without abandoning the stateful
/// session the server-push channel needs.
///
/// Mapping every 401 on the `/mcp` route is correct: the streamable transport emits
/// 401 *only* for missing/unknown sessions, and ToasterTalk does no transport-level
/// auth (all auth is the application `session_token`, validated inside the tool
/// handlers, which return their own JSON-RPC errors — never a 401). See TDD 1.6 and
/// ANTI-PATTERNS #9.
pub async fn map_session_401_to_404(resp: axum::response::Response) -> axum::response::Response {
    if resp.status() != StatusCode::UNAUTHORIZED {
        return resp;
    }
    let (mut parts, _body) = resp.into_parts();
    parts.status = StatusCode::NOT_FOUND;
    // The replacement body's length differs from the original; drop the stale
    // Content-Length so hyper recomputes it.
    parts.headers.remove(header::CONTENT_LENGTH);
    axum::response::Response::from_parts(
        parts,
        axum::body::Body::from("Session not found — reinitialize the MCP session"),
    )
}
