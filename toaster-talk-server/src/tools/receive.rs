use crate::{
    error::Error,
    host_cap,
    rooms::{RoomStore, RoomToken},
    state::{Inbox, QueuedMessage, SharedAppState},
    tools::{session_id, InboxMessage, Timeout},
};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// RAII guard that releases the receive slot (and aborts the sub-task) when dropped.
///
/// Handles two distinct cancellation scenarios:
/// - **Outer-task cancellation** (`task.abort()` or MCP host terminating the call):
///   the `handle_receive` future is cancelled; `drop` aborts our own sub-task and
///   releases the slot.
/// - **Preemption by a new receive**: the new caller aborts the sub-task via
///   `inbox.abort_existing_receive()` and claims a new generation before this guard
///   drops. When `drop` fires, `release_receive(gen)` is a CAS no-op because the
///   active generation no longer matches — the new receive's slot is preserved.
struct ReceiveGuard {
    inbox: Arc<Inbox>,
    generation: u64,
    /// Abort handle for our own inbox-wait sub-task. Set after the sub-task is
    /// spawned; `None` only during the brief window between guard creation and spawn.
    abort: Option<tokio::task::AbortHandle>,
}

impl ReceiveGuard {
    fn new(inbox: Arc<Inbox>, generation: u64) -> Self {
        ReceiveGuard {
            inbox,
            generation,
            abort: None,
        }
    }

    fn set_abort(&mut self, handle: tokio::task::AbortHandle) {
        self.abort = Some(handle);
    }
}

impl Drop for ReceiveGuard {
    fn drop(&mut self) {
        if let Some(h) = self.abort.take() {
            h.abort();
        }
        self.inbox.release_receive(self.generation);
    }
}

/// Outcome of parking on an inbox via [`park_on_inbox`].
pub(crate) struct ParkOutcome {
    pub messages: Vec<QueuedMessage>,
    /// `true` if this waiter was preempted (aborted) by a newer one — distinct from
    /// a genuine timeout, so callers can report `Timeout::None` rather than `Elapsed`.
    pub preempted: bool,
}

/// Park on `inbox` until a message matching `filter` arrives or `deadline`
/// elapses, using the inbox's single preemptible receive slot.
///
/// This is the **one** place any caller is allowed to block-and-drain an inbox.
/// Routing both `receive` and `send`-with-reply through it guarantees there is at
/// most one *live* waiter per inbox at any time: before claiming the slot it aborts
/// any prior waiter — including one orphaned by an MCP-host/client disconnect that
/// the framework left alive (ANTI-PATTERNS #5). That invariant is what prevents a
/// zombie waiter from winning a `notify_one()` wakeup and draining (stealing) a
/// message that belongs to the live waiter — the deadlock observed in the `surfing`
/// room. The slot is released and the sub-task aborted on drop (cancellation, return,
/// or panic) via [`ReceiveGuard`].
///
/// `filter` is the room-scoping predicate: `receive`'s optional room filter and
/// send-with-reply's mandatory one are both just predicates passed here — this is
/// *not* a new preemption model: still one inbox, one slot, one live waiter, same
/// chokepoint. Pass `|_| true` for "any room."
pub(crate) async fn park_on_inbox(
    inbox: Arc<Inbox>,
    deadline: Instant,
    member_name: &str,
    filter: impl Fn(&RoomToken) -> bool + Send + Sync + 'static,
) -> Result<ParkOutcome, Error> {
    if inbox.is_receive_active() {
        warn!(member = %member_name, "inbox wait slot active — preempting prior (possibly orphaned) waiter");
    }
    inbox.abort_existing_receive();
    let generation = inbox.claim_receive();

    // Guard releases the slot and aborts the sub-task on drop — handles outer-task
    // cancellation, normal completion, and preemption (where release_receive is a
    // CAS no-op because a newer generation is already installed).
    let mut guard = ReceiveGuard::new(inbox.clone(), generation);

    // Run the actual wait in a sub-task so that dropping (cancelling) this future
    // does not silently detach it — the guard explicitly aborts it via the handle.
    let subtask = tokio::spawn({
        let inbox = inbox.clone();
        async move { inbox.wait_drain_until_filtered(deadline, filter).await }
    });
    inbox.set_receive_abort(subtask.abort_handle());
    guard.set_abort(subtask.abort_handle());

    match subtask.await {
        Ok(messages) => Ok(ParkOutcome {
            messages,
            preempted: false,
        }),
        Err(e) if e.is_cancelled() => Ok(ParkOutcome {
            messages: vec![],
            preempted: true,
        }),
        Err(e) => Err(Error::Internal(format!("inbox wait task panicked: {e}"))),
    }
}

