# Plan: replace ms-mqtt-client with ntex-mqtt

Status: r7 (2026-11-14). Codex: NO ISSUES at r6. r7 applies the S1 results (see `phase-8-verification.md`): the ntex timer and the I/O shutdown add up to about 1 s to each bound, the v5 outgoing size check is ours, and the QoS 2 refusal uses `close_with_reason`. This work is phase 8.

## Goal

The owner requires MQTT 3.1.1 (2026-11-14). `ms-mqtt-client` supports MQTT 5 only. Replace it with `ntex-mqtt` 10.0.0, which has a v3 (MQTT 3.1.1) client and a v5 client.

- MQTT 3.1.1 is the default for a new profile.
- MQTT 5 keeps every behaviour of v0.1.x.
- The connection stays as robust as today, or becomes more robust. Section "Robust connection" lists the rules.
- Release the result as v0.2.0.

The audits that led to this choice are in [2026-11-12-rumqttc-next-plan.md](2026-11-12-rumqttc-next-plan.md): rumqttc-next (rejected: liveness defects D1, D2) and ntex-mqtt (accepted with conditions).

## Not in scope

- QoS 2. The `Qos` enum keeps two values.
- rustls. TLS stays on OpenSSL (vendored).
- Automatic server redirect. Unacknowledged QoS 1 messages are not sent again after a reconnect (the app shows "Lost on disconnect", as today).

## Facts (ntex-mqtt 10.0.0 and its ntex 4.x dependencies)

