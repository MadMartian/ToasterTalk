# Manual Multi-Agent Test Suite

These are **integration tests that `cargo test` cannot express.** They exercise the
one layer the in-process Rust suite is blind to: the real MCP transport plus real
client **disconnect / reconnect / concurrency**, with the operator `dump` as the
ground-truth oracle. Every entry here was authored from a *reproduced* behavior,
not a hypothesis — several of them surfaced defects the automated suite missed
(reconnect message-drop, an invariant false-positive), exactly the class of blind
spot that ANTI-PATTERNS #5, #7, and #9 document: in-process test helpers give false
greens because they bypass the transport/disconnect path.

Treat this suite like integration tests: **run it after any change touching session
lifecycle (connect / supersede / evict), the inbox / receive slot, send-with-reply,
the HTTP/SSE transport, or the TTL sweep.** See POLICY.md (build pipeline).

---

## Isolation model — why this can't touch production

| | Production (Unix) | Production (Windows) | Dev (this suite) |
|---|---|---|---|
| Port | 6669 | 6669 | **6668** |
| Launch | systemd | Windows service (`deploy/install.ps1`) | `make dev-up` (from the repo) |
| State | `/var/lib`, `/run` | `%ProgramData%\ToasterTalk` | `/tmp/toaster-talk-dev/`, `target/dev-instance/` (ephemeral / gitignored) |
| Scratch files | n/a | n/a | `target/dev-instance/scratch/` — any transient file the orchestrator or a probe writes goes here, never the repo root; created by `dev-up`, wiped by `dev-down` |
| MCP registration | user `~/.claude.json` | user `~/.claude.json` | `deploy/dev-mcp.json`, loaded **per-invocation only** |

The `make dev-up`/`make dev-down` dev harness itself is POSIX-shell tooling — it
runs fine on a Windows machine under Git Bash or WSL (both provide `/tmp` and a
`make`-compatible shell), but has no native PowerShell port. A Windows dev box
without either can still exercise the server directly (build, run
`toaster-talk-server.exe --config <path>`, drive it with `toaster-talk-cli.exe`)
without this suite's automated orchestration.

The dev MCP server (`ToasterTalkDev → http://localhost:6668/sse`) is **deliberately
not** in `.mcp.json`, so no ordinary session — including the one you develop in —
ever loads it or has to reconnect a stale endpoint. The test session is launched
with `--mcp-config deploy/dev-mcp.json --strict-mcp-config`, so it sees **only** the
dev server (not even prod ToasterTalk). Isolation is enforced in both directions.

---

## Running the suite

```sh
make dev-up        # build + boot the isolated :6668 instance, wait for ready
make manual-test   # launch the strict-config orchestrator session (runs, then dev-down)
# ...or drive it yourself:
claude --mcp-config deploy/dev-mcp.json --strict-mcp-config \
       --append-system-prompt-file toaster-talk-server/tests/orchestrator-brief.md
make dev-down      # stop instance, wipe dev state (master.key retained)
```

**Headless (`make manual-test`) is the primary mode** — it runs unattended so you can
do other things while the suite works. It is slower than interactive (a full run can
take several minutes: `-p` buffers output to the end, and each probe sub-agent spin-up
plus model latency dominates wall-clock), but unattended-ness wins. For live
visibility on a headless run add `--output-format stream-json --verbose`, and expect
to raise the timeout. **Interactive** (`claude --mcp-config … --strict-mcp-config`,
no `-p`) is the secondary mode — use it to *debug* a scenario, watching output live
and approving tools by hand.

**Cost controls (baked into the briefs):**
- **Probes run on Haiku** (`model: haiku`) — the steps are mechanical (connect / send /
  receive / report) and need no deep reasoning; Haiku is much faster per turn.
- **Probes try the MCP tool directly and only `ToolSearch` if it isn't present** — in a
  `--strict-mcp-config` session the ~4 dev tools are typically not deferred, so the
  ToolSearch round-trip is skipped.

