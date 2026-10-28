use crate::{
    error::Error,
    rooms::{impl_str_newtype, RoomToken},
    tools::session_id,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use toaster_talk_common::ct_str_eq;
use tokio::sync::{Mutex, Notify, RwLock};
use uuid::Uuid;

// ── SessionToken / MemberName ────────────────────────────────────────────────

/// A session credential minted by `connect`. Newtype over `String` so the
/// compiler rejects transposing it with a member name, room token, or seat token.
///
/// Credential-grade: `Debug` and `Display` print only the redacted 8-char
/// [`session_id`] prefix, and there is deliberately no `Deref<Target = str>`, so
/// the full value is reachable only through an explicit, greppable
/// [`SessionToken::as_str`]. In-memory only — it is never persisted, so it has no
/// serde impls. `From<String>` is unchecked: a caller-supplied string that is not
/// a live token simply fails to resolve in `AppState`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SessionToken(String);

impl_str_newtype!(SessionToken);

impl From<String> for SessionToken {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SessionToken {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl std::fmt::Display for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(session_id(&self.0))
    }
}

impl std::fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SessionToken({}…)", session_id(&self.0))
    }
}

/// A member's name within a room. Newtype over `String` so it cannot be
/// transposed with a session token or room token.
///
/// [`MemberName::new`] is the only in-code constructor and enforces the
/// member-name rules (non-empty, at most 40 chars, `[a-zA-Z0-9_-]`), so a name
/// built in code is valid by construction. `Deserialize` is `#[serde(transparent)]`
/// and unvalidated, keeping the persisted seat YAML byte-identical and the store's
/// load behaviour unchanged; the store only ever writes names that passed `new`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemberName(String);

impl_str_newtype!(MemberName);

impl MemberName {
    /// Validate `name` against the member-name rules and wrap it.
    pub fn new(name: impl Into<String>) -> Result<Self, Error> {
        let name = name.into();
        if toaster_talk_common::is_valid_member_name(&name) {
            Ok(Self(name))
        } else {
            Err(Error::InvalidMemberName(name))
        }
    }

    /// Unwrap into the underlying `String` (for error payloads and wire output).
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::ops::Deref for MemberName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MemberName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// ── AuthMethod ────────────────────────────────────────────────────────────────

/// How a member authenticated when they connected. Recorded as provenance on
/// `MemberState` and fixed for the room-membership's lifetime.
///
/// The idle-TTL sweep exempts a member **iff** `auth == Seat`. Exemption is keyed
/// on the auth *method*, never on the member's name: a member that joins via the
/// unprivileged room-token path under a name that happens to collide with a
/// reserved seat is still mortal. Keying on name would let such a squatter live
/// forever while authenticated by the unprivileged mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    /// Authenticated with a plain room token — subject to the idle TTL.
    RoomToken,
    /// Authenticated with a reserved seat token — exempt from the idle TTL.
    Seat,
}

/// Outcome of [`Inbox::try_push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// The message was enqueued.
    Pushed,
    /// The inbox is at `max_messages` capacity (SEC-2 DoS mitigation).
    Full,
    /// The inbox belongs to an evicted member; the message was not enqueued.
    Closed,
}

// ── QueuedMessage ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct QueuedMessage {
    pub id: Uuid,
    pub from: MemberName,
    /// The room this message was sent in — the token, not the display name (the
    /// display name lives only in `RoomStore`; callers resolve it for the wire
    /// response). Every message carries this because one session's inbox can be
    /// shared across several rooms: it is both the tag a `receive`/send-with-reply
    /// room filter matches against, and the basis for tagging each delivered
    /// message with the room it came from (TDD 3.10).
    pub room: RoomToken,
    pub body: String,
    pub enqueued_at: Instant,
    pub expires_at: Instant,
    /// When the sender's `send`-with-reply window closes, stamped at enqueue as
    /// `enqueued_at + await_reply_secs`. `None` for a fire-and-forget send. Read at
    /// delivery to tell the recipient how much of the sender's reply window remains
    /// (`reply_expected_within_secs`). Advisory intent, not live liveness: the sender
    /// may already have been preempted or disconnected by the time this is read.
    pub reply_deadline: Option<Instant>,
}

// ── Inbox ─────────────────────────────────────────────────────────────────────

/// Result of a bounded inbox drain: the messages taken, plus how many further
/// still-valid messages matching the same filter were left queued because the
/// per-call cap was reached.
#[derive(Debug, Default)]
pub struct Drained {
    pub messages: Vec<QueuedMessage>,
    pub remaining: usize,
}

/// Each session's private message queue — shared across every room membership
/// that session holds: one agent, however many rooms it has joined, drains one
/// inbox.
///
/// Uses a `Mutex<VecDeque>` rather than `mpsc` so that senders (`try_push`), the
/// send-with-reply reply-wait, and the `receive` handler can all access the queue
/// without holding the global AppState lock. Fire-and-forget `send` only pushes; it
/// never drains the sender's own inbox (rubric 2.3, strictly one-way).
///
/// The receive slot is controlled by a generation counter (`active_gen`). A value
/// of 0 means no receive is active; a nonzero value is the generation of the current
/// holder. When a new receive preempts a stale one it aborts the old sub-task via
/// `receive_abort` and claims a new generation, making the old `ReceiveGuard::drop`
/// a no-op via CAS — preventing the old guard from releasing the new holder's slot.
/// This remains a single-inbox, single-slot invariant even though the inbox is now
/// shared across rooms: `receive`'s optional room filter and send-with-reply's
/// mandatory one are just predicates threaded through the same one live waiter.
///
/// `notify` wakes a waiting `receive` call when a new message is enqueued.
///
/// `closed` marks an inbox whose owning session has no room memberships left. It is
/// set under the `queue` lock by [`Inbox::close`] so that close-vs-push is
/// serialised: a `try_push` that wins the lock enqueues into a still-live inbox
/// (acceptable lossy-mailbox behaviour if the member is evicted moments later),
/// while a `try_push` that loses the lock observes `closed` and fails-closed instead
/// of silently dropping the message and reporting a false delivery. `closed_notify`
/// wakes a `send`-with-reply sender parked watching the recipient's inbox.
pub struct Inbox {
    pub queue: Mutex<VecDeque<QueuedMessage>>,
    pub notify: Notify,
    active_gen: AtomicU64,
    gen_counter: AtomicU64,
    receive_abort: std::sync::Mutex<Option<tokio::task::AbortHandle>>,
    closed: AtomicBool,
    closed_notify: Notify,
    /// Count of in-flight `send`-with-reply calls parked waiting for a reply from
    /// *this* member. Each such sender holds `Arc<Inbox>` clones of this inbox (the
    /// `recipient_inbox` for the close-race), so a non-zero count legitimately
    /// explains `arc_strong_count > 1` while `receive_active` is false — preventing
    /// the diagnostic from misreading healthy in-flight traffic as an orphaned-frame
    /// leak (MT-3). Maintained by [`ReplyWaitGuard`].
    reply_waiters: AtomicUsize,
}

