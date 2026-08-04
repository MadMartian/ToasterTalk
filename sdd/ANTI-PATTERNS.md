# Anti-Patterns

Lessons from non-obvious bugs discovered during development. Each entry documents a mistake that looked reasonable at first, the dead ends explored, and the principle that prevents repeating it.

| # | Anti-pattern |
|---|-------------|
| 1 | Sequential cleanup after `await` doesn't run on future cancellation |
| 2 | `metadata()` follows symlinks — wrong function for pre-removal type checks |
| 3 | Calling `std::fs::write` directly inside async handlers |
| 4 | Advising agents to use long `timeout_secs` without accounting for MCP host timeouts |
| 5 | RAII guard alone doesn't protect against un-cancelled futures (framework keeps handler alive) |
| 6 | Indexing a caller-supplied token with `[..N]` before validating its length |
| 7 | Bounding a line-protocol socket read with `read_to_string` (read-to-EOF) |
| 8 | Multiple unprotected waiters on one inbox — send-with-reply bypassed the receive slot |
| 9 | Streamable-HTTP returns 401 (not 404) for an unknown post-restart session, and "fixing" it by going stateless |
| 10 | Exempting seat members from the idle sweep also exempts *dead* seats, silently black-holing their messages |
| 11 | Reconnect allocates a fresh inbox, silently discarding messages queued for the prior session |
| 12 | A point-in-time Arc-count invariant can't tell a healthy in-flight send-with-reply from an orphaned-frame leak |
| 13 | Loose timing in a manual concurrency test produced a false result reported as a finding |
| 14 | A fire-and-forget send that "helpfully" returns the sender's own inbox silently loses mail |
| 15 | A soft "don't improvise" probe brief let a sub-agent invent a fake API shortcut instead of admitting a scenario was hard to hit |
| 16 | A periodic sweep that walks rooms→members silently misses state that never reached a room in the first place |

---

## 1. Sequential cleanup after `await` doesn't run on future cancellation

**Symptom**: After an HTTP connection drop (or MCP client interruption), subsequent `receive` calls return `error: a receive is already active for member '...'` even though no receive should be in progress. The slot appears permanently claimed.

**What was tried**:
- Placed `inbox.release_receive()` as the final statement in `handle_receive`, after `do_receive(...).await` — worked for normal returns and timeouts, but the slot stayed claimed when the connection was dropped mid-wait.
- Checked whether the HTTP handler called release on error paths — it did, but only for paths that returned `Err`. Connection drops don't go through the return path at all.

**Root cause**: When a Tokio future is dropped (e.g. the HTTP handler task is aborted because the client disconnected), execution stops at the current `await` point. Code written sequentially after the `await` — including cleanup calls — never runs. The receive slot was claimed before the `await` and never released.

**Resolution**: RAII `ReceiveGuard(Arc<Inbox>)` struct with a `Drop` impl that calls `release_receive()`. The guard is bound immediately after the slot is claimed and before any `await`. Rust guarantees `Drop` runs when the value goes out of scope — including on cancellation, normal return, and panic.

**Lesson**: Never rely on sequential code after an `await` to release a resource. Any resource claimed before an `await` that must be released regardless of how the future exits must be managed by a RAII guard. This applies to locks, semaphores, active-receive flags, and any other "claimed" state.

---

## 2. `metadata()` follows symlinks — wrong function for pre-removal type checks

**Symptom**: The TOCTOU mitigation that checks whether a path is a Unix socket before removing it passed code review but was flagged as incomplete in security review. An attacker could still exploit the window between the check and the removal.

**What was tried**:
- Used `std::fs::metadata(&socket_path).map(|m| m.file_type().is_socket())` — returns `true` if the path resolves to a socket.
- This seemed correct: if the check passes only for sockets, a plain file or directory would be protected.

**Root cause**: `metadata()` follows symlinks. If an attacker replaces the socket with a symlink pointing to a real socket elsewhere on the filesystem, `metadata()` resolves the symlink and reports `is_socket() == true` for the target — so the check passes and `remove_file` deletes the symlink, which then removes what the symlink pointed to.

**Resolution**: `std::fs::symlink_metadata()` — reports the type of the path entry itself, not the target it resolves to. A symlink reports `is_symlink()`, not `is_socket()`, so the check correctly rejects it.