| Fact | Effect |
|---|---|
| `ntex_mqtt::v3` and `ntex_mqtt::v5`: `MqttConnector` (a `Service` that takes `Connect` and returns `Client`). `Client::sink()` gives `MqttSink`. `Client::start(service)` runs the dispatcher until the connection ends. | One adapter for each version (D6). The session task drives `start()` and uses the sink for operations. |
| The runtime is ntex. With feature `ntex-net/tokio` it runs on tokio, but needs `ntex_net::with_reactor(tokio::Reactor)` around the code, and a `LocalSet` (spawn is `spawn_local`). The types use `Rc`. | The net thread already uses `current_thread` + `LocalSet` (`net.rs:370-383`). Add `with_reactor` there. A nested call (while a reactor scope is active) panics; sequential calls are fine. We use one outer scope around the whole net-thread body, by choice. Nothing is `Send`; that is already the rule on the net thread. |
| These client deadlines are off by default: MQTT `handshake_timeout`, `IoConfig` connect timeout, `IoConfig` write timeout, keep alive (0). The v5 incoming size limit is off unless `Connect::max_packet_size` is set. Some transport timeouts are on: TLS handshake 5 s (`ntex-tls` `config.rs:41`), WebSocket handshake and close 5 s (`ntex` `ws/cfg.rs:72`), I/O shutdown 1 s. | Section "Robust connection" sets each one explicitly. Our outer 10 s connect deadline covers all of them; the 5 s transport timeouts end earlier and give a more exact text. A test checks each one. |
| A send waits for write readiness, with no deadline of its own. `sink.close()` clears the acknowledgement waits at once (`clear_queues`, source). A send that waits for a blocked write, and the attempt itself, end only after the I/O shutdown wait (S1: 1.0 to 1.1 s), with `Disconnected`. `sink.force_close()` ends both within ms. The `IoConfig` write timeout (probe: 3 s) ends a stalled connection with `WriteTimeout`. | Set a write timeout. Disconnect and stop use a graceful `close()`, wait at most 2 s, then `force_close()`. |
| Keep alive: a detached task sends PINGREQ at each interval, also while we publish. If the PINGRESP is still missing at the next tick (one full interval later), it closes (`v3/client/connection.rs:433-461`, v5 same). The ping state is private, and the cause reaches the caller only as a plain close. v5 uses the CONNACK Server Keep Alive when present, also 0. The task holds a sink clone until its next tick, then sees the closed connection and ends. | Detection takes up to 2 × keep alive (today: keep alive + 10 s). See "Robust connection". The text is general: "The connection closed (the broker did not answer, or the network failed)." Old keep-alive tasks end within one interval; a test proves it. |
| Refused CONNACK: `MqttClientError::Ack(packet)`. v3: return code. v5: reason code, reason string, server reference. A 3.1.1-only broker can answer an MQTT 5 CONNECT with a 3.1.1 CONNACK (return code 1); the v5 decoder reads it as a v5 packet or fails to decode it (`v5/codec/packet/connack.rs:165`). | The error text keeps all of these. Section "MQTT 3.1.1 in the app" defines the version texts. |
| PUBACK is sent after our handler returns. A handler error closes the connection without PUBACK. Both versions can deliver a PUBLISH payload as a stream of chunks (`v3/client/control.rs:83`, `v5/client/control.rs:95`). `max_payload_buffer_size` bounds unread chunks only; `read_all()` grows its own buffer (`payload.rs:136`). | The handler collects the payload asynchronously, with its own cap (the incoming packet limit), then does `try_send` to the ingest queue (drops counted, as today). The incoming packet limit already bounds the total, because the codec checks the declared length first. The partial payload is dropped when the connection ends. A broker that starts a packet and stops is caught by the frame read rate (see "Robust connection"). |
| Incoming QoS 2 goes to the handler. PUBREC has no "QoS not supported" reason; that reason (0x9B) exists only in DISCONNECT. | The handler never delivers a QoS 2 message. v5: the handler calls `sink.close_with_reason` (DISCONNECT 0x9B) and returns an error (S1: `ProtocolMessage::disconnect` loses the packet, because the I/O closes first); the session ends with `Failed("The broker sent a QoS 2 message. This app supports QoS 0 and 1.")`. v3: the handler returns an error, which closes the connection without PUBREC; same text. We subscribe with QoS ≤ 1, so a compliant broker never sends QoS 2. |
| Incoming size is checked at the fixed header, before allocation (probe: 2 MiB header with a 1 MiB limit closed in 0.1 ms). v3 `max_size` limits the Remaining Length; v5 Maximum Packet Size limits the whole packet (`v3/codec/codec.rs:131`, `v5/codec/codec.rs:294`). v3 applies `max_size` to outgoing packets too. | `NetLimits::max_packet` stays the whole-packet limit (1 MiB). v5: `Connect::max_packet_size(max_packet)`. v3: `set_max_size(r)`, where `r` is the largest Remaining Length with `1 + varint_len(r) + r <= max_packet` (for 1 MiB: `r = 1 048 572`). One helper computes it; a unit test checks the varint boundaries (127, 16 383, 2 097 151). The v3 codec accepts a non-minimal length encoding and checks only the Remaining Length, so a v3 packet can be up to 3 bytes over `max_packet`. Accepted: the memory bound stays. Outgoing v5 keeps today's rule: the broker's Maximum Packet Size when the CONNACK has it, else the MQTT maximum (`publish::validate_outgoing`). Outgoing v3 is limited to `max_packet`, because the v3 codec uses one limit for both directions; our publish check gives the text "MQTT 3.1.1 messages can be at most 1 MiB in this app." before the send. Boundary tests, incoming (minimal length encoding): a whole packet of exactly `max_packet` passes; one byte more fails, on both versions. Outgoing: exactly `max_packet` passes on both versions. One byte more is refused before the send on v3 (`OverMaxPacketSize`); on v5 it is refused only when the CONNACK announces a Maximum Packet Size (fake-broker test), and it is sent when the broker announces none (Mosquitto). v3: a non-minimal encoding of `r` passes, and `r + 1` fails. |
| Outgoing: v5 respects CONNACK Receive Maximum and Maximum Packet Size (`OverMaxPacketSize` before send). v3 uses `set_max_send` (default 16) and `set_max_size` for both directions. | Our publish code keeps its own size check for a clear error text. v3: set `max_size` to our limit. |
| TLS: `ntex-tls` (feature `openssl`) wraps an `openssl::ssl::SslConnector`. `ntex` is only a dev-dependency of `ntex-tls`. The connector uses the host of the request for SNI and hostname verification (`ntex-tls` `openssl/connect.rs:99`). The probe connected with TLS + CONNACK in 44 ms. | `tls.rs` keeps building the OpenSSL connector with our verify callback and `TlsDiag`. Only the outer type changes. The request keeps the profile host name; our resolved addresses go in with `Connect::set_addrs` (`ntex-net` `connect/message.rs:139`). |
| WebSocket: `ntex::ws::WsClient` (`ntex` feature `ws`). This is the only reason for the `ntex` crate. Frame cap 64 KiB and message cap 4 MiB by default; they are independent (`ws/cfg.rs:159`, `:169`). `WsClientConfig::set_address` sets the socket address and keeps the URI host. | Set both caps to `max_packet`. Offer subprotocol `mqtt`. Keep the profile host in the URI (HTTP authority, SNI, verification). The connection comes from our dialer (see DNS and dial). S1 tests: a 1 MiB packet in one frame, a fragmented packet, and Ping, Pong and Text frames. |
| DNS and dial: the ntex connector calls `getaddrinfo` on the calling thread when there is no ntex `System`, and the tokio reactor dials in a detached task that a timeout cannot cancel (`ntex-net` `tokio/mod.rs:62`). `ntex_net::from_tcp_stream(std TcpStream, cfg)` (`lib.rs:190`) turns a connected socket into an ntex `Io`. `WsClient::connector` takes any connector service (`ws/client.rs:115`). | **Our dialer** does DNS and TCP (`tokio::net::TcpStream::connect`, each resolved address in order) under the one connect deadline. A cancel ends the TCP dial and the DNS wait at once. The OS lookup itself cannot be stopped, so it runs in our own `spawn_blocking` that holds the only permit of a one-slot semaphore until the lookup returns. A new attempt waits for that permit, under its own deadline; at most one lookup runs at a time. It converts the socket with `from_tcp_stream`, then adds TLS with `ntex-tls` when needed. The same dialer is the connector for MQTT over TCP/TLS and for `WsClient`. The profile host name stays in the request (SNI, verification, WebSocket authority). Tests: the first address refuses and the second works; a stalled lookup (a test resolver hook) with 10 cancelled attempts keeps one lookup running, not 10; a black-hole address with a disconnect during the dial leaves no open socket (count the fds of the process before and after). |
| `send_at_least_once_no_block` without `publish_ack_cb` panics on the PUBACK. | Never use it. Use only the awaited sends. |
| Size: probe 2.6 MB stripped (no TLS); 7.5 MB with `ntex` (WebSocket) + vendored OpenSSL. MIT OR Apache-2.0. MSRV 1.97. | S4 compares the app size with v0.1.1. `cargo deny` checks the new tree. |
| One maintainer. 6 breaking major versions in 19 months. | Exact pin (`=10.0.0` and each `ntex-*` crate through `Cargo.lock`). Our broker tests on every update. Dependabot groups the ntex crates, and each major goes in its own commit. |

