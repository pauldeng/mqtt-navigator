# ADR 0002: Local patch of ms-mqtt-client

Status: accepted (2026-11-11). Advice: Codex (gpt-6.1-sol).

## Problem

`ms-mqtt-client` 0.1.0 has two gaps that a faulty or hostile broker can use:

1. `io/reader.rs` reserves the packet's remaining length (up to 256 MiB) before it reads the body. The advertised Maximum Packet Size is not enforced.
2. Incoming publishes go into an unbounded channel. If the application is slow to drain it, memory grows without limit.

## Decision

Vendor the crate in `vendor/ms-mqtt-client` and use it through `[patch.crates-io]`. The patch is small and marked `LOCAL PATCH`:

- `local_limits.rs`: process-wide limits for packet size and for the incoming queue (count and bytes), and a drop counter.
- `io/tokio_ws.rs`: open the WebSocket with a frame and message limit equal to the packet limit.
- `io/reader.rs`: reject a packet larger than the limit before memory is reserved. The connection then closes with an I/O error.
- `session.rs` and `client.rs`: admit a publish into the queue only if it fits, and free its space when `Receiver::recv` returns it.

We still advertise the Maximum Packet Size in CONNECT.

## Consequences

- A dropped QoS 1 publish is acknowledged when its token drops. QoS 1 therefore does not guarantee that the app shows the message.
- The cost of a queued publish is its payload, topic and properties (user properties, correlation data, content type, response topic) plus a fixed overhead. The client queue and the app queue each charge their own cost. Each one frees exactly what it charged. A message with an empty payload and many properties therefore still fills the budget.
- The limits are global, because the app runs one connection at a time.
- On each upstream release: copy it, apply the patch again, and run the crate tests (`cargo test --lib --features websockets` in the vendor folder). Remove the fork when upstream fixes both gaps.
- WebSocket frames and messages are bounded by the packet limit before the stream buffers them. Without a limit, tungstenite uses its defaults (16 MiB frame, 64 MiB message). The unit test uses a fake WS server on the plain `ws` path. The `wss` path uses the same configuration and is not tested separately.