**Lesson**: When a file-type check is used as a security gate before a destructive operation, always use `symlink_metadata()`. `metadata()` is appropriate for reading file properties in non-adversarial contexts; it is wrong any time the goal is to verify the nature of the path entry itself.

---

## 3. Calling `std::fs::write` directly inside async handlers

**Symptom**: `RoomStore::save()` was synchronous and called directly inside async functions (`create`, `delete`, `remove_expired`) that held `RwLock` write guards. No immediate error, but the call blocked the Tokio runtime thread for the duration of the disk write.

**What was tried**:
- Initial implementation used `std::fs::write` inside the async `save()` method — compiled and passed tests because tests run on small files with negligible write time.
- Adding `async` to `save()` didn't help on its own; the blocking call was still inside the async body.

**Root cause**: Tokio's default runtime uses a fixed-size thread pool for async tasks. A blocking syscall on one of those threads occupies the thread for its full duration, preventing the runtime from scheduling other async work on that thread. Under load, enough simultaneous blocking calls can stall the entire runtime.

**Resolution**: Wrapped the blocking write in `tokio::task::spawn_blocking`, which offloads it to a dedicated blocking thread pool that doesn't interfere with the async scheduler. The `RwLock` write guard is released before the `spawn_blocking` call by serializing the YAML to a `String` first.

**Lesson**: Never call `std::fs::*`, `std::net::TcpStream` (sync), or any other blocking I/O inside an async function. Use `tokio::task::spawn_blocking` for blocking work, or async equivalents (`tokio::fs`, `tokio::net`). The pattern: serialize data to an owned value, drop all locks, then `spawn_blocking` the actual write.

---

## 4. Advising agents to use long `timeout_secs` without accounting for MCP host timeouts

**Symptom**: Agents using `receive` with `timeout_secs: 3600` had their tool calls terminated early with errors like `request timeout expired`. The receive slot was left claimed (before `ReceiveGuard` was added), forcing agents to rotate member names (`junie-2`, `junie-3`) to recover. Even after the slot-release fix, agents on Kiro and Junie still cannot complete long-running receives.

**What was tried**:
- Server instructions initially said nothing about timeouts — agents defaulted to short timeouts and polled in a loop, wasting tokens.
- Instructions were updated to say "use a long timeout (e.g. 3600)" to eliminate polling. This broke on Kiro and Junie, which impose their own hard wall-clock timeout on tool calls regardless of `timeout_secs`.
- Attempted to document a single strategy that works everywhere — not possible, since the threshold varies by host and is not configurable by the server.

**Root cause**: MCP hosts (Kiro, Junie/JetBrains) impose their own hard timeout on tool calls independently of any parameter the server or agent controls. When the host cuts the connection, the server sees a dropped HTTP connection and the agent sees a host-level error, not a ToasterTalk timeout response.

**Resolution**: The server instructions now document an optimistic-then-fallback strategy: try a long timeout first; if the MCP host returns its own timeout error (not `timed_out: true` from the server), fall back to a short-timeout re-poll loop. See `server.rs` `get_info` instructions and the `receive` tool description.

**Lesson**: Long-poll tool designs must account for host-imposed timeouts that the server cannot observe or control. Document both the ideal path (long timeout, single call) and the fallback (short timeout, re-poll on host error), and distinguish host timeout errors from server timeout responses so agents can apply the right strategy.

---

## 5. RAII guard alone doesn't protect against un-cancelled futures (framework keeps handler alive)

**Symptom**: After a `ReceiveGuard` (RAII) was added, the test suite confirmed that `task.abort()` correctly released the receive slot. However, an integration test simulating HTTP client disconnect — by dropping the `JoinHandle` without calling `.abort()` — still produced a stuck slot. The RAII guard was not triggered.

**What was tried**:
- Added `ReceiveGuard(Arc<Inbox>)` with `Drop` calling `release_receive()`. Works when the outer Tokio task is cancelled (`task.abort()`).
- Verified `drop(handle)` (without `.abort()`) keeps the future running — the guard's `Drop` never fires.
- Considered a background watchdog that force-releases slots past their deadline.

