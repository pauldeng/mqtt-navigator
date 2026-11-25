# ADR 0006: MQTT client library: ntex-mqtt

Status: accepted (2026-11-14). Replaces the choice in [ADR 0001](0001-stack.md) and supersedes [ADR 0002](0002-patch-ms-mqtt-client.md). The plan and the audits are in [docs/plans/2026-11-14-ntex-mqtt-plan.md](../plans/2026-11-14-ntex-mqtt-plan.md) and [docs/plans/2026-11-12-rumqttc-next-plan.md](../plans/2026-11-12-rumqttc-next-plan.md). The results are in [docs/plans/phase-8-verification.md](../plans/phase-8-verification.md).

## Problem

The owner requires MQTT 3.1.1 (2026-11-14). `ms-mqtt-client` 0.1.0 supports MQTT 5 only. It also needed a local patch for packet size and queue limits (ADR 0002).

## Decision

Use `ntex-mqtt` 10. It has a v3 (MQTT 3.1.1) client and a v5 client. MIT OR Apache-2.0.

- **D1. Keep the boundary.** Only `navigator-core/src/net.rs` and its submodules, the test helpers and `examples/flood.rs` use ntex. The public API of `navigator_core::net` changes only where the plan allows it (the `protocol` field in the profile, subscription options, the `ConnectionDetails` event and request IDs).
- **D2. Keep OpenSSL.** `tls.rs` keeps the material, the verify callback and the diagnostics. It returns an `openssl::ssl::SslConnector`. `net.rs` wraps it with `ntex-tls`.
- **D3. Our supervisor owns reconnect.** One connector call and one client for each attempt. Backoff, retry rules, cancel, generations and the set of wanted subscriptions stay as they were. A session persists when `session_expiry_secs > 0` (MQTT 5) or when Clean Session is off (MQTT 3.1.1).
- **D4. No exact pin.** `ntex-mqtt = "10.0.2"` (a caret requirement; the owner decided on 2026-11-19 that it needs no pin). The other ntex crates (`ntex-tls` with `openssl`, `ntex-net` with `tokio`, `ntex` for WebSocket only) go through `Cargo.lock`. Dependabot groups them. Each major version goes in its own commit.
- **D5. Remove the vendor copy last.** `vendor/ms-mqtt-client` and the `[patch]` are gone after the last user was ported.
- **D6. One internal interface, two adapters.** A private `Wire` enum (`V3`, `V5`) has the same operations. Each adapter uses `start_with_control`, because the default control service drops the stop reason. The supervisor, the queues and the timers do not know the version.
- **D7. MQTT 3.1.1 is the default** for a new profile and for the command line. Existing profiles keep MQTT 5.
- **D8. Our TLS-diagnosis and error texts stay.** Each ntex error maps to our `Outcome` (temporary or final).

ntex runs on our tokio `current_thread` runtime inside `ntex_net::with_reactor` and a `LocalSet`. We set every client deadline ourselves, because ntex-mqtt has them off by default: connect, write, frame read rate and keep alive. Our own dialer does DNS and TCP under one deadline.

## Alternatives

| Option | Result |
|---|---|
| Stay on `ms-mqtt-client` | Rejected. MQTT 5 only. It needs a local patch (ADR 0002). It has no QoS 2 and its QoS 2 paths can panic. |
| `rumqttc-next` (`rumqttc-v4-next` and `rumqttc-v5-next`) | Rejected (2026-11-14). Two audits found liveness defects in 0.34.0 and in git main: a write stall that no deadline ends (D1), and outgoing sends that reset the keep-alive timer while a PINGRESP is owed (D2). Our layer would need a second client around it. |
| `ntex-mqtt` | Accepted with conditions. No liveness defect once the deadlines are set. Costs: a second I/O runtime stack, the `ntex` crate for WebSocket, one maintainer, a breaking major version about every 3 months. |

## Consequences

- MQTT 3.1.1 and MQTT 5 both work. MQTT 5 keeps every v0.1.x behaviour. Subscription options and Connection details are new for MQTT 5.
- QoS 2 stays out of scope. The `Qos` type has two values. A broker QoS 2 message ends the session with a plain text.
- A dead peer takes about 2 times the keep alive to find, plus timer and shutdown delays (v0.1.x: keep alive + 10 s). The plan bound is 2 × keep alive × 1.125 + 10 s (keep alive 60 s: at most 145 s). S1 measured 121.38 s at keep alive 60 s and 144.93 s at 70 s. This is accepted. If owners need it faster, a ping-response timeout can be proposed to ntex-mqtt (a public post, only with owner approval).
- ntex logs through the `log` crate. The app installs no `log` logger, so these lines do not reach the log file.
- Settings schema is 2. A profile without `protocol` loads as MQTT 5.
- Every ntex update needs the broker tests and the fake-broker tests.
- The release binary grows by 755 704 B (2.2%) against v0.1.1 (S4). The CI size gate passed at that check (2026-11-15). The owner removed the gate on 2026-11-20.
