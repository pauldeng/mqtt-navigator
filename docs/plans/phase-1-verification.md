# Phase 1 verification (tracer bullet)

Date: 2026-11-11. Machine: Ubuntu (GNOME, Wayland), local Mosquitto on 127.0.0.1:1883. Release build, `lto = "thin"`.

## Done

- `navigator-core`:
  - `store.rs`: arena topic tree, latest value, truncation by copy, topic cap, node budget, total budget with admission before insert and eviction of the oldest values. 12 tests, including a budget audit after every insert.
  - `net.rs`: net thread (tokio `current_thread` + `LocalSet`). It provides connect, subscribe, unsubscribe, publish (QoS 0 and 1, 10 s timeout), disconnect by a cancel signal, connection generations, bounded queue to the UI (count and bytes) with a drop counter, and panic containment. 6 integration tests against Mosquitto, plus a hostile broker that ignores Maximum Packet Size.
  - `examples/flood.rs`: load generator.
- `vendor/ms-mqtt-client`: local patch (ADR 0002). The packet size is checked before memory is reserved, and the incoming queue is bounded.
- `navigator-app`: gpui-kit window with title bar (state and Connect / Cancel / Disconnect), subscriptions with SUBACK state, a virtual topic tree on `uniform_list` (ADR 0003), latest value, publish composer (QoS 0/1, result text), and a status bar (rate, topics, memory, drops, "Not encrypted" / "Insecure TLS"). The theme follows the system. There is an opt-in metrics log (`MQTT_NAVIGATOR_METRICS=1`).

## Checks run

| Check | Result |
|---|---|
| `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` | pass |
| `cargo test --workspace -- --test-threads=1` | pass: 33 tests (4 app, 18 core unit, 11 Mosquitto and fake-broker), in parallel and with `--test-threads=1` |
| `cargo deny check` | pass (unmaintained crates checked for direct dependencies only; `0BSD` allowed) |
| Vendored client tests (`cargo test --lib --features websockets` in the vendor folder) | pass: 320 |

## Measurements (baseline)

| Item | Result | P1 target |
|---|---|---|
| Load: 10k msg/s, 10k topics, 100 B, 60 s warm-up + 5 min | 0 drops; average 9 999 msg/s | no drops |
| Frame time (render start to end of frame) | median of per-second p95: 5.8 ms; worst second p95: 13.3 ms; worst frame: 18.9 ms | p95 < 16 ms |
| Merge time on the UI thread | p95 ≤ 0.62 ms; max 1.64 ms | ≤ 4 ms |
| RSS during load | 126 MiB from start to end of the 5 min (no growth) | growth < 10 MiB |
| Overload: 50k msg/s for 60 s | 0 drops; 48% of one core; RSS 127 MiB; worst frame 13.7 ms | drops counted; UI responsive |
| 100k distinct topics at 10k msg/s for 60 s | 0 drops; store 40 MiB; RSS 174 MiB (flat); worst frame 15.1 ms | — |
| Cold start to first frame (warm disk cache, 3 runs) | 197–208 ms | provisional budget: 300 ms |
| Idle, connected, no traffic (30 s) | our threads: 0 wake-ups; CPU 0.01 s | no periodic wake-ups |
| Idle, disconnected (60 s) | our threads: 0 wake-ups; CPU 0.01 s | no periodic wake-ups |
| Idle RSS | 127–130 MiB | provisional budget: 160 MiB |
| Release binary (Linux x64, stripped) | 43.1 MB (+ 1.3 MB separate `.dwp` in debug info test) | baseline only |

Notes:
- The overload test did not reach the drop limit, because the app kept up at 50k msg/s. The integration test `queue drops` proves drop counting with a small queue.
- The first frames after start take up to 80 ms (window creation). This is before any traffic.
- Two third-party threads (`blocking-1`, `blocking-2`, from a gpui dependency) wake about once a second each, also when idle. Their CPU use is too small to measure. Find the source in P7.

## Not checked

