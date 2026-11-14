# Plan: replace ms-mqtt-client with rumqttc-next

Status: **rejected** (2026-11-14). Two independent audits (Sonnet and Grok) of `rumqttc-next` found liveness defects (see the audit sections at the end). The owner requires MQTT 3.1.1, and `ntex-mqtt` passed the same audit with conditions. The new plan is [2026-11-14-ntex-mqtt-plan.md](2026-11-14-ntex-mqtt-plan.md). This file is kept as the audit record.

## Goal

Replace the vendored `ms-mqtt-client` 0.1.0 (and its local patch, ADR 0002) with the actively maintained `rumqttc-next` fork. Support two protocol versions (owner request 2026-11-12):

- **MQTT 3.1.1** through `rumqttc-v4-next`. This is the default for a new profile.
- **MQTT 5.0** through `rumqttc-v5-next`. It keeps every MQTT 5 behaviour of v0.1.x and adds the features that section "MQTT 5 features" accepts.

Release the result as v0.2.0.

## Not in scope

- QoS 2. The `Qos` enum keeps two values. QoS 2 is a separate owner decision after this change.
- A move from OpenSSL to rustls. TLS stays as it is (see D2).
- MQTT 5 features that section "MQTT 5 features" rejects.

## Facts (read from the source of `thehouseisonfire/rumqtt`, commit 98bca64, 2026-11-02)

| Fact | Effect |
|---|---|
| Crates `rumqttc-v4-next` (MQTT 3.1.1) and `rumqttc-v5-next` (MQTT 5) 0.34.0 on crates.io (2026-08-13). Both have the library name `rumqttc` and share `rumqttc-core-next`. Apache-2.0. MSRV 1.89. | Depend on both, renamed (`mqtt4 = { package = "rumqttc-v4-next" }`, `mqtt5 = { package = "rumqttc-v5-next" }`). Pin `=0.34.0`. Dependabot proposes updates; review each one. |
| The v4 crate has the same model: `set_socket_connector`, `Transport::Ws`, `publish_tracked`, `subscribe_tracked`, `unsubscribe_tracked`, `set_clean_session`, `set_last_will`, `set_credentials`. Its incoming packet limit defaults to **10 KiB** (`set_max_packet_size(incoming, outgoing)`). | One adapter for each version behind one internal interface (D6). Set the v4 incoming limit to 1 MiB explicitly, or most real payloads fail. |
| Default feature `use-rustls` (aws-lc). Features `websocket`, `use-native-tls`, `use-rustls-*`, `tracing`, `proxy`. | `default-features = false, features = ["websocket"]`. No TLS feature: our connector does TLS (D2). No aws-lc build. |
| `AsyncClient` + `EventLoop`. The event loop does I/O only while `poll()` runs. It reconnects on the next `poll()` after an error. | Same model as today. Our supervisor keeps reconnect and backoff: it drops the event loop after an error and builds a new one for the next attempt. It never calls `poll()` after an error. |
| `MqttOptions::set_socket_connector(Fn(host, NetworkOptions) -> stream)`. The connector makes the base stream. `Transport::Tcp` or `Transport::Ws` layers on top. | Our connector does TCP plus the OpenSSL handshake. Then `Transport::Tcp` gives MQTT over TLS, and `Transport::Ws` gives WSS. TLS diagnostics stay ours. |
| `set_max_packet_size(Some(n))` sets CONNECT Maximum Packet Size and the incoming limit. `mqttbytes` `check()` compares the fixed-header length with the limit before it reads the body. | Replaces our local patch and the process-global `local_limits`. Proven by a test in S1 (a fake broker sends a 2 MiB PUBLISH). |
| `*_tracked` operations (`publish`, `subscribe`, `unsubscribe`) return a notice that resolves to `Qos0Flushed`, `Qos1(PubAck)`, SUBACK or UNSUBACK reasons, or `SessionReset`. | Maps directly to our `PublishResult` and the subscription status. Our 10 s timeout stays. |
| `set_request_channel_capacity`, `set_max_request_batch`, `set_read_batch_size`, `set_receive_maximum`, `set_topic_alias_max`, `set_session_expiry_interval`, `set_clean_start`, `set_keep_alive`, `set_connect_timeout`, `set_last_will`, `set_credentials`. | Every CONNECT field of a profile has a setter. Every queue has a bound. |
| The repository has many `TODO*.md` files and one main author (862 commits). | Bus-factor risk. Mitigation: exact pin, our own broker tests on every update, `ntex-mqtt` as the fallback (recorded in the ADR). |

