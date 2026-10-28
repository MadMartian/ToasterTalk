# ToasterTalk MCP — Technical Architecture

## System overview

Diagram source: [architecture.puml](architecture.puml) — render with PlantUML or a compatible viewer.

**MCP protocol**: Dual transport on a single port — Streamable HTTP (`POST /mcp`, MCP 2025-03-26) and Classic SSE (`GET /sse` + `POST /message`, MCP 2024-11-05). Platform-independent — this half of the server is identical on every host.
**Mgmt protocol**: a local IPC transport, one connection per operation, inline text commands. Linux uses a Unix domain socket (`SOCK_STREAM`) at `/run/toaster-talk/mgmt.sock` (configurable), mode `0660`, directory `0750`, group `operator` — no `sudo` required for group members. macOS uses `/usr/local/var/run/toaster-talk/mgmt.sock`, a persistent parent because launchd has no `RuntimeDirectory=` equivalent; the root-owned daemon makes the socket `root:wheel`, so the CLI requires `sudo`. Windows uses a named pipe at `\\.\pipe\toaster-talk` (configurable), DACL-restricted to the pipe's creator-owner and built-in Administrators — the equivalent operator-only gate, enforced by the OS rather than a Unix group. `mgmt.rs::run_mgmt_listener` and `handle_connection` are `#[cfg(unix)]`/`#[cfg(windows)]`-gated at the bind/accept/connect sites only; the request/response protocol logic is one code path, generic over `AsyncRead + AsyncWrite`. See [CLI ↔ server management protocol](#cli--server-management-protocol) below.
**Auth**: Room tokens are HMAC-SHA256(master_secret, room_name), base64-encoded. Seat tokens are HMAC-SHA256(master_secret, room_name || '\0' || member_name), base64-encoded. Both are deterministic given the server master secret and name inputs — the server recomputes and compares rather than storing raw token values. Session tokens are 32-byte random hex strings issued per connection and regenerated on every new `connect` call. Session lookups use `ct_str_eq` (constant-time string comparison) after the `HashMap` lookup as defense-in-depth, consistent with room and reconnect token handling. All log events include a `session` structured field (8-char token prefix) to correlate log lines without exposing the full token.
**TLS**: HTTPS is not required for the initial MVP. It is recommended for deployments exposed beyond localhost; see ISSUES.md.

## Third-party components

### Rust crates