impl Inbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Inbox {
            queue: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
            active_gen: AtomicU64::new(0),
            gen_counter: AtomicU64::new(0),
            receive_abort: std::sync::Mutex::new(None),
            closed: AtomicBool::new(false),
            closed_notify: Notify::new(),
            reply_waiters: AtomicUsize::new(0),
        })
    }

    /// Enqueue with an inbox size cap. Returns [`PushOutcome::Full`] if the inbox
    /// is already at `max_messages` capacity (SEC-2: DoS via memory exhaustion
    /// mitigation), or [`PushOutcome::Closed`] if the recipient has been evicted.
    ///
    /// The closed check happens **under the queue lock**, which `close` also takes;
    /// this serialises the close-vs-push race so a sender is never told a message
    /// was delivered to an evicted recipient (see the struct-level doc).
    pub async fn try_push(&self, msg: QueuedMessage, max_messages: usize) -> PushOutcome {
        let mut q = self.queue.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return PushOutcome::Closed;
        }
        if q.len() >= max_messages {
            return PushOutcome::Full;
        }
        q.push_back(msg);
        drop(q);
        self.notify.notify_one();
        PushOutcome::Pushed
    }

    /// Mark this inbox closed (the owning session has no room memberships left)
    /// and wake any sender parked watching it for a reply. Idempotent. Takes the
    /// queue lock to serialise against an in-flight `try_push` (see the
    /// struct-level doc).
    pub async fn close(&self) {
        let _q = self.queue.lock().await;
        self.closed.store(true, Ordering::Release);
        drop(_q);
        self.closed_notify.notify_waiters();
    }

    /// Returns `true` once this inbox has been closed by eviction.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Resolve once this inbox is closed. Used by a `send`-with-reply sender to
    /// abandon its reply-wait promptly if the recipient is evicted mid-call.
    /// Registers the wakeup before checking the flag so a concurrent `close`
    /// cannot be missed.
    pub async fn closed(&self) {
        let notified = self.closed_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_closed() {
            return;
        }
        notified.await;
    }

    /// Drain all currently queued messages (non-blocking). Reserved for tests.
    #[allow(dead_code)]
    pub async fn drain_ready(&self) -> Vec<QueuedMessage> {
        self.queue.lock().await.drain(..).collect()
    }

    /// Drain all messages that have not yet expired **and** whose room tag
    /// satisfies `filter`. A still-valid message that does not match `filter` is
    /// left queued, not dropped — this is the primitive behind `receive`'s
    /// optional room filter and send-with-reply's mandatory one. Unbounded:
    /// convenience wrapper over [`Inbox::drain_valid_filtered_capped`].
    pub async fn drain_valid_filtered(
        &self,
        now: Instant,
        filter: impl Fn(&RoomToken) -> bool + Send + Sync,
    ) -> Vec<QueuedMessage> {
        self.drain_valid_filtered_capped(now, filter, usize::MAX)
            .await
            .messages
    }

    /// Drain at most `limit` messages that have not yet expired **and** whose
    /// room tag satisfies `filter`, oldest first. Matching messages beyond
    /// `limit` stay queued in their original order (as do non-matching ones), so
    /// the next drain picks up exactly where this one stopped — FIFO is preserved
    /// and nothing is lost. Expired messages are dropped wherever they sit in the
    /// queue, same as the unbounded drain.
    ///
    /// The returned [`Drained::remaining`] is the number of still-valid messages
    /// matching `filter` left queued, counted under the same queue lock as the
    /// drain — this is what lets `receive` tell its caller "more is waiting".
    /// A `limit` of `0` is treated as `1`: a drain that could never return
    /// anything would strand a parked waiter in a busy loop.
    pub async fn drain_valid_filtered_capped(
        &self,
        now: Instant,
        filter: impl Fn(&RoomToken) -> bool + Send + Sync,
        limit: usize,
    ) -> Drained {
        let limit = limit.max(1);
        let mut q = self.queue.lock().await;
        let mut matched = Vec::new();
        let mut remaining_matching = 0usize;
        let mut kept = VecDeque::with_capacity(q.len());
        for m in q.drain(..) {
            if m.expires_at <= now {
                continue; // expired — dropped regardless of room, same as before
            }
            if filter(&m.room) {
                if matched.len() < limit {
                    matched.push(m);
                } else {
                    remaining_matching += 1;
                    kept.push_back(m);
                }
            } else {
                kept.push_back(m);
            }
        }
        *q = kept;
        Drained {
            messages: matched,
            remaining: remaining_matching,
        }
    }

    /// Drain all messages that have not yet expired, regardless of room.
    /// Convenience wrapper over [`Inbox::drain_valid_filtered`] for callers (TTL
    /// carryover, tests) that don't need the room filter.
    pub async fn drain_valid(&self, now: Instant) -> Vec<QueuedMessage> {
        self.drain_valid_filtered(now, |_| true).await
    }

    /// Seed this inbox with messages carried over from a superseded room
    /// membership's shared inbox during a `join` seat-token supersede
    /// (queue-transfer — ANTI-PATTERNS #11). Each message keeps its original
    /// `expires_at`, so TTL is preserved, not reset. Appends under the queue lock
    /// and always notifies: safe whether or not the inbox already has a live
    /// parked receiver — a spurious `notify_one()` permit with nothing new to
    /// drain just costs one harmless extra loop iteration in
    /// `wait_drain_until_filtered`.
    pub async fn seed(&self, messages: Vec<QueuedMessage>) {
        if messages.is_empty() {
            return;
        }
        {
            let mut q = self.queue.lock().await;
            q.extend(messages);
        }
        self.notify.notify_one();
    }

    /// Block until at least one valid message matching `filter` is available or
    /// `deadline` elapses. Returns up to `limit` such messages (oldest first) on
    /// wakeup, leaving any further matching messages — and every non-matching
    /// valid one — queued in order; or an empty [`Drained`] if the deadline passed
    /// before any matching message arrived. Because the queue is checked before
    /// parking, a call made while matching messages are already queued (e.g. the
    /// remainder of a previous capped drain) returns immediately.
    pub async fn wait_drain_until_filtered(
        &self,
        deadline: Instant,
        filter: impl Fn(&RoomToken) -> bool + Send + Sync,
        limit: usize,
    ) -> Drained {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);

            let drained = self
                .drain_valid_filtered_capped(Instant::now(), &filter, limit)
                .await;
            if !drained.messages.is_empty() {
                return drained;
            }

            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                return Drained::default();
            }

            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(remaining) => {
                    return Drained::default();
                }
            }
        }
    }

    /// Block until at least one valid message (any room) is available or
    /// `deadline` elapses, draining everything. Convenience wrapper over
    /// [`Inbox::wait_drain_until_filtered`].
    pub async fn wait_drain_until(&self, deadline: Instant) -> Vec<QueuedMessage> {
        self.wait_drain_until_filtered(deadline, |_| true, usize::MAX)
            .await
            .messages
    }

    /// Remove expired messages in-place (called by TTL sweeper).
    pub async fn evict_expired(&self, now: Instant) -> usize {
        let mut q = self.queue.lock().await;
        let before = q.len();
        q.retain(|m| m.expires_at > now);
        before - q.len()
    }

    /// Discard still-queued messages tagged for one specific room (used when a
    /// single room-membership is evicted but the owning session survives via its
    /// other rooms — rubric 10.16 applied per-room rather than to the whole shared
    /// inbox). Returns the count discarded.
    pub async fn discard_room(&self, now: Instant, room_token: &RoomToken) -> usize {
        let removed = self
            .drain_valid_filtered(now, |r| r == room_token)
            .await
            .len();
        removed
    }

    /// Returns `true` if a receive is currently active for this inbox.
    pub fn is_receive_active(&self) -> bool {
        self.active_gen.load(Ordering::Acquire) != 0
    }

    /// Number of `send`-with-reply calls currently parked waiting for a reply from
    /// this member. Used by the diagnostic to explain expected extra Arc holders.
    pub fn reply_waiters(&self) -> usize {
        self.reply_waiters.load(Ordering::Acquire)
    }

    fn enter_reply_wait(&self) {
        self.reply_waiters.fetch_add(1, Ordering::AcqRel);
    }

    fn exit_reply_wait(&self) {
        self.reply_waiters.fetch_sub(1, Ordering::AcqRel);
    }

    /// Abort any in-progress receive sub-task. No-op if no abort handle is stored.
    /// Called by a new receive to preempt a stale one before claiming the slot.
    pub fn abort_existing_receive(&self) {
        if let Ok(mut guard) = self.receive_abort.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
    }

    /// Claim the receive slot. Returns the new generation number, which the caller
    /// must pass to `release_receive` when it is done.
    pub fn claim_receive(&self) -> u64 {
        let generation = self.gen_counter.fetch_add(1, Ordering::AcqRel) + 1;
        self.active_gen.store(generation, Ordering::Release);
        generation
    }

    /// Store the abort handle for the current receive sub-task so that an
    /// incoming receive can preempt it via `abort_existing_receive`.
    pub fn set_receive_abort(&self, handle: tokio::task::AbortHandle) {
        if let Ok(mut guard) = self.receive_abort.lock() {
            *guard = Some(handle);
        }
    }

    /// Release the receive slot. No-op when `gen` no longer matches the active
    /// generation — meaning a newer receive has already claimed the slot.
    pub fn release_receive(&self, generation: u64) {
        let _ =
            self.active_gen
                .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Read-only point-in-time view of this inbox's queue and receive-slot state,
    /// for the operator diagnostic dump (TDD 13.3, 13.4). Mutates nothing — takes
    /// the queue lock only briefly and reads the receive-slot atomics.
    ///
    /// `strong_count` is supplied by the caller because the caller (not the inbox)
    /// owns the `Arc`; a value of 1 means only `AppState::session_inboxes` holds
    /// the inbox — no room-membership `MemberState` and no `receive` sub-task is
    /// parked on it.
    ///
    /// `room_names` resolves each queued message's room token to a display name
    /// (TDD 13.11 note: the dump should not force the operator to correlate raw
    /// tokens by hand); a room absent from the map (e.g. deleted mid-flight) falls
    /// back to a placeholder rather than failing the dump.
    pub async fn snapshot(
        &self,
        now: Instant,
        strong_count: usize,
        room_names: &HashMap<RoomToken, String>,
    ) -> InboxSnapshot {
        let messages: Vec<MessageSnapshot> = {
            let q = self.queue.lock().await;
            q.iter()
                .map(|m| MessageSnapshot {
                    id_prefix: m.id.to_string()[..8].to_string(),
                    from: m.from.clone(),
                    room: room_names
                        .get(&m.room)
                        .cloned()
                        .unwrap_or_else(|| "<unknown-room>".to_string()),
                    age_secs: now.saturating_duration_since(m.enqueued_at).as_secs(),
                    ttl_remaining_secs: m.expires_at.saturating_duration_since(now).as_secs(),
                })
                .collect()
        };
        let active_gen = self.active_gen.load(Ordering::Acquire);
        let gen_counter = self.gen_counter.load(Ordering::Acquire);
        let abort_handle_present = self
            .receive_abort
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false);
        InboxSnapshot {
            queue_depth: messages.len(),
            receive_active: active_gen != 0,
            active_gen,
            gen_counter,
            abort_handle_present,
            arc_strong_count: strong_count,
            reply_waiters: self.reply_waiters.load(Ordering::Acquire),
            messages,
        }
    }
}

/// RAII guard for an in-flight `send`-with-reply wait: increments the recipient
/// inbox's `reply_waiters` on construction and decrements it on drop — covering
/// normal return, timeout, cancellation, and an orphaned-frame drop. This lets the
/// operator diagnostic tell a *healthy* in-flight send-with-reply (which holds extra
/// `Arc<Inbox>` clones of the recipient) apart from a genuine orphaned-frame leak
/// (MT-3), so it stops flagging normal traffic.
pub(crate) struct ReplyWaitGuard(Arc<Inbox>);

impl ReplyWaitGuard {
    pub(crate) fn new(inbox: Arc<Inbox>) -> Self {
        inbox.enter_reply_wait();
        Self(inbox)
    }
}

impl Drop for ReplyWaitGuard {
    fn drop(&mut self) {
        self.0.exit_reply_wait();
    }
}

// ── MemberState ───────────────────────────────────────────────────────────────

/// One room membership. A session that belongs to N rooms has N `MemberState`
/// entries (one per `RoomState`), but every one of them shares the *same*
/// `inbox: Arc<Inbox>` — the session's one shared mailbox.
pub struct MemberState {
    pub session_token: SessionToken,
    // Stored for diagnostics and future LIST-MEMBERS command
    #[allow(dead_code)]
    pub member_name: MemberName,
    // Stored for diagnostics and future LIST-MEMBERS command
    #[allow(dead_code)]
    pub registered_at: Instant,
    /// Updated on every tool call that touches *this* room membership. Read to
    /// compute `last_seen` duration. Stays per-room-membership, never promoted to
    /// the session as a whole, so an idle room is still reaped even while the
    /// agent is actively working in another.
    pub last_seen: Instant,
    /// How this membership authenticated. Fixed for the membership's lifetime; the
    /// idle TTL sweep exempts a member iff `auth == AuthMethod::Seat`.
    pub auth: AuthMethod,
    /// The session's shared inbox — the same `Arc` as every other room membership
    /// belonging to this session, and as `AppState::session_inboxes[session_token]`.
    pub inbox: Arc<Inbox>,
}

// ── RoomState ─────────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct RoomState {
    pub members: HashMap<MemberName, MemberState>,
}

// ── Diagnostic snapshot ───────────────────────────────────────────────────────
//
// Read-only, redacted views of live in-memory state for the operator `DUMP`
// command (TDD section 13). No field here ever carries a full token — sessions
// are reduced to an 8-char prefix and room/seat tokens are omitted entirely.

/// One queued, undelivered message (TDD 13.4).
pub struct MessageSnapshot {
    /// First 8 chars of the message UUID — enough to correlate, not the full id.
    pub id_prefix: String,
    pub from: MemberName,
    /// Display name of the room this message was sent in — relevant now that one
    /// shared inbox can hold messages from several rooms at once.
    pub room: String,
    pub age_secs: u64,
    pub ttl_remaining_secs: u64,
}

/// A member's inbox + receive-slot machinery (TDD 13.3). Rendered once per
/// session (TDD 13.11), not once per room-membership row.
pub struct InboxSnapshot {
    pub queue_depth: usize,
    pub receive_active: bool,
    pub active_gen: u64,
    pub gen_counter: u64,
    pub abort_handle_present: bool,
    pub arc_strong_count: usize,
    /// In-flight `send`-with-reply calls awaiting a reply from this member. Non-zero
    /// explains extra Arc holders while `receive_active` is false (MT-3).
    pub reply_waiters: usize,
    pub messages: Vec<MessageSnapshot>,
}

/// A single room membership (TDD 13.2, 13.5) — the metadata that is genuinely
/// per-room. Does **not** carry the inbox (TDD 13.11): that is rendered once per
/// session in [`DiagnosticSnapshot::sessions`].
pub struct MemberSnapshot {
    pub member_name: MemberName,
    /// Redacted: first 8 chars of the session token only (TDD 13.6).
    pub session_prefix: String,
    /// Whether this room membership's session round-trips through the global
    /// session map.
    pub session_in_map: bool,
    pub last_seen_secs: u64,
    pub registered_secs_ago: u64,
    /// How the member authenticated: `"seat"` (idle-exempt) or `"room_token"`.
    pub auth_method: &'static str,
    /// Seconds until idle eviction for a room-token member, or `None` when the
    /// member is exempt (seat-authed) and never idle-evicted.
    pub idle_countdown_secs: Option<u64>,
    /// Seat-contention view for this (room, member) seat — `None` for room-token
    /// members, which are never superseded.
    pub seat_contention: Option<SeatContentionSnapshot>,
}

