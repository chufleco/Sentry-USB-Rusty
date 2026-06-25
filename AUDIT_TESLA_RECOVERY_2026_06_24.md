# Tesla VCSEC Recovery Audit — 2026-06-24

Author: auditor pass after the v3.11.17 ForceReconnect cascade on 4C+ bench session.
Read-only audit. No code edits proposed in this document — only diagnosis +
state-machine proposal.

## 0. Scope and method

- Failure under audit: 29-minute healthy session on Rock 4C+ (BCM4345C0)
  collapsed into Tesla VCSEC refuse-cooldown after v3.11.17's `ForceReconnect`
  fired on a 2/3 keep-awake nudge failure. Only a full car wake event cleared it
  ~1 hour later.
- Goal: identify what makes our recovery path escalate a transient wobble into
  a refuse-cooldown loop, contrast against Tesla's reference SDK
  (`teslamotors/vehicle-command`) and one mature community implementation
  (`wimaha/TeslaBleHttpProxy`), and propose the recovery state machine that
  gracefully reattaches without flooding the car.
- Source files audited:
  - `crates/tesla_ble/src/manager.rs` (2001 lines, primary)
  - `crates/tesla_ble/src/auth.rs`, `session.rs`, `transport.rs`, `gatt.rs`,
    `scan.rs`
  - `crates/tesla_telemetry/src/main.rs` (keep-awake nudge dispatch ~L1284-1359)
  - `crates/tesla_telemetry/src/sample_ble.rs`
- Reference sources audited (downloaded from `main` 2026-06-24):
  - `pkg/connector/ble/ble.go` (360 LoC)
  - `internal/dispatcher/dispatcher.go` (536), `session.go` (136)
  - `pkg/protocol/error.go` (264), `pkg/protocol/protocol.md` (833)
  - `pkg/protocol/protobuf/universal_message.proto` (fault enum comments)
  - `pkg/vehicle/vehicle.go` (275)
  - Community: `wimaha/TeslaBleHttpProxy/internal/ble/control/control.go` (563)

---

## 1. Failure model — anatomy of the 29-minute cascade

### 1.1 Pre-failure shape (healthy steady state)

From journal sample around the disconnect line in
`/mutable/sentryusb-ble-disconnects.log`:

```
held=29m26s queries=11 last_ok=...s_ago drops_total=0
keep_awake_sent_total=7
```

`PersistentSession` was warm. 11 successful authenticated `state` queries
(2.6/min ≈ matches the active-mode scheduler), 7 successful CPC keep-awake
nudges at 60s cadence. `query_timeout_retries = 0` on entry.

### 1.2 The cascade, event by event

The trigger sequence (reconstructed from the user's report and the code paths
that produce each message; see file:line refs in §2):

| t (rel) | Event                                                  | Source code path                     |
|---------|--------------------------------------------------------|--------------------------------------|
| t0      | Keep-awake CPC nudge fires                             | `main.rs:1289` send_action           |
| t0+~1s  | `BLE write error: Not connected` (attempt 1/3)         | `gatt.rs:315` `BLE write` context    |
| t0+30s  | CPC retry 2/3, same error                              | `main.rs:1354` 30s spacing           |
| t0+30s  | **ForceReconnect fires** (`nudge_retry_count == 2`)    | `main.rs:1322-1338`                  |
|         |   → `handle_force_reconnect` returns `Closed`          | `manager.rs:1817` `conn.take`        |
|         |   → `state.domains.clear()` — both VCSEC + Infotainment| `manager.rs:1820`                    |
|         |   → `lifetime_drops++`, `last_force_reconnect_at` set  | `manager.rs:1774, 1824`              |
| t0+60s  | Next nudge tick (interval after 3/3 reached & reset)   | `main.rs:1352`                       |
|         |   → `ensure_connected` runs: 30s scan → connect (8s)   | `manager.rs:1485-1561`               |
|         |   → connect succeeds, **fresh BLE link to the car**    | `gatt.rs:84`                         |
|         |   → `ensure_domain_session(VehicleSecurity)` — TX SIR  | `manager.rs:1666` + `session.rs:79`  |
|         |   → **Tesla returns ATT 0x0e on the SessionInfo READ** | (surfaced as anyhow error chain)     |
|         |   → mapped to `SessionError::Other(e)` (NOT KeyNotPaired)| `session.rs:1464`                  |
|         |   → bubbles up as `signed_request_with_refresh_retry`  | `manager.rs:867`                     |
|         |       → `is_transport_error` matches "BLE write" / "Peripheral" → **TEARDOWN**| `manager.rs:1838-1844` |
|         |   → `handle_transport_error_if_any`: another drop      | `manager.rs:1293`                    |
| t0+90s  | Next tick → `ensure_connected` again                   |                                      |
|         |   → connect succeeds again (BLE link is fine, Tesla    |                                      |
|         |       advertising)                                      |                                      |
|         |   → SessionInfoRequest TX → 10s timeout                | `session.rs:80` `Duration::from_secs(10)`|
|         |   → `session-info handshake … deadline has elapsed`    | `manager.rs:1681`                    |
| ...     | Loop continues. Each iteration: scan(up to 30s) +      |                                      |
|         | connect(up to 8s) + handshake(10s timeout) +           |                                      |
|         | backoff(1.5s → 3s → 6s → ... → 30s).                   | `manager.rs:1502` doubling           |
|         | Effective rate: roughly one full handshake attempt     |                                      |
|         | every 30-50s, sustained for ~1 hour, until user        |                                      |
|         | walked to the car (door open = wake event).            |                                      |