| Crate | Role |
|---|---|
| `tokio` (full features) | Async runtime; drives all I/O and timer tasks |
| `axum` | HTTP server framework; handles `/mcp` endpoint routing |
| `rmcp` | Official Rust MCP SDK; provides JSON-RPC 2.0 framing, tool dispatch, and MCP type definitions |
| `serde` + `serde_json` | Serialization for MCP messages and tool arguments (JSON-RPC protocol-bound) |
| `serde_yml` | Serialization for the room store (`rooms.yaml`) |
| `uuid` | Session token and message ID generation (`Uuid::new_v4`) |
| `hmac` | HMAC-SHA256 token derivation for room and seat tokens (RustCrypto, audited) |
| `sha2` | SHA-256 digest (required by `hmac`; RustCrypto, audited) |
| `tracing` + `tracing-subscriber` | Structured async-aware logging |
| `clap` | CLI argument parsing for server (`--port`, `--bind`, `--config`) and CLI client subcommands |
| `toml` | Configuration file parsing |
| `arc-swap` | Live config cell (`Arc<ArcSwap<Config>>`) — lock-free hot-swap of tunable settings on reload |
| `base64` | Encoding/decoding room tokens written to `.tok` files |
| `windows-sys` (Windows only) | Win32 bindings used to restrict the mgmt named pipe's DACL to its creator-owner + built-in Administrators (`mgmt.rs::create_pipe_instance`) — already a transitive dependency of tokio's own Windows named-pipe support, so this adds no new supply-chain surface |
| `windows-service` (Windows only) | Service Control Manager integration — hosts the daemon as a native Windows service, mapping `Stop`/`Shutdown` control events to graceful shutdown (`main.rs`'s `windows_service_support` module) |

### System dependencies

| Component | Role |
|---|---|
| systemd (Linux system mode) | Lifecycle management — start, stop, restart, enable at boot; `ExecReload` wires `systemctl reload` to the config/store reload |
| launchd (macOS) | The native equivalent — a `/Library/LaunchDaemons` plist with `RunAtLoad`/`KeepAlive`; no `RuntimeDirectory=` analogue, so the mgmt socket's parent must be a persistent path the installer provisions |
| Windows Service Control Manager (Windows) | The native equivalent — start, stop, automatic-start-at-boot; the mgmt `RELOAD` command is the reload path (no `SIGHUP` equivalent exists) |
| Linux, macOS, Windows | Server and CLI build and run natively on all three; see the platform-conditional code called out throughout this document (`#[cfg(unix)]` / `#[cfg(windows)]`) |

## Integration boundaries

### MCP transports (agent ↔ server)

The server exposes two MCP transports on the same TCP port, both backed by the same `ToasterServer` handler:

**Streamable HTTP** (`POST /mcp`, MCP 2025-03-26 spec):
- Used by Kiro (Amazon) and Claude Code.
- Each tool invocation is a `POST /mcp` with a JSON-RPC 2.0 body; the server streams the response back over the same connection.
- Runs in rmcp's default **stateful** mode (a session per `Mcp-Session-Id`), which keeps the standalone `GET /mcp` SSE push channel available. rmcp answers a request bearing an unknown session id (the case a client holds after a server restart) with HTTP 401, which hosts read as a connection fault and stall on; the `map_session_401_to_404` response layer on the `/mcp` route rewrites that to 404 — the transport spec's reinitialize signal, and the same answer the classic SSE transport already gives for an unknown session. See TDD 1.6 and ANTI-PATTERNS #9.
- Long-running tools (`receive`, `send` with `await_reply_secs`) hold the HTTP connection open. Kiro imposes a hard ~120s transport timeout. Kiro HTTP clients send the `X-Host-Cap-Secs: 110` request header. rmcp injects the raw HTTP request `Parts` (headers included) into each MCP request's extensions; the `call_tool` handler reads the header from there and scopes a `tokio::task_local` (`REQUEST_HOST_CAP`) around the tool dispatch. This is done inside `call_tool` — not in an axum middleware layer — because rmcp's Streamable HTTP transport spawns a separate task per session, and task-locals do not cross `tokio::spawn` boundaries; setting the scope in `call_tool` guarantees the value is visible in the same task where the tool runs. The tool handlers read the task-local and cap their blocking wait accordingly, returning `timeout: capped` so the agent knows to re-poll immediately. Claude Code sends no such header and is unaffected — each request's cap is fully isolated.

**Classic SSE** (`GET /sse` + `POST /message`, MCP 2024-11-05 spec):
- Used by Junie (JetBrains) and any host implementing the older spec.
- Agent opens a persistent `GET /sse` event stream. Each tool invocation is a `POST /message` which returns `202 Accepted` immediately; the tool result arrives later as an SSE event on the open stream. Because the POST completes instantly, the host HTTP client never imposes a wall-clock timeout on long-running tools — fully enabling the long-poll pattern without restriction.

Both transports use the same `rmcp` `ServerHandler` implementation. The `axum` router merges both services under one `TcpListener`.

### CLI ↔ server management protocol

The CLI binary (`toaster-talk-cli`) is a dumb client — it translates subcommands and flags into wire requests sent to the running server. It has no business logic of its own; all room creation, deletion, and listing logic lives in the server.

**Transport**: Linux uses a Unix domain socket (`SOCK_STREAM`) at `/run/toaster-talk/mgmt.sock`, mode `0660`, directory `0750`, group `operator`; macOS uses `/usr/local/var/run/toaster-talk/mgmt.sock` because launchd has no transient runtime directory, and the root-owned daemon requires `sudo` for CLI access. On Windows, a named pipe at `\\.\pipe\toaster-talk` (same config key), whose DACL is restricted at creation to the pipe's creator-owner and built-in Administrators — the OS-native equivalent authorization boundary, since Windows has no analogue of Unix group-owned IPC endpoints. One connection per operation on either platform — the CLI connects, sends a request, reads the response, and closes.

**`RELOAD`**: triggers the same config + room-store reload `SIGHUP` drives on Unix, through this connection instead of a signal — the only reload trigger on Windows, and an explicit alternative to `SIGHUP` on Unix (TDD 8.17). `mgmt.rs::cmd_reload` sends a completion `oneshot` through a channel (`mgmt::ReloadSender`) to the reloader task running in `main.rs` (`run_command_reloader`, armed on every platform, alongside Unix-only `run_sighup_reloader`) and blocks on it, so the CLI's response reflects a reload that has actually run, not merely been queued.

**Name constraints**: Room names and member names must be non-empty, ≤ 40 characters, and must not contain spaces, double-quote characters (`"`), or null bytes (`\0`); member names are further restricted to the `[a-zA-Z0-9_-]` character set. These constraints are enforced server-side at creation time. Room names are validated in `RoomStore::create`; member names are validated by `MemberName::new` (`state.rs`), the only in-code constructor of a `MemberName`, which wraps the shared predicate `toaster_talk_common::is_valid_member_name` and is called by both `join` paths and `RoomStore::create_seat`.

**Token and name types**: `RoomToken`, `SeatToken` (`rooms.rs`), `SessionToken` and `MemberName` (`state.rs`) are single-field newtypes over `String` (not type aliases), so the compiler rejects transposing any two of them — e.g. a member name and a session token in adjacent parameters. All four implement `Borrow<str>` so keyed `HashMap`s/`HashSet`s can be queried with `&str`. `RoomToken`, `SeatToken` and `MemberName` use `#[serde(transparent)]` (identical on-disk representation; `MemberName` is persisted in `MemberSeat`). `SessionToken` is credential-grade and in-memory only: it has no serde impls, its `Debug`/`Display` print only the redacted 8-char `session_id` prefix, and it has no `Deref<Target = str>`, so the full value is reachable only through an explicit `as_str()` (used solely to return it from `connect`). `MemberName` is valid by construction in code — `MemberName::new` validates, there is no unchecked `From<String>` — while its `Deserialize` is unvalidated so loading the room store behaves exactly as before; `Display`/`Deref` expose the plain name. Raw strings are converted at the boundaries: MCP tool params in `server.rs` (`session_token` → `SessionToken`) and the handlers (`member_name` → `MemberName::new` in `join`), and mgmt command arguments in `mgmt.rs` (`KCKM`/`DELMB` names that fail validation can match no member, so they are treated as not found).

**Seat token revocation is non-cryptographic**: because a seat token is `HMAC(master_secret, room_name || '\0' || member_name)`, it is fully determined by the master secret and the room/member names. `DELMB` removes the persisted `MemberSeat` and evicts the active session (via `evict_member_superseded`), but a subsequent `ADDMB` for the same room and member name regenerates the *identical* token. Permanently barring a member therefore requires rotating the master secret or changing the room/member name — see [POLICY.md](POLICY.md#seat-token-revocation).

**`DUMP` diagnostic**: a read-only command taking only read guards on the room store and `AppState` — it mutates nothing and renders a redacted YAML snapshot with two top-level sections: `rooms:` (every room and member, including per-room-membership metadata — last_seen, auth method, idle-eviction countdown, a seat shows its dead-seat-reaper countdown or `exempt` when the reaper is disabled) and `sessions:` (one entry per session, since one shared inbox can now back several room memberships — TDD 13.11). Each session entry shows the room/member labels sharing it, plus its inbox rendered exactly **once** (queue + receive-slot machinery + inbox `Arc` strong count + `reply_waiters`) — never duplicated per room row. The invariants section flags desyncs (e.g. an active receive slot with no live receiver — the "listening but nothing arrives" failure class) computed against a **structural baseline** of `1 (session_inboxes entry) + N (one per room membership sharing that inbox)`, not a flat `1`, now that the inbox is shared. The inverse check — extra `Arc` holders beyond that baseline with no active receive — fires only when no in-flight send-with-reply (`reply_waiters == 0`) explains them, so healthy traffic is not misflagged (ANTI-PATTERNS #12). Secrets are redacted to 8-char prefixes; tokens never appear. The CLI prints the body verbatim for piping to `yq`. See `AppState::diagnostic_snapshot` (`state.rs`) and `render_dump_yaml` (`mgmt.rs`).

The socket is removed on clean shutdown and recreated on startup; a stale socket that cannot be bound causes the server to abort. For the request/response wire framing and the full command table, see SCHEMA.md.

### Token files (operator ↔ agent)

Token files are the out-of-band channel through which the operator shares room credentials with agents. They live at `~/.toaster-talk/tokens/` on Unix/macOS, `%USERPROFILE%\.toaster-talk\tokens` on Windows, and are written by the CLI permissioned so only the owning account can read them — mode `0600` on Unix; on Windows, `create_new` under the user's own profile inherits that profile's own restrictive ACL (owner + `SYSTEM` + `Administrators`), the practical equivalent, since Windows has no `chmod` analogue. That equivalence is inherited from the token directory's ACL, not enforced independently by the file-write code — see SCHEMA.md's Token file formats section for the caveat when `--token-dir` points somewhere with a broader ACL. Room token files are flat in `tokens/`; seat token files are grouped one subdirectory per room (`tokens/user/<room>/`) with the member name as the filename, so seats are self-describing on disk.

Agents scan token files at connect time using a two-phase lookup: first scan `~/.toaster-talk/tokens/user/` **recursively** (seat files live in per-room subdirectories) for a seat token file whose first line matches the room name — if found, use the seat token and member name from that file. If no seat token file matches, fall back to scanning `~/.toaster-talk/tokens/` for a `.tok` file whose first line matches the room name and use the room token from that file.

For cross-machine use, the operator copies the appropriate token file to the remote machine — room token files into `~/.toaster-talk/tokens/`, seat token files into `~/.toaster-talk/tokens/user/<room>/` — before starting the agent there.

For the exact 2-line / 3-line file layouts and the filename-derivation rules, see SCHEMA.md.

### MCP client configuration

Both transports are registered by URL against the same server process on the same port: streamable-HTTP hosts (Claude Code, Kiro) use `http://<host>:6669/mcp`; classic-SSE hosts (Junie) use `http://<host>:6669/sse`. See README §3 for the per-host client-config JSON and the `X-Host-Cap-Secs` header.

## MCP tools

| Tool | Key arguments | Returns | Blocks? |
|---|---|---|---|
| `connect` | `old_session_token?` | `{session_token}` | No |
| `join` | `session_token`, `room_token?`, `seat_token?`, `member_name` | `{room}` | No |
| `leave` | `session_token`, `room_name` | `{room, member_name}` | No |
| `send` | `session_token`, `room_name`, `to`, `message`, `ttl_secs?`, `await_reply_secs?` | `{message_id, ttl_secs, last_seen?, timeout?, remaining?, messages[]}` | Only if `await_reply_secs` set |
| `receive` | `session_token`, `timeout_secs?`, `room?` | `{timeout, remaining, messages[]}` | Yes |
| `members` | `session_token`, `room_name` | `{room, member_count, members[]}` | No |

A few architectural properties the field reference (SCHEMA.md) does not capture:

- **`connect` is a pure identity mint; `join` is the only place a room membership is created.** One session token spans every room an agent belongs to, shared with one inbox (`AppState.session_inboxes`). `connect` never sees a room/seat credential or member name — there is no first-room special case. Every room, the first included, goes through `join`. `connect`'s `old_session_token` rotates the identity and tears down every room membership under it wholesale (no carryover: a bearer-only token presentation must never recover rooms — re-`join` with the room/seat credential is required); recovering a room afterward always goes through `join` again.
- **`leave` is the inverse of `join`, built on the same eviction primitive every other departure path uses.** It resolves `room_name`, confirms the caller's session holds that membership, then calls the same `AppState::evict_member` that CLI `kckm` calls — so last-membership session teardown, in-flight blocking-call abort, and queued-message discard for the vacated room (rubric 10.16) all apply identically to a voluntary leave. It never touches a persisted seat credential in `RoomStore`: a seat holder can `join` the same room again later with the same seat token.
- **Send-with-reply has no dedicated reply tool.** `send` with `await_reply_secs` blocks on the *sender's own* inbox — filtered to the room the `send` targeted (the mandatory counterpart of `receive`'s optional room filter) — the peer replies with a plain `send` in the opposite direction, which unblocks the waiting call. Delivered messages carry `reply_expected_within_secs` so the recipient knows a reply is awaited and how long the sender's window has left.
- **No false delivery.** If the recipient is evicted at enqueue or mid-reply-wait, `send` returns `RecipientEvicted` rather than a `message_id` or a plain reply timeout — a sender is never told a message reached a member who is gone. The effective message TTL is extended to `await_reply_secs + ttl_secs` so the message cannot expire before the reply window closes.
- **`receive`'s room filter is optional, `send`'s is mandatory.** Both reuse the same underlying room-filtered drain primitive (`Inbox::wait_drain_until_filtered`) threaded through the single preemptible receive slot (`park_on_inbox`) — this is *not* a new preemption model, just a predicate parameter on the existing one-inbox-one-slot machinery.
- **Every drain is batch-capped at `max_receive_batch` (default 16).** `park_on_inbox` takes the cap and passes it to `Inbox::wait_drain_until_filtered` → `drain_valid_filtered_capped`, which takes the oldest matching messages up to the cap and leaves the rest queued in their original order, counting them (under the same queue lock) as `remaining`. Both `receive` and the send-with-reply reply-wait pass the same configured cap, so no blocking-read path returns an unbounded batch, and a reply past the cap is left queued for the next `receive` rather than dropped. Because the wait loop drains before parking, a call made while a remainder is queued returns immediately. `max_inbox_messages` bounds memory; `max_receive_batch` bounds a single response.
- **`members` is now a set-membership check**, not equality. A session can belong to more than one room; the caller names which one to enumerate, and a room it has not joined is rejected with `NotConnectedToRoom`.

For full parameter lists, response fields, and the delivered-message object, see SCHEMA.md.

## Module responsibilities

**Server** (`toaster-talk-server`):

| Module | Responsibility |
|---|---|
| `src/main.rs` | Entry point; loads config into the live `Arc<ArcSwap<Config>>` cell; starts dual-transport axum server (`/mcp` + `/sse`), management listener, TTL sweeper, and both reloaders (`run_sighup_reloader`, Unix only; `run_command_reloader`, every platform, backing the mgmt `RELOAD` command). The server body is factored into `run(shutdown_signal)`, driven directly by a plain `#[tokio::main]` on Unix and, on Windows, by the `windows_service_support` submodule — which registers as a native service when launched by the Service Control Manager (mapping `Stop`/`Shutdown` to `run`'s graceful-shutdown future) and transparently falls back to running `run` as a plain console process otherwise (`cargo run`, a manual invocation, a debug session) |
| `src/config.rs` | `Config` struct; TOML deserialization; merge logic (file → env → CLI, last wins); `Config::reload` — re-resolves config for a live reload, retaining restart-only fields and warning on any attempt to change them; platform-conditional defaults for `mgmt_socket`/`store_path`/config path (`/etc`, `/var/lib`, and `/run` on Linux; `/etc`, `/var/lib`, and `/usr/local/var/run` on macOS; `%ProgramData%\ToasterTalk`, `\\.\pipe\toaster-talk` on Windows) and `home_dir()` via `std::env::home_dir()` (`$HOME` / `%USERPROFILE%`) |
| `src/server.rs` | `ToasterServer` handler; wires `rmcp` tool dispatch to handler functions; shared by both transports |
| `src/rooms.rs` | `RoomStore` — persisted room definitions; load/save to disk; room creation, deletion, token verification; `RoomStore::reload` — re-reads `rooms.yaml` for a live `SIGHUP` reload, erroring (not falling back to empty) on a missing/corrupt file |
| `src/state.rs` | `AppState` — in-memory agent registrations and message queues; references `RoomStore`; `room_membership_mut` / `check_room_membership` — shared session→room lookup chain used by `join`, `send`, `receive`, and `members` |
| `src/tools/connect.rs` | `connect` handler — mint a roomless session token, or rotate (and fully tear down) one given `old_session_token`; never touches room/seat credentials |
| `src/tools/join.rs` | `join` handler — the only path that ever creates a room membership; room-token branch (reject on name collision) and seat-token branch (evict-and-supersede the existing holder in that room, unless seat-contention flap detection has frozen the seat — then `SeatContention`) |
| `src/tools/leave.rs` | `leave` handler — the inverse of `join`; resolves the caller's own membership in `room_name` and evicts it via the same `AppState::evict_member` every other eviction path uses, never touching the persisted seat credential |
| `src/tools/send.rs` | `send` handler — resolve `room_name`, enqueue message; optionally await a room-filtered reply via own inbox |
| `src/tools/receive.rs` | `receive` handler — await next message (optionally room-filtered) from the session's shared inbox with timeout; `park_on_inbox` is the one chokepoint every blocking wait (`receive` and send-with-reply) goes through |
| `src/tools/roster.rs` | `members` handler — list members of the caller's own room (session-scoped) |
| `src/host_cap.rs` | `REQUEST_HOST_CAP` task-local — threads the `X-Host-Cap-Secs` header value, read in `call_tool` from the request extensions, into the tool handlers; `apply_timeout(was_capped, timed_out) -> Timeout` — shared cap/elapsed classification used by both `send` and `receive` |
| `src/http_transport.rs` | `map_session_401_to_404` — `map_response` layer on the `/mcp` route that rewrites the streamable transport's unknown-session `401` to `404` (the spec's reinitialize signal), matching the SSE transport so a post-restart stale session prompts a clean reconnect instead of a host-side hang |
| `src/mgmt.rs` | Management protocol handler — serves CLI client requests (`ADDRM`, `ADDRM_FORCE`, `DELRM`, `LSRM`, `GTRM`, `KCKM`, `KCKALL`, `ADDMB`, `DELMB`, `LSMB`, `RELOAD`, `DUMP`) over the platform's mgmt transport (Unix socket / Windows named pipe); `handle_connection` and `dispatch` are one code path shared by both, generic over `AsyncRead + AsyncWrite` |
| `src/ttl.rs` | Background task — on a fixed interval, sweep expired messages, expired rooms, expired seat tokens, idle non-seat members (idle TTL), dead seat members (the dead-seat reaper: a seat idle past `seat_idle_ttl_secs` with no active receive; `0` disables), idle roomless sessions, and stale seat-contention history (`prune_seat_flaps`) |
| `src/error.rs` | Error enum; maps internal errors to MCP JSON-RPC error codes and management protocol errors |

**CLI client** (`toaster-talk-cli`):

| Module | Responsibility |
|---|---|
| `cli/main.rs` | Parses subcommands (`addrm`, `delrm`, `lsrm`, `gtrm`, `kckm`, `kckall`, `addmb`, `delmb`, `lsmb`, `reload`, `dump`); writes room and seat token files locally, permissioned per-platform (`open_token_file`); connects to the server's mgmt transport (`connect()`, `#[cfg(unix)]`/`#[cfg(windows)]`-gated, with retry-on-`ERROR_PIPE_BUSY` on Windows); sends requests to server; prints response |

**Shared crate** (`toaster-talk-common`):

| Module | Responsibility |
|---|---|
| `src/lib.rs` | Dependency-free primitives shared by the server and CLI: `ct_bytes_eq` / `ct_str_eq` (constant-time comparison — single source of truth for the token-verification path), `is_valid_member_name` / `is_valid_room_name` (name validation, breaking the rooms ← tools cycle), `parse_quoted_line` (management-protocol tokenizer used by both `mgmt.rs` and the CLI), `sanitize_filename`, and the duration formatters. Contains no business logic. |

## In-memory state model

Two distinct state layers with different persistence properties:

**Persisted — RoomStore** (survives server restart):
```
RoomStore  ← serialized to disk at configured path
└── rooms: HashMap<RoomToken, Room>
    └── Room
        ├── name: String
        ├── token: RoomToken   (base64, HMAC-SHA256 derived)
        ├── created_at: DateTime
        ├── expires_at: DateTime   (TTL up to 1 year)
        └── seats: HashMap<SeatToken, MemberSeat>   ← NEW
            └── MemberSeat
                ├── member_name: String
                ├── created_at_secs: u64
                └── expires_at_secs: u64
```

**In-memory — AppState** (lost on server restart):
```
AppState  ← Arc<RwLock<_>>
├── rooms: HashMap<RoomToken, RoomState>
│   └── RoomState
│       └── members: HashMap<MemberName, MemberState>
│           └── MemberState                    ← one entry per ROOM MEMBERSHIP, not per session
│               ├── session_token: SessionToken
│               ├── registered_at: Instant
│               ├── last_seen: Instant           ← updated per-room-membership; anchors that room's idle TTL
│               ├── auth: AuthMethod             ← RoomToken | Seat; fixed at join; Seat ⇒ reaped only when dead (idle past seat_idle_ttl_secs, no active receive); exempt only when the reaper is disabled
│               └── inbox: Arc<Inbox>   ← the SESSION's shared mailbox — the same Arc as every other
│                                          room membership under this session, and as session_inboxes[token]
├── sessions: HashMap<SessionToken, HashSet<(RoomToken, MemberName)>>
│                                        ← every room a session belongs to; an empty set is a valid,
│                                          roomless session (fresh from connect, before any join)
├── session_inboxes: HashMap<SessionToken, Arc<Inbox>>
│                                        ← canonical owner of each session's one shared Inbox; created at
│                                          connect, removed when the session's last room membership is
│                                          evicted (or by connect's old_session_token full teardown)
└── seat_flaps: HashMap<(RoomToken, MemberName), SeatFlap>
                                         ← seat-contention history, keyed by SEAT (it must outlive each
                                           superseded MemberState): recent supersession instants inside
                                           the window, last_supersede, and frozen_until while frozen.
                                           Cleared by kckm/kckall/delmb and room delete/expiry; pruned by
                                           the sweep once neither frozen nor inside its window

Inbox   ← one per SESSION, not one per room membership
├── queue: Mutex<VecDeque<QueuedMessage>>
├── notify: Notify          ← wakes a waiting receive when a message is pushed
├── active_gen: AtomicU64   ← 0 = inactive; nonzero = generation of current receive holder
├── gen_counter: AtomicU64  ← monotonically increasing; source of generation numbers
├── receive_abort: Mutex<Option<AbortHandle>>  ← sub-task handle for external preemption
├── closed: AtomicBool      ← set by eviction (under the queue lock); fails-closed new pushes
├── closed_notify: Notify   ← wakes a send-with-reply sender watching an evicted recipient
└── reply_waiters: AtomicUsize ← in-flight send-with-reply calls awaiting a reply from this member;
                                 explains extra Arc holders in the dump so a healthy sender is not
                                 misflagged as an orphaned frame (ReplyWaitGuard; ANTI-PATTERNS #12)

QueuedMessage
├── id: Uuid
├── from: MemberName
├── room: RoomToken                ← which room this message was sent in; the tag `receive`'s optional
│                                     filter and send-with-reply's mandatory filter match against, and
│                                     the basis for each delivered message's `room` field (SCHEMA.md)
├── body: String
├── enqueued_at: Instant           ← used to compute delivery age
├── expires_at: Instant
└── reply_deadline: Option<Instant> ← send-with-reply only; surfaced at delivery as
                                       reply_expected_within_secs (see SCHEMA.md)
```

A session's `Inbox` holds a `Mutex<VecDeque<QueuedMessage>>` and a `Notify`, shared across every room membership that session holds — `MemberState.inbox` is the same `Arc` for all of them, and `AppState.session_inboxes` is its canonical, room-independent home (needed because a freshly-`connect`ed, still-roomless session has an inbox before it has any `MemberState` to hang it off of). The `Mutex` lets a sender's `try_push`, the send-with-reply reply-wait, and `receive` touch the queue without holding the global `AppState` lock. Fire-and-forget `send` only ever pushes — it never drains the sender's own inbox (rubric 2.3, strictly one-way). `Notify` wakes a blocking `receive` when a message is enqueued. On a seat-token `join` supersede, the evicted holder's still-valid, *room-tagged* queue entries for that one room (never its other rooms' traffic, now that the inbox is shared) are drained and carried into the new session's own inbox (`Inbox::seed`, TTL preserved, always safe to call whether or not a receiver is already parked) so messages queued while the member was briefly away are not lost — the mailbox is identity-scoped, not room-membership-scoped (ANTI-PATTERNS #11). `connect`'s `old_session_token` rotation is the one path that deliberately does **not** carry anything over (bearer-only rotation must not recover rooms; see Concurrency model).

**Single preemptible waiter per inbox.** Every block-and-drain on an inbox — both `receive` *and* the send-with-reply reply-wait — goes through one helper, `park_on_inbox` (`tools/receive.rs`). This is a hard invariant: there is at most one *live* waiter parked on any inbox at a time. It exists because `Notify::notify_one()` wakes a single arbitrary waiter, so two waiters on one inbox let an orphaned one steal a message (see ANTI-PATTERNS #8 — the `surfing` deadlock). `receive`'s optional room filter and send-with-reply's mandatory one are just a predicate (`Fn(&RoomToken) -> bool`) threaded through this same one-inbox-one-slot machinery via `Inbox::wait_drain_until_filtered` — not a new preemption model: still one inbox (now shared across a session's rooms), one slot, one live waiter.

The waiter slot is controlled by a generation counter (`active_gen`). 0 = inactive; a nonzero value is the generation of the current holder. When `park_on_inbox` arrives while a slot is already active (e.g. the previous HTTP connection was dropped without cancelling the handler future), it preempts the old one: it aborts the old wait sub-task via `receive_abort` and claims a new generation. The old `ReceiveGuard::drop` calls `release_receive(old_gen)`, a CAS no-op because `active_gen` no longer matches — the new waiter's slot is preserved. The RAII `ReceiveGuard` also handles outer-task cancellation (e.g. `task.abort()` from a test or framework shutdown) by aborting its own sub-task and releasing the slot on drop.

## Concurrency model

- **Shared state**: `Arc<RwLock<AppState>>`. Write locks are held during `connect` (member registration), `send` (last_seen update + recipient lookup), `receive` (last_seen update + inbox claim), and management operations (room create/delete). Write locks are released before any async I/O — inbox operations use the per-inbox `Mutex` rather than the global state lock. Note: `send` and `receive` require write locks (not read locks) because `MemberState::last_seen` must be mutated on every call.
- **Last-seen tracking**: every tool call updates `MemberState::last_seen` under the brief write lock. The same clock drives two independent actions: a *soft* one — on `send`, if the recipient's `last_seen` exceeds `idle_threshold_secs`, a human-readable `last_seen` hint is added to the response (it evicts no one) — and a *hard* one — the sweeper idle-evicts a non-seat member past `member_idle_ttl_secs` (see below). The soft warn-line must sit below the hard reap-line; config validation enforces `idle_threshold_secs ≤ member_idle_ttl_secs`.
- **Member provenance & idle TTL**: `MemberState::auth` records how a member authenticated (`RoomToken` vs `Seat`), fixed for the session's lifetime. It is the sole basis for idle-TTL exemption — a member is exempt **iff** `auth == Seat`. Exemption is never keyed on member *name*: a member who joins via the room-token path under a name colliding with a reserved seat is still mortal, closing the squat-an-exempt-name abuse. A member's idle window slides on `last_seen` (so a working agent is never reaped) and is bounded by the operator-only, mandatory `member_idle_ttl_secs` (`1800..=86400`).
- **Message delivery & close-on-evict**: After the global write lock is released, `send` enqueues via `Inbox::try_push` (which holds only the per-inbox `Mutex`) and notifies waiting receivers via `Notify`. No global lock is held during enqueueing. Because the recipient can be evicted in the window after the lock is released, every eviction path closes the evicted member's inbox (a `closed` flag set **under the queue `Mutex`** by `Inbox::close`, alongside the existing receive-abort). This serialises close-vs-push: a push that wins the lock enqueues into a still-live inbox (acceptable lossy-mailbox if the member is evicted moments later); a push that loses it observes `closed` and returns `PushOutcome::Closed`, which `send` maps to `Error::RecipientEvicted` — never a false `message_id`. A send-with-reply additionally selects on the recipient inbox's `closed_notify`, so it abandons its reply-wait promptly (with `RecipientEvicted`) if the recipient is evicted mid-wait rather than draining the full timeout.
- **Blocking wait (receive and send-with-reply)**: both call `park_on_inbox`, which claims the inbox's single preemptible slot, spawns a Notify-based wait sub-task (loop with deadline), and awaits it. A RAII `ReceiveGuard` aborts the sub-task and releases the slot on drop — handling outer-task cancellation (`task.abort()`) and preemption. If a wait is already active when a new one arrives (e.g. the HTTP client dropped the connection without axum cancelling the handler), the new waiter preempts the old: it aborts the old sub-task via `inbox.abort_existing_receive()`, claims a new generation, and proceeds. The old guard's `release_receive(old_gen)` becomes a CAS no-op, preserving the new holder's slot. This guarantees at most one live waiter per inbox so an orphaned waiter cannot win a `notify_one()` wakeup (ANTI-PATTERNS #8).
- **Send-with-reply**: `send` with `await_reply_secs` enqueues the message, releases the global lock, then awaits the *sender's own* inbox **through `park_on_inbox`** (the same slot `receive` uses), filtered to the room the `send` targeted, with a timeout. The peer's plain `send` (in that same room) delivers into the original sender's inbox, unblocking it. Routing it through the slot — rather than parking directly — is what prevents a disconnected send-with-reply from lingering as a zombie thief.
- **Session lifecycle (`connect`/`join`) and per-room-membership eviction**: one `session_token` spans every room an agent has joined. `connect` mints a roomless session (`AppState::register_session`) or, given a still-valid `old_session_token`, tears that session down wholesale first (`AppState::invalidate_session_fully` — every room membership under it removed, no carryover) before minting the replacement; this is the one place a session is invalidated regardless of how many rooms remain. `join` is the only path that ever attaches a room membership (`AppState::register_room_membership`), sharing the session's existing inbox. Every other eviction path — idle TTL, `kckm`/`kckall`, room expiry, a seat-token `join` collision — funnels through `AppState::evict_one`, which removes just the one `(room_token, member_name)` pair from the session's membership set; the session and its shared inbox are torn down only when that was the *last* remaining membership. If other rooms survive, the shared inbox itself is left running (still-queued messages tagged for other rooms are untouched) — only that one room's own queued traffic is discarded (rubric 10.16 applied per room). A seat-token collision (`AppState::evict_member_superseded`) additionally carries the vacated room's own room-tagged queued messages into the new holder's session (ANTI-PATTERNS #11), and marks the one-time `Superseded` notice only when the eviction happened to empty the victim's last membership — otherwise the victim's other rooms continue completely unaffected (TDD 10.21), and a later call about the vacated room simply reports `NotConnectedToRoom` (TDD 10.22), not `Superseded`.
- **Join vs. room/seat deletion (lock order store → app)**: `join` takes the `RoomStore` read guard, verifies the room or seat token, then takes the `AppState` write lock **while still holding the store guard**, and releases both only after the membership is registered. Every deletion of a room or seat (`DELRM`, `DELMB`, the expiry sweeps) needs the store *write* lock first and evicts from `AppState` only afterwards, so it is serialised against an in-flight join: either the deletion lands first and the join's verification fails exactly as against a nonexistent credential, or the join registers first and the deletion then evicts that membership. Nesting is always store-then-app (the same order `LSRM` and `DUMP` use); no path takes the store while holding `AppState`, so the extra nesting cannot deadlock. Only lock acquisition — and the seat path's existing in-lock inbox drain — is awaited under the guards; no I/O is.
- **Seat-contention flap detection**: a seat-token `join` that supersedes an existing holder records the supersession in `AppState::seat_flaps` (under the same `AppState` write lock — one map lookup and a short deque, no extra lock or await). When the in-window count exceeds `seat_flap_threshold` the seat is frozen for `seat_flap_hold_down_secs` and a WARN is logged; while frozen, a would-be supersede is rejected with `SeatContention` *before* any eviction, so the holder (and its in-flight receive) is untouched. The policy is snapshotted from the live config per call, so a reload retunes it. A lapsed freeze is cleared lazily on the next join and by the sweep.
- **Live config cell**: the resolved `Config` lives behind an `Arc<ArcSwap<Config>>` shared by every handler, the sweeper, and the mgmt listener. Each tool call takes a cheap per-call snapshot (`load_full` for waits held across an `await`, `load` for synchronous reads); the sweeper and mgmt listener re-read at the top of each cycle / connection. A `SIGHUP` handler task re-resolves config from disk via `Config::reload` and atomically `store`s the new snapshot, so a `systemctl reload` retunes the tunable knobs without dropping a connection — even for an agent whose MCP session predates the reload. `Config::reload` retains the restart-only fields (`bind`, `port`, `mgmt_socket`, `store_path`, `master_secret`) from the startup config and logs a WARN for each changed-but-ignored value; the active `log_level` is retuned through a `tracing_subscriber::reload` filter handle. A failed reload (malformed edit) is logged and leaves the running config intact.
- **TTL sweeper**: A `tokio::spawn`ed task wakes every `sweep_interval_secs` and runs four steps. Step 1 takes a read lock to sweep expired messages (using per-inbox `Mutex`). Step 2 takes a write lock to remove expired rooms from `RoomStore`; when a room is removed, **all** connected members are evicted from `AppState` — seat-authed included — so room expiry caps every member regardless of the idle TTL. Step 3 takes a write lock to remove expired seat tokens from `RoomStore` (`remove_expired_seats`); this only affects the persisted seat credentials and does not touch active sessions. Step 4 takes a write lock and idle-evicts non-seat (`auth == RoomToken`) members whose `last_seen` is older than `member_idle_ttl_secs`, skipping any member actively parked in a wait (`is_receive_active()`) since their wait is itself bounded by `max_receive_timeout_secs`. Victims are collected first, then evicted (never mutating while iterating). Step 4 runs after step 3 so a member whose seat was just removed is still judged by its connect-time provenance, not the now-absent seat. Every eviction (steps 2 and 4, plus all CLI/supersede paths) funnels through `evict_one`, which invalidates the session, aborts any in-flight wait, and closes the inbox.

## Configuration

Settings are resolved in this order (last wins): config file → environment variables → CLI flags. Default config file path: `/etc/toaster-talk/config.toml` (override with `--config <path>`). All fields are optional **except `[messages].member_idle_ttl_secs`** and `master_secret_file` — the operator must consciously choose the non-seat member idle TTL, and the server refuses to start without a master secret.

For the full field-by-field schema (types, defaults, env vars, validation constraints, and the reloadable-vs-restart-only classification), see SCHEMA.md.

### Live reload (`systemctl reload` / SIGHUP, or the mgmt `RELOAD` command)

`systemctl reload toaster-talk` (→ `SIGHUP`, Unix only) or `toaster-talk-cli reload` (the mgmt `RELOAD` command, every platform — TDD 8.17) re-resolves the configuration **in place**, without restarting the process, so live MCP connections and sessions are never dropped. Both triggers funnel into the same `apply_reload` function in `main.rs`; the new values take effect for subsequent tool calls, the next sweeper cycle, and the next management connection regardless of which trigger fired. Windows has no `SIGHUP` equivalent, so the mgmt command is its only reload path; on Unix it is an explicit alternative alongside `SIGHUP`.

A subset of fields is **restart-only** — each is bound to a live OS resource or underpins every issued token, so it cannot change in-process. On reload these keep their startup value and a changed value is logged at WARN; apply them with a full `systemctl restart`:

| Reloadable in place | Restart-only |
|---------------------|--------------|
| `log_level`, all `[rooms]` limits/TTLs, all `[messages]` knobs (message/idle TTLs, sweep interval, timeouts, inbox cap) | `bind`, `port`, `mgmt_socket`, `[rooms].store_path` (the *path*), `master_secret_file` |

`master_secret` is restart-only by necessity: rotating it in-process would invalidate every live session and seat token. (`RUST_LOG`, if set in the environment, continues to win over `log_level` across reloads — it is fixed for the process lifetime.)

The same `SIGHUP` also re-reads `rooms.yaml` — the *contents* at `store_path`, not the path itself, are live: `RoomStore::reload` (`src/rooms.rs`) re-parses the file and the reloader task swaps it wholesale into the `Arc<RwLock<RoomStore>>`. This is what lets an operator hand-edit a room's or seat's `expires_at_secs` (there is no `ADDRM`/`ADDMB` "extend" verb) and have it take effect without a restart. A missing or corrupt file is rejected and the running store is kept — unlike `RoomStore::load` at startup, which falls back to an empty store, a live reload must never silently wipe rooms/seats out from under connected agents. No reconciliation with `AppState` is needed: post-connect operations (`send`, `receive`) authorize purely against `AppState`, never against `RoomStore` — only `connect` and the TTL sweeper consult it — so a room or seat removed by hand-edit decays the same way an expired one already does (via the next sweep), not by an immediate disconnect.

## systemd service

The system unit lives at `deploy/toaster-talk.service`, installed to `/etc/systemd/system/`. The architecturally load-bearing lines: `ExecStart` runs `toaster-talk-server --config /etc/toaster-talk/config.toml`; `ExecReload=/bin/kill -HUP $MAINPID` wires `systemctl reload` to the in-place config reload (see Live reload above); `RuntimeDirectory`/`StateDirectory` provision the `/run` and `/var/lib` paths; the service runs as `User=toaster-talk`, `Group=operator`.

The `toaster-talk` system account is created by `make install`. Members of the `operator` group can use `toaster-talk-cli` without `sudo`. Managed via `systemctl [start|stop|reload|enable|status] toaster-talk`.

## macOS service

`make install` dispatches on `uname -s`, so the same target installs the systemd unit on Linux and a launchd daemon on Darwin (`_install-linux` / `_install-macos`). The plist is generated by the Makefile rather than shipped in `deploy/`, because its `ProgramArguments` and log paths are derived from the same install variables — it lands at `/Library/LaunchDaemons/com.toaster-talk.plist` with `RunAtLoad`, `KeepAlive`, and stdout/stderr redirected to `/var/log/`.

Two boundaries differ from systemd and shape the layout. launchd has no `RuntimeDirectory=`, and macOS clears `/var/run` at boot, so the mgmt socket lives under `/usr/local/var/run/toaster-talk/` — a path that survives a reboot, since the server binds the socket but never creates its parent. And the plist declares no `UserName`/`GroupName`, so the daemon runs as root and the socket is `root:wheel`; the `Group=operator` sudo-free CLI access the systemd unit provides has no counterpart here.

## Windows service

`deploy/install.ps1` is the Windows counterpart to `make install`/`make uninstall`: it copies the release binaries into `%ProgramFiles%\ToasterTalk`, generates `%ProgramData%\ToasterTalk\master.secret` and a default `config.toml` if neither exists yet, and registers `toaster-talk-server.exe` as a service named `ToasterTalk` via `New-Service`. Must be run from an elevated PowerShell session; `-Uninstall` reverses the binary/service install while leaving config, the master secret, and the room store in place (the same asymmetry `make uninstall` has with `/etc` and `/var/lib`).

Service lifecycle integration is `main.rs`'s `windows_service_support` module (`#[cfg(windows)]`), using the `windows-service` crate: `service_dispatcher::start` is attempted first; when the process was actually launched by the Service Control Manager it succeeds and blocks on `service_main`, which registers a control handler, reports `Running`, and calls `run()` with a shutdown future that resolves when the SCM sends `Stop` or `Shutdown` — the future is passed to `axum::serve(...).with_graceful_shutdown(...)`, so the MCP HTTP server drains in place exactly as the rest of `run()`'s `tokio::select!` already expects. When `service_dispatcher::start` fails with `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` (1063) — the documented signal that the process was *not* launched by the SCM — it falls back to running `run()` directly as a plain console process, so `cargo run`, a manual double-click, and a debugging session all behave exactly like the Unix build (no service lifecycle, `Ctrl+C`/process-kill to stop).

There is no Windows equivalent of `SIGHUP`; live reload goes exclusively through the mgmt `RELOAD` command (`toaster-talk-cli.exe reload`) on this platform — see Live reload above.