/// Per-seat flap-detection state as shown in `DUMP`.
pub struct SeatContentionSnapshot {
    /// Supersessions inside the detection window right now.
    pub recent_supersedes: usize,
    /// Seconds since the most recent supersession, `None` if none is on record.
    pub last_supersede_secs_ago: Option<u64>,
    /// Seconds left on the freeze, `None` when the seat is not frozen.
    pub frozen_secs_remaining: Option<u64>,
}

/// A room and its members (TDD 13.2). The room token is never included.
pub struct RoomSnapshot {
    pub name: String,
    pub member_count: usize,
    pub members: Vec<MemberSnapshot>,
}

/// A session's shared inbox, rendered exactly once regardless of how many rooms
/// the session belongs to (TDD 13.11).
pub struct SessionSnapshot {
    /// Redacted: first 8 chars of the session token (TDD 13.6).
    pub session_prefix: String,
    /// Seconds since `connect` minted this session (TDD 13.12). The only way to
    /// tell a roomless session (`memberships: []`) that is merely mid-handshake
    /// from one that is genuinely stuck — a bare age alone can't distinguish
    /// them without this.
    pub connected_secs_ago: u64,
    /// `"room/member"` labels for every room membership sharing this inbox, so the
    /// operator can cross-reference back to the room-oriented listing above.
    pub memberships: Vec<String>,
    pub inbox: InboxSnapshot,
}

/// Full point-in-time diagnostic view of `AppState` (TDD section 13).
pub struct DiagnosticSnapshot {
    pub rooms: Vec<RoomSnapshot>,
    pub sessions: Vec<SessionSnapshot>,
    pub session_map_size: usize,
    /// Redacted prefixes of sessions awaiting a one-time `Superseded` delivery.
    pub superseded_session_prefixes: Vec<String>,
    /// Health findings: a single `OK:` line when consistent, otherwise one
    /// `WARN:`/`ERROR:` line per detected invariant violation (TDD 13.7).
    pub invariants: Vec<String>,
}

// ── Membership resolution outcome ─────────────────────────────────────────────

/// Outcome of resolving a `(session_token, room_token)` pair against live state.
/// Kept token/name-agnostic so callers (which know the room's display name) can
/// build the right `Error` variant themselves.
pub enum MembershipCheck {
    /// The session holds a membership in this room, under this member name.
    Member(MemberName),
    /// The session exists but does not belong to this room.
    NotMember,
    /// The session token does not resolve to any live session.
    InvalidSession,
    /// The session was displaced (seat-holder join collision) and its one-time
    /// `Superseded` notice has just been consumed.
    Superseded,
}

// ── Seat-contention flap detection ────────────────────────────────────────────

/// Tunables for seat-contention flap detection, snapshotted from the live
/// `Config` per `join` call. A seat superseded MORE than `threshold` times within
/// `window` is frozen for `hold_down`. `threshold == 0` disables detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeatFlapPolicy {
    pub threshold: u32,
    pub window: Duration,
    pub hold_down: Duration,
}

impl SeatFlapPolicy {
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            threshold: config.seat_flap_threshold,
            window: Duration::from_secs(config.seat_flap_window_secs),
            hold_down: Duration::from_secs(config.seat_flap_hold_down_secs),
        }
    }
}

impl Default for SeatFlapPolicy {
    /// The default-config values: freeze on the 4th supersession within 30 s, for
    /// 60 s.
    fn default() -> Self {
        Self {
            threshold: 3,
            window: Duration::from_secs(30),
            hold_down: Duration::from_secs(60),
        }
    }
}

/// Per-seat (`room_token`, `member_name`) supersession history. Lives outside
/// `MemberState` because every supersede replaces the `MemberState` — the history
/// must outlive any one holder. Pruned by the TTL sweep once it is neither frozen
/// nor carrying a supersession inside its window, so it never accumulates.
#[derive(Debug, Clone)]
pub struct SeatFlap {
    /// Supersession instants still inside `window` (oldest first; pruned lazily).
    pub supersedes: VecDeque<Instant>,
    /// Most recent supersession, kept even after it ages out of `supersedes`.
    pub last_supersede: Instant,
    /// The window in force when the last supersession was recorded — lets the
    /// sweep and the dump judge staleness without consulting the live config.
    pub window: Duration,
    /// `Some(t)` while the seat is frozen: superseding joins are rejected until `t`.
    pub frozen_until: Option<Instant>,
}

impl SeatFlap {
    /// Supersessions inside the window as of `now`, without mutating (dump-safe).
    pub fn recent_count(&self, now: Instant) -> usize {
        self.supersedes
            .iter()
            .filter(|t| now.saturating_duration_since(**t) <= self.window)
            .count()
    }

    /// Remaining freeze as of `now`, or `None` when not (or no longer) frozen.
    pub fn frozen_remaining(&self, now: Instant) -> Option<Duration> {
        self.frozen_until
            .filter(|until| *until > now)
            .map(|until| until - now)
    }
}

/// Outcome of [`AppState::record_seat_supersede`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlapOutcome {
    /// Recorded; still at or under the threshold (or detection disabled).
    Counted(usize),
    /// This supersession pushed the seat over the threshold; it is now frozen.
    Frozen { count: usize, hold_down: Duration },
}

// ── AppState ──────────────────────────────────────────────────────────────────

/// In-memory agent registrations and message queues.
/// Lost on server restart — rooms persist via `RoomStore`.
#[derive(Default)]
pub struct AppState {
    /// Keyed by room token (matches RoomStore key).
    pub rooms: HashMap<RoomToken, RoomState>,
    /// Keyed by session token → the set of rooms (as `(room_token, member_name)`
    /// pairs) that session currently belongs to. A session with an empty set is a
    /// valid, roomless session (fresh from `connect`, before any `join`) —
    /// distinct from a session token that doesn't exist in this map at all.
    pub sessions: HashMap<SessionToken, HashSet<(RoomToken, MemberName)>>,
    /// Keyed by session token → that session's one shared inbox. The canonical
    /// owner of the `Arc<Inbox>` cloned into every `MemberState.inbox` for that
    /// session's room memberships. Populated at `connect` (mint-only, roomless);
    /// removed when the session is torn down (its last room membership evicted,
    /// or `connect`'s `old_session_token` rotation).
    pub session_inboxes: HashMap<SessionToken, Arc<Inbox>>,
    /// Keyed by session token → the `Instant` `connect` minted it. Maintained in
    /// lockstep with `session_inboxes` (same insert/remove sites) so every live
    /// session has an entry. Exists so a roomless session — one that never
    /// reaches `join` — can be aged and idle-reaped (TDD 1a.5): such a session
    /// has no room membership, so it is otherwise invisible to the per-room idle
    /// sweep and would live forever (e.g. an agent that repeatedly retries a bare
    /// `connect` without ever calling `join`). Also surfaced in `DUMP` as
    /// `connected_secs_ago` so an operator can distinguish a session that is
    /// merely mid-handshake from one that is genuinely stuck.
    pub session_connected_at: HashMap<SessionToken, Instant>,
    /// Session tokens that have been superseded by a seat-holder eviction.
    /// One-time delivery: the token is removed from this set on first `Superseded` error.
    pub superseded_sessions: HashSet<SessionToken>,
    /// Seat-contention flap history keyed by `(room_token, member_name)`. Only
    /// seat-token supersessions are recorded here; see [`SeatFlap`].
    pub seat_flaps: HashMap<(RoomToken, MemberName), SeatFlap>,
}

pub type SharedAppState = Arc<RwLock<AppState>>;

impl AppState {
    pub fn new() -> SharedAppState {
        Arc::new(RwLock::new(AppState::default()))
    }

    // ── Session lifecycle (connect / join) ────────────────────────────────────

    /// Mint a brand-new, roomless session: registers an empty membership set and
    /// a fresh shared inbox. This is `connect`'s entire job — no room or seat
    /// credential involved.
    pub fn register_session(&mut self, session_token: SessionToken) -> Arc<Inbox> {
        let inbox = Inbox::new();
        self.sessions.insert(session_token.clone(), HashSet::new());
        self.session_connected_at
            .insert(session_token.clone(), Instant::now());
        self.session_inboxes.insert(session_token, inbox.clone());
        inbox
    }

    /// Attach a room membership to an already-registered session, sharing that
    /// session's existing inbox. Panics-free: if the session is somehow absent
    /// (should never happen — callers validate first via
    /// [`AppState::check_room_membership`]), the membership is still recorded
    /// against a freshly minted inbox rather than silently dropped, and a `WARN`
    /// is logged so the desync is visible.
    pub fn register_room_membership(
        &mut self,
        room_token: RoomToken,
        member_name: MemberName,
        session_token: &SessionToken,
        now: Instant,
        auth: AuthMethod,
    ) {
        let inbox = match self.session_inboxes.get(session_token) {
            Some(ib) => ib.clone(),
            None => {
                tracing::warn!(
                    session = %session_id(session_token),
                    "register_room_membership: no existing session inbox found — minting one \
                     (this should not happen; the caller must validate the session first)"
                );
                let ib = Inbox::new();
                self.session_inboxes
                    .insert(session_token.clone(), ib.clone());
                ib
            }
        };
        let member_state = MemberState {
            session_token: session_token.clone(),
            member_name: member_name.clone(),
            registered_at: now,
            last_seen: now,
            auth,
            inbox,
        };
        self.rooms
            .entry(room_token.clone())
            .or_default()
            .members
            .insert(member_name.clone(), member_state);
        self.sessions
            .entry(session_token.clone())
            .or_default()
            .insert((room_token, member_name));
    }

    /// Tear down an entire session: every room membership it holds is removed
    /// from its `RoomState`, the session-map entry and shared inbox are removed,
    /// and the inbox is aborted/closed. This is the one place a session is
    /// invalidated wholesale regardless of how many rooms remain — `connect`'s
    /// `old_session_token` rotation path (TDD 1a.4). No room/inbox carryover:
    /// bearer-only token presentation must not recover rooms. Ordinary per-room
    /// eviction (idle TTL, `kckm`, room expiry, seat supersede) never calls this;
    /// it goes through [`AppState::evict_member`] and friends, which only tear
    /// down the whole session when the *last* membership is the one being removed.
    pub async fn invalidate_session_fully(&mut self, session_token: &SessionToken) {
        let Some(memberships) = self.sessions.remove(session_token) else {
            return;
        };
        for (room_token, member_name) in &memberships {
            if let Some(rs) = self.rooms.get_mut(room_token.as_str()) {
                rs.members.remove(member_name);
            }
        }
        if let Some(inbox) = self.session_inboxes.remove(session_token) {
            inbox.abort_existing_receive();
            inbox.close().await;
        }
        self.session_connected_at.remove(session_token);
        tracing::info!(
            session = %session_id(session_token),
            rooms_evicted = memberships.len(),
            "session fully invalidated (connect old_session_token rotation)"
        );
    }