### 1.3 Tipping point identification

The link wobble at t0 was almost certainly recoverable — `BLE write: Not
connected` on a held session is the same class of event we have already proven
absorbable with `query_timeout_retries` for response timeouts
(`manager.rs:1199-1221`). The tip into refuse-cooldown happened at **t0+30s
when ForceReconnect fired AND cleared `state.domains`**. From that moment on,
every subsequent attempt was sending Tesla a fresh `SessionInfoRequest` over a
freshly-opened GATT link, on a car that was either:

1. **Mid-rotation on its own end** — Tesla had just rotated/expired the
   pre-existing session epoch (most likely; explains the ATT 0x0e on the very
   first attempt-2 read), or
2. **Defensively backing off** from our flurry of fresh L2CAP connects after
   the nudge wobble.

Once we hit refuse-cooldown, our reconnect loop is the worst possible behavior
for clearing it: every 30-50s we scan-and-connect, which on Tesla's side
counts as another "new BLE peer trying to negotiate a session" — extending the
cooldown clock.

### 1.4 What an alternate timeline looks like

Had ForceReconnect NOT cleared `state.domains` at t0+30s, the next nudge at
t0+60s would have taken one of:

- **a)** Succeeded on the held session (the link self-healed on the chip side).
- **b)** Tripped `query_timeout_retries` on the existing connection (no
  fresh-scan, no fresh-connect, no fresh `SessionInfoRequest` to Tesla).
- **c)** Tripped a single transport teardown via `handle_transport_error_if_any`
  — one scan/connect/handshake instead of N over an hour.

The single most damaging thing ForceReconnect did was **wipe the cached domain
sessions before knowing whether the car was actually unreachable** — and then
the existing transport-error path doubled down by tearing the link again on the
ATT 0x0e bubble-up.

---

## 2. What our code does today, by error class

References are `file:line` against working-tree HEAD.

### 2.1 Classification surface

Three predicates live in `manager.rs`:

- `is_transport_error(e)` — `manager.rs:1838-1844`. Matches substrings:
  `"notification stream ended"`, `"BLE write"`, `"not connected"`,
  `"Peripheral"`. On match → drop GATT, clear all domains, reconnect.
- `is_query_response_timeout(e)` — `manager.rs:1866-1868`. Matches
  `"waiting for response"` (only). Smart-retries up to
  `MAX_QUERY_TIMEOUT_RETRIES = 2` (`manager.rs:321`) before falling through to
  teardown.
- `is_decrypt_error(e)` — `manager.rs:1876-1878`. Matches
  `"AES-GCM response decrypt"`. Drops the cached domain session only; link
  stays up.

There is **no** predicate for the broader class "the car authenticated us
incorrectly" / "ATT auth failure" / "0x0e" / "Insufficient Authentication". The
session-handshake layer (`session.rs`) maps such errors to
`SessionError::Other(anyhow::Error)` (`session.rs:1464` — actually
`session.rs:43-44`), which becomes a plain anyhow chain by the time it reaches
`handle_transport_error_if_any`.

### 2.2 Per-error class behavior

| Error class                                  | Predicate match           | Behavior                                                            | File:line                              |
|----------------------------------------------|---------------------------|---------------------------------------------------------------------|----------------------------------------|
| `BLE write: Not connected` (held session)    | `is_transport_error`      | Drop conn, `domains.clear()`, reconnect via `ensure_connected`      | `manager.rs:1223-1304`                 |
| `notification stream ended`                  | `is_transport_error`      | Same as above                                                       | `manager.rs:1840`                      |
| `waiting for response: deadline has elapsed` (1st/2nd) | `is_query_response_timeout` AND gated on `queries_since_connect ≥ 1` AND retries-left | Swallow, keep link, retry on next tick           | `manager.rs:1199-1221`                 |
| `waiting for response: deadline has elapsed` (3rd+)    | falls through              | Drop conn, `domains.clear()`, reconnect                              | `manager.rs:1223`                      |
| `BLE connect: deadline has elapsed`          | `is_transport_error` NOPE; `is_query_response_timeout` NOPE | Falls through both — handled upstream in `ensure_connected` with `ConnectFailure` context + backoff | `manager.rs:1531-1538`                 |
| ATT 0x0e (Insufficient Authentication) on session-info READ | matches `"BLE write"` substring? NO — actual btleplug error renders differently; matches `"Peripheral"` if surfaced via btleplug's typed error | Drop conn, `domains.clear()`, reconnect — **WRONG: should re-handshake on the held link** | `session.rs:79-87` → `manager.rs:1681` → `manager.rs:1223` |
| `session-info handshake … deadline has elapsed` | `is_query_response_timeout`? — `session.rs:84` adds context `"session-info round-trip"`; the underlying `round_trip` adds `"waiting for response"`. So **YES**, this matches `is_query_response_timeout`. | BUT: gate `queries_since_connect >= 1` FAILS (no prior successful query on this connection), so the smart-retry path is skipped. Falls through to teardown. | `manager.rs:1199-1200`                 |
| Car responds with `OPERATIONSTATUS_WAIT`     | n/a (not an error)        | One retry after 400ms, then bail to caller                          | `manager.rs:784-799`                   |
| Car responds with `OPERATIONSTATUS_ERROR`    | bails directly            | Surfaced; transport handler may or may not teardown depending on chain   | `manager.rs:1066`                      |
| Recoverable fault (5/6/15/17)                | inside `try_signed_request_once` | Drop cached domain session, bail; next query re-handshakes domain | `manager.rs:1033-1046`                 |
| `KEY_NOT_ON_WHITELIST` SessionInfo refresh   | special-cased             | Bail with descriptive error; link stays up                          | `manager.rs:1007-1018`                 |
| `KEY_NOT_ON_WHITELIST` SessionInfo handshake | mapped to `SessionError::KeyNotPaired` | `ensure_domain_session` bails without dropping conn               | `manager.rs:1669-1679`                 |
| SessionInfo refresh that doesn't converge     | retry budget exhausted    | Drop cached domain session, do **one** clean re-handshake retry, then bail | `manager.rs:754-776`                   |
| Decrypt failure (`aead::Error`)              | `is_decrypt_error`        | Drop cached domain session, one re-handshake retry                  | `manager.rs:806-815`                   |
| Counter == `u32::MAX`                        | counter-rollover guard    | Drop cached domain session, bail (next query re-handshakes)         | `manager.rs:876-888`                   |
| `ForceReconnect` (caller-driven)             | manual                    | Drop conn, `domains.clear()`, reconnect; cooldown-gated 90s         | `manager.rs:1742-1827`                 |