## Robust connection (rules, each with a test)

Timer order. `CONNECT_TIMEOUT` (10 s) covers DNS to CONNACK. `OP_TIMEOUT` (10 s, from submit) covers each publish, subscribe and unsubscribe; its expiry stays final (`Failed`), as today. The `IoConfig` write timeout (10 s) covers write backpressure (the write buffer is above its high mark); its expiry is temporary (reconnect). Small writes below that mark (PUBACK, PINGREQ) sit in the kernel buffer; keep alive finds a peer that takes them but never answers. The frame read rate (`IoConfig::set_frame_read_rate(10 s, 60 s, 256)`) ends a connection when a started packet does not progress at 256 bytes in each 10 s, or does not finish in 60 s (see the cap bound in the table below). Keep alive covers silence between packets.

Keep alive 0 (from the profile, or a v5 Server Keep Alive of 0) turns off dead-peer detection, as MQTT defines. Then a lost connection is found only when an operation times out or the OS reports the error. The connection sheet shows this under the keep-alive field, and Connection details shows "Keep alive: off".

| Rule | Setting or code | Test |
|---|---|---|
| One deadline over DNS, TCP, TLS, WebSocket upgrade, CONNECT and CONNACK | Our `timeout` around DNS + the connector call (10 s, as today), plus `set_handshake_timeout(10 s)` | Black-hole address, silent CONNACK, stalled TLS: each fails in ≤ 11 s |
| A peer that stops reading cannot block us | `IoConfig::set_write_timeout(10 s)`; `OP_TIMEOUT` for operations | Fake broker stops reading while we send large payloads (backpressure): the publish ends with "Timed out" in ≤ 11 s, and the attempt ends in ≤ write timeout + 2.5 s = 12.5 s. Two stages add to the write timeout: the I/O timer fires after at least `t` and less than `t + 1` s (`ntex-io` `ops.rs`), and the dispatcher then waits for the I/O shutdown (one `shutdown_timeout`, 1 s, plus timer rounding). S1: 11.0 s, with keep alive 0 and with keep alive 2 s running |
| A started packet that stops cannot block us | Frame read rate (above), for TCP and TLS. Over WS and WSS, the WebSocket layer keeps an incomplete frame until it is whole, so the MQTT timer does not start; there keep alive finds the stall (the PINGRESP is behind the stalled frame). With keep alive 0 over WS, a stalled frame is not found. | TCP and TLS: fake broker sends half a fixed header, and half a PUBLISH payload, with keep alive 0: a half frame ends at the first rate check: one period (10 s), plus up to 1 s timer step, 1 s I/O shutdown and 1 s rounding, so in ≤ 13 s; a payload that trickles above 256 bytes in each 10 s and never completes ends at the 60 s cap. Source (`ntex-mqtt` `io/mod.rs`): each rate check takes one period from the budget and starts a new timer, so there are 6 checks, each at most 11 s long, and then 1 s of shutdown and 1 s of rounding: the bound is 6 × 11 + 2 = 68 s. The attempt then reconnects. WS: a half binary frame with keep alive 2 s: ends in ≤ 2 × keep alive × 1.125 + 2 s (the WS close waits up to 1 s for the peer; S1: 5.15 s). A disconnect during each wait is prompt. |
| A dead peer is found | ntex keep alive. When the write buffer is not blocked: up to 2 × keep alive (profile default 60 s, so ≤ 120 s). When it is blocked, the write timeout ends the connection first (10 s). The ntex timer can fire up to one timer step late for each sleep (the step grows with the delay: about 1 s at 60 s, up to 1/8 of the delay for long delays; `ntex-rt` `time/mod.rs:26`). Upper bound: 2 × keep alive × 1.125 + 10 s (60 s: ≤ 145 s). This is slower than today (keep alive + 10 s). Accepted: MQTT allows "a reasonable time". | Fake broker reads but never answers PINGREQ, with and without our publishes, keep alive 2 s and 60 s: the attempt ends in ≤ 2 × keep alive × 1.125 + 1 s. With a blocked write buffer: ≤ 2 × keep alive × 1.125 + write timeout + 2.5 s (17 s at keep alive 2 s and write timeout 10 s). Keep alive 70 s (in the 8 s timer step range) checks the rounding. Server Keep Alive 0 and 5 s: the client follows it. |
| Old keep-alive tasks end | The task ends at its first tick after the close (`v3/client/connection.rs:437-441`, v5 same) | The task logs "stopping keep-alive task" (`log` debug) when it ends. The test captures `log` records. Connect and disconnect 20 times with keep alive 2 s, against Mosquitto. Before the first tick (at 1 s): the count of those records is 0, which proves the test sees a live task. After 3 s: the count is 20. |
| Disconnect and stop are prompt | Graceful DISCONNECT, wait ≤ 2 s, then `force_close()`. The attempt task is joined, then aborted (as today) | Disconnect during a stalled send: state is Disconnected in ≤ 3 s |
| Bounded memory | Incoming packet limit on both versions; ingest queue by count and bytes (as today); v3 `max_receive` 16; v5 Receive Maximum in CONNECT; payload collection capped at the packet limit | 2 MiB header: closed before allocation; overload test (RSS flat) |
| Bounded outgoing | Our command queue (as today); v3 `max_send` 16; v5 broker Receive Maximum | 1000 queued QoS 1 publishes at Receive Maximum 2: RSS stays flat, results arrive in order |
| No stale events | Generations and attempt tokens (as today) | Existing tests |
| Panic containment | Attempt task (as today). ntex runs extra handler calls in detached tasks, so a panic there skips our join. Each adapter callback runs inside `catch_unwind` (`FutureExt::catch_unwind` with `AssertUnwindSafe`); a panic calls `force_close()` and ends the attempt with `Failed`. | Existing test; plus a handler that panics after an await while a second call is pending: the attempt ends with `Failed`, no hang |