    /// Idle-reap sessions minted by `connect` that never reached `join` (TDD
    /// 1a.5). A roomless session has no room membership, so it is invisible to
    /// the per-room idle sweep (`ttl::do_sweep` step 4) and would otherwise live
    /// forever — the failure mode this closes is an agent that repeatedly
    /// retries a bare `connect` (e.g. acting on a stale, pre-split understanding
    /// of the API) without ever supplying `old_session_token`: every retry
    /// leaked a new orphaned `sessions`/`session_inboxes` entry. Measured
    /// against `ttl_secs` (the caller passes `member_idle_ttl_secs`, the same
    /// clock as a room-token member's idle window) anchored on `connect` time —
    /// a roomless session has no room activity to re-anchor against. Safe to
    /// reap unconditionally: `handle_receive` rejects a roomless session before
    /// it ever parks (`NotJoinedAnyRoom`), so a session in this set can never be
    /// actively waiting, and it can never be a `send` recipient either (nothing
    /// resolves a target with no room membership). Returns the number reaped.
    pub async fn reap_idle_roomless_sessions(&mut self, now: Instant, ttl_secs: u64) -> usize {
        let ttl = Duration::from_secs(ttl_secs);
        let victims: Vec<SessionToken> = self
            .sessions
            .iter()
            .filter(|(_, memberships)| memberships.is_empty())
            .filter_map(|(token, _)| {
                let connected_at = self.session_connected_at.get(token)?;
                (now.saturating_duration_since(*connected_at) > ttl).then(|| token.clone())
            })
            .collect();

        for token in &victims {
            self.sessions.remove(token);
            self.session_connected_at.remove(token);
            if let Some(inbox) = self.session_inboxes.remove(token) {
                inbox.abort_existing_receive();
                inbox.close().await;
            }
            tracing::info!(
                session = %session_id(token),
                "roomless session reaped after idle TTL (connected but never joined a room)"
            );
        }

        victims.len()
    }

    // ── Eviction (per-room-membership) ────────────────────────────────────────

    /// Remove one room membership from a session's membership set and, if that
    /// was the session's *last* membership, tear the whole session down (session
    /// map entry + shared inbox aborted and closed). If other memberships remain,
    /// the shared inbox is left fully alone for the survivors *except* that any
    /// still-queued messages tagged for the room being vacated are discarded
    /// (rubric 10.16, applied per-room rather than to the whole shared inbox) —
    /// they can never be delivered again since this session no longer belongs to
    /// that room.
    ///
    /// Every eviction path (`evict_member`, `evict_all_members`, `evict_room`,
    /// `evict_member_superseded`) funnels through this so the last-membership
    /// teardown rule lives in exactly one place.
    async fn evict_one(
        &mut self,
        room_name: &str,
        member_name: &MemberName,
        session_token: &SessionToken,
        room_token: &RoomToken,
        reason: &str,
    ) {
        let mut session_emptied = false;
        if let Some(set) = self.sessions.get_mut(session_token) {
            set.remove(&(room_token.clone(), member_name.clone()));
            session_emptied = set.is_empty();
        }

        if session_emptied {
            self.sessions.remove(session_token);
            self.session_connected_at.remove(session_token);
            if let Some(inbox) = self.session_inboxes.remove(session_token) {
                inbox.abort_existing_receive();
                inbox.close().await;
            }
        } else if let Some(inbox) = self.session_inboxes.get(session_token) {
            let discarded = inbox.discard_room(Instant::now(), room_token).await;
            if discarded > 0 {
                tracing::debug!(
                    room = %room_name,
                    member = %member_name,
                    discarded,
                    "queued messages for the vacated room discarded (session survives via other rooms)"
                );
            }
        }

        let sid = session_id(session_token);
        tracing::warn!(
            room = %room_name,
            member = %member_name,
            session = %sid,
            reason = %reason,
            session_torn_down = session_emptied,
            "member evicted from room"
        );
    }

    /// Evict a single named member from a room.
    /// Returns `true` if found and evicted, `false` if room or member not found.
    pub async fn evict_member(
        &mut self,
        room_token: &str,
        room_name: &str,
        member_name: &MemberName,
        reason: &str,
    ) -> bool {
        let member_state = match self.rooms.get_mut(room_token) {
            Some(rs) => match rs.members.remove(member_name) {
                Some(ms) => ms,
                None => return false,
            },
            None => return false,
        };
        let rt = RoomToken::from(room_token.to_string());
        self.evict_one(
            room_name,
            member_name,
            &member_state.session_token,
            &rt,
            reason,
        )
        .await;
        true
    }

    /// Evict all members from a room without removing the room from app state.
    /// Returns the number of members evicted. No-op if the room is not in app state.
    pub async fn evict_all_members(
        &mut self,
        room_token: &str,
        room_name: &str,
        reason: &str,
    ) -> usize {
        let members: Vec<(MemberName, MemberState)> = match self.rooms.get_mut(room_token) {
            Some(rs) => rs.members.drain().collect(),
            None => return 0,
        };
        let count = members.len();
        let rt = RoomToken::from(room_token.to_string());
        for (member_name, member_state) in &members {
            self.evict_one(
                room_name,
                member_name,
                &member_state.session_token,
                &rt,
                reason,
            )
            .await;
        }
        count
    }

    /// Evict all members from a room and remove it from app state.
    /// Returns the number of members evicted. No-op if the room is not in app state.
    pub async fn evict_room(&mut self, room_token: &str, room_name: &str, reason: &str) -> usize {
        // The room is gone, so is every seat in it: drop their contention history.
        self.clear_room_seat_flaps(room_token);
        if let Some(room_state) = self.rooms.remove(room_token) {
            let count = room_state.members.len();
            let rt = RoomToken::from(room_token.to_string());
            for (member_name, member_state) in room_state.members {
                self.evict_one(
                    room_name,
                    &member_name,
                    &member_state.session_token,
                    &rt,
                    reason,
                )
                .await;
            }
            count
        } else {
            0
        }
    }

    /// Evict the existing holder of a `(room, member_name)` seat-token collision
    /// (`join`'s seat-token path — TDD 1b, 12.2) and mark the evicted session's
    /// one-time `Superseded` notice, but only if this was that session's *last*
    /// membership (see [`AppState::evict_one`]) — if the victim still holds other
    /// live rooms, this eviction stays scoped to the one room and no
    /// session-wide `Superseded` flag is set, so a later call about a *different*
    /// still-joined room is processed normally (TDD 10.21), never a false
    /// `Superseded`.
    ///
    /// Returns the still-valid, room-tagged queued messages that belonged
    /// specifically to the room being vacated, so the caller (`join`) can carry
    /// them into the new holder's own (possibly-already-live) shared inbox
    /// (queue-transfer — ANTI-PATTERNS #11). Messages belonging to the victim's
    /// *other* rooms are never touched. Empty if the member was not present.
    pub async fn evict_member_superseded(
        &mut self,
        room_token: &str,
        room_name: &str,
        member_name: &MemberName,
    ) -> Vec<QueuedMessage> {
        let member_state = match self.rooms.get_mut(room_token) {
            Some(rs) => match rs.members.remove(member_name) {
                Some(ms) => ms,
                None => return Vec::new(),
            },
            None => return Vec::new(),
        };
        let old_session_token = member_state.session_token.clone();
        let inbox = member_state.inbox.clone();
        let rt = RoomToken::from(room_token.to_string());

        // Carry over ONLY the messages tagged for the room being vacated — the
        // rest (if any) belong to this session's other live rooms and must stay
        // queued for it.
        let carried = inbox
            .drain_valid_filtered(Instant::now(), |r| r == &rt)
            .await;

        let mut session_emptied = false;
        if let Some(set) = self.sessions.get_mut(&old_session_token) {
            set.remove(&(rt.clone(), member_name.clone()));
            session_emptied = set.is_empty();
        }

        if session_emptied {
            self.sessions.remove(&old_session_token);
            self.session_connected_at.remove(&old_session_token);
            self.session_inboxes.remove(&old_session_token);
            self.superseded_sessions.insert(old_session_token.clone());
            inbox.abort_existing_receive();
            inbox.close().await;
        }

        let sid = session_id(&old_session_token);
        tracing::warn!(
            room = %room_name,
            member = %member_name,
            session = %sid,
            session_torn_down = session_emptied,
            carried = carried.len(),
            "member evicted — superseded by seat-holder join"
        );

        carried
    }

    // ── Seat-contention flap detection ────────────────────────────────────────

    /// Remaining freeze on seat `(room_token, member_name)`, or `None` if it is not
    /// frozen. A freeze that has lapsed is cleared here (along with its window, so
    /// counting restarts from zero) and logged — the lazy half of expiry; the TTL
    /// sweep's [`AppState::prune_seat_flaps`] is the other.
    pub fn seat_freeze_remaining(
        &mut self,
        room_token: &str,
        room_name: &str,
        member_name: &MemberName,
        now: Instant,
    ) -> Option<Duration> {
        let key = (RoomToken::from(room_token.to_string()), member_name.clone());
        let flap = self.seat_flaps.get_mut(&key)?;
        flap.frozen_until?;
        if let Some(rem) = flap.frozen_remaining(now) {
            return Some(rem);
        }
        flap.frozen_until = None;
        flap.supersedes.clear();
        tracing::info!(
            room = %room_name,
            member = %member_name,
            "seat contention freeze lapsed — superseding seat-token joins accepted again"
        );
        None
    }

    /// Record one seat-token supersession of `(room_token, member_name)` at `now`
    /// and, if it pushes the in-window count over `policy.threshold`, freeze the
    /// seat for `policy.hold_down` and emit the contention WARN. Cheap: one map
    /// entry and a deque that never holds more than `threshold + 1` instants per
    /// window, because the freeze stops further supersessions.
    pub fn record_seat_supersede(
        &mut self,
        room_token: &str,
        room_name: &str,
        member_name: &MemberName,
        now: Instant,
        policy: SeatFlapPolicy,
    ) -> FlapOutcome {
        let key = (RoomToken::from(room_token.to_string()), member_name.clone());
        let flap = self.seat_flaps.entry(key).or_insert_with(|| SeatFlap {
            supersedes: VecDeque::new(),
            last_supersede: now,
            window: policy.window,
            frozen_until: None,
        });
        flap.last_supersede = now;
        flap.window = policy.window;
        flap.supersedes.push_back(now);
        while let Some(front) = flap.supersedes.front() {
            if now.saturating_duration_since(*front) > policy.window {
                flap.supersedes.pop_front();
            } else {
                break;
            }
        }
        let count = flap.supersedes.len();
        if policy.threshold > 0 && count > policy.threshold as usize {
            flap.frozen_until = Some(now + policy.hold_down);
            tracing::warn!(
                room = %room_name,
                member = %member_name,
                supersedes_in_window = count,
                window_secs = policy.window.as_secs(),
                threshold = policy.threshold,
                hold_down_secs = policy.hold_down.as_secs(),
                "seat credential contention — possible shared token: seat superseded too often; \
                 freezing it (the current holder keeps the seat; further superseding seat-token \
                 joins are rejected with SeatContention until the hold-down lapses or an operator \
                 runs kckm/kckall/delmb for this member)"
            );
            FlapOutcome::Frozen {
                count,
                hold_down: policy.hold_down,
            }
        } else {
            tracing::debug!(
                room = %room_name,
                member = %member_name,
                supersedes_in_window = count,
                threshold = policy.threshold,
                "seat supersession recorded for flap detection"
            );
            FlapOutcome::Counted(count)
        }
    }