### 2.3 Critical gaps in the predicates

**Gap A** (`is_transport_error` is a substring grep that over-matches): the
literal substring `"Peripheral"` matches:
- a genuinely dropped link (good),
- a btleplug rendering of an ATT-level error on a healthy link (BAD — should
  re-handshake, not tear down),
- a btleplug rendering of pretty much any operation on a Peripheral object.

`gatt.rs:315` wraps writes with `.context("BLE write")`, so even an ATT 0x0e
returned to a write (rare; usually returned to reads) would render as `"BLE
write: …"` and match. This means the predicate cannot distinguish "link is
gone" from "link is fine but the car rejected us at L2CAP/ATT layer".

**Gap B** (`ensure_domain_session` errors are not distinguished from query
errors): `session.rs:43-44` lumps everything that isn't `KeyNotPaired` into
`SessionError::Other(anyhow::Error)`. By the time `handle_transport_error_if_any`
sees it, the original cause (handshake-time read failure, handshake-time write
failure, handshake-time decode failure, handshake-time auth failure) is gone.

**Gap C** (the handshake-timeout 10s and the query-timeout 15s have different
gates): the smart-retry path in `manager.rs:1199-1221` requires
`queries_since_connect >= 1`. A handshake-timeout on a fresh conn that never
ran a query falls straight through to teardown. So when Tesla is slow to
respond to SessionInfoRequest specifically — which is the symptom of car-side
contention / cooldown — we don't absorb, we tear down and retry, accelerating
the cooldown.

**Gap D** (no preflight on `Connectable` advertising flag): see §3 — Tesla's
reference explicitly checks the advertising packet's Connectable bit before
dialing, returning a non-retryable `ErrMaxConnectionsExceeded` if false. Our
`scan.rs:111-118` only checks `local_name`. We attempt L2CAP connect against
a non-connectable advertising car, which on bluez ends in the 8s
`CONNECT_TIMEOUT` (`gatt.rs:22`) and counts as a connect-layer failure with
backoff doubling.

**Gap E** (no Tesla-side cooldown detection at all): nothing in our code looks
for the signature of refuse-cooldown (connects timing out + ATT auth errors on
fresh handshakes + zero successful handshakes for N consecutive attempts).
Backoff caps at 30s (`RECONNECT_BACKOFF_MAX`) which is way too aggressive once
we're in cooldown.

---

## 3. What the Tesla reference does

References to `teslamotors/vehicle-command` `main` HEAD as of 2026-06-24.

### 3.1 Reference architecture (different shape from ours)

Critical: Tesla's reference is **per-call connect-and-tear-down**, not
persistent session. `wimaha/TeslaBleHttpProxy` is the same pattern (see
§3.5). They both open a fresh BLE conn for each command/batch, do the
handshake(s), execute, then `car.Disconnect()` + `conn.Close()`. This is
worth stating upfront because some of the reference behavior we'd want to
borrow is structured around that assumption.

That said, the **per-error-class semantics** are still directly applicable to
our persistent-session model.

### 3.2 Connection layer — `pkg/connector/ble/ble.go`

- **Connectable check** (`ble.go:302-304`):
  ```go
  if !target.Connectable {
      return nil, false, ErrMaxConnectionsExceeded
  }
  ```
  Hard non-retryable error. They never even dial if the car's advertising
  packet says non-connectable.
- **Retry classifier** (`ble.go:259-278` `NewConnectionFromScanResult`):
  caller-driven loop, with `retry bool` returned alongside `err`. They retry
  forever (`for {}`) until `ctx` expires or `retry=false`. The classifier
  inside `tryToConnect`:
  - `ErrMaxConnectionsExceeded` → non-retryable (`Connectable=false`)
  - `LocalName mismatch` → non-retryable
  - Adapter init failure → non-retryable
  - Everything else (scan fail, dial fail, service-discovery fail,
    char-discovery fail, subscribe fail, MTU exchange fail) → retryable
- **No backoff between attempts**. The Connector's `RetryInterval()` is
  consumed by the dispatcher (`dispatcher.go:457`), 1 second
  (`ble.go:60`). They explicitly do NOT back off on connect-layer failures —
  the assumption is the user-supplied `ctx` bounds the patience.
- **Close on failure** (`ble.go:91-92`): `ClearSubscriptions()` then
  `CancelConnection()`. No domain-state clearing because there's no held
  session.

### 3.3 Dispatcher layer — `internal/dispatcher/dispatcher.go` + `session.go`

This is the closest analog to our `PersistentSession`.

