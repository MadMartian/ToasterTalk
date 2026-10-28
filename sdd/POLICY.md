# ToasterTalk MCP — Development Policy

## Build

- Build with `cargo build --release` for all testing and deployment. Debug builds are acceptable for rapid iteration but never for performance or timing-sensitive tests.
- Minimum Rust edition: 2021. Minimum toolchain: stable. Do not use nightly-only features.
- The project produces two binaries: `toaster-talk-server` and `toaster-talk-cli`. Both are built from the same workspace.

## Build pipeline

Before any change is considered valid, run these steps in order:

1. `cargo fmt --check` — formatting must be clean
2. `cargo clippy --all-targets -- -D warnings` — zero warnings permitted
3. `cargo build --release` — must compile cleanly
4. `cargo test` — all tests must pass

Do not skip any step. If `clippy` emits a warning, fix it — do not suppress it with `#[allow(...)]` unless there is a documented reason in a comment on the same line.

## Testing

### Unit tests

- Every tool handler (`connect`, `join`, `leave`, `send` [both variants], `receive`, `members`) must have unit tests covering at least the happy path and the primary error case.
- `RoomStore` persistence (load/save) must have unit tests covering round-trip serialization and handling of a missing or corrupted store file.
- Configuration merge logic (file → env → CLI precedence) must have unit tests for each override level.
- Run with `cargo test`.

### Integration tests

- Integration tests live in `tests/` and spin up a real server instance on a random port.
- After any change to a tool handler, run integration tests and verify at least one full round-trip: join room → send → receive.
- After any change to `RoomStore`, verify that a room created before a server restart is still accessible after restart.

### Manual multi-agent integration tests

