# ToasterTalk MCP — Test Rubrics

| # | Functional area | Rubrics     |
|---|---|-------------|
| 1a | Connect | 1a.1 – 1a.5   |
| 1b | Join | 1b.1 – 1b.8   |
| 1c | Leave | 1c.1 – 1c.6   |
| 2 | Send (Fire and Forget) | 2.1 – 2.6   |
| 3 | Receive | 3.1 – 3.13   |
| 4 | Send-with-reply | 4.1 – 4.5   |
| 5 | General Tools | 5.1 – 5.3   |
| 6 | Message TTL | 6.1 – 6.3   |
| 7 | Auth and isolation | 7.1 – 7.2   |
| 8 | Configuration | 8.1 – 8.20  |
| 9 | Error handling | 9.1 – 9.7   |
| 10 | Room Management & Member Lifecycle | 10.1 – 10.27, 10.10b, 10.13b |
| 11 | Room & Member Name Validation | 11.1, 11.1b, 11.2, 11.3 |
| 12 | Seat Tokens | 12.1 – 12.7 |
| 13 | Diagnostics | 13.1 – 13.13 |

---

## 1a. Connect

### 1a.1

- **Given** the MCP server is not yet connected
- **When** the agent determines it must use the ToasterTalk MCP for the first time
- **Then** the agent calls the connect tool and the connect tool mints a new session token used for sub-sequent requests

### 1a.2
- **Given** the MCP server was restarted while an agent was a member in a room
- **When** the agent attempts to reconnect to the server
- **Then** the MCP server will accept the connection attempt and issue a new session token

### 1a.3
- **Given** the MCP server was restarted while an agent was a member in a room
- **When** the agent attempts to call a tool that requires a valid session token
- **Then** the MCP server will respond with a descriptive error indicating that the session token is invalid
- **And** the agent's host is signalled to reconnect promptly rather than left hanging

### 1a.4
- **Given** an agent holds a still-valid `session_token` from a prior `connect`, with one or more room memberships under it
- **When** the agent calls `connect` again supplying that value as `old_session_token`
- **Then** the previous session token is invalidated immediately and a new, roomless session token is issued
- **And** none of the old session's room memberships or queued messages transfer automatically — the agent must call `join` again for each room it wants to resume, using the room or seat token it still holds
- **And** this bounds a leaked `session_token` to identity-rotation only: possessing it alone can never recover a room without also holding that room's own credential

### 1a.5
- **Given** a session was minted by `connect` and never carried through `join` — it holds no room membership at all
- **When** that session sits idle (no `join` call) for longer than the configured non-seat member idle TTL and the
  periodic sweep runs
- **Then** the session is reaped — its session-map entry, connect timestamp, and shared inbox are all removed — so a
  client that repeatedly retries a bare `connect` without ever joining (or without supplying `old_session_token`) does
  not leak one orphaned session per attempt; a roomless session has no room membership and so is otherwise invisible to
  the per-room idle sweep and would live forever
- **And** a roomless session younger than the idle TTL is left alone — the reap is idle-time-based, not an immediate
  connect/join grace-window rejection

## 1b. Join

### 1b.1
- **Given** the MCP server is connected
- **When** the agent determines it must join a ToasterTalk MCP room for the first time
- **Then** the agent must find the correct token for the room name that was requested, and use this to join the room

### 1b.2
- **Given** the MCP server is connected but the agent has not joined a room yet
- **When** the agent tries to use the MCP server before joining
- **Then** the MCP tool will return an error indicating that joining is required

### 1b.3
- **Given** some room contains some member
- **When** another agent tries to join using the same member name
- **Then** the MCP server will reject the join attempt with an error, indicating that the name is already in use

### 1b.4
- **Given** an agent has determined a room or seat token to use, but it is not valid and/or expired
- **When** the agent attempts to join the room
- **Then** the MCP server will reject the join attempt with an error, indicating that the room token (or seat token) is invalid or 
  expired

### 1b.5
- **Given** an agent has a valid session token
- **When** they attempt to join a room with a name that is not already taken (in the room)
- **Then** they join it successfully

### 1b.6
- **Given** an agent has a valid session token
- **And** they have joined at least one room
- **When** they attempt to join another room using a different session token that is not valid
- **Then** the MCP server will reject the join attempt with an error, indicating that the session token is not valid

### 1b.7
- **Given** the agent has joined a room and has a valid session token
- **When** the agent attempts to join that same room again, with a valid session token
- **Then** the MCP server returns an error message indicating that the agent has already joined that room