## MQTT 5 features

The question for each feature: does it help a person who subscribes, inspects and publishes? Cost is the UI and test work. "Have" means v0.1.x already has it; it stays for MQTT 5 profiles.

| Feature | Status | Value for this app | Decision |
|---|---|---|---|
| Reason codes and reason strings (CONNACK, SUBACK, UNSUBACK, PUBACK, DISCONNECT) | Have | High: the user sees why the broker refused | Keep |
| Session expiry interval, clean start, session present | Have | High: persistent sessions | Keep |
| PUBLISH properties both ways: content type, response topic, correlation data, message expiry, payload format, user properties | Have | High: the main reason to inspect MQTT 5 traffic | Keep |
| Request/response (Reply with response topic and correlation data) | Have | Medium | Keep |
| Will properties | Have (delay) | Low | Keep as it is |
| Maximum packet size (CONNECT), receive maximum from the broker | Have | High: memory safety and flow control | Keep (the library enforces both) |
| Incoming topic aliases | Library | Required for correctness | Keep (the library resolves them) |
| Shared subscriptions (`$share/group/filter`) | Have (filter text and matching) | Medium | Keep |
| **Subscription options: No Local, Retain As Published, Retain Handling** | Missing | High. No Local hides your own publishes on a filter you also publish to. Retain Handling 2 skips the retained flood on subscribe. Retain As Published shows the real retain flag of a forwarded message. | **Add.** An "Options" control on the subscription row. Defaults are the MQTT defaults. Stored in the profile. |
| **Connection details from CONNACK**: assigned client ID, server keep alive, maximum QoS, retain available, wildcard / shared / subscription-ID available, receive maximum, topic alias maximum, maximum packet size, response information, server reference | Partly (maximum packet size) | High for diagnostics, low cost (display only) | **Add.** A "Connection details" popover from the status bar. The app also obeys server keep alive and shows assigned client ID. |
| Server redirection (server reference in CONNACK or DISCONNECT) | Missing | Low | Show the server reference in the error text only. No automatic redirect. |
| Subscription identifiers | Missing | Low. The app already matches filters on the client for colour and "Show only". Broker support varies. | Skip |
| Outgoing topic aliases | Missing | None for a desktop tool (saves bandwidth only) | Skip |
| User properties on CONNECT, SUBSCRIBE, UNSUBSCRIBE, DISCONNECT | Missing | Low | Skip |
| Enhanced authentication (AUTH packet, SCRAM, Kerberos) | Missing | Low; few brokers use it | Skip. Revisit on request (the library has `auth-scram`). |
| Request problem information, request response information flags | Missing | Low | Skip (library defaults) |

Result: two features to add (subscription options, connection details) and one small text change (server reference). All other MQTT 5 work stays as it is.

## MQTT 3.1.1 in the app

- **Profile.** New field `protocol: V311 | V5`. A new profile uses `V311`. The settings `schema_version` goes from 1 to 2. Migration: a profile without the field gets `V5`, because every v0.1.x profile connected with MQTT 5. A profile import without the field also gets `V5`, for the same reason. The connection sheet has a "Protocol" select.
- **What a 3.1.1 profile shows or hides.**
  - Connection sheet: "Clean session" instead of "Clean start" plus "Session expiry". No will delay. No subscription options.
  - Composer: no properties section. "Edit and publish" or "Reply" of an MQTT 5 message in a 3.1.1 session copies topic, payload, QoS and retain, and shows "MQTT 3.1.1 does not send properties."
  - Inspector: "No properties (MQTT 3.1.1)."
  - Results: 3.1.1 has no PUBACK reason, so QoS 1 shows "Acknowledged" or a timeout. SUBACK shows "granted QoS n" or "Refused (0x80)". CONNACK return codes 1 to 5 get plain text (wrong protocol version, client ID rejected, server unavailable, bad user name or password, not authorized).
  - "Connection details" shows only session present.
