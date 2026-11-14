# Phase 8 verification

Plan: [2026-11-14-ntex-mqtt-plan.md](2026-11-14-ntex-mqtt-plan.md). Branch: `feat/ntex-mqtt`.
This file records only checks that ran.

## S1 spike

Date: 2026-11-14. Libraries: `ntex-mqtt` 10.0.0, `ntex` 4.8.0, `ntex-net` 4.12.0, `ntex-tls` 4.8.0, `ntex-io` 4.7.2, `ntex-util` 4.11.0, `ntex-service` 5.7.2 (all in `Cargo.lock`). Broker: Docker Mosquitto 2.1.2 from `tests/broker`. Host: Linux, Rust 1.99.1, release profile of the workspace.

### What was built

- `crates/navigator-core/examples/ntex_spike.rs` and the helper modules in `examples/ntex_spike/`: `dialer.rs`, `wire.rs`, `fake.rs`, `cases.rs`, `fakecases.rs`.
- ntex crates as dev-dependencies of `navigator-core` (`ntex-mqtt` pinned as `=10.0.0`). `ms-mqtt-client` is unchanged.
- One additive function in `crates/navigator-core/src/tls.rs`: `TlsMaterial::ssl_connector()`. It returns the `openssl::ssl::SslConnector` and the `TlsDiag`. Existing behaviour is unchanged. S2 needs it.
- The runtime is the one of the net thread: `tokio` `current_thread` + `LocalSet` + `ntex_net::with_reactor(tokio Reactor)`. There is no ntex `System`. Every ntex timer, task and I/O worked in this set-up.
- Our dialer: DNS in our own `spawn_blocking` that owns the one permit of a one-slot semaphore, `tokio::net::TcpStream::connect` for each resolved address in order, `ntex_net::from_tcp_stream`, then `ntex-tls` OpenSSL when needed. The profile host stays in the request (SNI, verification, WebSocket authority). The same dialer is the connector for MQTT (TCP, TLS) and for `ntex::ws::WsClient` (WS, WSS, subprotocol `mqtt`, frame and message caps 1 MiB). One `tokio::time::timeout` (10 s) covers DNS to CONNACK.
- Both versions run through `start_with_control`. Each handler runs inside `catch_unwind`. A panic calls `force_close()`. Incoming messages go to a bounded queue (256) with `try_send`; the run dropped 0.
- Settings used: write timeout 10 s, `set_handshake_timeout` 10 s, frame read rate (10 s, 60 s, 256), v3 `set_max_size(1 048 572)`, v5 `max_packet_size(1 MiB)`, `max_receive` 16, `max_send` 16.

### Commands run

