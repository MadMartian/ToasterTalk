# ToasterTalk MCP — Product Definition

## What it is

ToasterTalk MCP is a shared daemon MCP server that provides reliable, authenticated inter-agent message passing for AI agents. It replaces the fragile named-FIFO approach of the original FIFO-based skill with a server-managed message queue that is immune to fd-racing, rogue process hijacking, and silent message loss.

## Background: the original FIFO-based skill

The original FIFO-based skill enabled two AI agents to exchange messages using named pipes (FIFOs) managed by a small C++ utility. Agents called wrapper scripts (`ipc-send.sh`, `ipc-listen.sh`) which in turn invoked the binary.

The design used length-prefixed message framing and `flock`-based write serialisation to prevent partial reads. To avoid the ENXIO deadlock (a sender blocking on `open()` when no reader exists), the sender forked a child process to hold the FIFO's file descriptor open for a configurable timeout, allowing the message to survive in the kernel pipe buffer until a listener arrived.

This approach worked under ideal conditions but proved fragile in practice:

- **Rogue fd holders**: Stale utility processes from cancelled tool calls kept the FIFO open. Because FIFOs are kernel-level first-come-first-served, these orphan processes consumed the next message before the legitimate listener could read it.
- **Silent message loss**: The forked child buffer-hold pattern gave the sender an exit code of 0 regardless of whether anyone actually read the message. Agents had no reliable way to confirm delivery.
- **Deadlock on misordering**: If sender and listener started in the wrong order relative to the child's timeout, the message was lost and both agents stalled waiting for a reply that would never come.
- **Dangerous cleanup**: `ipc-listen.sh` used `pkill -f` to kill stale listeners by pattern-matching against process command lines. This risked killing unrelated processes or the agent's own shell.

ToasterTalk MCP replaces the entire FIFO-based system with a persistent HTTP daemon that owns message queues server-side, with named rooms as the durable unit of agent coordination.

## The problem it solves

When two AI agents need to exchange messages (e.g., a QA agent and a developer agent), they need a channel that:

1. **Delivers messages to the right recipient.** FIFO files are first-come-first-served at the kernel level. Stale or rogue processes that have the FIFO open consume messages intended for the legitimate agent, silently.

2. **Confirms delivery.** The original skill returns exit code 0 even when messages are silently discarded (the forked buffer-hold child exits before any listener reads). Agents waste tokens on elaborate reply-listen retry loops to detect loss that should not happen in the first place.

3. **Does not deadlock.** The FIFO approach requires careful choreography: the listener must be ready before the sender, or the forked child holds the buffer. Misordering or cancellation causes permanent deadlocks that can only be cleared by human intervention. ToasterTalk MCP enforces TTLs at two layers to make indefinite blocking structurally impossible:
   - **Message TTL**: every enqueued message carries an expiry time (caller-specified, default 11 minutes, max 1 hour). The server discards expired messages automatically. A receiver can never be stuck waiting for a message that already expired.
   - **Tool call timeout**: `send-with-reply` and `receive` both require a caller-supplied timeout. The tool call returns — with an empty result — once the timeout elapses, regardless of what the peer is doing. There is no operation in the API that can block indefinitely.

ToasterTalk MCP eliminates all three problems by placing message queues server-side under authenticated access. No process can steal a message without possessing the room token. Messages are delivered exactly-once to the registered recipient. Fire-and-forget sends succeed even if the recipient is not yet listening, and every blocking operation is bounded by a mandatory timeout.

## Who uses it

**Primary user**: AI agents running in separate terminal sessions or shell instances, coordinating on a shared task (e.g., code-review-and-fix workflows, multi-agent pipelines). Agents interact entirely through MCP tools — they do not use the CLI.

**Operator**: The human who provisions rooms via the CLI and instructs each agent which room to join and what member name to use. Beyond that initial setup, the operator does not need to intervene.

## Core capabilities

### Room and member management (CLI)