- **Errors that point to the other version.** A 3.1.1 broker refuses an MQTT 5 CONNECT: "This broker does not support MQTT 5. Set Protocol to MQTT 3.1.1." An MQTT 5-only broker refuses 3.1.1 (return code 1): "Set Protocol to MQTT 5."
- **Publish size.** `publish.rs` computes the wire size for each version (3.1.1 has no properties field).
- **Command line.** `--mqtt 3` or `--mqtt 5`. Default 3.

## Decisions

- **D1. Keep the boundary.** The protocol libraries are used only in `navigator-core/src/net.rs` (and the test helpers and `examples/flood.rs`). The public API of `navigator_core::net` gets only additive changes: the protocol in `ConnectConfig`, subscription options, and a `ConnectionDetails` event. The app changes only for the features above.
- **D2. Keep OpenSSL.** The connector uses `openssl` (vendored, as today) with `tokio-openssl`. Reasons: the same TLS errors and diagnostics on every OS, encrypted PEM keys with a passphrase (rustls cannot read them), the "Allow insecure TLS" switch, and no new crypto stack. A move to rustls is a later, separate decision.
- **D3. Our supervisor owns reconnect. One event loop for each attempt.** Each connect attempt builds `MqttOptions`, `AsyncClient` and `EventLoop`. The attempt ends at the first error, and the supervisor never calls `poll()` after an error. This keeps the generation rule: nothing of an old attempt reaches the UI. The comparison below (published 0.34.0 against `net.rs`) is the reason.

## Reconnect: ours or the library's

| Concern | `net.rs` supervisor (today) | `rumqttc-v5-next` 0.34.0 (`EventLoop::poll` after an error) |
|---|---|---|
| Wait between attempts | Exponential backoff 1 s to 30 s, jitter ±20 %, reset after 30 s of stable connection (`backoff_delay`, `next_retries`) | None. The next `poll()` connects at once (`eventloop.rs:1156-1163`). The caller must sleep. The only delay in the crate is the 100 ms dial stagger between addresses (`lib.rs:197`). |
| Retry or stop | Classified: bad credentials, ban, protocol errors stop; server busy, unavailable, quota, rate retry (`connack_is_temporary`, `disconnect_is_temporary`) | No policy. `ConnectionRefused(code)` is returned and the next `poll()` tries again, also for bad credentials. |
| User cancel during connect or backoff | Cancel signal; bounded join (5 s), then abort (`Sup::stop`) | Caller's job |
| Stale events after a new Connect | Generation and attempt token on every event | Caller's job |
| Panic in the client | Contained in the attempt task; becomes "Failed" | Caller's job |
| Subscriptions after a reconnect | Re-subscribes the desired set; removes filters that were removed offline when the session resumes (`SessionSet`) | Replays pending, unacknowledged SUBSCRIBE packets only. It does not know the desired set. |
| Unacknowledged QoS 1 PUBLISH at the loss | Reported "Lost on disconnect"; the user can send it again from History | Kept and sent again with DUP after the reconnect, with the same notice (`clean()`, `eventloop.rs:554-590`; test `poll_preserves_pending_state_across_reconnect_with_same_client_id`) |
| Tests | About 20 tests against a real Mosquitto (restart, refuse, resume, offline removal, cancel) | Unit tests with a mock network |

