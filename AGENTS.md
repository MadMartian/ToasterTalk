# AGENTS.md

ToasterTalk MCP: a shared daemon MCP server that provides authenticated, exactly-once inter-agent message passing via persistent named rooms.

## Documentation

This project uses Spec-Driven Development (SDD). Project documentation lives in the `sdd/` directory.

### If you are exploring this project

Read these files first:
- `sdd/PRODUCT.md` — What this project is, why it exists, and the full vocabulary reference
- `sdd/TDD.md` — Behavioural contract (118 rubrics across 15 functional areas). Read the table of contents first (~15 lines), then the sections relevant to your task.

### If you are contributing to this project — or building, running, or testing it

Read all of the above, plus:
- `sdd/POLICY.md` — Development rules, build pipeline, logging requirements, and hard architectural constraints. You must follow these.
- `sdd/TECH.md` — Architecture diagram, module responsibilities, state model, concurrency design. The map, not the field reference.
- `sdd/SCHEMA.md` — Field-level contracts: the MCP tool API, the delivered-message object, the CLI↔server management protocol, token file formats, and the configuration schema. Read before constructing or changing anything that crosses one of those boundaries.
- `sdd/ANTI-PATTERNS.md` — Lessons from non-obvious bugs. Read the table of contents (~10 lines) first, then load any entries relevant to your task.
- `toaster-talk-server/tests/MANUAL-TEST.md` — Manual multi-agent integration suite (real transport, disconnect/reconnect, concurrency) that `cargo test` cannot express. Run it after any change touching session lifecycle, the inbox/receive slot, send-with-reply, the transport, or the TTL sweep (see POLICY.md).

## Important: the two-step connect → join flow

`connect` is a pure identity mint — it takes at most one optional input (`old_session_token`) and never sees a room token, seat token, or member name. It never creates a room membership, not even your first one. Call it once per agent process to obtain a `session_token`.

Every room membership — including your very first room — is created by calling `join` with that `session_token` plus a room or seat token and your member name. To join additional rooms later, call `join` again, once per room; one session, one shared inbox, spans all of them. `send` now requires `room_name` (which room to send in) and `receive`'s optional `room` field lets you drain one room specifically or, omitted, everything queued across every room you've joined in one batch.

`join` never silently mints a session: a missing or invalid `session_token` is a hard error, not an implicit `connect`. `MemberNameTaken` (room-token collision with a different session) is a **fatal, non-retryable error** — do not append a suffix or invent a new name, and do not retry `join` with a different name; surface it to the operator instead. Joining a room your own session already belongs to is also rejected (not a silent no-op) — do not re-`join` a room you already hold.

`leave` is the inverse of `join`: it removes only your membership in the named room, leaving your session's other room memberships (and shared inbox) completely unaffected. If it was your last remaining room, your whole session is torn down and you must `connect` + `join` fresh to use ToasterTalk again. `leave` never revokes a seat credential — you can `join` the same room again later with the same seat token.

**`old_session_token` no longer carries over room state.** Only pass it to `connect` if you are deliberately rotating a still-live session token (e.g. you suspect it leaked). Doing so invalidates that session — and *every* room membership under it — immediately, with no room or message carryover whatsoever. Recovering access afterward always means calling `join` again for each room, using the room/seat token you still hold. Do not use `old_session_token` as a way to "reconnect" to a room after an ordinary disconnect — if your session token is still valid, you don't need to do anything special; if it isn't (evicted, or the server restarted), just `connect` fresh (omit `old_session_token`) and `join` each room you need.

Because `session_token` now grants access to every room your session belongs to (not just one), treat it as credential-grade for as long as it lives: never log it in full, never paste it insecurely.

### Additional documents (read when relevant)

- `sdd/ISSUES.md` — Known unresolved issues (10 open). Read the table of contents first, then only the entries bearing on your task; consult it when investigating unexpected behaviour or before touching transport, auth, or receive.
- `sdd/PLAN.broadcast.md` — Deferred design for broadcast and multi-consumer rooms. Read before adding any fanout or multi-recipient capability.