#[derive(Debug)]
pub struct ReceiveResult {
    pub messages: Vec<InboxMessage>,
    pub timeout: Timeout,
}

/// Block until messages arrive in the member's shared inbox, or the timeout
/// elapses.
///
/// `room` is optional (TDD 3.8, 3.9, 3.10):
/// - Omitted (default): blocks until a message from *any* of the session's rooms
///   arrives, and drains everything currently valid across all of them. Each
///   returned message carries its `room`.
/// - Supplied: blocks (and drains) only messages tagged with that specific room,
///   leaving messages from the session's other rooms queued for a later call.
///
/// If a receive is already active when this call arrives, the stale connection is
/// preempted: its sub-task is aborted and this call claims the slot. This handles
/// the case where the MCP host or network dropped the previous connection without
/// cancelling the server-side handler future (ISS-006).
pub async fn handle_receive(
    app: &SharedAppState,
    store: &Arc<RwLock<RoomStore>>,
    session_token: String,
    timeout_secs: u64,
    host_cap_secs: Option<u64>,
    room: Option<String>,
) -> Result<ReceiveResult, Error> {
    // Resolve the optional room filter to a token up front (needs the store, so
    // done before taking the AppState lock — consistent with connect/join/send).
    let filter_room_token: Option<RoomToken> = match &room {
        Some(room_name) => {
            let store_read = store.read().await;
            let token = store_read
                .get_by_name(room_name)
                .map(|r| r.token.clone())
                .ok_or_else(|| Error::RoomNotFound(room_name.clone()))?;
            Some(token)
        }
        None => None,
    };

    // Resolve session and host cap under a single brief write lock. Slot claiming
    // and parking happen after the lock is released, inside park_on_inbox.
    let (inbox, member_label, effective_timeout_secs, was_capped) = {
        let mut app_write = app.write().await;

        app_write.check_session(&session_token)?;

        let memberships = app_write
            .session_memberships(&session_token)
            .ok_or(Error::InvalidSessionToken)?
            .clone();
        if memberships.is_empty() {
            return Err(Error::NotJoinedAnyRoom);
        }

        if let Some(target) = &filter_room_token {
            if !memberships.iter().any(|(rt, _)| rt == target) {
                return Err(Error::NotConnectedToRoom(room.clone().unwrap_or_default()));
            }
            // A room-specific receive touches only that one room's activity clock
            // at the start of the call, same granularity as `send`.
            app_write.touch_membership(&session_token, target.as_str());
        }

        let inbox = app_write
            .session_inbox(&session_token)
            .ok_or(Error::InvalidSessionToken)?;

        // Apply the host cap from the X-Host-Cap-Secs request header (rubrics 8.6, 8.7).
        // Hosts without the header are unaffected.
        let (effective, was_capped) = match host_cap_secs {
            Some(cap) if cap < timeout_secs => (cap, true),
            _ => (timeout_secs, false),
        };

        let member_label = room.clone().unwrap_or_else(|| "*".to_string());
        (inbox, member_label, effective, was_capped)
    }; // write lock released

    let deadline = Instant::now() + Duration::from_secs(effective_timeout_secs);

    debug!(session = %session_id(&session_token), room = %member_label, effective_timeout_secs, was_capped, "receive waiting for messages");

    // Single preemptible-slot park — preempts any stale/orphaned waiter so a zombie
    // cannot steal this receive's wakeup. The filter is the room-scoping predicate:
    // "any room" when `room` was omitted, or an exact-token match when it wasn't.
    let wait_filter = filter_room_token.clone();
    let outcome =
        park_on_inbox(
            inbox,
            deadline,
            &member_label,
            move |rt: &RoomToken| match &wait_filter {
                Some(target) => rt == target,
                None => true,
            },
        )
        .await?;

    // Re-anchor the idle window at *completion*. A room-omitted receive parks on
    // the whole shared inbox, blanket-exempting every one of the session's rooms
    // from the idle sweep for the duration (`is_receive_active` is one atomic
    // shared by the whole session) — so every room's clock must be re-anchored
    // together, not just the one(s) that happened to deliver a message (see
    // `AppState::touch_all_memberships`). A room-specific receive only re-anchors
    // that one room, matching its narrower activity scope. No-op if the member was
    // evicted during the wait.
    {
        let mut app_write = app.write().await;
        match &filter_room_token {
            Some(target) => app_write.touch_membership(&session_token, target.as_str()),
            None => app_write.touch_all_memberships(&session_token),
        }
    }

    if !outcome.messages.is_empty() {
        let room_names = resolve_room_names(store, &outcome.messages).await;
        let delivered: Vec<InboxMessage> = outcome
            .messages
            .into_iter()
            .map(|m| {
                let room_name = room_names
                    .get(&m.room)
                    .cloned()
                    .unwrap_or_else(|| "<unknown-room>".to_string());
                let msg = InboxMessage::from_queued(m, room_name);
                debug!(
                    message_id = %msg.id,
                    from = %msg.from,
                    room = %msg.room,
                    age = %msg.age,
                    size_bytes = msg.body.len(),
                    "message delivered to receiver"
                );
                msg
            })
            .collect();

        info!(
            session = %session_id(&session_token),
            message_count = delivered.len(),
            "receive completed with messages"
        );

        return Ok(ReceiveResult {
            messages: delivered,
            timeout: host_cap::apply_timeout(was_capped, false),
        });
    }

    // No messages. A preemption is neither a delivery nor a genuine timeout, so it
    // gets its own status: reporting `None` here would claim messages were returned
    // (SCHEMA: `none` = messages returned) when the batch is empty, and reporting
    // `Elapsed` would claim the requested wait ran to completion when it did not.
    // Preemption outranks the host cap — the caller's own newer call ended this
    // wait, which is what they need to know regardless of any cap.
    let timeout = if outcome.preempted {
        Timeout::Preempted
    } else {
        host_cap::apply_timeout(was_capped, true)
    };
    if outcome.preempted {
        debug!(session = %session_id(&session_token), "receive sub-task cancelled — preempted by new receive");
    } else if timeout == Timeout::Capped {
        debug!(session = %session_id(&session_token), effective_timeout_secs, "receive capped — host cap fired with no messages");
    } else {
        debug!(session = %session_id(&session_token), effective_timeout_secs, "receive timed out — no messages");
    }
    Ok(ReceiveResult {
        messages: vec![],
        timeout,
    })
}