Result: the supervisor is better on every point but one: the library keeps an unacknowledged QoS 1 PUBLISH across a reconnect. That one point does not justify two reconnect state machines in one process, and an honest "Lost on disconnect" suits a test tool. So the supervisor stays, and the library's reconnect is never used. If the owner later wants the resend, the supervisor can keep one event loop per user Connect and call `poll()` after its own backoff. That is a separate change with its own tests.
- **D4. Exact pin and review.** `rumqttc-v4-next` and `rumqttc-v5-next`, both `=0.34.0`. `cargo deny` checks the new tree (`flume`, `async-tungstenite`, `rand` 0.10, `tokio-util`, `fixedbitset`).
- **D5. Remove the vendor copy last.** `vendor/ms-mqtt-client`, `[patch]`, ADR 0002 (superseded) go only after every gate passes.
- **D6. One internal interface, two adapters.** `net.rs` has one private `Wire` enum (`V4`, `V5`) with the same operations: connect, publish, subscribe, unsubscribe, disconnect, and next event. Each adapter maps its library types to our own types (`Incoming`, reason texts, `Outcome`). The supervisor, generations, queues and timers are shared and do not know the version.
- **D7. MQTT 3.1.1 is the default** for a new profile and for the command line. Existing profiles stay on MQTT 5 (migration above).

## Mapping

| Today (`ms-mqtt-client`) | New (`rumqttc-v5-next`) |
|---|---|
| `new_client(ClientOptions)`, `ConnectResult` | `MqttOptions` + `AsyncClient::builder(..).try_build()`; first `Event::Incoming(ConnAck)` or `ConnectionError` with the CONNACK reason |
| `ConnectionTransportConfig` + `TlsConfig` (OpenSSL) | socket connector (TCP + OpenSSL) + `Transport::Tcp` / `Transport::Ws` |
| `local_limits` (process-global) | `set_max_packet_size` for each client. The test lock for `local_limits` goes away. |
| publish + PUBACK wait | `publish_tracked`; `Qos0Flushed` → Sent, `Qos1(PubAck)` → Acknowledged or Rejected(reason), notice error → Lost on disconnect, our timer → Timed out |
| subscribe / unsubscribe, one filter each | `subscribe_tracked` / `unsubscribe_tracked`, one filter each (as today) |
| DISCONNECT from the broker, protocol errors | `Event::Incoming(Disconnect)`, `ConnectionError` → our `Outcome` (temporary or final, as today) |
| incoming PUBLISH with properties | `Event::Incoming(Packet::Publish)`: topic, payload `Bytes`, QoS, retain, `PublishProperties` (all fields the inspector shows) |

## Steps (tracer bullet first)

Each step ends with the gate: `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace`, `cargo fmt --all --check`, `cargo deny check`. Each step ends with a Grok review (read-only) until it says NO ISSUES. Work on branch `feat/rumqttc-next`. Nothing goes to `main` before S5.

- **S0. Plan review.** Grok reviews this plan until NO ISSUES.
- **S1. Spike (go or no-go).** A test program in `navigator-core/examples/` uses `rumqttc-v5-next` with our connector against the Docker Mosquitto (`tests/broker/up.sh`):
  - for **both** MQTT 3.1.1 and MQTT 5: TCP, TLS, mTLS, WS and WSS connect, subscribe, publish QoS 0 and 1; for MQTT 5 also all properties in both directions and the three subscription options;
  - the v4 client accepts a 1 MiB payload (its default limit is 10 KiB);
  - a fake broker (raw TCP) sends a 2 MiB PUBLISH: the client ends the connection, and memory does not grow by 2 MiB;
  - a broker that only speaks MQTT 3.1.1 gives a clear error;
  - TLS failures (expired, wrong host, unknown CA, bad client key) reach the connector as the same OpenSSL errors as today.
  Go: all pass. No-go: write down the gap. Then fall back to `ntex-mqtt` with a new plan, or stay on `ms-mqtt-client`.