### 1b.8
- **Given** an agent's `join` (room token or seat token) has verified its credential
- **When** the operator deletes that room (or, for a seat join, that seat) at the same moment
- **Then** either the join is rejected exactly as a join against the already-deleted credential would be, or the join
  completes first and the deletion then evicts the new membership — a membership is never left attached to a room or
  seat that no longer exists

---

## 1c. Leave

### 1c.1
- **Given** an agent's session has joined a room
- **When** it calls `leave` naming that room
- **Then** the room membership is removed and the server confirms which room and member name were removed

### 1c.2
- **Given** an agent's session belongs to more than one room
- **When** it calls `leave` for one of them
- **Then** only that one room membership is removed — the session, its shared inbox, and its other room memberships are completely unaffected (mirrors 10.21)

### 1c.3
- **Given** an agent's session holds exactly one room membership
- **When** it calls `leave` for that room
- **Then** the room membership is removed and, since it was the session's last remaining membership, the whole session and its shared inbox are torn down — resuming any room afterward requires a fresh `connect` and `join`, exactly like any other last-membership eviction

### 1c.4
- **Given** an agent's session has not joined the named room
- **When** it calls `leave` for that room
- **Then** the server rejects the call with an error indicating the agent is not connected to that room, rather than treating it as a no-op

### 1c.5
- **Given** an agent calls `leave` naming a room that does not exist
- **Then** the server rejects the call with an error indicating the room was not found

### 1c.6
- **Given** an agent calls `leave` with a missing or invalid session token
- **Then** the server rejects the call with an error indicating the session token is invalid

---

## 2. Send (Fire and Forget)

### 2.1
- **Given** the agent has joined the room and becomes a member
- **When** the member sends a message without requesting a reply
- **Then** the message is enqueued for delivery and the tool returns immediately, without waiting

### 2.2
- **Given** the member thinks a certain recipient is joined and active, but in fact does not exist
- **When** the member attempts to send a message to that recipient
- **Then** the member will receive an error indicating that the recipient is not in the room

### 2.3
- **Given** a message is waiting in the 1st member's own inbox from some other 2nd member
- **When** the 1st member sends a fire-and-forget message (to anyone, not requesting a reply)
- **Then** the send returns only the enqueue result — never the waiting message — and the
  1st member's inbox is left intact, so the waiting message is still delivered by a
  subsequent `receive`. Fire-and-forget is strictly one-way: it never hands back mail.

### 2.4
- **Given** the agent wants to send a message to a specific party
- **When** the agent makes the call without specifying the room name as well as the recipient in that room
- **Then** the MCP server will return an error indicating that the room name input parameter is required

### 2.5
- **Given** the agent wants to send a message to a specific party
- **When** the agent makes the call specifying a room name and a recipient that is not in that room
- **Then** the MCP server will return an error indicating that the recipient is not present in that room

### 2.6
- **Given** the agent wants to send a message to a specific party
- **When** the agent makes the call specifying an invalid room name
- **Then** the MCP server will return an error indicating that the room does not exist

---

## 3. Receive

### 3.1
- **Given** a message has been enqueued for a member
- **When** the member invokes the tool to receive messages
- **Then** the member will receive the message

### 3.2
- **Given** no message is waiting
- **When** the timeout elapses
- **Then** the tool will return immediately indicating no message was received

### 3.3
- **Given** multiple messages are waiting, no more than the configured per-call batch size (`max_receive_batch`)
- **When** the member invokes a tool to receive messages
- **Then** all enqueued messages will be returned in the order they were enqueued

### 3.4
- **When** the member requests to receive a message without specifying a timeout
- **Then** the tool will use the configured timeout value, or default value if not configured

### 3.5
- **Given** the member cancels a request to receive a message before a timeout elapses, and before receiving the message
- **When** the member invokes a tool to receive messages again
- **Then** the previous request will be canceled and will try to receive a message again

### 3.6
- **Given** a waiter was orphaned — left parked on a member's inbox after its connection dropped without the handler future being cancelled
- **When** the same member issues a new wait and a message subsequently arrives
- **Then** the new (live) waiter receives the message, and the orphaned waiter is preempted and cannot drain or steal it
- **And** there is at most one live waiter per inbox at any time

### 3.7
- **Given** a member has a blocking `receive` (or send-with-reply) parked on its inbox, and the same member starts a second blocking call that preempts the first
- **When** the preempted (first) call returns
- **Then** it reports `timeout: preempted` with an empty batch — distinct from `none` (messages were returned) and `elapsed` (the full wait ran) — so the caller can tell its wait was superseded by its own newer call, not mistake the empty result for a delivery or a genuine timeout

### 3.8
- **When** an agent specifies to `receive` messages from a specific room instead of from anywhere
- **Then** the MCP server will return messages only from that room, leaving other messages from other rooms in that inbox