Some behaviors — the real MCP transport, client disconnect / reconnect (supersede), and true cross-session concurrency — cannot be exercised by `cargo test`: in-process helpers bypass the transport/disconnect path and produce false greens (see ANTI-PATTERNS #5, #7, #9). They are covered by a manual multi-agent suite in `toaster-talk-server/tests/MANUAL-TEST.md`, driven against an isolated dev instance (`make dev-up`, `make manual-test`, `make dev-down`).

- **Run the manual suite after any change touching**: session lifecycle (connect / supersede / evict), the inbox or receive slot, send-with-reply, the HTTP/SSE transport, or the TTL sweep. A change limited to config parsing, documentation, or a leaf that none of those depend on does not require it.
- Each scenario asserts against a documented operator-`dump` signature. **Dump-verify every concurrency step before reading its outcome** — an unverified interleaving is not a result (ANTI-PATTERNS #13).
- The dev instance is fully isolated from production (port 6668 vs 6669, ephemeral state, per-invocation MCP registration); the suite must never touch a production room.

## Input and output sanitization

- **Room token**: must be base64-encoded and match the stored value exactly. Reject on any mismatch — do not attempt partial matches or case-folding.
- **Session token**: validate on every tool call. Return an auth error immediately if not found in `AppState` — do not proceed with partial state.
- **Member name**: reject names that are empty, exceed 40 characters, or contain characters outside `[a-zA-Z0-9_-]`. Return a clear validation error.
- **Message body**: enforce a maximum size of 100 KB. Reject larger payloads with a descriptive error before enqueuing.
- **TTL values**: reject message TTL above `max_ttl_secs` (config); reject room TTL above `max_ttl_days` (config). Never silently clamp — return an error so the caller knows their value was refused.
- **Token file**: on write, set file permissions to `0600`. Room token files are written flat under `tokens/`; seat token files are written under a per-room subdirectory, `tokens/user/<room>/<member>.tok` (room → subdirectory, member → filename, both passed through `sanitize_filename`). If the derived filename already exists within its directory with exactly the contents about to be written, reuse that file and write nothing; if it exists with different contents, append an incrementing numeric suffix (`_1`, `_2`, …) until a free slot is found (up to 999) — an identical file at a suffixed name is likewise reused. If no free slot exists, abort with an error — do not silently overwrite.
- **Tool parameters**: every MCP tool's parameter struct must carry `#[serde(deny_unknown_fields)]`. A call naming a field its target tool doesn't accept must be rejected with a structured validation error, never silently deserialized with the unrecognized field dropped — applies to any new tool added later, not just the current six.

## Code style

- Propagate errors with `?`, mapping to the appropriate type in `src/error.rs`.
- All tool handlers must return structured errors mapping to MCP JSON-RPC error codes via `src/error.rs`.
- Log thoroughly at every level. Some bugs escape unit and integration tests and can only be diagnosed from log traces. Sparse logging makes those bugs undiagnosable.
- The primary consumer of logs is the Claude Code agent tasked with diagnosing and fixing bugs. Log messages must be precise, unambiguous, and self-contained enough that an agent reading them can understand exactly what the system was doing at the time of a failure — without needing to ask clarifying questions or cross-reference source code.
- Level guidelines:
  - `tracing::error!` — unrecoverable errors, unexpected state, panics caught at boundaries
  - `tracing::warn!` — recoverable anomalies: invalid tokens, unknown members, TTL violations, `last_seen` exceeding idle threshold, room at capacity, forced member eviction (room expired, preempted, or idle TTL expired)
  - `tracing::info!` — lifecycle events: server start/stop, room created/deleted/expired, member joined/left, room TTL sweep results
  - `tracing::debug!` — per-operation detail: every tool call with its arguments (excluding body), enqueue/dequeue events with message ID and TTL, session token issuance, config values resolved at startup
  - `tracing::trace!` — fine-grained internal state: lock acquisition/release, each step of config merge, token file scan results
- Include enough context in every log line that an agent can reconstruct what happened without needing a debugger or asking follow-up questions. Prefer structured fields (`tracing::debug!(room = %room_name, member = %member_name, "joined")`) over unstructured strings — structured fields are easier for an agent to parse and correlate across multiple log lines.

## Dependencies

- New dependencies require a documented rationale. Prefer crates already in the workspace. One-function utilities that are trivially implemented in std do not warrant a new crate.
- Use YAML for human-readable output; binary or nothing for non-human-readable formats. JSON is permitted only where the protocol mandates it (MCP JSON-RPC 2.0).
- Current crates and their covered responsibilities:

| Crate | Covers |
|---|---|
| `tokio` | Async runtime, timers, channels |
| `axum` | HTTP server, `/mcp` endpoint |
| `rmcp` | MCP JSON-RPC framing and tool dispatch |
| `serde` + `serde_json` | Serialization — MCP messages only (JSON-RPC protocol-bound) |
| `serde_yml` | Serialization — room store (`rooms.yaml`) |
| `toml` | Config file parsing |
| `uuid` | Token and message ID generation |
| `base64` | Room token encoding for token files |
| `hmac` | HMAC-SHA256 implementation — RustCrypto, audited; used for token derivation |
| `sha2` | SHA-256 digest — RustCrypto, audited; required by `hmac` |
| `clap` | CLI argument parsing |
| `tracing` + `tracing-subscriber` | Structured logging |
| `windows-sys` (Windows-only target dependency) | Win32 bindings for the mgmt named pipe's DACL (`mgmt.rs`) — already a transitive dependency of tokio's own Windows named-pipe support |
| `windows-service` (Windows-only target dependency) | Service Control Manager integration (`main.rs`'s `windows_service_support`) |

## Architecture rules

- The server must only bind to the address specified in configuration (default `127.0.0.1`). Do not hard-code any binding address.
- `RoomStore` (persisted) and `AppState` (in-memory) must remain strictly separated. Room definitions live in `RoomStore`; agent registrations and message queues live in `AppState`. Do not merge them or store message queue state on disk.
- Session tokens and message IDs must use cryptographically random sources (`uuid::Uuid::new_v4`). Room tokens and seat tokens are HMAC-SHA256 derived from the server master secret (deterministic given the secret and name inputs). Sequential IDs, timestamps, and predictable values are forbidden for tokens.
- Each member's inbox is a `Mutex<VecDeque>` + `Notify` + `AtomicU64` (`active_gen`) stored as `Arc<Inbox>` in `AppState`. The `active_gen` counter enforces a **single preemptible waiter per inbox**: 0 = inactive, nonzero = generation of the current holder. A new waiter preempts a stale one by aborting its sub-task via `receive_abort` and claiming a fresh generation; the old `ReceiveGuard::drop` becomes a CAS no-op. The RAII `ReceiveGuard` releases the slot on drop, handling cancellation, normal return, and panic.
- **Every block-and-drain on an inbox must go through `park_on_inbox` (`tools/receive.rs`) — never park on an inbox's `Notify` directly.** `Notify::notify_one()` wakes one arbitrary waiter, so two waiters on one inbox let an orphaned one (a future the framework kept alive after a client disconnect) steal a message. `park_on_inbox` is the single chokepoint that guarantees at most one live waiter. Both `receive` and the send-with-reply reply-wait use it; any future blocking-read path must too. See ANTI-PATTERNS #8.
- Write locks on `AppState` must be held for the minimum possible duration. Acquire the lock to look up or mutate state, release it before any async I/O. Never hold a lock across an `await` point.
- The CLI binary (`toaster-talk-cli`) must contain no business logic. All room creation, deletion, and listing logic lives in the server. The CLI translates subcommands into wire requests and prints responses.
- All data managed by this system must have a TTL. Configuration files and token files are the only exceptions.
- **The non-seat member idle TTL is server-controlled only.** `member_idle_ttl_secs` lives solely on `Config`; it must never become a parameter of any MCP tool or any management-socket command. Agents have no wire path to it — they must not be able to set, extend, disable, or read it as a knob. It is **mandatory with no default** (the operator must consciously choose it) and bounded to `1800..=86400`; absent, zero, or out-of-range is a fatal startup error. Disabling the idle TTL for non-seat members is prohibited.
- **Idle-eviction exemption is conferred by authentication method, never by member name.** Only a member that connected with a valid **seat token** (`MemberState::auth == Seat`) is exempt from the idle TTL. Do not key exemption on "a seat exists for this member name": a member could then join via the unprivileged room-token path under a colliding name and live forever while authenticated by the unprivileged mechanism. Provenance is recorded at connect time and is fixed for the session's lifetime.
- **No eviction path may report a false delivery.** Every member eviction (idle TTL, `kckm`, `kckall`, room delete/expiry, seat supersede) funnels through `evict_one`, which invalidates the session, aborts any in-flight blocking wait, and closes the member's inbox. A `send` whose recipient is evicted during the call must surface `RecipientEvicted` — never a `message_id` (fire-and-forget) or a plain reply timeout (await-reply) — and a sender must never block past the bounded reply timeout.

## Management socket authorization

- The management socket (`mgmt.sock`) is created with mode `0660`, owned by the service account and the operator group. Authorization to run **any** management command (`ADDRM`, `ADDRM_FORCE`, `DELRM`, `LSRM`, `GTRM`, `KCKM`, `KCKALL`, `ADDMB`, `DELMB`, `LSMB`, `RELOAD`, `DUMP`) is therefore by **Unix group membership**, not by any room or seat token. There is no per-command credential check: any process that can open the socket is treated as a trusted operator. Restrict membership of the operator group accordingly.
- On Windows there is no socket file or group to own; the equivalent boundary is the named pipe's DACL, restricted at creation (`mgmt.rs::create_pipe_instance`) to the pipe's creator-owner and built-in Administrators only. The same "no per-command credential check" rule applies — any process that can open the pipe is a trusted operator.
- Diagnostic output (the `DUMP` command and any future state-inspection command) must never contain a full token of any kind. Session tokens are redacted to a fixed prefix of at most 8 characters (reuse the `session_id` helper); room tokens and seat tokens must never appear in any form, not even a prefix. Resolve room *names* from the store for display rather than emitting room tokens. Diagnostic commands must be strictly read-only — they take read guards only and must not evict members, dequeue messages, invalidate sessions, or consume one-time `Superseded` entries.
- The master secret must never be passed via a plain environment variable in production — that exposes it through `/proc/<pid>/environ`. The `TOASTER_MASTER_SECRET` inline variable is gated behind `#[cfg(test)]` and is read only in test builds. Production deployments must use `TOASTER_MASTER_SECRET_FILE` or `master_secret_file` (a `0600` file owned by the service account).

## Seat token revocation

- Seat tokens are deterministic: `HMAC(master_secret, room_name || '\0' || member_name)`. `DELMB` removes the persisted seat and immediately evicts any active session (delivered to the agent once as `Superseded`), **but it does not invalidate the credential** — running `ADDMB` again for the same room and member name regenerates the identical token, and any copy of the old token works again. Permanently excluding a member requires rotating the master secret (delete and recreate every room) or changing the room/member name. To revoke, always use `delmb` rather than deleting a token file from disk.

## Deployment

### Linux

- Build first (as the user, not root): `make build`
- Install (requires root): `sudo make install` — copies binaries to `/usr/local/bin/`, installs the systemd unit to `/etc/systemd/system/`, creates the `toaster-talk` system account if absent, writes a default config to `/etc/toaster-talk/config.toml` if none exists, and generates `/etc/toaster-talk/master.secret` if absent (mode `0600`, owned by the `toaster-talk` service account, per the master-secret rule above).
- Systemd system service: `deploy/toaster-talk.service`. Enable with `sudo systemctl enable --now toaster-talk`.
- Default config: `/etc/toaster-talk/config.toml`. Default token directory (operator/agent side): `~/.toaster-talk/tokens/`.
- To update: run `make build`, then `sudo make install`, then `sudo systemctl restart toaster-talk`.

### macOS

- Build first (as the user, not root): `make build`
- Install (requires root): `sudo make install` — the same target dispatches on `uname -s`, so it takes the launchd path on Darwin. Copies binaries to `/usr/local/bin/`, generates `/etc/toaster-talk/master.secret` if absent, writes a default config to `/etc/toaster-talk/config.toml` if none exists, and emits `/Library/LaunchDaemons/com.toaster-talk.plist`. Start with `sudo launchctl bootstrap system /Library/LaunchDaemons/com.toaster-talk.plist` — use `bootstrap`, not the legacy `load`: Apple's `launchctl` man page deprecates `load`/`unload` in favor of `bootstrap`/`bootout`, and `load` frequently fails outright for system `LaunchDaemons` on current macOS with an opaque `Load failed: 5: Input/output error`.
- Default config: `/etc/toaster-talk/config.toml`. Mgmt socket: `/usr/local/var/run/toaster-talk/mgmt.sock` (see the macOS service section in TECH.md for why not `/var/run`). Default token directory (operator/agent side): `~/.toaster-talk/tokens/`.
- The daemon runs as root, so the mgmt socket is `root:wheel` `0660` and `toaster-talk-cli` requires `sudo` — there is no `operator`-group equivalent on this platform.
- There is no `systemctl reload` equivalent — use `sudo toaster-talk-cli reload` for a live config/room-store reload. `SIGHUP` still works if you signal the process directly.
- To update: `make build`, `sudo make install`, then `sudo launchctl bootout system/com.toaster-talk` + `bootstrap system /Library/LaunchDaemons/com.toaster-talk.plist` the plist.

### Windows

- Build first: `cargo build --release`
- Install (requires an elevated PowerShell session): `.\deploy\install.ps1` — copies binaries to `%ProgramFiles%\ToasterTalk`, registers the `ToasterTalk` Windows service, generates `%ProgramData%\ToasterTalk\master.secret` if absent, and writes a default config to `%ProgramData%\ToasterTalk\config.toml` if none exists. `.\deploy\install.ps1 -Uninstall` reverses it (config/secret/store left in place, matching `make uninstall`'s treatment of `/etc` and `/var/lib`).
- Start/stop/status: `Start-Service ToasterTalk`, `Stop-Service ToasterTalk`, `Get-Service ToasterTalk`.
- Default config: `%ProgramData%\ToasterTalk\config.toml`. Default token directory (operator/agent side): `%USERPROFILE%\.toaster-talk\tokens`.
- To update: rebuild, then re-run `.\deploy\install.ps1` (updates the service's binary path in place), then `Restart-Service ToasterTalk`.
- There is no `SIGHUP`/`systemctl reload` equivalent — use `toaster-talk-cli.exe reload` for a live config/room-store reload on this platform (see the mgmt protocol's `RELOAD` command in SCHEMA.md).

## Debug output

- Logging is controlled by the `RUST_LOG` environment variable (via `tracing-subscriber`). Both the systemd unit and the launchd plist set `RUST_LOG=info` by default — sufficient for normal operation.
- For troubleshooting, escalate progressively:
  - `RUST_LOG=toaster_talk=debug` — full operation detail without internal noise
  - `RUST_LOG=toaster_talk=trace` — maximum verbosity; use when debug is insufficient to pinpoint an issue
  - `RUST_LOG=trace` — includes dependency crate output (tokio, axum, rmcp); use only when the bug may be in a dependency
- Override in the running systemd unit without a restart: `sudo systemctl set-environment RUST_LOG=toaster_talk=debug && sudo systemctl restart toaster-talk`
- Log output goes to stdout/stderr. On Linux systemd captures it — view with `journalctl -u toaster-talk -f`; on macOS launchd redirects it to `/var/log/toaster-talk.log` and `.err`.
- Per the global CLAUDE.md convention, any temporary debug output added during development must be prefixed with `[🐛DEBUG]` and removed before committing.