- **S2. Port `net.rs` with MQTT 5 only.** Add the `Wire` interface (D6) with the V5 adapter first. Replace the session task and the operations. Keep the supervisor, generations, bounded queues, the drop counter and the cancel signal. Port `tls.rs` to return the OpenSSL connector and the diagnostics. All existing tests must pass here: this proves parity before anything new.
- **S2b. Add the V4 adapter.** Add `protocol` to `ConnectConfig`. Map the 3.1.1 CONNACK return codes, SUBACK failure and the error texts above.
- **S2c. Profile and app.** `protocol` field, schema 2 and its migration (test: a v1 file loads with `V5`), connection sheet select, the show and hide rules, `--mqtt`, the publish size for each version.
- **S2d. The two MQTT 5 additions.** Subscription options (profile, subscription row, net) and the "Connection details" popover (net event, status bar). Server reference in the error text.
- **S3. Port the tests and tools.** `net_mosquitto.rs`, `broker_matrix.rs`, `broker_compat.rs` (EMQX, HiveMQ), `load_ci.rs`, `soak.rs`, `examples/flood.rs`, `fuzz`. Remove the `local_limits` lock. Run the broker matrix for **both** protocol versions (Mosquitto, EMQX and HiveMQ all speak both). Add tests for each subscription option and for the CONNACK details. Add a test for each version-mismatch error.
- **S4. Measure.** Compare with the P3 and P7 numbers: the 10k msg/s load test (no drops), the overload test, idle wake-ups, release binary size (the CI size gate), and a 10-minute soak run. A worse number blocks S5 until it is explained or fixed.
- **S5. Clean up and document.** Remove `vendor/ms-mqtt-client` and the `[patch]`. Write ADR 0006 (client choice, D1 to D5, the fallback). Mark ADR 0002 superseded. Update ARCHITECTURE, README, AGENTS (the QoS 2 rule stays), the user guide (protocol select, subscription options, connection details, the 3.1.1 limits), `docs/ui-design.md`, `docs/parity.md`, `docs/open-questions.md` (close the upstream-issue item), THIRD-PARTY-NOTICES, CHANGELOG.
- **S6. Merge and release.** Merge to `main`, CI on three OSes, tag v0.2.0. The owner publishes the draft.

## Risks

| Risk | Mitigation |
|---|---|
| One maintainer; fast API change (0.x) | Exact pin; our broker tests on every update; ADR names `ntex-mqtt` as the fallback |
| The socket connector and `Transport::Ws` do not work over our TLS stream | S1 tests WSS first. Fallback: `TlsConfiguration::Connector` with our OpenSSL `TlsHandshakeConnector` (needs one TLS feature on). |
| Hidden queues in the client grow under load | Set every capacity and batch size. S4 measures RSS under overload. |
| Different reconnect or session-resume behaviour | D3 (we own reconnect); the existing resume tests (`a_lost_unsubscribe_is_retried_on_the_resumed_session` and others) must pass |
| Larger binary or more dependencies (two client crates) | S4 size gate; `cargo deny`. Both crates share `rumqttc-core-next` and `tokio`. |
| Version-specific bugs (a fix in one adapter only) | Shared types and tests that run the same scenario on both versions |
| Users of v0.1.x lose MQTT 5 on update | The migration keeps `V5` for every existing profile |

## Audit of the published 0.34.0 (2026-11-12)

Two independent read-only audits of the crates from crates.io (not the git main tree, which is unreleased `0.34.0-alpha`). Both agree on the items below. Line numbers are in `rumqttc-v5-next` 0.34.0.

