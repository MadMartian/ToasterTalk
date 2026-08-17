# ToasterTalk MCP — Known Issues

This is a living register of known problems, limitations, and deferred concerns. Items are removed when resolved.

| ID | Issue | Severity |
|----|-------|----------|
| ISS-001 | Batch receive is unbounded (DoS vector) | Medium |
| ISS-002 | No TLS; room tokens and session tokens are transmitted in plaintext | Low / High |
| ISS-003 | Member disconnect is not reliably detectable | Low |
| ISS-006 | Seat token revocation requires server-side deletion | Medium |
| ISS-007 | Master secret rotation invalidates all tokens with no migration path | Medium |
| ISS-008 | Join/delete TOCTOU windows can register memberships against a deleted room | Negligible |
| ISS-009 | `SessionToken` and `MemberName` remain type aliases, not newtypes | Negligible |
| ISS-010 | Two agents sharing one seat credential cause eviction thrash | Medium |
| ISS-011 | A send-with-reply reply can be orphaned if the recipient reconnects mid-wait | Low |

---

## ISS-001 — Batch receive is unbounded (DoS vector)

**Area**: Receive
**Severity**: Medium
**Status**: Open

The `receive` tool returns all queued messages in a single batch. A malicious or malfunctioning member could flood a recipient's inbox, causing a single `receive` call to return an arbitrarily large payload. This could degrade agent performance or exhaust memory.

**Mitigation shipped (partial):**
Per-member inbox size limit is enforced: `send` is rejected with `InboxFull` when the recipient's inbox reaches the configured `max_inbox_messages` cap (default 100). This bounds memory consumption per member.

**Remaining exposure:**
The batch size at `receive` time is still unbounded — all queued messages are returned in one call. Further options to evaluate:
- Cap the number of messages returned per `receive` call (e.g. max 100), with a continuation token for the remainder
- Cap the total byte size of a single `receive` response

One session's inbox is shared across every room it has joined, with one `max_inbox_messages` budget for all of them combined (not one budget per room). That does not multiply the exposure above — an N-room session still has exactly one inbox and one cap — but it introduces a **cross-room noisy-neighbor** effect: a chatty room A can fill the shared budget and cause `InboxFull` rejections for an unrelated room B's senders. Worth factoring into any future per-`receive`-call cap design.

---

## ISS-002 — No TLS; room tokens and session tokens are transmitted in plaintext

**Area**: Transport security
**Severity**: Low (localhost/trusted network), High (untrusted network)
**Status**: Open

The server currently speaks plain HTTP. Room tokens and session tokens are transmitted without encryption. On a trusted LAN or localhost this is acceptable for MVP, but any network path that traverses untrusted infrastructure exposes credentials to interception.

**Resolution**: Add HTTPS support with operator-supplied certificate, or support a TLS-terminating reverse proxy (e.g. nginx). Document the recommended setup for network deployments.

