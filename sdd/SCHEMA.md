# Schema Reference

Field-level contracts for everything that crosses a ToasterTalk boundary: the MCP
tool API, the delivered-message object, the CLI↔server management protocol, the
on-disk token files, and the configuration file. `TECH.md` describes *where* these
boundaries sit in the architecture and *why*; this document is the exact shape of
what flows across them.

All types are named as they appear in the implementation (`string`, `u64`, `uuid`,
`Option<T>` rendered as *optional*). MCP tool responses are emitted as YAML text
(one `Content::text` block); a field marked *optional* in a response is **omitted
entirely** when absent, never rendered as `null`.

---

## MCP tool API (agent ↔ server)

Five tools, dispatched by `ToasterServer` over both transports. Parameter structs
live in `server.rs` (`ConnectParams` / `JoinParams` / `SendParams` /
`ReceiveParams` / `MembersParams`); responses are rendered by the formatters in
`tools/mod.rs`. Every parameter struct denies unknown fields (TDD 9.7) — a call
carrying a field its target tool doesn't accept (for example, `connect` called
with `join`'s `member_name`/`room_token`) is rejected with a structured
validation error rather than having the unrecognized field silently discarded.
This is cross-cutting and not repeated per tool below.

One `session_token` is shared across every room an agent has joined: `connect`
mints the identity, `join` attaches each room membership to it.
**`session_token` is credential-grade** — because it resolves to every room the
session belongs to (not just one), a leaked token lets whoever holds it attach
to any of those rooms via `join`, given their own room/seat credential. Treat it
with the same care as a persisted secret for however long it lives: never log it
in full, never paste it insecurely. Its *lifecycle* is short (minted once per
`connect` episode, reused across that session's calls, invalidated on
eviction/rotation) — short-lived is not the same as safe-to-expose.

### `connect`

Mint a session token — identity only, never a room membership. Takes at most one
optional input.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `old_session_token` | `string` | no | A prior `session_token` to rotate. Omit on a genuine first connect: mints a fresh, roomless token, nothing to invalidate. Supplied and still valid: that session — and *every* room membership under it — is invalidated immediately, and a fresh, roomless token is issued in its place. No room state or queued messages ever transfer; recover each room afterward via `join`, using the room/seat token you still hold. |

**Response:**

| Field | Type | Description |
|-------|------|-------------|
| `session_token` | `string` | Per-session credential (64 hex chars) for all subsequent `join`/`send`/`receive`/`members` calls. |

Errors: a well-formed call always succeeds (a stale/unrecognized
`old_session_token` is silently ignored, not an error — connect always
succeeds with a fresh mint); an unrecognized field is rejected per the
cross-cutting note above.

### `join`

Attach a room membership to an existing session. The **only** path by which any
room membership is ever created — including an agent's very first room. Exactly
one of `seat_token` / `room_token` must be supplied.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_token` | `string` | yes | From `connect`. Missing/unrecognized is a hard `InvalidSessionToken` — `join` never silently mints a session. |
| `seat_token` | `string` | one-of | Line 3 of a 3-line seat token file. Authorises this member specifically; unconditionally evicts (and supersedes) any other session currently holding this room/member. |
| `room_token` | `string` | one-of | Line 2 of a 2-line room token file. Shared-access credential. Never supersedes another session — a name collision is rejected outright (see Errors). |
| `member_name` | `string` | yes | 1–40 chars, `[a-zA-Z0-9_-]`. The name this agent takes in *this* room (may differ from names used in the session's other rooms). |

**Response:**

| Field | Type | Description |
|-------|------|-------------|
| `room` | `string` | The room name just joined. |

Errors: `InvalidSessionToken` (missing/unrecognized `session_token`),
`MemberNameTaken` (room-token path: name in use by a *different* session —
**fatal, non-retryable**, no `old_session_token` escape hatch on `join`),
`AlreadyJoinedRoom` (the caller's own session already belongs to this room —
rejected as redundant, not a no-op), invalid/expired room or seat token,
`RoomExpired`.

### `send`

Send a message to a named member in a room the caller has joined. Non-blocking
unless `await_reply_secs` is set.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_token` | `string` | yes | From `connect`. |
| `room_name` | `string` | yes | The room to send in. Required now that one session's shared inbox can span several rooms — `to` alone no longer disambiguates which room's member namespace to search. |
| `to` | `string` | yes | Recipient member name (must be in the same room). |
| `message` | `string` | yes | Body, max 100 KB. |
| `ttl_secs` | `u64` | no | Message TTL. Default `default_ttl_secs` (660). Rejected if `> max_ttl_secs`. |
| `await_reply_secs` | `u64` | no | Block until a reply *tagged with `room_name`* lands in the sender's own inbox or this elapses (mandatory room-filtered wait — a reply from one of the sender's other rooms does not satisfy it and stays queued). Clamped to `max_await_reply_secs`. Omit for fire-and-forget. |