| # | Area | Verdict | Evidence |
|---|---|---|---|
| 1 | One connect deadline over connector, TLS, WebSocket upgrade, CONNECT, CONNACK | OK | `eventloop.rs:1008` |
| 2 | Write stall: a peer that stops reading blocks `poll()` forever; keepalive cannot run; `disconnect_now` cannot preempt | DEFECT (v5); v4 has `FlushTimeout` | `framed.rs:279-291`, `eventloop.rs:1543-1546` |
| 3 | Dead-connection detection: each successful send resets the keepalive timer, also while a PINGRESP is owed; at best 2 × keep alive; no shorter ping timeout; keep alive 0 disables it | DEFECT | `eventloop.rs:1531`, `1650-1658`, `state.rs:2277-2281` |
| 4 | Fresh event loop + resumed session: `Strict` (default) fails every attempt; `AllowBrokerOnly` then rejects every QoS 1 publish, subscribe and unsubscribe | DEFECT for D3 (needs a supervisor-owned `SessionStore`) | `eventloop.rs:402-420`, `1433-1438` |
| 5 | WebSocket: no tungstenite frame or message cap (default 64 MiB message); a whole message is buffered before the MQTT size check; Ping, Pong and Text payloads enter the MQTT byte stream | RISK / DEFECT candidate | `eventloop.rs:1987-1991`, `rumqttc-core-next` `websockets.rs` |
| 6 | Refused CONNACK: reason string and server reference are discarded | DEFECT | `eventloop.rs:2089-2090` |
| 7 | After a read error mid-batch, QoS 1 messages already acknowledged can be dropped before delivery (automatic ack) | DEFECT (data loss with persistent sessions) | `framed.rs:124-148`, `eventloop.rs:603` |
| 8 | Outgoing tracking for 65 535 packet ids for each attempt unless `set_outgoing_inflight_upper_limit`; oversized outgoing publish is not checked before encode and ends the connection | RISK | `state.rs:673-674` |
| 9 | Incoming QoS 2 is accepted (PUBREC) with no option to refuse | RISK | `state.rs:1771-1781` |
| 10 | Panics from broker input | OK (none found) | |
| 11 | Spawned tasks, runtime | OK (none; `current_thread` works) | |

The CHANGELOG of the main tree lists fixes for several of these (write deadlines, keepalive scans, starvation) as unreleased. Work-arounds in our layer are possible (a write watchdog in the connector stream, our own WebSocket layer, an in-memory session store, manual acknowledgement, size checks before send), but they would add a second client around the client. The switch therefore waits.

## Audit of git main at 98bca64 (2026-11-12)

Owner question: use the GitHub main branch instead of the release. Two independent audits (Sonnet, with runnable probes under paused Tokio time; Grok, read-only) of the same defects. They agree:

| # | Defect | v5 on main | v4 on main |
|---|---|---|---|
| D1 | Write or flush with no deadline; `poll()` can hang | NOT FIXED (only protocol response writes have a deadline; publish feed and flush do not; probe: blocked 600 s) | PARTLY (flush has `FlushTimeout` 5 s; a large feed still hangs) |
| D2 | Outgoing sends reset the keepalive timer while PINGRESP is owed | NOT FIXED (probe: steady publishing to a dead peer, no error in 120 s) | NOT FIXED |
| D3 | Fresh event loop cannot resume a persistent session | PARTLY: works with a shared `SessionStore`, but only if each loop is polled until its `Err` (the checkpoint is saved there) | Resumes, but with no local state it can collide with packet ids the broker holds |
| D4 | WebSocket caps and frame filtering | NOT FIXED (not on our path if the connector does WebSocket) | NOT FIXED |
| D5 | Refused CONNACK details | FIXED (`take_connection_failure_packet()`) | n/a |
| D6 | Acknowledged QoS 1 dropped after a read error | FIXED | FIXED |
| D7 | Tracking slots; oversize publish | Slots FIXED; oversize still ends the connection | Same; default outgoing cap 10 KiB |
| D8 | Incoming QoS 2 | FIXED (manual ack with a reason) | PARTLY |

Other facts: no panics from broker bytes found; no spawned tasks; version `0.34.0-alpha`; the unreleased CHANGELOG has at least 7 breaking items, so a later move off the pinned rev costs migration work; default features pull rustls/aws-lc (use `default-features = false`).

