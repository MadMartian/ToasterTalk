# ToasterTalk MCP

[![CI](https://github.com/MadMartian/ToasterTalk/actions/workflows/ci.yml/badge.svg)](https://github.com/MadMartian/ToasterTalk/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

A reliable MCP server for inter-agent communication — persistent, authenticated rooms where AI agents exchange messages with exactly-once delivery, across terminals, machines, and platforms.

## What it does

AI agents running in separate terminal sessions — or on separate machines — send and receive messages through a shared daemon. Rooms are persistent, authenticated spaces where N agents meet under a shared token. Messages are queued server-side and delivered exactly once, each only to the member it was addressed to. Every blocking operation has a mandatory timeout, so agents can never deadlock waiting for each other indefinitely.

The usual way to divvy up work — spawning sub-agents — stops at the machine boundary: a coordinator can't spawn a sub-agent on another box. ToasterTalk makes agents on different machines and platforms first-class peers instead: an agent on a Linux workstation can confer with one on a Windows machine working the same project, over nothing more than HTTP to the shared daemon.

## Features

- **Deadlock-free by construction**: message TTLs and mandatory tool timeouts make indefinite blocking structurally impossible
- **Exactly-once delivery**: messages are queued on the server until their recipient collects them — never dropped, never duplicated, never handed to the wrong peer
- **Persistent rooms**: named rooms support any number of members; one shared token gets everyone in
- **Cross-machine, cross-platform**: agents on different hosts and OSes coordinate as peers — where sub-agent hierarchies can't reach past their own machine
- **Fire-and-forget or round-trip**: send without waiting, or block until the peer replies — one tool handles both
- **Staleness awareness**: every delivered message includes its queue age; tool responses flag members who haven't been seen recently, and crashed agents don't linger as phantoms — non-seat members are reaped after a server-configured idle TTL, and a dead seat (idle past a generous dead-seat window with no active receive) is reaped too so its inbox stops black-holing messages. A seat holder rejoins with the same name and reserved credential at any time, and messages queued while it was briefly away are carried over on reconnect
- **Simple operator setup**: one CLI command creates a room and writes a token file; agents find it automatically
- **Zero-downtime reconfiguration**: `systemctl reload` (Unix) or `toaster-talk-cli reload` (any platform) re-reads tunable settings in place — retune TTLs and timeouts without dropping a single agent connection

## Requirements

| Requirement | Details |
|---|---|
| OS | Linux, macOS, or Windows — native service integration on all three (systemd / Windows Service Control Manager) |
| Rust | 1.75+ stable |
| Claude Code | MCP HTTP transport support |

## Quickstart

```bash
git clone https://github.com/MadMartian/ToasterTalk.git
cd ToasterTalk
```

### 1. Install and start the daemon

**Linux / macOS:**

```bash
# Build as your normal user (cargo must be in PATH)
make build

# Install to /usr/local/bin/ (requires root)
sudo make install

# Enable and start the system daemon
sudo systemctl enable --now toaster-talk

# Verify it's running
systemctl status toaster-talk
```

`make install` creates the `toaster-talk` system account if it does not exist. Members of the `operator` group can run `toaster-talk-cli` without `sudo`.

**Windows:**

```powershell
# Build (cargo must be in PATH)
cargo build --release

# Install and register the service (run from an elevated PowerShell session)
.\deploy\install.ps1

# Start it
Start-Service ToasterTalk

# Verify it's running
Get-Service ToasterTalk
```

`install.ps1` writes a default config and generates a master secret under `%ProgramData%\ToasterTalk` on first run.

> **Changed a setting?** Edit `config.toml` and run `sudo systemctl reload toaster-talk` (Linux/macOS) or `toaster-talk-cli reload` (any platform, including Linux/macOS) — the daemon re-reads its tunable settings (message/room TTLs, timeouts, inbox cap, log level, …) in place **without dropping any agent connection**. The same reload also re-reads `rooms.yaml`, so a hand-edited room/seat expiry takes effect too. A few fields are restart-only (`bind`, `port`, `mgmt_socket`, the `store_path` *location* itself, `master_secret`) and need a full restart; a reload that changes one keeps the running value and logs a warning.

### 2. Create a room

Run this once before assigning agents to a task. Anyone who needs to communicate goes into the same room.

```bash
toaster-talk-cli addrm "qa-review"
# Token file written to ~/.toaster-talk/tokens/qa-review.tok
# Agents find this file automatically when they connect.
```

For per-member credentials instead of a shared room token, create a **seat** for each
participant — a `join` with that seat token unconditionally evicts (and supersedes)
whatever session currently holds that seat's room/member, so an agent recovers by
simply connecting and joining again:

```bash
toaster-talk-cli addmb "qa-review" "dev"
# Seat token file written to ~/.toaster-talk/tokens/user/qa-review/dev.tok
```

### 3. Configure your AI client

The server exposes two MCP transports on the same port. Use the one your host supports:

| Host | Transport | URL |
|---|---|---|
| Claude Code | Streamable HTTP (MCP 2025-03-26) | `http://localhost:6669/mcp` |
| Amazon Kiro | Streamable HTTP (MCP 2025-03-26) | `http://localhost:6669/mcp` |
| JetBrains Junie | Classic SSE (MCP 2024-11-05) | `http://localhost:6669/sse` |

#### Claude Code

Add the server to `~/.claude/settings.json` (global — available to all Claude Code sessions):

```json
{
  "mcpServers": {
    "toaster-talk": {
      "type": "http",
      "url": "http://localhost:6669/mcp"
    }
  }
}
```

Or add it to `.claude/settings.json` inside a specific project directory to limit the scope.

> **Long-lived listeners — disable Claude Code's idle timeout.** As of Claude Code
> v2.1.187, the client aborts any single MCP tool call after `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`
> milliseconds of no output (default `300000` = 5 min) and re-issues it. A `receive`
> asked to wait 8 hours is therefore chopped into ~5-minute round-trips, each leaving a
> briefly-orphaned waiter server-side. To let a single `receive` block uninterrupted,
> disable the idle timeout in `~/.claude/settings.json` (global) on the **listening
> agent's** host:
>
> ```json
> {
>   "env": { "CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT": "0" }
> }
> ```
>
> Alternatively, leave the timeout in place and add an `X-Host-Cap-Secs` header (see
> [Host cap](#host-cap-mcp-hosts-with-hard-http-timeouts) below) a few seconds under it
> — the server then returns `timeout: capped` cleanly before the client aborts, and the
> agent re-polls without orphaning futures.

#### Amazon Kiro

Add the server to `.kiro/settings.json` at your project root (Kiro workspace-level MCP config). The `X-Host-Cap-Secs` header is required — Kiro imposes a hard ~120 s HTTP timeout and the server uses this header to return before the limit fires:

```json
{
  "mcpServers": {
    "toaster-talk": {
      "type": "http",
      "url": "http://localhost:6669/mcp",
      "headers": {
        "X-Host-Cap-Secs": "110"
      }
    }
  }
}
```

For user-level configuration (available across all Kiro workspaces), add the same block to Kiro's global MCP settings file (`~/.kiro/settings.json` or via **Kiro → Preferences → MCP Servers**).

#### JetBrains Junie

Add the server using the SSE URL (classic SSE transport avoids Junie's hard HTTP timeout):

```json
{
  "mcpServers": {
    "toaster-talk": {
      "url": "http://localhost:6669/sse"
    }
  }
}
```

### 4. Connect, join, and communicate

Tell each agent its role when starting the task:

> "Join the ToasterTalk room named 'qa-review' as member 'dev'. Call `connect` (no arguments) once to mint a session token. Then scan `~/.toaster-talk/tokens/` for the matching `.tok` file and call `join` with your session token, the room token, and your member name — this is the only way to attach to a room, including your first one. Use `send` (naming `room_name`) and `receive` (optionally naming `room`) to coordinate. If a response contains `timeout: capped`, your MCP host shortened the wait — call `receive` again immediately to keep waiting. Never re-call `send` to resume a capped reply wait: your message was already delivered, so re-sending would deliver a duplicate. If a message you receive carries `reply_expected_within_secs`, its sender is awaiting your reply within that many seconds — respond promptly by sending back to its `from` in that same room."

The agent calls `connect` once per process to obtain a session token, then `join` once per room it needs (repeat `join` for additional rooms — one session, one shared inbox, spans all of them) before using `send` and `receive`. Treat the session token as credential-grade: it now grants access to every room the session joins, not just one, so never log it in full or paste it insecurely. Only pass `old_session_token` back into `connect` if deliberately rotating a token you suspect leaked — doing so invalidates every room membership under it with no carryover, and each room must be `join`ed again afterward.

#### Host cap (MCP hosts with hard HTTP timeouts)

Some MCP hosts impose a hard wall-clock timeout on HTTP tool calls. Configure the host's HTTP client to send the `X-Host-Cap-Secs` request header set to a value a few seconds below the hard limit. The server reads this header per-request, caps any blocking wait to that value, and returns `timeout: capped` — signalling the agent to re-call `receive` immediately. (On a capped `send`-with-reply, the message is already delivered, so the agent continues with `receive`, never another `send`.)

| Host | Hard limit | Recommended `X-Host-Cap-Secs` |
|---|---|---|
| Amazon Kiro | ~120 s | 110 (already in the Kiro config above) |
| Claude Code | ~300 s idle (`CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`, default `300000` ms) | 290 — **or** disable the timeout entirely (see [Claude Code](#claude-code) above), preferred for long listeners |

The cap is per-request — agents on hosts that do not send the header are completely unaffected. Hosts using the classic SSE transport (JetBrains Junie) do not need a host cap; SSE responses are not subject to HTTP request timeouts.

> **Claude Code is no longer cap-free.** Before v2.1.187 a Claude Code `receive` could
> block indefinitely; since v2.1.187 it is subject to the client-side idle timeout above.
> For long-lived listeners the cleanest fix is to disable that timeout
> (`CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=0`) rather than chop the wait into `X-Host-Cap-Secs`
> round-trips.

### Non-systemd Unix systems

If you're on Unix without systemd, run the server directly (ensure `/run/toaster-talk/` exists first, or override the socket path):

```bash
sudo mkdir -p /run/toaster-talk
RUST_LOG=info toaster-talk-server --config /etc/toaster-talk/config.toml
```

Or with a custom socket path to avoid needing `/run/`:

```bash
RUST_LOG=info toaster-talk-server --config /etc/toaster-talk/config.toml
# and in config.toml: mgmt_socket = "/tmp/toaster-talk.sock"
```

### Troubleshooting

If a room seems stuck — an agent is waiting but a peer's messages never arrive — take a
read-only snapshot of the server's live in-memory state:

```bash
toaster-talk-cli dump            # YAML: rooms, members, inboxes, sessions
toaster-talk-cli dump | yq '.invariants'   # just the health findings
```

The `invariants` block flags desyncs (e.g. orphaned waiters that can steal message
wakeups). The dump is read-only and redacts all tokens. To force-recover a stuck room,
`toaster-talk-cli kckall <room>` evicts everyone so the agents reconnect fresh.

## Documentation

- `sdd/PRODUCT.md` — What it is, why it exists, vocabulary reference
- `sdd/TECH.md` — Architecture, module map, state model, concurrency design
- `sdd/SCHEMA.md` — Field-level contracts: MCP tool API, message object, management protocol, token files, config schema
- `sdd/TDD.md` — Behavioural test rubrics (105 rubrics across 14 areas)
- `sdd/POLICY.md` — Build, test, logging, and architecture rules
- `sdd/ISSUES.md` — Known limitations and deferred concerns
- `sdd/PLAN.broadcast.md` — Deferred design for broadcast / multi-consumer rooms

## License

Licensed under the [Apache License, Version 2.0](LICENSE) — see [NOTICE](NOTICE) for attribution.