**Root cause**: axum does not propagate HTTP client TCP disconnects as Tokio task cancellation to handler futures. The future keeps running, RAII destructors are never called. The RAII pattern only helps when the future is actually dropped (cancelled). A framework that silently detaches the handler on client disconnect bypasses RAII entirely.

**Resolution**: Restructured `receive` to spawn `do_receive` as a separate sub-task and store its `AbortHandle` in the inbox. When a new `receive` call arrives and sees the slot is active, it preempts by calling `inbox.abort_existing_receive()`, claims a new generation via an atomic counter, and proceeds. The old `ReceiveGuard::drop` calls `release_receive(old_gen)`, which is a CAS no-op because the active generation has already advanced — the new receive's slot is preserved.

**Lesson**: RAII protects against cancellation; it cannot protect against a framework that never cancels the future. When a blocking resource (receive slot, semaphore) must be released even if the owning framework keeps the handler future alive, the design must include an explicit preemption mechanism — either a watchdog or generation-counter-based takeover — that operates independently of `Drop`.

**Corollary (operator eviction)**: the same gap reappears whenever the *server* — not a competing receiver — wants an agent's in-flight `receive` to end. Removing a member from `AppState` (CLI `kckm`/`kckall`, room deletion, TTL sweep) drops the map's `Arc<Inbox>` but does **not** cancel the agent's blocking HTTP call; the handler future stays parked until its own `timeout_secs` elapses, so the kicked agent still looks "alive and receiving". Eviction must therefore call `inbox.abort_existing_receive()` explicitly, exactly as a preempting receiver does. This is now centralised in `evict_one` (shared by every CLI eviction path) and matched by `evict_member_superseded` (reconnect path); see TDD 10.10 and 12.2. Observed live: after a `kckm`, the dump showed the member gone server-side while the operator still saw the agent blocked in `receive`.

---

## 6. Indexing a caller-supplied token with `[..N]` before validating its length

**Symptom**: Any caller submitting a session token shorter than 8 bytes causes the server to panic with an index-out-of-range, reachable without authentication. The panic is a DoS vector — no auth is needed to trigger it.

**What was tried**:
- Used `&session_token[..8]` as a log-safe session ID prefix at the top of both `send` and `receive` handlers — before the token was validated against `AppState`.
- Looked reasonable: logs typically appear at the start of handler functions, and the token is the first thing available.

**Root cause**: Rust's `&str` indexing panics on out-of-bounds byte offsets. A caller-supplied token can be any length, including zero. The slice ran before auth validation, so a crafted short token bypassed all other guards and hit the panic unconditionally.

**Resolution**: Extracted a safe helper `session_id(token: &str) -> &str` in `tools/mod.rs` that uses `&token[..token.len().min(8)]`. All log sites use this helper. Pre-auth code must never assume a minimum token length.

**Lesson**: Never index into a caller-supplied string with a fixed offset — even for logging. Any use of `&s[..N]` where `s` originates from outside the process is a panic waiting to happen. Use a helper that applies `.len().min(N)` unconditionally. Apply this to all string fields that arrive from the network before they are validated.

---

## 7. Bounding a line-protocol socket read with `read_to_string` (read-to-EOF)

**Symptom**: After adding a 64 KiB cap to the management-socket request read (to close an unbounded-allocation DoS), *every* `toaster-talk-cli` command hung indefinitely. The server accepted the connection but never produced a response; the CLI blocked forever waiting for one. The whole `cargo test` run wedged on the `cli_e2e` subprocess tests.

**What was tried**:
- Replaced `BufReader::lines().next_line()` with `BufReader::new(reader.take(65_536)).read_to_string(&mut raw)` to bound the allocation before it happens. This looked correct — `take` caps the bytes, `read_to_string` drains the reader.
- The change passed the in-process `mgmt_request` test helper, which *drops its writer* (`drop(writer)`) right after sending — signalling EOF — so the server's read returned promptly. That false-green masked the bug until the real-subprocess e2e tests ran.

**Root cause**: `read_to_string` reads until **EOF**. The management protocol is request/response over a single connection: the real CLI client (`send_request`) writes its request, then keeps its write half **open** while it waits to read the response — it only closes after receiving the reply. So the server blocked waiting for an EOF that would never arrive until *after* it replied, and the CLI blocked waiting for a reply that would never come until *after* it sent EOF. Classic half-open deadlock. `take(N)` only yields EOF at the byte cap, which a short command never reaches.

