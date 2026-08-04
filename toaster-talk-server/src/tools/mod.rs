pub mod connect;
pub mod join;
pub mod receive;
pub mod roster;
pub mod send;

use crate::state::QueuedMessage;
use std::time::{Duration, Instant};
use uuid::Uuid;

// ── Timeout status ────────────────────────────────────────────────────────────

/// Describes whether and why a blocking tool call returned without a result.
///
/// Returned as the `timeout` field in `receive` and send-with-reply responses.
#[derive(Debug, PartialEq)]
pub enum Timeout {
    /// The call returned with a result before any timeout applied.
    None,
    /// The requested timeout elapsed naturally (no cap, no messages).
    Elapsed,
    /// The call was cut short by the `X-Host-Cap-Secs` header before the
    /// requested timeout. Re-call the tool immediately to continue waiting.
    Capped,
    /// The wait was preempted: a newer blocking call from the same member
    /// claimed the inbox's single receive slot, aborting this one. Nothing was
    /// delivered and nothing was lost — the newer call now owns the wait. This
    /// is distinct from `None` (which promises messages were returned) and from
    /// `Elapsed` (which promises the requested wait ran to completion); it
    /// almost always means the caller issued concurrent blocking calls.
    Preempted,
}

impl std::fmt::Display for Timeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Timeout::None => f.write_str("none"),
            Timeout::Elapsed => f.write_str("elapsed"),
            Timeout::Capped => f.write_str("capped"),
            Timeout::Preempted => f.write_str("preempted"),
        }
    }
}

// ── Shared message type ───────────────────────────────────────────────────────

/// A message delivered from an inbox (used in both `send` and `receive` responses).
#[derive(Debug)]
pub struct InboxMessage {
    pub id: Uuid,
    pub from: String,
    /// Display name of the room this message was sent in. Every delivered message
    /// carries this now that one session's shared inbox can hold traffic from
    /// several rooms at once (PLAN.multi-receive.md) — the recipient always knows
    /// where a message came from, even when `receive`'s room filter is omitted
    /// and messages from several rooms are returned together (TDD 3.10).
    pub room: String,
    pub body: String,
    /// Human-readable time the message spent waiting in the queue, e.g. "2 minutes in queue".
    pub age: String,
    /// Seconds remaining in the sender's `send`-with-reply window as of delivery, or
    /// `None` for a fire-and-forget message. Computed from the queued message's
    /// `reply_deadline`; saturates to 0 once the window has closed. Its presence is
    /// the signal that the sender is awaiting a reply — the recipient replies with a
    /// plain `send` back to `from`.
    pub reply_expected_within_secs: Option<u64>,
}

impl InboxMessage {
    /// `room_name` is the display name resolved by the caller (from `RoomStore`)
    /// for the message's `room` token — state.rs deals in tokens, not names.
    pub fn from_queued(m: QueuedMessage, room_name: String) -> Self {
        InboxMessage {
            age: format_age(m.enqueued_at),
            reply_expected_within_secs: m
                .reply_deadline
                .map(|d| d.saturating_duration_since(Instant::now()).as_secs()),
            id: m.id,
            from: m.from,
            room: room_name,
            body: m.body,
        }
    }
}

/// Return the first 8 bytes of a token string as a log-safe session ID.
/// Uses `len().min(8)` so caller-supplied (potentially short/empty) tokens never panic.
pub fn session_id(token: &str) -> &str {
    &token[..token.len().min(8)]
}

/// Format a queued message's wait time as a human-readable age string.
pub fn format_age(enqueued_at: Instant) -> String {
    format_last_seen(enqueued_at)
        .trim_end_matches(" ago")
        .to_string()
        + " in queue"
}

/// Format a slice of InboxMessages as YAML for MCP tool output.
pub fn format_messages_yaml(messages: &[InboxMessage]) -> String {
    let mut out = String::from("messages:\n");
    for m in messages {
        out.push_str(&format!(
            "  - id: {}\n    from: {}\n    room: {}\n    age: \"{}\"\n",
            m.id, m.from, m.room, m.age
        ));
        // Emit only for a send-with-reply. Must precede `body`, whose `|` block scalar
        // would otherwise swallow any following field.
        if let Some(secs) = m.reply_expected_within_secs {
            out.push_str(&format!("    reply_expected_within_secs: {secs}\n"));
        }
        out.push_str(&format!(
            "    body: |\n      {}\n",
            m.body.replace('\n', "\n      ")
        ));
    }
    out
}

/// Format a duration as a human-readable "last seen" string,
/// e.g. "4 hours ago", "2 minutes ago", "30 seconds ago".
pub fn format_last_seen(last_seen: Instant) -> String {
    let elapsed = last_seen.elapsed();
    format!("{} ago", format_duration(elapsed))
}