- **Session is per-domain, per-(VIN, dispatcher instance)** (`session.go:26-46`).
- **`processHello` updates session in place** (`session.go:117-136`):
  ```go
  if s.ctx == nil {
      s.ctx, err = authentication.NewAuthenticatedSigner(...)
  } else {
      err = s.ctx.UpdateSignedSessionInfo(challenge, info, tag)
  }
  ```
  When the car sends fresh `session_info` (either as a refresh or as part of
  an error reply), they UPDATE the existing session object rather than
  destroying and recreating it. They never call `delete(d.sessions, domain)`
  on any error path I can find.
- **`checkForSessionUpdate`** (`dispatcher.go:182-224`): every inbound message
  is checked for an attached `session_info` field. If present and HMAC-valid
  AND received within `maxLatency` (default 4s; `ble.go:29`,
  `dispatcher.go:194`), the cached session is updated **before** the reply is
  passed to the response handler. The reply is then handled normally. This
  means the typical flow on a stale-session error is:
  - We send command with stale counter.
  - Car replies with `INVALID_TOKEN_OR_COUNTER` fault + attached fresh
    `SessionInfo`.
  - Dispatcher updates session in place, passes the error to the receiver.
  - `Vehicle.Send` (vehicle.go:236-257) sees `Temporary() = true` (per the
    fault enum classification in `error.go:200-211`), waits
    `RetryInterval()` (1s), and re-sends — now with the fresh session.
- **No teardown on auth errors at all.** `dispatcher.go` has zero
  `conn.Close()` calls. The connection lives until `Stop()` is called
  externally, and even `Stop()` doesn't close the underlying connector.
- **Retryable faults** (`error.go:200-211`): a fixed enumeration —
  `BUSY, TIMEOUT, INVALID_SIGNATURE, INVALID_TOKEN_OR_COUNTER, INTERNAL,
  INCORRECT_EPOCH, TIME_EXPIRED, TIME_TO_LIVE_TOO_LONG`. Marked
  `Temporary=true`. Caller's `vehicle.Send` retries these.
