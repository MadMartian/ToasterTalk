use crate::{
    config::Config,
    error::Error,
    host_cap,
    rooms::{RoomStore, RoomToken},
    state::{Inbox, PushOutcome, QueuedMessage, ReplyWaitGuard, SharedAppState},
    tools::{format_last_seen, receive::park_on_inbox, session_id, InboxMessage, Timeout},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

#[derive(Debug)]
pub struct SendResult {
    pub message_id: Uuid,
    pub ttl_secs: u64,
    /// Set if the recipient has not been seen within the idle threshold.
    pub last_seen: Option<String>,
    /// The reply(ies) delivered to a send-with-reply during its wait. Always empty
    /// for fire-and-forget — that path is strictly one-way (rubric 2.3).
    pub messages: Vec<InboxMessage>,
    /// Timeout status for the `await_reply_secs` wait, if one was requested.
    /// `None` when no `await_reply_secs` was supplied or a reply arrived.
    pub timeout: Timeout,
}

/// Send a message to a named member in the same room.
///
/// `room_name` is required (TDD 2.4, 2.5, 2.6) — recipient `to` alone no longer
/// disambiguates which room's namespace to search, since one session's shared
/// inbox can span several rooms.
///
/// Two modes:
/// - Fire-and-forget: enqueues and returns immediately. Strictly one-way — it never
///   returns messages from the sender's own inbox (rubric 2.3); mail arrives only via
///   `receive`.
/// - Send-with-reply: blocks on the sender's own inbox until a reply *from this
///   same room* arrives or `await_reply_secs` elapses, returning any reply in the
///   response. The room-filtered wait is mandatory here — a reply from a different
///   room the sender also belongs to does not satisfy it and stays queued.
#[allow(clippy::too_many_arguments)]
pub async fn handle_send(
    app: &SharedAppState,
    config: &Config,
    store: &Arc<RwLock<RoomStore>>,
    session_token: String,
    room_name: String,
    to: String,
    message: String,
    ttl_secs: Option<u64>,
    await_reply_secs: Option<u64>,
    host_cap_secs: Option<u64>,
) -> Result<SendResult, Error> {
    // Validate message size
    const MAX_BODY_BYTES: usize = 100 * 1024; // 100 KB — matches POLICY.md and rubric 9.2
    let body_size = message.len();
    if body_size > MAX_BODY_BYTES {
        return Err(Error::MessageTooLarge {
            size: body_size,
            max: MAX_BODY_BYTES,
        });
    }

    // Resolve and validate TTL
    let ttl_secs = ttl_secs.unwrap_or(config.default_message_ttl_secs);
    if ttl_secs > config.max_message_ttl_secs {
        return Err(Error::MessageTtlExceeded {
            given_secs: ttl_secs,
            max_secs: config.max_message_ttl_secs,
        });
    }

    // When the caller is waiting for a reply, extend the message lifetime so it
    // cannot expire before the reply window closes. effective = await + declared.
    let effective_ttl_secs = match await_reply_secs {
        Some(await_secs) => await_secs.saturating_add(ttl_secs),
        None => ttl_secs,
    };

    let now = Instant::now();
    let expires_at = now + Duration::from_secs(effective_ttl_secs);
    let message_id = Uuid::new_v4();

    // Resolve the room name to its token (rubric 2.6: an unknown room name is a
    // descriptive RoomNotFound, not a member-lookup failure).
    let room_token: RoomToken = {
        let store_read = store.read().await;
        store_read
            .get_by_name(&room_name)
            .map(|r| r.token.clone())
            .ok_or_else(|| Error::RoomNotFound(room_name.clone()))?
    };

    // Resolve sender + recipient under a brief write lock; release before async I/O.
    let (sender_name, recipient_inbox, sender_inbox, last_seen_str) = {
        let mut app_write = app.write().await;

        let (sender_name, room_state) =
            app_write.room_membership_mut(&session_token, room_token.as_str(), &room_name)?;

        // Verify recipient is in the same room (rubric 2.5).
        let recipient = room_state
            .members
            .get(&to)
            .ok_or_else(|| Error::MemberNotFound(to.clone()))?;

        let recipient_inbox = recipient.inbox.clone();

        // Check recipient last_seen for idle warning
        let idle_threshold = Duration::from_secs(config.idle_threshold_secs);
        let last_seen_str = if recipient.last_seen.elapsed() > idle_threshold {
            let s = format_last_seen(recipient.last_seen);
            warn!(
                session = %session_id(&session_token),
                sender = %sender_name,
                recipient = %to,
                last_seen = %s,
                "recipient may be offline — last_seen exceeds idle threshold"
            );
            Some(s)
        } else {
            None
        };

        // Update sender's last_seen (this room membership only) and grab their inbox
        let sender = room_state
            .members
            .get_mut(&sender_name)
            .ok_or_else(|| Error::MemberNotFound(sender_name.clone()))?;
        sender.last_seen = now;
        let sender_inbox = sender.inbox.clone();

        (sender_name, recipient_inbox, sender_inbox, last_seen_str)
    }; // write lock released

    // Enqueue the message — enforce per-inbox size cap (SEC-2) and fail-closed if
    // the recipient was evicted in the window after the lock was released. A
    // `Closed` outcome means the recipient is gone: report it truthfully rather
    // than returning a message_id for a message that was never delivered.
    // Stamp the reply-window deadline for the recipient's `reply_expected_within_secs`
    // hint. Uses the caller's declared `await_reply_secs` (already clamped to
    // `max_await_reply_secs` in `server.rs::send`) — deliberately NOT the host-capped
    // value, since a capped sender re-polls via `receive` and keeps waiting the full
    // declared window.
    let reply_deadline = await_reply_secs.map(|s| now + Duration::from_secs(s));

    match recipient_inbox
        .try_push(
            QueuedMessage {
                id: message_id,
                from: sender_name.clone(),
                room: room_token.clone(),
                body: message,
                enqueued_at: now,
                expires_at,
                reply_deadline,
            },
            config.max_inbox_messages,
        )
        .await
    {
        PushOutcome::Pushed => {}
        PushOutcome::Full => return Err(Error::InboxFull(to)),
        PushOutcome::Closed => {
            warn!(
                session = %session_id(&session_token),
                sender = %sender_name,
                recipient = %to,
                "send aborted — recipient was evicted before the message could be delivered"
            );
            return Err(Error::RecipientEvicted(to));
        }
    }

    debug!(
        session = %session_id(&session_token),
        sender = %sender_name,
        room = %room_name,
        to = %to,
        message_id = %message_id,
        ttl_secs,
        body_size,
        "message enqueued"
    );

    // Apply host cap to the reply wait (rubric 8.6): if X-Host-Cap-Secs is smaller
    // than await_reply_secs we use the cap and record that the wait was shortened.
    let (effective_await_secs, await_was_capped) = match (await_reply_secs, host_cap_secs) {
        (Some(a), Some(cap)) if cap < a => (Some(cap), true),
        (Some(a), _) => (Some(a), false),
        (None, _) => (None, false),
    };

    // Fire-and-forget is strictly one-way (rubric 2.3): it enqueues and returns the
    // enqueue result only. It must NOT drain the sender's own inbox — mail is
    // delivered solely through `receive`. A prior "implicit receive-on-send" that
    // returned waiting messages here silently lost them whenever the agent did not
    // read the send response, since the drain removed them from the inbox.
    //
    // Send-with-reply: block on sender's own inbox — filtered to THIS room only,
    // the mandatory counterpart of receive's optional room filter — while also
    // watching the recipient's inbox so the wait abandons promptly (rather than
    // draining the full timeout) if the recipient is evicted before replying.
    let (reply_messages, reply_preempted) = if let Some(reply_timeout_secs) = effective_await_secs {
        wait_for_messages(
            sender_inbox.clone(),
            recipient_inbox.clone(),
            reply_timeout_secs,
            &to,
            &sender_name,
            room_token.clone(),
            room_name.clone(),
        )
        .await?
    } else {
        (vec![], false)
    };

    // A send-with-reply parks on the sender's own inbox for the reply window and is
    // exempt from idle eviction while parked; re-anchor the sender's idle window (for
    // this room only — the reply-wait is filtered to it) to when that wait
    // completes (same rationale as receive). Fire-and-forget does not park, so its
    // start-of-call stamp already suffices.
    if effective_await_secs.is_some() {
        app.write()
            .await
            .touch_membership(&session_token, room_token.as_str());
    }

    // A preempted reply-wait is neither a delivery nor a real timeout — surface it
    // as its own status so a caller who fired concurrent blocking calls can tell
    // "my newer call took over" apart from "the wait elapsed empty" (mirrors the
    // receive path). Preemption outranks the host cap for the same reason.
    let empty = reply_messages.is_empty();
    let timeout = if reply_preempted && empty {
        Timeout::Preempted
    } else {
        host_cap::apply_timeout(await_was_capped, await_reply_secs.is_some() && empty)
    };
    if timeout == Timeout::Preempted {
        debug!(session = %session_id(&session_token), sender = %sender_name, to = %to, "send-with-reply preempted — newer blocking call took the receive slot");
    } else if timeout == Timeout::Capped {
        debug!(session = %session_id(&session_token), sender = %sender_name, to = %to, "send-with-reply capped — host cap fired with no reply");
    }

    // Only a send-with-reply can return messages (the awaited reply). Fire-and-forget
    // never does.
    let all_messages: Vec<InboxMessage> = reply_messages;

    if !all_messages.is_empty() {
        info!(
            sender = %sender_name,
            message_count = all_messages.len(),
            "reply returned to sender in send-with-reply response"
        );
    }

    Ok(SendResult {
        message_id,
        ttl_secs: effective_ttl_secs,
        last_seen: last_seen_str,
        messages: all_messages,
        timeout,
    })
}

/// Block on the sender's own inbox until a reply *from `room_token`* arrives or
/// the timeout elapses.
///
/// Routes the wait through [`park_on_inbox`] — the same single preemptible slot used
/// by `receive` — so a send-with-reply whose client disconnects cannot linger as a
/// zombie waiter that steals message wakeups (the `surfing` deadlock). A later
/// `receive` (or another send-with-reply) by the same member preempts it cleanly.
/// The room filter is mandatory here (unlike `receive`'s optional one): only a
/// message tagged with the room this `send` targeted satisfies the wait; anything
/// else that arrives in the same window (e.g. traffic from one of the sender's
/// other rooms) stays queued for a later `receive`.
///
/// The wait also races the recipient's inbox close signal: if the recipient is
/// evicted (kick, room expiry, idle TTL, supersede) while we wait, the reply can
/// never arrive, so we abandon promptly with [`Error::RecipientEvicted`] instead of
/// reporting a misleading `timeout: elapsed` after the full window.
#[allow(clippy::too_many_arguments)]
async fn wait_for_messages(
    inbox: Arc<Inbox>,
    recipient_inbox: Arc<Inbox>,
    timeout_secs: u64,
    peer: &str,
    member_name: &str,
    room_token: RoomToken,
    room_name: String,
) -> Result<(Vec<InboxMessage>, bool), Error> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    // Count this in-flight reply-wait against the recipient's inbox for the whole
    // wait so the operator diagnostic doesn't misread the recipient's extra Arc
    // holders (this sender's `recipient_inbox` clones) as an orphaned-frame leak
    // (MT-3). Dropped on every exit path, including cancellation.
    let _reply_guard = ReplyWaitGuard::new(recipient_inbox.clone());
    let outcome = tokio::select! {
        outcome = park_on_inbox(inbox, deadline, member_name, move |rt| rt == &room_token) => outcome?,
        _ = recipient_inbox.closed() => {
            warn!(peer = %peer, "send-with-reply aborted — recipient evicted before replying");
            return Err(Error::RecipientEvicted(peer.to_string()));
        }
    };

    if outcome.messages.is_empty() && !outcome.preempted {
        warn!(peer = %peer, timeout_secs, "send-with-reply timed out — no reply received");
    }

    let preempted = outcome.preempted;
    let messages = outcome
        .messages
        .into_iter()
        .map(|m| {
            // The reply-wait's mandatory filter guarantees every returned message is
            // tagged with `room_token` — the room this `send` targeted — so its
            // display name is already known, no extra store lookup needed.
            let msg = InboxMessage::from_queued(m, room_name.clone());
            debug!(message_id = %msg.id, from = %msg.from, age = %msg.age, "reply received");
            msg
        })
        .collect();
    Ok((messages, preempted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        rooms::RoomStore,
        state::AppState,
        test_support::{test_config, TEST_MASTER_SECRET},
        tools::{connect::handle_connect, join::handle_join_room_token},
    };
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::RwLock;

    async fn connect_two(
        dir: &TempDir,
    ) -> (
        crate::state::SharedAppState,
        Arc<RwLock<RoomStore>>,
        String,
        String,
    ) {
        let store_path = dir.path().join("rooms.yaml");
        let mut store = RoomStore::load(store_path).unwrap();
        let room = store
            .create("testroom".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();
        let store = Arc::new(RwLock::new(store));
        let app = AppState::new();

        let room_token = room.token.to_string();
        let alice = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_room_token(
            &app,
            &store,
            alice.clone(),
            room_token.clone(),
            "alice".into(),
        )
        .await
        .unwrap();
        let bob = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_room_token(&app, &store, bob.clone(), room_token, "bob".into())
            .await
            .unwrap();
        (app, store, alice, bob)
    }

    /// Fetch a member's inbox Arc from app state (test helper).
    async fn member_inbox(app: &crate::state::SharedAppState, member: &str) -> Arc<Inbox> {
        let state = app.read().await;
        for room_state in state.rooms.values() {
            if let Some(m) = room_state.members.get(member) {
                return m.inbox.clone();
            }
        }
        panic!("member {member} not found");
    }

    #[tokio::test]
    async fn test_send_fire_and_forget() {
        // Rubric 2.1: send without await_reply_secs returns immediately after enqueueing.
        let dir = TempDir::new().unwrap();
        let (app, store, alice, _) = connect_two(&dir).await;

        let result = handle_send(
            &app,
            &test_config(),
            &store,
            alice,
            "testroom".into(),
            "bob".into(),
            "hello".into(),
            None,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_invalid_session() {
        // Rubric 1.2: every tool call validates the session token.
        let dir = TempDir::new().unwrap();
        let (app, store, _, _) = connect_two(&dir).await;

        let result = handle_send(
            &app,
            &test_config(),
            &store,
            "bad-session".into(),
            "testroom".into(),
            "bob".into(),
            "hi".into(),
            None,
            None,
            None,
        )
        .await;
        assert!(matches!(
            result.unwrap_err(),
            crate::error::Error::InvalidSessionToken
        ));
    }

    #[tokio::test]
    async fn test_send_recipient_not_found() {
        // Rubric 2.2/2.5: sending to a non-existent member returns a descriptive error.
        let dir = TempDir::new().unwrap();
        let (app, store, alice, _) = connect_two(&dir).await;

        let result = handle_send(
            &app,
            &test_config(),
            &store,
            alice,
            "testroom".into(),
            "nobody".into(),
            "hi".into(),
            None,
            None,
            None,
        )
        .await;
        assert!(matches!(
            result.unwrap_err(),
            crate::error::Error::MemberNotFound(_)
        ));
    }

    #[tokio::test]
    async fn test_send_unknown_room_name_is_room_not_found() {
        // Rubric 2.6: an invalid room name is a descriptive RoomNotFound.
        let dir = TempDir::new().unwrap();
        let (app, store, alice, _) = connect_two(&dir).await;

        let result = handle_send(
            &app,
            &test_config(),
            &store,
            alice,
            "no-such-room".into(),
            "bob".into(),
            "hi".into(),
            None,
            None,
            None,
        )
        .await;
        assert!(matches!(
            result.unwrap_err(),
            crate::error::Error::RoomNotFound(_)
        ));
    }

    #[tokio::test]
    async fn test_send_room_name_not_joined_is_not_connected_to_room() {
        // A session that belongs to a DIFFERENT room cannot send into a room that
        // exists but that it has not joined.
        let dir = TempDir::new().unwrap();
        let (app, store, alice, _bob) = connect_two(&dir).await;
        store
            .write()
            .await
            .create("other-room".into(), 30, 50, TEST_MASTER_SECRET)
            .await
            .unwrap();

        let result = handle_send(
            &app,
            &test_config(),
            &store,
            alice,
            "other-room".into(),
            "bob".into(),
            "hi".into(),
            None,
            None,
            None,
        )
        .await;
        assert!(matches!(
            result.unwrap_err(),
            crate::error::Error::NotConnectedToRoom(_)
        ));
    }

    #[tokio::test]
    async fn test_send_default_ttl_in_response() {
        // Rubric 6.1: when no TTL is specified the configured default is used,
        // and the response includes the TTL that was applied.
        let dir = TempDir::new().unwrap();
        let (app, store, alice, _) = connect_two(&dir).await;
        let config = test_config();

        let result = handle_send(
            &app,
            &config,
            &store,
            alice,
            "testroom".into(),
            "bob".into(),
            "hi".into(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.ttl_secs, config.default_message_ttl_secs);
    }

    #[tokio::test]
    async fn test_send_message_too_large() {
        // Rubric 9.2: messages exceeding 100 KB are rejected with a size error.
        let dir = TempDir::new().unwrap();
        let (app, store, alice, _) = connect_two(&dir).await;

        let oversized = "x".repeat(100 * 1024 + 1);
        let result = handle_send(
            &app,
            &test_config(),
            &store,
            alice,
            "testroom".into(),
            "bob".into(),
            oversized,
            None,
            None,
            None,
        )
        .await;
        assert!(matches!(
            result.unwrap_err(),
            crate::error::Error::MessageTooLarge { .. }
        ));
    }

    #[tokio::test]
    async fn test_send_ttl_exceeds_maximum() {
        let dir = TempDir::new().unwrap();
        let (app, store, alice, _) = connect_two(&dir).await;

        let result = handle_send(
            &app,
            &test_config(),
            &store,
            alice,
            "testroom".into(),
            "bob".into(),
            "hi".into(),
            Some(9999),
            None,
            None,
        )
        .await;
        assert!(matches!(
            result.unwrap_err(),
            crate::error::Error::MessageTtlExceeded { .. }
        ));
    }

    #[tokio::test]
    async fn test_send_with_reply_effective_ttl_is_await_plus_declared() {
        // When await_reply_secs is set, the effective TTL reported in the result
        // must be await_reply_secs + ttl_secs, not just ttl_secs. This prevents
        // messages from expiring before the reply window closes.
        let dir = TempDir::new().unwrap();
        let (app, store, alice, _) = connect_two(&dir).await;
        let config = test_config();

        let declared_ttl = 60u64;
        let await_secs = 30u64;

        let result = handle_send(
            &app,
            &config,
            &store,
            alice,
            "testroom".into(),
            "bob".into(),
            "hi".into(),
            Some(declared_ttl),
            Some(await_secs),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            result.ttl_secs,
            await_secs + declared_ttl,
            "effective TTL must be await_reply_secs + declared TTL"
        );
    }

    #[tokio::test]
    async fn test_send_with_reply_parks_in_receive_slot_and_gets_reply() {
        // Regression for the `surfing` deadlock: the send-with-reply reply-wait must
        // go through the single preemptible receive slot (so it can't become a zombie
        // thief), and must still deliver the reply that lands in the sender's inbox.
        let dir = TempDir::new().unwrap();
        let (app, store, alice, bob) = connect_two(&dir).await;
        let config = test_config();

        // alice sends to bob and awaits a reply — parks on alice's own inbox.
        let app2 = app.clone();
        let store2 = store.clone();
        let send = tokio::spawn(async move {
            handle_send(
                &app2,
                &test_config(),
                &store2,
                alice,
                "testroom".into(),
                "bob".into(),
                "ping".into(),
                None,
                Some(30),
                None,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Proof the reply-wait went through the slot: alice's inbox is receive-active.
        {
            let alice_inbox = member_inbox(&app, "alice").await;
            assert!(
                alice_inbox.is_receive_active(),
                "send-with-reply must hold the sender's preemptible receive slot"
            );
        }

        // bob replies → lands in alice's inbox → wakes the parked send-with-reply.
        handle_send(
            &app,
            &config,
            &store,
            bob,
            "testroom".into(),
            "alice".into(),
            "pong".into(),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let result = send.await.unwrap().unwrap();
        assert_eq!(result.messages.len(), 1, "the reply must be delivered");
        assert_eq!(result.messages[0].body, "pong");
    }

    #[tokio::test]
    async fn test_fire_and_forget_to_closed_inbox_returns_recipient_evicted() {
        // TDD 10.10 (send/eviction): a fire-and-forget send whose recipient was
        // evicted at/just-before enqueue must return RecipientEvicted — never a
        // success/message_id (no false delivery). We close bob's inbox while bob is
        // still in the room map, reproducing the post-lock-release race window
        // deterministically.
        let dir = TempDir::new().unwrap();
        let (app, store, alice, _) = connect_two(&dir).await;

        member_inbox(&app, "bob").await.close().await;

        let err = handle_send(
            &app,
            &test_config(),
            &store,
            alice,
            "testroom".into(),
            "bob".into(),
            "hello".into(),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, Error::RecipientEvicted(ref m) if m == "bob"),
            "expected RecipientEvicted, got {err:?}"
        );
    }

    #[tokio::test]
    async fn test_send_with_reply_marks_recipient_message_and_fire_and_forget_does_not() {
        // Rubric 4.4: a send-with-reply stamps the delivered message with
        // reply_expected_within_secs (counting down the sender's remaining window);
        // a fire-and-forget delivers with the field absent (None).
        use crate::tools::receive::handle_receive;
        let dir = TempDir::new().unwrap();
        let (app, store, alice, bob) = connect_two(&dir).await;

        // alice sends to bob awaiting a reply on a long window so the countdown is > 0.
        let app2 = app.clone();
        let store2 = store.clone();
        let alice_send = alice.clone();
        let send = tokio::spawn(async move {
            handle_send(
                &app2,
                &test_config(),
                &store2,
                alice_send,
                "testroom".into(),
                "bob".into(),
                "ping".into(),
                None,
                Some(1000),
                None,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // bob receives — the delivered message advertises the sender's reply window.
        let recv = handle_receive(&app, &store, bob.clone(), 1, None, None)
            .await
            .unwrap();
        assert_eq!(recv.messages.len(), 1);
        let within = recv.messages[0].reply_expected_within_secs;
        assert!(
            matches!(within, Some(n) if n > 0 && n <= 1000),
            "send-with-reply must stamp reply_expected_within_secs in (0, 1000]; got {within:?}"
        );

        // bob replies so alice's parked send unblocks cleanly.
        handle_send(
            &app,
            &test_config(),
            &store,
            bob.clone(),
            "testroom".into(),
            "alice".into(),
            "pong".into(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let _ = send.await.unwrap().unwrap();

        // A fire-and-forget from alice delivers with no reply-expected marker.
        handle_send(
            &app,
            &test_config(),
            &store,
            alice,
            "testroom".into(),
            "bob".into(),
            "hi".into(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let recv_ff = handle_receive(&app, &store, bob, 1, None, None)
            .await
            .unwrap();
        assert_eq!(recv_ff.messages.len(), 1);
        assert_eq!(
            recv_ff.messages[0].reply_expected_within_secs, None,
            "fire-and-forget must not set reply_expected_within_secs"
        );
    }

    #[tokio::test]
    async fn test_await_reply_unblocks_with_recipient_evicted_on_eviction() {
        // TDD 10.10 (send/eviction): an await-reply send whose recipient is evicted
        // DURING the wait must unblock promptly with RecipientEvicted, not drain the
        // full timeout and report timeout: elapsed.
        let dir = TempDir::new().unwrap();
        let (app, store, alice, _bob) = connect_two(&dir).await;

        // alice sends to bob and awaits a reply on a long (3600s) window.
        let app2 = app.clone();
        let store2 = store.clone();
        let send = tokio::spawn(async move {
            handle_send(
                &app2,
                &test_config(),
                &store2,
                alice,
                "testroom".into(),
                "bob".into(),
                "ping".into(),
                None,
                Some(3600),
                None,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Evict bob — this closes bob's inbox, which alice is watching.
        {
            let token = app
                .read()
                .await
                .rooms
                .keys()
                .next()
                .unwrap()
                .as_str()
                .to_string();
            app.write()
                .await
                .evict_member(&token, "room", "bob", "kicked")
                .await;
        }

        // alice's send must return promptly (well under 3600s) with RecipientEvicted.
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), send)
            .await
            .expect("send-with-reply did not unblock promptly after recipient eviction")
            .expect("send task panicked");
        assert!(
            matches!(result, Err(Error::RecipientEvicted(ref m)) if m == "bob"),
            "expected RecipientEvicted, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_send_with_reply_room_filter_ignores_reply_from_other_room() {
        // TDD 4.3 / mandatory room filter: a message that arrives in the sender's
        // inbox from a DIFFERENT room during the reply-wait does not satisfy it —
        // it stays queued, and the wait continues (here, times out empty).
        let dir = TempDir::new().unwrap();
        let (app, store, alice, _bob) = connect_two(&dir).await;

        // alice also joins a second room.
        let other_room_token = {
            let mut s = store.write().await;
            s.create("other-room".into(), 30, 50, TEST_MASTER_SECRET)
                .await
                .unwrap()
                .token
                .to_string()
        };
        handle_join_room_token(
            &app,
            &store,
            alice.clone(),
            other_room_token.clone(),
            "alice2".into(),
        )
        .await
        .unwrap();
        let carol = handle_connect(&app, None).await.unwrap().session_token;
        handle_join_room_token(
            &app,
            &store,
            carol.clone(),
            other_room_token,
            "carol".into(),
        )
        .await
        .unwrap();

        // alice sends to bob (in testroom) awaiting a reply, on a short window.
        let app2 = app.clone();
        let store2 = store.clone();
        let alice2 = alice.clone();
        let send = tokio::spawn(async move {
            handle_send(
                &app2,
                &test_config(),
                &store2,
                alice2,
                "testroom".into(),
                "bob".into(),
                "ping".into(),
                None,
                Some(1),
                None,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // carol (other-room) sends to alice2 — lands in the SAME shared inbox, but
        // tagged with other-room, not testroom.
        handle_send(
            &app,
            &test_config(),
            &store,
            carol,
            "other-room".into(),
            "alice2".into(),
            "unrelated".into(),
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // The testroom reply-wait must NOT be satisfied by the other-room message —
        // it times out empty.
        let result = send.await.unwrap().unwrap();
        assert!(
            result.messages.is_empty(),
            "a reply from a different room must not satisfy this room's reply-wait"
        );

        // And the unrelated message must still be sitting in the shared inbox,
        // retrievable via a receive filtered to other-room.
        use crate::tools::receive::handle_receive;
        let recv = handle_receive(&app, &store, alice, 0, None, Some("other-room".into()))
            .await
            .unwrap();
        assert_eq!(recv.messages.len(), 1);
        assert_eq!(recv.messages[0].body, "unrelated");
    }
}
