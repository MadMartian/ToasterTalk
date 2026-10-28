# ToasterTalk MCP — Known Issues

This is a living register of known problems, limitations, and deferred concerns. Items are removed when resolved.

| ID | Issue | Severity |
|----|-------|----------|
| ISS-003 | Member disconnect is not reliably detectable | Low |

## Closed issues

Accepted limitations: no fix is planned. Do not reopen without a new constraint.

| ID | Issue |
|----|-------|
| CLSD-01 | No TLS; room tokens and session tokens are transmitted in plaintext |
| CLSD-02 | Seat token revocation requires server-side deletion |
| CLSD-03 | Master secret rotation invalidates all tokens with no migration path |
| CLSD-04 | A send-with-reply reply can be orphaned if the recipient reconnects mid-wait |

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

## CLSD-01 — No TLS; room tokens and session tokens are transmitted in plaintext

**Area**: Transport security
**Status**: Closed (accepted limitation)

The server currently speaks plain HTTP. Room tokens and session tokens are transmitted without encryption. On a trusted LAN or localhost this is acceptable for MVP, but any network path that traverses untrusted infrastructure exposes credentials to interception.

`session_token` is credential-grade: a captured token exposes every room the session belongs to (via `join`, given the attacker's own room/seat credential), not just one. A plaintext-transport capture is therefore a bigger prize, so any non-localhost deployment must put TLS in front of the listener.

**Why it's closed**: accepted limitation. ToasterTalk is a local daemon for agents on one machine or a trusted LAN, and its value is a small, dependency-light server. In-process TLS would add certificate provisioning, renewal, and a TLS stack to every deployment to serve a case the operator does not run. Untrusted-network deployments are covered without project changes by a TLS-terminating reverse proxy (e.g. nginx, Caddy) or an SSH tunnel in front of the plain-HTTP listener. Reopen only if a deployment must expose the listener directly to an untrusted network.

---

## CLSD-02 — Seat token revocation requires server-side deletion

**Area**: Security / seat tokens
**Status**: Closed (accepted limitation)

Deleting a seat token file from disk does not cryptographically invalidate it. The HMAC output remains valid until the corresponding `MemberSeat` is removed from the server via `DELMB`. An operator who intends to revoke access by deleting the file only (e.g. on a remote agent machine) leaves a valid credential in circulation.

**Workaround**: Always revoke via `toaster-talk-cli delmb <room> <member>` rather than deleting the token file. If the file was copied to a remote machine and cannot be confirmed destroyed, rotate the room token (delete and recreate the room) to invalidate all credentials derived from it.

**Why it's closed**: accepted limitation, inherent to the credential model. A seat token is a deterministic bearer credential (`HMAC(master_secret, room || '\0' || member)`); possession *is* authorization, so destroying one copy of the file can never invalidate the other copies. Server-side revocation via `delmb` is the revocation mechanism, and rotating the room (or master secret) is the escalation for a leaked copy. Making file deletion revoke would require per-token server state tied to file lifecycle, which a bearer-token design cannot observe.

---

## CLSD-03 — Master secret rotation invalidates all tokens with no migration path

**Area**: Security / operations
**Status**: Closed (accepted limitation)

Rotating the master secret (e.g. after a suspected compromise) invalidates all room tokens and seat tokens derived from it. There is no rotation procedure — the operator must delete and recreate every room and redistribute all token files. Agents must reconnect with new credentials.

**Workaround**: Keep the master secret file at mode `0600`, operator-owned. Treat it as a long-lived credential that is not rotated routinely.

**Why it's closed**: accepted limitation, inherent to the credential model. Every room and seat token is an HMAC derivation of the master secret, so changing the key necessarily changes every derived token. A migration path would mean dual-key verification windows and a token redistribution channel, which is disproportionate for a secret that is not rotated routinely. Rotation stays a deliberate, rare operator procedure: delete and recreate rooms, then redistribute token files.

---

## CLSD-04 — A send-with-reply reply can be orphaned if the recipient reconnects mid-wait

**Area**: Send-with-reply / reconnect
**Status**: Closed (accepted limitation)

When an agent is parked in a send-with-reply to member B and B reconnects (supersede) during the wait, the sender's call unblocks promptly with `RecipientEvicted` — the recipient's old inbox is closed on supersede. The request itself is carried into B's new inbox (reconnect queue-transfer, ANTI-PATTERNS #11), so B does see and can answer it, but the sender has already returned. B's reply then lands in the sender's inbox with no one waiting (the sender must do a later `receive` to find it), and if the sender retries the send on `RecipientEvicted`, B receives the request twice.

This is the trade-off of the transfer-queue reconnect design: the *request* survives the reconnect, but the *reply* around a concurrent reconnect can be orphaned or duplicated.

**Why it's closed**: accepted limitation of the transfer-queue reconnect design. The window is narrow (a recipient reconnect landing exactly during a send-with-reply to that member), and the sender always gets a clear `RecipientEvicted`; the request survives the reconnect and the reply is retrievable by a later `receive`. The only alternative, reusing the old `Arc<Inbox>` across reconnect, reworks the generation counter and the close/`RecipientEvicted` contract, a cost judged out of proportion to the window.