```bash
tests/broker/up.sh mosquitto
cargo build --release -p navigator-core --example ntex_spike
./target/release/examples/ntex_spike                               # final run: 93 cases (run 6)
SPIKE_FULL=1 SPIKE_ONLY=half ./target/release/examples/ntex_spike  # frame read rate at the plan values
cargo build --release -p navigator-core --example ntex_spike --example flood
cargo tree -e normal -p navigator-core                              # and with `-e normal,dev`
cargo deny check
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

`SPIKE_ONLY=<text>` runs the cases with that text in the name. `SPIKE_LOG=1` prints every `log` record.

### Runs

| Run | Code and plan | Result | Status |
|---|---|---|---|
| 1 | Plan r6 bounds, first checks | 76 cases: 71 PASS, 5 FAIL (WS half frame 5.15 s against 5 s, stalled write 11.01 and 11.02 s against 11 s, v5 outgoing +1 byte sent) | Historical. The numbers in it are not used below. |
| 2 | Plan r7 bounds | 76 cases: 76 PASS | Historical. Codex found that several checks could not fail. |
| 3 | Plan r7 bounds, strict checks, nine more cases | 85 cases: 85 PASS | Historical. Codex round 2 found more checks that could not fail. |
| 4 | Plan r7, round 2 fixes, four more cases | 89 cases: 89 PASS | Historical. Codex round 3 found a loose half-frame bound and missing cases. |
| 5 | Plan r7, round 3 fixes, four more cases | 93 cases: 93 PASS | Historical. Codex round 4 found checks that accepted any error. |
| 6 | Plan r7, round 4 fixes | **93 cases: 93 PASS, 0 FAIL** | Final. Every table and number below comes from run 6. |

Run 6 saved its output in `spike-run6.txt` (scratchpad). The runs at the plan values are `spike-run6-full.txt` (20 cases, 20 PASS) and `spike-run6-trickle-full.txt` (2 cases, 2 PASS).

### What the checks require (run 6)

After the Codex review, each check fails when its named mechanism does not happen:

- Stalled write: a send is pending, ends with "Timed out" in write timeout - 0.3 s to + 1 s, the attempt ends in the bound, and the stop reason is `WriteTimeout`.
- Ping never answered: exactly one PINGREQ at about the keep alive, and the connection lives at least 2 x keep alive.
- Half frame (TCP and TLS): the stop reason is `ReadTimeout`, at the first rate check and not later than one period + 3 s (timer step, shutdown, rounding): 13 s at the plan values.
- Disconnect during a stalled send: a send is pending first, and it ends with `Disconnected`.
- Size: v3 outgoing +1 byte fails with `OverMaxPacketSize`. v5 outgoing +1 byte is sent when the broker announces no limit. v5 with a CONNACK Maximum Packet Size of 1 MiB sends exactly 1 MiB and refuses +1 byte before the send (the fake broker sees one packet). Incoming +1 byte and the non-minimal `r + 1` need the stop reason `MaxSizeExceeded`.
- WebSocket: the client sends exactly one Pong with the Ping payload, ignores an unsolicited Pong, and closes on a Text frame with the Text-frame error.
- Panic: the first handler call is held pending (a gate), the second (detached) call panics after an await. Two calls start, none completes, one panics, and the attempt ends with the panic as stop reason.
- 2 MiB header: the stop reason is `MaxSizeExceeded`, and the variable header is valid (topic `t/b`), so only the size check can close the connection.
- WS and WSS stalled frame: the broker sees exactly one PINGREQ at 1.8 to 2.8 s, the attempt ends in 3.9 to 6.5 s, and the stop reason is `filter shutdown timed out`.
- v3 length helper: both sides of the 1-to-2, 2-to-3 and 3-to-4 byte transitions (limits 129, 130, 131; 16 386, 16 387, 16 388; 2 097 155, 2 097 156, 2 097 157 give r = 127, 127, 128; 16 383, 16 383, 16 384; 2 097 151, 2 097 151, 2 097 152).
- Version errors: the 3.1.1 CONNACK to a v5 CONNECT must give `Protocol(Decode(MalformedPacket))`, and a close before CONNACK must give `Disconnected(None)` (v3 and v5). These are the errors of the real run.
- Stalled DNS: every one of the 10 attempts ends with the connect-deadline error at 0.25 to 0.6 s.
- A disconnect whose attempt task fails (join error) is a FAIL.
- Black-hole dial cancelled: every attempt ends with "Our connect deadline expired" at 0.25 to 0.6 s, then the descriptor count is compared.
- WS upgrade never answered: the WebSocket handshake timeout (ntex default, 5 s) ends it at 4.9 to 5.6 s, before our 10 s deadline, with its own error text.
- Deadline cases: each needs its own error and time range. Silent CONNACK and the black hole: "Our connect deadline expired" at 9.9 to 10.6 s. Stalled TLS: `TLS Handshake timeout` at 4.9 to 5.6 s. TLS set-up errors are no longer discarded.
- v5 outgoing +1 byte with no announced limit: the send returns Ok, and the task ends cleanly (`Ok(())`) with `MaxSizeExceeded` (the echo exceeded our incoming limit). Only the stop reason is claimed.
- Trickling payload: a valid PUBLISH header with a payload that never completes, with more than 256 bytes in each period. The stop reason is `ReadTimeout`, at the cap and not later than the source bound.
- Server Keep Alive 0 with a profile keep alive of 2 s: no PINGREQ and the connection stays open for 7 s (more than 2 x 2 x 1.125 + 1 s).

### Table (run 6)

| Case | Result | Time | Detail |
|---|---|---|---|
| a V3 TCP | PASS | 0.00 s | connect 1.5 ms (CONNACK session_present=false), qos0 sent rtt 0.3 ms, qos1 acked rtt 0.5 ms (rx qos 1) |
| a V3 TCP, user and password | PASS | 0.00 s | connect 1.5 ms (CONNACK session_present=false), qos0 sent rtt 0.3 ms, qos1 acked rtt 0.4 ms (rx qos 1) |
| a V3 TLS (localhost) | PASS | 0.05 s | connect 43.8 ms (CONNACK session_present=false), qos0 sent rtt 0.3 ms, qos1 acked rtt 0.4 ms (rx qos 1) |
| a V3 TLS (127.0.0.1, IP in SAN) | PASS | 0.04 s | connect 42.7 ms (CONNACK session_present=false), qos0 sent rtt 0.3 ms, qos1 acked rtt 0.5 ms (rx qos 1) |
| a V3 mTLS | PASS | 0.05 s | connect 44.8 ms (CONNACK session_present=false), qos0 sent rtt 0.5 ms, qos1 acked rtt 0.5 ms (rx qos 1) |
| a V3 WS | PASS | 0.00 s | connect 1.1 ms (CONNACK session_present=false), qos0 sent rtt 0.2 ms, qos1 acked rtt 0.5 ms (rx qos 1) |
| a V3 WSS | PASS | 0.04 s | connect 42.9 ms (CONNACK session_present=false), qos0 sent rtt 0.4 ms, qos1 acked rtt 0.5 ms (rx qos 1) |
| a V5 TCP | PASS | 0.00 s | connect 0.7 ms (CONNACK Success), qos0 sent rtt 0.2 ms, qos1 acked Success rtt 0.4 ms (rx qos 1) |
| a V5 TCP, user and password | PASS | 0.00 s | connect 1.6 ms (CONNACK Success), qos0 sent rtt 0.4 ms, qos1 acked Success rtt 0.5 ms (rx qos 1) |
| a V5 TLS (localhost) | PASS | 0.04 s | connect 42.9 ms (CONNACK Success), qos0 sent rtt 0.3 ms, qos1 acked Success rtt 0.4 ms (rx qos 1) |
| a V5 TLS (127.0.0.1, IP in SAN) | PASS | 0.04 s | connect 42.6 ms (CONNACK Success), qos0 sent rtt 0.3 ms, qos1 acked Success rtt 0.4 ms (rx qos 1) |
| a V5 mTLS | PASS | 0.05 s | connect 43.4 ms (CONNACK Success), qos0 sent rtt 0.4 ms, qos1 acked Success rtt 0.5 ms (rx qos 1) |
| a V5 WS | PASS | 0.00 s | connect 1.1 ms (CONNACK Success), qos0 sent rtt 0.4 ms, qos1 acked Success rtt 0.5 ms (rx qos 1) |
| a V5 WSS | PASS | 0.05 s | connect 43.5 ms (CONNACK Success), qos0 sent rtt 0.3 ms, qos1 acked Success rtt 0.6 ms (rx qos 1) |
| b V3 TLS expired certificate | PASS | 0.00 s | 1.9 ms: "The server certificate expired on Jan  2 00:00:00 2020 GMT. Renew it, or turn off verification." |
| b V3 TLS wrong host name | PASS | 0.00 s | 1.8 ms: "The server certificate is for another host name." |
| b V3 TLS unknown CA (no CA file) | PASS | 0.00 s | 1.6 ms: "The server certificate is not trusted. Add its CA file, or turn off verification." |
| b V3 mTLS without client certificate | PASS | 0.00 s | io error text: "..tlsv13 alert certificate required..SSL alert number 116". Maps to the client-certificate text. |
| b V5 TLS expired certificate | PASS | 0.00 s | 1.9 ms: "The server certificate expired on Jan  2 00:00:00 2020 GMT. Renew it, or turn off verification." |
| b V5 TLS wrong host name | PASS | 0.00 s | 1.6 ms: "The server certificate is for another host name." |
| b V5 TLS unknown CA (no CA file) | PASS | 0.00 s | 1.8 ms: "The server certificate is not trusted. Add its CA file, or turn off verification." |
| b V5 mTLS without client certificate | PASS | 0.00 s | io error text: "..tlsv13 alert certificate required..SSL alert number 116". Maps to the client-certificate text. |
| b local key texts (encrypted, wrong passphrase, mismatch) | PASS | 0.01 s | encrypted, wrong passphrase, right passphrase, mismatch: texts unchanged |
| c v3 varint boundaries (helper unit check) | PASS | 0.00 s | max_packet->r: 128->126 129->127 130->127 16385->16382 16386->16383 2097153->2097149 2097154->2097150 1048576->1048572 129->127 130->127 131->128 16386->16383 16387->16383 16388->16384 2097155->2097151 2097156->2097151 2097157->2097152 (1 MiB -> 1 048 572) |
| c V3 Tcp exactly 1 MiB packet out and in | PASS | 0.01 s | exact 1 MiB QoS0 out+in 6.8 ms; exact 1 MiB QoS1 out+in 4.5 ms; |
| c V3 Ws exactly 1 MiB packet out and in | PASS | 0.01 s | exact 1 MiB QoS0 out+in 5.8 ms; exact 1 MiB QoS1 out+in 5.6 ms; |
| c V3 Wss exactly 1 MiB packet out and in | PASS | 0.06 s | exact 1 MiB QoS0 out+in 7.7 ms; exact 1 MiB QoS1 out+in 8.1 ms; |
| c V3 1 MiB + 1 byte outgoing (v3 refused, v5 sent as today) | PASS | 0.00 s | refused before the send: Encode(OverMaxPacketSize) |
| c V3 incoming exactly 1 MiB (fake) | PASS | 0.00 s | whole packet of 1048576 bytes delivered in 1.1 ms |
| c V3 incoming 1 MiB + 1 byte (fake) | PASS | 0.00 s | 1048577 bytes (max+1) refused after 0.1 ms: incoming channel closed; stop Protocol(ProtocolError { err: Decode(MaxSizeExceeded { size: 1048573, max_size: 1048572 }) }) |
| c V5 Tcp exactly 1 MiB packet out and in | PASS | 0.01 s | exact 1 MiB QoS0 out+in 3.2 ms; exact 1 MiB QoS1 out+in 2.8 ms; |
| c V5 Ws exactly 1 MiB packet out and in | PASS | 0.01 s | exact 1 MiB QoS0 out+in 3.9 ms; exact 1 MiB QoS1 out+in 4.4 ms; |
| c V5 Wss exactly 1 MiB packet out and in | PASS | 0.10 s | exact 1 MiB QoS0 out+in 49.7 ms; exact 1 MiB QoS1 out+in 6.9 ms; |
| c V5 1 MiB + 1 byte outgoing (v3 refused, v5 sent as today) | PASS | 0.00 s | v5 publish returned "sent" (no announced limit); the connection ended cleanly with stop Protocol(ProtocolError { err: Decode(MaxSizeExceeded { size: 1048577, max_size: 1048576 }) }) |
| c V5 incoming exactly 1 MiB (fake) | PASS | 0.00 s | whole packet of 1048576 bytes delivered in 0.5 ms |
| c V5 incoming 1 MiB + 1 byte (fake) | PASS | 0.00 s | 1048577 bytes (max+1) refused after 0.1 ms: incoming channel closed; stop Protocol(ProtocolError { err: Decode(MaxSizeExceeded { size: 1048577, max_size: 1048576 }) }) |
| c V3 non-minimal length encoding of r passes (fake) | PASS | 0.00 s | r = 1048572, non-minimal length, packet of 1048577 bytes delivered |
| c V3 non-minimal length encoding of r + 1 fails (fake) | PASS | 0.00 s | r + 1 = 1048573, packet of 1048578 bytes refused: incoming channel closed; stop Protocol(ProtocolError { err: Decode(MaxSizeExceeded { size: 1048573, max_size: 1048572 }) }) |
| c V5 CONNACK announces Maximum Packet Size 1 MiB: +1 byte refused before the send (fake) | PASS | 0.30 s | exact send ok; +1 byte: Err("Encode(OverMaxPacketSize)"); the broker saw publish sizes [1048576] |
| d V3 stalled write, write timeout 10 s, keep alive 0 | PASS | 11.13 s | attempt ended 11.03 s after the blocked send began (bound 12.50 s, total 11.03 s); publish ended "Timed out" at 10.00 s; stop: Protocol(ProtocolError { err: WriteTimeout }) |
| d V3 blocked write buffer + missing PINGRESP (ka 2 s) | PASS | 11.12 s | attempt ended 11.02 s after the blocked send began (bound 17.00 s, total 11.02 s); publish ended "Timed out" at 10.00 s; stop: Protocol(ProtocolError { err: WriteTimeout }) |
| d V3 PINGREQ never answered, no publishes (ka 2 s) | PASS | 4.09 s | keep alive 2 s: ended 4.09 s (bound 5.50 s), 1 PINGREQ seen (first at 2.04301655 s), result Ok(()) |
| d V3 PINGREQ never answered, with publishes (ka 2 s) | PASS | 4.09 s | keep alive 2 s: ended 4.09 s (bound 5.50 s), 1 PINGREQ seen (first at 2.046204279 s), result Ok(()) |
| d V3 old keep-alive tasks end (20 connects) | PASS | 7.01 s | records before first tick (1 s): 0, after 3 s: 20 |
| d V3 2 MiB header, 1 MiB limit | PASS | 0.00 s | closed after 0.00 s, RSS 14684 -> 14684 kB, result Ok(()) stop Protocol(ProtocolError { err: Decode(MaxSizeExceeded { size: 2097152, max_size: 1048572 }) }) |
| d V3 incoming QoS 2 never delivered | PASS | 0.10 s | closed after 0.00 s, delivered=false, bytes from client after PUBLISH: [e0 00], eof=true, result Ok(()) stop Error(Error { err: HandlerErr("QoS 2") }) |
| d V3 TCP half fixed header, ka 0, frame read rate | PASS | 2.01 s | ended 2.01 s (bound 5 s, SCALED rate 2 s / 6 s / 256), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V3 TCP half PUBLISH payload, ka 0, frame read rate | PASS | 2.02 s | ended 2.02 s (bound 5 s, SCALED rate 2 s / 6 s / 256), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V3 TLS half fixed header, ka 0, frame read rate | PASS | 3.06 s | ended 3.01 s (bound 5 s, SCALED rate 2 s / 6 s / 256), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V3 TLS half PUBLISH payload, ka 0, frame read rate | PASS | 3.06 s | ended 3.01 s (bound 5 s, SCALED rate 2 s / 6 s / 256), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V3 Tcp disconnect while a half frame waits | PASS | 0.30 s | Tcp: disconnected in 0.00 s (graceful) |
| d V3 Tls disconnect while a half frame waits | PASS | 1.39 s | Tls: disconnected in 1.05 s (graceful) |
| d V3 Ws disconnect while a half frame waits | PASS | 1.41 s | Ws: disconnected in 1.11 s (graceful) |
| d V3 Wss disconnect while a half frame waits | PASS | 1.41 s | Wss: disconnected in 1.06 s (graceful) |
| d V3 WS half binary frame, ka 2 s | PASS | 5.10 s | WS: ended 5.10 s (bound 6.5 s), PINGREQ seen at [2.048150705] s, Stop notice at 5.103383099 s, stop PeerGone(PeerGone(Some(Custom { kind: TimedOut, error: "filter shutdown timed out" }))) |
| d V3 WS upgrade never answered (WebSocket handshake timeout) | PASS | 5.01 s | failed after 5.01 s: Connect(Io(Custom { kind: Other, error: "Timeout" })) |
| d V3 WSS half binary frame, ka 2 s | PASS | 5.10 s | WSS: ended 5.06 s (bound 6.5 s), PINGREQ seen at [2.004554431] s, Stop notice at 5.059111284 s, stop PeerGone(PeerGone(Some(Custom { kind: TimedOut, error: "filter shutdown timed out" }))) |
| d V3 TCP trickling payload (rate met, never completes), ka 0 | PASS | 6.03 s | ended 6.02 s (cap 6 s, bound 11 s, SCALED rate 2 s / 6 s / 256), stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V3 WS fragmented packet, Ping, Pong, Text | PASS | 0.50 s | fragmented packet: delivered; Pong frames from the client: [(10, [112, 105, 110, 103])]; unsolicited Pong ignored: true; Text frame: closed after 0.16 s with PeerGone(PeerGone(Some(Custom { kind: InvalidData, error: "WebSockets Text frames are not supported" }))) |
| d V3 handler panic after await, second call pending | PASS | 0.06 s | attempt ended 0.06 s; handler calls started 2, completed 0, panicked 1; stop Error(Error { err: HandlerErr("panic: spike: handler panic after await") }) |
| d V3 disconnect during a stalled send (force_close) | PASS | 4.23 s | graceful: attempt ended in 1.03 s; the pending send ended Disconnected 1.03 s after the call; force_close: attempt ended in 0.00 s; the pending send ended Disconnected 0.00 s after the call |
| d V5 stalled write, write timeout 10 s, keep alive 0 | PASS | 11.12 s | attempt ended 11.01 s after the blocked send began (bound 12.50 s, total 11.01 s); publish ended "Timed out" at 10.00 s; stop: Protocol(ProtocolError { err: WriteTimeout }) |
| d V5 blocked write buffer + missing PINGRESP (ka 2 s) | PASS | 11.12 s | attempt ended 11.02 s after the blocked send began (bound 17.00 s, total 11.02 s); publish ended "Timed out" at 10.00 s; stop: Protocol(ProtocolError { err: WriteTimeout }) |
| d V5 PINGREQ never answered, no publishes (ka 2 s) | PASS | 4.09 s | keep alive 2 s: ended 4.09 s (bound 5.50 s), 1 PINGREQ seen (first at 2.041903322 s), result Ok(()) |
| d V5 PINGREQ never answered, with publishes (ka 2 s) | PASS | 4.10 s | keep alive 2 s: ended 4.10 s (bound 5.50 s), 1 PINGREQ seen (first at 2.049736973 s), result Ok(()) |
| d V5 old keep-alive tasks end (20 connects) | PASS | 7.01 s | records before first tick (1 s): 0, after 3 s: 20 |
| d V5 2 MiB header, 1 MiB limit | PASS | 0.00 s | closed after 0.00 s, RSS 15344 -> 15344 kB, result Ok(()) stop Protocol(ProtocolError { err: Decode(MaxSizeExceeded { size: 2097157, max_size: 1048576 }) }) |
| d V5 incoming QoS 2 never delivered | PASS | 0.10 s | closed after 0.00 s, delivered=false, bytes from client after PUBLISH: [e0 02 9b 00], eof=true, result Ok(()) stop Error(Error { err: HandlerErr("QoS 2") }) |
| d V5 TCP half fixed header, ka 0, frame read rate | PASS | 2.01 s | ended 2.01 s (bound 5 s, SCALED rate 2 s / 6 s / 256), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V5 TCP half PUBLISH payload, ka 0, frame read rate | PASS | 2.02 s | ended 2.02 s (bound 5 s, SCALED rate 2 s / 6 s / 256), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V5 TLS half fixed header, ka 0, frame read rate | PASS | 3.06 s | ended 3.01 s (bound 5 s, SCALED rate 2 s / 6 s / 256), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V5 TLS half PUBLISH payload, ka 0, frame read rate | PASS | 3.06 s | ended 3.01 s (bound 5 s, SCALED rate 2 s / 6 s / 256), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V5 Tcp disconnect while a half frame waits | PASS | 0.30 s | Tcp: disconnected in 0.00 s (graceful) |
| d V5 Tls disconnect while a half frame waits | PASS | 1.36 s | Tls: disconnected in 1.02 s (graceful) |
| d V5 Ws disconnect while a half frame waits | PASS | 1.41 s | Ws: disconnected in 1.11 s (graceful) |
| d V5 Wss disconnect while a half frame waits | PASS | 1.41 s | Wss: disconnected in 1.06 s (graceful) |
| d V5 WS half binary frame, ka 2 s | PASS | 5.10 s | WS: ended 5.10 s (bound 6.5 s), PINGREQ seen at [2.046941538] s, Stop notice at 5.103361403 s, stop PeerGone(PeerGone(Some(Custom { kind: TimedOut, error: "filter shutdown timed out" }))) |
| d V5 WS upgrade never answered (WebSocket handshake timeout) | PASS | 5.01 s | failed after 5.01 s: Connect(Io(Custom { kind: Other, error: "Timeout" })) |
| d V5 WSS half binary frame, ka 2 s | PASS | 5.10 s | WSS: ended 5.06 s (bound 6.5 s), PINGREQ seen at [2.002973735] s, Stop notice at 5.059314912 s, stop PeerGone(PeerGone(Some(Custom { kind: TimedOut, error: "filter shutdown timed out" }))) |
| d V5 TCP trickling payload (rate met, never completes), ka 0 | PASS | 6.02 s | ended 6.02 s (cap 6 s, bound 11 s, SCALED rate 2 s / 6 s / 256), stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V5 WS fragmented packet, Ping, Pong, Text | PASS | 0.50 s | fragmented packet: delivered; Pong frames from the client: [(10, [112, 105, 110, 103])]; unsolicited Pong ignored: true; Text frame: closed after 0.16 s with PeerGone(PeerGone(Some(Custom { kind: InvalidData, error: "WebSockets Text frames are not supported" }))) |
| d V5 handler panic after await, second call pending | PASS | 0.06 s | attempt ended 0.06 s; handler calls started 2, completed 0, panicked 1; stop Error(Error { err: HandlerErr("panic: spike: handler panic after await") }) |
| d V5 disconnect during a stalled send (force_close) | PASS | 4.23 s | graceful: attempt ended in 1.03 s; the pending send ended Disconnected 1.03 s after the call; force_close: attempt ended in 0.00 s; the pending send ended Disconnected 0.00 s after the call |
| d V5 broker DISCONNECT reason kept | PASS | 0.00 s | ended 0.00 s: broker DISCONNECT Some("Disconnect { reason_code: ServerShuttingDown, session_expiry_interval_secs: None, server_reference: None, reason_string: Some("maintenance"), user_properties: [] }"); stop PeerGone(PeerGone(None)) |
| d V5 Server Keep Alive 5 s followed | PASS | 7.00 s | Server Keep Alive 5 s, profile 60 s: first PINGREQ at 5.078254426 s, connection open true |
| d V5 Server Keep Alive 0 followed (no pings) | PASS | 7.00 s | Server Keep Alive 0 s, profile 2 s: first PINGREQ at None s, connection open true |
| d PINGREQ never answered, keep alive 60 s and 70 s, with and without publishes, v3 and v5 (8 runs together) | PASS | 144.93 s | Eight runs (v3 and v5, with and without our publishes). Each: exactly 1 PINGREQ. Keep alive 60 s: ended 121.38 s (bound 136.00 s), PINGREQ at 60.96 s. Keep alive 70 s: ended 144.93 s (bound 158.50 s), PINGREQ at 71.20 s. |
| d deadlines: silent CONNACK, stalled TLS, black-hole dial | PASS | 10.81 s | Silent CONNACK v3 and v5: 10.00 s, "Our connect deadline expired" (want 9.9 to 10.6 s). Stalled TLS: 5.12 s, `TLS Handshake timeout` (want 4.9 to 5.6 s). Black-hole dial: 10.00 s, "Our connect deadline expired". |
| d refused CONNACK (v3 code 5, v5 0x87 + reason + reference) | PASS | 0.00 s | v3 code 5: `Ack(ConnectAck { return_code: NotAuthorized })`. v5 0x87: `Ack(ConnectAck { reason_code: NotAuthorized, reason_string: Some("not allowed here"), server_reference: Some("other.example:1883"), .. })`. All fields reach the caller. |
| d version errors as raw bytes | PASS | 0.00 s | Required exactly: v5 CONNACK 0x84: `Ack(.. UnsupportedProtocolVersion ..)`. 3.1.1 CONNACK `20 02 00 01` to a v5 CONNECT: `Protocol(Decode(MalformedPacket))`. v3 code 1: `Ack(.. UnacceptableProtocolVersion ..)`. Close before CONNACK: `Disconnected(None)` (v3 and v5). |
| d first address refuses, second works | PASS | 0.00 s | refused on :34169, connected on :18830 in 0.00 s; resolver calls 1 |
| d stalled DNS, 10 cancelled attempts | PASS | 3.21 s | 10 attempts in 3.01 s: lookups started 1, running 1, peak 1; after release running 0; next attempt: Ok("connected") |
| d black-hole dial cancelled leaves no fd | PASS | 5.93 s | Tcp: 5 attempts ended with our deadline at 0.25 to 0.6 s, fds 35 -> 35; Tls: 5 attempts ended with our deadline at 0.25 to 0.6 s, fds 35 -> 35; Ws: 5 attempts ended with our deadline at 0.25 to 0.6 s, fds 35 -> 35; |

### Frame read rate at the plan values (run 6, `SPIKE_FULL=1`)

The table above uses a scaled frame read rate (2 s, 6 s, 256) to save time. This run uses the plan values (10 s, 60 s, 256).

| Case | Result | Time | Detail |
|---|---|---|---|
| d V3 TCP half fixed header, ka 0, frame read rate | PASS | 10.01 s | ended 10.01 s (bound 13 s), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V3 TCP half PUBLISH payload, ka 0, frame read rate | PASS | 10.02 s | ended 10.02 s (bound 13 s), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V3 TLS half fixed header, ka 0, frame read rate | PASS | 11.06 s | ended 11.01 s (bound 13 s), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V3 TLS half PUBLISH payload, ka 0, frame read rate | PASS | 11.06 s | ended 11.01 s (bound 13 s), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V3 Tcp disconnect while a half frame waits | PASS | 0.30 s | Tcp: disconnected in 0.00 s (graceful) |
| d V3 Tls disconnect while a half frame waits | PASS | 1.40 s | Tls: disconnected in 1.05 s (graceful) |
| d V3 Ws disconnect while a half frame waits | PASS | 1.41 s | Ws: disconnected in 1.11 s (graceful) |
| d V3 Wss disconnect while a half frame waits | PASS | 1.41 s | Wss: disconnected in 1.07 s (graceful) |
| d V3 WS half binary frame, ka 2 s | PASS | 5.11 s | WS: ended 5.11 s (bound 6.5 s), PINGREQ seen at [2.049839756] s, Stop notice at 5.10947179 s, stop PeerGone(PeerGone(Some(Custom { kind: TimedOut, ... |
| d V3 WSS half binary frame, ka 2 s | PASS | 5.13 s | WSS: ended 5.08 s (bound 6.5 s), PINGREQ seen at [2.022935925] s, Stop notice at 5.082274996 s, stop PeerGone(PeerGone(Some(Custom { kind: TimedOut... |
| d V5 TCP half fixed header, ka 0, frame read rate | PASS | 10.02 s | ended 10.02 s (bound 13 s), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V5 TCP half PUBLISH payload, ka 0, frame read rate | PASS | 10.01 s | ended 10.01 s (bound 13 s), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V5 TLS half fixed header, ka 0, frame read rate | PASS | 11.06 s | ended 11.01 s (bound 13 s), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V5 TLS half PUBLISH payload, ka 0, frame read rate | PASS | 11.06 s | ended 11.01 s (bound 13 s), result Ok(()) stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V5 Tcp disconnect while a half frame waits | PASS | 0.30 s | Tcp: disconnected in 0.00 s (graceful) |
| d V5 Tls disconnect while a half frame waits | PASS | 1.41 s | Tls: disconnected in 1.07 s (graceful) |
| d V5 Ws disconnect while a half frame waits | PASS | 1.41 s | Ws: disconnected in 1.11 s (graceful) |
| d V5 Wss disconnect while a half frame waits | PASS | 1.41 s | Wss: disconnected in 1.07 s (graceful) |
| d V5 WS half binary frame, ka 2 s | PASS | 5.11 s | WS: ended 5.11 s (bound 6.5 s), PINGREQ seen at [2.04825901] s, Stop notice at 5.106391084 s, stop PeerGone(PeerGone(Some(Custom { kind: TimedOut, ... |
| d V5 WSS half binary frame, ka 2 s | PASS | 5.13 s | WSS: ended 5.08 s (bound 6.5 s), PINGREQ seen at [2.023173638] s, Stop notice at 5.082328468 s, stop PeerGone(PeerGone(Some(Custom { kind: TimedOut... |
| d V3 TCP trickling payload (rate met, never completes), ka 0 | PASS | 60.09 s | ended 60.09 s (cap 60 s, bound 68 s), stop Protocol(ProtocolError { err: ReadTimeout }) |
| d V5 TCP trickling payload (rate met, never completes), ka 0 | PASS | 60.08 s | ended 60.08 s (cap 60 s, bound 68 s), stop Protocol(ProtocolError { err: ReadTimeout }) |

A half frame ends at the first rate check (10 s). TLS adds 1 s for the TLS shutdown. A payload that trickles above the rate and never completes ends at the 60 s cap (60.08 and 60.09 s). Source bound (`ntex-mqtt` `io/mod.rs`, `handle_timeout`): each rate check takes one period from the cap and starts a new timer, so there are 6 checks of at most 11 s each, then 1 s of shutdown and 1 s of rounding: 68 s.

### Scaled values and test set-up

- Frame read rate (10 s, 60 s, 256) was scaled to (2 s, 6 s, 256) in the main run. The run at the plan values used (10 s, 60 s, 256) for the half-frame and trickle cases (the trickle case ended at 60.08 and 60.09 s).
- Every other timer ran at the plan value: write timeout 10 s, connect deadline 10 s, TLS handshake 5 s (ntex default), keep alive 2 s, 60 s and 70 s.
- The black hole is a loopback listener with backlog 1 and a full accept queue. A dial to it hangs (checked in the test before use).
- The stalled-write fake uses a receive buffer of 4096 bytes. The client sends 512 KiB QoS 0 packets until one send blocks.
- The "same text as today" check for TLS compares with the exact strings of `TlsDiag` (`tls.rs`) and with `io_outcome` in `net.rs`. `io_outcome` is private. The example holds a copy of its text branches (`today_text` in `cases.rs`). The check is a mirror, not a call of the real function.
- The eight long keep-alive runs (60 s and 70 s, v3 and v5, with and without our publishes) share the wall clock: they ran together in one case (144.93 s).

### Measured numbers (run 6)

| Item | Value |
|---|---|
| Connect, TCP / WS | 0.7 to 1.6 ms |
| Connect, TLS / mTLS / WSS (with CONNACK) | 42.6 to 44.8 ms |
| Publish round trip (publish, ack, delivery) | 0.2 to 0.6 ms |
| 1 MiB packet, publish to delivery | 2.8 to 49.7 ms (TCP, WS, WSS; the high value is WSS) |
| Stalled write: our publish timer ("Timed out") | 10.00 s |
| Stalled write: attempt ends (`WriteTimeout`) | 11.01 to 11.03 s, with keep alive 0 and with keep alive 2 s running |
| PINGREQ never answered, keep alive 2 s | 4.09 to 4.10 s (bound 5.5 s), with and without our publishes |
| PINGREQ never answered, keep alive 60 s (4 runs) | 121.38 s (bound 136.00 s); PINGREQ at 60.96 s |
| PINGREQ never answered, keep alive 70 s (4 runs) | 144.93 s (bound 158.50 s); PINGREQ at 71.20 s |
| Old keep-alive tasks, 20 connects | 0 records at 1 s, 20 at 3 s (v3 and v5) |
| 2 MiB header, 1 MiB limit (valid variable header) | `MaxSizeExceeded`; the closing time is below the 0.01 s resolution of the log (the check requires under 1 s); RSS 14 684 kB before and after (v3), 15 344 kB before and after (v5) |
| Silent CONNACK / black-hole dial | fail at 10.00 s |
| Stalled TLS | fails at 5.12 s (`TLS Handshake timeout`) |
| WS upgrade never answered | fails at 5.01 s (`Timeout`, kind `Other`) |
| First address refuses | connects on the second address in 0.00 s, one lookup |
| Stalled DNS, 10 cancelled attempts | 1 lookup started, 1 running, peak 1; 0 running after release |
| Black-hole dial cancelled (TCP, TLS, WS, 5 attempts each, all ended with our deadline at 0.25 to 0.6 s) | open fds 35 before and 35 after |
| Handler panic after an await, first call held | attempt ends in 0.06 s; 2 calls started, 0 completed, 1 panicked |
| Disconnect during a stalled send | graceful close: the pending send and the attempt end with `Disconnected` after 1.03 s. `force_close()`: 0.00 s |
| Disconnect while a half frame waits (TCP / TLS / WS / WSS) | 0.00 s / 1.02 to 1.05 s / 1.11 s / 1.06 s |
| v5 Server Keep Alive 5 s (profile 60 s) | first PINGREQ at 5.08 s |
| v5 Server Keep Alive 0 (profile 2 s) | no PINGREQ in 7 s |
| WS / WSS half binary frame, keep alive 2 s (bound 6.5 s) | 5.10 s / 5.06 s; one PINGREQ at 2.00 to 2.05 s |
| Trickling payload at the plan values (cap 60 s, bound 68 s) | 60.08 to 60.09 s (v3 and v5); scaled run: see the table |
| Half frame at the plan values, TCP / TLS (bound 13 s) | 10.01 to 10.02 s / 11.01 s |

### Release size and dependencies

| Item | Value |
|---|---|
| `cargo build --release --example ntex_spike` (stripped, final code) | 7 977 672 bytes (7.61 MiB) |
| `cargo build --release --example flood` (ms-mqtt-client, same tokio and vendored OpenSSL) | 6 001 128 bytes (5.72 MiB) |
| Difference | +1.98 MB (+33 %) |
| Unique crates, `cargo tree -e normal -p navigator-core` | 74 (unchanged: the ntex crates are dev-dependencies) |
| Unique crates with dev-dependencies, before and after the spike | 78, then 182: the spike adds 104 |
| New packages in `Cargo.lock` | 65 |

The example includes the fake brokers and the test code, so the size is an upper bound for the client. The `ntex` crate with `tokio` and `ws` pulls `ntex-h2`, `ntex-server` and `ntex-io-uring` into the graph. The app does not call them. S4 measures the size of the real app. (The crate counts and the `flood` size come from the first measurement; the dependencies did not change after it.)

### `cargo deny check`

Result: advisories ok, bans ok, licenses ok, sources ok. The ntex crates are MIT OR Apache-2.0. One new duplicate-version warning: `rand_chacha` (ntex uses `rand` 0.8). The other duplicate warnings were there before. No change to `deny.toml` is needed.

### Gate

| Command | Result |
|---|---|
| `cargo fmt --all --check` | ok |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | ok |
| `cargo test --workspace` | ok (283 tests passed, 0 failed) |
| `cargo deny check` | ok (see above) |

Broker tests, all three brokers (`tests/broker/up.sh` with no argument, then `cargo test -p navigator-core --features broker-tests --no-fail-fast -- --test-threads=1`, nothing skipped). Saved in `broker-tests3.txt`:

| Test binary | Result |
|---|---|
| unit tests | 130 passed, 1 ignored |
| `broker_compat` (Mosquitto, EMQX, HiveMQ) | 15 passed |
| `broker_matrix` | 15 passed |
| `load_ci` | 1 passed (360.68 s) |
| `net_mosquitto` | 21 passed |
| `soak` | 1 ignored (it needs `--ignored` and a long run) |

An earlier identical run (`broker-tests3-first.txt`) had one failure: `net_mosquitto::a_lost_unsubscribe_is_retried_on_the_resumed_session` ("the removed filter still delivers"). The test passed 3 of 3 times when run alone, and the repeat of the whole command passed all tests. The test and the code under it (`ms-mqtt-client`, `net.rs`) are not changed by S1. I treat it as an existing flaky test; it is not a result of S1, but it is not explained.

After the runs I stopped EMQX and HiveMQ (`docker compose -f tests/broker/compose.yml stop emqx hivemq`). Mosquitto is still running.

### Findings and gaps

1. **WebSocket keep-alive end takes 1 s more than plain TCP.** Keep alive closes the connection at about 4.1 s. The WebSocket filter then waits 1 s for a close reply that never comes (`filter shutdown timed out`, the I/O shutdown timeout, minimum 1 s). Result: 5.10 s. Plan r7 bound: 2 x keep alive x 1.125 + 2 s (6.5 s). Passes.
2. **Write timeout fires up to 2 timer stages late.** The `ntex-io` source (`ops.rs`) says an I/O timer expires after at least `t` and less than `t + 1` s. The dispatcher then waits for the I/O shutdown (one `shutdown_timeout`, 1 s, plus timer rounding). The attempt therefore ends within write timeout + 2 s plus rounding. Plan bound: write timeout + 2.5 s (12.5 s). Measured: 11.01 to 11.03 s, also with keep alive 2 s running. Our own `OP_TIMEOUT` ends the publish at 10.00 s. The measured value does not prove the bound; the source does.
3. **v5 incoming QoS 2: use `close_with_reason`.** Returning `ProtocolMessage::disconnect(pkt)` from the protocol service loses the DISCONNECT packet: the dispatcher closes the io before it writes the packet (first run: no bytes reached the broker). `sink.close_with_reason(Disconnect 0x9B)` and then returning an error works: the broker receives `e0 02 9b 00`. v3: the client sends `e0 00` (DISCONNECT) and closes, with no PUBREC.
4. **Outgoing size.** v3: one byte more than `max_packet` is refused with `OverMaxPacketSize` before the send. v5: the library sends it when the broker announces no Maximum Packet Size (Mosquitto). This is today's rule (`publish::validate_outgoing` with the announced limit, else allowed). The task then ends cleanly with `MaxSizeExceeded`: the broker's echo is over our incoming limit. When the CONNACK announces 1 MiB, exactly 1 MiB is sent and one byte more is refused before the send.
5. **v3 length encoding.** A 4-byte (non-minimal) encoding of `r` = 1 048 572 passes (the packet is 1 byte over `max_packet`). `r + 1` fails with `MaxSizeExceeded`.
6. **WebSocket frames.** A fragmented packet (3 frames) is delivered. A Ping gets one Pong with the same payload. An unsolicited Pong is ignored. A Text frame closes the connection (`WebSockets Text frames are not supported`, `InvalidData`). The behaviour of `ms-mqtt-client` for Text frames was not compared.
7. **mTLS refusal.** With TLS 1.3 the refusal arrives after the handshake as `Disconnected(Some(io::Error))`. Its `Display` text contains `tlsv13 alert certificate required`, so the existing `io_outcome` branch gives the client-certificate text. TLS 1.2 was not tested (Mosquitto negotiates 1.3).
8. **TLS texts.** Expired, wrong host and unknown CA reach `TlsDiag` and give the same texts as today. The error value from ntex carries no typed TLS error, so `TlsDiag` stays the only source.
9. **Stalled TLS.** The `ntex-tls` handshake timeout (5 s) ends it before our 10 s deadline. The error is an I/O error of kind `TimedOut` with the text `TLS Handshake timeout`. `io_outcome` maps `TimedOut` to "The connection timed out.".
10. **Silent CONNACK.** Our 10 s deadline and `set_handshake_timeout(10 s)` expire together. Our deadline reports first.
11. **Graceful close.** A graceful `close()` during a stalled send ends the blocked write and the attempt with `Disconnected` after the I/O shutdown wait (1.03 s). `force_close()` does both in 0.00 s. During a half frame, a graceful `close()` ends the attempt in 0.00 s (TCP), 1.02 to 1.05 s (TLS), 1.11 s (WS) and 1.06 s (WSS). Acknowledgement waits are cleared at once by `clear_queues` (source); the spike did not measure an acknowledgement wait. The plan rule stays: graceful close, wait at most 2 s, then `force_close()`.
12. **Keep alive 70 s and 60 s.** At 70 s the first PINGREQ came at 71.20 s (timer step in the 8 s range) and the connection ended at 144.93 s, inside 2 x 70 x 1.125 + 1 = 158.5 s. At 60 s: 60.96 s and 121.38 s, inside 136 s. These numbers are the same with and without our publishes. A timer-step bound is needed in the user guide: detection can take 2.1 x keep alive.
13. **Keep-alive log test needs a quiet start.** The first run counted 4 records from tasks of earlier cases. The final test waits 4 s first. This agrees with "old tasks end within one interval".
14. **`with_reactor`.** A nested call (a reactor scope already active) panics. Sequential calls are valid (`scoped_tls` restores the previous value). The plan uses one outer scope for the whole net-thread body, by choice.
15. **Dependencies.** ntex with `tokio` + `ws` adds 104 crates to the dev graph and 65 packages to `Cargo.lock`.

### Not run

- A stalled write over TLS or WS (only TCP ran).
- A stalled WebSocket upgrade over WSS (only WS ran; the TLS handshake timeout over WSS has no separate case).
- mTLS over WSS (the Mosquitto config has no such listener).
- A broker that negotiates TLS 1.2 only.
- A trickling payload over TLS (only TCP ran; the cap logic is in the dispatcher, not in the transport).
- An acknowledgement wait cancelled by `close()` (the spike measured a blocked write only).
- The overload test (RSS flat) and the 1000 queued QoS 1 publishes at Receive Maximum 2. S3 and S4 own them.
- An IPv6-only host, and the order of `getaddrinfo` for `localhost` (the first-address case uses a test resolver).

### Verdict: GO

All S1 cases that the plan lists ran, except the items under "Not run", and the final run passes: 93 of 93, with checks that can fail (see "What the checks require"). The plan r7 bounds come from the `ntex-io` source (timer step and I/O shutdown) and from the measured runs. Every robust-connection rule has a working mechanism in our runtime set-up.

Conditions for S2:

1. D6: the v5 QoS 2 path uses `close_with_reason(0x9B)` and then returns an error. Do not return `ProtocolMessage::disconnect`.
2. The WebSocket handshake timeout error is `Connect(Io(Other "Timeout"))`, not kind `TimedOut`. `io_outcome` needs a branch for the text "Timeout" (otherwise it gives "Network error: Timeout").
3. S2 uses one outer `with_reactor` scope for the whole net-thread body (a choice: nested calls panic, sequential calls work).
4. v5 outgoing size follows today's rule (`publish::validate_outgoing` with the announced limit, else allowed). v3 outgoing is limited to `max_packet`.
5. The flaky `a_lost_unsubscribe_is_retried_on_the_resumed_session` (one failure in two full broker runs) needs a look in S2, because S2 changes the code that it tests.
6. The "Not run" items stay open for S3 and S4.

## S2 port of `net.rs` (MQTT 5 only)

Date: 2026-11-15. Branch `feat/ntex-mqtt`, base commit 713fea5. Nothing is committed.

### What changed

- `crates/navigator-core/Cargo.toml`: `ntex-mqtt` (pinned `=10.0.0`), `ntex` (`default-features = false`, `tokio` and `ws`), `ntex-net` (`tokio`), `ntex-tls` (`openssl`) and the small `ntex-*` helper crates are now dependencies. `tokio` gets the `net` feature. `ms-mqtt-client` is a dev-dependency (used by `tests/` and `examples/flood.rs` until S3). The vendor copy and `[patch]` stay.
- `src/net.rs`: supervisor, backoff, retry rules, generations, attempt tokens, bounded queues, drop counter, cancel signal, `SessionSet`, `OP_TIMEOUT` (final) and `CONNECT_TIMEOUT` are as before. The session, the operations and the incoming path use the private `Wire` interface. The net thread body runs inside one `with_reactor` scope. `io_outcome` has a branch for the WebSocket upgrade timeout (kind `Other`, text "Timeout").
- `src/net/wire.rs` (new): `Wire` (one variant, V5; S2b adds V3), `connect` (one 10 s deadline), per-connection `SharedCfg` (write timeout 10 s, handshake timeout 10 s, frame read rate 10 s / 60 s / 256, WS frame and message caps = `max_packet`, subprotocol `mqtt`, v5 `max_packet_size` = `max_packet`, `topic_alias_max` 16, Receive Maximum 16), `start_with_control`, `catch_unwind` around the protocol service, payload collection, QoS 2 refusal (`close_with_reason` 0x9B, then an error), error mapping to `Outcome`.
- `src/net/dial.rs` (new): our dialer (DNS in our `spawn_blocking` with a one-slot semaphore, `tokio` TCP for each address, `from_tcp_stream`, `ntex-tls`). The same dialer serves `WsClient`.
- `src/tls.rs`: `TlsMaterial::connector` and `tls::build` return `(openssl::ssl::SslConnector, TlsDiag)`. The S1 function `ssl_connector` is removed (the spike now calls `connector`). `TlsDiag` and all texts are unchanged. Callers: `net.rs`, the spike (`examples/ntex_spike/cases.rs`); `grep -rn` found no other.
- `tests/net_mosquitto.rs`: the file is behind the `broker-tests` feature. A missing broker now fails the test (`require_broker`, `expect`). It was an early return in 15 tests.
- `tests/broker/up.sh`: checks a real MQTT connect on the Mosquitto listeners 18830 to 18835 (plain, user and password, TLS with the test CA and host name, mTLS with the client certificate, WS, WSS). The probe is bash plus `openssl s_client`. I checked that it fails for: mTLS without a client certificate, the auth listener with an anonymous CONNECT, a closed port, and a TCP port used as WebSocket.
- `.github/workflows/ci.yml`: the broker job also runs `--test net_mosquitto` (the file is no longer run by `cargo test --workspace`).
- `git diff crates/navigator-app` is empty.

### Commands run (final code)

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo deny check
cargo test --workspace
tests/broker/up.sh mosquitto        # six listeners, real connect
<net_mosquitto binary> x30          # one run after another, nothing else running
tests/broker/up.sh                  # all three brokers
cargo test -p navigator-core --features broker-tests --no-fail-fast -- --test-threads=1   # 3 times
docker compose -f tests/broker/compose.yml stop emqx hivemq
cargo run --release -p navigator-core --example ntex_spike
```