    /// Operator clear: drop the contention history (and any freeze) for one seat.
    /// Returns `true` if the seat was frozen at the time.
    pub fn clear_seat_flap(
        &mut self,
        room_token: &str,
        member_name: &MemberName,
        now: Instant,
    ) -> bool {
        let key = (RoomToken::from(room_token.to_string()), member_name.clone());
        self.seat_flaps
            .remove(&key)
            .is_some_and(|f| f.frozen_remaining(now).is_some())
    }

    /// Drop the contention history of every seat in a room (room deleted/expired,
    /// or `kckall`).
    pub fn clear_room_seat_flaps(&mut self, room_token: &str) {
        self.seat_flaps
            .retain(|(rt, _), _| rt.as_str() != room_token);
    }

    /// TTL sweep: drop every history that is neither frozen nor holding a
    /// supersession inside its window. Returns the number removed.
    pub fn prune_seat_flaps(&mut self, now: Instant) -> usize {
        let before = self.seat_flaps.len();
        self.seat_flaps.retain(|_, f| {
            f.frozen_remaining(now).is_some()
                || now.saturating_duration_since(f.last_supersede) <= f.window
        });
        before - self.seat_flaps.len()
    }

    // ── Session / membership resolution ───────────────────────────────────────

    /// True if `token` is a currently registered session (any number of rooms,
    /// including zero — a fresh roomless `connect`).
    pub fn session_exists(&self, token: &SessionToken) -> bool {
        self.sessions
            .get_key_value(token)
            .map(|(stored, _)| ct_str_eq(stored.as_str(), token.as_str()))
            .unwrap_or(false)
    }

    /// All `(room_token, member_name)` pairs a session belongs to, or `None` if
    /// the session token itself does not resolve.
    pub fn session_memberships(
        &self,
        token: &SessionToken,
    ) -> Option<&HashSet<(RoomToken, MemberName)>> {
        self.sessions
            .get_key_value(token)
            .filter(|(stored, _)| ct_str_eq(stored.as_str(), token.as_str()))
            .map(|(_, set)| set)
    }

    /// The session's shared inbox, or `None` if the session token does not
    /// resolve. Works regardless of how many (if any) rooms the session belongs
    /// to — a fresh roomless session already has one, minted at `connect`.
    pub fn session_inbox(&self, token: &SessionToken) -> Option<Arc<Inbox>> {
        self.session_inboxes
            .get_key_value(token)
            .filter(|(stored, _)| ct_str_eq(stored.as_str(), token.as_str()))
            .map(|(_, inbox)| inbox.clone())
    }

    /// Validate a session token in isolation (no particular room in mind) —
    /// consumes a one-time `Superseded` notice if pending, otherwise confirms the
    /// token resolves to a live session. Used by `receive`'s room-omitted path and
    /// by `join`'s up-front session check.
    pub fn check_session(&mut self, session_token: &SessionToken) -> Result<(), Error> {
        if self.superseded_sessions.remove(session_token) {
            return Err(Error::Superseded);
        }
        if self.session_exists(session_token) {
            Ok(())
        } else {
            Err(Error::InvalidSessionToken)
        }
    }

    /// Resolve whether `session_token` currently holds a membership in
    /// `room_token`. Consumes a one-time `Superseded` notice if pending. Does not
    /// borrow `self.rooms` — callers needing the `RoomState` too should follow up
    /// with a separate `self.rooms.get_mut(room_token)` (see
    /// [`AppState::room_membership_mut`]).
    pub fn check_room_membership(
        &mut self,
        session_token: &SessionToken,
        room_token: &str,
    ) -> MembershipCheck {
        if self.superseded_sessions.remove(session_token) {
            return MembershipCheck::Superseded;
        }
        match self.session_memberships(session_token) {
            None => MembershipCheck::InvalidSession,
            Some(set) => match set.iter().find(|(rt, _)| rt.as_str() == room_token) {
                Some((_, member_name)) => MembershipCheck::Member(member_name.clone()),
                None => MembershipCheck::NotMember,
            },
        }
    }

    /// Validate that `session_token` belongs to `room_token` and return both the
    /// resolved member name and a mutable reference to that room's `RoomState`.
    /// `room_name` is used only to build a friendly [`Error::NotConnectedToRoom`]
    /// — state.rs deals in tokens, so the caller (who resolved the name from the
    /// store in the first place) supplies it for the error message.
    ///
    /// Replaces the old single-room `room_for_session_mut` now that a session can
    /// belong to more than one room (`send`, `members`).
    pub fn room_membership_mut(
        &mut self,
        session_token: &SessionToken,
        room_token: &str,
        room_name: &str,
    ) -> Result<(MemberName, &mut RoomState), Error> {
        match self.check_room_membership(session_token, room_token) {
            MembershipCheck::Superseded => Err(Error::Superseded),
            MembershipCheck::InvalidSession => Err(Error::InvalidSessionToken),
            MembershipCheck::NotMember => Err(Error::NotConnectedToRoom(room_name.to_string())),
            MembershipCheck::Member(member_name) => {
                let room_state = self
                    .rooms
                    .get_mut(room_token)
                    .ok_or_else(|| Error::RoomNotFound(room_token.to_string()))?;
                Ok((member_name, room_state))
            }
        }
    }

    /// Refresh one room membership's `last_seen` to now. No-op if the session
    /// does not belong to that room. Used to re-anchor the idle window at the
    /// **completion** of an activity that touched specifically this room (`send`,
    /// `members`, or a room-*filtered* `receive`/send-with-reply reply-wait) —
    /// see [`AppState::touch_all_memberships`] for the room-omitted `receive` case.
    pub fn touch_membership(&mut self, session_token: &SessionToken, room_token: &str) {
        let Some(set) = self.sessions.get(session_token) else {
            return;
        };
        let Some((_, member_name)) = set.iter().find(|(rt, _)| rt.as_str() == room_token) else {
            return;
        };
        let member_name = member_name.clone();
        if let Some(member) = self
            .rooms
            .get_mut(room_token)
            .and_then(|rs| rs.members.get_mut(&member_name))
        {
            member.last_seen = Instant::now();
        }
    }

    /// Refresh **every** room membership's `last_seen` for a session. Used only
    /// by a room-*omitted* `receive`: parking on the shared inbox blanket-exempts
    /// every room the session belongs to from the idle sweep for the duration of
    /// the wait (`Inbox::is_receive_active()` is one atomic shared by the whole
    /// session, not per room), so the re-anchor at completion must symmetrically
    /// cover every room — otherwise a room that happened to receive no traffic
    /// during a long wait would read as instantly idle-reapable the moment the
    /// wait ends, even though the agent was genuinely parked and alive the whole
    /// time (the same trap TDD 10.19 already guards against for the single-room
    /// case). This does not violate per-room idle-TTL granularity: each room's
    /// `last_seen` field is still tracked independently — a room-*filtered*
    /// receive still only touches its one target room (see
    /// [`AppState::touch_membership`]) — this is specific to the room-omitted
    /// call shape, which by definition represents activity across every room it
    /// was capable of waking for.
    pub fn touch_all_memberships(&mut self, session_token: &SessionToken) {
        let Some(set) = self.sessions.get(session_token).cloned() else {
            return;
        };
        let now = Instant::now();
        for (room_token, member_name) in &set {
            if let Some(member) = self
                .rooms
                .get_mut(room_token.as_str())
                .and_then(|rs| rs.members.get_mut(member_name))
            {
                member.last_seen = now;
            }
        }
    }