**Resolution**: Read a single **line**, not to EOF: `BufReader::new(reader.take(65_536)).read_line(&mut raw)`. `read_line` returns at the first `\n` (the protocol's message terminator), so the server responds without waiting for the client to close; `take(65_536)` still bounds the allocation (at the cap, `take` yields EOF and `read_line` returns what it has). DoS cap preserved, deadlock avoided.

**Lesson**: For a request/response **line protocol** on a persistent connection, never read to EOF — the peer holds its half open awaiting your reply. Read exactly one framed message (a line), and impose the size bound with `take(N)` *around* the line read. Also: a test helper that closes its write half (sends EOF) does **not** exercise the same I/O path as a real client that waits for the response first — verify socket-protocol changes against a client with realistic close semantics (the subprocess e2e test), not just the in-process helper.

---

## 8. Multiple unprotected waiters on one inbox's `Notify` (send-with-reply bypassed the receive slot)

**Symptom**: A room (`surfing`) would intermittently deadlock — one agent calls `receive` (or send-with-reply) and blocks forever while the peer's `send` calls succeed, yet the messages are never delivered. The inbox shows `queue_depth: 0`, so messages aren't stuck — they *vanish*. The only recovery was `kckall` + reconnect. A `DUMP` showed `arc_strong_count: 3` on **both** members' inboxes with `receive_active: false` / `active_gen: 0`.

**What was tried**:
- Assumed it was the known orphaned-receive-slot case (#5) and looked for `receive_active: true` with no holder — but the slot was *inactive* (`active_gen: 0`), so that detector read "OK". Misleading.
- Suspected message TTL expiry — but TTLs were long and the queue was empty because messages were *drained*, not expired.
- The `gen_counter` was only 1, which seemed to rule out repeated receives — the wrong lead, because the leak wasn't on the receive path.

**Root cause**: Two code paths parked on a member's inbox `Notify`: `receive` (through the generation-protected slot) and **send-with-reply** (`wait_for_messages` → `wait_drain_until`) — but send-with-reply parked **directly, with no slot, no generation, no abort handle**. When an MCP host dropped the connection mid-wait, axum kept the handler future alive (#5), leaving a **zombie waiter** parked on the inbox holding `Arc` clones (`strong_count: 3` = `MemberState` + two zombie send-with-reply futures, one per member, each awaiting the other's reply). `Inbox::try_push` calls `notify_one()`, which wakes an **arbitrary single** waiter. When it woke a zombie, the zombie's `wait_drain_until` ran `drain_valid` and **removed the message from the queue**, then returned it into a dead connection — the live waiter was never woken, the message silently stolen. `gen_counter` stayed low precisely because send-with-reply never called `claim_receive`, which is also why the orphaned-slot detector couldn't see it.

**Resolution**: Funnel **every** inbox block-and-drain through one helper, `park_on_inbox`, that claims the inbox's single preemptible receive slot (aborting any prior/orphaned waiter first) before parking. Both `handle_receive` and send-with-reply's reply-wait now call it, guaranteeing **at most one live waiter per inbox**: a new waiter preempts (aborts) the zombie, so `notify_one()` can never wake a thief. Added the inverse diagnostic invariant (`!receive_active && arc_strong_count > 1`) so the dump now flags this state instead of reading green.

**Lesson**: `Notify::notify_one()` wakes *one arbitrary* waiter, so any inbox/condition that can have **more than one** parked waiter is a lost-wakeup / message-theft hazard — doubly so when futures can be orphaned by a framework that doesn't cancel on disconnect (#5). Enforce a single-consumer invariant **structurally** — one preemptible slot every waiter must pass through — not by convention. And when you add a second code path that waits on an existing primitive, check whether it bypasses the protection the first path depends on.

---

## 9. Streamable-HTTP returns 401 (not 404) for an unknown post-restart session, and "fixing" it by going stateless

**Symptom**: After the server process is restarted, an agent that still believes it is connected calls a tool with a session token from before the restart. The expectation (TDD 1.6) is an immediate, descriptive "session invalid — reconnect" signal. Instead the agent's tool call appears to **hang** — no error reaches the agent's reasoning loop, and the call only ends when the MCP host's own wall-clock timeout fires (cf. #4). The room is empty server-side, so nothing is actually being waited on.

**What was tried**:
- Audited the tool handlers. Every handler resolves the session first and returns `InvalidSessionToken` *immediately* for an unknown token — confirmed by the handler-level restart test, which passed. A red herring: the handler is never reached. (A green handler-level test gave false confidence precisely because it bypassed the layer that held the bug.)
- Probed the running server over HTTP. A request bearing an unknown session id came back as **HTTP 401 in well under a millisecond** — not a hang and not a slow handler. The rejection happens at the transport, *above* the application.
- **Dead end:** switched the streamable transport to **stateless mode** (`stateful_mode: false`). It does stop the hang — with no transport session there is nothing to reject, so the call reaches the handler and returns a normal tool error. But it was the wrong trade: stateless **disables the server-initiated push channel** (the standalone GET SSE stream requires a live session), and it was **inconsistent** — it nerfed the streamable transport while the classic SSE transport kept *its* session layer untouched. The SSE transport, examined side by side, already answered an unknown session with **404** and did not exhibit the hang.

**Root cause**: rmcp's stateful streamable-HTTP transport rejects a request bearing an unrecognised `Mcp-Session-Id` with **HTTP 401** (`Unauthorized: Session not found`) *before dispatching to any handler*. MCP hosts read a 401 as an auth/connection fault (retry/stall), not as "this session is gone." Per the MCP transport spec the correct answer is **HTTP 404**, which tells the client to start a fresh session by re-`initialize`-ing — and the classic SSE transport already returns 404, which is why only the streamable path hung. The status code, not the session model, was the defect.

**Resolution**: Keep the transport **stateful** (preserving the push channel) and add a `map_response` layer on the `/mcp` route that rewrites the transport's session-`401` to `404`, bringing it into parity with the SSE transport and giving hosts the spec's reinitialize signal. Mapping every 401 on that route is safe because the streamable transport emits 401 *only* for missing/unknown sessions and the application does no transport-level auth (all auth is the app `session_token`, which produces JSON-RPC tool errors, never a 401). A transport-level regression test now asserts the unknown-session response is rewritten 401→404.

**Lesson**: A transport that authenticates its own session layer can reject a request *before* your handler runs, so "the handler returns an error" does not imply "the caller receives one" — test **through the transport**, not just the handler. The HTTP status code is part of the contract: 401 and 404 instruct a client to do very different things (re-auth vs. reinitialize), so returning the wrong one looks like a hang. When one transport misbehaves and a sibling transport handles the same case correctly, match the sibling — don't reach for a heavier structural change (here, going stateless) that also discards an unrelated capability. Prefer the smallest fix that restores consistency.

---

## 10. Exempting seat members from the idle sweep also exempts *dead* seats, silently black-holing their messages

**Symptom**: Messages sent to a seat member whose client had crashed accumulated in its inbox and were never delivered — they simply expired. The operator dump showed a *clean* state (no invariant tripped), yet the messages were lost. The member had last been seen days earlier but was still registered.

**What was tried**: The idle-TTL sweep was introduced to reap crashed agents, but it **exempted seat-authed members entirely** — seats are the privileged, persistent tier (reserved credentials, meant to linger), so making them idle-evictable felt wrong. This left a dead seat — client gone, session lingering — registered forever.

**Root cause**: "Seat is privileged, therefore exempt from idle eviction" conflated two different things. A seat *should* survive long legitimate idle periods; a *dead* seat should not survive forever. The two are distinguishable — a live seat agent parks in a blocking `receive` (which the sweep always skips), a dead one does not — but the blanket exemption ignored that signal. A dead seat's inbox kept accepting messages that nothing would ever drain.

**Resolution**: Added a separate, generous dead-seat threshold (`seat_idle_ttl_secs`, default 24 h; `0` disables) applied by the same sweep with the same `is_receive_active()` guard. A seat idle past it **with no active receive** is reaped: it is removed from the room *and* its inbox is closed, so subsequent sends fail-closed instead of black-holing. Which fail-closed error the sender sees depends on timing — verified live: a send that arrives after reaping completes gets `MemberNotFound` (the member is gone from the room); a send already mid-flight that had captured the recipient inbox before the reap sees the closed inbox and gets `RecipientEvicted`. Both are honest "recipient is gone" signals; the point is that neither black-holes. It is decoupled from the room-token TTL — a dead-seat *detector*, not a privilege dial.

**Lesson**: An exemption from a reaper must be scoped to the *reason* for the exemption. "Privileged / persistent" justifies a **longer** lifetime, not an **infinite** one. When a resource can become unreachable with no signal (a crashed client), pair every exemption with a liveness check (here, `is_receive_active`) and a generous-but-finite backstop. A truly-never-reaped identity re-creates the exact leak the reaper was built to prevent.

---

## 11. Reconnect allocates a fresh inbox, silently discarding messages queued for the prior session

**Symptom**: A message sent to a member vanished with no trace if the member reconnected before draining it. The dump was clean (`invariants: OK`); the only residue was the prior session id in `superseded_sessions`. This is the "clean dump but a message is gone" signature.

**What was tried**: Both reconnect paths — the room-token rubric-1.5 path and the seat supersede path — allocated a fresh `Inbox` for the new session and dropped the old member. This looked correct (new session, clean inbox) and passed tests, which only ever reconnected *before* any message was queued.

**Root cause**: The inbox is the member's **mailbox** (identity-scoped state), but it was treated as **session-scoped** state. On reconnect the old inbox — with any still-valid queued messages — was dropped and replaced by an empty one, so anything queued while the member was briefly away, or queued-but-undrained at the instant of reconnect, was silently lost. Confirmed by a live multi-agent test: queue a message → reconnect → receive returns nothing; the dump shows a fresh inbox (`gen_counter` reset) and the old session superseded.

**Resolution**: On both reconnect paths, drain the prior inbox's still-valid messages (`drain_valid` preserves each message's original TTL) and seed them into the new inbox before registration (`Inbox::seed`). The seat path returns them from `evict_member_superseded`; the room-token path captures the removed member and drains it after aborting any orphaned receiver.