### Results

| Check | Result |
|---|---|
| `cargo fmt --all --check` | ok |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | ok |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok |
| `cargo test --workspace` | 271 passed, 0 failed, 1 ignored (139 unit tests in `navigator-core`) |
| `tests/broker/up.sh` | all six Mosquitto listeners, EMQX and HiveMQ answered CONNACK |
| Full broker suite, run 1, 2, 3 | each run: 139 unit, 15 `broker_compat`, 15 `broker_matrix`, 1 `load_ci` (about 361 s), 21 `net_mosquitto`; 0 failed; `soak` 1 ignored. Same command as S1 (`--test-threads=1`). |
| `net_mosquitto` alone, 30 runs | 30 passed, 0 failed |
| `ntex_spike` (release) | 93 cases, 93 passed, 0 failed |

The run of the whole suite does not skip anything. `net_mosquitto` now fails when no broker is there.

New unit tests (each was made to fail once, or has an exact expected value): `protocol_errors_have_their_own_outcome`, `a_missing_pingresp_ends_the_attempt_with_the_general_text` (bound 2 x keep alive x 1.125 + 1 s), `a_stalled_websocket_upgrade_is_a_temporary_timeout` (fails without the new `io_outcome` branch: checked by removing it), `a_handler_panic_after_an_await_fails_the_attempt` (first call never ends, second panics; fails without `catch_unwind`: checked by removing it), `tests/net_blocked_write.rs::disconnect_during_a_blocked_write_is_prompt` (production `OP_TIMEOUT`; about 20 MB of QoS 0 publishes to a broker that does not read; the test checks that some sends are still open at the disconnect, no result is `TimedOut`, the state never was `Failed`, and Disconnected comes in under 3 s), `the_next_address_is_tried_when_the_first_refuses`, `cancelled_attempts_keep_one_lookup`, `websocket_errors_become_io_errors`, `a_will_topic_with_a_wildcard_is_not_valid`. The first version of the blocked-write test was a unit test. Its disconnect (after 500 ms) came after the unit-test `OP_TIMEOUT` (300 ms), so it could pass after an operation timeout. It moved to an integration test with the production timeout. The test does not prove the `force_close` branch: the I/O shutdown wait (1 s) already ends the blocked write.

### Changed expectations in existing tests

| Test | Change | Plan line |
|---|---|---|
| `net::tests::qos2_publish_fails_the_session_without_retry_or_panic` | DISCONNECT packet `e0 01 82` (reason 0x82) became `e0 02 9b 00` (0x9B, QoS not supported). `QOS2_TEXT` has a new value: "The broker sent a QoS 2 message. This app supports QoS 0 and 1." The check of the packet moved from the fake-broker thread (where a failed `assert_eq!` did not fail the test) into the test body, so it is stronger. | Facts table, row "Incoming QoS 2 goes to the handler" (line 33); S1 finding 3 |
| `net_mosquitto.rs` (15 tests) | `if !broker_up() { return; }` became `require_broker()`, which panics. The file is behind `#![cfg(feature = "broker-tests")]`, so `cargo test --workspace` without a broker still works. | Steps, item 3 (line 128); S2 extras |
| `net_mosquitto::a_lost_unsubscribe_is_retried_on_the_resumed_session` | The test now waits for `Reconnecting`, then for `Connected` (before: only `Connected`). The assertion, the sleep and the timeouts are the same. See "Flaky test" below. No plan line: this is a race in the test set-up. | none |
| `net::tests::stale_attempt_results_are_dropped` | Rewritten around `Ingest::queue(|generation| Incoming)` instead of the old `Publish` type and `local_limits::dropped()`. The checks (stale token queues nothing and counts no drop, live token queues) are the same. | Mapping row `local_limits` (line 116) |
| `net::tests::temporary_and_final_reasons_are_sorted` | Enum types of ntex instead of `ms-mqtt-client`. The two lists of reasons are the same. | D8 (line 71) |
| `net::tests::io_errors_are_sorted` | Added cases only: `Other "Timeout"` is temporary; `Other "Timeout while ..."` is final. | S1 condition 2 |
| `tls::tests` | `.err().expect(..)` became `.expect_err(..)` (clippy `err_expect`, because `build` now returns a type that has `Debug`). Same checks. | none |
| `reconnects_after_drop_and_resubscribes_live_filters` | No change to the test. The observed text of a plain close is the general text (it contains "closed"). | Facts table, row "Keep alive" (line 30) |

No test timeout was changed. No assertion was removed.

### Flaky test: `a_lost_unsubscribe_is_retried_on_the_resumed_session`

- Result: with the first port (before the fix of the test) the 30 runs of `net_mosquitto` alone gave 26 passed and 4 failed. All 4 failures were this test ("the removed filter still delivers"). After the fix: 30 of 30, and 3 of 3 full suite runs.
- Cause (found with debug prints that are removed now, and a time stamp for each line): the test cuts the proxy and sends the UNSUBSCRIBE at once, then waits for `Connected`. The error of the lost UNSUBSCRIBE reaches the UI before the supervisor has moved the status from `Connected` to `Reconnecting`. `wait_for(Connected)` is true at once, so the test sleeps 500 ms and publishes while the client is still in its backoff. The broker still holds the filter (the UNSUBSCRIBE was lost), so it queues the message and delivers it at the next CONNACK, before our stale UNSUBSCRIBE is answered. This is correct behaviour; the test published at the wrong time.
- Old client versus new: the old client reported the failed operation later, after the status change, so the race was rare (it showed in 2 of the full-suite runs of S1). ntex fails the operation at once when the I/O closes, so the race is now frequent. The product code is not at fault: in every failing run the stale UNSUBSCRIBE was sent after the new CONNACK with `session present` true.