**Response:**

| Field | Type | Optional | Description |
|-------|------|----------|-------------|
| `message_id` | `uuid` | no | Id of the enqueued message. |
| `ttl_secs` | `u64` | no | **Effective** TTL applied. When `await_reply_secs` is set this is `await_reply_secs + ttl_secs` (so the message cannot expire before the reply window closes). |
| `last_seen` | `string` | yes | Human-readable idle duration (e.g. `"4 hours ago"`), present only when the recipient has not been seen within `idle_threshold_secs`. |
| `timeout` | `"none" \| "elapsed" \| "capped" \| "preempted"` | yes | Present only when `await_reply_secs` was set. `none` = a reply was returned; `elapsed` = the wait expired with no reply; `capped` = an `X-Host-Cap-Secs` host limit cut the wait short (message already delivered — continue with `receive`, never re-`send`); `preempted` = a newer blocking call from the same member claimed the inbox's single receive slot and aborted this wait (nothing delivered, nothing lost — the newer call owns the wait now; almost always means concurrent blocking calls were issued). |
| `messages` | *message[]* | yes | The reply(ies) delivered during a send-with-reply wait. **Only** a send-with-reply ever populates this — fire-and-forget is strictly one-way and never returns inbox messages (rubric 2.3). See [Message object](#message-object). |

Errors: `RoomNotFound` (unknown `room_name`), `NotConnectedToRoom` (the session
has not joined `room_name`), `RecipientEvicted` (recipient gone at enqueue or
during the reply-wait — never returns a `message_id` in this case),
`MemberNotFound` (recipient not present in `room_name`), `InboxFull`,
`MessageTooLarge`, `MessageTtlExceeded`, `InvalidSessionToken`, `Superseded`.

### `receive`

Block until at least one matching message arrives or the timeout elapses.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_token` | `string` | yes | From `connect`. |
| `timeout_secs` | `u64` | no | Max block. Default `default_receive_timeout_secs` (30). Clamped to `max_receive_timeout_secs`. |
| `room` | `string` | no | Restrict this receive to one specific room (name, not token) the session has joined. Omitted (default): blocks until a message from *any* joined room arrives and drains all of them in one batch — each message reports its own `room`. Supplied: blocks and drains only that room's messages, leaving the session's other rooms' messages queued for a later call. |

**Response:**

| Field | Type | Description |
|-------|------|-------------|
| `timeout` | `"none" \| "elapsed" \| "capped" \| "preempted"` | Always present. `none` = messages returned; `elapsed` = wait expired empty; `capped` = host limit cut it short (call `receive` again); `preempted` = a newer blocking call from the same member took over the inbox's single receive slot and aborted this one (empty result, nothing lost — do not treat as a delivery; you issued concurrent receives). |
| `messages` | *message[]* | All matching queued messages in enqueue order, or `[]`. See [Message object](#message-object). |

Errors: `RoomNotFound` (unknown `room` name), `NotConnectedToRoom` (the session
has not joined `room`), `NotJoinedAnyRoom` (`room` omitted and the session
belongs to zero rooms — call `join` first), `InvalidSessionToken`, `Superseded`.

### `members`

List the members of a room the caller's session has joined.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session_token` | `string` | yes | From `connect`. |
| `room_name` | `string` | yes | Must be one of the rooms this session has joined (set-membership check — a session may belong to more than one room); otherwise rejected with `NotConnectedToRoom`. |

**Response:**

| Field | Type | Description |
|-------|------|-------------|
| `room` | `string` | The room name. |
| `member_count` | `int` | Number of connected members. |
| `members` | `string[]` | Member names, sorted (includes the caller), or `[]`. |

### Message object

The element shape of every `messages[]` array (both `send` and `receive`), rendered
by `format_messages_yaml` from an `InboxMessage`.

| Field | Type | Optional | Description |
|-------|------|----------|-------------|
| `id` | `uuid` | no | Message id. |
| `from` | `string` | no | Sender's member name (room-scoped — the sender may use a different name in other rooms). Reply by `send`ing back to this name, in this same `room`. |
| `room` | `string` | no | The room this message was sent in. Present on every message because one session's shared inbox can hold traffic from several rooms at once — always enough context to reply into the right room even when a `receive` with no room filter returns messages from several rooms in one batch. |
| `age` | `string` | no | Human-readable time spent queued, e.g. `"2 minutes in queue"`. |
| `reply_expected_within_secs` | `u64` | yes | Present **only** when the sender issued a send-with-reply (`await_reply_secs`). Seconds remaining in the sender's reply window as of delivery; counts down as the message sits queued and saturates to `0` once closed. Its presence *is* the signal that a reply is expected. Advisory intent stamped at send time — not a live guarantee the sender is still parked. |
| `body` | `string` | no | Message body, rendered as a YAML block scalar (`\|`). Always last (the block scalar consumes following lines). |

**Example** (a received send-with-reply message):
```yaml
timeout: none
messages:
  - id: c012f865-aa56-4d2c-b6ea-cd30f51bdb22
    from: grok
    room: qa-review
    age: "5 seconds in queue"
    reply_expected_within_secs: 1180
    body: |
      Ready when you are — what's the ETA?
```

---

## Management protocol (CLI ↔ server)

`toaster-talk-cli` is a thin client: it translates subcommands into wire requests
over a local IPC transport, one connection per operation — a Unix domain socket
(`SOCK_STREAM`) on Unix/macOS, a named pipe (`\\.\pipe\toaster-talk` by default) on
Windows. Both transports carry the identical text protocol below; `mgmt_socket`
names whichever one applies to the host platform (see Configuration schema). All
room and member logic lives server-side. For the architectural role of this
boundary, see `TECH.md`.

**Request framing:** inline text, `COMMAND [arg ...]\r\n`. String arguments are
double-quoted for structural clarity.

**Response framing:**

| Prefix | Meaning |
|--------|---------|
| `+<message>\r\n` | Success |
| `-<message>\r\n` | Error |
| `*<count>\r\n` | Multi-value: `<count>` data lines follow (one per row) |

**Commands:**

| Command | Request | Success response |
|---------|---------|------------------|
| `ADDRM` | `ADDRM <name> [TTL <days>]` | `+ROOM_CREATED "<name>" <token> <expires_unix>` |
| `DELRM` | `DELRM <name>` | `+ROOM_DELETED "<name>" evicted:<count>` |
| `LSRM` | `LSRM` | `*<n>` then `<name> created:<ts> expires:<ts> members:<n>` per room |
| `GTRM` | `GTRM "<name>"` | `+ROOM "<name>" created:<ts> expires:<ts> members:<n>` then one member name per line |
| `KCKM` | `KCKM "<room>" "<member>" ["<member>" ...]` | `+KICKED "<room>" kicked:<member> [...] [notfound:<member> ...]` |
| `KCKALL` | `KCKALL "<room>"` | `+KICKED_ALL "<room>" evicted:<count>` |
| `ADDMB` | `ADDMB "<room>" "<member>" [TTL <days>]` | `+SEAT_CREATED "<room>" "<member>" <seat_token> <expires_unix>` |
| `DELMB` | `DELMB "<room>" "<member>"` | `+SEAT_DELETED "<room>" "<member>"` |
| `LSMB` | `LSMB "<room>"` | `*<n>` then `"<member>" created:<ts> expires:<ts>` per seat |
| `RELOAD` | `RELOAD` | `+RELOADED` once the reload has actually been applied (TDD 8.17) |
| `DUMP` | `DUMP` | `+DUMP` then a redacted YAML body — the live in-memory snapshot (see `TECH.md` and TDD §13) |

`RELOAD` triggers the same config + room-store reload as Unix's `SIGHUP` (see
`[messages]`/`[rooms]` Reload column below) through this connection instead of an
OS signal — the only reload trigger on Windows, and an explicit alternative to
`SIGHUP` on Unix. The response is held back until the reload has actually run, not
merely queued, so a `-<message>` error means the reload task itself is gone
(should not happen in normal operation) rather than that any individual config
value failed to apply — a bad edit is logged and the previous config kept, exactly
as `SIGHUP` behaves, and still returns `+RELOADED`.