Rooms are the durable unit of agent coordination. They are provisioned by the human operator using the CLI and persist on disk until explicitly deleted. The CLI is a thin client — it translates commands into wire requests to the running server.

| CLI operation | Description |
|---|---|
| Create room | Derives a room token via HMAC-SHA256, writes a room token file to `~/.toaster-talk/tokens/<name>.tok` (mode 0600), and registers the room with the server. Room TTL is configurable up to 1 year. |
| Delete room | Invalidates the room token on the server. All agents currently connected to that room are disconnected. The local token file is not deleted. |
| List rooms | Shows all active rooms known to the server: name, creation time, TTL, and connected agent count. |
| Add member | Creates a seat token for a named member in a room; writes a seat token file to `~/.toaster-talk/tokens/user/<room>/<member>.tok` (mode 0600). Seat TTL defaults to the room's remaining lifetime. |
| Delete member | Removes the server-side seat credential for a member and invalidates any active session. The local token file is not deleted. |
| List members | Shows all registered seat credentials for a room (no token values shown). |

Room token file format (two lines, `~/.toaster-talk/tokens/`):
```
<room name>
<base64-encoded room token>
```

Seat token file format (three lines, `~/.toaster-talk/tokens/user/<room>/<member>.tok`):
```
<room name>
<member name>
<base64-encoded seat token>
```

The server enforces a maximum number of active rooms (default: 50). Creation is refused when the limit is reached.

### Agent participation (MCP tools)

