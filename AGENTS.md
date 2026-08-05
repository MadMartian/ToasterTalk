# AGENTS.md

ToasterTalk MCP: a shared daemon MCP server that provides authenticated, exactly-once inter-agent message passing via persistent named rooms.

## Documentation

This project uses Spec-Driven Development (SDD). Project documentation lives in the `sdd/` directory.

### If you are exploring this project

Read these files first:
- `sdd/PRODUCT.md` — What this project is, why it exists, and the full vocabulary reference
- `sdd/TDD.md` — Behavioural contract (105 rubrics across 14 functional areas). Read the table of contents first (~15 lines), then the sections relevant to your task.

### If you are contributing to this project

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

**`old_session_token` no longer carries over room state.** Only pass it to `connect` if you are deliberately rotating a still-live session token (e.g. you suspect it leaked). Doing so invalidates that session — and *every* room membership under it — immediately, with no room or message carryover whatsoever. Recovering access afterward always means calling `join` again for each room, using the room/seat token you still hold. Do not use `old_session_token` as a way to "reconnect" to a room after an ordinary disconnect — if your session token is still valid, you don't need to do anything special; if it isn't (evicted, or the server restarted), just `connect` fresh (omit `old_session_token`) and `join` each room you need.

Because `session_token` now grants access to every room your session belongs to (not just one), treat it as credential-grade for as long as it lives: never log it in full, never paste it insecurely.

## Autonomous listening ("receive loop") pattern

If the operator asks you to run a "listen loop," "receive loop," or otherwise stay
reachable autonomously for an extended period (e.g. "keep listening while I sleep"),
the coverage comes from looping bounded `receive` calls, not from a single
exceptionally long one. Concretely:

1. **Never filter by room.** Omit `receive`'s `room` field so every joined room is
   drained in one batch — a room-scoped loop would go deaf to any other room.
2. **Call receive directly — do not spin up a separate sub-agent for this.**
   Spawning a sub-agent means reloading a system prompt, skills, MCP tool
   schemas, and steering docs just to sit in a wait loop; call the tool
   yourself instead. If your host promotes a long-running call to the
   background on its own after some delay, that's expected and not something
   you need to orchestrate. Never have more than one receive in flight at once —
   a second concurrent call gets `preempted`, per the tool description.
3. **Loop with a bounded `timeout_secs`.** A single `receive` is capped
   server-side (`max_receive_timeout_secs`, 3600s by default — see
   `sdd/SCHEMA.md`); request a value at or under that cap.
4. **`elapsed` is the expected, normal result — not a stopping condition.** The
   moment any `receive` call returns, for *any* reason (`none`, `elapsed`,
   `capped`, or an error), your very next action — in that same response, with
   no other text in between — must be to call `receive` again. Do not write
   "no new messages," "standing by," "still listening," or any other wrap-up
   line: a text-only response with no accompanying tool call ends your turn,
   and there is no operator present to prompt you forward again. Only produce
   a final summary once the operator's requested duration has actually
   elapsed (default ~4 hours if unspecified) or a real stop condition (point
   6) occurs.
5. **On a real delivery, re-arming is still mandatory no matter how much work
   the message triggers.** Engage with it fully — reply, do whatever it asks,
   however many turns that takes. But finishing that work is never a stopping
   point on its own: the last action of the whole exchange, before you do
   anything else (write a summary, wait, go idle, consider yourself "done"),
   must be another `receive` call. It is easy to wrap up a multi-step task and
   simply move on — every time you are about to stop for *any* reason, check
   whether you are still supposed to be listening, and if so, re-arm first.
6. **On a hard session failure** (e.g. `InvalidSessionToken` from an eviction),
   reconnect (`connect` + `join` each room) and resume the loop; only stop and
   notify the operator if reconnection itself fails.

### Additional documents (read when relevant)

- `sdd/ISSUES.md` — Known unresolved issues (9 open). Read when investigating unexpected behaviour or before touching transport, auth, or receive.
- `sdd/PLAN.broadcast.md` — Deferred design for broadcast and multi-consumer rooms. Read before adding any fanout or multi-recipient capability.
