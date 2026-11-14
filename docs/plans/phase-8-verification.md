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