`ADDRM` returns the room token in the response; the CLI writes the token file locally
(permissioned to the owning account only — mode `0600` on Unix, ACL inheritance on
Windows; see Token file formats below) and never sends it back. Room tokens never
appear in `LSRM` output.

**Name constraints** (enforced server-side at creation): room and member names are
non-empty, ≤ 40 chars, and must not contain spaces, `"`, or `\0`; member names are
further restricted to `[a-zA-Z0-9_-]`.

---

## Token file formats (operator ↔ agent)

Out-of-band credential files written by the CLI under `~/.toaster-talk/tokens/`
(`%USERPROFILE%\.toaster-talk\tokens` on Windows). Agents scan them at connect
time: seat files first (recursively under `tokens/user/`), then room files (flat
in `tokens/`).

Permissioned so only the owning account can read them: mode `0600` on Unix. On
Windows there is no `chmod` analog — the CLI creates the file with `create_new`
and relies on the token directory's own ACL being inherited onto it. Under the
default location (`%USERPROFILE%\.toaster-talk\tokens`) that ACL is the profile's
own — owner, `SYSTEM`, and `Administrators` only, the practical equivalent of
`0600`. This is inheritance, not an explicit grant the CLI makes: an operator who
points `--token-dir` at a location with a broader ACL (for example a shared drive,
or a fresh top-level directory that inherited a permissive default) gets whatever
that directory grants, since nothing in `write_token_file_inner`/`open_token_file`
narrows it further. Keep `--token-dir` under a user profile (or another
already-restricted directory) to preserve the `0600`-equivalent guarantee.