### 3.9
- **Given** messages are waiting for an agent in more than one room
- **When** an agent specifies to `receive` messages without specifying any room at all from which to receive them
- **Then** the MCP server will return the messages in the inbox coming from any room that that agent has joined
  (up to the per-call batch size — see 3.11)

### 3.10
- **Given** multiple messages from different rooms are waiting in an agent's inbox
- **When** the agent calls `receive` to receive all of them, without specifying a specific room from which to receive them
- **Then** each message in the response indicates the room and member of that room from which the message came

### 3.11
- **Given** more messages are waiting for a member than the configured per-call batch size (`max_receive_batch`,
  default 16)
- **When** the member calls `receive`
- **Then** the response contains exactly the batch size of the oldest waiting messages, in enqueue order
- **And** the response's `remaining` field reports how many further messages are still queued, so the agent knows to
  call `receive` again; a response that emptied the inbox (or returned nothing) reports `remaining: 0`

### 3.12
- **Given** a previous `receive` returned a capped batch and reported a non-zero `remaining`
- **When** the member calls `receive` again, even with a long `timeout_secs`
- **Then** the call returns immediately (without waiting) with the next batch, continuing in enqueue order
- **And** across successive calls every queued message is delivered exactly once — none is lost or reordered by the cap

### 3.13
- **Given** a member's inbox holds messages from several rooms, and more from one room than the batch size
- **When** the member calls `receive` naming that room
- **Then** the batch cap applies to that room's messages only: up to the batch size of that room's oldest messages are
  returned, `remaining` counts only that room's leftover messages, and other rooms' messages stay queued, in their
  original order, for a later call

---

## 4. Send-with-reply
This section implies rubrics from section 3 (Receive) and rubrics 2.4-2.6 from section 2 (Send, fire and forget)

### 4.1
- **Given** the other member never sends a reply
- **When** the member sends a message and expects a reply
- **Then** the tool will return a response indicating that no reply was received

### 4.2
- **Given** the agent has joined the room and becomes a member
- **When** the member determines a message should be sent to some member and a reply should be expected
- **Then** the member will send the message to the appropriate member and the tool blocks until the other member sends
  a reply or the timeout elapses

### 4.3
- **Given** a member issues a send-with-reply
- **When** the reply-wait blocks on the member's own inbox conditional upon a reply from the recipient's room
- **Then** it uses the same single preemptible receive slot as `receive` — so it cannot become an orphaned waiter that
  steals message wakeups, and a later `receive` or send-with-reply by that member preempts the prior wait

### 4.4
- **Given** a member sends a message with `await_reply_secs` set (send-with-reply)
- **When** the recipient receives that message
- **Then** the delivered message carries a `reply_expected_within_secs` field giving the seconds remaining in the
  sender's reply window as of delivery (counting down as the message sits queued); a fire-and-forget message
  (no `await_reply_secs`) carries no such field

### 4.5
- **Given** more messages from the target room are waiting in the sender's inbox than the per-call batch size
- **When** the sender's send-with-reply wait drains them
- **Then** it returns at most the batch size of them, oldest first — the same cap `receive` applies — and reports the
  rest in `remaining`
- **And** the messages past the cap (which may include the awaited reply) stay queued in order and are delivered by
  the sender's next `receive` — a reply is never discarded by the cap

---

## 5. General Tools

### 5.1
- **Given** a message has been enqueued for a member
- **When** a member finally receives that message
- **Then** the message will contain the duration of time between when the message was enqueued and when it was delivered

### 5.2
- **Given** a member has not been seen for longer than a configured idle threshold
- **When** another member sends a message to it
- **Then** the send response will include a human-readable "last seen" duration indicating how long ago the recipient was
  last seen

### 5.3
- **Given** an agent is connected to a room
- **When** it asks to list the members of a room, naming that room
- **Then** the names of all members currently connected to that room are returned
- **And** if the named room is not one the agent has joined, the request is rejected with an error indicating that the agent is not a member of that room

---

## 6. Message TTL

### 6.1
- **When** the member sends a message with no TTL specified
- **Then** the configured default message TTL will be used, and the tool response includes the TTL used

### 6.2
- **When** a message expires
- **Then** the message is discarded immediately or within a short time frame

### 6.3
- **Given** a member sends a message with an explicit TTL
- **When** that TTL elapses without the message being delivered to the intended member
- **Then** the message is discarded

---

## 7. Auth and isolation

### 7.1
- **Given** four members in the same room, members A, B, C and D
- **When** a member tries to send a message, A → C or member B → D or member C → A or member D → B
- **Then** messages are correctly exchanged between the members