The **orchestrator** is the launched session itself (not a sub-agent — sub-agents
can't reliably spawn further sub-agents). It drives each scenario and spawns the
**probe** clients as plain `claude`-type sub-agents. Custom `.claude/agents/`
profiles are intentionally **not** used: `.claude/` is gitignored, `--agents` takes
inline JSON not a directory, and (per Claude Code issue #25200) a custom agent with
a restricted `tools:` list may not receive MCP tools at all. General sub-agents
driven by the probe brief below are proven to work and sidestep all three.

### Note: connect/join split (read before running any scenario below)

`connect` is a pure identity mint — it takes at most `old_session_token` and
never a room/seat token or member name. Every room membership, including a
probe's very first, is created by a separate `join` call: `join` with
`session_token` + `seat_token` + `member_name` (or `room_token` +
`member_name`). Wherever the scenarios below say a probe "connects (seat)",
read that as **connect (mint a session token) then join with the seat token** —
two calls, not one. Wherever they say a probe "reconnects (connect again with
its seat → supersede)", read that as: **connect fresh (roomless, no
`old_session_token` — the seat token itself proves identity) then join again
with the same seat token** — the `join`
seat-token branch is what unconditionally evicts and supersedes the prior
holder, exactly as `connect`'s seat path used to. `connect`'s `old_session_token`
is a different thing now: it rotates and *invalidates* an existing session
(and every room membership under it) with no carryover — it is never the right
tool for "reconnect to my room," which is what these older scenarios mean by
"reconnect."

### Probe management commands (dev CLI)

```sh
DEV="target/release/toaster-talk-cli --socket /tmp/toaster-talk-dev/mgmt.sock --token-dir target/dev-instance/tokens"
$DEV addrm devroom                 # create the test room
$DEV addmb devroom probe_a         # seat → target/dev-instance/tokens/user/devroom/probe_a.tok
$DEV dump | yq '...'               # ground-truth snapshot
$DEV delmb devroom probe_a         # evict + delete seat (cleanup)
```

---

## ⚠️ Methodology rule — dump-verify every concurrency step

**This is not optional, and it is the single most important rule in this file.**
The first time MT-2 was run, loose timing produced a *false* result: the reconnect
landed *after* the sender's wait had already elapsed, so the sender reported a normal
timeout and it looked like `RecipientEvicted` didn't fire. It does. The lesson:

> Before reading the outcome of any concurrent step, **assert the precondition with a
> `dump`** — e.g. confirm the sender shows `receive_active: true` *and* the recipient
> shows the superseded/new session, at the exact interleaving point. A step whose
> timing you did not verify is not a result.

Use polling loops that gate on the dump, never fixed sleeps, to reach the required
interleaving. (This trap is recorded as an ANTI-PATTERN.)

---

## The probe brief (template for spawning a probe sub-agent)

Spawn probes as `claude`-type sub-agents **with `model: haiku`** and a prompt built
from this template. Keep it literal — probes must not improvise, poll, or loop.

```
You are a controlled ToasterTalk test client. Follow the numbered steps LITERALLY.
Do NOT improvise, do NOT poll your inbox, do NOT loop, do NOT do anything beyond the
steps. Report exact tool results / error text verbatim, then stop.

Talk to the server ONLY through the mcp__ToasterTalkDev__* tools listed below — never
curl, nc, or any other shortcut, even if a step feels slow or fiddly to hit that way.
If you genuinely need to write a transient file, it goes under
target/dev-instance/scratch/ (never the repo root) — but the steps below should never
require one.

Identity: room "<ROOM>", member "<MEMBER>".
Seat token file: <ABS PATH>/target/dev-instance/tokens/user/<ROOM>/<MEMBER>.tok
(3 lines: room, member, seat token.)

Steps:
1. Call the mcp__ToasterTalkDev__ tool the step needs DIRECTLY. Only if it is not in
   your available tools, first run ToolSearch "select:mcp__ToasterTalkDev__connect,
   mcp__ToasterTalkDev__join,mcp__ToasterTalkDev__send,mcp__ToasterTalkDev__receive",
   then call it.
2. Call connect (no arguments) to obtain session_token. Then call join with that
   session_token, your seat_token, and member_name — this attaches your room
   membership. (Two calls — connect never takes a room/seat token or member name.)
3. <the specific step(s) for this scenario>
Report: <exactly what to report>.
```

Reuse a probe's session across turns via SendMessage to its agent id (the same
member stays registered between turns).

---

## Scenarios

