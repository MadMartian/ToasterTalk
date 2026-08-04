# ToasterTalk MCP — Plan: Broadcast and Multi-Consumer Rooms

**Status**: Deferred — not in scope for the initial MVP.

---

## Problem statement

The current design delivers each message to exactly one named recipient. There is no way for one member to send a message to all current members of a room simultaneously. This limits ToasterTalk MCP to two-agent turn-based workflows.

Use cases that require broadcast:
- A coordinator agent announcing a task to all worker agents
- A status update that every member in a room should receive
- Fanout notifications where the sender does not need to know the exact member list

---

## Proposed tools

| Tool | Arguments | Returns | Blocks? |
|---|---|---|---|
| `subscribe` | `session_token`, `topic?` | `{ok}` | No |
| `unsubscribe` | `session_token`, `topic?` | `{ok}` | No |
| `publish` | `session_token`, `topic`, `message`, `ttl_secs?` | `{message_id, recipient_count}` | No |

`subscribe` opts the calling member into receiving messages published to a topic within their room. If `topic` is omitted, a default topic (e.g. the room name) is used.

`publish` fans out the message to all currently subscribed members at publish time. Each subscriber receives an independent copy in their inbox. The sender receives back the count of recipients the message was fanned out to.

Direct `send` (fire-and-forget and send-with-reply) remains unchanged. Broadcast and direct messaging are orthogonal.

---

## Fan-out model

- Fan-out happens at publish time, not at delivery time. If a member subscribes after a message is published, they do not receive it.
- Each subscriber gets an independent `QueuedMessage` in their inbox. Message TTL and delivery age apply independently per copy.
- Reply to a broadcast uses plain direct `send` back to the publisher. No reply-all concept.
- An empty subscriber list (no one subscribed) is not an error — `publish` returns `{recipient_count: 0}`.

---

## Constraints and open questions

- **Inbox capacity**: Broadcast amplifies the DoS concern from ISS-001. Publishing to a room with 20 members creates 20 inbox entries simultaneously. Any inbox cap must be applied per recipient, not per sender.
- **Per-room vs. cross-room topics**: Topics are scoped to the room. A member cannot subscribe to topics in another room.
- **Persistence**: Subscriptions are in-memory only (like all agent state). A member that disconnects and reconnects must re-subscribe.
- **Tool naming conflict**: `subscribe`/`unsubscribe`/`publish` are reserved for this design. Do not use these names for other purposes.

---

## Implementation order

1. Add `subscribe`/`unsubscribe` to `AppState` (per-room subscription sets)
2. Add `publish` handler in `src/tools/publish.rs`
3. Add TDD rubrics for broadcast scenarios
4. Update `TECH.md` module map and state model
5. Update `PRODUCT.md` to reflect multi-consumer capability
6. Remove this plan file — distribute implementation details to TECH.md and TDD.md
