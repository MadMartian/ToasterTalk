/// MCP server handler.
///
/// Wires the rmcp 0.2 `ServerHandler` trait to our business-logic tool handlers.
/// Uses `#[tool_router]` to generate a `ToolRouter<Self>` from methods marked
/// with `#[tool]`. Tool dispatch and JSON-RPC framing are handled by rmcp;
/// business logic lives in `src/tools/`.
use crate::{
    config::Config,
    error::Error,
    host_cap,
    rooms::RoomStore,
    state::SharedAppState,
    tools::{self, connect, join, leave, receive, roster, send, Timeout},
};
use arc_swap::ArcSwap;
use axum::http::request::Parts;
use rmcp::{
    handler::server::{
        router::tool::ToolRouter,
        tool::{Parameters, ToolCallContext},
    },
    model::{
        CallToolRequestParam, CallToolResult, Content, Implementation, ListToolsResult,
        PaginatedRequestParam, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_router, RoleServer, ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{future::Future, sync::Arc};
use tokio::sync::RwLock;
use tracing::warn;

// ── Server struct ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ToasterServer {
    app: SharedAppState,
    store: Arc<RwLock<RoomStore>>,
    /// Live config cell. A `SIGHUP` reload swaps in a new `Config` here, so a
    /// snapshot taken at the start of each tool call sees the latest tunables —
    /// even for an agent whose MCP session predates the reload. Held as an
    /// `Arc<Config>` snapshot per call (via `load_full`) rather than a borrowed
    /// guard, because tool handlers can block for up to an hour on a long poll.
    config: Arc<ArcSwap<Config>>,
    tool_router: ToolRouter<Self>,
}

impl ToasterServer {
    /// Construct with a static config snapshot. The snapshot is wrapped in a fresh
    /// live cell that never receives a reload — convenient for tests and any
    /// one-shot handler that does not share the server's reloadable config.
    pub fn new(app: SharedAppState, store: Arc<RwLock<RoomStore>>, config: Arc<Config>) -> Self {
        Self::with_live_config(app, store, Arc::new(ArcSwap::from(config)))
    }

    /// Construct sharing the server's live config cell, so a `SIGHUP` reload that
    /// swaps the cell is seen by this handler's subsequent tool calls.
    pub fn with_live_config(
        app: SharedAppState,
        store: Arc<RwLock<RoomStore>>,
        config: Arc<ArcSwap<Config>>,
    ) -> Self {
        Self {
            app,
            store,
            config,
            tool_router: Self::tool_router(),
        }
    }
}

// ── Tool parameter types ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConnectParams {
    /// A prior session_token to rotate. Omit on a genuinely first connect: mints a
    /// fresh, roomless session token, nothing to invalidate. Supplied and still
    /// valid: that session (and every room membership under it) is invalidated
    /// immediately, and a fresh, roomless token is issued in its place — room
    /// membership is never carried over (recover each room afterward via `join`,
    /// using the room or seat token you still hold).
    pub old_session_token: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JoinParams {
    /// Session token from `connect`. Required — join never silently mints a
    /// session; a missing or invalid value is a hard error.
    pub session_token: String,
    /// Seat token from the 3-line ~/.toaster-talk/tokens/user/<room>/<member>.tok file.
    /// Preferred over room_token when present — authorises this member specifically,
    /// and unconditionally evicts (and supersedes) any other session currently
    /// holding this room/member. Exactly one of seat_token or room_token must be
    /// provided.
    pub seat_token: Option<String>,
    /// Room token from the 2-line ~/.toaster-talk/tokens/<room>.tok file.
    /// Use when no seat token is available. Exactly one of seat_token or room_token
    /// must be provided.
    pub room_token: Option<String>,
    /// The member name this agent will use in the room (1–40 chars, [a-zA-Z0-9_-]).
    pub member_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendParams {
    /// Session token returned by connect.
    pub session_token: String,
    /// Name of the room to send in. Must be a room your session has joined (see
    /// `join`) — required because one session's inbox can span several rooms, so
    /// the recipient name alone no longer says which room's namespace to search.
    pub room_name: String,
    /// Member name of the recipient (must be in the same room).
    pub to: String,
    /// Message body (max 100 KB).
    pub message: String,
    /// Message TTL in seconds. Defaults to the server default (660s).
    pub ttl_secs: Option<u64>,
    /// How long to wait for the recipient to reply (seconds).
    /// Omit for fire-and-forget (returns immediately after enqueue).
    /// Set to a large value (e.g. 3600) when you are genuinely awaiting a
    /// reply — this is the promise/await pattern and keeps the connection open.
    /// Set to a small value (e.g. 5) only if you want to poll opportunistically
    /// without blocking long.
    pub await_reply_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReceiveParams {
    /// Session token returned by connect.
    pub session_token: String,
    /// How long to wait for messages before returning an empty result (seconds).
    /// Defaults to the server-configured default (30s unless overridden).
    /// Choose based on intent:
    ///   - Awaiting a reply (promise/await pattern): use a large value (e.g. 3600)
    ///     so the server holds the connection open until a message arrives.
    ///   - Polling / checking opportunistically: use a small value (e.g. 5)
    ///     so the call returns quickly whether or not a message is waiting.
    ///   - Do not use a long timeout if you are not actually expecting a message.
    pub timeout_secs: Option<u64>,
    /// Restrict this receive to messages from one specific room (name, not
    /// token) your session has joined. Omit to block until a message arrives
    /// from ANY of your joined rooms and drain all of them in one batch — each
    /// returned message reports which room it came from. Supplying a room only
    /// drains that room's messages, leaving other rooms' messages queued for a
    /// later call.
    pub room: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MembersParams {
    /// Session token returned by connect.
    pub session_token: String,
    /// Name of the room to list members of. Must be the room your session belongs
    /// to — you can only enumerate members of a room you are connected to.
    pub room_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaveParams {
    /// Session token returned by connect.
    pub session_token: String,
    /// Name of the room to leave. Must be a room your session has joined — you
    /// can only leave a room you are connected to.
    pub room_name: String,
}

// ── Tool implementations ───────────────────────────────────────────────────────

#[tool_router]
impl ToasterServer {
    /// Mint a ToasterTalk session token — identity only, no room membership.
    ///
    /// Call this once per agent process. Omit `old_session_token` on your very
    /// first connect. Every room you want to join — including your first —
    /// happens afterward via `join`, using a room or seat token.
    #[tool(
        description = "Mint a ToasterTalk session token — identity only, never a room membership. Call this once per agent process; every room you join (your very first included) happens afterward via `join`, not here. Omit old_session_token on your first connect. Only pass old_session_token if you are deliberately rotating a still-live session token (e.g. you suspect it leaked) — doing so invalidates that session AND every room membership under it immediately with no carryover; you must `join` each room again afterward using the room/seat token you still hold. Returns a session_token — treat it as credential-grade for as long as it lives: it now grants access to every room your session joins, not just one, so never log it in full or paste it insecurely."
    )]
    async fn connect(&self, p: Parameters<ConnectParams>) -> Result<CallToolResult, rmcp::Error> {
        let result = match connect::handle_connect(&self.app, p.0.old_session_token).await {
            Ok(r) => {
                let body = format!("session_token: {}\n", r.session_token);
                CallToolResult::success(vec![Content::text(body)])
            }
            Err(e) => tool_error(e.to_string()),
        };
        Ok(result)
    }

    /// Join a room — the only way any room membership is ever created.
    ///
    /// Requires a valid `session_token` from `connect`, plus a room or seat
    /// token and the member name to use in that room.
    #[tool(
        description = "Join a room, attaching a room membership to your existing session_token (from connect). This is the only way any room membership is ever created — including your very first room. Requires a valid session_token (a missing/invalid one is a hard error — join never silently mints a session). Token files live in your home directory's `.toaster-talk/tokens` folder — `~/.toaster-talk/tokens` on Unix/macOS, `%USERPROFILE%\\.toaster-talk\\tokens` on Windows. First scan the `tokens/user/` subtree recursively (seat files live in per-room subdirectories, e.g. tokens/user/<room>/<member>.tok) for a 3-line seat token file whose first line matches the room name — if found, pass seat_token (line 3) and member_name (line 2). Otherwise scan `tokens/` for a 2-line room token file and pass room_token (line 2) with your assigned member_name. Exactly one of seat_token or room_token must be provided. A room-token join fails with MemberNameTaken if a different session already holds that name in the room (fatal — do not retry with a different name; surface it to the operator). A seat-token join unconditionally evicts and supersedes any other session currently holding that seat's room/member. Joining a room your own session already belongs to is rejected as redundant, not a silent no-op."
    )]
    async fn join(&self, p: Parameters<JoinParams>) -> Result<CallToolResult, rmcp::Error> {
        let result = match (p.0.seat_token, p.0.room_token) {
            (Some(seat_token), _) => {
                match join::handle_join_seat(
                    &self.app,
                    &self.store,
                    p.0.session_token,
                    seat_token,
                    p.0.member_name,
                )
                .await
                {
                    Ok(r) => {
                        let body = format!("room: \"{}\"\n", r.room_name);
                        CallToolResult::success(vec![Content::text(body)])
                    }
                    Err(e) => tool_error(e.to_string()),
                }
            }
            (None, Some(room_token)) => {
                match join::handle_join_room_token(
                    &self.app,
                    &self.store,
                    p.0.session_token,
                    room_token,
                    p.0.member_name,
                )
                .await
                {
                    Ok(r) => {
                        let body = format!("room: \"{}\"\n", r.room_name);
                        CallToolResult::success(vec![Content::text(body)])
                    }
                    Err(e) => tool_error(e.to_string()),
                }
            }
            (None, None) => tool_error(
                Error::Config("exactly one of seat_token or room_token must be provided".into())
                    .to_string(),
            ),
        };
        Ok(result)
    }

    /// Send a message to another member in the same room.
    ///
    /// Fire-and-forget returns immediately and is strictly one-way — it never
    /// returns messages from the sender's own inbox (rubric 2.3); mail is read
    /// only via `receive`. Set await_reply_secs to block until the recipient
    /// replies or the timeout elapses.
    #[tool(
        description = "Send a message to another member in a room your session has joined. room_name is required — pass the room to send in, since your session may belong to more than one. Fire-and-forget (no await_reply_secs) returns immediately and is strictly one-way — it never returns messages from your own inbox; to read your mail, call receive. Set await_reply_secs to a large value (e.g. 3600) when genuinely awaiting a reply — this is the promise/await pattern; only a reply from THIS room's recipient satisfies the wait, even if your session has other rooms with traffic of their own. Only use a small await_reply_secs (e.g. 5) for an opportunistic poll where you are not actually expecting a reply. The response `timeout` field reports the reply wait: `none` = a reply was returned, `elapsed` = the wait expired with no reply, `capped` = an MCP-host time limit (X-Host-Cap-Secs) cut the wait short, `preempted` = a newer blocking call of yours (a receive, or another send-with-reply) took over your inbox's single receive slot and aborted this wait (empty result, nothing lost — do not run blocking calls concurrently). CRITICAL: on `capped` your message has ALREADY been delivered — do NOT call send again to keep waiting (that delivers a duplicate). Instead call receive to continue awaiting the reply. On `preempted` your message was also already delivered; just let your other call await the reply. INBOX HYGIENE: if you have done significant work since your last inbox check, first call receive with timeout_secs: 3 so this message accounts for anything that arrived in the meantime. A returned message that carries `reply_expected_within_secs` means its sender is awaiting your reply within that many seconds — reply promptly with a `send` back to its `from`."
    )]
    pub async fn send(&self, p: Parameters<SendParams>) -> Result<CallToolResult, rmcp::Error> {
        let host_cap_secs = host_cap::REQUEST_HOST_CAP.try_with(|v| *v).ok().flatten();
        let config = self.config.load_full();
        let await_reply_secs =
            p.0.await_reply_secs
                .map(|s| s.min(config.max_await_reply_secs));
        let result = match send::handle_send(
            &self.app,
            &config,
            &self.store,
            p.0.session_token,
            p.0.room_name,
            p.0.to,
            p.0.message,
            p.0.ttl_secs,
            await_reply_secs,
            host_cap_secs,
        )
        .await
        {
            Ok(r) => {
                let mut out = format!("message_id: {}\nttl_secs: {}\n", r.message_id, r.ttl_secs);
                if let Some(ls) = &r.last_seen {
                    out.push_str(&format!("last_seen: \"{ls}\"\n"));
                }
                if r.timeout != Timeout::None {
                    out.push_str(&format!("timeout: {}\n", r.timeout));
                }
                if !r.messages.is_empty() {
                    out.push_str(&tools::format_messages_yaml(&r.messages));
                }
                CallToolResult::success(vec![Content::text(out)])
            }
            Err(e) => tool_error(e.to_string()),
        };
        Ok(result)
    }

    /// Wait for incoming messages.
    ///
    /// Blocks until at least one message arrives or timeout_secs elapses.
    /// All queued messages are returned in a single batch in enqueue order.
    #[tool(
        description = "Wait for incoming messages. Blocks until messages arrive or timeout_secs elapses, then returns all queued messages in order. Omit `room` (the default) to wait across every room your session has joined — each returned message reports its `room` so you know where it came from. Pass `room` to wait for (and drain) only that one room's messages, leaving other rooms' messages queued for a later call. Use a large timeout (e.g. 3600) when genuinely awaiting a message — this is the promise/await pattern and avoids wasteful polling. Use a small timeout (e.g. 5) only when polling opportunistically. The response `timeout` field reports the wait: `none` = messages were returned, `elapsed` = the wait expired with no messages, `capped` = an MCP-host time limit (X-Host-Cap-Secs) cut the wait short. On `capped`, simply call receive again to keep waiting — re-polling is safe and never duplicates messages. IMPORTANT: never run two receives (or a receive and a send-with-reply) at the same time — one receive already drains every queued message in a single batch, so a second concurrent call is pointless AND preempts the first: the preempted call returns `timeout: preempted` with an empty batch. That is NOT an empty inbox (`elapsed`) and NOT a delivery (`none`) — nothing was lost, your other call now owns the wait; do not act on it as a result. A delivered message that carries `reply_expected_within_secs` means its sender is awaiting your reply within that many seconds — reply promptly with a `send` back to its `from`."
    )]
    pub async fn receive(
        &self,
        p: Parameters<ReceiveParams>,
    ) -> Result<CallToolResult, rmcp::Error> {
        let config = self.config.load();
        let timeout_secs =
            p.0.timeout_secs
                .unwrap_or(config.default_receive_timeout_secs)
                .min(config.max_receive_timeout_secs);
        let host_cap_secs = host_cap::REQUEST_HOST_CAP.try_with(|v| *v).ok().flatten();
        let result = match receive::handle_receive(
            &self.app,
            &self.store,
            p.0.session_token,
            timeout_secs,
            host_cap_secs,
            p.0.room,
        )
        .await
        {
            Ok(r) => {
                let mut out = format!("timeout: {}\n", r.timeout);
                if r.messages.is_empty() {
                    out.push_str("messages: []\n");
                } else {
                    out.push_str(&tools::format_messages_yaml(&r.messages));
                }
                CallToolResult::success(vec![Content::text(out)])
            }
            Err(e) => tool_error(e.to_string()),
        };
        Ok(result)
    }

    /// List the members of the room your session belongs to.
    ///
    /// You must name the room you are connected to; you cannot enumerate the
    /// members of a room you have not joined.
    #[tool(
        description = "List the member names currently connected to a room. You must pass the name of the room your own session belongs to — this tool only reveals the membership of a room you are connected to, never any other room. Returns the room name, the member count, and the member names in sorted order (including yourself)."
    )]
    pub async fn members(
        &self,
        p: Parameters<MembersParams>,
    ) -> Result<CallToolResult, rmcp::Error> {
        let result =
            match roster::handle_members(&self.app, &self.store, p.0.session_token, p.0.room_name)
                .await
            {
                Ok(r) => {
                    let mut out = format!(
                        "room: \"{}\"\nmember_count: {}\n",
                        r.room_name,
                        r.members.len()
                    );
                    if r.members.is_empty() {
                        out.push_str("members: []\n");
                    } else {
                        out.push_str("members:\n");
                        for m in &r.members {
                            out.push_str(&format!("  - {m}\n"));
                        }
                    }
                    CallToolResult::success(vec![Content::text(out)])
                }
                Err(e) => tool_error(e.to_string()),
            };
        Ok(result)
    }

    /// Leave a room — the inverse of `join`.
    ///
    /// Removes only this one room membership. If your session belongs to other
    /// rooms, they are unaffected. If this was your session's last remaining
    /// membership, the whole session (and its shared inbox) is torn down —
    /// resuming any room afterward requires a fresh `connect` and `join`.
    #[tool(
        description = "Leave a room — the inverse of `join`. Removes only your membership in the named room_name; you must name a room your session has joined. If your session belongs to other rooms, they and your shared inbox are completely unaffected. If this was your session's only remaining room membership, the whole session (and its shared inbox) is torn down — to use ToasterTalk again afterward, call `connect` and `join` fresh. Leaving does not delete or revoke a seat credential: a seat holder can `join` the same room again later with the same seat token. Returns the room name and member name that left."
    )]
    pub async fn leave(&self, p: Parameters<LeaveParams>) -> Result<CallToolResult, rmcp::Error> {
        let result =
            match leave::handle_leave(&self.app, &self.store, p.0.session_token, p.0.room_name)
                .await
            {
                Ok(r) => {
                    let body = format!(
                        "room: \"{}\"\nmember_name: \"{}\"\n",
                        r.room_name, r.member_name
                    );
                    CallToolResult::success(vec![Content::text(body)])
                }
                Err(e) => tool_error(e.to_string()),
            };
        Ok(result)
    }
}