## Decisions

- **D1. Keep the boundary.** Only `navigator-core/src/net.rs` (and its new submodules), the test helpers and `examples/flood.rs` use ntex. The public API of `navigator_core::net` has these allowed changes (the only caller is our app): the `protocol` field in the profile, `options` and `req` fields on `Command::Subscribe`, a `req` field on the `Subscribed` result (the ID of the Subscribe that it answers), an `attempt` field on `Status`, and the `ConnectionDetails` event with its `attempt`.
- **D2. Keep OpenSSL.** `tls.rs` keeps the material, the verify callback and the diagnostics. It returns an `openssl::ssl::SslConnector`. `net.rs` wraps it with `ntex-tls`.
- **D3. Our supervisor owns reconnect.** One `MqttConnector` call and one `Client` for each attempt. ntex-mqtt has no reconnect of its own, so there is no conflict. Backoff, retry rules, cancel, generations and the desired-subscription set stay as they are. "The session persists" is decided per version: v5 when `session_expiry_secs > 0`; v3 when Clean Session is off. That one function replaces `session_expiry_secs > 0` in `AttemptCtx` (`net.rs:853`). The session-filter tests (resume, offline removal, lost UNSUBACK, app restart) run on both versions.
- **D4. Exact pin.** `ntex-mqtt = "=10.0.0"`. Add `ntex-tls` (`openssl`), `ntex-net` (`tokio`), and `ntex` with `default-features = false, features = ["tokio", "ws"]` for WebSocket only. Remove `ms-mqtt-client` and the vendor copy in S5, after the last user is ported.
- **D5. Remove the vendor copy last.** `vendor/ms-mqtt-client`, `[patch]`, and ADR 0002 (marked superseded) go in S5.
- **D6. One internal interface, two adapters.** A private `Wire` enum (`V3`, `V5`) with the same operations: publish, subscribe, unsubscribe, disconnect, and the incoming handler. Each adapter uses `start_with_control`, not `start`: the default control service drops the stop reason (`control.rs:158`). Protocol messages (broker DISCONNECT with its reason) and connection-control messages (`WriteTimeout`, protocol error, peer gone) map to `Outcome` separately. The broker DISCONNECT reason is kept before the close completes. Each adapter maps library types to our types (`Incoming`, reason texts, `Outcome`). The supervisor, queues and timers do not know the version.
- **D7. MQTT 3.1.1 is the default** for a new profile and for the command line. Existing profiles keep MQTT 5 (migration below).
- **D8. Our TLS-diagnosis and error texts stay.** Each ntex error is mapped to our `Outcome` (temporary or final) with the same texts as today where the cause is the same.