fn format_duration(d: Duration) -> String {
    toaster_talk_common::format_duration_human(d)
}

/// Validate a member name against the allowed character set and length limit.
/// Delegates to the shared predicate in the common crate (NEW-2).
pub fn validate_member_name(name: &str) -> Result<(), crate::error::Error> {
    if toaster_talk_common::is_valid_member_name(name) {
        Ok(())
    } else {
        Err(crate::error::Error::InvalidMemberName(name.to_string()))
    }
}

/// Generate a cryptographically random session token (32-byte hex string).
pub fn new_session_token() -> String {
    use uuid::Uuid;
    // Two UUIDs concatenated give 32 random bytes = 64 hex chars
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    // TDD 11.2: member name validation
    #[test]
    fn test_member_name_accepts_hyphen() {
        assert!(validate_member_name("qa-agent").is_ok());
        assert!(validate_member_name("dev_01").is_ok());
    }

    #[test]
    fn test_member_name_rejects_space() {
        assert!(matches!(
            validate_member_name("my name"),
            Err(Error::InvalidMemberName(_))
        ));
    }

    #[test]
    fn test_member_name_rejects_empty() {
        assert!(matches!(
            validate_member_name(""),
            Err(Error::InvalidMemberName(_))
        ));
    }

    #[test]
    fn test_member_name_rejects_too_long() {
        assert!(matches!(
            validate_member_name(&"a".repeat(41)),
            Err(Error::InvalidMemberName(_))
        ));
    }

    // Rubric 4.4 (wire shape): reply_expected_within_secs renders before `body` when
    // Some, and is omitted entirely when None. Placement matters — the `body` block
    // scalar (`|`) swallows any following line, so the field must precede it.
    #[test]
    fn test_format_messages_yaml_reply_expected_present_and_absent() {
        let with = InboxMessage {
            id: Uuid::nil(),
            from: "alice".into(),
            room: "myroom".into(),
            body: "hi".into(),
            age: "1 second in queue".into(),
            reply_expected_within_secs: Some(42),
        };
        let out = format_messages_yaml(&[with]);
        assert!(
            out.contains("reply_expected_within_secs: 42\n"),
            "expected the field rendered; got: {out}"
        );
        assert!(
            out.contains("room: myroom\n"),
            "expected the room field rendered; got: {out}"
        );
        let field_at = out.find("reply_expected_within_secs").unwrap();
        let body_at = out.find("body:").unwrap();
        assert!(
            field_at < body_at,
            "reply_expected_within_secs must precede the body block scalar; got: {out}"
        );

        let without = InboxMessage {
            id: Uuid::nil(),
            from: "bob".into(),
            room: "myroom".into(),
            body: "yo".into(),
            age: "1 second in queue".into(),
            reply_expected_within_secs: None,
        };
        let out = format_messages_yaml(&[without]);
        assert!(
            !out.contains("reply_expected_within_secs"),
            "fire-and-forget must omit the field entirely; got: {out}"
        );
    }

    // Rubric 4.4 (value semantics): from_queued turns reply_deadline into a remaining
    // seconds countdown, saturating to 0 once the window has closed (never underflowing
    // or dropping the field), and stays None for a fire-and-forget message.
    #[test]
    fn test_from_queued_reply_window_counts_down_and_saturates() {
        let now = Instant::now();
        let base = |deadline| QueuedMessage {
            id: Uuid::nil(),
            from: "a".into(),
            room: crate::rooms::RoomToken::from("rt".to_string()),
            body: "x".into(),
            enqueued_at: now,
            expires_at: now + Duration::from_secs(600),
            reply_deadline: deadline,
        };

        // Future deadline → positive remaining, at most the window (truncated by as_secs).
        let m =
            InboxMessage::from_queued(base(Some(now + Duration::from_secs(120))), "room".into());
        assert!(
            matches!(m.reply_expected_within_secs, Some(n) if (118..=120).contains(&n)),
            "expected ~120s remaining; got {:?}",
            m.reply_expected_within_secs
        );

        // Elapsed deadline → saturates to Some(0): still signals intent, never underflows.
        let m = InboxMessage::from_queued(base(Some(now - Duration::from_secs(5))), "room".into());
        assert_eq!(
            m.reply_expected_within_secs,
            Some(0),
            "an elapsed reply window must saturate to 0, not underflow or vanish"
        );

        // No deadline → None.
        assert_eq!(
            InboxMessage::from_queued(base(None), "room".into()).reply_expected_within_secs,
            None
        );
    }
}