- **`ShouldRetry` policy** (`error.go:123-136`):
  - If `MayHaveSucceeded()` → no retry (don't risk double-execution).
  - If `Temporary()` → retry.
  - Else → no retry.

### 3.4 Vehicle layer — `pkg/vehicle/vehicle.go`

- **`Vehicle.Send`** (`vehicle.go:236-257`): the application-level loop. For
  each `trySend`, classify the error via `ShouldRetry`. If retryable,
  sleep `RetryInterval` (1s on BLE) and re-send the same payload. If not
  retryable, surface to caller. **No exponential backoff**, no special
  handling for "Tesla seems unhappy with us specifically".
- **`Vehicle.Disconnect`** (`vehicle.go:174-179`): only when explicitly
  requested. Never on its own initiative.

### 3.5 Community implementation — `wimaha/TeslaBleHttpProxy`

`control.go` is the dispatcher loop. Worth noting:

- **Per-command connect-and-disconnect** (`control.go:171-181`). 15-second
  context for the connect+handshake phase.
- **Connect retry budget = 3 with exponential backoff starting at 3s, doubling**
  (`control.go:130, 159-169`). After 3 attempts → caller sees an error.
  Crucially, the backoff sleeps respect the parent context (which has its
  own timeout per command).
- **Scan timeout is configurable**, defaulting to the user's config
  (`control.go:220-243`). Tesla beacons every ~200ms so they consider scan
  timeout the dominant signal: "if it's not in range by now, retrying
  immediately won't help".
- **Sleep-status cache** (`control.go:104-117`): if they confirmed the vehicle
  awake within the last 9 minutes, they SKIP the `BodyControllerState` probe
  for the next command. This is purely a latency optimization but the
  underlying assumption — "9 minutes is the safe re-use window post-confirmed-
  awake" — is informative: they're matching the post-2026.14 ~12-minute
  online window with a safety margin.
- **`ExecuteCommand`** (`control.go:528-559`): same 3-attempt retry with
  3s→6s→12s backoff. On error containing `"closed pipe"` they return the
  command for upper-layer reattempt (i.e., reconnect at the higher level).
  Otherwise they just retry on the existing connection.

### 3.6 Protocol doc — `pkg/protocol/protocol.md`

The "Caching session state" + "Recovering from synchronization errors"
sections (`protocol.md:690-722`) are very explicit:

> The vehicle may include up-to-date session state in an error message in
> cases where an authentication error could be attributed to a
> synchronization fault. For example, if the infotainment system reboots,
> then the vehicle and the client may not be using the same epoch.
>
> The client MUST discard the session information if any of the following
> are true:
>
> - The client did not use the request UUID in the last several seconds.
> - The session info HMAC tag is incorrect.
> - The clock time is earlier than the clock time in a previously
>   authenticated session info message with the same epoch.
>
> The client MUST update its session state if none of the above are true.
> When updating its session state, the client MUST NOT rollback its
> anti-replay counter unless the epoch changes.

And the recovery mechanism is explicitly characterized as **no more
expensive than performing the handshake in the first place** — they DO
expect clients to re-handshake on synchronization faults, but they expect
the re-handshake to happen on the held connection, not after a teardown.

### 3.7 What the reference does NOT document

No documented behavior for:
- ATT-level errors (0x0e Insufficient Authentication, 0x0f Insufficient
  Encryption, 0x05 Authentication Failure). These are GATT-layer, below the
  Tesla protocol. Tesla's BLE doc treats the GATT link as a dumb pipe.
- Refuse-cooldown. The reference assumes the car answers; there's no
  documented "back off because we're being rate-limited" semantic.
- BLE slot-contention recovery. They check the Connectable bit upfront and
  bail. They don't describe what to do after multiple consecutive
  Connectable=false advertisements.

**This means our refuse-cooldown observation is in the empirical-but-undocumented
bucket. Without controlled experiments we can't say exactly which Tesla-side
counter we're tripping or how it decays.**

---

## 4. The gap — side-by-side, ranked by suspected impact

| # | Error class                                | What we do                                          | What ref does                                                                                              | Impact on cooldown |
|---|--------------------------------------------|-----------------------------------------------------|------------------------------------------------------------------------------------------------------------|--------------------|
| 1 | "Auth-like" error on held session (ATT 0x0e, INVALID_SIG, etc.) post-rotation | Match `is_transport_error` substring → **tear down GATT + clear all domains** | UPDATE session in place from attached `session_info`, or re-handshake on held link; never tear down        | **HIGH** — directly trips cooldown via fresh L2CAP after teardown                       |
| 2 | Keep-awake nudge fail 2/3                  | **Force-teardown** via `ForceReconnect` clearing both domains and link | n/a (no equivalent watchdog)                                                                               | **HIGH** — the actual lighter we lit                                                                       |
| 3 | SessionInfo handshake timeout (fresh conn) | Tear down, retry from scratch every 30-50s         | Sleep `RetryInterval` (1s), retry on existing dispatcher (which holds the underlying conn open)            | **HIGH** — once cooldown starts our loop sustains it                                                       |
| 4 | Non-connectable advertising                | Try to dial anyway, eat 8s CONNECT_TIMEOUT, double backoff | Refuse to dial (`Connectable=false` → ErrMaxConnectionsExceeded, non-retryable) | **MEDIUM** — we waste connect slots when slot is held; doesn't directly cause cooldown but accelerates it  |
| 5 | Stale-session fault with attached SessionInfo refresh | Apply refresh, retry once (`signed_request_with_refresh_retry`) | Apply refresh in `processHello` in-place, then retry — same shape, slightly cleaner | **LOW** — our path is roughly correct                                                                      |
| 6 | Backoff after consecutive failures         | Doubles 1.5s → 30s and stays at 30s forever        | None (1s flat); upper layer's context bounds it                                                            | **HIGH** — 30s is way too aggressive once Tesla is refusing                                                |
| 7 | OPERATIONSTATUS_WAIT                       | 400ms then retry; one budget                       | Treated as `ErrBusy`, retry via `Vehicle.Send` loop at `RetryInterval` until ctx expires                  | **LOW** — both are reasonable                                                                              |
| 8 | KEY_NOT_ON_WHITELIST                       | Bail with helpful message, link stays up           | Same (`ErrKeyNotPaired` from `protocol.GetError`)                                                          | **NONE**                                                                                                   |

### 4.1 The single most damaging gap

**Gap #1.** Our `is_transport_error` predicate doesn't distinguish "link
gone" from "car rejected us at the auth layer". When ATT 0x0e fires on a
session-info READ after Tesla rotated the session on its side, we tear down
the BLE link AND clear `state.domains`. The next attempt is a full
scan-and-dial, which Tesla sees as a fresh BLE peer trying to negotiate from
zero. That's exactly what the refuse-cooldown classifier (whatever it
actually is on Tesla's side) appears to penalize.

The reference implementation literally cannot do this — there's no `Close()`
call on any error path in `dispatcher.go`. They keep the link, re-handshake
in-place, and let the user-supplied context bound patience.

---

## 5. The recovery taxonomy — proposed state machine

Goal of every branch: **do the lightest-weight thing that could plausibly
fix the symptom, never bigger; treat BLE teardown as a near-irreversible
nuclear option.**

### 5.1 Classification (proposed; replaces the substring grep)

Replace `is_transport_error(e)` with a richer classifier that distinguishes
five sources:

| Class                    | Definition                                                                      | Predicate sketch                                                                              |
|--------------------------|---------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------|
| `LinkDropped`            | The chip-level BLE link is confirmed gone                                       | `"notification stream ended"` or `peripheral.is_connected().await == Ok(false)` (re-probe)    |
| `WriteSyscallFailed`     | A write call failed — but link MAY still be up                                  | `"BLE write"` context AND `is_connected==true` re-probe                                       |
| `ResponseTimeout`        | We sent, waited, got nothing                                                    | `"waiting for response"` (current `is_query_response_timeout`)                                |
| `AttAuthRejected`        | Car returned ATT auth error (0x0e/0x0f/0x05)                                    | Need to thread btleplug typed errors up through `gatt.rs`; today they collapse to anyhow      |
| `ProtocolFault`          | Car returned `signed_message_fault` ≠ NONE                                      | Already explicit in `try_signed_request_once:968-1048`                                        |

The reason `LinkDropped` vs `WriteSyscallFailed` matters: btleplug's
"not connected" can fire for transient bluez state (a chunk write losing a
race with a notification on the same characteristic) on a link the chip
still considers up. Today we conflate both with `is_transport_error` and
tear down.

### 5.2 Per-class actions

| Class | Action | Why | Numeric proposals |
|-------|--------|-----|-------------------|
| `LinkDropped` (confirmed) | Close conn, clear domains, reconnect with backoff `[5s, 15s, 45s, 90s, 180s]` (caps at 180s, NOT 30s); reset on any successful handshake. | Real teardown — but pace it. Tesla's `~12m01s online / 5m14s offline` parked-sleep cadence means 180s gives Tesla a meaningful window to come back. | min=5s, max=180s, multiplier=3 |
| `WriteSyscallFailed` (link still up per re-probe) | Drop the failing query (let scheduler retry on next tick), do NOT close conn, do NOT clear domains. Track a `consecutive_write_fail_count`; only escalate to `LinkDropped` after 3 in a row OR after the next `keepalive` tick (8s) confirms the link broken. | Today's behavior nukes the session on the first wobble. Tesla cares about the BLE link, not about our single failed write. | escalate=3 fails, or after one failed keepalive |
| `ResponseTimeout` (held session, `queries_since_connect ≥ 1`) | Current smart-retry up to N=2 retries. **KEEP THIS.** It's already correct and proven. | Tesla's reference keeps the dispatcher across timeouts; ours matches. | N=2 (unchanged), retry on next scheduler tick |
| `ResponseTimeout` on session-info handshake (no prior successful query) | Do NOT tear down. Apply backoff inline: 5s → 15s → 45s → 90s → 180s, retry handshake on the SAME held link until: (a) handshake succeeds, (b) `LinkDropped` re-probe fires, or (c) caller cancels via context. | This is the exact symptom of Tesla being in refuse-cooldown. Tearing down + reconnecting makes it WORSE; staying on the held link gives Tesla a stable peer to come back to. | min=5s, max=180s, multiplier=3 |
| `AttAuthRejected` (0x0e/0x05/0x0f on any GATT op) | Do NOT tear down. Drop the affected domain from `state.domains`. Next op re-runs `ensure_domain_session` on the SAME held link — Tesla gets a fresh SessionInfoRequest on the existing L2CAP, which is exactly the doc's "synchronization fault recovery" pattern. | Tesla auth errors on a held link mean "your session keys are stale" not "your link is bad". Match the reference's `processHello` semantics. | Drop domain only, retry once; if second 0x0e in a row → escalate to `LinkDropped` (treat the link as compromised) |
| `ProtocolFault` (5, 6, 15, 17) with attached SessionInfo | Current path: apply refresh, retry once. **KEEP** but add: also accept attached SessionInfo on non-fault replies (the reference's `checkForSessionUpdate`). Right now we only look for SessionInfo refresh on a SessionRefresh `payload` (`manager.rs:986-1024`). | The reference treats attached SessionInfo as an in-band session-update mechanism, not just a fault-response mechanism. We may be missing free refreshes the car volunteers. | n/a (logic addition) |
| `ProtocolFault` (other) | Current path: bail to caller. **KEEP.** | These are application-level. | n/a |
| **N consecutive `LinkDropped` + handshake failures across reconnects** (`>= 5` in 10 minutes) | Enter explicit `RefuseCooldown` state: stop scanning, hold for `cooldown_dur = 10min` minimum, then probe ONCE every 5min after. Surface this state via the status file so the API + UI can render "Vehicle appears to be in BLE cooldown — will retry every 5 minutes". | Stops the radio thrashing. Aligns the probe cadence with Tesla's offline window. Makes the state observable. | 5 fails in 600s → enter cooldown; cooldown_dur=600s; probe every 300s |
| **Keep-awake nudge fail 2/3** (current ForceReconnect trigger) | **REMOVE the ForceReconnect call entirely.** Let the existing per-tick retry + `handle_transport_error_if_any` chain handle it. The 3rd failure already triggers the user notification at `main.rs:1340-1347`. The 2nd failure should do NOTHING extra — give the link one more chance on its own. | The whole reason we're writing this audit. | Remove `main.rs:1322-1338` |

### 5.3 Connectable-bit preflight (Gap D)

Replace the local-name-only filter in `scan.rs:111-118` with a Connectable+name
filter. btleplug exposes the advertising flag via `Peripheral::properties()`'s
`PeripheralProperties::tx_power_level` neighborhood (specifically the
`connectable` field on the advertisement events for some platforms; on Linux
via `org.bluez.Device1.Connectable` D-Bus property — readable post-discovery).

When non-connectable seen for the matching VIN:
- Don't dial.
- Keep scanning for up to a smaller per-attempt budget (5s) waiting for a
  Connectable=true advertisement (slot freed by another client).
- If still non-connectable after that budget → return a distinct
  `SlotContention` error (NOT `ConnectFailure`) — caller backs off
  differently (longer; phone-key cycles are typically 30-120s).

### 5.4 Where the backoff numbers come from

Anchored on three known durations:
- **Tesla parked-sleep cadence**: `12m01s online / 5m14s offline`
  (memory `project_tesla_parked_sleep_scheduler.md`, 2026-06-23
  TeslaMate-confirmed). Maximum useful retry interval is the offline window
  (5m14s = 314s) — anything longer skips a wake cycle. Anything shorter is
  speculative work during the asleep half.
- **wimaha's connect retry**: 3s → 6s → 12s (`control.go:130, 168`) for 3
  attempts, total ~21s. They give up after that and surface to the caller.
  Our budget can be longer since we're a background daemon (not a sync HTTP
  proxy), but the shape should be the same — fast first, expanding.
- **Tesla reference**: 1s flat (`ble.go:60`), bounded by user `ctx`.

Proposed backoff for `LinkDropped` reconnect: `5s, 15s, 45s, 90s, 180s` then
stay at 180s. That's:
- Fast enough to catch a 12-min online window (5s first attempt; 4-5 attempts
  in the first 5 minutes).
- Slow enough not to flood Tesla within a single online window.
- Capped under the offline-window length so we don't go silent for too long.

Proposed backoff for `RefuseCooldown`: hold 600s, then probe every 300s. The
600s hold gives Tesla ~1 full sleep cycle (`5m14s`) plus margin to clear
whatever counter we tripped. 300s probe matches the offline window — we'll
catch the moment the car wakes up.

---

## 6. What to ship vs. what to research more

### 6.1 Ship now (small diffs, high confidence)

**S1. Remove ForceReconnect call from the keep-awake nudge path.**
File: `crates/tesla_telemetry/src/main.rs` lines 1322-1338. The cooldown gate
inside `handle_force_reconnect` was a band-aid; the real fix is to not call it
at all in this hot path. Keep the `ForceReconnect` API itself (it's
plumbed and tested) so an explicit operator-driven recovery still exists, just
don't auto-invoke it on a nudge fail.

**S2. Hard-distinguish handshake-time errors from query-time errors in the
classifier.** Add a `is_handshake_failure(e: &anyhow::Error) -> bool` that
matches the `"session-info round-trip"` context (`session.rs:84`) or
`"session-info handshake"` (`manager.rs:1682`). Route these to a separate
branch in `handle_transport_error_if_any` that **does NOT tear down** —
instead drops the domain from `state.domains` and lets the next op
re-handshake on the held link. This is one new predicate + one new branch.

**S3. Raise `RECONNECT_BACKOFF_MAX` to 180s** (`manager.rs:60`). Pure constant
change. Doesn't affect successful sessions (backoff resets on connect
success).