## MQTT 5 features

Moved here from the rumqttc-next plan, unchanged. "Have" means v0.1.x has it; it stays for MQTT 5 profiles.

| Feature | Decision |
|---|---|
| Reason codes and strings (CONNACK, SUBACK, UNSUBACK, PUBACK, DISCONNECT) | Keep |
| Session expiry, clean start, session present | Keep |
| PUBLISH properties both ways | Keep |
| Reply (response topic, correlation data) | Keep |
| Will properties (delay) | Keep |
| Maximum packet size, Receive Maximum | Keep (library enforces the broker values; we set ours) |
| Incoming topic aliases | Keep. Set CONNECT `topic_alias_max` to 16 (the client dispatcher limit, `v5/client/connection.rs:189`). Test alias set, reuse and an invalid alias. |
| Shared subscriptions | Keep |
| **Subscription options: No Local, Retain As Published, Retain Handling** | **Add.** "Options" on the subscription row; stored in the profile. ntex: `SubscriptionOptions`. |
| **Connection details from CONNACK** | **Add.** A "Connection details" popover from the status bar. ntex: `Client::packet()`. The app keeps only the details of the latest connection. |
| Server reference | Show it in the error text only |
| Subscription identifiers, outgoing topic aliases, user properties on CONNECT/SUBSCRIBE/UNSUBSCRIBE/DISCONNECT, enhanced auth, request-problem/response-information flags | Skip |

## MQTT 3.1.1 in the app

- **Profile.** New field `protocol: V311 | V5`. A new profile uses `V311`. Settings `schema_version` goes from 1 to 2. A profile without the field (old file or import) gets `V5`, because every v0.1.x profile used MQTT 5. The connection sheet has a "Protocol" select.
- **Hide or show for 3.1.1.**
  - Connection sheet: "Clean session" instead of "Clean start" and "Session expiry". No will delay. No subscription options.
  - Composer: no properties. "Edit and publish" of an MQTT 5 message copies topic, payload, QoS and retain. "Reply" keeps its behaviour (the topic is the Response Topic) and copies no properties. Each action shows "MQTT 3.1.1 does not send properties." when the source message has an MQTT 5 property.
  - Inspector: "No properties (MQTT 3.1.1)."
  - Results: QoS 1 shows "Acknowledged" or "Timed out". SUBACK shows "Granted QoS n" or "Refused (0x80)". CONNACK return codes 1 to 5 get plain text.
  - Connection details: session present only.