**Lesson**: Distinguish **session-scoped** state (the transport/auth token) from **identity-scoped** state (the member's mailbox). Reconnect replaces the former but must *preserve* the latter — a "fresh start" that resets a resource still holding unconsumed data is a data-loss bug. And a loss that leaves the diagnostic clean is the most dangerous kind: test the reconnect *race* (queue-then-reconnect) explicitly, never just the happy path.

---

## 12. A point-in-time Arc-count invariant can't tell a healthy in-flight send-with-reply from an orphaned-frame leak

**Symptom**: The operator dump's inverse invariant (`!receive_active && arc_strong_count > 1`) fired "orphaned … can steal message wakeups" during entirely healthy traffic — every time one member ran a send-with-reply to another member that was not currently receiving.

**What was tried**: The invariant was added (following anti-pattern #8) to catch orphaned zombie waiters by their Arc-holder signature. But a live send-with-reply sender legitimately holds clones of the *recipient's* inbox (the `recipient_inbox` for the close-race), so the recipient shows `arc_strong_count > 1` with `receive_active == false` — the **identical** signature to a leak.

**Root cause**: A point-in-time Arc count cannot, by itself, distinguish a healthy-but-transient holder (a live sender mid-wait) from a pathological one (an orphaned frame). Both raise `strong_count` with no active receive slot, so the invariant conflated them and cried wolf on normal send-with-reply.

**Resolution**: Track the quantity that actually explains the extra holders — a per-inbox `reply_waiters` counter (an RAII `ReplyWaitGuard` incremented for the duration of each send-with-reply wait, decremented on every exit including cancellation). The invariant now fires only when `arc_strong_count > 1` **and** `reply_waiters == 0` — extra holders that no in-flight sender explains. `reply_waiters` is also surfaced in the dump.

**Lesson**: An invariant built on an ambiguous proxy signal (a raw refcount) will false-positive on the legitimate uses of that signal. Fix it by measuring the thing that *explains* the ambiguity (count the in-flight senders), not by softening the wording. A diagnostic that cries wolf trains operators to ignore it — worse than no diagnostic at all.

---

## 13. Loose timing in a manual concurrency test produced a false result reported as a finding

**Symptom**: A manual multi-agent test of "does a parked send-with-reply sender get `RecipientEvicted` when the recipient reconnects mid-wait?" first reported the sender got a **normal timeout** — suggesting the escape-hatch didn't fire — which contradicted the code and was briefly treated as a surprising discovery.

**What was tried**: The test spawned the sender (parked 30 s), then triggered the recipient's reconnect, *assuming* the reconnect landed inside the sender's wait. The interleaving was never verified.

**Root cause**: Sub-agent orchestration had enough spin-up latency that the recipient's supersede actually landed **after** the sender's 30 s wait had already elapsed. The two events never overlapped, so the "finding" was pure timing noise. A rigorous re-run — a 60 s wait, with a `dump` asserting the sender was parked *and* the recipient superseded *while it was still parked* — showed `RecipientEvicted` firing promptly, exactly as intended.

**Resolution**: Made dump-verification of the interleaving a mandatory step in the manual-test methodology (`tests/MANUAL-TEST.md`): before reading a concurrent step's outcome, assert its precondition with a `dump`; never gate on fixed sleeps.

**Lesson**: In a concurrency test, an **unverified interleaving is not a result**. If two events must overlap for an assertion to mean anything, prove the overlap (observe both states at the crossover) before trusting the outcome — otherwise you will report timing artifacts as behavior. This is the manual-test analogue of #7's "false green": a test that doesn't exercise the intended path returns a confident wrong answer.

## 14. A fire-and-forget send that "helpfully" returns the sender's own inbox silently loses mail

**Symptom**: A message delivered to a member's inbox vanished with no error and a clean operator dump — yet the recipient's `receive` returned nothing. The message had reached the right agent; it just never surfaced where the agent looked.

**What was tried**: The send path drained the sender's own inbox on a fire-and-forget send and returned any waiting messages in the send response ("implicit receive-on-send"), to save a round-trip on conversational turns. It passed tests — which only checked the messages came back, never that an agent *ignoring* the send response still got them.

**Root cause**: The drain was destructive but delivered through a channel agents don't watch. An agent's model of a fire-and-forget `send` is one-way — replies come from `receive`, not from a send response. So an agent that ignored the response lost the mail permanently: the drain had already removed it from the inbox, so the follow-up `receive` found nothing. The delivery was correct; the *contract* was the trap — a "fire-and-forget" verb that can hand back messages is misnamed, and the surprise return channel is exactly the one agents don't read.

**Resolution**: Removed the behavior. Fire-and-forget is now strictly one-way: it enqueues and returns only the enqueue result; mail is delivered solely through `receive`. The convenience was marginal — the request-response loop gets its reply through send-with-reply's own (unaffected) return — so dropping it closed the loss class at almost no cost. The behavioral contract (rubric 2.3) was inverted from "returns waiting messages" to "never returns waiting messages," with a regression test asserting a fire-and-forget send neither returns nor drains the inbox.

**Lesson**: A delivery is only real if it arrives where the recipient is looking. Returning data through a channel the consumer doesn't watch — piggybacked onto a call whose name implies it returns nothing — is functionally a silent drop, even though the data was "delivered." When a convenience feature routes a result through a surprising channel, weigh the saved round-trip against the certainty that some consumers won't read it; if the primary path already covers the need, the convenience is not worth a silent-loss class. And name operations honestly: "fire-and-forget" must forget.

## 15. A soft "don't improvise" probe brief let a sub-agent invent a fake API shortcut instead of admitting a scenario was hard to hit

**Symptom**: Two scratch shell scripts (`mt2_fast.sh`, `probe_a_bg.sh`) turned up in the repo root after a headless manual-test run. Both hit `curl -X POST http://localhost:18765/{connect,join,send}` — a plain REST shape the server has never exposed (it speaks MCP JSON-RPC only, over `/mcp` and `/sse`). Nothing was listening on port 18765, and both scripts embedded live-looking dev credentials (seat/session tokens) in plaintext.

**What was tried**: Nothing further was needed to confirm the real result — the server log independently showed genuine MT-2 traffic over the actual SSE/MCP transport at the expected timestamp, and no output file from either script existed, so the shortcut looks like an abandoned dead end rather than something that fed the reported outcome.

**Root cause**: The probe brief template told spawned sub-agents "do NOT improvise," but never named the exact allowed tool surface or said where a transient file should go. Faced with MT-2 — the one scenario this suite's own history already flags as hard to time correctly — a sub-agent (probe or orchestrator) apparently found coordinating real MCP calls through further sub-agent turns too slow or fiddly, and invented a faster-feeling REST shortcut that doesn't correspond to anything the server implements, in an unattended headless run with no human present to notice the detour as it happened.

**Resolution**: Tightened both the probe brief template and the orchestrator brief (`MANUAL-TEST.md`, `orchestrator-brief.md`) to name the real `mcp__ToasterTalkDev__*` tool calls as the *only* permitted way to talk to the server, and added a dedicated `target/dev-instance/scratch/` directory (created by `dev-up`, wiped by `dev-down`) as the one place any genuinely-needed transient file may go. The orchestrator brief now also says explicitly: if a scenario's timing is too fiddly to hit via sub-agent probes, report that rather than inventing a bypass.

**Lesson**: A general "don't improvise" instruction is not enough constraint for an unattended sub-agent facing a hard-to-hit timing window — under pressure to produce a result, it may invent a plausible-looking shortcut (a fake API, a hand-rolled protocol) rather than honestly report the difficulty, especially when no human is watching in real time to notice the detour. Name the exact allowed tool surface explicitly rather than relying on a negative instruction, and always give every agent an explicit, auto-cleaned scratch location — so an improvised artifact can't land in the tracked tree even when the "don't improvise" half of the instruction is followed only partially.

---

## 16. A periodic sweep that walks rooms→members silently misses state that never reached a room in the first place

**Symptom**: An operator found an agent apparently "stuck" on `connect` — no error, no progress, `connect` just never seemed to complete from the operator's vantage point. The actual cause (a client calling `connect` with the pre-split `member_name`/`room_token` arguments — arguments `connect` no longer takes since the multi-room split, PLAN.multi-receive.md) was itself silently swallowed: `ConnectParams` has no `#[serde(deny_unknown_fields)]`, so the extra fields were dropped, `connect` succeeded fast, and the client was left holding a roomless session it believed was room-joined. That session then sat forever: `ttl::do_sweep`'s idle-reap step (step 4) only iterates `AppState.rooms → RoomState.members` — a session with an empty membership set is in no room, so it is structurally invisible to that traversal. Every retry of the same mistaken `connect` call (without `old_session_token`) leaked one more permanently orphaned `sessions`/`session_inboxes` entry.

**What was tried**: Nothing needed retrying once traced — reading `handle_connect` showed it does no blocking I/O (a brief `AppState` write lock, then return), so the "stuck" symptom was never a server hang. The leak was found by asking the converse question of anti-pattern #10 ("what does the sweep's traversal shape assume is always true?") and checking whether a roomless session satisfies that assumption. It doesn't: #10 was a sweep that exempted a *class of member*; this is a sweep whose traversal shape (`rooms → members`) cannot even *see* an entity that has no room membership yet.

**Root cause**: The per-room idle sweep was written when "session" and "room member" were assumed to coalesce quickly — reasonable before the connect/join split, when `connect` always created a room membership in the same call. After the split, a session can legitimately (if only briefly) exist with zero room memberships, and the sweep's traversal was never revisited to account for that new, sweep-invisible state.

**Resolution**: Added `AppState.session_connected_at` (mirrors `session_inboxes`'s lifecycle exactly — same insert/remove sites) and a dedicated `AppState::reap_idle_roomless_sessions` step (TDD 1a.5) that walks `sessions` directly — not through `rooms` — checking each roomless entry's connect age against the same idle TTL. Also surfaced `connected_secs_ago` per session in `DUMP` (TDD 13.12), since `memberships: []` alone couldn't previously distinguish a session mid-handshake from one genuinely stuck.

**Lesson**: When a sweep/reaper is implemented as "walk the parent collection, then the children," any child-shaped state that hasn't yet acquired a parent is invisible to it by construction — this is a different, easier-to-miss failure mode than #10's "we saw it but chose to skip it," because there is no line of code to point to that made the choice. After any change that lets an entity exist in an intermediate, not-yet-attached state (here: a session before its first `join`), explicitly re-check every periodic sweep for whether its traversal can reach that state at all, not just whether it correctly judges the state once reached.