Each scenario records the **current** result (today's behavior) and the **target**
result (after the corresponding fix). As fixes land, flip current→target and this
file doubles as the fix-tracking checklist.

### MT-1 — Reconnect drops queued messages  *(defect #3)*

**Purpose:** a message queued for a member must not vanish when that member reconnects.

**Steps:**
1. probe_b connects (seat). Leave it idle.
2. probe_a connects, sends probe_b a fire-and-forget `PROBE-CONTROL`.
3. probe_b `receive` (no reconnect) → **receives `PROBE-CONTROL`** (positive control: delivery works).
4. probe_a sends probe_b a fire-and-forget `PROBE-TEST`.
5. **Verify dump:** probe_b `queue_depth == 1`, message id present.
6. probe_b **reconnects** (connect again with its seat → supersede), then `receive`.

**Expected dump signature after step 6:**
```
probe_b: queue_depth == 0, gen_counter reset to a LOW value (the fresh Inbox's own
         counter — far below the old inbox's; equals the number of receives issued
         on the new session, so ~1–2), session prefix changed
superseded_sessions: contains the OLD probe_b session
invariants: "OK: all invariants satisfied"
```
| | Result |
|---|---|
| **Current (pre-fix)** | Step 6 receive returns **NONE** — `PROBE-TEST` silently dropped. Clean dump. |
| **Target (post-fix)** | Step 6 receive returns **`PROBE-TEST`** (queue transferred across reconnect). |

### MT-2 — Send-with-reply survives recipient supersede  *(escape-hatch guard)*

**Purpose:** a sender parked in send-with-reply must be unblocked *promptly* (not hang
the full window) if the recipient reconnects/evicts mid-wait.

**Steps:**
1. probe_a `send` to probe_b with `await_reply_secs=60` (parks).
2. **Dump-verify** probe_a `receive_active: true` AND probe_b `queue_depth: 1` (parked + request delivered).
3. probe_b **reconnects** (supersede) — **dump-verify** probe_b session changed while probe_a still `receive_active: true`.
4. Observe probe_a's return.

| | Result |
|---|---|
| **Current & target** | probe_a returns **`recipient 'probe_b' was evicted during the send; message not delivered`** promptly (well under 60s). Guards regression of the `recipient_inbox.closed()` race. |

Note: the request message is also dropped and the sender must retry — this is the AX
seam that motivates the reconnect queue-transfer (MT-1 fix).

### MT-3 — Invariant fidelity under live send-with-reply  *(diagnostic defect)*

**Purpose:** the dump's `!receive_active && arc_strong_count > 1` invariant must not
cry wolf during healthy in-flight traffic.

**Steps:**
1. probe_a `send` to probe_b with `await_reply_secs=30` (parks).
2. While parked, **dump the invariants**.

**Observed:** probe_b shows `receive_active: false, arc_strong_count: 3` (the sender
legitimately holds two clones of the recipient inbox), and the dump emits:
```
WARN: member 'probe_b' ... no active receive slot but ... 2 extra Arc holder(s) ...
      likely orphaned ... these can steal message wakeups
```
This is a **false positive** — nothing is orphaned; it clears the instant the send
completes (arc → 1).

| | Result |
|---|---|
| **Current (pre-fix)** | False WARN emitted during every healthy send-with-reply to a non-receiving member. |
| **Target (post-fix)** | No warning during healthy send-with-reply — warn only on **persistence** across ≥N sweeps, or account for known in-flight senders. |

### MT-4 — Dead-seat reaper  *(defect #1 — requires the reaper fix)*

**Purpose:** a seat whose client died is reaped so it stops black-holing messages.
Requires `seat_idle_ttl_secs` set (dev config uses 15s) **and** the reaper sweep logic.

**Steps:**
1. probe_b connects (seat), then goes fully idle (no receive).
2. Wait past `seat_idle_ttl_secs` (dev: >15s; sweeper runs every 5s).
3. **Dump-verify** probe_b is gone from devroom.
4. probe_a `send` to probe_b with `await_reply_secs=20`.

| | Result |
|---|---|
| **Post-fix — FULLY CONFIRMED (2026-07-17)** | Reaping: idle seats past `seat_idle_ttl_secs` are reaped — `member evicted ... reason=seat idle TTL expired (dead seat reaped)` (dev.log). Step 4: a `send` to the already-reaped seat returns **`MemberNotFound`** — `error: member not found in room: probe_b` (dev.log response id=4, is_error=true), because reaping *removes* the member from the room. Note this is **not** `RecipientEvicted`: that error only arises in the close-*race* (a send that captured the recipient inbox before the reap closed it). Both are honest "recipient is gone" fail-closed signals — neither black-holes. |
| **Prior (pre-fix)** | probe_b **never reaped** (seats exempt); step 4 black-holed / eventually timed out. |

### MT-5 — A preempted receive reports `preempted`  *(status-fidelity fix — VERIFIED 2026-07-17)*

**Purpose:** there is only **one** receive slot per inbox, so a second blocking waiter
preempts (aborts) the first. The preempted call returns an empty batch — and it must
say so honestly with `timeout: preempted`, NOT `none` (which the schema defines as
"messages returned", so an agent could mistake it for a real empty delivery) and NOT
`elapsed` (which claims the full wait ran). One `receive` already drains every queued
message in a single batch, so a second concurrent wait is pointless — the status is
how an agent learns that.

**⚠️ How to actually reproduce this — a single agent CANNOT.** The Claude Code MCP SSE
client **serializes same-session calls** (single in-flight): two `receive` calls issued
"in parallel" from one session do not overlap — the client holds the second until the
first returns, so a lone agent always gets `{elapsed, elapsed}` and never a preempt.
(Proven in-run: the 2nd request arrived ~3 ms *after* the 1st's response.) You need a
genuine second concurrent claimant on the **same inbox**:
- **Two clients sharing one `session_token`** — park a `receive` as the member, then a
  second client (a background sub-agent) fires a `receive` **reusing that same
  session_token** (do NOT `connect` again — connecting *supersedes/evicts*, a different
  path). The later claimant preempts the parked one.
- **Seat supersede** also produces it as a side effect: a new `connect` for the same
  seat aborts the prior session's parked receive (observed in-run — the superseded
  session's receive returned `preempted`).

**Steps (same-token method):**
1. probe_a connects (seat), capturing its `session_token`.
2. Client A calls `receive` (`timeout_secs=20`) with that token — it parks.
3. **Dump-verify** probe_a `receive_active: true` before proceeding (the park is real).
4. Client B calls `receive` (`timeout_secs=20`) with the **same** `session_token`.
   Report BOTH calls' `timeout` field and message count verbatim.

**Expected outcome:** the parked call (A) returns `timeout: preempted` with an empty
batch; the later call (B) returns `timeout: elapsed` empty (it parked and 20 s
expired). Ground-truth oracle is the server log, not just the dump — look for
`receive sub-task cancelled — preempted by new receive` immediately followed by a
`CallToolResult` whose text is `timeout: preempted\nmessages: []`.

| | Result |
|---|---|
| **Current (pre-fix)** | The preempted call returned a bare `messages: []` with **no** `timeout` line (the `None` variant is not rendered) — indistinguishable from a real empty delivery. |
| **Target (post-fix) — CONFIRMED** | The preempted call returns `timeout: preempted` (empty); the survivor returns `elapsed`. Verified on the wire: two independent events returned `timeout: preempted\nmessages: []` (dev.log response ids 13 & 24, 2026-07-17). |

**Timing caveat:** the dev dead-seat reaper (`seat_idle_ttl_secs = 15`) evicts an idle
seat ~15 s after connect. Keep connect→park→preempt back-to-back; orchestrator thinking
latency between steps alone can blow the window and yield `invalid or expired session
token` instead of a clean preempt.

### MT-6 — Fire-and-forget `send` must NOT drain the sender's own inbox  *(implicit receive-on-send blackhole — REPRODUCED then FIXED 2026-07-17)*

**Purpose:** guard the fix for the real message-loss path. The prior behavior (rubric
2.3, "implicit receive-on-send") had a fire-and-forget `send` drain the *sender's own*
inbox and return those messages in the `send` response body. An agent that called
`send` and ignored the response — the ToasterTalk instructions never said a plain
`send` could return messages — permanently lost whatever was waiting, because the drain
already removed it. The fix drops the drain entirely: fire-and-forget is now strictly
one-way (rubric 2.3 rewritten), and mail is delivered only via `receive`.

**Steps:**
1. probe_b connects (seat). Leave it idle (no receive).
2. probe_a connects, sends probe_b a fire-and-forget `MSG-INBOUND`.
3. **Dump-verify** probe_b `queue_depth == 1` (MSG-INBOUND is waiting in its inbox).
4. probe_b sends probe_a a **fire-and-forget** `MSG-OUTBOUND` (an unrelated outbound
   message — NO `await_reply_secs`). Report probe_b's full `send` response verbatim,
   including any `messages:` block and its count.
5. probe_b then calls `receive` (`timeout_secs=3`). Report its result verbatim.

**Dump-verify (post-fix):** after step 4, probe_b `queue_depth == 1` — the send did
**not** drain the inbox; it drops to 0 only after step 5's `receive`. (Pre-fix it went
to 0 at step 4.) Because the dev reaper evicts an idle seat in ~15 s, keep steps 2/4/5
tight — probe_b's own `send` must land inside its 15 s window or it is reaped first.

| | Result |
|---|---|
| **Prior (pre-fix) — was CONFIRMED** | Step 4's `send` response contained **MSG-INBOUND** (`message_count=1`); step 5 `receive` returned **empty** — blackholed if the send response was ignored. Server-log proof (dev.log, 2026-07-17 16:05:29): `implicit receive-on-send: delivering waiting message ... from=probe_a`, then `messages returned to sender in send response sender=probe_b message_count=1`. |
| **Target (post-fix) — decided: DROP the drain** | Step 4's `send` response contains **no** `messages:` block (fire-and-forget is one-way); step 5 `receive` returns **MSG-INBOUND**. Covered by the unit-level regression `test_fire_and_forget_is_one_way` (integration.rs → TDD 2.3); this manual scenario additionally checks it over the real transport. |

### MT-7 — connect/join split: join is the only path to a room membership, and `old_session_token` never carries one over

**Purpose:** verify live, over the real transport, that (a) `send`/`receive` before
any `join` fail cleanly rather than hang or silently register a room, and (b)
rotating via `connect`'s `old_session_token` genuinely tears down every room
membership with zero carryover — real client, real disconnect semantics, exactly
the blind spot ANTI-PATTERNS #5/#7/#9 flag for `cargo test`.

**Steps:**
1. probe_a calls `connect` (no arguments) and captures `session_token_1`.
2. probe_a calls `receive` (`timeout_secs=3`) with `session_token_1`, **before any
   `join`**. Report the exact error text verbatim.
3. probe_a calls `join` with `session_token_1`, its seat token, and its member name.
   **Dump-verify** probe_a now appears under `devroom` with `session_in_map: true`.
4. probe_b sends probe_a a fire-and-forget `MSG-BEFORE-ROTATE`. **Dump-verify**
   probe_a's session inbox shows `queue_depth: 1`.
5. probe_a calls `connect` again, this time **with** `old_session_token:
   session_token_1`. Capture the returned `session_token_2` (must differ from
   `session_token_1`).
6. **Dump-verify:** `session_token_1`'s prefix no longer appears anywhere in
   `sessions:`, and probe_a's member entry is gone from `devroom`'s `members:`.
7. probe_a calls `receive` (`timeout_secs=3`) with `session_token_2`. Report the
   exact result verbatim.

**Expected outcome:**
- Step 2: a descriptive error indicating no room has been joined yet (`NotJoinedAnyRoom`) — not a hang, not a silent empty result indistinguishable from "no mail yet."
- Step 6: `devroom`'s member list no longer contains probe_a at all (the old
  membership was removed, not merely relabeled), and no session entry redacts to
  `session_token_1`'s prefix.