### Deviations from the plan, and open points

1. `net_mosquitto.rs` is behind the `broker-tests` feature, and `.github/workflows/ci.yml` runs it in the broker job. The plan says only "must fail, not return early". Without the feature gate, the normal CI job (no broker) would fail.
2. `tls::TlsMaterial::ssl_connector` (S1) is merged into `connector`. `tls::build` and `connector` return `SslConnector`.
3. A plain close of an established connection has the general text "The connection closed (the broker did not answer, or the network failed)." ntex reports a keep-alive death, a broker close and a lost network in the same way (`Stop(PeerGone(None))`; checked with a debug print in the keep-alive test), so one text must cover all three. The old text "The broker closed the connection." stays for a close before the CONNACK and for a reset (I/O error). Keep alive is `Retry`.
4. New texts (new causes): write timeout "The broker did not read our data for 10 s."; read timeout "The broker stopped in the middle of a packet."; a publish that the codec refuses by size: "The broker does not accept a message of this size.". 
5. The version texts of the plan (CONNACK 0x84, a reply that is not MQTT 5, close before CONNACK with "Check ... the Protocol") are not in S2: they point to a Protocol select that does not exist before S2c. The existing texts stay ("the broker does not support MQTT 5").
6. A timeout of our 10 s deadline has two texts, as before: "The connection timed out." before the transport is up, and "The broker did not answer the connection request." after TCP and TLS are up.
7. The user guide (`docs/user-guide.md`, line 395) still lists "The broker did not answer the keep-alive ping." S5 updates the user guide. The log filter `ms_mqtt_client=off` is in `navigator-app` (`logging.rs`); the change is S5 (the app must not change in S2).
8. The test lock for the old `local_limits` is still in `net_mosquitto.rs`, `broker_matrix.rs`, `broker_compat.rs`. `Net` does not set global limits any more, but the old test publishers still link the old client. S3 removes it.
9. Not done in S2 (S3 owns them): the fd-count test and the stalled-lookup test in the `tests/` set (the dialer unit tests cover the first-address and one-lookup rules), the frame-read-rate cases, the overload test, the 1000-publish Receive Maximum 2 test, topic alias set, reuse and invalid alias.

### Codex round 1: six defects fixed