    /// Build a read-only, redacted diagnostic snapshot of all live state
    /// (TDD section 13). `room_names` maps room tokens to display names so the
    /// dump can show room names without ever emitting room tokens; a room present
    /// here but absent from `room_names` is itself flagged as an invariant
    /// violation (orphaned room). Mutates nothing.
    pub async fn diagnostic_snapshot(
        &self,
        now: Instant,
        room_names: &HashMap<RoomToken, String>,
        member_idle_ttl_secs: u64,
        seat_idle_ttl_secs: u64,
    ) -> DiagnosticSnapshot {
        let mut rooms = Vec::new();
        let mut invariants = Vec::new();

        // Deterministic ordering for stable, diffable output.
        let mut room_entries: Vec<(&RoomToken, &RoomState)> = self.rooms.iter().collect();
        room_entries.sort_by(|a, b| room_names.get(a.0).cmp(&room_names.get(b.0)));

        for (room_token, room_state) in &room_entries {
            let room_label = room_names.get(*room_token).cloned();
            let display_name =
                room_label.unwrap_or_else(|| "<orphaned-room (not in store)>".to_string());
            if !room_names.contains_key(*room_token) {
                invariants.push(format!(
                    "ERROR: room '{display_name}' is registered in app state but has no entry \
                     in the room store (orphaned room)"
                ));
            }

            let mut members: Vec<(&MemberName, &MemberState)> = room_state.members.iter().collect();
            members.sort_by(|a, b| a.0.cmp(b.0));

            let mut member_snaps = Vec::new();
            for (member_name, member) in members {
                let session_in_map = self
                    .sessions
                    .get(&member.session_token)
                    .map(|set| set.contains(&((*room_token).clone(), member_name.clone())))
                    .unwrap_or(false);
                if !session_in_map {
                    invariants.push(format!(
                        "ERROR: member '{member_name}' in room '{display_name}' has a session \
                         token that does not round-trip through the session map"
                    ));
                }

                let last_seen_secs = now.saturating_duration_since(member.last_seen).as_secs();
                // Countdown to idle eviction. Room-token members always count down;
                // seat members count down against the dead-seat reaper when it is
                // enabled (`seat_idle_ttl_secs > 0`) and show `exempt` only when it is
                // disabled — so a reapable seat is never mislabelled "exempt" in the
                // dump. A negative remainder (already past the deadline) saturates to
                // 0 — reaped on the next sweep.
                let (auth_method, idle_countdown_secs) = match member.auth {
                    AuthMethod::Seat => (
                        "seat",
                        (seat_idle_ttl_secs > 0)
                            .then(|| seat_idle_ttl_secs.saturating_sub(last_seen_secs)),
                    ),
                    AuthMethod::RoomToken => (
                        "room_token",
                        Some(member_idle_ttl_secs.saturating_sub(last_seen_secs)),
                    ),
                };

                member_snaps.push(MemberSnapshot {
                    member_name: member_name.clone(),
                    session_prefix: session_id(&member.session_token).to_string(),
                    session_in_map,
                    last_seen_secs,
                    registered_secs_ago: now
                        .saturating_duration_since(member.registered_at)
                        .as_secs(),
                    auth_method,
                    idle_countdown_secs,
                    seat_contention: (member.auth == AuthMethod::Seat).then(|| {
                        let flap = self
                            .seat_flaps
                            .get(&((*room_token).clone(), member_name.clone()));
                        SeatContentionSnapshot {
                            recent_supersedes: flap.map_or(0, |f| f.recent_count(now)),
                            last_supersede_secs_ago: flap
                                .map(|f| now.saturating_duration_since(f.last_supersede).as_secs()),
                            frozen_secs_remaining: flap
                                .and_then(|f| f.frozen_remaining(now))
                                // Round up so a live freeze never displays as 0.
                                .map(|d| d.as_secs() + u64::from(d.subsec_nanos() > 0)),
                        }
                    }),
                });
            }

            rooms.push(RoomSnapshot {
                name: display_name,
                member_count: room_state.members.len(),
                members: member_snaps,
            });
        }

        // ── Sessions: shared inbox rendered once per session (TDD 13.11) ───────
        let mut sessions = Vec::new();
        let mut session_tokens: Vec<&SessionToken> = self.sessions.keys().collect();
        session_tokens.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        for session_token in session_tokens {
            let memberships_set = &self.sessions[session_token];
            let mut memberships: Vec<String> = memberships_set
                .iter()
                .map(|(room_token, member_name)| {
                    let room_label = room_names
                        .get(room_token)
                        .cloned()
                        .unwrap_or_else(|| "<orphaned-room>".to_string());
                    format!("{room_label}/{member_name}")
                })
                .collect();
            memberships.sort();

            let Some(inbox) = self.session_inboxes.get(session_token) else {
                invariants.push(format!(
                    "ERROR: session '{}' is registered in the session map but has no shared \
                     inbox in session_inboxes (dangling session)",
                    session_id(session_token)
                ));
                continue;
            };

            let connected_secs_ago = match self.session_connected_at.get(session_token) {
                Some(connected_at) => now.saturating_duration_since(*connected_at).as_secs(),
                None => {
                    invariants.push(format!(
                        "ERROR: session '{}' is registered in the session map but has no \
                         connected_at timestamp (dangling session)",
                        session_id(session_token)
                    ));
                    0
                }
            };

            // Every (room_token, member_name) pair claimed for this session must
            // resolve to a live MemberState whose own session_token matches — the
            // desync class behind "listening but nothing arrives".
            for (room_token, member_name) in memberships_set {
                let resolves = self
                    .rooms
                    .get(room_token)
                    .and_then(|rs| rs.members.get(member_name))
                    .map(|m| ct_str_eq(m.session_token.as_str(), session_token.as_str()))
                    .unwrap_or(false);
                if !resolves {
                    invariants.push(format!(
                        "ERROR: session map contains an entry for member '{member_name}' that does \
                         not resolve to a live member (dangling session)"
                    ));
                }
            }

            let strong_count = Arc::strong_count(inbox);
            let snap = inbox.snapshot(now, strong_count, room_names).await;

            // Structural baseline: `session_inboxes` holds one clone, and every
            // room membership sharing this inbox (`MemberState.inbox`) holds
            // another — a session with two rooms legitimately shows strong_count
            // 3 with nobody parked and nothing orphaned.
            let baseline = 1 + memberships_set.len();

            if snap.receive_active && snap.arc_strong_count <= baseline {
                invariants.push(format!(
                    "WARN: session '{}' has an active receive slot (active_gen={}) but inbox \
                     strong_count={} (baseline {baseline}) — no live receiver is parked; likely \
                     an orphaned/stuck receive",
                    session_id(session_token),
                    snap.active_gen,
                    snap.arc_strong_count
                ));
            }
            // Extra Arc holders beyond the structural baseline, with no active
            // receive slot, are only suspicious when NO in-flight send-with-reply
            // targets this session — a live sender legitimately holds
            // recipient-inbox clones (MT-3). With reply_waiters == 0 the holders
            // are unexplained: a genuine orphaned-frame leak (self-clears at the
            // frame's deadline).
            if !snap.receive_active && snap.arc_strong_count > baseline && snap.reply_waiters == 0 {
                invariants.push(format!(
                    "WARN: session '{}' has no active receive slot and no in-flight \
                     send-with-reply, yet its inbox has {} extra Arc holder(s) beyond the \
                     structural baseline of {baseline} (strong_count={}) — an orphaned frame is \
                     holding an inbox clone (bounded by its park deadline; self-clears on expiry \
                     or eviction). Persistent across dumps indicates a dead session — reap it",
                    session_id(session_token),
                    snap.arc_strong_count - baseline,
                    snap.arc_strong_count
                ));
            }

            sessions.push(SessionSnapshot {
                session_prefix: session_id(session_token).to_string(),
                connected_secs_ago,
                memberships,
                inbox: snap,
            });
        }

        let mut superseded_session_prefixes: Vec<String> = self
            .superseded_sessions
            .iter()
            .map(|s| session_id(s).to_string())
            .collect();
        superseded_session_prefixes.sort();

        if invariants.is_empty() {
            invariants.push("OK: all invariants satisfied".to_string());
        }

        DiagnosticSnapshot {
            rooms,
            sessions,
            session_map_size: self.sessions.len(),
            superseded_session_prefixes,
            invariants,
        }
    }
}

#[cfg(test)]
mod newtype_tests {
    use super::*;

    const FULL: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    // TDD 11.2: member name validation, enforced at construction.
    #[test]
    fn member_name_new_accepts_alnum_hyphen_underscore() {
        assert!(MemberName::new("qa-agent").is_ok());
        assert!(MemberName::new("dev_01").is_ok());
    }

    #[test]
    fn member_name_new_rejects_space() {
        assert!(matches!(
            MemberName::new("my name"),
            Err(Error::InvalidMemberName(n)) if n == "my name"
        ));
    }

    #[test]
    fn member_name_new_rejects_empty() {
        assert!(matches!(
            MemberName::new(""),
            Err(Error::InvalidMemberName(_))
        ));
    }

    #[test]
    fn member_name_new_rejects_too_long() {
        assert!(matches!(
            MemberName::new("a".repeat(41)),
            Err(Error::InvalidMemberName(_))
        ));
    }

    #[test]
    fn session_token_display_shows_only_redacted_prefix() {
        let token = SessionToken::from(FULL);
        assert_eq!(token.to_string(), "01234567");
    }

    #[test]
    fn session_token_debug_does_not_leak_full_value() {
        let rendered = format!("{:?}", SessionToken::from(FULL));
        assert_eq!(rendered, "SessionToken(01234567…)");
    }

    #[test]
    fn session_token_redaction_survives_short_and_non_ascii_input() {
        // Caller-supplied tokens are arbitrary strings; redaction must not panic
        // on a short one or slice through a multi-byte character.
        assert_eq!(SessionToken::from("abc").to_string(), "abc");
        assert_eq!(SessionToken::from("€€€€").to_string(), "€€");
    }

    #[test]
    fn session_token_as_str_exposes_full_value() {
        assert_eq!(SessionToken::from(FULL).as_str(), FULL);
    }

    #[test]
    fn member_name_serde_is_transparent_string() {
        let name = MemberName::new("alice").unwrap();
        let yaml = serde_yml::to_string(&name).unwrap();
        assert_eq!(yaml, serde_yml::to_string("alice").unwrap());
        let back: MemberName = serde_yml::from_str(&yaml).unwrap();
        assert_eq!(back, name);
    }