**Room token file** — 2 lines, at `~/.toaster-talk/tokens/<sanitized-room>.tok`:
```
<room name>
<base64-encoded room token>
```

**Seat token file** — 3 lines, at `~/.toaster-talk/tokens/user/<room>/<member>.tok`:
```
<room name>
<member name>
<base64-encoded seat token>
```

**Filename derivation** (`sanitize_filename`): the room name (room files) or member
name (seat files) is lowercased, non-alphanumerics stripped except spaces → `_`. On
collision within a directory an incrementing suffix (`_1`, `_2`, …) is appended. Seat
files are grouped one subdirectory per room, so seats are self-describing on disk.

---

## Configuration schema (`config.toml`)

Resolved last-wins: config file → environment variable → CLI flag. Default path
`/etc/toaster-talk/config.toml` on Unix, `%ProgramData%\ToasterTalk\config.toml`
on Windows (override either with `--config`). Each key's env var is the
upper-snake form prefixed `TOASTER_` (e.g. `TOASTER_PORT`). All fields are optional
**except `[messages].member_idle_ttl_secs`** and `master_secret_file`.

The **Reload** column marks whether a live reload — `systemctl reload` (`SIGHUP`,
Unix only) or the mgmt `RELOAD` command (every platform, TDD 8.17) — applies the
change: *live* = takes effect on the next tool call / sweep / mgmt connection;
*restart* = bound at startup, retained on reload with a WARN, needs a full restart.

### Top level

| Key (env) | Type | Default | Reload | Description |
|-----------|------|---------|--------|-------------|
| `bind` (`TOASTER_BIND`) | `string` | `"127.0.0.1"` | restart | MCP listening address (`0.0.0.0` for external). |
| `port` (`TOASTER_PORT`) | `u16` | `6669` | restart | MCP endpoint port. |
| `log_level` (`TOASTER_LOG_LEVEL`) | `string` | `"info"` | live | `trace\|debug\|info\|warn\|error`. `RUST_LOG`, if set, wins for the process lifetime. |
| `mgmt_socket` (`TOASTER_MGMT_SOCKET`) | `path` | `/run/toaster-talk/mgmt.sock` (Unix); `\\.\pipe\toaster-talk` (Windows) | restart | CLI transport address. Unix: socket path (mode `0660`, dir `0750`, group `operator`). Windows: named-pipe name, DACL-restricted to the pipe's creator-owner and built-in Administrators (the equivalent gate — see `TECH.md`). |
| `master_secret_file` (`TOASTER_MASTER_SECRET_FILE`) | `path` | — (**required**) | restart | Path (relative to config dir or absolute) to the master secret: a 40–120 char UTF-8 passphrase. Derives all room/seat tokens via HMAC-SHA256. Server refuses to start without it. |