### 7.2
- **Given** two separate rooms, and separate tokens for each, each with members A ⇄ B in room #1, and 
  members C ⇄ D in room #2
- **When** a member tries to send a message, A → C or member B → D or member C → A or member D → B
- **Then** the tool responds with a descriptive error indicating that cross-room messages are not permitted

---

## 8. Configuration

### 8.1
- **Given** no configuration is provided
- **When** the server starts
- **Then** it uses the default configuration

### 8.2
- **Given** some configuration property is configured other than via CLI
- **When** the server starts with the same CLI property specified
- **Then** the CLI property wins as this takes precedence over other configuration sources

### 8.3
- **Given** a configuration property configured anywhere is incorrect or malformed
- **When** the server starts
- **Then** the server will log a descriptive error and abort with an error

### 8.4
- **Given** a configuration property is set in the configuration file
- **When** the same property is set via an environment variable
- **Then** the environment variable takes precedence over the configuration file for that property

### 8.5
- **Given** a configuration property is set via environment variable
- **When** the same property is set via CLI
- **Then** the CLI property takes precedence over the environment variable property

### 8.6
- **Given** the MCP server is pre-configured with a request timeout "host cap"
- **When** any request takes longer than this configured timeout
- **Then** the server will terminate the request prior to reaching the "host cap", despite a timeout specified by the 
  agent, and signal in the response that the host cap has been reached.
- **And** this applies to an in-flight `send`-with-reply just as it does to `receive`: when the host cap fires before a
  reply arrives, the reply-wait is shortened to the cap and the response reports `timeout: capped` (not `elapsed`) — the
  message having already been delivered, so the agent continues with `receive`, never another `send`

### 8.7
- **Given** a host cap timeout configuration is set for a particular MCP host `A`
- **When** a different MCP host `B` interacts with the MCP server that does not have such a timeout configured
- **Then** the server will not impose the host cap from MCP host `A` on MCP host `B`

### 8.8
- **Given** a server-side maximum is configured for `await_reply_secs` (send) and `timeout_secs` (receive)
- **When** a member specifies a value exceeding the configured maximum
- **Then** the server silently clamps the value to the maximum, regardless of which transport is used

### 8.9
- **Given** the MCP server is configured with the host cap header, but it is malformed
- **When** the agent calls either `send` or `receive`
- **Then** the MCP server will respond with a structured validation error

### 8.10
- **Given** no idle TTL for non-seat members has been configured
- **When** the server starts
- **Then** the server refuses to start with a descriptive error — there is no default; the operator must consciously
  choose this value (it is server-controlled and never settable by an agent)

### 8.11
- **Given** the configured non-seat member idle TTL is below the 30-minute minimum or above the 24-hour maximum (or is
  zero)
- **When** the server starts
- **Then** the server refuses to start with a descriptive error indicating the permitted bounds

### 8.12
- **Given** the configured non-seat member idle TTL is within 30 minutes to 24 hours inclusive
- **When** the server starts
- **Then** the server accepts the value and applies it to the idle-eviction sweep

### 8.13
- **Given** the operator changes a tunable setting (for example the default message TTL) and triggers a configuration reload
- **When** agents continue to use their existing connections
- **Then** the new setting takes effect for subsequent operations, no agent has to reconnect, and no live connection or session is dropped

### 8.14
- **Given** the operator triggers a configuration reload whose changes include a setting that cannot be applied without a restart (for example the listen port or the master secret)
- **When** the reload is processed
- **Then** the server keeps running on the original value for that setting, logs a warning identifying each ignored change, and continues to apply every other (reloadable) change

### 8.15
- **Given** the operator hand-edits the room store (for example extending a room's or seat's expiry) while the server is running
- **When** a configuration reload is triggered
- **Then** the new expiry takes effect without a restart, and no live connection or session is dropped

### 8.16
- **Given** the operator triggers a reload while the room store file is missing or contains malformed data
- **When** the reload is processed
- **Then** the server keeps running on the previously loaded room store, logs a warning, and continues to apply every other reloadable change

### 8.17
- **Given** the operator wants to apply a configuration or room-store change without restarting the server
- **When** they issue an explicit reload command through the management interface
- **Then** the change takes effect exactly as it does for any other reload trigger (per 8.13–8.16) — reload does not
  depend on an out-of-band signal being available

### 8.18
- **Given** the operator sets the per-call receive batch size (`max_receive_batch`) in the config file, an environment
  variable, both, or neither
- **When** the server starts or reloads
- **Then** the environment variable overrides the config file, the config file overrides the default, and the default
  is 16 — and the resolved value is the cap every `receive` and send-with-reply drain applies