    #[test]
    fn member_name_display_is_the_plain_name() {
        assert_eq!(MemberName::new("alice").unwrap().to_string(), "alice");
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use crate::test_support::mn;

    fn rt(s: &str) -> RoomToken {
        RoomToken::from(s.to_string())
    }

    fn names(pairs: &[(&str, &str)]) -> HashMap<RoomToken, String> {
        pairs
            .iter()
            .map(|(tok, name)| (rt(tok), name.to_string()))
            .collect()
    }

    fn msg(room: RoomToken) -> QueuedMessage {
        let now = Instant::now();
        QueuedMessage {
            id: Uuid::new_v4(),
            from: mn("bob"),
            room,
            body: "hi".into(),
            enqueued_at: now,
            expires_at: now + Duration::from_secs(300),
            reply_deadline: None,
        }
    }

    /// Register a session with one room membership using a fresh shared inbox
    /// seeded with `messages`, mirroring `connect` + `join`.
    async fn register_one_room(
        app: &mut AppState,
        room_token: &str,
        member: &str,
        session_token: &str,
        now: Instant,
        auth: AuthMethod,
        messages: Vec<QueuedMessage>,
    ) {
        let session_token = SessionToken::from(session_token);
        let inbox = app.register_session(session_token.clone());
        inbox.seed(messages).await;
        app.register_room_membership(rt(room_token), mn(member), &session_token, now, auth);
    }

    /// TDD 13.2/13.3/13.4/13.5: the snapshot includes every room, member, inbox
    /// field, and queued message — and redacts the session token (13.6).
    #[tokio::test]
    async fn snapshot_includes_all_state_and_redacts_secrets() {
        let mut app = AppState::default();
        let now = Instant::now();
        register_one_room(
            &mut app,
            "room-token-AAAA",
            "alice",
            "session-secret-token-1234567890",
            now,
            AuthMethod::RoomToken,
            vec![msg(rt("room-token-AAAA"))],
        )
        .await;

        let name_map = names(&[("room-token-AAAA", "myroom")]);
        let snap = app.diagnostic_snapshot(now, &name_map, 1800, 0).await;

        assert_eq!(snap.rooms.len(), 1);
        let room = &snap.rooms[0];
        assert_eq!(room.name, "myroom");
        assert_eq!(room.member_count, 1);
        assert_eq!(room.members.len(), 1);

        let m = &room.members[0];
        assert_eq!(m.member_name, "alice");
        // Redacted to the 8-char session_id prefix — never the full token.
        assert_eq!(m.session_prefix, "session-");
        assert!(m.session_in_map);

        // Inbox is rendered once, at the session level (TDD 13.11).
        assert_eq!(snap.sessions.len(), 1);
        let s = &snap.sessions[0];
        assert_eq!(s.session_prefix, "session-");
        assert_eq!(s.memberships, vec!["myroom/alice".to_string()]);
        let ib = &s.inbox;
        assert_eq!(ib.queue_depth, 1);
        assert_eq!(ib.messages.len(), 1);
        assert_eq!(ib.messages[0].from, "bob");
        assert_eq!(ib.messages[0].room, "myroom");
        assert!(ib.messages[0].ttl_remaining_secs <= 300);
        assert_eq!(ib.messages[0].id_prefix.len(), 8);
        assert!(!ib.receive_active);
        assert_eq!(ib.active_gen, 0);

        assert_eq!(snap.session_map_size, 1);
        // Healthy snapshot is clearly distinguishable (TDD 13.7).
        assert!(snap.invariants.iter().any(|i| i.starts_with("OK")));
    }

    /// TDD 13.11: a session belonging to two rooms renders its shared inbox once,
    /// with each room row showing only its own per-room membership metadata.
    #[tokio::test]
    async fn snapshot_groups_shared_inbox_once_per_session() {
        let mut app = AppState::default();
        let now = Instant::now();
        let session = &SessionToken::from("shared-session-token");

        let inbox = app.register_session(session.clone());
        inbox.seed(vec![msg(rt("rt-a")), msg(rt("rt-b"))]).await;
        app.register_room_membership(rt("rt-a"), mn("alice"), session, now, AuthMethod::RoomToken);
        app.register_room_membership(
            rt("rt-b"),
            mn("alice2"),
            session,
            now,
            AuthMethod::RoomToken,
        );

        let name_map = names(&[("rt-a", "room-a"), ("rt-b", "room-b")]);
        let snap = app.diagnostic_snapshot(now, &name_map, 1800, 0).await;

        assert_eq!(snap.rooms.len(), 2);
        // Only one session-level entry exists, not one per room.
        assert_eq!(snap.sessions.len(), 1);
        let s = &snap.sessions[0];
        assert_eq!(
            s.memberships,
            vec!["room-a/alice".to_string(), "room-b/alice2".to_string()]
        );
        // Both messages (from different rooms) show up exactly once, tagged.
        assert_eq!(s.inbox.messages.len(), 2);
        let rooms_seen: HashSet<&str> = s.inbox.messages.iter().map(|m| m.room.as_str()).collect();
        assert!(rooms_seen.contains("room-a"));
        assert!(rooms_seen.contains("room-b"));
    }

    /// TDD 13.7: an active receive slot with no live receiver parked is flagged.
    #[tokio::test]
    async fn snapshot_flags_orphaned_receive_slot() {
        let mut app = AppState::default();
        let now = Instant::now();
        let session = &SessionToken::from("sess-aaaa");
        let inbox = app.register_session(session.clone());
        inbox.claim_receive(); // active_gen set, but no receiver Arc clone exists
        app.register_room_membership(rt("rt"), mn("alice"), session, now, AuthMethod::RoomToken);
        // Drop the constructor's own returned handle — only the structural
        // baseline (session_inboxes + this one MemberState) should remain, same
        // as no live parker holding an extra clone.
        drop(inbox);

        let snap = app
            .diagnostic_snapshot(now, &names(&[("rt", "room")]), 1800, 0)
            .await;

        let s = &snap.sessions[0];
        assert!(s.inbox.receive_active);
        assert_eq!(s.inbox.arc_strong_count, 2); // baseline: session_inboxes + 1 membership
        assert!(
            snap.invariants
                .iter()
                .any(|i| i.starts_with("WARN") && i.contains("orphaned")),
            "expected an orphaned-receive warning, got {:?}",
            snap.invariants
        );
    }

    /// TDD 13.7: a session-map entry with no matching live member is flagged.
    #[tokio::test]
    async fn snapshot_flags_dangling_session() {
        let mut app = AppState::default();
        app.sessions.insert(
            "ghost-session".into(),
            HashSet::from([(rt("rt"), mn("nobody"))]),
        );
        app.session_inboxes
            .insert("ghost-session".into(), Inbox::new());

        let snap = app
            .diagnostic_snapshot(Instant::now(), &HashMap::new(), 1800, 0)
            .await;

        assert_eq!(snap.session_map_size, 1);
        assert!(
            snap.invariants
                .iter()
                .any(|i| i.starts_with("ERROR") && i.contains("dangling")),
            "expected a dangling-session error, got {:?}",
            snap.invariants
        );
    }

    /// TDD 13.7: an inbox with extra Arc holders, no active receive slot, AND no
    /// in-flight send-with-reply (reply_waiters == 0) is a genuine orphaned-frame
    /// leak — the inverse of the orphaned-slot check — and is flagged.
    #[tokio::test]
    async fn snapshot_flags_orphaned_parkers_without_active_slot() {
        let mut app = AppState::default();
        let now = Instant::now();
        let session = &SessionToken::from("sess");
        let inbox = app.register_session(session.clone());
        // Simulate orphaned frames holding extra Arc clones with no active slot and
        // no in-flight sender to explain them.
        let _orphan_a = inbox.clone();
        let _orphan_b = inbox.clone();
        app.register_room_membership(rt("rt"), mn("alice"), session, now, AuthMethod::RoomToken);
        // Drop the constructor's own returned handle so only the two orphans
        // sit beyond the structural baseline (session_inboxes + MemberState).
        drop(inbox);

        let snap = app
            .diagnostic_snapshot(now, &names(&[("rt", "room")]), 1800, 0)
            .await;

        let s = &snap.sessions[0];
        assert!(!s.inbox.receive_active);
        assert_eq!(s.inbox.arc_strong_count, 4); // session_inboxes + MemberState + 2 orphans
        assert_eq!(s.inbox.reply_waiters, 0);
        assert!(
            snap.invariants
                .iter()
                .any(|i| i.starts_with("WARN") && i.contains("orphaned frame")),
            "expected an orphaned-frame warning, got {:?}",
            snap.invariants
        );
    }

    /// MT-3: a *healthy* in-flight send-with-reply legitimately holds extra Arc
    /// clones of the recipient inbox while `receive_active` is false. With
    /// `reply_waiters > 0` explaining them, the diagnostic must NOT cry wolf.
    #[tokio::test]
    async fn snapshot_does_not_flag_live_send_with_reply() {
        let mut app = AppState::default();
        let now = Instant::now();
        let session = &SessionToken::from("sess");
        let inbox = app.register_session(session.clone());
        // A live send-with-reply: extra clones AND a counted reply-waiter.
        let _sender_clone_a = inbox.clone();
        let _sender_clone_b = inbox.clone();
        let _guard = ReplyWaitGuard::new(inbox.clone());
        app.register_room_membership(rt("rt"), mn("alice"), session, now, AuthMethod::RoomToken);

        let snap = app
            .diagnostic_snapshot(now, &names(&[("rt", "room")]), 1800, 0)
            .await;

        let s = &snap.sessions[0];
        assert!(!s.inbox.receive_active);
        assert!(s.inbox.arc_strong_count > 1);
        assert!(s.inbox.reply_waiters >= 1);
        assert!(
            !snap
                .invariants
                .iter()
                .any(|i| i.starts_with("WARN") && i.contains("orphaned frame")),
            "healthy in-flight send-with-reply must NOT be flagged, got {:?}",
            snap.invariants
        );
    }

    /// A seat member's dump entry reflects the dead-seat reaper: a countdown when the
    /// reaper is enabled (it is mortal, not exempt), and `exempt` (None) when disabled.
    #[tokio::test]
    async fn snapshot_seat_countdown_tracks_reaper() {
        let mut app = AppState::default();
        let now = Instant::now();
        let session = &SessionToken::from("sess");
        app.register_session(session.clone());
        app.register_room_membership(rt("rt"), mn("seatholder"), session, now, AuthMethod::Seat);

        // Reaper enabled (3600s) → seat shows a countdown (mortal), not exempt.
        let snap = app
            .diagnostic_snapshot(now, &names(&[("rt", "room")]), 1800, 3600)
            .await;
        assert_eq!(snap.rooms[0].members[0].idle_countdown_secs, Some(3600));

        // Reaper disabled (0) → seat shows exempt (None).
        let snap = app
            .diagnostic_snapshot(now, &names(&[("rt", "room")]), 1800, 0)
            .await;
        assert_eq!(snap.rooms[0].members[0].idle_countdown_secs, None);
    }

    /// TDD 13.5/13.6: superseded sessions are reported, redacted to a prefix.
    #[tokio::test]
    async fn snapshot_reports_redacted_superseded_sessions() {
        let mut app = AppState::default();
        app.superseded_sessions
            .insert("superseded-token-xyz".into());

        let snap = app
            .diagnostic_snapshot(Instant::now(), &HashMap::new(), 1800, 0)
            .await;

        assert_eq!(
            snap.superseded_session_prefixes,
            vec!["supersed".to_string()]
        );
    }
}

#[cfg(test)]
mod eviction_tests {
    use super::*;
    use crate::test_support::mn;
    use crate::tools::receive::{park_on_inbox, ParkOutcome};

    fn rt(s: &str) -> RoomToken {
        RoomToken::from(s.to_string())
    }

