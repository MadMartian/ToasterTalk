use crate::tools::Timeout;

// Per-request host cap, threaded into the tool handlers via a tokio task-local.
//
// `ToasterServer::call_tool` reads the `X-Host-Cap-Secs` request header from the HTTP
// request `Parts` that rmcp injects into the MCP request extensions, then wraps the
// tool dispatch in `REQUEST_HOST_CAP.scope(cap, ...)`. Handlers read the value with
// `REQUEST_HOST_CAP.try_with(|v| *v).ok().flatten()`.
//
// The scope is set in `call_tool` rather than an axum middleware on purpose: rmcp's
// Streamable HTTP transport spawns a separate task per session, and task-locals do not
// cross `tokio::spawn` boundaries — a middleware-set scope would not be visible in the
// task where the tool actually runs. Setting it in `call_tool` keeps the value in-task.
//
// SSE-transport requests carry no such header and simply resolve the task-local to
// `None` (no cap) — the SSE POST returns 202 immediately, so the host HTTP client has
// no long-lived request to time out against.
tokio::task_local! {
    pub static REQUEST_HOST_CAP: Option<u64>;
}

/// Classify the outcome of a capped blocking wait into a Timeout variant.
///
/// `was_capped` — the wait was shortened by the X-Host-Cap-Secs header.
/// `timed_out`  — the wait ended with no result (no messages / no reply).
///
/// Both `send` and `receive` delegate to this function so the classification
/// logic stays in one place and cannot diverge between the two handlers.
pub fn apply_timeout(was_capped: bool, timed_out: bool) -> Timeout {
    if was_capped && timed_out {
        Timeout::Capped
    } else if timed_out {
        Timeout::Elapsed
    } else {
        Timeout::None
    }
}