### 8.19
- **Given** the operator sets `max_receive_batch` to `0` or to more than `1000`
- **When** the server starts (or a reload is processed)
- **Then** the configuration is rejected with an error naming `max_receive_batch` (startup fails; a reload keeps the
  running configuration) — a value above `max_inbox_messages` is accepted, as it is simply never reached

### 8.20
- **Given** the operator sets the seat-contention detection knobs (supersession threshold, counting window, freeze
  hold-down) in the config file and/or environment
- **When** the configuration is resolved
- **Then** each knob follows the usual file → environment precedence and defaults to a threshold of 3, a 30 s window
  and a 60 s hold-down when unset; a threshold of 0 disables detection, and a value outside its documented bounds is
  refused with a configuration error rather than clamped

---

## 9. Error handling

### 9.1
- **When** an error occurs during message processing
- **Then** the MCP server will log the error and return a descriptive error to the member

### 9.2
- **When** a member sends a message exceeding the maximum message size limit (100 KB)
- **Then** the tool rejects the message with an error indicating the size limit

### 9.3
- **When** a member connection or request is abnormally terminated
- **Then** the MCP server will log the error, then close and release the member and connection

### 9.4
- **Given** the MCP host imposes a hard-coded time limit on requests
- **When** a member sends a request that exceeds the time limit
- **Then** the MCP server will respond with a descriptive error and indicating that the host cap timeout has elapsed

### 9.5
- **Given** the MCP server is connected
- **When** the send or receive tools are inspected
- **Then** the description will include brief instruction about the host cap timeout and how it works

### 9.6
- **Given** an agent session does not have a valid session token
- **When** the agent tries to `send` or `receive`
- **Then** the server will respond with a structured authentication error

### 9.7
- **Given** a tool call includes a field that tool does not accept — for example a stale client calling `connect` with
  the pre-connect/join-split `member_name`/`room_token` arguments
- **When** the server deserializes the call's parameters
- **Then** the call is rejected with a structured validation error naming the problem, rather than silently dropping
  the unrecognized field and proceeding as if it had never been sent

## 10. Room Management & Member Lifecycle

### 10.1
- **Given** a human operator is using the CLI to manage the server
- **When** a room is created
- **Then** a token file is created and the room is registered with the server

### 10.2
- **Given** a human operator is using the CLI to manage the server
- **When** a room is deleted
- **Then** the room is unregistered with the server, invalidating the token

### 10.3
- **Given** a human operator is using the CLI to manage the server
- **When** the list of rooms are requested
- **Then** a list of rooms is returned, excluding sensitive information

### 10.4
- **Given** a room's TTL has elapsed
- **When** the server routinely checks for expired rooms
- **Then** the room is deleted, its token invalidated, and any / all members are kicked from the room

### 10.5
- **When** a token is deleted via the CLI
- **Then** the server invalidates the token but any associated token file is left intact

### 10.6
- **Given** a human operator is using the CLI to manage the server
- **When** a timestamp is involved in the output returned to the human operator
- **Then** it will be formatted as a human-readable date and time in the local timezone

### 10.7
- **Given** a human operator is using the CLI to manage the server
- **When** details about a specific room are requested
- **Then** the CLI will return the details of the room, including the list of members and their names

### 10.8
- **Given** one or more members are connected to a room
- **When** the operator uses the CLI to kick one or more of them by name in a single command
- **Then** each named member that is present is removed from the room and their session invalidated, the room itself
  remains, and the CLI reports which names were kicked and which were not found; if none of the named members were
  present the command is reported as an error

### 10.9
- **Given** one or more members are connected to a room
- **When** the operator uses the CLI to kick all members from that room
- **Then** all members are removed from the room and their sessions invalidated, but the room itself remains

### 10.10
- **Given** a member is connected to a room and currently blocked in a long-timeout `receive` **or** a send-with-reply reply-wait, and this eviction empties the last (or only) room membership the session holds
- **When** that member is evicted by any path — operator CLI (`kckm`, `kckall`, room deletion `delrm`), idle TTL, room expiry, a seat-holder supersede, or a voluntary `leave`
- **Then** the member's in-flight blocking call is aborted and returns promptly — it does **not** stay parked until its `timeout_secs` elapses

### 10.10b
- **Given** a session belongs to more than one room, and is currently blocked in a `receive` (any-room or filtered to a specific room) or a send-with-reply reply-wait
- **When** the session is evicted from *one* of its rooms but at least one other room membership survives
- **Then** the in-flight blocking call is **not** aborted by that eviction alone, it is unaffected by that eviction

### 10.11
- **Given** a member who joined with a plain room token has been idle (no tool activity) for longer than the configured non-seat member idle TTL
- **When** the server runs its periodic sweep
- **Then** that member is evicted — their session is invalidated and they are removed from the room — while the room itself and other members remain