`session_token` is credential-grade: a captured token exposes every room the session belongs to (via `join`, given the attacker's own room/seat credential), not just one. A plaintext-transport capture is therefore a bigger prize; this raises the urgency of resolving this issue for any non-localhost deployment.

---

## ISS-003 — Member disconnect is not reliably detectable

**Area**: Presence / last-seen
**Severity**: Low
**Status**: Open — partially mitigated by last-seen, seat tokens, and the non-seat member idle TTL

The server can detect a member disconnect only while an active blocking tool call (receive, send-with-reply) is in progress. Between calls, the server cannot distinguish a member that is thinking from one that has crashed or been terminated. The `last_seen` field in tool responses provides a best-effort staleness signal, but does not guarantee the member is actually online.

**Seat token mitigation**: An agent provisioned with a seat token can evict its own stale session on reconnect unconditionally — it does not need to know whether a prior session is live or stale. This resolves the "stuck member name" variant of this problem. The fundamental detectability gap (is the member alive right now?) remains.

**Idle-TTL mitigation**: a non-seat (room-token-authed) member is now reaped by the sweeper once idle past the operator-configured `member_idle_ttl_secs` (`1800..=86400`), so a crashed or abandoned agent no longer lingers as a phantom member indefinitely — it is bounded to at most one TTL window plus one sweep interval. This is a *cleanup* bound, not real-time liveness: the window is ≥30 min, and seat-authed members (privileged by design) are exempt, so the "is the member alive right now?" gap remains for both.

**Resolution**: See `PLAN.disconnect-detection.md` (to be written) for a heartbeat/keepalive design.

---

## ISS-006 — Seat token revocation requires server-side deletion

**Area**: Security / seat tokens
**Severity**: Medium
**Status**: Open

Deleting a seat token file from disk does not cryptographically invalidate it. The HMAC output remains valid until the corresponding `MemberSeat` is removed from the server via `DELMB`. An operator who intends to revoke access by deleting the file only (e.g. on a remote agent machine) leaves a valid credential in circulation.

**Workaround**: Always revoke via `toaster-talk-cli delmb <room> <member>` rather than deleting the token file. If the file was copied to a remote machine and cannot be confirmed destroyed, rotate the room token (delete and recreate the room) to invalidate all credentials derived from it.

---

## ISS-007 — Master secret rotation invalidates all tokens with no migration path

**Area**: Security / operations
**Severity**: Medium
**Status**: Open

Rotating the master secret (e.g. after a suspected compromise) invalidates all room tokens and seat tokens derived from it. There is no rotation procedure — the operator must delete and recreate every room and redistribute all token files. Agents must reconnect with new credentials.

**Workaround**: Keep the master secret file at mode `0600`, operator-owned. Treat it as a long-lived credential that is not rotated routinely.

---

## ISS-008 — Join/delete TOCTOU windows can register memberships against a deleted room

**Area**: Concurrency
**Severity**: Negligible (NV)
**Status**: Open — documented, accepted

Both `join` paths (room-token and seat-token) verify the credential under a `RoomStore` read lock, release it, then acquire the `AppState` write lock to register the membership. A concurrent deletion in that window can attach a membership against a room or seat that no longer exists in the store. The window lives entirely on `join` (`tools/join.rs`) — the only tool that ever registers a room membership — and recurs once per room a session joins. Shape and severity are negligible: the resulting membership is harmless (evicted on the next TTL sweep or when the room/seat is recreated), self-healing, and still requires a pre-existing valid credential to enter.

**Resolution (optional)**: Re-verify room/seat existence under the `AppState` write lock by re-reading the store, accepting the extra lock round-trip. Not currently judged worth the cost.

---

## ISS-009 — `SessionToken` and `MemberName` remain type aliases, not newtypes

**Area**: Type safety
**Severity**: Negligible (NV)
**Status**: Open — accepted by design

After the M-R5-002 migration, `RoomToken` and `SeatToken` are newtypes, but `SessionToken` and `MemberName` (`state.rs`) are still `pub type … = String`. Adjacent same-typed parameters (e.g. `register_member(member_name, session_token)`) could be transposed without a compile error.

**Decision**: Left as aliases deliberately. Unlike room/seat tokens — which are persisted, security-sensitive credentials whose confusion would be a correctness/security bug — session tokens and member names are in-memory-only identifiers that never cross the persistence or HMAC boundary. The newtype boilerplate (a second `impl_token_newtype!` expansion plus `.to_string()`/`.as_str()` churn at every `String` interop point) is not justified by the marginal safety gain. Revisit if a transposition bug ever occurs in practice.

---

## ISS-010 — Two agents sharing one seat credential cause eviction thrash (reconnect livelock)

**Area**: Auth / seat tokens
**Severity**: Medium
**Status**: Open

A seat token *is* the member identity — the server cannot distinguish the legitimate holder of seat `<room>/<member>` from a second agent that also holds (or was mistakenly handed) the same seat file. Both are cryptographically `<member>`. Authoritative seat eviction (a new connect unconditionally supersedes the prior session for that member — the ISS-003 mitigation) is therefore symmetric: when **two** live agents hold one seat, each agent's `connect` evicts the other, the evicted agent reconnects and evicts the winner, and the two ping-pong indefinitely.

**Observed incident** (`surfing` room, 2026-06-20): an agent joined as `dev` using `dev`'s seat while the real `dev` was mid-`receive`. Logs show six `dev` supersede→reconnect cycles in ~70 s (`c3954f88 → c5733953 → a5dba408 → cbb36e36 → 0373d27c → 5e134c48`). Every cycle aborted the victim's in-flight `receive` (delivered as `Superseded`), so `dev` "received nothing" for the duration — not a server deadlock, but a credential-collision livelock. It self-resolved only when the operator renamed the rogue agent to a different member name. Server state throughout was structurally healthy (`invariants: OK`, single live receiver) — this is an auth/identity gap, **not** a zombie-waiter bug (cf. ANTI-PATTERNS #8) nor the disconnect-detectability gap (cf. ISS-003).

**Why the existing design does not catch it**: eviction has no "first holder wins" precedence, no cooldown, and no flap detection. Each reconnect is individually valid, so nothing rejects the loop. The `dump` diagnostic shows the *aftermath* (`superseded_sessions` accumulates) but surfaces no rate signal an operator could alert on.

**Mitigation options (to evaluate — none shipped):**
- **Post-supersede cooldown / hold-down**: protect a freshly connected session from being superseded for a short grace window (e.g. 2–5 s). The first agent to claim the seat holds it; the second's immediate reconnect is rejected with a soft, retryable error instead of evicting the holder — breaking the ping-pong. Trade-off: a genuinely stale session briefly blocks a legitimate reconnect until the window lapses.
- **Flap detection**: count supersessions per seat within a sliding window; on exceeding a threshold (e.g. >3 in 30 s) emit a `WARN` ("seat credential contention — possible shared token") and optionally freeze the seat pending operator action. Turns a silent livelock into an actionable signal.
- **Diagnostic surfacing**: add a recent-supersede counter (and/or last-supersede age) per member to the `DUMP` snapshot so contention is visible before an operator has to read the journal.

**Status decision**: left open and unfixed deliberately. The operator workaround below is straightforward and the collision is operator-induced (two agents handed one seat), so the mitigations above are not currently judged worth their complexity. Revisit if the collision recurs in normal operation rather than by misconfiguration.

**Operator workaround today** — the reliable fix is to break the loop by hand: **cancel the in-flight request on either side**. The thrash is sustained only because both agents keep re-issuing `connect`/`receive`; the moment one side's in-flight call is cancelled and that agent stops reconnecting, the other agent's next connect wins uncontested and the room settles. Concretely:

1. Identify the contended seat — `dump` shows rapid `superseded_sessions` growth and a member whose `session_prefix` keeps changing; the journal shows repeated `superseded by seat-holder reconnect` lines for one member.
2. Stop one of the two agents (cancel its current tool call / pause it) so it is no longer reconnecting — *either* side will do; you do not need to identify the "real" one, because the seat makes them identical.
3. The surviving agent's next `connect` is now uncontested. If it is wedged on a stale call, `kckm <room> <member>` once to force a clean reconnect.

Prevention: ensure each seat file is held by exactly one agent. If a rogue agent must stay in the room, give it its **own** seat (`addmb <room> <other-name>`) and have it reconnect under that distinct member name — never share one seat across two live agents.

Eviction is per-room-membership rather than whole-session, so a seat fight over one room (`join`'s seat-token collision path) no longer collaterally evicts the victim's other live room memberships under the same session — only the contended room thrashes. The core livelock (two agents sharing one seat credential) is otherwise unchanged.

---

## ISS-011 — A send-with-reply reply can be orphaned if the recipient reconnects mid-wait

**Area**: Send-with-reply / reconnect
**Severity**: Low
**Status**: Open

When an agent is parked in a send-with-reply to member B and B reconnects (supersede) during the wait, the sender's call unblocks promptly with `RecipientEvicted` — the recipient's old inbox is closed on supersede. The request itself is carried into B's new inbox (reconnect queue-transfer, ANTI-PATTERNS #11), so B does see and can answer it, but the sender has already returned. B's reply then lands in the sender's inbox with no one waiting (the sender must do a later `receive` to find it), and if the sender retries the send on `RecipientEvicted`, B receives the request twice.

This is the trade-off of the transfer-queue reconnect design: the *request* survives the reconnect, but the *reply* around a concurrent reconnect can be orphaned or duplicated.

**Mitigation options:**
- Accept it — the sender gets a clear `RecipientEvicted` and can retry; the window (a reconnect landing exactly during a send-with-reply to that member) is narrow.
- Adopt a "reuse the old inbox" reconnect design instead of transfer-queue: keep the same `Arc<Inbox>` across reconnect (abort the old receiver, swap the session token, do not close it), so the waiting sender is never evicted and receives the real reply. Larger change; needs care around the generation counter and the close / `RecipientEvicted` contract.