/// Resolve each delivered message's room token to a display name in one batch
/// (one store read lock), rather than one lookup per message.
async fn resolve_room_names(
    store: &Arc<RwLock<RoomStore>>,
    messages: &[QueuedMessage],
) -> HashMap<RoomToken, String> {
    let store_read = store.read().await;
    messages
        .iter()
        .map(|m| m.room.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .filter_map(|rt| {
            store_read
                .verify_token(rt.as_str())
                .map(|r| (rt.clone(), r.name.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        rooms::RoomStore,
        state::{AppState, QueuedMessage},
        test_support::TEST_MASTER_SECRET,
        tools::{connect::handle_connect, join::handle_join_room_token, Timeout},
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use tempfile::TempDir;
    use tokio::sync::RwLock;
    use uuid::Uuid;

    fn large_inbox() -> usize {
        usize::MAX
    }

    async fn setup(
        dir: &TempDir,
    ) -> (crate::state::SharedAppState, Arc<RwLock<RoomStore>>, String) {
        let store_path = dir.path().join("rooms.yaml");
        let mut store = RoomStore::load(store_path).unwrap();
        let room = store
            .create("testroom".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let store = Arc::new(RwLock::new(store));
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_room_token(
            &app,
            &store,
            session.clone(),
            room.token.to_string(),
            "alice".into(),
        )
        .await
        .unwrap();
        (app, store, session)
    }

    async fn push_to(
        app: &crate::state::SharedAppState,
        member: &str,
        from: &str,
        room: &str,
        body: &str,
    ) {
        let state = app.read().await;
        for room_state in state.rooms.values() {
            if let Some(m) = room_state.members.get(member) {
                let now = Instant::now();
                m.inbox
                    .try_push(
                        QueuedMessage {
                            id: Uuid::new_v4(),
                            from: from.into(),
                            room: RoomToken::from(room.to_string()),
                            body: body.into(),
                            enqueued_at: now,
                            expires_at: now + Duration::from_secs(300),
                            reply_deadline: None,
                        },
                        large_inbox(),
                    )
                    .await;
                return;
            }
        }
        panic!("member {member} not found in AppState");
    }

    /// Fetch the room token registered for `room_name` in the test store.
    async fn room_token_for(store: &Arc<RwLock<RoomStore>>, room_name: &str) -> String {
        store
            .read()
            .await
            .get_by_name(room_name)
            .unwrap()
            .token
            .to_string()
    }

    #[tokio::test]
    async fn test_receive_delivers_waiting_message() {
        // Rubric 3.1: a queued message is delivered when the member calls receive.
        let dir = TempDir::new().unwrap();
        let (app, store, session) = setup(&dir).await;
        let room_token = room_token_for(&store, "testroom").await;
        push_to(&app, "alice", "bob", &room_token, "hello").await;

        let result = handle_receive(&app, &store, session, 1, None, None)
            .await
            .unwrap();
        assert_eq!(result.timeout, Timeout::None);
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].from, "bob");
        assert_eq!(result.messages[0].room, "testroom");
        assert_eq!(result.messages[0].body, "hello");
    }

    #[tokio::test]
    async fn test_receive_times_out_when_no_messages() {
        // Rubric 3.2: when no messages arrive within the timeout, the tool returns
        // an empty result with the timed_out flag set.
        let dir = TempDir::new().unwrap();
        let (app, store, session) = setup(&dir).await;

        let result = handle_receive(&app, &store, session, 0, None, None)
            .await
            .unwrap();
        assert_eq!(result.timeout, Timeout::Elapsed);
        assert!(result.messages.is_empty());
    }

    #[tokio::test]
    async fn test_receive_uses_configured_default_when_no_timeout_given() {
        // Rubric 3.4: when timeout_secs is omitted by the caller, the server
        // resolves it to config.default_receive_timeout_secs before calling
        // handle_receive. This test simulates that resolution (None → default)
        // and verifies handle_receive behaves correctly with the resolved value.
        let dir = TempDir::new().unwrap();
        let (app, store, session) = setup(&dir).await;

        let resolved = 0u64;

        let result = handle_receive(&app, &store, session, resolved, None, None)
            .await
            .unwrap();
        assert_eq!(result.timeout, Timeout::Elapsed);
        assert!(result.messages.is_empty());
    }

    #[tokio::test]
    async fn test_receive_returns_multiple_messages_in_enqueue_order() {
        // Rubric 3.3: all queued messages are returned in a single batch in enqueue order.
        let dir = TempDir::new().unwrap();
        let (app, store, session) = setup(&dir).await;
        let room_token = room_token_for(&store, "testroom").await;
        push_to(&app, "alice", "bob", &room_token, "first").await;
        push_to(&app, "alice", "charlie", &room_token, "second").await;
        push_to(&app, "alice", "dave", &room_token, "third").await;

        let result = handle_receive(&app, &store, session, 1, None, None)
            .await
            .unwrap();
        assert_eq!(result.timeout, Timeout::None);
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.messages[0].body, "first");
        assert_eq!(result.messages[1].body, "second");
        assert_eq!(result.messages[2].body, "third");
    }

    #[tokio::test]
    async fn test_receive_preempts_stale_connection() {
        // Rubric 3.5 (preemption path): when the receive slot is already claimed —
        // simulating a stale connection whose handler was not cancelled — a new receive
        // call must preempt and succeed rather than returning an error.
        let dir = TempDir::new().unwrap();
        let (app, store, session) = setup(&dir).await;

        // Pre-claim the slot as if a prior receive is stuck (no live sub-task).
        {
            let state = app.read().await;
            for room_state in state.rooms.values() {
                if let Some(m) = room_state.members.get("alice") {
                    m.inbox.claim_receive();
                    break;
                }
            }
        }

        // New receive must preempt the stale slot and succeed.
        let result = handle_receive(&app, &store, session, 0, None, None).await;
        assert!(
            result.is_ok(),
            "receive must preempt stale slot, not return an error: {result:?}"
        );
        assert_eq!(result.unwrap().timeout, Timeout::Elapsed);
    }

    #[tokio::test]
    async fn test_receive_slot_released_on_future_cancellation() {
        // Rubric 3.5: if a receive call is cancelled (future dropped), the receive
        // slot must be released so the next receive call can proceed.
        let dir = TempDir::new().unwrap();
        let (app, store, session) = setup(&dir).await;

        // Spawn a blocking receive and immediately abort it, simulating an HTTP
        // connection drop / CancelledNotification from the MCP client.
        let app_clone = app.clone();
        let store_clone = store.clone();
        let session_clone = session.clone();
        let task = tokio::spawn(async move {
            handle_receive(&app_clone, &store_clone, session_clone, 60, None, None).await
        });
        // Give the task a moment to claim the receive slot before aborting.
        tokio::time::sleep(Duration::from_millis(10)).await;
        task.abort();
        let _ = task.await; // wait for abort to complete

        // The receive slot must have been released by ReceiveGuard::drop().
        // A second receive call must succeed.
        let result = handle_receive(&app, &store, session, 0, None, None).await;
        assert!(
            result.is_ok(),
            "receive slot must be free after cancellation: {result:?}"
        );
        assert_eq!(result.unwrap().timeout, Timeout::Elapsed);
    }

    #[tokio::test]
    async fn test_live_receive_preempted_reports_preempted_status() {
        // A concurrent-receive footgun: when a second blocking receive preempts a
        // first that is still parked, the first returns empty. It must report
        // `Timeout::Preempted` — NOT `None` (which the schema defines as "messages
        // returned") and NOT `Elapsed` (which claims the full wait ran). This is the
        // signal an agent needs to notice it issued redundant concurrent receives.
        let dir = TempDir::new().unwrap();
        let (app, store, session) = setup(&dir).await;

        // First receive parks on a long deadline, claiming the live slot.
        let app_clone = app.clone();
        let store_clone = store.clone();
        let session_clone = session.clone();
        let first = tokio::spawn(async move {
            handle_receive(&app_clone, &store_clone, session_clone, 3600, None, None).await
        });
        tokio::time::sleep(Duration::from_millis(20)).await; // let it claim + park

        // Second receive preempts the first, then elapses empty itself.
        let second = handle_receive(&app, &store, session, 0, None, None)
            .await
            .unwrap();
        assert_eq!(second.timeout, Timeout::Elapsed);

        let first = first.await.unwrap().unwrap();
        assert!(first.messages.is_empty());
        assert_eq!(
            first.timeout,
            Timeout::Preempted,
            "a live receive cut short by a newer one must report Preempted, not None/Elapsed"
        );
    }

    #[tokio::test]
    async fn test_receive_returns_capped_when_host_cap_fires() {
        // Rubric 8.6: when host_cap_secs < timeout_secs and no messages arrive,
        // the result must be Timeout::Capped (not Timeout::Elapsed).
        let dir = TempDir::new().unwrap();
        let (app, store, session) = setup(&dir).await;

        // cap=0 < timeout=60: capped wait expires immediately
        let result = handle_receive(&app, &store, session, 60, Some(0), None)
            .await
            .unwrap();
        assert_eq!(result.timeout, Timeout::Capped);
        assert!(result.messages.is_empty());
    }

    #[tokio::test]
    async fn test_receive_not_capped_when_message_arrives_before_cap() {
        // Rubric 8.6 (guard): if a message arrives before the cap fires, the result
        // must be Timeout::None — not Timeout::Capped — even when host_cap_secs is set.
        let dir = TempDir::new().unwrap();
        let (app, store, session) = setup(&dir).await;
        let room_token = room_token_for(&store, "testroom").await;
        push_to(&app, "alice", "bob", &room_token, "hi").await;

        // cap=60, timeout=3600: message is already queued so it's delivered immediately
        let result = handle_receive(&app, &store, session, 3600, Some(60), None)
            .await
            .unwrap();
        assert_eq!(result.timeout, Timeout::None);
        assert_eq!(result.messages.len(), 1);
    }

    #[tokio::test]
    async fn test_receive_room_filter_returns_only_that_room_leaves_rest_queued() {
        // Rubric 3.8: specifying a room returns only messages from that room,
        // leaving other rooms' messages queued in the shared inbox.
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("rooms.yaml");
        let mut store = RoomStore::load(store_path).unwrap();
        let room_a = store
            .create("room-a".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let room_b = store
            .create("room-b".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let store = Arc::new(RwLock::new(store));
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_room_token(
            &app,
            &store,
            session.clone(),
            room_a.token.to_string(),
            "alice-a".into(),
        )
        .await
        .unwrap();
        handle_join_room_token(
            &app,
            &store,
            session.clone(),
            room_b.token.to_string(),
            "alice-b".into(),
        )
        .await
        .unwrap();

        push_to(&app, "alice-a", "bob", room_a.token.as_str(), "from-a").await;
        push_to(&app, "alice-b", "carol", room_b.token.as_str(), "from-b").await;

        let result = handle_receive(
            &app,
            &store,
            session.clone(),
            1,
            None,
            Some("room-a".into()),
        )
        .await
        .unwrap();
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].room, "room-a");
        assert_eq!(result.messages[0].body, "from-a");

        // room-b's message is still queued for a later call.
        let result_b = handle_receive(&app, &store, session, 1, None, Some("room-b".into()))
            .await
            .unwrap();
        assert_eq!(result_b.messages.len(), 1);
        assert_eq!(result_b.messages[0].room, "room-b");
        assert_eq!(result_b.messages[0].body, "from-b");
    }

    #[tokio::test]
    async fn test_receive_no_room_filter_returns_all_rooms_tagged() {
        // Rubric 3.9/3.10: omitting the room filter returns messages from every
        // joined room in one batch, each tagged with its originating room.
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("rooms.yaml");
        let mut store = RoomStore::load(store_path).unwrap();
        let room_a = store
            .create("room-a".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let room_b = store
            .create("room-b".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let store = Arc::new(RwLock::new(store));
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_room_token(
            &app,
            &store,
            session.clone(),
            room_a.token.to_string(),
            "alice-a".into(),
        )
        .await
        .unwrap();
        handle_join_room_token(
            &app,
            &store,
            session.clone(),
            room_b.token.to_string(),
            "alice-b".into(),
        )
        .await
        .unwrap();

        push_to(&app, "alice-a", "bob", room_a.token.as_str(), "from-a").await;
        push_to(&app, "alice-b", "carol", room_b.token.as_str(), "from-b").await;

        let result = handle_receive(&app, &store, session, 1, None, None)
            .await
            .unwrap();
        assert_eq!(result.messages.len(), 2);
        let rooms: std::collections::HashSet<&str> =
            result.messages.iter().map(|m| m.room.as_str()).collect();
        assert!(rooms.contains("room-a"));
        assert!(rooms.contains("room-b"));
    }

    #[tokio::test]
    async fn test_receive_room_filter_unknown_room_name_is_room_not_found() {
        let dir = TempDir::new().unwrap();
        let (app, store, session) = setup(&dir).await;
        let result =
            handle_receive(&app, &store, session, 0, None, Some("no-such-room".into())).await;
        assert!(matches!(result.unwrap_err(), Error::RoomNotFound(_)));
    }

    #[tokio::test]
    async fn test_receive_room_filter_not_a_member_is_not_connected() {
        let dir = TempDir::new().unwrap();
        let store_path = dir.path().join("rooms.yaml");
        let mut store = RoomStore::load(store_path).unwrap();
        store
            .create("room-a".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        store
            .create("room-b".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let store = Arc::new(RwLock::new(store));
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;
        let room_a_token = room_token_for(&store, "room-a").await;
        handle_join_room_token(&app, &store, session.clone(), room_a_token, "alice".into())
            .await
            .unwrap();

        let result = handle_receive(&app, &store, session, 0, None, Some("room-b".into())).await;
        assert!(matches!(result.unwrap_err(), Error::NotConnectedToRoom(_)));
    }

    #[tokio::test]
    async fn test_receive_with_no_rooms_joined_is_not_joined_any_room() {
        // Rubric 1b.2: using receive before joining any room is a clear error.
        let dir = TempDir::new().unwrap();
        let store = Arc::new(RwLock::new(
            RoomStore::load(dir.path().join("rooms.yaml")).unwrap(),
        ));
        let app = AppState::new();
        let session = handle_connect(&app, None).await.unwrap().session_token;

        let result = handle_receive(&app, &store, session, 0, None, None).await;
        assert!(matches!(result.unwrap_err(), Error::NotJoinedAnyRoom));
    }

    #[tokio::test]
    async fn test_park_preempts_orphaned_waiter_so_message_is_not_stolen() {
        // Root cause of the `surfing` deadlock: an orphaned waiter (a future the
        // framework left alive after a client disconnect) parked on the inbox could
        // win a notify_one() wakeup and drain — steal — the message meant for the
        // live waiter. park_on_inbox enforces a single live waiter per inbox: the
        // arriving waiter preempts the orphan, so the message reaches the live one.
        let inbox = Inbox::new();

        // Orphaned waiter A claims the slot and parks on a long deadline.
        let a = tokio::spawn({
            let inbox = inbox.clone();
            async move {
                park_on_inbox(
                    inbox,
                    Instant::now() + Duration::from_secs(30),
                    "alice",
                    |_| true,
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await; // let A claim + park

        // Live waiter B arrives — it must preempt A and take over the slot.
        let b = tokio::spawn({
            let inbox = inbox.clone();
            async move {
                park_on_inbox(
                    inbox,
                    Instant::now() + Duration::from_secs(30),
                    "alice",
                    |_| true,
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await; // let B preempt + park

        // Exactly one message arrives.
        let now = Instant::now();
        inbox
            .try_push(
                QueuedMessage {
                    id: Uuid::new_v4(),
                    from: "bob".into(),
                    room: RoomToken::from("rt".to_string()),
                    body: "reply".into(),
                    enqueued_at: now,
                    expires_at: now + Duration::from_secs(60),
                    reply_deadline: None,
                },
                usize::MAX,
            )
            .await;

        let b_out = b.await.unwrap().unwrap();
        let a_out = a.await.unwrap().unwrap();

        assert_eq!(
            b_out.messages.len(),
            1,
            "the live waiter must receive the message"
        );
        assert_eq!(b_out.messages[0].body, "reply");
        assert!(
            a_out.messages.is_empty(),
            "the orphaned waiter must NOT steal the message"
        );
        assert!(
            a_out.preempted,
            "the orphaned waiter must have been preempted"
        );
    }
}