### 10.12
- **Given** a non-seat member is actively parked in a blocking `receive` (or send-with-reply reply-wait), even if the wait has lasted longer than the idle TTL
- **When** the server runs its periodic sweep
- **Then** that member is **not** idle-evicted while the wait is live — an actively waiting agent is treated as alive

### 10.13
- **Given** a member who joined with a reserved seat token whose client has gone away — it has been idle (no tool activity) longer than the configured dead-seat window and is **not** actively waiting for messages
- **When** the server runs its periodic sweep
- **Then** that dead seat is reaped — its session is invalidated and it is removed from the room — so that a sender to it is promptly told the recipient is gone, rather than having messages silently pile up in an inbox no live client will ever drain
- **And** this dead-seat reaping is generous by default and can be turned off by configuration, restoring the historical behavior in which seats are never idle-evicted

### 10.13b
- **Given** a member who joined with a reserved seat token is actively parked in a blocking `receive` (or send-with-reply reply-wait), however long the clock says it has been idle
- **When** the server runs its periodic sweep
- **Then** that seat is **not** reaped — an actively waiting seat agent is treated as alive, which is why a well-behaved persistent seat (one that holds a long receive) is always safe and only genuinely dead seats are reaped

### 10.14
- **Given** a member whose name collides with a reserved seat, but who connected via the unprivileged room-token path (not the seat token)
- **When** that member sits idle past the idle TTL and the sweep runs
- **Then** that member **is** idle-evicted — exemption is conferred by the authentication method used, never by the member's name, so a name collision cannot buy immortality through the unprivileged path

### 10.15
- **Given** a room contains members, including at least one seat-authed member
- **When** the room's TTL elapses and the sweep runs
- **Then** **all** members are evicted regardless of their individual idle TTLs — room expiry caps every member
- **And** this includes seat-authed members, capped by room expiry regardless of how (or whether) the dead-seat reaper is configured

### 10.16
- **Given** an idle non-seat member has undelivered messages queued in their inbox
- **When** that member is idle-evicted
- **Then** their queued messages are discarded — undelivered mail is lost by design when a member is evicted

### 10.17
- **Given** a member sends a fire-and-forget message and the intended recipient is evicted at (or immediately before) the moment of enqueue
- **When** the send completes
- **Then** the sender is told the recipient was evicted and the message was not delivered — the sender is **never** given a success/message id for a message that no live member received (no false delivery)

### 10.18
- **Given** a member sends a message and is awaiting a reply, and the recipient is evicted during the wait
- **When** the recipient's eviction takes effect
- **Then** the sender's call unblocks promptly, reporting that the recipient was evicted rather than a plain "no reply" timeout
- **And** in every case a sender is never blocked beyond the bounded reply timeout regardless of recipient state

### 10.19
- **Given** a non-seat member performs an activity that counts as being seen — sending, receiving, or listing the room's members — including a long blocking `receive` or send-with-reply that parks for much of the idle window
- **When** that activity completes
- **Then** the member's idle window is re-anchored to when the activity **completes**, not merely when it begins — consistent with parking (an actively waiting member is treated as alive for the whole wait), so a member that just finished a legitimate long blocking call is granted a fresh full idle window and is **not** reaped on the next sweep
- **And** the window slides with continued activity, so an actively working agent is never idle-evicted (the TTL measures inactivity, not session age)

### 10.20
- **Given** a message has been queued for a member — sent while the member's prior session was briefly away, or queued but not yet drained — and the member then reconnects (superseding its prior session)
- **When** the reconnected member performs a `receive`
- **Then** the still-valid (non-expired) queued messages are delivered to it — carried over from the prior session's inbox rather than silently discarded — with their original expiry preserved (not reset)
- **And** this holds for both reconnect paths: a seat-holder reconnect, and a room-token member reconnecting with its prior session token

### 10.21
- **Given** an agent is evicted from a room
- **When** the agent makes any request involving a different room they have joined but have not been evicted from (yet)
- **Then** their request is processed normally and no "evicted" error results

### 10.22
- **Given** an agent is evicted from a room
- **When** the agent makes any request involving that room (other than join)
- **Then** their request is rejected indicating that they are not a member of that room

### 10.23
- **Given** a human operator is using the CLI to manage the server
- **When** they create, delete, or kick-all across several rooms (`addrm`, `delrm`, `kckall`), or add or delete several members of one room (`addmb`, `delmb`), in a single command
- **Then** each distinct name is processed independently, in the order given, and the CLI reports the outcome of every item; a name repeated on the command line is processed once