- Step 7: `receive` with the rotated token returns an error indicating no room has
  been joined (`NotJoinedAnyRoom`) — `MSG-BEFORE-ROTATE` is gone, confirming
  `old_session_token` rotation does not carry over room membership or queued mail
  (TDD 1a.4). Recovery requires an explicit `join` with `session_token_2` (not
  exercised further here — MT-1's queue-transfer guarantee is scoped to `join`'s
  seat-token supersede, not to `connect` rotation).

| | Result |
|---|---|
| **Target** | Steps 2 and 7 both report a clear "join first" style error, never a silent empty receive indistinguishable from an idle inbox; step 6's dump shows the old session and its room membership both fully gone. |

### MT-8 — Cross-room eviction isolation: one room's eviction never touches another (TDD 10.21 / 10.22)

**Purpose:** with one shared inbox now spanning every room a session has joined,
confirm live that evicting a session from room A leaves its room B membership,
and the shared inbox itself, completely functional — and that room B's traffic
during the eviction is not lost or misdelivered into room A's now-gone context.
This is exactly the kind of interleaving-sensitive guarantee `cargo test`'s
in-process helpers can give a false green on (ANTI-PATTERNS #13) — verify by dump,
not assumption.

**Setup:** create a second room `devroom2` (`$DEV addrm devroom2`) and a seat for
probe_a in it (`$DEV addmb devroom2 probe_a`) in addition to its existing
`devroom` seat.

**Steps:**
1. probe_a: `connect` → `session_token`. `join` `devroom` with its `devroom` seat
   token. `join` `devroom2` with its `devroom2` seat token (same session_token
   both times). **Dump-verify:** exactly one `sessions:` entry for probe_a's
   session prefix, with `memberships:` listing both `devroom/probe_a` and
   `devroom2/probe_a`.
2. Operator runs `$DEV kckm devroom probe_a` (evict only the `devroom` membership).
3. **Dump-verify:** `devroom`'s member list no longer contains probe_a; `devroom2`'s
   member list still does; the `sessions:` entry for probe_a's session prefix
   still exists (not torn down) with `memberships:` now listing only
   `devroom2/probe_a`.
4. probe_b (a `devroom2` member) sends probe_a a fire-and-forget `MSG-STILL-GOOD`.
5. probe_a calls `receive` naming `room: devroom2` explicitly. Report the result.
6. probe_a calls `send` naming `room_name: devroom` to any member. Report the
   exact error text verbatim.

**Expected outcome:**
- Step 3: the session is NOT superseded and NOT torn down — only the one room
  membership is gone (TDD 10.21's "no evicted error results" guarantee extends to
  the shared session surviving intact).
- Step 5: probe_a receives `MSG-STILL-GOOD` normally — the shared inbox kept
  working for the surviving room throughout.
- Step 6: a descriptive `NotConnectedToRoom` error (TDD 10.22) — not `Superseded`,
  since the session as a whole was never invalidated, only that one room
  membership.

| | Result |
|---|---|
| **Target** | All three assertions hold: room A eviction is invisible to room B's traffic, the session survives, and a subsequent request naming the evicted room gets `NotConnectedToRoom` rather than a confusing `Superseded` or a hang. |

### MT-9 — Partial eviction does not abort a live parked wait (TDD 10.10 / 10.10b)

**Purpose:** `evict_one_room_does_not_abort_receive_parked_on_surviving_room`
(`state.rs`) already proves this in-process; this scenario checks the same claim
over the real transport, since a parked receive interacting with an eviction
sweep is exactly the concurrency class `cargo test`'s in-process helpers can give
a false green on (ANTI-PATTERNS #5/#7/#9). TDD `10.10` aborts a parked wait only
when the eviction empties the session's *last* room membership; `10.10b` says a
partial eviction (another room survives) must leave an in-flight wait running
untouched.

**Setup:** reuse MT-8's two rooms (`devroom`, `devroom2`), both seated for probe_a.

**Steps:**
1. probe_a: `connect` → `session_token`, `join` `devroom`, `join` `devroom2` (same
   session both times). **Dump-verify** one `sessions:` entry with both
   memberships listed.
2. probe_a calls `receive` (`timeout_secs=25`, no `room` filter — blocks on the
   shared inbox for either room). **Dump-verify** `receive_active: true` before
   proceeding (the park is real).
3. Operator runs `$DEV kckm devroom probe_a` (evicts only the `devroom`
   membership) **while the step-2 receive is still parked**.
4. **Dump-verify immediately after the kckm:** `devroom` membership is gone,
   `devroom2` membership still present, session still exists — **and
   `receive_active` is still `true`**. This is the core assertion: the partial
   eviction must not have preempted the parked wait.
5. probe_b (a `devroom2` member) sends probe_a a fire-and-forget
   `MSG-AFTER-PARTIAL-EVICT`.
6. Observe the step-2 `receive` call's return. Report its result and elapsed time
   verbatim.

**Expected outcome:**
- Step 4: `receive_active: true` immediately after the `kckm` — confirms `10.10b`,
  not just by absence of an early return but by directly observed server state.
- Step 6: the call returns `MSG-AFTER-PARTIAL-EVICT` (`timeout: none`), well
  under its 25 s deadline — the wait kept running on the shared inbox and was
  correctly satisfied by the surviving room's traffic, neither aborted by the
  partial eviction nor blackholed.

| | Result |
|---|---|
| **Target** | Step 4 shows the wait still live right after the partial eviction; step 6 delivers the surviving room's message well inside the timeout, confirming the wait was never preempted. |

---

## Cleanup

`make dev-down` stops the instance and wipes `/tmp/toaster-talk-dev/` and the dev
token dir. Always run it — a lingering dev instance on :6668 is harmless but untidy.
Delete stray probe seats mid-run with `$DEV delmb devroom <member>`.