### `[rooms]`

| Key (env) | Type | Default | Reload | Description |
|-----------|------|---------|--------|-------------|
| `store_path` (`TOASTER_STORE_PATH`) | `path` | `/var/lib/toaster-talk/rooms.yaml` (Unix); `%ProgramData%\ToasterTalk\rooms.yaml` (Windows) | restart\* | Persisted room store. |
| `max_rooms` (`TOASTER_MAX_ROOMS`) | `usize` | `50` | live | Hard ceiling on active rooms. |
| `default_ttl_days` (`TOASTER_DEFAULT_ROOM_TTL_DAYS`) | `u64` | `30` | live | Default room TTL. |
| `max_ttl_days` (`TOASTER_MAX_ROOM_TTL_DAYS`) | `u64` | `365` | live | Hard ceiling on room TTL (1 year). |

\* `store_path` — the *path* is restart-only (it cannot be repointed to a different file
in-process), but the room store's **contents** at that path are re-read on every
reload trigger, same as a config reload: hand-edit `rooms.yaml` (for example to
extend a room's or seat's `expires_at_secs`) and trigger a reload (`systemctl
reload` on Unix, or `RELOAD` via the mgmt protocol on any platform) to pick it up
without a restart. A missing or corrupt file leaves the running store intact, same
as a malformed `config.toml` edit.

### `[messages]`

| Key (env) | Type | Default | Reload | Description |
|-----------|------|---------|--------|-------------|
| `default_ttl_secs` (`TOASTER_DEFAULT_MESSAGE_TTL_SECS`) | `u64` | `660` | live | Default message TTL (11 min). |
| `max_ttl_secs` (`TOASTER_MAX_MESSAGE_TTL_SECS`) | `u64` | `3600` | live | Hard ceiling on message TTL (1 h). |
| `sweep_interval_secs` (`TOASTER_SWEEP_INTERVAL_SECS`) | `u64` | `60` | live | How often the TTL sweeper runs. |
| `idle_threshold_secs` (`TOASTER_IDLE_THRESHOLD_SECS`) | `u64` | `300` | live | Soft "peer has gone quiet" hint threshold. Must be ≤ `member_idle_ttl_secs`. |
| `member_idle_ttl_secs` (`TOASTER_MEMBER_IDLE_TTL_SECS`) | `u64` | — (**required**) | live | Idle TTL for non-seat members; must be `1800..=86400` (30 min–24 h). Server-controlled. Absent / 0 / out-of-range is a fatal startup error. |
| `seat_idle_ttl_secs` (`TOASTER_SEAT_IDLE_TTL_SECS`) | `u64` | `86400` | live | Dead-seat reaper threshold. A seat member idle this long **with no active receive** is reaped (its inbox closed, so senders fail-closed instead of black-holing). Independent of `member_idle_ttl_secs`. `0` disables the reaper (seats fully exempt — the pre-reaper behavior). |
| `default_receive_timeout_secs` (`TOASTER_DEFAULT_RECEIVE_TIMEOUT_SECS`) | `u64` | `30` | live | Default `receive` block when `timeout_secs` omitted. |
| `max_await_reply_secs` (`TOASTER_MAX_AWAIT_REPLY_SECS`) | `u64` | `3600` | live | Hard ceiling on `send` `await_reply_secs`. |
| `max_receive_timeout_secs` (`TOASTER_MAX_RECEIVE_TIMEOUT_SECS`) | `u64` | `3600` | live | Hard ceiling on `receive` `timeout_secs`. |
| `max_inbox_messages` (`TOASTER_MAX_INBOX_MESSAGES`) | `usize` | `100` | live | Per-member inbox cap; a `send` that would exceed it is rejected (`InboxFull`). |

`master_secret` is restart-only by necessity: rotating it in-process would invalidate
every live session and seat token. (Test builds also honour an inline
`TOASTER_MASTER_SECRET`; see `config.rs`.)