    /// Spawn a long-deadline `park_on_inbox` on `inbox` and give it time to claim
    /// the slot and install its abort handle. The returned task must be aborted by
    /// eviction, not by its (1-hour) deadline.
    async fn park(inbox: Arc<Inbox>) -> tokio::task::JoinHandle<Result<ParkOutcome, Error>> {
        let handle = tokio::spawn(async move {
            park_on_inbox(
                inbox,
                Instant::now() + Duration::from_secs(3600),
                "alice",
                usize::MAX,
                |_| true,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await; // let it claim + park
        handle
    }

    /// Assert the parked receive returned well under its 3600 s deadline because it
    /// was aborted (preempted) by eviction — the core of TDD 10.10 / 12.2.
    async fn assert_aborted_promptly(handle: tokio::task::JoinHandle<Result<ParkOutcome, Error>>) {
        let outcome = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("parked receive did not return promptly after eviction")
            .expect("park task panicked")
            .expect("park_on_inbox returned an error");
        assert!(
            outcome.preempted,
            "eviction must abort the in-flight receive (preempted=true)"
        );
        assert!(outcome.messages.is_empty());
    }

    /// TDD 10.10: a CLI `kckm` aborts the member's in-flight blocking receive so it
    /// returns promptly instead of parking until its timeout elapses — the member's
    /// only (last) room membership, so the whole session (and its inbox) is torn down.
    #[tokio::test]
    async fn evict_member_aborts_in_flight_receive() {
        let mut app = AppState::default();
        let inbox = app.register_session("sess".into());
        let handle = park(inbox.clone()).await;
        app.register_room_membership(
            rt("rt"),
            mn("alice"),
            &"sess".into(),
            Instant::now(),
            AuthMethod::RoomToken,
        );

        assert!(
            app.evict_member("rt", "room", &mn("alice"), "kicked by operator")
                .await
        );
        assert_aborted_promptly(handle).await;
    }

    /// TDD 10.10: a CLI `kckall` (and the shared `evict_all_members` path) aborts
    /// each member's in-flight receive.
    #[tokio::test]
    async fn evict_all_members_aborts_in_flight_receive() {
        let mut app = AppState::default();
        let inbox = app.register_session("sess".into());
        let handle = park(inbox.clone()).await;
        app.register_room_membership(
            rt("rt"),
            mn("alice"),
            &"sess".into(),
            Instant::now(),
            AuthMethod::RoomToken,
        );

        assert_eq!(app.evict_all_members("rt", "room", "room kicked").await, 1);
        assert_aborted_promptly(handle).await;
    }

    /// TDD 12.2 (And-clause): a seat-holder join that supersedes the prior
    /// session aborts that member's in-flight receive so it returns promptly (the
    /// evicted room was that session's only membership).
    #[tokio::test]
    async fn evict_member_superseded_aborts_in_flight_receive() {
        let mut app = AppState::default();
        let inbox = app.register_session("sess".into());
        let handle = park(inbox.clone()).await;
        app.register_room_membership(
            rt("rt"),
            mn("alice"),
            &"sess".into(),
            Instant::now(),
            AuthMethod::RoomToken,
        );

        app.evict_member_superseded("rt", "room", &mn("alice"))
            .await;
        assert_aborted_promptly(handle).await;
    }

    /// TDD 10.21/10.22: evicting one room membership never disturbs the same
    /// session's other, still-joined room — the shared inbox is not aborted or
    /// closed, and a receive parked on it (room-omitted, so it can still be
    /// satisfied by the surviving room) is left running.
    #[tokio::test]
    async fn evict_one_room_leaves_other_room_and_shared_inbox_untouched() {
        let mut app = AppState::default();
        let inbox = app.register_session("sess".into());
        app.register_room_membership(
            rt("room-a"),
            mn("alice"),
            &"sess".into(),
            Instant::now(),
            AuthMethod::RoomToken,
        );
        app.register_room_membership(
            rt("room-b"),
            mn("alice2"),
            &"sess".into(),
            Instant::now(),
            AuthMethod::RoomToken,
        );

        assert!(
            app.evict_member("room-a", "room-a", &mn("alice"), "kicked")
                .await
        );

        // The session survives via room-b.
        assert!(app.session_exists(&"sess".into()));
        assert!(app
            .session_memberships(&"sess".into())
            .unwrap()
            .contains(&(rt("room-b"), mn("alice2"))));
        assert!(!app
            .session_memberships(&"sess".into())
            .unwrap()
            .contains(&(rt("room-a"), mn("alice"))));
        // The shared inbox is untouched (not closed) — room-b's traffic still flows.
        assert!(!inbox.is_closed());
    }

    /// TDD 10.10b: evicting one room membership does not abort an in-flight
    /// receive parked on the shared inbox, as long as the session still holds
    /// another room membership — only an eviction that empties the session's
    /// *last* membership aborts a parked wait (10.10, tested above).
    #[tokio::test]
    async fn evict_one_room_does_not_abort_receive_parked_on_surviving_room() {
        let mut app = AppState::default();
        let inbox = app.register_session("sess".into());
        app.register_room_membership(
            rt("room-a"),
            mn("alice"),
            &"sess".into(),
            Instant::now(),
            AuthMethod::RoomToken,
        );
        app.register_room_membership(
            rt("room-b"),
            mn("alice2"),
            &"sess".into(),
            Instant::now(),
            AuthMethod::RoomToken,
        );

        // A short deadline (rather than park()'s 3600s) so the test can observe a
        // genuine natural timeout instead of waiting an hour.
        let handle = tokio::spawn({
            let inbox = inbox.clone();
            async move {
                park_on_inbox(
                    inbox,
                    Instant::now() + Duration::from_millis(200),
                    "alice",
                    usize::MAX,
                    |_| true,
                )
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await; // let it claim + park

        // Evict room-a — the room the parked receive's own member belongs to —
        // while room-b survives under the same session.
        assert!(
            app.evict_member("room-a", "room-a", &mn("alice"), "kicked")
                .await
        );

        // The wait must NOT be preempted by this partial eviction: it keeps running
        // and returns only once its own (short) deadline elapses.
        let outcome = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("parked receive did not return within its own deadline")
            .expect("park task panicked")
            .expect("park_on_inbox returned an error");
        assert!(
            !outcome.preempted,
            "a partial (non-last-membership) eviction must not abort an in-flight wait"
        );
        assert!(outcome.messages.is_empty());
    }

    /// TDD 10.16 applied per-room: a message still queued and tagged for the room
    /// being evicted is discarded, but a message tagged for the session's other,
    /// surviving room is left queued.
    #[tokio::test]
    async fn evict_one_room_discards_only_that_rooms_queued_messages() {
        let mut app = AppState::default();
        let inbox = app.register_session("sess".into());
        let now = Instant::now();
        inbox
            .seed(vec![
                QueuedMessage {
                    id: Uuid::new_v4(),
                    from: mn("bob"),
                    room: rt("room-a"),
                    body: "for a".into(),
                    enqueued_at: now,
                    expires_at: now + Duration::from_secs(300),
                    reply_deadline: None,
                },
                QueuedMessage {
                    id: Uuid::new_v4(),
                    from: mn("bob"),
                    room: rt("room-b"),
                    body: "for b".into(),
                    enqueued_at: now,
                    expires_at: now + Duration::from_secs(300),
                    reply_deadline: None,
                },
            ])
            .await;
        app.register_room_membership(
            rt("room-a"),
            mn("alice"),
            &"sess".into(),
            now,
            AuthMethod::RoomToken,
        );
        app.register_room_membership(
            rt("room-b"),
            mn("alice2"),
            &"sess".into(),
            now,
            AuthMethod::RoomToken,
        );

        assert!(
            app.evict_member("room-a", "room-a", &mn("alice"), "kicked")
                .await
        );

        let remaining = inbox.drain_valid(Instant::now()).await;
        assert_eq!(remaining.len(), 1, "only room-b's message should survive");
        assert_eq!(remaining[0].body, "for b");
    }
}

#[cfg(test)]
mod inbox_close_tests {
    use super::*;
    use crate::test_support::mn;

    fn msg() -> QueuedMessage {
        let now = Instant::now();
        QueuedMessage {
            id: Uuid::new_v4(),
            from: mn("bob"),
            room: RoomToken::from("rt".to_string()),
            body: "hi".into(),
            enqueued_at: now,
            expires_at: now + Duration::from_secs(60),
            reply_deadline: None,
        }
    }

    /// A push to a live inbox succeeds; once closed, every push fails-closed so the
    /// sender is never told a message reached an evicted recipient.
    #[tokio::test]
    async fn try_push_fails_closed_after_close() {
        let inbox = Inbox::new();
        assert_eq!(inbox.try_push(msg(), 100).await, PushOutcome::Pushed);

        inbox.close().await;
        assert!(inbox.is_closed());
        assert_eq!(inbox.try_push(msg(), 100).await, PushOutcome::Closed);
    }

    /// A full (but still-live) inbox reports `Full`, not `Closed` — the two
    /// rejection reasons are distinct so `send` can map them to different errors.
    #[tokio::test]
    async fn try_push_reports_full_distinct_from_closed() {
        let inbox = Inbox::new();
        assert_eq!(inbox.try_push(msg(), 1).await, PushOutcome::Pushed);
        assert_eq!(inbox.try_push(msg(), 1).await, PushOutcome::Full);
    }

    /// `closed()` resolves immediately when the inbox is already closed and
    /// promptly when it is closed while a waiter is parked — the signal a
    /// send-with-reply sender uses to abandon its reply-wait.
    #[tokio::test]
    async fn closed_resolves_immediately_and_on_close() {
        let inbox = Inbox::new();
        inbox.close().await;
        // Already closed: returns at once.
        tokio::time::timeout(Duration::from_secs(1), inbox.closed())
            .await
            .expect("closed() must return immediately when already closed");

        // Closed while parked: a concurrent close wakes the waiter promptly.
        let fresh = Inbox::new();
        let watcher = {
            let fresh = fresh.clone();
            tokio::spawn(async move { fresh.closed().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        fresh.close().await;
        tokio::time::timeout(Duration::from_secs(1), watcher)
            .await
            .expect("closed() must resolve promptly when the inbox is closed")
            .expect("watcher task panicked");
    }

    /// `evict_one` (via `evict_member`) closes the evicted session's inbox when
    /// the room evicted was its last membership.
    #[tokio::test]
    async fn evict_member_closes_inbox_when_last_membership() {
        let mut app = AppState::default();
        let inbox = app.register_session("sess".into());
        app.register_room_membership(
            RoomToken::from("rt".to_string()),
            mn("alice"),
            &"sess".into(),
            Instant::now(),
            AuthMethod::RoomToken,
        );

        assert!(!inbox.is_closed());
        assert!(app.evict_member("rt", "room", &mn("alice"), "kicked").await);
        assert!(inbox.is_closed(), "eviction must close the member's inbox");
        assert_eq!(inbox.try_push(msg(), 100).await, PushOutcome::Closed);
    }
}

#[cfg(test)]
mod drain_cap_tests {
    use super::*;
    use crate::test_support::mn;

    fn msg_at(room: &str, body: &str, ttl: Duration) -> QueuedMessage {
        let now = Instant::now();
        QueuedMessage {
            id: Uuid::new_v4(),
            from: mn("bob"),
            room: RoomToken::from(room.to_string()),
            body: body.into(),
            enqueued_at: now,
            expires_at: now + ttl,
            reply_deadline: None,
        }
    }

    fn bodies(d: &Drained) -> Vec<&str> {
        d.messages.iter().map(|m| m.body.as_str()).collect()
    }

    async fn queued_bodies(inbox: &Inbox) -> Vec<String> {
        inbox
            .queue
            .lock()
            .await
            .iter()
            .map(|m| m.body.clone())
            .collect()
    }

    const LIVE: Duration = Duration::from_secs(300);

    #[tokio::test]
    async fn capped_drain_takes_oldest_and_keeps_rest_in_order() {
        let inbox = Inbox::new();
        inbox
            .seed(
                (0..5)
                    .map(|i| msg_at("a", &format!("m{i}"), LIVE))
                    .collect(),
            )
            .await;
        let d = inbox
            .drain_valid_filtered_capped(Instant::now(), |_| true, 2)
            .await;
        assert_eq!(bodies(&d), ["m0", "m1"]);
        assert_eq!(d.remaining, 3);
        assert_eq!(queued_bodies(&inbox).await, ["m2", "m3", "m4"]);
    }

    #[tokio::test]
    async fn capped_filtered_drain_counts_only_matching_and_preserves_interleave() {
        let inbox = Inbox::new();
        inbox
            .seed(vec![
                msg_at("b", "b0", LIVE),
                msg_at("a", "a0", LIVE),
                msg_at("a", "a1", LIVE),
                msg_at("b", "b1", LIVE),
                msg_at("a", "a2", LIVE),
            ])
            .await;
        let a = RoomToken::from("a".to_string());
        let d = inbox
            .drain_valid_filtered_capped(Instant::now(), |r| r == &a, 1)
            .await;
        assert_eq!(bodies(&d), ["a0"]);
        assert_eq!(d.remaining, 2, "non-matching room b is not counted");
        assert_eq!(queued_bodies(&inbox).await, ["b0", "a1", "b1", "a2"]);
    }

    #[tokio::test]
    async fn capped_drain_drops_expired_everywhere_and_excludes_them_from_remaining() {
        let inbox = Inbox::new();
        inbox
            .seed(vec![
                msg_at("a", "live0", LIVE),
                msg_at("a", "dead", Duration::ZERO),
                msg_at("a", "live1", LIVE),
                msg_at("a", "live2", LIVE),
            ])
            .await;
        let d = inbox
            .drain_valid_filtered_capped(Instant::now(), |_| true, 1)
            .await;
        assert_eq!(bodies(&d), ["live0"]);
        assert_eq!(d.remaining, 2);
        assert_eq!(queued_bodies(&inbox).await, ["live1", "live2"]);
    }

    #[tokio::test]
    async fn capped_drain_zero_limit_is_treated_as_one() {
        let inbox = Inbox::new();
        inbox
            .seed(vec![msg_at("a", "x", LIVE), msg_at("a", "y", LIVE)])
            .await;
        let d = inbox
            .drain_valid_filtered_capped(Instant::now(), |_| true, 0)
            .await;
        assert_eq!(bodies(&d), ["x"]);
        assert_eq!(d.remaining, 1);
    }

    #[tokio::test]
    async fn wait_drain_returns_queued_remainder_immediately() {
        let inbox = Inbox::new();
        inbox
            .seed(
                (0..3)
                    .map(|i| msg_at("a", &format!("m{i}"), LIVE))
                    .collect(),
            )
            .await;
        let deadline = Instant::now() + Duration::from_secs(3600);
        let first = inbox.wait_drain_until_filtered(deadline, |_| true, 2).await;
        assert_eq!(bodies(&first), ["m0", "m1"]);
        assert_eq!(first.remaining, 1);
        let second = tokio::time::timeout(
            Duration::from_secs(2),
            inbox.wait_drain_until_filtered(deadline, |_| true, 2),
        )
        .await
        .expect("a queued remainder must not block");
        assert_eq!(bodies(&second), ["m2"]);
        assert_eq!(second.remaining, 0);
    }
}