// ── ServerHandler impl ────────────────────────────────────────────────────────

impl ServerHandler for ToasterServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: Default::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "toaster-talk-server".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            instructions: Some(
                "ToasterTalk MCP: authenticated inter-agent message passing, one session per \
                 agent shared across every room it joins. \
                 Step 1 — connect: call connect (no arguments needed on a first connect) to mint \
                 a session_token. It never takes a room/seat token or member name — it is \
                 identity only. Only pass old_session_token if you are deliberately rotating a \
                 still-live token you suspect leaked; that invalidates it and every room under it \
                 with no carryover, so you would need to join each room again afterward. \
                 Step 2 — join (once per room, including your very first): locate your token file \
                 in your home directory's `.toaster-talk/tokens` folder — `~/.toaster-talk/tokens` \
                 on Unix/macOS, `%USERPROFILE%\\.toaster-talk\\tokens` on Windows. Scan the \
                 `tokens/user/` subtree recursively (seat files live in per-room subdirectories, \
                 e.g. tokens/user/<room>/<member>.tok) for a 3-line seat token file (room name, \
                 member name, seat token) matching your room. If found, call join with \
                 session_token, seat_token, and member_name. Otherwise scan `tokens/` for a \
                 2-line room token file and call join with session_token, room_token, and your \
                 member_name. MemberNameTaken is fatal — do not retry with a different name. \
                 Joining a room your session already belongs to is rejected, not a no-op — do not \
                 re-join a room you already hold. Repeat join once per additional room you need. \
                 Step 3 — send and receive: both now take/report a room. send requires room_name \
                 (which room to send in); receive's room field is optional — omit it to drain \
                 messages from every room you've joined in one batch (each tagged with its room), \
                 or pass one room name to drain only that room, leaving the rest queued. \
                 INBOX HYGIENE — when you perform significant work between turns in a dialogue \
                 (after any send, whether fire-and-forget or with await_reply_secs), poll your \
                 inbox with a short receive (timeout_secs: 3) to catch messages that arrived \
                 while you were busy. Always do this before sending another message and before \
                 returning to idle, so you never act on a stale view or leave a peer waiting. \
                 IMPORTANT — when waiting for a reply, pass a long timeout_secs (e.g. 3600) so \
                 the server holds the connection open until a message arrives. \
                 receive and send (with await_reply_secs) always include a `timeout` field: \
                 `none` = messages delivered, `elapsed` = wait expired normally, \
                 `capped` = X-Host-Cap-Secs header shortened the wait, `preempted` = a newer \
                 blocking call of yours took over the inbox's single receive slot and aborted \
                 this one (empty result, nothing lost). On `capped`, always \
                 continue by calling receive — never call send again to resume a capped reply \
                 wait: your message was already delivered, and resending would deliver a duplicate. \
                 Never issue two blocking calls (receive and/or send-with-reply) concurrently: one \
                 receive drains the whole batch, and a second concurrent call only preempts the \
                 first (`preempted`) without gaining anything. Treat session_token as \
                 credential-grade: a leak now exposes every room your session belongs to, not \
                 just one — never log it in full or paste it insecurely."
                    .into(),
            ),
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParam,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, rmcp::Error>> + Send + '_ {
        // Extract the host cap from the HTTP request headers injected by rmcp into
        // the MCP request extensions. Setting the task-local here (rather than in
        // an axum middleware) ensures the value is visible in this same task where
        // the tool handler actually runs — tokio task-locals do not cross spawn
        // boundaries and rmcp spawns a separate task per session.
        //
        // If the header is present but not a valid u64, reject the request immediately
        // rather than silently falling back — a malformed cap is a misconfiguration.
        let cap_result: Result<Option<u64>, ()> = context
            .extensions
            .get::<Parts>()
            .map(|parts| parse_host_cap(&parts.headers))
            .unwrap_or(Ok(None));

        async move {
            let cap = match cap_result {
                Ok(v) => v,
                Err(()) => {
                    warn!("X-Host-Cap-Secs header present but not a valid positive integer — rejecting request");
                    return Ok(tool_error(
                        "X-Host-Cap-Secs header must be a positive integer (seconds); \
                         got an unparseable value — fix the MCP client configuration"
                            .to_string(),
                    ));
                }
            };
            let ctx = ToolCallContext::new(self, request, context);
            host_cap::REQUEST_HOST_CAP
                .scope(cap, self.tool_router.call(ctx))
                .await
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, rmcp::Error>> + Send + '_ {
        let tools = self.tool_router.list_all();
        std::future::ready(Ok(ListToolsResult {
            tools,
            next_cursor: None,
        }))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse the `X-Host-Cap-Secs` header from an HTTP `HeaderMap`.
/// - `Ok(None)` — header absent, no cap.
/// - `Ok(Some(v))` — valid cap value.
/// - `Err(())` — header present but not a valid positive integer (rubric 8.9).
fn parse_host_cap(headers: &axum::http::HeaderMap) -> Result<Option<u64>, ()> {
    match headers.get("x-host-cap-secs") {
        None => Ok(None),
        Some(hv) => hv
            .to_str()
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Some)
            .ok_or(()),
    }
}