**S4. Apply attached SessionInfo from any reply, not just fault refreshes.**
Mirror the reference's `checkForSessionUpdate` — when a normal `Plaintext`
response also carries a fresh `SessionInfo` field, apply it. Today we only
apply on the `SessionRefresh` branch (`manager.rs:986-1024`). This is a
~10-line addition inside `try_signed_request_once` before the existing
SessionInfo branch.

**S5. Tighten `is_transport_error` to NOT match the bare substring
`"Peripheral"`.** This is too broad; replace with explicit btleplug error
shapes. Today an ATT-layer error rendered via btleplug's typed errors can
match `"Peripheral"` and trigger teardown. Drop that one substring; the
remaining `"BLE write"`, `"not connected"`, `"notification stream ended"`
cover the actual link-dead cases.

### 6.2 Ship after one controlled experiment

**E1. Connectable-bit preflight.** Need to verify btleplug exposes
`Connectable` on Linux reliably (the `PeripheralProperties` field is
platform-dependent). Bench experiment: log Connectable on every scan match
for a week, correlate with phone-key proximity. If reliable, ship the
preflight + `SlotContention` distinct error class.

**E2. `AttAuthRejected` predicate.** Need to capture an actual ATT 0x0e from
btleplug's error chain and confirm what string it renders as. Until we have a
real capture (which v3.11.17's cascade should have produced — pull
`/mutable/journal-*` from the 4C+ for the actual error text), we can't write
a tight predicate. Without a tight predicate we shouldn't ship branch
behavior conditional on it.