1. `net_mosquitto.rs` used 127.0.0.1:1883 (the owner's own Mosquitto). Every use (readiness, profiles, publishers, proxy upstream) now points to the compose anonymous listener 18830. The doc comment says so. The CI edit stays.
2. `Disconnected(None)` from the WebSocket connector (the peer closes before the upgrade answer) is now an `UnexpectedEof` I/O error: temporary, "The broker closed the connection.". Tests: `a_close_before_the_upgrade_answer_is_retried` (fake server; checked: without the fix the state is `Failed("Network error: Connector has been disconnected: None")`) and `a_disconnect_before_the_upgrade_answer_is_a_temporary_close`.
3. The unit test of the blocked write is replaced by `tests/net_blocked_write.rs` (see above). The unit version could not prove its claim.
4. SUBACK and UNSUBACK refusals keep the broker reason string (cleaned with `display_safe`): "<reason text> Broker reason: <string>" and "Unsubscribe rejected: <Reason>: <string>". Test: `suback_and_unsuback_keep_the_reason_string` (distinct strings, a direction control, and the cases without a string).
5. The detailed invalid-filter reasons are back for subscribe, unsubscribe and the route check: "Invalid topic filter: empty filter / bad string: ... / bad filter ...". `filter_error` follows the rules of the old client. Test: `filter_errors_are_the_texts_of_the_old_client` compares 27 filters at that time (empty, too long, NUL, control, non-character, wildcards, `$share` forms) with the old `TopicFilter::new` message. Residual: a code point that is unassigned but not a non-character is not detected (the old client used Unicode tables).
6. `up.sh` also probes Mosquitto 18836 and 18837 (without verification: the certificate is bad on purpose), EMQX WebSocket 18841 and HiveMQ WebSocket 18851 with a real CONNECT (12 listeners in all).

Runs after the fixes (logs `s2r1-*.txt` in the scratchpad):

| Check | Result |
|---|---|
| fmt, clippy `--all-features -D warnings` | ok |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok |
| `cargo test --workspace` | 275 passed, 0 failed, 1 ignored |
| `tests/broker/up.sh` (all three brokers) | all 12 listeners answered CONNACK (Mosquitto 18830 to 18837, EMQX 18840 and 18841, HiveMQ 18850 and 18851) |
| Full broker suite, once, `--no-fail-fast -- --test-threads=1` | 142 unit, 15 `broker_compat`, 15 `broker_matrix`, 1 `load_ci`, 1 `net_blocked_write`, 21 `net_mosquitto`; 0 failed; `soak` 1 ignored |
| `net_mosquitto` alone, 10 runs (compose Mosquitto on 18830) | 10 passed, 0 failed |

EMQX and HiveMQ are stopped. `git diff crates/navigator-app` is empty.

### Codex round 2: three defects fixed

1. IPv6 TLS name: the dialer now strips the brackets of an IPv6 address before `tls.connect` (SNI and verification). The URL keeps them. Test `tls_and_wss_verify_an_ipv6_address_without_brackets`: a TLS server and a WSS server on `[::1]` with a certificate that has an IP entry for `::1` (the test certificate helper now adds an IP entry for an IP address). The attempt must be `Reconnecting` and the server must see a good handshake. Without the fix the state is `Failed("The server certificate is for another host name.")` for WSS (checked). Plain TLS already passed the name without brackets; the test covers it too.
2. One shared validator (`topic::topic_error`, `topic::filter_error`, and a string check) now serves publish topics, response topics, Will topics, and subscribe and unsubscribe filters. It has the rules and texts of the old client, including controls and unassigned code points (general category table of `unicode-properties`, the crate the old client used). `unicode-properties` 0.1.5 was already in `Cargo.lock`; it is now a normal dependency of `navigator-core` (MIT OR Apache-2.0, `cargo deny` ok). The publish path gives "Invalid topic: ..." and "Invalid response topic: ..." as at HEAD. Parity tests against the old client: `topic_errors_are_the_texts_of_the_old_client` (19 topics) and `filter_errors_are_the_texts_of_the_old_client` (34 filters), with NUL, controls, non-characters, unassigned code points, private use, wildcards, empty and too long.
3. The listener count is 12, not 14 (8 Mosquitto, 2 EMQX, 2 HiveMQ). Corrected above.

Runs after the fixes (`s2r2-*.txt`): fmt ok; clippy `--all-features -D warnings` ok; `cargo deny check` ok; `cargo test --workspace` 277 passed, 0 failed, 1 ignored; `net_mosquitto` 21 passed and `net_blocked_write` 1 passed against Mosquitto on 18830.

## S2b V3 adapter (MQTT 3.1.1)

Date: 2026-11-15. Branch `feat/ntex-mqtt`, base commit d150889. Nothing is committed. Logs: `s2b-*.txt` in the scratchpad (`ntex/`).

### What changed

- `net.rs`: new `Protocol` (`V311`, `V5`) and field `ConnectConfig::protocol`. `ConnectConfig::from(Profile)` gives `V5`, so the app keeps MQTT 5 until S2c. New `session_persists`: v5 with a session expiry, v3 with Clean Session off (D3). It replaces `session_expiry_secs > 0` in `AttemptCtx`.
- `net/wire.rs`:
  - `Wire::V3(sink, max_packet)`, `connect_v3`, `HandlerV3`, `publish_v3`, `subscribe_v3`, `unsubscribe_v3`. The v3 CONNECT uses the profile `clean_start` as Clean Session. A v3 password is sent only with a user name.
  - The transport (our dialer, WS upgrade) is one function, `transport`. V3 and V5 use it. Timers, write timeout, frame read rate, `start_with_control`, `catch_unwind` (`contained`) and the disconnect path are shared; the v3 handler and the v5 handler call the same `contained`.
  - `v3_max_remaining`: one function for the largest `r` with `1 + varint_len(r) + r <= max_packet`. `shared_cfg` calls `set_max_size(r)` for v3. The v3 publish check uses the same `r` and gives "MQTT 3.1.1 messages can be at most 1 MiB in this app." before the send.
  - v3 CONNACK return codes 1 to 5 have plain texts. Only code 3 is temporary.
  - v5 version errors: 0x84, an undecodable CONNACK (`Decode` error before the CONNACK) and a close before the CONNACK have the texts of the plan.
  - v3 QoS 2: the handler returns an error and sets the QoS 2 flag. The session ends with `QOS2_TEXT`.
  - A v3 protocol error says "not valid MQTT 3.1.1". The v5 text is unchanged.
  - A v3 Will message over 65 535 bytes fails the connect with a text (the CONNECT field has a 16-bit length). A v3 Will has no delay and no properties.
- Tests: `net.rs` (unit tests with a fake broker), `tests/broker_matrix.rs`, `tests/net_mosquitto.rs`, `tests/broker_compat.rs`. `git diff crates/navigator-app` is empty.

### Tests added

Unit tests (`net.rs`; each was checked by breaking the code, see below):
`the_largest_remaining_length_is_exact_at_the_varint_boundaries` (hand values at 127, 16 383 and 2 097 151 and a brute-force comparison), `a_session_persists_by_version`, `the_v3_connect_packet_has_level_4_and_the_clean_session_flag`, `v3_connack_return_codes_have_plain_text` (codes 1 to 5, exact state and text), `v5_connack_0x84_names_the_protocol_setting`, `a_connack_that_v5_cannot_decode_says_the_reply_is_not_mqtt_5` (raw bytes `20 02 00 01`), `a_close_before_the_connack_does_not_claim_a_version` (v3 and v5), `v3_suback_codes_become_results`, `v3_unsuback_means_done`, `v3_publish_results` (Sent, Acknowledged, Lost on disconnect, Timed out), `v3_qos2_closes_without_pubrec` (the only packet from the client is the DISCONNECT 0xE0 of the close; no PUBREC), `a_v3_handler_panic_after_an_await_fails_the_attempt`, `an_incoming_packet_of_max_packet_passes_and_one_more_byte_fails` (v3 and v5, whole packet of exactly 1024 and 1025 bytes), `v3_non_minimal_length_encoding_is_checked_by_remaining_length`, `a_v3_publish_of_max_packet_is_sent_and_one_more_byte_is_refused` (1 MiB passes; +1 is refused with the exact text; the broker sees only the DISCONNECT).

Broker tests against the compose brokers:

- Mosquitto 18830 (`net_mosquitto.rs`): `v3_subscribe_publish_receive_and_disconnect`, `v3_persistent_session_resumes_with_the_queued_message`, `v3_clean_session_keeps_nothing`, `v3_a_packet_of_one_mib_round_trips_and_one_byte_more_is_refused`, `v3_an_incoming_packet_over_the_limit_ends_the_session`, `v3_a_removal_while_offline_reaches_a_resumed_session`. The last one shares its body with the V5 test (`removal_while_offline(protocol)`); the V5 test calls it with `V5` and has the same assertions.
- `broker_matrix.rs`: v3 round trip over TCP, password TCP, TLS, mTLS, WS and WSS, and a v3 wrong-password test.
- `broker_compat.rs`: for Mosquitto, EMQX and HiveMQ: v3 TCP and WS round trip, v3 retained delivery and clear, v3 persistent session with a queued message.

Mutation checks (each made a test fail, then I restored the code): no `Decode` arm for v5 (the not-MQTT-5 test failed); no v3 publish size check (the outgoing test failed); no `set_max_size` for v3 (both incoming v3 tests failed); `Failure` text changed (SUBACK test failed); no QoS 2 branch in `HandlerV3` (QoS 2 test failed); `session_persists` always false for v3 and Clean Session always on (the persistent session test failed in both cases).

### Gate (final code)

| Check | Result |
|---|---|
| `cargo fmt --all --check` | ok (`s2b-fmt.txt`) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | ok (`s2b-clippy.txt`) |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok (`s2b-deny.txt`) |
| `cargo test --workspace` | 292 passed, 0 failed, 1 ignored (159 unit tests in `navigator-core`) (`s2b-test.txt`) |
| `tests/broker/up.sh` | all 12 listeners of the three brokers answered CONNACK (`s2b-up.txt`) |
| Full broker suite, run 1 and 2 (`--no-fail-fast -- --test-threads=1`) | each run: 159 unit, 24 `broker_compat`, 22 `broker_matrix`, 1 `load_ci` (about 361 s), 1 `net_blocked_write`, 27 `net_mosquitto`; 0 failed; `soak` 1 ignored (`s2b-full-1.txt`, `s2b-full-2.txt`) |
| `net_mosquitto` alone, 10 runs | 10 runs of 27 passed, 0 failed (`s2b-mosquitto10.txt`). I ran the test binary with `--test-threads=4`; every test takes the `LIMITS` lock, so the tests still run one after another. |

EMQX and HiveMQ are stopped. Mosquitto still runs.

### Changed expectations in existing tests

None of the assertions of an existing test changed. The old text "the broker does not support MQTT 5" had no test. Two pre-existing tests that run the same code were not changed. The `removal_while_offline` split keeps the assertions and the timing of `a_removal_while_offline_reaches_a_resumed_session`. The broker_compat and broker_matrix helpers got a `protocol` parameter with a wrapper that keeps the old signature and V5.

New texts (plan "Version errors", plan lines 100-105 of the "MQTT 3.1.1 in the app" section; the S2 report deviation 5 deferred them):

- v5 CONNACK 0x84: "This broker does not support MQTT 5. Set Protocol to MQTT 3.1.1."
- v5 undecodable CONNACK: "The broker sent a reply that is not MQTT 5. If the broker supports only MQTT 3.1.1, set Protocol to MQTT 3.1.1."
- A close before the CONNACK (both versions): "The broker closed the connection before it accepted it. Check the address, the TLS settings and the Protocol."
- v3 code 1: "This broker does not support MQTT 3.1.1. Set Protocol to MQTT 5."

### Deviations and open points

1. A v3 close with a QoS 2 message sends a DISCONNECT (0xE0) before the close. The plan says "close without PUBREC". There is no PUBREC. The test pins the DISCONNECT.
2. Mosquitto answers a wrong password with return code 5 ("not authorized"), not 4. The broker test expects that; code 4 has a fake-broker test.
3. The transport for TCP, TLS, WS and WSS is now one boxed closure (`ConnFuture`), shared by both versions. It adds one boxed future for each connect. This is not on a hot path.
4. Not tested for v3 (the code is shared with v5 and S2 tested it for v5): the write timeout, keep alive, frame read rate, and the disconnect with `force_close`. The v3 stalled-write and missing-PINGRESP cases from the S1 spike are in the spike only. S3 moves the fake-broker cases to `tests/`.
5. The plan asks for the session-filter tests (resume, offline removal, lost UNSUBACK, app restart) on both versions. S2b runs the offline removal and the resume on v3. The lost UNSUBACK and the app restart cases are not run on v3. S3 or S2c can add them.
6. The v3 Will has no properties and no delay. The profile `will.delay_secs` is ignored for v3. S2c hides it in the UI.
7. `broker_max_packet` for v3 is `None`, as the CONNACK has no limit.
8. The v3 TLS cases with bad certificates, the stalled WS upgrade and other transport cases are the same code as V5 and were not repeated for v3.

### S2b Codex round 1: three defects fixed

1. The v3 panic test sent one PUBLISH, so the first call ran in the attempt task and the test passed without `contained()`. `HandlerV3` now has the test-only `__handler_hold__` branch of V5. The test sends a held call and then a call that panics after an await. It checks `Failed(PANIC_TEXT)` (bounded by the 5 s status wait), one accept, and that a new connect reaches `Connected`. Checked: with `contained()` removed from `HandlerV3::call`, the test times out. The V5 test already used the hold and panic pair: no change.
2. An empty client ID with Clean Session off is refused in `connect_v3` before the dial: `Failed("MQTT 3.1.1 needs a client ID when Clean session is off. Enter a client ID, or turn on Clean session.")`. Test `v3_empty_client_id_needs_clean_session` (also checks that the broker got no connection). Checked: without the check the state is `Failed("The broker sent data that is not valid MQTT 3.1.1.")`.
3. `Disconnected(Some(io))` with kind EOF, reset, abort or broken pipe during the handshake now gives the close-before-CONNACK text (temporary), unless a TLS diagnosis exists. The test `a_close_before_the_connack_does_not_claim_a_version` covers a clean close and a reset (the fake closes with unread data, which sends a RST) on v3 and v5. Checked: without the mapping the reset case gives "The broker closed the connection.".

Runs (`s2br1-*.txt`): fmt ok; clippy `--all-features -D warnings` ok; `cargo deny check` ok; `cargo test --workspace` 293 passed, 0 failed, 1 ignored (161 unit tests); `net_mosquitto` once against Mosquitto 18830: 27 passed. `git diff crates/navigator-app` is empty. Not rerun: the full broker suite (the change touches the connect error path and one test-only branch).

## S2c Profile and app

Date: 2026-11-15. Branch `feat/ntex-mqtt`, base commit d704988. Nothing is committed. Logs: `s2c-*.txt` in the scratchpad (`ntex/`).

### What changed

- `navigator-core`:
  - `Protocol` has serde (`"v311"`, `"v5"`). `Profile.protocol` is new. `Profile::new()` and `Profile::default()` give `V311` (D7). A profile JSON without the field gives `V5` (field-level serde default), for a settings file and for an import.
  - Settings `CURRENT_SCHEMA` is 2. Migration `v1_to_v2` writes `"protocol": "v5"` into each profile that has none. The profile export (version 1 of the export format) includes the field. An export without the field imports as `V5`.
  - `From<Profile> for ConnectConfig` passes `profile.protocol` (S2b hard-coded `V5`).
  - `publish.rs` now holds the v3 size rules: `v3_max_remaining`, `v3_remaining`, `size_text` and the new `validate_v3_size`. They moved from `net/wire.rs` without change. `wire.rs` calls `validate_v3_size`, so the text "MQTT 3.1.1 messages can be at most 1 MiB in this app." exists once.
- `navigator-app`:
  - Connection sheet: a "Protocol" select (MQTT 3.1.1, MQTT 5) after Transport. For 3.1.1 the sheet shows "Clean session" instead of "Clean start" and hides "Session expiry" and "Will delay". A hidden field is not read: the profile keeps its saved value. The app has no subscription options yet (S2d).
  - Clean session mapping (D3): the profile field `clean_start` is the CONNECT Clean Session flag in 3.1.1 (the same bit). Clean Session off means a persistent session. `session_expiry_secs` is ignored for 3.1.1 and keeps its value in the profile.
  - `Navigator.protocol` is the version of the latest Connect (set in `send_connect`). The composer and the inspector follow it.
  - Composer, 3.1.1: no "Properties" section. Send drops properties from the draft before the check, so hidden fields cannot block a send. Before the send, the composer checks `validate_v3_size` with the app limit (the same check as v5 with the broker Maximum Packet Size). "Edit and publish" and "Reply" copy the draft, drop the properties and show the notification "MQTT 3.1.1 does not send properties." only when the draft had a property.
  - Inspector, 3.1.1: a message with no properties shows "No properties (MQTT 3.1.1)."
  - Subscription row: "granted N" became "Granted QoS N". Refusal texts ("Refused (0x80)") come from core.
  - `--mqtt 3|5`, default 3. Help text and `docs/user-guide.md` list it.
- Docs: `docs/user-guide.md` (Protocol field, "MQTT 3.1.1 connections" section, `--mqtt`, "Granted QoS"; the keep-alive text at line 395 is untouched), `docs/ui-design.md` (two lines).

### Tests added

`profile::tests`: `a_new_profile_uses_mqtt_311_and_an_old_one_mqtt_5`, `the_connect_config_takes_the_protocol_of_the_profile`, `export_has_the_protocol_and_an_import_without_it_gets_mqtt_5`. `settings::tests`: `a_schema_1_file_loads_its_profiles_as_mqtt_5`, `the_protocol_survives_a_save_and_a_load`. `publish::tests::the_v3_size_rule_refuses_one_byte_over_the_limit`. App: `mqtt_3_is_the_default_and_mqtt_5_is_an_option` (`main.rs`), `an_mqtt_311_edit_keeps_topic_payload_qos_and_retain_and_drops_the_properties` (`composer.rs`), `mqtt_311_shows_clean_session_and_hides_the_mqtt_5_session_fields` (`connections.rs`), `a_suback_shows_the_granted_qos` (`messages.rs`).

Mutation checks: the field default changed to `V311` made `a_new_profile_uses_mqtt_311_and_an_old_one_mqtt_5` and `export_has_the_protocol_and_an_import_without_it_gets_mqtt_5` fail. `From<Profile>` with a hard-coded `V5` made `the_connect_config_takes_the_protocol_of_the_profile` fail. I restored both. Not checked by mutation: the migration function (the field default gives the same result; the function is needed so that `MIGRATIONS[version - 1..]` does not panic at schema 2).

### Changed expectations (plan lines)

| Test or text | Change | Plan line |
|---|---|---|
| All broker tests and net unit tests that build `Profile { .., ..Profile::default() }` (`net.rs`, `broker_compat.rs`, `broker_matrix.rs`, `net_mosquitto.rs`, `net_blocked_write.rs`, `load_ci.rs`, `soak.rs`) | The profile now sets `protocol: Protocol::V5`. No assertion changed. | S2: "Existing tests that build a profile pin it to V5 before S2c changes the default" (line 142) |
| `settings::tests::round_trip_and_defaults` | `schema_version` after the load of an old file: 1 became 2. | "Settings schema_version goes from 1 to 2" (line 94) |
| Subscription row text | "granted N" became "Granted QoS N" (no test pinned the old text; the new text has a test). `docs/ui-design.md` and the user guide changed with it. | "SUBACK shows Granted QoS n" (line 99) |

### Gate

| Check | Result |
|---|---|
| `cargo fmt --all --check` | ok (`s2c-fmt.txt`) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | ok (`s2c-clippy.txt`) |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok (`s2c-deny.txt`) |
| `cargo test --workspace` | 303 passed, 0 failed, 1 ignored (`s2c-test.txt`) |
| `tests/broker/up.sh mosquitto` | all Mosquitto listeners answered (`s2c-up.txt`) |
| Broker suite, `--no-fail-fast -- --test-threads=1` (`s2c-broker.txt`) | 166 unit, 22 `broker_matrix`, 1 `load_ci`, 1 `net_blocked_write`, 27 `net_mosquitto` passed. `broker_compat`: 8 passed (Mosquitto), 16 failed (all `emqx::` and `hivemq::` tests: those brokers were not started, so "connect failed" / "not connected"). Those brokers were not started in the first run. See "S2c Codex round 1" for the rerun. |
| `cargo build --release -p navigator-app` | ok |
| `--help` | prints the new usage (`s2c-help.txt`); the exit code is 2 |
| Short run `--mqtt 3 --tcp --host 127.0.0.1 --port 18830`, then `--mqtt 5` | the app ran until the 8 s timeout (exit 124) for both, with no output on stderr (`s2c-run3.txt`, `s2c-run5.txt`). No screenshot: no screenshot tool is available here. |

Mosquitto still runs. EMQX and HiveMQ were not started.

### Deviations and open points

1. No UI-state tests with `#[gpui_kit::test]`: the codebase has none. The hide and show rules are pure functions with unit tests. The sheet, composer and inspector rendering was built and launched but not seen.
2. The QoS 1 timeout text stays "No acknowledgement after 10 s." (existing text, same for both versions). The plan's "Timed out" is the name of the result.
3. The inspector cannot tell the version of a stored message (no per-message version). It uses the version of the latest Connect and shows "No properties (MQTT 3.1.1)." only when the message has no properties. A store that keeps messages across a version change can mix.
4. `publish.rs` has no per-version `encoded_size`. The v5 `encoded_size` stays. The v3 rule is `validate_v3_size` (Remaining Length against `v3_max_remaining`), the same rule as the net thread. `Net::send` still checks only the v5 rules; the 3.1.1 limit is checked in the composer and again on the net thread.
5. The sheet does not check "Clean session off with an empty client ID" (core refuses it at connect with its own text, S2b).
6. The lost-UNSUBACK and app-restart session tests on v3 (S2b open point 5) are not added.
7. Edit/Reply notification shows only when the draft had a property.

### S2c Codex round 1

Fixes (logs `s2cr1-*.txt`; fmt ok, clippy `--all-features -D warnings` ok, `cargo deny check` ok, `cargo test --workspace` 308 passed, 0 failed, 1 ignored):

1. The composer follows the connection while connected, and the selected profile while disconnected (`composer_protocol`; test in `ui.rs`, both directions).
2. `Incoming.protocol` (set by the net thread) and `Msg.protocol` (also for sent messages: `record_sent` has a `protocol` argument) keep the version of each message. The inspector note uses it (`inspector::no_properties_note`). Tests: store rows V5, V311, V5 and a sent V311; the note for both switches; the net test for a packet of `max_packet` checks `Incoming.protocol` for v3 and v5.
3. The sheet keeps hidden session expiry and will delay values across a protocol switch and a save. Under 3.1.1 a wrong text is no error. Test `a_hidden_session_field_keeps_its_edit_and_raises_no_error`.
4 and 5. Plan line 97 changed: "Edit and publish" copies topic, payload, QoS and retain; "Reply" keeps its behaviour minus properties. The notice comes from the source message and the target protocol (`composer::v3_drops_properties`). Tests pin Edit, Reply and the notice (a Response Topic alone).
6. The first broker run had no EMQX or HiveMQ. The earlier claim "S2c touches no core net code" was wrong: `From<Profile>` changes the protocol selection, and `Incoming` and `Msg` have a new field. The full broker rerun with all three brokers (`cargo test -p navigator-core --features broker-tests --no-fail-fast -- --test-threads=1`, log `s2cr1-broker.txt`) passed: 167 unit (1 ignored), 24 `broker_compat`, 22 `broker_matrix`, 1 `load_ci`, 1 `net_blocked_write`, 27 `net_mosquitto`; 0 failed; `soak` ignored. After the run, EMQX and HiveMQ were stopped. Mosquitto still runs.

Deviations (all): 1 no `#[gpui_kit::test]` UI tests (none exist in the code base); 2 the QoS 1 timeout text stays "No acknowledgement after 10 s."; 3 (closed by fix 2); 4 `publish.rs` has no per-version `encoded_size`, the v3 rule is `validate_v3_size`, and `Net::send` checks only the v5 rules; 5 the sheet does not check Clean session off with an empty client ID (core refuses it); 6 lost-UNSUBACK and app-restart session tests on v3 are not added (D3 coverage open, S3); 7 (closed by fix 4 and 5); 8 the migration `v1_to_v2` is not covered by a mutation check (the field default gives the same result; the slice index needs it).

### S2c Codex round 2

1. While disconnected, the composer takes the protocol from the saved profile (`find_profile`, which reads the settings), not from the profile of the last Connect. While a connection or an attempt is live it uses the connection's protocol. Test `the_composer_follows_the_connection_while_connected_and_the_profile_otherwise` now also pins the edit during a connection followed by a disconnect, V311 to V5 and V5 to V311.
2. `docs/user-guide.md` describes "Edit and publish" and "Reply" separately: Edit copies topic, payload, QoS and retain; Reply uses the Response Topic; on MQTT 3.1.1 both drop properties and show the notice when the message had a property.

Runs (`s2cr2-*.txt`): fmt ok, clippy `--all-features -D warnings` ok, `cargo test --workspace` 308 passed, 0 failed, 1 ignored. The broker suite was not rerun: the change is in the app only.

### S2c Codex round 3

The composer protocol has three cases (`composer_protocol(pending, active, connection, saved)`): a Connect that waits for the keyring uses the saved protocol of its profile (`conn_protocol` changes only when the command goes out); else an active connection or attempt uses the connection's protocol; else the saved protocol of the selected profile. New test `a_connect_that_waits_for_the_keyring_uses_the_protocol_of_its_profile` (both directions, with and without an active connection); the earlier test passes `pending = None`. Runs (`s2cr3-*.txt`): fmt ok, clippy `--all-features -D warnings` ok, `cargo test --workspace` 309 passed, 0 failed, 1 ignored. Broker suite not rerun (app-only change).

### S2c Codex round 4

Root cause: `cancel_connection` cleared the generation gate but kept the last status (for example Connected of generation 1). The gate then drops the later statuses of that connection, so a cancel during the keyring read left "Connected" on screen for good. Before this change the same path already showed a stale "Connected" in the status bar and the Connect button, and it kept Send on (`connected()` read the stale state): a bug from before S2c, in the reconnect of the same profile. Fixes: `cancel_connection` resets `status` to the default (Disconnected, generation 0); the new `connection_active` counts a status only when its generation is the current one (or a Connect has no status yet), and the composer decision uses it. Test `a_cancelled_reconnect_leaves_no_active_connection` runs the real inputs (`GenGate`, `ConnState`, generations) in both protocol directions; with the generation check removed it fails (checked). The status reset itself has no unit test (it needs the window). Runs (`s2cr4-*.txt`): fmt ok, clippy `--all-features -D warnings` ok, `cargo test --workspace` 310 passed, 0 failed, 1 ignored. Broker suite not rerun (app-only change).

## S2d MQTT 5 additions

Date: 2026-11-15. Branch `feat/ntex-mqtt`, base commit aeb9a20. Nothing is committed. Logs: `s2d-*.txt` in the scratchpad (`ntex/`).

### What changed

- `navigator-core`:
  - `profile.rs`: `RetainHandling` (`on_subscribe`, `on_new_subscription`, `never`), `SubOptions` (`no_local`, `retain_as_published`, `retain_handling`; default = MQTT 3.1.1 behaviour), `SubOptions::check` (No Local on `$share/` is refused), `Profile.subscription_options` (map filter to options, default empty, not written when empty) and `Profile::options_of`. `topic::is_shared` is new.
  - Schema: no `schema_version` bump. The field is additive with a default, so a schema 2 file without it loads as before, and an export or import without it works (test below). A build from S2c reads a file with the field and ignores it (it would drop the options on its next save). That is accepted for a branch that is not released.
  - `net.rs`: `Command::Subscribe` has a field `options`. This is a change to a variant that the app and tests build, not a new variant (D1 says additive: the public type gains a field; all call sites in the workspace were updated). The wanted set holds `(filter, QoS, options)`. A new Subscribe for a known filter replaces its options. The first connect reads the options from the profile.
  - `net/wire.rs`: `subscribe_v5` sends the options (`wire_options`). v3 never sends them (`Wire::subscribe` ignores them). `subscribe_op` refuses No Local on a shared filter on v5 before any send, with "No Local is not allowed on a shared subscription ($share/...)." ntex also refuses it at encode time, so the local check gives the clear text. v3 sends the shared filter as a plain filter.
  - `Event::ConnectionDetails { generation, details }`, `ConnectionDetails { protocol, session_present, v5 }`, `V5Details`. The app keeps only the details of the latest connection (round 2): each connect stores them in the shared state, a newer connection replaces them, and they go to the event log when it has room, after the `Connected` status. v3 has `session_present` only. v5 has the assigned client ID, the effective keep alive (Server Keep Alive when present, also 0, else the profile value), Server Keep Alive, Receive Maximum, Maximum Packet Size, Maximum QoS, Retain Available, wildcard, shared and subscription identifier availability, Topic Alias Maximum, Server Reference, Response Information and Reason String. Broker texts go through `display_safe`.
  - Server reference: a refused v5 CONNACK text and a broker DISCONNECT text get " Server reference: X." appended. The old texts (D8) are unchanged when there is no reference. Before S2d neither text had the reference.
- `navigator-app`:
  - Subscription row: an "Options" button (v5 only, hidden on 3.1.1) opens a `Popover` (gpui-component 0.9.1 `Popover` through gpui-kit, as `ui/update.rs` already does) with two `Switch`es and a `ToggleGroup` (Always, If new, Never). A change sends a new SUBSCRIBE and the row shows "Subscribing…". No Local on a shared filter shows the refusal text under the input and sends nothing. The options are saved with the profile (`Subs::write_to`, `persist_subs`, the connection sheet save keeps them). The add input has no options.
  - Status bar: a "Connection details" button (only while connected and the details have arrived) opens a `Popover` with the lines of `messages::details_lines`. "Keep alive: off" shows when the effective keep alive is 0.
  - `same_connection` ignores the options (like the subscriptions).
- Docs: `docs/user-guide.md` (Subscription options, Connection details), `docs/ui-design.md` (Options, status bar, component table).

### Tests added

Core fake broker (`net.rs`): `v5_subscribe_sends_the_options_byte` (exact option bytes 0x2C, 0x01, 0x10, 0x05), `v3_subscribe_never_sends_options` (exact SUBSCRIBE body, also for a shared filter with all options), `resubscribe_keeps_the_options`, `changed_options_stay_after_a_reconnect`, `no_local_on_a_shared_subscription_is_refused_locally` (the broker sees only DISCONNECT), `connection_details_come_from_the_connack` (all properties, Server Keep Alive 0), `connection_details_use_the_profile_keep_alive_without_a_server_one`, `v3_connection_details_have_only_the_session_flag`, `a_refused_connack_text_ends_with_the_server_reference`, `a_broker_disconnect_text_ends_with_the_server_reference`. Profile: `subscription_options_are_additive_and_round_trip`, `no_local_is_refused_on_a_shared_filter_only`. App: `messages::tests` (details lines, keep alive off, options tooltip), `subscription_rows_keep_their_options_and_write_only_non_default_ones`. Mosquitto (`net_mosquitto.rs`, 127.0.0.1:18830): `no_local_hides_our_own_publish`, `retain_handling_decides_whether_a_retained_message_comes_at_subscribe` (Never: none; Always and If new: the retained message), `retain_as_published_keeps_the_retain_flag_of_a_live_message`.

Mutation checks (each made the named tests fail; code restored): `no_local` forced false (`v5_subscribe_sends_the_options_byte`, `no_local_hides_our_own_publish`); `retain_as_published` forced false (same v5 test, `retain_as_published_keeps...`); Retain Handling Never sent as 0 (`retain_handling_decides...`); `options_of` always default (`v5_subscribe...` and `resubscribe_keeps_the_options`); replacing the options of a known filter removed (`changed_options_stay_after_a_reconnect`); local No Local check disabled (`no_local_on_a_shared...`); Server Keep Alive ignored (`connection_details_come_from_the_connack`); server reference text removed (both server reference tests); "off" text changed (`details_show_keep_alive_off_when_it_is_0`); the non-default filter removed from `wanted_options` (the app rows test). Not checked by mutation: v3 sends no options (the v3 code path has no option argument, so a mutation does not exist without adding one); the popover rendering.

### Changed expectations (plan lines)

| Test | Change | Plan line |
|---|---|---|
| `net.rs` test helper `next_event` | It skips `Event::ConnectionDetails`. `next_event_any` keeps it. Three tests failed without this (`reconnects_after_drop_and_resubscribes_live_filters`, `v3_suback_codes_become_results`, `v3_unsuback_means_done`): the new event arrives before the first SUBACK result. No assertion changed. | D1 (line 64): "the `ConnectionDetails` event" is an additive event |
| `tests/net_mosquitto.rs` (`next_event`, three tests that read raw events, the generation match), `tests/broker_compat.rs`, `tests/broker_matrix.rs`, `tests/load_ci.rs` | They skip or accept `ConnectionDetails`. No assertion, count or time changed. | D1 (line 64) |
| All `Command::Subscribe { .. }` literals in tests | They gain `options: Default::default()` (the default is today's behaviour). | S2d, line 144 |

### Gate

| Check | Result |
|---|---|
| `cargo fmt --all --check` | ok (`s2d-fmt.txt`) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | ok (`s2d-clippy.txt`; the first run found `type_complexity` and `too_many_arguments`, both fixed) |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok (`s2d-deny.txt`) |
| `cargo test --workspace` | 327 passed, 0 failed (`s2d-test.txt`). This run came before the last test-only change (the three `net_mosquitto` readers). Round 1 below has the rerun on the final code. |
| `tests/broker/up.sh` (all three brokers) | all listeners answered (`s2d-up.txt`) |
| Broker suite `--no-fail-fast -- --test-threads=1` (`s2d-broker.txt`) | unit 179 (1 ignored), `broker_compat` 24, `broker_matrix` 22, `load_ci` 1 (361 s), `net_blocked_write` 1: all passed. `net_mosquitto`: 27 passed, 3 failed (`pending_operations_are_capped`, `slow_ui_still_gets_every_publish_result`, `many_filters_and_a_slow_ui_lose_no_result`): each read raw events and panicked on the new `ConnectionDetails` event. I fixed the three readers (arms for the new event) and reran the binary: 30 passed (`s2d-broker-mosq.txt`). The other binaries were not rerun after that test-only change. |
| EMQX and HiveMQ stopped | `docker compose -f tests/broker/compose.yml stop emqx hivemq`; Mosquitto still runs. No image was pruned. |
| `cargo build --release -p navigator-app`, short runs | `--mqtt 3` and `--mqtt 5` with `--tcp --host 127.0.0.1 --port 18830` ran until the 8 s timeout (exit 124) with no stderr output (`s2d-run3.txt`, `s2d-run5.txt`). No screenshot tool: the Options popover and the Connection details popover were built and launched but not seen or clicked. |

### Deviations and open points

1. `Command::Subscribe` gained a field (see above). A strictly additive API would need a new variant. Decision of the orchestrator: keep the field. D1 in the plan now lists the three allowed changes (round 1, item 3).
2. No UI interaction test: the popovers, the click handling on the row (the wrapper `div` stops the click so that "Show only" does not toggle) and the redraw of the row after a change are not verified in a window. The pure parts (rows, options write-back, detail lines) have tests.
3. The add input has no options. A user who wants Retain Handling "Never" on a new topic adds the row, then sets the option, which sends a second SUBSCRIBE. The first SUBSCRIBE can already deliver the retained message.
4. A saved profile with No Local on a shared filter (for example a hand-edited file) loads and shows the refusal text on that row at each connect. The profile check (`Profile::validate`) does not reject it.
5. The Options button follows `composer_protocol()` (the connection while live, else the saved profile).
6. The v3 "no options sent" test checks bytes only; it has no mutation check.
7. Not run: Codex review for this step (the caller does it).

### S2d Codex round 1

Logs: `s2dr1-*.txt`.

1. Options change while a SUBSCRIBE waits. A SUBACK names the filter only, so with change A in flight, a failed send of change B could end with A's SUBACK showing "Active" for B. Fix (serialize, the simpler choice): while the connection is up and a row is Pending, or while the row is being removed, the options are locked. The Switches and the toggle group are disabled, and `Subs::begin_options_change` refuses with `Busy` (the click handler checks it too). Offline, a Pending row has no SUBSCRIBE in flight, so it stays editable. Test `options_cannot_change_while_a_subscribe_waits_for_its_suback`: offline change allowed, connected and Pending gives `Busy` and keeps A, the SUBACK confirms A, then B is allowed, same options and unknown rows do nothing, a removal locks the row, No Local on a shared filter is refused. Mutation: removing the connected-and-Pending lock fails the test. Not tested: the disabled look of the controls in a window. A test of a failed second send is not possible any more: there is no second send.
2. `ConnectionDetails` and the event log. It no longer calls `Shared::event` at once. It waits in `Resub` and goes out in `feed` when the log has room, like `Reconnected` (the drain signal wakes it). Test `connection_details_wait_for_room_in_a_full_log_and_are_not_lost`: the log is filled to its reserve (1409 events), the connection drops and comes back, no event is added while the log is full (`events.len()` stays 1409), then the UI reads with `recv_event` and gets exactly two details with the generation of the connection, one `Reconnected`, and `event_drops` is 0. Mutation: sending the details at once added an event (1410) and failed the test. The log cannot be driven to zero room by reconnects in a test (about 640 reconnects would be needed), so the test pins the gate, not an overflow.
3. D1: decision of the orchestrator, see deviation 1. `docs/plans/2026-11-14-ntex-mqtt-plan.md` line 64 now names the three allowed API changes.
4. Raw lifecycle test `connection_details_come_once_for_each_connect` (`net_mosquitto.rs`, Mosquitto with a cuttable proxy): first connect, reconnect of the same generation after a cut, and a replacement by a second Connect. Each step reads raw events: exactly one `ConnectionDetails` with the right generation, the status is already Connected when it arrives, `Reconnected` follows the details on the reconnect, and no other event comes. The `next_event` helper of that file now asserts that each skipped details has a generation from 1 to the latest. Mutation: a fixed generation 1 in the event failed the test (second connect).
5. The workspace run of the first report preceded the last test-only change: now stated in the gate table. Round 1 reran everything:

| Check | Result |
|---|---|
| fmt, clippy `--all-features -D warnings`, `cargo deny check` | ok (`s2dr1-fmt.txt`, `s2dr1-clippy.txt`, `s2dr1-deny.txt`) |
| `cargo test --workspace` | 329 passed, 0 failed, 1 ignored (`s2dr1-test.txt`) |
| `tests/broker/up.sh`, all three brokers | ok (`s2dr1-up.txt`) |
| Broker suite, all three brokers (`--no-fail-fast -- --test-threads=1`, `s2dr1-broker.txt`) | unit 180 (1 ignored), `broker_compat` 24, `broker_matrix` 22, `load_ci` 1, `net_blocked_write` 1, `net_mosquitto` 31: all passed, 0 failed; `soak` ignored. EMQX and HiveMQ stopped afterwards (no prune); Mosquitto still runs. |

### S2d Codex round 2

Logs: `s2dr2-*.txt`.

1. Options lock and a failed Remove. A failed Remove changes Pending to NotRemoved while the SUBSCRIBE is still out, so the state-based lock opened too early. Each row now has `inflight`: set when the UI sends a SUBSCRIBE (`send_subscribe`, and the add path), cleared only by the `Subscribed` result of that filter, by a new connection or a status that is not Connected (`connection_reset`), or by a cancel (`cancel_connection`). The lock is: removal accepted, or `inflight`, or connected and Pending. The button and `Subs::begin_options_change` both use it. Test `options_cannot_change_while_a_subscribe_waits_for_its_suback` has the new case: a failed Remove during an in-flight SUBSCRIBE keeps `Busy` (connected and offline), `clear_inflight` opens it. Mutation: ignoring `inflight` fails the test.
2. Details and the attempt. The details of a connection no longer live in the `Resub` of its attempt. The latest value (with its generation) is in `Shared.details`, outside the attempt, and `Resub::feed` of any attempt sends it when the log has room. A newer connection replaces an older value. This is a latest value, not a log of each connect. Plan: the feature-table row "Connection details from CONNACK" now says "The app keeps only the details of the latest connection." Wording change in this report: the earlier text "one event for each connect" is replaced by "the details of the latest connection"; when the log has room, each connect still gives one event, so the raw lifecycle test `connection_details_come_once_for_each_connect` holds unchanged. New test `a_full_log_keeps_only_the_details_of_the_latest_connection`: the log is full, a second Connect replaces the first, its connection drops twice, and after the drain the UI has `[g1, g2]`: the details of g1 that were already in the log, and exactly one for g2 (no details of the two dropped attempts). Mutation: sending the details at once gave `[1, 2, 2, 2]` and failed the test. The earlier full-log test (`connection_details_wait_for_room_in_a_full_log_and_are_not_lost`) passes unchanged.

Gate (round 2): fmt, clippy `--all-features -D warnings` and `cargo deny check` ok; `cargo test --workspace` 330 passed, 0 failed, 1 ignored (`s2dr2-test.txt`); `net_mosquitto` against Mosquitto, 31 passed (`s2dr2-mosq.txt`). The change touches event delivery in `net.rs` (the details store and `feed`) and the app. The full three-broker suite was not rerun: EMQX and HiveMQ stayed stopped.

### S2d Codex round 3

Logs: `s2dr3-*.txt`.

1. Rows that core subscribes by itself. The first subscribe after a Connect and the automatic resubscribe after a reconnect left `inflight` false. Now `Subs::connection_started` (called when a Connect goes out) locks every row at once, and `Subs::on_status` (called by the status handler for each state that is not Connected) locks every row for `Connecting` and `Reconnecting` and clears the lock for `Disconnected`, `Disconnecting` and `Failed`. Each `Subscribed` result (success or failure) clears the lock of its filter. Test `rows_wait_for_results_after_a_connect_and_a_reconnect` drives these entry points (the same ones that the window code calls) for the first connect, a reconnect, a failed result, a disconnect and a failure, with a failed Remove during each wait. Mutations: `connection_started` without the lock and `on_status` locking only for `Connecting` each fail the test. Not covered: a status that skips `Reconnecting` (the status channel keeps the latest value only), so the lock is not set in that case; and a late "lost" result of the old connection can clear the lock of a filter whose resubscribe is still out. Both are accepted: the window is short and the results carry no operation id.
2. Stale details after a reconnect. `Status` has a new field `attempt` (the attempt that is Connected, 0 otherwise) and `Event::ConnectionDetails` has the same `attempt`; this is the smallest tag. The app keeps the latest details in `DetailsCache`: the status handler clears it when the status leaves Connected (and a Connect or a cancel clears it too), and the popover shows the details only while the status is Connected and the attempt of the details equals `Status.attempt`. An event of an older attempt that is still in the log shows nothing. Test `the_details_cache_never_shows_an_older_attempt`: attempt 5 shows, a loss clears, an old event of attempt 5 does not show under attempt 6, the details of attempt 6 (other values) show, details that arrive before the Connected status show after it, and a status that skipped the clear does not show attempt 7 under attempt 8. Mutation: ignoring the attempt fails the test. In core, `connection_details_come_from_the_connack` and the raw lifecycle test now assert that the event attempt equals `Status.attempt` of the connected attempt.
3. Full-log test with distinct attempts. `a_full_log_keeps_only_the_details_of_the_latest_connection`: each attempt of the fake broker has its own Receive Maximum (10, 11, 12, 13), the catch-all arm is gone (an unexpected event panics), and the test asserts the whole order after the drain: details of g1 (10, attempt of the first Connected status), details of g2 (13, attempt of the final Connected status), then `Reconnected` of g2. The dropped attempts (11, 12) leave nothing. Mutation: keeping the oldest stored details gave `details 2 11 2` instead of `details 2 13 4` and failed. The first full-log test now also matches events without a catch-all and asserts details (old attempt), details (new attempt), then `Reconnected`.

Gate (round 3): fmt, clippy `--all-features -D warnings` and `cargo deny check` ok; `cargo test --workspace` 332 passed, 0 failed, 1 ignored (`s2dr3-test.txt`); `net_mosquitto` against Mosquitto 31 passed (`s2dr3-mosq.txt`). The core change adds `Status.attempt` and `attempt` on the event (no change to how connections run). The full three-broker suite was not rerun.

### S2d Codex round 4: the lock is replaced by value correlation

Logs: `s2dr4-*.txt`. This section replaces the lock of round 1 (item 1), round 2 (item 1) and round 3 (item 1). Those descriptions no longer apply: `inflight`, `options_locked`, `Busy`, `connection_started`, `on_status` locking and the disabled state while a SUBSCRIBE waits are deleted. The UI no longer guesses what core has in flight.

- Core: `Event::Subscribed` has a new field `options`: the options of the SUBSCRIBE that the result answers. This holds for a live subscribe, the first subscribe from the profile, the automatic resubscribe after a reconnect, and a local refusal. Test `subscribed_results_carry_the_options_that_were_sent` (fake broker; profile options, a live subscribe, a No Local refusal on a shared filter, and the resubscribe after a drop). Mutation: a default value in the event fails it.
- App: `Subs::on_subscribed(filter, options, result)` changes a row only if `options` equal the options of the row (and the row is not in a removal). A result for older options changes nothing: the newer change has its own result, or its failure is on the row. The options stay editable at all times, except that a row in a removal does not change (`Ignore`). While a connection or an attempt is live (`is_live`), a change is sent as a SUBSCRIBE; with no connection, only the row and the profile change, and the next Connect subscribes with them (`write_to` into the profile, then the first subscribe of the connection).
- Tests (app): `a_result_for_older_options_does_not_decide_the_row` (A sent, B fails to queue, A's result: the row keeps B and the failure; B's own result then applies), `results_for_a_then_b_end_active_with_b` (results A then B, then a late "lost" for A is ignored), `a_skipped_reconnecting_status_still_gives_correct_rows` (no reset needed: results for the options of the row apply, an older one does not), `a_row_in_removal_keeps_its_options`, `an_edit_without_a_connection_reaches_the_profile_of_the_next_connect`. Mutation: removing the options equality check fails the first three. The earlier lock tests are deleted with the lock.
- Not tested: the offline edit "sends nothing" as a window test (it is the `is_live` branch of `set_sub_options`, a gpui method); the bytes of the first subscribe from profile options are pinned by the core test `v5_subscribe_sends_the_options_byte`.
- D1 in the plan lists the allowed changes: the `protocol` field, `options` on `Command::Subscribe` and on the `Subscribed` result, `attempt` on `Status`, and the `ConnectionDetails` event with its `attempt`. `Status.attempt` stays: the details cache needs it.

Gate (round 4): fmt, clippy `--all-features -D warnings` and `cargo deny check` ok; `cargo test --workspace` 336 passed, 0 failed, 1 ignored (`s2dr4-test.txt`); `tests/broker/up.sh` all three brokers ok; broker suite `--no-fail-fast -- --test-threads=1` (`s2dr4-broker.txt`): unit 182 (1 ignored), `broker_compat` 24, `broker_matrix` 22, `load_ci` 1, `net_blocked_write` 1, `net_mosquitto` 31 passed, 0 failed; `soak` ignored. EMQX and HiveMQ stopped afterwards (no prune); Mosquitto still runs.

### S2d Codex round 5: request IDs replace value correlation

Logs: `s2dr5-*.txt`. This section replaces the design of round 4 (values compared). Value comparison failed for repeated values (A, B, A) and for a failed enqueue followed by a reconnect.

- Core: `Command::Subscribe` has `req: u64`. The wanted set stores `req` with the filter, and `Event::Subscribed` has `req` instead of `options` (the app has no use for the options on the result, so the field is gone). Every result echoes the `req` of the latest accepted Subscribe of its filter. The automatic resubscribe reuses the stored `req`. A filter from the profile has `req` 0. A local refusal also echoes it. Test `subscribed_results_echo_the_req_also_on_resubscribe` (fake broker: profile filter with 0, a refused shared filter, the same filter twice with the same options and IDs 11 and 12, and the resubscribe after a drop with the stored IDs). Mutation: the resubscribe with `req` 0 fails it.
- App: a row has `req`, the ID of the last Subscribe that core accepted. The row takes new options only when `Net::send` accepted the command (`Subs::check_options_change` checks, `commit_options` stores, with the new ID). If the send fails (queue full), the row keeps the options and the ID that core holds, shows the not-sent text, and the profile does not change. A result changes a row only if its `req` equals the row's (`on_subscribed(filter, req, result)`). A new row (add, or add after a removal) gets a new ID from a counter that never repeats. A new Connect sets every ID to 0 (core builds the wanted set from the profile with 0). Offline, an edit changes the row and the profile only and keeps the ID. `Retry` sends with a new ID. On `Reconnecting` the rows go to Pending as before, and the resubscribe result (stored ID) resolves them.
- Tests (app): `a_failed_enqueue_leaves_the_row_with_what_core_holds`, `repeated_values_are_told_apart_by_the_request_id` (A, B, A), `a_failed_enqueue_then_a_reconnect_resolves_with_the_resubscribe_result`, `an_older_row_or_connection_cannot_decide_a_new_row` (removal and re-add, new connection), `an_edit_without_a_connection_reaches_the_profile_of_the_next_connect`, `a_row_in_removal_keeps_its_options`. Mutations: the `req` comparison forced true fails three tests; a check that stores the options before the send fails `a_failed_enqueue_leaves_the_row_with_what_core_holds`. The value-correlation tests of round 4 are replaced.
- Known and accepted: a "lost" result of an old attempt carries the current `req`, so it can show Failed on the row for a short time until the resubscribe result arrives. The glue in `set_sub_options` (gpui method: `is_live`, the send, the commit or the failure) has no window test; its steps are the tested `Subs` functions.
- D1 in the plan now says: `options` and `req` on `Command::Subscribe`, `req` on the `Subscribed` result, `attempt` on `Status`, and the `ConnectionDetails` event with its `attempt`.

Gate (round 5): fmt, clippy `--all-features -D warnings` and `cargo deny check` ok; `cargo test --workspace` 337 passed, 0 failed, 1 ignored (`s2dr5-test.txt`); `tests/broker/up.sh` all three brokers ok; broker suite `--no-fail-fast -- --test-threads=1` (`s2dr5-broker.txt`): unit 182 (1 ignored), `broker_compat` 24, `broker_matrix` 22, `load_ci` 1, `net_blocked_write` 1, `net_mosquitto` 31 passed, 0 failed; `soak` ignored. EMQX and HiveMQ stopped afterwards (no prune); Mosquitto still runs.

## S3 Port the tests and tools

Date: 2026-11-15. Branch `feat/ntex-mqtt`, base commit 61ca6ae. Nothing is committed. Logs: `s3-*.txt` in the scratchpad (`ntex/`).

### What changed

- `ms-mqtt-client` is out of `navigator-core` (dev-dependency removed). `cargo tree -i ms-mqtt-client` says the package is not in the graph. The grep for `ms_mqtt_client` and `ms-mqtt-client` in `crates/` and `fuzz/` finds two places, both left for S5: the `[patch]` in `Cargo.toml` and `fuzz/Cargo.toml`, and the log filter `ms_mqtt_client=off` in `navigator-app/src/logging.rs`. `vendor/ms-mqtt-client` is unchanged. Cargo prints "patch ... was not used" until S5 removes the `[patch]`. `Cargo.lock` and `fuzz/Cargo.lock` lose `ms-mqtt-client` and the crates only it used (`async-tungstenite`, `derive-where`, and others).
- One test publisher for all tools: `tests/common/mod.rs` (`Publisher`). It is a second `Net` (the code path of the app). `send` waits while the command queue or the event log is full. `drain` counts the results. `settle` waits for the results. `examples/flood.rs` includes it with `#[path]`. Users: `net_mosquitto.rs` (`publish_plain`: QoS 1, waits for the PUBACK result), `load_ci.rs`, `soak.rs`, `flood.rs`. `broker_compat.rs::connack_probe` reads `Event::ConnectionDetails` from `Net`.
- `fuzz/fuzz_targets` never used the old client. Nothing to port.
- `net.rs` unit tests `topic_errors_*` and `filter_errors_*` compared our texts with the old client. The old client is gone, so they are now tables with the texts that the old client gave (taken from the old client at HEAD, 19 topics and 34 filters, same cases). They are named `topic_errors_have_fixed_texts` and `filter_errors_have_fixed_texts`.
- New fake broker `tests/common/fake.rs`: TCP, TLS, WS, WSS on the loopback with blocking sockets, certificates made in memory, optional client certificate check. New test files: `tests/net_fake.rs` (89 tests, 8 are `#[ignore]`), `tests/net_fd_leak.rs`, `tests/net_keepalive_tasks.rs`. Both of the last two have their own binary because they use process-global state (file descriptor count, the `log` logger).
- `broker_matrix.rs` and `net_mosquitto.rs` have more tests (see below). The spike example stays.

### The `LIMITS` locks

All `LIMITS` locks are removed (`net_mosquitto.rs`, `broker_matrix.rs`, `broker_compat.rs`). Plan line 116 ("The test lock for `local_limits` goes away"). What I checked for another need of serialization:

- `net_mosquitto.rs`: no test restarts the broker or takes a fixed port. The proxy tests bind port 0. Every test has its own topic prefix (process ID and time). Every profile has a random client ID (`Profile::default`). The fixed client IDs of the v3 tests (`v3-basic`, `v3-mib`, `v3-limit-sub`, `v3-limit-pub`, `v3-persist-other`, `v3-clean-other`) are each used by one test only. The persistent v3 session test uses an ID with the process ID. Result: no lock needed. 10 runs and a full run with the default test threads pass (below).
- `broker_matrix.rs` and `broker_compat.rs` have a test that restarts a broker (`broker_restart_reconnects_and_resubscribes`, `restart_reconnects`). A restart in parallel with the other tests of the file would break them. So `LIMITS` became a `tokio::sync::RwLock` (`BROKER`): the restart test takes the write side, every other test takes the read side. A tokio lock is held across `await` without a clippy `await_holding_lock` allow, and works between the runtimes of different tests. This is a lock for another reason than `local_limits`.
- The `ms_mqtt_client::local_limits` global is gone. `Net` has no process-global state (S2).

### Spike cases: moved, already covered, not moved

Cases of `examples/ntex_spike/cases.rs` and `fakecases.rs` (run 6, 93 cases). "Moved" means: a new test in `tests/` that runs `Net` against the fake broker or Mosquitto, at the plan bound.

| Spike case | Where it is now |
|---|---|
| a: round trips v3/v5 on TCP, TCP + password, TLS, mTLS, WS, WSS | Already covered: `broker_matrix` `anonymous_tcp`, `password_tcp`, `tls_with_test_ca`, `mtls_with_client_certificate`, `websocket`, `secure_websocket_with_test_ca` and the `v3_` twins (S2b) |
| a: TLS and WSS with a DNS name (`localhost`) | Moved: `broker_matrix` `tls_with_a_dns_name`, `secure_websocket_with_a_dns_name`, `v3_` twins |
| b: expired, wrong host, unknown CA, mTLS without certificate (v5) | Already covered: `broker_matrix` `tls_expired_certificate`, `tls_wrong_host_name`, `tls_without_the_ca_is_not_trusted`, `mtls_without_client_certificate_fails` |
| b: the same on v3 | Moved: the four tests above, plus `tls_verify_off_accepts_untrusted_certificates` and `mtls_mismatched_key_fails`, now run for both versions (`*_with(protocol)`, `v3_` twins) |
| b: local key texts | Already covered: `tls::tests::encrypted_client_key_fails_at_once`, `mismatched_client_key_is_reported` |
| c: varint boundaries; incoming exactly max and +1 (v3, v5); non-minimal length encoding | Already covered (S2b): `the_largest_remaining_length_is_exact_at_the_varint_boundaries`, `an_incoming_packet_of_max_packet_passes_and_one_more_byte_fails`, `v3_non_minimal_length_encoding_is_checked_by_remaining_length` |
| c: v3 outgoing exactly 1 MiB and +1 | Already covered: `a_v3_publish_of_max_packet_is_sent_and_one_more_byte_is_refused`, `net_mosquitto::v3_a_packet_of_one_mib_round_trips_and_one_byte_more_is_refused` |
| c: exactly 1 MiB out and in on TCP, WS, WSS (v5), WS, WSS (v3) | Moved: `broker_matrix` `one_mib_over_tcp`, `one_mib_over_websocket`, `one_mib_over_secure_websocket`, `v3_one_mib_over_websocket`, `v3_one_mib_over_secure_websocket`. See deviation 4 (the v5 packet that returns is 3 bytes smaller) |
| c: v5 CONNACK announces Maximum Packet Size: exact size sent, +1 refused before the send | Moved: `net_fake::a_v5_publish_over_the_announced_maximum_packet_size_is_refused_before_the_send` |
| d: stalled write, v3 and v5, TCP, TLS, WS, WSS | Moved: `net_fake::stalled_write::*` (8 tests; v3, TLS, WS, WSS were "Not run" in S1) |
| d: blocked write buffer + missing PINGRESP (ka 2 s, bound 17 s) | Moved: `net_fake::v3_stalled_write_with_keep_alive_2_s`, `v5_...` |
| d: PINGREQ never answered, ka 2 s, with and without publishes | Moved: `net_fake::missing_pingresp::*` (8 wires; v3 was not run on `Net`), `v3_missing_pingresp_with_publishes`, `v5_...`. The v5 TCP case also exists as `net.rs::a_missing_pingresp_ends_the_attempt_with_the_general_text` |
| d: ka 60 s and 70 s | Moved, `#[ignore]` (see below): `net_fake::v3_keep_alive_60_s`, `v5_keep_alive_60_s`, `v3_keep_alive_70_s`, `v5_keep_alive_70_s` |
| d: old keep-alive tasks end (20 connects) | Moved: `net_keepalive_tasks::old_keep_alive_tasks_end` (v3 and v5) |
| d: 2 MiB header, 1 MiB limit | Moved: `net_fake::big_header::*` (TCP and TLS, both versions): the end comes at once with the size text. The RSS was not measured |
| d: incoming QoS 2 (v3, v5) | Already covered: `qos2_publish_fails_the_session_without_retry_or_panic`, `v3_qos2_closes_without_pubrec` |
| d: half fixed header, half PUBLISH payload on TCP and TLS | Moved: `net_fake::half_fixed_header::*`, `half_payload::*` (at the plan values: 10 s check, bound 13 s) |
| d: disconnect while a half frame waits (4 transports) | Moved: `net_fake::disconnect_while_a_half_frame_waits::*` (8 wires) |
| d: WS and WSS half binary frame, ka 2 s | Moved: `net_fake::half_binary_frame::*` (WSS was "Not run" for the stalled upgrade; the half frame ran in S1 and now runs on `Net`) |
| d: WS upgrade never answered | WS already covered: `net.rs::a_stalled_websocket_upgrade_is_a_temporary_timeout`. WSS moved (was "Not run"): `net_fake::v3_a_stalled_upgrade_over_wss_is_a_temporary_timeout`, `v5_...` |
| d: trickling payload (cap 60 s) | Moved for TCP and TLS, both versions, `#[ignore]` (60 s): `net_fake::trickle::*`. TLS was "Not run" |
| d: WS fragmented packet, Ping, Pong, Text | Moved: `net_fake::websocket_frames::*` (WS and WSS, both versions) |
| d: handler panic after an await | Already covered: `a_handler_panic_after_an_await_fails_the_attempt`, `a_v3_handler_panic_after_an_await_fails_the_attempt` |
| d: disconnect during a stalled send (graceful, force close) | Moved: `net_fake::disconnect_during_a_blocked_write::*` (8 wires). The v5 TCP case also exists as `net_blocked_write` |
| d: acknowledgement wait cancelled by close | Moved (was "Not run"): `net_fake::ack_wait_cancelled_by_close::*` (8 wires): `LostOnDisconnect` and `Disconnected` in at most 3 s |
| d: v5 broker DISCONNECT reason kept | Already covered: `a_broker_disconnect_text_ends_with_the_server_reference` |
| d: Server Keep Alive 5 s and 0 followed | Moved: `net_fake::v5_server_keep_alive_5_s_is_followed`, `v5_server_keep_alive_0_sends_no_ping` |
| d: silent CONNACK (v3, v5), stalled TLS | Moved: `net_fake::silent_connack::*`, `stalled_tls::*` (TLS and WSS). Stalled TLS also: `net.rs::tls_handshake_cut_off_is_retried` |
| d: black-hole dial | Moved (Linux only): `net_fake::black_hole::a_black_hole_dial_ends_with_the_connect_deadline` |
| d: refused CONNACK (v3 code 5, v5 0x87), version errors as raw bytes | Already covered (S2b): `v3_connack_return_codes_have_plain_text`, `v5_connack_0x84_names_the_protocol_setting`, `a_connack_that_v5_cannot_decode_says_the_reply_is_not_mqtt_5`, `not_authorized_fails_without_retry`, `a_refused_connack_text_ends_with_the_server_reference` |
| d: first address refuses, stalled DNS | Already covered: `dial::the_next_address_is_tried_when_the_first_refuses`, `cancelled_attempts_keep_one_lookup` |
| d: black-hole dial cancelled leaves no fd | Moved (Linux only): `net_fd_leak::a_cancelled_black_hole_dial_leaves_no_file_descriptor` (the dial ends by a disconnect, because the deadline is 10 s in production) |
| "Not run": mTLS over WSS | Moved: `net_fake::v3_mtls_over_wss`, `v5_mtls_over_wss` (the fake serves a WSS listener that needs a client certificate) |
| "Not run": 1000 queued QoS 1 publishes at Receive Maximum 2 | Moved: `net_fake::a_thousand_publishes_at_receive_maximum_2_keep_to_two_in_flight` (in-flight count and order; no RSS) |
| S2b open point 5 / S2c deviation 6: lost UNSUBACK and app restart on v3 (D3) | Moved: `net_mosquitto` `v3_a_lost_unsubscribe_is_retried_on_the_resumed_session`, `v3_a_removal_survives_a_restart_with_the_saved_session_filters`, and also `v3_a_removal_after_a_manual_disconnect_reaches_the_next_session`, `v3_a_new_profile_on_a_shared_session_gets_the_cached_set`, `v3_a_clean_session_forgets_the_filters`. The v5 tests keep their names and bodies (now `*_with(protocol)`) |

Not moved, and why:

- (Round 1, finding 9: moved. `net_fake::a_v5_publish_over_the_local_limit_is_sent_when_the_broker_announces_none`.)
- ka 60 s and 70 s with our publishes (the spike ran 8 cases together). The new ignored tests run without publishes. With publishes, ka 2 s is covered.
- RSS of the 2 MiB header case, and the overload test (RSS flat): S4 measures memory.
- A broker that negotiates TLS 1.2 only, an IPv6-only host, the order of `getaddrinfo` for `localhost`: need a host set-up that the repo does not have.
- The overload test (S2 deviation 9, the plan lists it under S4): not done in S3. Topic alias set and reuse are covered by the Round 1 proxy test; the invalid alias by Round 2 finding 3.
- The spike also measured the release size and scaled frame-read-rate values. These are numbers of S1 and S4, not tests.

Spike decision: the example stays (`examples/ntex_spike.rs`, `examples/ntex_spike/`, and the `log` dev-dependency). The cases above under "Not moved" are only there. The plan calls the spike throw-away. A later step (S5) can delete it when the owner accepts those gaps.

### Tests changed in S3 (assertions and timing)

No assertion was removed. Round 1 found two checks that were weaker than before: the PING timing bounds (finding 4) and the QoS result check of `Publisher` (finding 6). Both are restored. No timing of an existing test changed otherwise.

| Test | Change | Plan line |
|---|---|---|
| `net.rs` `topic_errors_*`, `filter_errors_*` (renamed `*_have_fixed_texts`) | The oracle is a table of the texts of the old client (same cases) instead of a call of the old client. The checks are the same values. | S3 (line 145) and S5 (the old client goes away) |
| `broker_compat::connack_probe` | Reads `ConnectionDetails` from `Net` instead of the CONNACK of the old client. Kept: clean start gives `session_present` false, retain available, wildcards available. `is_success` has no equivalent: a failed connect never reaches `ConnectionDetails`. The test also asserts the protocol is V5. | Mapping row "`new_client`" (line 114) |
| `load_ci.rs`, `soak.rs`, `flood.rs` publisher | A second `Net` publishes QoS 0 (was the old client). `load_ci` fails if any publish result is not `Sent`. `soak` counts as `sent` the accepted publishes (Round 1, finding 1: failures are counted apart), and waits while the publisher status is not `Connected` (the old publisher connected again by itself). All rate, drop, RSS and slice assertions are the same. | S3 (line 145) |
| `net_mosquitto::publish_plain` | Uses `Publisher` (QoS 1, waits for the result `Acknowledged`, asserts exactly one result). | S3 (line 145) |
| `LIMITS` locks | Removed (see above). | Mapping row `local_limits` (line 116) |
| `net_mosquitto.rs`, `broker_matrix.rs` | Seven tests now call a `*_with(protocol)` body and keep their name for V5; the V3 twin is new. | S3 (line 145) |

### Mutation checks

Each check changed the product code (or the test input), ran the named tests, and the tests failed. I restored the code after each.

| Mutation | Tests that failed |
|---|---|
| No stale UNSUBSCRIBE after the resume (`self.unsubs.pop_front()` replaced) | `a_lost_unsubscribe_is_retried_on_the_resumed_session`, `v3_...` (`s3-mut1.txt`) |
| v3 CONNECT keep alive set to 0 | `missing_pingresp::v3_tcp` (`s3-mut2.txt`) |
| Frame read rate check 10 s changed to 40 s | `half_fixed_header::v3_tls` (`s3-mut3.txt`) |
| Write timeout and `OP_TIMEOUT` 60 s | `stalled_write::v3_wss` (`s3-mut4.txt`) |
| `PublishResult::LostOnDisconnect` mapped to `TimedOut` | `ack_wait_cancelled_by_close::v3_ws`, `v3_wss` (`s3-mut5.txt`) |
| v5 keep alive 0 | `half_binary_frame::v5_ws`, `v5_wss` (`s3-mut6.txt`) |
| `io_outcome` branch for the WebSocket "Timeout" removed | both `*_a_stalled_upgrade_over_wss_is_a_temporary_timeout` (`s3-mut8.txt`) |
| Client certificate not loaded | `v3_mtls_over_wss`, `v5_mtls_over_wss` (`s3-mut9.txt`) |
| Packet limits (`max_packet`) times 8 in the codec set-up | the four `big_header` tests (`s3-mut10.txt`) |
| Outgoing size check `size > max + 1` | `a_v5_publish_over_the_announced_maximum_packet_size_is_refused_before_the_send` (`s3-mut11.txt`) |
| Receive Maximum 2 changed to 100 in the fake CONNACK | `a_thousand_publishes_at_receive_maximum_2_keep_to_two_in_flight` (`s3-mut12.txt`) |
| Keep alive 2 s changed to 30 s in the test input | `old_keep_alive_tasks_end` (count 0 after 3 s) |

Mutation checks that could not be made, or did not fail:

- `CLOSE_GRACE` 2 s changed to 60 s did not fail `ack_wait_cancelled_by_close`. The wait for an acknowledgement is cleared at once by the close (S1 finding 11), so the grace time does not matter for it. The test pins the result and the 3 s bound; the `LostOnDisconnect` mutation above fails it. The prompt close of a blocked write is the case that depends on the grace time, and `disconnect_during_a_blocked_write` covers it (not mutation-checked).
- `net_fd_leak`, `disconnect_during_a_blocked_write`, `websocket_frames`, `half_payload`, `silent_connack`, `stalled_tls`, the v3 matrix tests, the DNS name tests and the 1 MiB tests: not mutation-checked. The first two have no simple way to inject a leak or a slow close. The others check library behaviour or exact texts that the broker or the fake produces.
- The ka 60 and 70 tests, and the trickle tests: not mutation-checked (long runs).

### Gate (final code)

Mosquitto, EMQX and HiveMQ started with `tests/broker/up.sh` (all 12 listeners answered CONNACK, `s3-up.txt`).

| Check | Result |
|---|---|
| `cargo fmt --all --check` | ok (`s3-fmt.txt`) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | ok (`s3-clippy.txt`) |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok (`s3-deny.txt`) |
| `cargo test --workspace` | 420 passed, 0 failed, 9 ignored (`s3-test.txt`). `navigator-core`: 182 unit, 1 `net_blocked_write`, 81 `net_fake` (68.59 s), 1 `net_fd_leak` (24.99 s), 1 `net_keepalive_tasks` (14.06 s). The 9 ignored are 1 older unit test, 4 `trickle`, 4 long keep alive |
| Full broker suite, run 1 and 2 (`--no-fail-fast -- --test-threads=1`) | each run: 182 unit, 24 `broker_compat`, 37 `broker_matrix`, 1 `load_ci` (about 361 s), 1 `net_blocked_write`, 81 `net_fake` (about 382 s serial), 1 `net_fd_leak`, 1 `net_keepalive_tasks`, 36 `net_mosquitto`; 0 failed; `soak` 1 ignored (`s3-full-1.txt`, `s3-full-2.txt`) |
| Full broker suite with the default test threads (after the `LIMITS` locks went) | the same counts, 0 failed. `broker_matrix` 8.89 s, `net_mosquitto` 9.85 s, `net_fake` 68.59 s (`s3-full-parallel.txt`) |
| `net_mosquitto` alone, 10 runs, default test threads | 10 runs of 36 passed, 0 failed (`s3-mosquitto10.txt`) |
| Ignored tests: `trickle` (4) | 4 passed in 61.16 s (`s3-trickle.txt`). Run before the later tests were added; the trickle code did not change after |
| Ignored tests: keep alive 60 s and 70 s (4) | 4 passed in 147.72 s (`s3-longka.txt`) |
| Soak, 10 min | see the next section (`s3-soak.txt`) |
| `examples/flood.rs`, release, Mosquitto 18830, 10 000 msg/s, 100 topics, 10 s | the last five one-second reports show 9998 to 10004 msg/s; `done: sent 100100, results ok 100100, failed 0` (`s3-flood.txt`) |
| `cargo +nightly fuzz build` (4 targets) | ok (`s3-fuzz-build.txt`). Plain `cargo fuzz build` fails here (needs the nightly toolchain, as in CI) |

EMQX and HiveMQ are stopped (`docker compose -f tests/broker/compose.yml stop emqx hivemq`). Mosquitto still runs. No image was pruned.

### Soak, 10 minutes

`SOAK_SECS=600 cargo test --release -p navigator-core --features broker-tests --test soak -- --ignored --nocapture` against Mosquitto (`tests/broker/up.sh mosquitto`), 500 msg/s, 2 000 topics, 6 broker restarts. Result: 1 passed in 603.19 s.

```
soak: 603s sent 295240 received 295240 lost_in_restarts 0 dup 0 restarts 6 windows [1, 1, 1, 3, 3, 1]
publisher_connects 7 subscribed 7 reconnected 6 net_drops 0 event_drops 0 window_drops 0 store_drops 0/0
topics 2001 max_used 114551099 B (hard limit 167772160 B) rss_growth 16863232 B (max rss 95 MiB)
slices 17131 slice_max 368.959us errors_in_windows 0
```

"Messages flow again" after 1.6 to 3.3 s in the six restarts. RSS was 22 MiB at 60 s, 52 MiB at 180 s, 79 MiB at 300 s (baseline) and 95 MiB at 540 s and 600 s. The RSS number is not comparable with P7: the publisher is now a second `Net` in the same process (it was the old client). S4 does the comparison.

### Deviations and open points

1. The plan says nothing about the test bounds of the fake-broker cases beyond the table. Each new test uses the plan bound (the stalled write 12.5 s, 17 s with keep alive 2 s; the half frame 13 s; the half WS frame 6.5 s; the trickle 68 s; ka 2 x ka x 1.125 + 1 s, +2 s over WS). Every bound also has a lower bound (the end must wait for the timer), so an immediate error cannot pass.
2. Stalled write on `Net`: the operation timer (10 s from the submit, final) and the write timeout (10 s, temporary) race in `stalled_write`, so that test accepts two ends. The write timeout alone is proven by `write_timeout_alone` (Round 1, finding 3).
3. A WS or WSS half frame with keep alive 2 s ends with `Reconnecting("The connection timed out.")`, not the general "The connection closed ..." text. The test pins the observed text. A Text frame ends with `Failed("Network error: WebSockets Text frames are not supported")` (final). These are the texts that S2 gives. The plan does not name them. The old client's behaviour for a Text frame was not compared (S1 finding 6).
4. v5 and Mosquitto: the first copy of a message that Mosquitto forwards to `Net` has 3 more bytes. The first explanation (Subscription Identifier) was wrong. The cause is a Topic Alias property (Round 1, finding 5).
5. `net_fake` runs in `cargo test --workspace` (no broker needed). Serial it takes about 382 s, so the broker run with `--test-threads=1` is about 16 minutes. The CI broker job passes `--test-threads=1` only for the three broker test files, so its time does not change. `.github/workflows/ci.yml` is not changed: no command or test file name in it changed. The new tests run in the existing `cargo test --workspace` step. The Windows and macOS CI jobs run `net_fake` for the first time (the stalled write fills socket buffers with about 20 MB; loopback buffers differ by OS). `net_fd_leak` is Linux only.
6. The full broker run was done in the background and polled, not in the foreground: one foreground call is limited to 10 minutes and a run takes about 16.
7. Ignored tests (more than 30 s): 4 `trickle` (60 s), 4 keep alive 60 s and 70 s (about 2 to 2.5 minutes). The other `net_fake` tests take at most 25 s (the fd test).
8. `Publisher` makes the publisher a full `Net`. A QoS 0 publish through `Net` needs a command, a result event and a drain. It kept the 10 000 msg/s of `load_ci` and `flood` (9998 to 10004 msg/s). `load_ci` passed with its unchanged assertions (95 % of the rate, no drops, no missing, merge slice p99 and maximum, RSS growth).
9. Open: delete the spike (S5 decision), the open items under "Not moved", and the Windows and macOS runs of the new tests (CI, S6).

### Round 1 fixes

The Codex review of S3 (round 1) found 10 points. Each point is fixed at its root. No product code changed in `src/` (the two temporary mutations in `src/` were restored). No test found a bug in the product. Logs are in the scratchpad as `s3r1-*.txt`.

| # | Finding | Fix | Evidence |
|---|---|---|---|
| 1 | Soak: publisher failures were taken out of `sent`. They could hide loss. | `sent` is the number of accepted publishes again. The publisher thread counts every result that is not `Sent` in `failures`, and in `failures_outside` when it arrives while the main loop says "not in a restart window or its grace time" (a shared flag). The summary line shows `publisher_failures` and `failures_outside_windows`. The test asserts `failures_outside == 0` and `failures <= allowed` (the same allowance as the loss in the restart windows). `lost_in_restarts` (accepted - received) includes every failure. | `soak.rs`. See "Soak after Round 1". |
| 2 | The blocked-write precondition could pass with no blocked write: the drain stopped at the first event that was not a result. | New helper `drain_until_quiet` takes every event until none comes for 500 ms (it replaces the fixed 500 ms sleep). Fewer results than sends proves that a send waits for the write (the broker reads nothing and QoS 0 gives a result only when the buffer takes the message). All results so far must be `Sent`. After the disconnect or the write timeout, at least one send must report `LostOnDisconnect`. Same fix in `stalled_write` and `disconnect_during_a_blocked_write`. `net_blocked_write.rs` already drained every event. It keeps its fixed 500 ms wait; it only gained the `LostOnDisconnect` check. | `net_fake.rs`, `net_blocked_write.rs` |
| 3 | The operation timer could hide a broken write timeout. | New tests `v3_write_timeout_alone_is_a_temporary_failure`, `v5_...`. The broker reads nothing. The test sends QoS 0 messages one at a time and waits 4.5 s after each result. The first send with no result in 0.7 s is the blocked one. ntex starts its write timeout when the buffer goes above the high watermark and does not drain (a trace in an exploring run showed `Start write timer` when the last message was taken, and `Write backpressure timeout` 10.01 s later: `s3r1-write-timer-log.txt`; the trace code is not kept). Let T0 be the stall. The write timeout ends the attempt at T0 + 10 s to T0 + 12 s (timer up to 1 s late, then up to 1 s of I/O shutdown). The blocked send comes at about T0 + 4.5 s, so the end is 5.5 s to 7.5 s after it. The operation timer of that send would end it 10 s after it. The test requires: the end comes less than 9.8 s after the blocked send, the state is `Reconnecting` with "The broker did not read our data for 10 s.", and the blocked send reports `LostOnDisconnect` (not `TimedOut`). The first version waited 2 s only; a review of the margin showed that the two ends could overlap, so I raised it. | Mutation: `WRITE_TIMEOUT` changed from `Seconds(10)` to `Seconds(0)` in `src/net/wire.rs`. Both tests failed: the attempt ended 10.00 s after the blocked send with `Failed("The broker did not answer for 10 s. Connect again.")` (`s3r1-mut-write-timeout.txt`, final version of the test). Restored. |
| 4 | The PING timing checks were weaker than the spike. | `missing_pingresp_full` uses the spike bounds again. First PINGREQ: `ka - 0.2 s` to `ka x 1.125 + 0.5 s` (spike: same). End of the attempt: at least `2 x ka - 0.1 s` (spike: same) and at most the plan bound `2 x ka x 1.125 + 1 s` (+ 1 s over WS and WSS) plus 0.3 s. The 0.1 s and 0.2 s cover the delay between the client event and the test clock. The 0.3 s is the poll step of the test loop that sends publishes. The old limits were `1.5 x ka` and `0.75 x ka`. | `net_fake.rs`. All 8 wires with ka 2 s pass (`s3r1-workspace.txt`). The ka 60 s and 70 s ignored tests passed with the final bounds (`s3r1-longka.txt`, "Gate after Round 1"). |
| 5 | The v5 1 MiB test used a wrong size adjustment (Subscription Identifier). | I captured the real packets with a raw MQTT 5 client and with a TCP proxy between `Net` and Mosquitto. Mosquitto adds nothing for a client that announces no Topic Alias Maximum (raw client: forwarded size equals sent size, for 1 MiB - 3, 1 MiB - 6 and 1 MiB). `Net` announces Topic Alias Maximum 16 in CONNECT. For the first message on a topic, Mosquitto adds the property Topic Alias (id 0x23 and 2 bytes: 3 bytes; property length 3 instead of 0). For the next message on that topic it sends an empty topic with the same alias. So the copy of the 1 MiB packet is 1 MiB + 3 - 42 (the topic) = 1 048 537 bytes, and it is delivered. The report said that Mosquitto drops this copy. That was wrong. The new test `v5_forwarded_copy_carries_a_topic_alias` puts a proxy in front of Mosquitto and asserts the sizes on the wire: sent 1 MiB - 3 and 1 MiB; first copy exactly 1 MiB with property bytes `03 23 00 01`; second copy topic length 0 and size 1 MiB + 3 - topic length. `one_mib_round_trip` now receives both copies (it had skipped the second). | `broker_matrix.rs`. Raw capture: `sniff.py` output (`s3r1-sniff.txt`) and proxy log (`s3r1-proxy.txt`). |
| 6 | `Publisher` accepted `Sent` for QoS 1. | `Publisher` keeps the QoS of each open operation. QoS 0 needs `Sent`. QoS 1 needs `Acknowledged`. Any other result, and a result with an unknown ID, goes to `failed`. `publish_plain` (QoS 1) therefore needs a PUBACK. | `tests/common/mod.rs`. `net_mosquitto` `publish_plain` users pass in the broker run. |
| 7 | `net_keepalive_tasks` depended on the setup speed. | The capture logger records the time of each "start ... keep-alive task" and "stopping keep-alive task" record. Each phase waits (at most 10 s) for 20 starts, then for 20 stops. Each task (sorted start and stop pairs) must live 1.8 s to 4 s: not less than one interval (so a stop before the first tick cannot pass), and not more than the bound. A phase first waits until every earlier start has its stop. | `net_keepalive_tasks.rs`. Passed in the workspace run. |
| 8 | Fixed sleeps did not synchronize the fake broker. | `big_header`: the fake waits for a `go` message that the test sends after it saw `Connected`. `websocket_frames`: the fake sends the Text frame after `go`, which the test sends after it received the fragmented packet. `disconnect_while_a_half_frame_waits`: `go` after `Connected`, then the fake writes the half frame and sends a `sent` confirmation, and the test disconnects after that. No `sleep` is left in a fake broker handler except the deliberate pacing of the trickle test and `stop_reading`. The client may still process the half frame after the disconnect starts. The disconnect must be prompt in both cases. | `net_fake.rs` |
| 9 | No regression test for v5 outgoing with no Maximum Packet Size. | New test `a_v5_publish_over_the_local_limit_is_sent_when_the_broker_announces_none`. The CONNACK has no properties. A PUBLISH of `max_packet + 1` bytes and one of `2 x max_packet` are sent whole (the fake measures the packet and sends the PUBACK) and get `Acknowledged`. A message of 256 MiB + 1 byte (the MQTT maximum is 268 435 455 + 5 bytes) is refused before the send with "The message is larger than the MQTT limit (256 MiB)." and the fake sees no third PUBLISH. This is the rule of plan line 34. I did not send a packet of exactly 256 MiB: it needs more than 512 MB of memory. | Mutation: `Net::send` passed `broker_max_packet().or(Some(1 MiB))`. The test failed (the broker got no PUBLISH, `s3r1-mut-v5-nolimit.txt`). Restored. |
| 10 | The merge timing was not checked: all `load_ci` runs used a debug build. | I ran `LOAD_WARMUP_SECS=10 LOAD_SECS=60 cargo test --release -p navigator-core --features broker-tests --test load_ci -- --nocapture` (release build, where the p99 and maximum checks are active). Result: passed. Sent 700 400 (10 001 msg/s), received 700 400, missing 0, dup 0, drops 0, RSS growth 1 302 528 B in 60 s, 2 002 merge slices, p99 1.65 ms (limit 4 ms), maximum 1.83 ms (limit 8 ms). The 5-minute default run was not made. | `s3r1-load-ci-release.txt` |

### Gate after Round 1

Logs are in the scratchpad (`s3r1-*.txt`). Mosquitto, EMQX and HiveMQ were started with `tests/broker/up.sh` (`s3r1-up.txt`).

| Check | Result |
|---|---|
| `cargo fmt --all --check` | ok, run again after the last edit (`s3r1-fmt.txt`) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | ok, run again after the last edit (`s3r1-clippy.txt`) |
| `cargo deny check` | ok (`s3r1-deny.txt`). It ran before the last two edits (`net_fake.rs` pause, `soak.rs` window flag). They change no dependency. |
| `cargo test --workspace` | 423 passed, 0 failed, 9 ignored. Run twice: before the last two edits (`s3r1-workspace.txt`) and after them (`s3r1-workspace2.txt`). `navigator-core`: 182 unit, 1 `net_blocked_write`, 84 `net_fake` (83.82 s), 1 `net_fd_leak`, 1 `net_keepalive_tasks`. The 9 ignored are 1 older unit test, 4 `trickle`, 4 long keep alive |
| Full broker suite (`--no-fail-fast -- --test-threads=1`) | 368 passed, 0 failed, 10 ignored (`s3r1-broker.txt`): 182 unit, 24 `broker_compat`, 38 `broker_matrix` (new: the Topic Alias proxy test), 1 `load_ci` (debug, 360.71 s), 1 `net_blocked_write`, 84 `net_fake` (411.24 s serial), 1 `net_fd_leak`, 1 `net_keepalive_tasks`, 36 `net_mosquitto`. The ignored are 1 unit test, 8 `net_fake`, 1 `soak`. This run started before the last two edits (the pause in `write_timeout_alone` 2 s to 4.5 s, and the soak window flag). Neither edit touches a broker test file. `soak` is not in this run |
| `net_fake` alone, 10 runs, default test threads | 10 runs of 84 passed, 0 failed, 8 ignored, each 83.8 s (`s3r1-netfake-1.txt` to `-10.txt`). These ran after the last edit to `net_fake.rs` |
| `write_timeout_alone` mutation (`WRITE_TIMEOUT` 0) | both tests fail with the final test (`s3r1-mut-write-timeout.txt`). Restored (`const WRITE_TIMEOUT: Seconds = Seconds(10)` checked) |
| v5 no-limit mutation (`Net::send` applies 1 MiB when the broker announces none) | the test fails (`s3r1-mut-v5-nolimit.txt`). Restored |
| `load_ci`, release, `LOAD_WARMUP_SECS=10 LOAD_SECS=60` | passed: 10 001 msg/s, no missing, no drops, merge p99 1.65 ms, max 1.83 ms (`s3r1-load-ci-release.txt`) |
| Ignored tests: keep alive 60 s and 70 s (4), final bounds | 4 passed in 147.72 s (`s3r1-longka.txt`) |
| Ignored tests: `trickle` (4) | not run again. The code and bounds of these tests did not change in Round 1 |
| `examples/flood.rs`, `cargo +nightly fuzz build` | not run again. Round 1 changed neither |

EMQX and HiveMQ are stopped (`docker compose -f tests/broker/compose.yml stop emqx hivemq`). Mosquitto still runs. No image was pruned. The disk became full once during the work (the `target` directory is 26 GB). I deleted `target/debug/incremental` (a cache). Then I found that `net_fake.rs` had been cut short by the full disk, and I rebuilt its end from an earlier copy; the 10 runs and the workspace run above are of the rebuilt file.

### Soak after Round 1

`SOAK_SECS=600 cargo test --release -p navigator-core --features broker-tests --test soak -- --ignored --nocapture` against Mosquitto, 500 msg/s, 2 000 topics, 6 broker restarts (`s3r1-soak.txt`). Result: 1 passed in 603.17 s.

```
soak: 603s sent 295970 received 295910 lost_in_restarts 60 publisher_failures 0 failures_outside_windows 0
dup 0 restarts 6 windows [1, 3, 1, 1, 1, 1] publisher_connects 7 subscribed 7 reconnected 6 net_drops 0
event_drops 0 window_drops 0 store_drops 0/0 topics 2001 max_used 114551099 B (hard limit 167772160 B)
rss_growth 16769024 B (max rss 95 MiB) slices 17166 slice_max 351.429us errors_in_windows 0
```

`sent` is now the number of accepted publishes. 60 messages were lost in the restarts (the first run showed 0 because it subtracted failures). No publisher failure was reported, and none came outside a restart window. The allowance for the loss is the same as before.

### Round 2 fixes

Logs are `s3r2-*.txt` in the scratchpad.

| # | Finding | Fix |
|---|---|---|
| 1 | `flood` exited 0 after failed publishes, and counted local rejections in its rate. | The rate and the total use accepted minus failed (after a drain). After `settle`, `flood` exits with an error if `failed` is not empty or `done != accepted`. Run against Mosquitto on 18830, 5 000 msg/s, 8 s: 40 050 accepted, 40 050 ok, 0 failed, exit 0 (`s3r2-flood.txt`). The error path was not run against a failing broker. |
| 2 | Failures drained in the final `settle` of the soak publisher skipped the outside-window count. | The same accounting (`failures_outside` when the window flag is false) now runs after the final `settle`. Only this accounting changed in `soak.rs`, so I did not run the soak again. It compiled under clippy only. The Round 1 soak result stands for everything else. |
| 3 | No invalid Topic Alias test. | New test `a_v5_publish_with_an_invalid_topic_alias_is_not_delivered` (fake broker, v5 TCP). The broker sends a valid control PUBLISH (topic `a/ok`, alias 1), which must arrive. Then it sends one invalid PUBLISH per case: alias 0, alias 17 (above our announced 16), and alias 2 with an empty topic (unknown). Each case ends the connection with `Failed("The broker sent data that is not valid MQTT 5.")`, and only `a/ok` is ever in `Net::incoming`. |

Gate: `cargo fmt --check` ok, `cargo clippy --workspace --all-targets --all-features -- -D warnings` ok, `cargo test --workspace` ok (0 failed), `net_fake` 5 runs of 85 passed, 0 failed, 8 ignored, 83.8 s each (`s3r2-netfake-1.txt` to `-5.txt`). The new test was not mutation-checked (no production change to revert).


### Round 3 fix

- Codex found that the v5 `half_payload` packet had no property length byte. The decoder read the first payload byte as a property length, so the test stopped in the properties, not in the payload.
- The test now adds an empty property length (`0x00`) for MQTT 5.
- `cargo test -p navigator-core --test net_fake half_`: 20 passed, 0 failed (log `s3r3-half.txt`). The v5 `half_payload` tests pass with the read timeout text, so the payload read stall now ends the connection.