/// Return a tool-level error (isError = true). This is a protocol-correct tool
/// response, not a JSON-RPC transport error.
fn tool_error(message: String) -> CallToolResult {
    CallToolResult {
        content: vec![Content::text(format!("error: {message}\n"))],
        is_error: Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rubric 8.9: a malformed X-Host-Cap-Secs header is rejected with a
    /// structured validation error; valid values and absent headers pass through.
    #[test]
    fn test_malformed_host_cap_header_rejected() {
        use axum::http::HeaderMap;

        let mut headers = HeaderMap::new();

        // Absent header → no cap
        assert_eq!(parse_host_cap(&headers), Ok(None));

        // Valid header → cap value
        headers.insert("x-host-cap-secs", "42".parse().unwrap());
        assert_eq!(parse_host_cap(&headers), Ok(Some(42)));

        // Malformed header → validation error
        headers.insert("x-host-cap-secs", "not-a-number".parse().unwrap());
        assert!(
            matches!(parse_host_cap(&headers), Err(())),
            "malformed X-Host-Cap-Secs must return Err"
        );

        // Zero is technically valid for u64 parse; negative values are not representable
        headers.insert("x-host-cap-secs", "0".parse().unwrap());
        assert_eq!(parse_host_cap(&headers), Ok(Some(0)));
    }

    /// Rubric 9.5: the send and receive tool descriptions must each include a brief
    /// instruction about the host-cap timeout and how it works. The behavioural
    /// guidance lives in the description (not the response body), so agents learn it
    /// up front from the tool schema.
    #[test]
    fn test_send_and_receive_descriptions_document_host_cap() {
        let tools = ToasterServer::tool_router().list_all();

        for name in ["send", "receive"] {
            let tool = tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} tool must be registered"));
            let desc = tool
                .description
                .as_deref()
                .unwrap_or_else(|| panic!("{name} tool must have a description"));

            assert!(
                desc.contains("X-Host-Cap-Secs"),
                "{name} description must name the host-cap header; got: {desc}"
            );
            assert!(
                desc.contains("capped"),
                "{name} description must explain the `capped` timeout state; got: {desc}"
            );
        }
    }

    /// Rubric 4.4: the send and receive tool descriptions must each tell a recipient
    /// what `reply_expected_within_secs` on a delivered message means, so an agent
    /// learns to reply promptly from the tool schema itself.
    #[test]
    fn test_send_and_receive_descriptions_document_reply_expected() {
        let tools = ToasterServer::tool_router().list_all();

        for name in ["send", "receive"] {
            let tool = tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} tool must be registered"));
            let desc = tool
                .description
                .as_deref()
                .unwrap_or_else(|| panic!("{name} tool must have a description"));

            assert!(
                desc.contains("reply_expected_within_secs"),
                "{name} description must document the reply_expected_within_secs field; got: {desc}"
            );
        }
    }

    /// Rubric 9.7: an unrecognized field on a tool call is rejected outright
    /// rather than silently dropped. Regression guard for the stuck-connect
    /// incident (ANTI-PATTERNS #16): a client calling `connect` with the
    /// pre-split `member_name`/`room_token` arguments got a normal,
    /// successful roomless session back — the unknown fields were
    /// deserialized away with no signal that the call didn't do what the
    /// caller intended.
    #[test]
    fn test_unknown_field_rejected_on_every_tool_params_struct() {
        let bad_connect = serde_json::json!({
            "member_name": "farming",
            "room_token": "some-token"
        });
        assert!(
            serde_json::from_value::<ConnectParams>(bad_connect).is_err(),
            "connect must reject the pre-split member_name/room_token arguments"
        );

        let bad_join = serde_json::json!({
            "session_token": "s",
            "room_token": "r",
            "member_name": "m",
            "typo_field": "x"
        });
        assert!(serde_json::from_value::<JoinParams>(bad_join).is_err());

        let bad_send = serde_json::json!({
            "session_token": "s",
            "room_name": "r",
            "to": "bob",
            "message": "hi",
            "reply_to": "oops"
        });
        assert!(serde_json::from_value::<SendParams>(bad_send).is_err());

        let bad_receive = serde_json::json!({
            "session_token": "s",
            "rooms": ["a", "b"]
        });
        assert!(serde_json::from_value::<ReceiveParams>(bad_receive).is_err());

        let bad_members = serde_json::json!({
            "session_token": "s",
            "room_name": "r",
            "room_token": "extra"
        });
        assert!(serde_json::from_value::<MembersParams>(bad_members).is_err());

        let bad_leave = serde_json::json!({
            "session_token": "s",
            "room_name": "r",
            "member_name": "extra"
        });
        assert!(serde_json::from_value::<LeaveParams>(bad_leave).is_err());

        // Sanity: the legitimate shapes still deserialize fine.
        assert!(serde_json::from_value::<ConnectParams>(serde_json::json!({})).is_ok());
        assert!(serde_json::from_value::<JoinParams>(serde_json::json!({
            "session_token": "s",
            "room_token": "r",
            "member_name": "m"
        }))
        .is_ok());
        assert!(serde_json::from_value::<LeaveParams>(serde_json::json!({
            "session_token": "s",
            "room_name": "r"
        }))
        .is_ok());
    }
}