**E3. `RefuseCooldown` mode.** Need to validate the "5 fails in 600s" trigger
empirically. Could be 3, could be 7. The state machine entry point is correct;
the threshold needs a soak. Ship the state and observability first
(write to status file), only enable the behavior change after we've
confirmed the threshold catches the failure mode without false-firing on a
flappy radio.

### 6.3 Research before designing

**R1. Does Tesla actually have a refuse-cooldown classifier, or is the
observed behavior just the car going asleep and staying asleep?** Critical
distinction — if it's the latter, the fix is just to wait for the next online
window (5m14s offline gives us the cadence). If it's a real classifier, we
need to understand what triggers it (fresh L2CAP rate? handshake failure
rate? both?). The data needed: a clean controlled session where we
deliberately fail handshakes at varying rates and time-to-recovery for each.

**R2. Does the held-link re-handshake actually work on Tesla as the protocol
doc claims?** The doc says "no more expensive than performing the handshake
in the first place" — but it's silent on whether re-handshaking over a
long-held GATT connection produces different behavior than a fresh-conn
handshake. Bench experiment: hold link for 10min, deliberately drop one
domain's cached session, re-handshake over held link; compare success
rate to fresh-conn handshake at same intervals.

---

## 7. What we should explicitly NOT do

### 7.1 NOT: add another retry layer

Every time we've added a retry budget (refresh_retries, wait_retries,
stale_retries, query_timeout_retries, ForceReconnect cooldown) it has been
correct in isolation and worse in aggregate. Reason: each retry is
indistinguishable from the last from Tesla's perspective. If 3 retries don't
work, 5 retries don't work either — Tesla is either ready or it's not.

The fix is never another retry layer. The fix is to make the EXISTING retries
operate on the held link (cheap) instead of after teardown (expensive).

### 7.2 NOT: more aggressive teardown

The intuition "if it's stuck, reset it harder" is exactly wrong for Tesla
VCSEC. Every teardown:
- Drops cached domain keys.
- Returns the L2CAP slot to bluez.
- Forces a fresh scan-and-dial on next attempt.
- Looks like a NEW peer from Tesla's perspective.

Tesla appears to count "fresh peers within a window". Teardowns increase
that count. The right intuition is the opposite: **a stuck link costs us
nothing as long as it's not blocking other ops, so leave it.**

### 7.3 NOT: shorter timeouts

`QUERY_TIMEOUT=15s`, `CONNECT_TIMEOUT=8s`, handshake timeout=10s. Each was
sized by trial against real Tesla latency. Shortening them just creates
more `ResponseTimeout` events, each of which (under current code) burns a
retry budget faster. The fix is the opposite: leave timeouts alone, just
don't escalate to teardown when they fire.