- **Version errors.**
  - v5 CONNACK reason 0x84 (unsupported protocol version): "This broker does not support MQTT 5. Set Protocol to MQTT 3.1.1."
  - An MQTT 5 connect gets a CONNACK that the v5 decoder cannot read (a 3.1.1 broker sends a 3.1.1 refusal; ntex reports `MalformedPacket` and drops the bytes): "The broker sent a reply that is not MQTT 5. If the broker supports only MQTT 3.1.1, set Protocol to MQTT 3.1.1."
  - v3 CONNACK return code 1: "This broker does not support MQTT 3.1.1. Set Protocol to MQTT 5."
  - The broker closes before CONNACK: "The broker closed the connection before it accepted it. Check the address, the TLS settings and the Protocol." This is not a version claim.
  - Tests send each case as raw bytes from the fake broker.
- **Publish size.** `publish.rs` computes the wire size for each version.
- **Command line.** `--mqtt 3` or `--mqtt 5`. Default 3.

## Mapping

| Today (`ms-mqtt-client`) | ntex-mqtt |
|---|---|
| `new_client(ClientOptions)`, `ConnectResult` | `v3::client::MqttConnector` / `v5::client::MqttConnector` with our connector, called with `Connect`; `Ok(Client)` or `MqttClientError` (`Ack(packet)`, `Handshake`, `Disconnected`, timeout) |
| `ConnectionTransportType::{Tcp, Tls, Ws}` + `TlsConfig` | Our connector service: DNS (ours) → ntex TCP → `ntex-tls` OpenSSL (TLS, WSS) → `ntex::ws::WsClient` (WS, WSS) → `IoBoxed` |
| `local_limits` (process-global) | Per connection: `MqttServiceConfig` and `IoConfig` in a `SharedCfg`. The test lock for `local_limits` goes away. |
| Publish + PUBACK wait | `sink.publish(topic)...send_at_most_once` → Sent; `send_at_least_once` → Acknowledged (v3) or `PublishAck` reason (v5) → Acknowledged / Rejected; `Disconnected` → Lost on disconnect; our 10 s timer → Timed out |
| Subscribe | One filter for each packet, as today. v3: `Vec<SubscribeReturnCode>`: `Success(q)` → granted QoS q; `Failure` → "Refused (0x80)". v5: `SubscribeAck` status for the filter: granted QoS or the reason code and reason string. A granted QoS 2 cannot happen (we ask ≤ 1); if it does, the result is an error. |
| Unsubscribe | v3: `Ok(())` = done (3.1.1 has no reason). v5: `UnsubscribeAck` status: 0x00 and 0x11 (no subscription existed) = done; any other reason = failed, with its text. The session filter set changes only on done. |
| Receive loop | `client.start_with_control(handler, control)` (D6). The handler collects the payload, maps `Publish` to `Incoming`, and does `try_send`. |

## Steps (tracer bullet first)

Each step ends with the gate:

1. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo deny check`.
2. `cargo test --workspace`.
3. Broker tests: `tests/broker/up.sh`, then `cargo test -p navigator-core --features broker-tests`. `up.sh` checks one TCP listener for each broker today; S2 extends it to a real connect on each listener (TCP, auth, TLS, mTLS, WS, WSS). A broker test that finds no broker must fail, not return early (change `net_mosquitto.rs:157` in S2).
4. A read-only Codex review until it says NO ISSUES (AGENTS.md). Grok may add a second review.

Work on branch `feat/ntex-mqtt`. Nothing goes to `main` before S6. The report `docs/plans/phase-8-verification.md` records the commands run and the numbers measured, for each step. It records only checks that ran.

- **S0. Plan review.** Codex reviews this plan until NO ISSUES.
- **S1. Spike (go or no-go).** An example program in `navigator-core/examples/` against the Docker Mosquitto (`tests/broker/up.sh`), inside our `current_thread` + `LocalSet` + `with_reactor` set-up:
  - v3 and v5: TCP, TLS, mTLS, WS and WSS: connect, subscribe, publish QoS 0 and 1, receive; with a DNS name (SNI and verification), not only an IP address;
  - TLS: server verification failures (expired, wrong host, unknown CA) reach `TlsDiag` with the same texts as today; a peer mTLS refusal gives the same text as today; local material errors (bad or encrypted client key) keep their texts from `TlsMaterial::load` (no change, no network);
  - a packet of exactly `max_packet` both ways on both versions; one byte more fails for incoming packets on both versions, and for outgoing packets on v3 and on v5 when the CONNACK announces a Maximum Packet Size (a v5 broker that announces none gets the packet, as today);
  - the "Robust connection" fake-broker cases: stalled write (both kinds), missing PINGRESP, old keep-alive tasks end, 2 MiB header, silent CONNACK, refused CONNACK (v3 and v5), the version-error bytes, incoming QoS 2;
  - release size of the example with the full feature set.
  Go: all pass. No-go: write down the gap and stop.
- **S2. Port `net.rs`, MQTT 5 only.** The `Wire` interface with the V5 adapter first. Replace the session task, the connector and the operations. Port `tls.rs` to return the OpenSSL connector. Existing tests that build a profile pin it to `V5` before S2c changes the default. All existing tests pass: this proves parity before anything new. `ms-mqtt-client` stays a dev-dependency until S3 ports the test publishers.
- **S2b. V3 adapter.** `protocol` in `ConnectConfig`. CONNACK return codes, SUBACK failure, error texts.
- **S2c. Profile and app.** `protocol` field, schema 2 and its migration (test: a v1 file loads as `V5`), the Protocol select, the hide and show rules, `--mqtt`, the publish size for each version.
- **S2d. The MQTT 5 additions.** Subscription options and the "Connection details" popover. Server reference in the error text.
- **S3. Port the tests and tools.** `net_mosquitto.rs`, `broker_matrix.rs`, `broker_compat.rs` (EMQX, HiveMQ), `load_ci.rs`, `soak.rs`, `examples/flood.rs`, `fuzz`. Run the broker matrix so that each transport and each version is used at least once. A test for a second version or transport stays only where the code differs. Tests may shorten the time limits with `NetLimits::timers`; one test for each limit keeps the real value. Move the S1 fake-broker cases into `tests/`. Run the soak for 10 minutes: `SOAK_SECS=600 cargo test --release -p navigator-core --features broker-tests --test soak -- --ignored`.
- **S4. Measure.** Compare with the P3 and P7 numbers: 10k msg/s with no drops, the overload test, idle wake-ups (the keep alive timer is the only one while connected), release size (CI size gate), a 10-minute soak. A worse number blocks S5 until it is explained or fixed.
- **S5. Clean up and document.** Remove `vendor/ms-mqtt-client` and the `[patch]`. ADR 0006 (client choice, D1 to D8). ADR 0002 superseded. Update ARCHITECTURE, README, AGENTS, the user guide, `docs/ui-design.md`, `docs/parity.md`, `docs/open-questions.md`, THIRD-PARTY-NOTICES, CHANGELOG, the log filter (`ms_mqtt_client=off` → ntex targets).
- **S6. Merge and release.** CI passes on three OSes on the branch. Merge to `main` after owner approval. Tag v0.2.0. The owner publishes the draft.

## Risks

| Risk | Mitigation |
|---|---|
| One maintainer; a breaking major about every 3 months | Exact pin; Dependabot proposes, we upgrade on our schedule; broker tests on every upgrade; the `Wire` adapters keep ntex in one module |
| The client deadlines are off by default; a future version changes a default | Each setting is explicit in one function; each has a fake-broker test |
| The `ntex` crate (HTTP, WS) increases size and dependencies | `default-features = false`; S4 size gate; `cargo deny` |
| WS/WSS through `ntex::ws` behaves differently from today | S1 proves it first; the broker matrix covers WS and WSS (each transport and each version at least once) |
| TLS errors lose detail through `ntex-tls` | Our verify callback and `TlsDiag` keep the detail; S1 checks the four cases |
| ntex and tokio on one thread (`with_reactor`) | S1 proves it in our set-up; S4 checks idle wake-ups |
| Dead-peer detection is slower than today (2 × keep alive) | Documented in the user guide. If owners need faster detection, propose a ping-response timeout to ntex-mqtt upstream (a public post, only with owner approval). |
| Users of v0.1.x lose MQTT 5 | The migration keeps `V5` for every existing profile |