### 10.24
- **Given** the CLI is about to write a room or seat token file
- **When** a file at the target name — or at one of its numeric-suffix variants — already holds exactly the contents about to be written
- **Then** that file is reused and no additional file is created; a differing file at the name still gets the next free numeric suffix, never an overwrite

### 10.25
- **Given** a batch command in which some items can be applied and others cannot (unknown room, unknown member, invalid name)
- **When** the command runs
- **Then** the items that can succeed are applied, each failed item is reported with its name, and the CLI exits non-zero because at least one item failed

### 10.26
- **Given** a room already exists (with or without seats and connected members)
- **When** the operator runs `addrm` naming it — alone or in a batch, and without `-f`/`--force`
- **Then** the room is never silently replaced: its seats, seat token files, timestamps and members are left exactly as they were
- **And** at an interactive terminal the CLI asks, per existing room, whether to overwrite it (dropping all its seats), defaulting to No; a declined room is skipped and reported, the rest of the batch still proceeds, and the CLI exits non-zero because that room was not (re)created
- **And** without an interactive terminal the CLI does not prompt: it reports each existing room as an error naming `--force` and exits non-zero, while any new names in the same batch are still created
- **And** a name repeated within one `addrm` command is processed once, so a repeat can never replace the room its first occurrence just created

### 10.27
- **Given** a room already exists with seats and connected members
- **When** the operator runs `addrm --force` (or `-f`) naming it, or confirms the overwrite at the prompt
- **Then** the room is replaced: its room token is unchanged (and its token file reused), but all of its seats are dropped so their seat tokens no longer authenticate, its created and expiry timestamps are reset to the new TTL, and its connected members are evicted
- **And** the CLI reports the room as overwritten rather than created

## 11. Room & Member Name Validation

### 11.1
- **Given** a member name is provided
- **When** it contains any character outside the allowed set (i.e. other than A-Z, a-z, 0-9, `_`, or `-`)
- **Then** the name is rejected as invalid with a descriptive error

### 11.1b
- **Given** a room name is provided
- **When** it contains a space, a double-quote character (`"`), or a null byte (`\0`)
- **Then** the name is rejected as invalid with a descriptive error

### 11.2
- **Given** a room or member name is provided
- **When** it exceeds the maximum allowed length
- **Then** the name is rejected as invalid with a descriptive error

### 11.3
- **Given** a room or member name is provided
- **When** it does not meet the minimum length requirements
- **Then** the name is rejected as invalid with a descriptive error

## 12. Seat Tokens

### 12.1
- **Given** a member seat token is created via the CLI tool
- **When** it is used to join a room with the correct name
- **Then** the member is added to the room and their session is validated

### 12.2
- **Given** a member has a valid seat token and a member of the same name exists in the room
- **When** the agent joins the room with this seat token
- **Then** the existing member in the room is evicted from that room and replaced by the new member
- **And** if the evicted member was blocked in a `receive`, that call is aborted and returns promptly rather than waiting out its `timeout_secs`

### 12.3
- **Given** an active session is marked as `Superseded`
- **When** when this agent makes a subsequent call wit the same old session token
- **Then** the server returns an `InvalidSessionToken`

### 12.4
- **Given** a seat token has been deleted via the CLI
- **When** an agent attempts to connect using this token
- **Then** the request is rejected with a descriptive authentication error

### 12.5
- **Given** an operator creates a seat for a member in a room via the CLI
- **When** the seat token file is written to disk
- **Then** it is placed at `tokens/user/<room>/<member>.tok` — the room name as a subdirectory and the member name as the filename (both filesystem-sanitised), mode `0600`
- **And** if that path already exists, an incrementing numeric suffix (`_1`, `_2`, …) is appended within the same room subdirectory rather than overwriting

### 12.6
- **Given** two live agents hold the same seat token and keep joining with it, superseding each other
- **When** the seat is superseded more than the configured threshold of times within the configured window
- **Then** the server logs a warning naming the room and member as possible seat credential contention (never any
  token), and freezes the seat for the configured hold-down
- **And** while frozen, a seat-token join that would supersede the current holder is rejected with a distinct
  `SeatContention` error stating how long the freeze lasts, and the current holder keeps its membership — the
  ping-pong ends with one winner
- **And** a frozen seat that currently has no holder can still be claimed

### 12.7
- **Given** a seat is frozen for credential contention
- **When** the hold-down lapses, or the operator kicks that member (`kckm`), kicks the whole room (`kckall`), or
  deletes the seat (`delmb`)
- **Then** the freeze and its supersession history are cleared, and the next superseding seat-token join is accepted

## 13. Diagnostics