### 7.4 NOT: ping the car proactively to "see if it's awake"

Every probe = a new GATT op = either successful (great, we didn't need to
probe) or unsuccessful (we just contributed to the refuse-cooldown
counter). The existing 8s keepalive on the held link is already a probe;
adding a second one before the next nudge does nothing for us.

If we genuinely want to know whether the car is awake before sending an
authenticated nudge, the cheapest probe is **looking at the most recent
keep-accessory state observation** — that's car-side ground truth we already
have, no BLE op needed.

### 7.5 NOT: drop to per-call connect/disconnect

Sometimes raised as "well the reference does this and they don't have our
problem". The reference also doesn't have our use case: they're a Fleet API
mediator, not a continuous sampler. Their per-call model is great when calls
are infrequent (minutes between commands); it's terrible at our cadence (15s
between polls). Per-call would put us through 4× the L2CAP negotiations per
minute, which is the exact thing we're trying to avoid more of.

### 7.6 NOT: rate-limit ourselves "to be safe"

A blanket "no more than N ops per minute" rule sounds defensive but is
counterproductive: it slows down successful sessions for no benefit. The
discipline isn't "fewer ops" — it's "no teardowns". A held connection doing
4 successful ops/minute is fine. A connection that gets torn down and
re-established once is already a worse signal to Tesla than 100 successful
ops on the held link.

### 7.7 NOT: try to detect refuse-cooldown by parsing kernel dmesg

We already capture dmesg snippets for short-held drops
(`manager.rs:1267-1273`). Tempting to extend this into a refuse-cooldown
detector by pattern-matching HCI error codes. Don't — those codes are
chip-and-controller dependent (BCM vs AIC8800 produce different strings for
the same Tesla-side rejection). Detect cooldown in the protocol layer using
our own observable counters (handshake-fail rate, time since last successful
handshake), not in the kernel layer.

---

## 8. Appendix — quick reference to file:line citations

Ours:
- `is_transport_error`: `crates/tesla_ble/src/manager.rs:1838-1844`
- `is_query_response_timeout`: `crates/tesla_ble/src/manager.rs:1866-1868`
- `is_decrypt_error`: `crates/tesla_ble/src/manager.rs:1876-1878`
- `handle_transport_error_if_any`: `crates/tesla_ble/src/manager.rs:1171-1306`
- `handle_force_reconnect`: `crates/tesla_ble/src/manager.rs:1742-1827`
- `ensure_connected` + backoff: `crates/tesla_ble/src/manager.rs:1485-1561`
- Reconnect backoff constants: `crates/tesla_ble/src/manager.rs:59-60` (1.5s/30s)
- `MAX_QUERY_TIMEOUT_RETRIES = 2`: `crates/tesla_ble/src/manager.rs:321`
- `FORCE_RECONNECT_COOLDOWN = 90s`: `crates/tesla_ble/src/manager.rs:329`
- `ensure_domain_session` (re-handshake on held link): `crates/tesla_ble/src/manager.rs:1657-1709`
- `signed_request_with_refresh_retry` (refresh / WAIT / decrypt retry budget): `crates/tesla_ble/src/manager.rs:729-821`
- Recoverable-fault drop-domain branch: `crates/tesla_ble/src/manager.rs:1029-1046`
- SessionInfo refresh apply: `crates/tesla_ble/src/manager.rs:986-1024`, `manager.rs:837-859`
- KEY_NOT_ON_WHITELIST (handshake): `crates/tesla_ble/src/session.rs:170-178`
- KEY_NOT_ON_WHITELIST (refresh): `crates/tesla_ble/src/manager.rs:1007-1018`
- ATT MTU + `round_trip`: `crates/tesla_ble/src/gatt.rs:235-424`
- `Connection::open` (connect+discover+subscribe): `crates/tesla_ble/src/gatt.rs:76-172`
- Pre-write `is_connected` probe: `crates/tesla_ble/src/gatt.rs:287-303`
- Scan filter (local-name only, no Connectable check): `crates/tesla_ble/src/scan.rs:111-118`
- Keep-awake nudge fail-path + ForceReconnect trigger: `crates/tesla_telemetry/src/main.rs:1284-1359`

Reference (`teslamotors/vehicle-command` `main` HEAD 2026-06-24):
- Connectable check: `pkg/connector/ble/ble.go:302-304`
- Per-call connect retry shape: `pkg/connector/ble/ble.go:259-278`
- BLE `Close()`: `pkg/connector/ble/ble.go:89-92`
- `RetryInterval()` = 1s: `pkg/connector/ble/ble.go:60`
- `maxLatency` for session-info attach: `pkg/connector/ble/ble.go:29`
- `processHello` (update in place, never destroy): `internal/dispatcher/session.go:117-136`
- `checkForSessionUpdate` (free in-band refreshes): `internal/dispatcher/dispatcher.go:182-224`
- Retryable fault enumeration: `pkg/protocol/error.go:200-211`
- `ShouldRetry` policy: `pkg/protocol/error.go:123-136`
- `Vehicle.Send` retry loop (no exponential backoff): `pkg/vehicle/vehicle.go:236-257`
- "Recovering from synchronization errors" prose: `pkg/protocol/protocol.md:702-722`
- "Caching session state": `pkg/protocol/protocol.md:690-700`

Community (`wimaha/TeslaBleHttpProxy` `main`):
- Connect retry budget + 3s→6s→12s backoff: `internal/ble/control/control.go:130, 159-169`
- 9-minute sleep-status cache: `internal/ble/control/control.go:104-117`
- ExecuteCommand retry: `internal/ble/control/control.go:528-559`