| Capability | Description |
|---|---|
| Connect | Mint a session token — identity only, no room membership. Called once per agent process; returns `{session_token}`. Optionally rotates a prior still-live token (`old_session_token`), which invalidates it and every room membership under it, with no carryover. |
| Join | Attach a room membership to an existing session. Agent scans local token files — seat tokens first (`~/.toaster-talk/tokens/user/`, searched recursively through its per-room subdirectories), then room tokens (`~/.toaster-talk/tokens/`) — to find the file matching the room name, then passes the token plus its session token and desired member name. This is the only way any room membership is ever created, including an agent's very first room; repeat once per additional room. An agent with a seat token unconditionally evicts (and supersedes) any other session already holding that seat's room/member. |
| Leave | The inverse of join: voluntarily remove the caller's own membership in one named room. Other room memberships the session holds are unaffected; if it was the session's only remaining room, the whole session is torn down. Does not revoke a seat credential — a seat holder can join the same room again later with the same seat token. |
| Fire-and-forget send | Enqueue a message for a named peer in a specific room (session's own); return immediately. Message survives in the queue for the configured TTL (default: 11 minutes). |
| Send-with-reply | Send a message and block until a reply *from that same room* arrives in the sender's own inbox, or the timeout expires. The peer replies using a plain fire-and-forget send back to the original sender. Eliminates the two-step send/listen pattern for request-response exchanges. |
| Receive | Block until a matching message arrives or timeout expires. Omit the room to drain messages from every room the session has joined in one batch (each tagged with its room); name one room to drain only that room, leaving the rest queued. Returns the message(s) and the sender's name. |

## How is it used?

**With room tokens (shared access):**
1. Operator creates a room using the CLI, giving it a name and TTL — a room token file is written to `~/.toaster-talk/tokens/`
2. Operator copies the token file to each agent's machine and tells each agent the room name and their assigned member name
3. Each agent calls connect once (mints a session token), then scans for the room token file and calls join — attaching that room membership to its session
4. Members exchange messages using send (fire-and-forget or send-with-reply, naming the room) and receive (optionally naming the room)
5. Rooms expire automatically when their TTL elapses; the operator can also delete a room early via the CLI
6. To join additional rooms, the same session calls join again per room — one session, many rooms, one shared inbox

**With seat tokens (individual credentials):**
1. Operator creates a room, then runs `addmb` for each participant — seat token files are written to `~/.toaster-talk/tokens/user/<room>/<member>.tok`
2. Each seat token file encodes both the room name and the member name; the agent reads both from the file when it calls join
3. An agent with a seat token can recover after a crash or disconnect by connecting (fresh session token) and then joining that room again — the seat token unconditionally evicts and supersedes any other session still holding that seat's room/member; still-queued mail tagged for that room carries over
4. Operator can revoke individual access via `delmb` without affecting other members

## Vocabulary

| Term | Definition |
|---|---|
| **Room** | A named, persistent group in which N members can exchange messages, identified by a unique room token |
| **Room token** | The cryptographic credential that grants access to a room; shared among all intended members by the operator via token files |
| **Seat token** | A per-member credential derived from the master secret; authorises exactly one named member to connect to one room; an agent holding a seat token can always evict its own stale session on reconnect |
| **Session** | The shared, multi-room identity bound to one `session_token`, minted by `connect`. One session can span several rooms at once — an agent holds exactly one session regardless of how many rooms it has joined. |
| **Room membership** (or **active room membership**) | What a room token or seat token grants within one specific room, attached to a session via `join` — distinct from the session itself, which can hold several of these at once. A session's room memberships can even use different member names in different rooms. |
| **Session token** | A per-session credential minted by `connect` (identity only, no room membership) and shared across every room the session subsequently joins via `join`; regenerated on rotation (`old_session_token`); used to authenticate all subsequent `join`/`send`/`receive`/`members` calls without re-exposing any room or seat token. Now credential-grade: a leak exposes every room the session belongs to (via `join`, given the attacker's own room/seat credential), not just one — handle it with the same care as a persisted secret. |
| **Member** | A participant actively connected to a room under a registered name (one room membership) |
| **Token file** | A `.tok` file on disk containing credentials written by the CLI. Room token files (two lines: room name, room token) live in `~/.toaster-talk/tokens/`. Seat token files (three lines: room name, member name, seat token) live in `~/.toaster-talk/tokens/user/<room>/<member>.tok`. Both are written mode `0600`. |
| **Inbox** | A session's private queue of messages waiting to be received, shared across every room membership that session holds; each queued message is tagged with the room it arrived from |
| **Message TTL** | The lifespan of an unread message; expired messages are discarded automatically. When `await_reply_secs` is set, the effective message TTL is `await_reply_secs + ttl_secs` to prevent expiry before the reply window closes. |
| **Room TTL** | The lifespan of a room; when elapsed, the room is automatically deleted and its token invalidated |
| **Last seen** | The time elapsed since a member last made any tool call; included in responses when the intended recipient may be offline, and the clock that drives the member idle TTL |
| **Member idle TTL** | The maximum time a member who joined with a plain room token may sit inactive before the server evicts them; server-configured (mandatory, 30 min–24 h), it reaps abandoned agents so they don't linger as phantom members. |
| **Dead-seat reaper** | The counterpart for seat members: a seat that has sat inactive past a generous server-configured window (default 24 h) **and is not actively waiting for messages** is reaped, so a crashed seat holder stops silently swallowing messages sent to it. A live seat (one holding a receive) is never reaped, and the reaper can be disabled to restore the older always-exempt behavior. Reaping removes only the stale live session — the seat credential and its reserved name persist, so the holder rejoins with the same name at any time. |
| **Reply-expected hint** | On a delivered message, the `reply_expected_within_secs` field signals that its sender issued a send-with-reply and is awaiting a reply, giving the seconds still remaining in that window as of delivery. Absent on fire-and-forget messages. It is an advisory intent stamped at send time, not a guarantee the sender is still parked. |

## What it is not

- **Not a general-purpose message broker.** No topics, fanout, or message durability. See `PLAN.broadcast.md` for the deferred broadcast/multi-consumer design.
- **Messages are not persistent.** Message queues live in memory only and do not survive server restarts. Rooms and their tokens do survive restarts — only the in-flight messages are lost.
- **Not a public service.** Designed for trusted network use. Room tokens are the only auth layer — deploy behind a firewall or VPN if exposed beyond localhost.
- **Not multi-consumer.** Each message is delivered to exactly one recipient. Multi-consumer delivery is part of the broadcast plan.