- Windows and macOS builds, including the vendored OpenSSL build. This needs a CI run, and nothing is pushed yet.
- TLS, mTLS, WS and WSS paths. The code compiles. P2 adds the real-broker tests.
- Input-to-frame latency. There is no input automation on this Wayland session. The frame metric is a proxy.
- Software-renderer (GPU fallback) check. Moved to P7 (advisor decision D5).
- A screenshot of the window: GNOME does not allow programmatic screenshots. The owner can look at the running app.

## Advisor decisions used

Codex decided D1 (own flat-row tree, ADR 0003), D2 (vendored patch, ADR 0002), D3 (panic containment in a `LocalSet`; drop the client after a panic), D4 (bound the client queue at its enqueue point) and D5 (thin P1: no Tree benchmark; bring panic containment and basic memory bounds forward).

## Codex review 1: findings and fixes

| # | Finding | Fix |
|---|---|---|
| 1 | Queue byte limits counted only payload bytes | One `net::queue_cost` (payload, topic, properties, fixed overhead) for admission and release. The vendored queue also charges topic and properties. Test: property-heavy messages, drops, and recovery after the drain. |
| 2 | WS and WSS used the default WebSocket limits (16 MiB frame, 64 MiB message) | The vendored `tokio_ws.rs` sets the frame and message limits to the packet limit. Test: a fake WS server sends an oversized frame. |
| 3 | No hard frame bound for structural work and payload formatting | Topics deeper than 64 levels are dropped (`store::MAX_TOPIC_LEVELS`). The details pane formats a payload once for each message, and at most 64 KiB (P5 adds background formatting). |
| 4 | A queued Connect could run after a Disconnect | Commands carry a cancel epoch (`Net::send`). The supervisor ignores commands older than the last Disconnect. |
| 5 | New-generation data could be dropped before the status update | The UI accepts data by generation from any source (`accept`): older is dropped, newer resets the view. |
| 6 | A stale row callback could panic or select another topic | Row callbacks carry the generation. The store has a checked `get`. |
| 7 | Pending operations were not capped; subscribe had no timeout | At most 256 pending operations. Subscribe and unsubscribe time out after 10 s. |
| 8 | Blocking work on the UI thread (metrics I/O, `Net` drop waited up to 1 s) | Metrics I/O runs on a background thread. `Net::drop` does not wait. On quit, the app awaits `Net::shutdown` (gpui allows 200 ms). |
| 9 | Exit evidence incomplete | See "Not checked" above. These items are deferred, not passed. |

Load test after the fixes (10k msg/s, 10k topics, 60 s warm-up + 60 s): 0 drops, RSS 130 MiB flat, frame p95 median 6.0 ms, worst 13.3 ms, worst frame 15.4 ms, first frame 215 ms.

## Codex review 2: findings and fixes

| Finding | Fix |
|---|---|
| High: a timed-out operation left its in-flight state in the client, so the operation cap could be passed | A timeout of any operation ends the session (`Failed: The broker did not answer for 10 s.`). The client is dropped. Test with a silent broker. |
| Disconnect, then Connect: the old cancel could stop the new session | Each session keeps its start epoch. A cancel stops only older sessions. Test: 10 rounds. |
| One insert could evict about 100k tiny values | At most 1 024 evictions for each insert (`store::MAX_EVICTIONS_PER_INSERT`); otherwise the message is dropped. The old value of the topic is released only when the new value is stored. |
| UNSUBACK reasons were ignored | A failed UNSUBACK gives `Unsubscribe rejected: <reason>`. Test with a fake broker. |
| Tests changed the global client limits in parallel | One lock for each integration test. Tests pass in parallel and in series. |
| The details cache could show an evicted payload | The cache key includes the stored length and the truncated flag. |

Finding 9 (evidence): Codex accepted the deferrals in "Not checked" (CI for Windows, macOS and OpenSSL; input latency; TLS and WS real-broker tests in P2; GPU fallback in P7). They do not block P1.

## Codex reviews 3 to 5

- Round 3: a failed replacement could evict the topic's own old value (fixed: eviction skips the topic that is being replaced; test `failed_replacement_keeps_the_old_value`), and the composer trimmed topic names (fixed: spaces are kept).
- Round 4: a late result of an older publish could replace the result of the newest send (fixed: the result line follows the newest `op_id` only).
- Round 5: no issues.