A read-only operator command that takes a snapshot of the server's live in-memory state
(rooms, members, inboxes, sessions) for troubleshooting message-delivery desyncs while the
server is running. The dump must surface every piece of state needed to diagnose a stuck
receive without restarting the server or kicking members.

### 13.1
- **Given** the server has live rooms, connected members, and queued messages
- **When** the operator requests the diagnostic dump via the CLI
- **Then** the server's in-memory state is left completely unchanged — no member is evicted, no
  message is dequeued, no session is invalidated, and no one-time `superseded` entry is consumed

### 13.2
- **Given** multiple rooms each with one or more connected members
- **When** the operator requests the diagnostic dump
- **Then** every room (by name) and every member (by name) currently registered in app state
  appears in the output, and the reported member count per room matches the actual membership

### 13.3
- **Given** a member is connected and its inbox holds the receive-slot machinery
- **When** the dump is produced
- **Then** the member's entry includes every field needed to diagnose a stuck receive: queue
  depth, the receive-active flag, the active generation, the generation counter, whether a
  receive abort handle is currently stored, and the inbox `Arc` strong-reference count

### 13.4
- **Given** a member has queued, undelivered messages
- **When** the dump is produced
- **Then** each queued message is represented individually with its id prefix, sender name,
  queue age, and remaining TTL — in enqueue order

### 13.5
- **Given** members are connected with active sessions, and at least one session has been marked superseded
- **When** the dump is produced
- **Then** each member's session is shown (redacted) together with a flag indicating whether it
  round-trips through the global session map, and the full set of superseded session tokens
  (redacted) is included

### 13.6
- **Given** the dump contains session tokens and other secret-bearing identifiers
- **When** the output is produced
- **Then** no full secret appears anywhere — only a fixed-length prefix (at most 8 characters) is
  emitted; room tokens and seat tokens are never written to the dump in any form

### 13.7
- **Given** an inbox has its receive slot marked active but no live receiver holds it (the active
  generation is nonzero while the inbox `Arc` strong count shows no parked receiver), or a
  session-map entry has no corresponding live member (or vice-versa)
- **When** the dump is produced
- **Then** the output includes an `invariants` section that flags each violation with a
  descriptive warning, and a fully healthy snapshot is clearly distinguishable from a
  desynchronised one (e.g. an explicit "all invariants OK" state versus one or more warnings)

### 13.8
- **Given** an agent will parse the dump for automated analysis
- **When** the dump is produced
- **Then** the body is well-formed YAML that parses without error

### 13.9
- **Given** a member's inbox has one or more extra `Arc` holders (strong count > 1) but no active receive slot **and** no
  in-flight send-with-reply to that member to explain them — the signature of an orphaned frame left alive after a disconnect
- **When** the diagnostic dump is produced
- **Then** the `invariants` section flags it with a descriptive warning (the inverse of the active-slot-with-no-holder
  check), so the "listening but nothing arrives" deadlock is visible rather than reading as healthy
- **And** a *healthy* in-flight send-with-reply — which legitimately raises the recipient's `Arc` count while the recipient
  has no active receive — is **not** flagged, so the diagnostic distinguishes a real orphaned frame from normal traffic and
  does not cry wolf on it

### 13.10
- **Given** a room has both seat-authed and room-token-authed members
- **When** the diagnostic dump is produced
- **Then** each member's entry shows how they authenticated and their idle-eviction status — a room-token member shows a
  countdown (seconds remaining until idle eviction); a seat-authed member shows its own countdown to the dead-seat reaper
  when that reaper is enabled, or `exempt` when it is disabled — so a reapable seat is never mislabelled "exempt" and the
  operator can see who is about to be reaped

### 13.11
- **Given** a session belongs to multiple rooms
- **When** the diagnostic dump is produced
- **Then** the shared inbox and its queued messages are rendered once per session, not duplicated under every room row
- **And** each room row shows only that room's own membership metadata (last seen, auth method, idle/reap countdown)

### 13.12
- **Given** a session that was minted by `connect` and has not (yet) joined any room, alongside sessions that have
- **When** the diagnostic dump is produced
- **Then** every session's entry — roomless or not — shows how long ago it was connected, so an operator can tell a
  session that is merely mid-handshake (`memberships: []`, connected moments ago) from one that is genuinely stuck

### 13.13
- **Given** a seat member whose seat has been superseded (possibly enough to freeze it)
- **When** the diagnostic dump is produced
- **Then** that member's row shows the number of recent supersessions inside the detection window, how long ago the
  last one happened (or that there has been none), and whether the seat is frozen and for how much longer — so
  credential contention is visible without reading the journal, and no token appears in the output
  (`memberships: []`, connected far longer than a `connect`→`join` round-trip should ever take)