Result: main fixes four items, but not the two liveness defects (D1, D2). With main, our layer would still need a write-progress timeout in the connector stream, our own dead-peer detection (TCP user timeout or an incoming-silence watchdog that does not fire falsely during steady QoS 0 publishing), and a shared `SessionStore` with the "poll to `Err`" rule. The switch stays on hold.

## Audit of ntex-mqtt 10.0.0 (2026-11-14)

Two independent audits: Sonnet ran probes against fake brokers on loopback, in real time. Grok did a read-only review of the code. They agree, except where the table says otherwise.

| # | rumqttc-next defect | ntex-mqtt 10.0.0 |
|---|---|---|
| D1 | Write stall | No deadline by default. Set `IoConfig::set_write_timeout`. Probe: the send failed with `Disconnected` at 3.07 s with a 3 s write timeout. With no write timeout, the probe blocked for 12 s with no error. Cancel needs `force_close()`, because `close()` does not unblock the send. |
| D2 | Dead peer | OK. A keep-alive task sends PINGREQ at each interval, even while the app publishes. Probe: keep alive 5 s, with or without traffic, closed at 10.2 s. The cause surfaces only as `PeerGone(None)`. The timer waits while the write buffer is full, so the write timeout must cover that case. |
| D3 | Fresh client, resumed session | OK. `session_present` is visible. A broker redelivery (DUP) goes to the handler and gets a PUBACK. Unacknowledged outgoing messages are not resent (same as our "Lost on disconnect"). |
| D4 | WebSocket caps | Not in ntex-mqtt. The `ntex` crate's `WsClient` has caps (frame 64 KiB, message 4 MiB) that we can configure. Not tested end to end. |
| D5 | Refused CONNACK | OK. `MqttClientError::Ack` carries the full packet. v5 probe: reason code, reason string and server reference all came through. |
| D6 | QoS 1 acknowledged and then dropped | OK. PUBACK goes out after the handler returns. |
| D7 | Outgoing limits | v5 OK: the in-flight cap is the CONNACK Receive Maximum (65,535 if absent), and `OverMaxPacketSize` is raised before send. v3: the in-flight cap is a fixed 16, and the outgoing limit is the same as `max_size` (256 KiB default). |
| D8 | Incoming QoS 2 | Goes to the handler. The handler can refuse it: on v5 with a reason code, on v3 only by closing the connection. |
| — | Incoming size | Checked at the fixed header, before allocation. Probe: a 2 MiB header with a 1 MiB limit closed the connection in 0.1 ms. On v5 the limit is off unless `Connect::max_packet_size` is set. |
| — | Connect deadline | `handshake_timeout` is off by default. When set, it covers TCP, TLS, CONNECT and CONNACK. Probe: 3.08 s for a 3 s limit. DNS runs `getaddrinfo` inline on the net thread. The timeout cannot cancel it. |
| — | Panics | None. 6 million mutated decodes and 14 malformed dispatcher cases ended in clean errors. One panic needs no broker input: `send_at_least_once_no_block` without an ack callback. Do not use it. |
| — | Runtime | ntex runtime stack. It can run in our tokio `current_thread` runtime, but only with `ntex_net::with_reactor` and a `LocalSet`. The types use `Rc`. |
| — | TLS | Our `SslStream` cannot be used directly. `ntex::connect::openssl::SslConnector` works (probe: TLS + CONNACK in 44 ms), but it pulls in the large `ntex` crate. |
| — | Size | Probe binary: 101 unique crates, 2.6 MB stripped. With `ntex` and vendored OpenSSL: 186 unique crates, 7.5 MB stripped. |
| — | Project | MIT OR Apache-2.0. MSRV 1.97. One maintainer (135 of 137 commits in 12 months). 6 breaking major versions in 19 months (4 in the last 11 months). |

Result: ntex-mqtt has none of the liveness defects of rumqttc-next, once these are set: write timeout, handshake timeout, both size limits, and our own DNS step. The cost is a second I/O runtime stack and the `ntex` crate for TLS and WebSocket. There is also a breaking major version about every 3 months, from one maintainer.
