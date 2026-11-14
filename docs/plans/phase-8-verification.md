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
