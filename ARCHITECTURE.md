# Architecture

This page gives the high-level design. The full design and the reasons are in [the plan](docs/plans/2026-11-10-plan.md). The decisions are in [docs/adr/](docs/adr/).

## Product

The app has two major features: **Subscribe** and **Publish**. Received messages show in two views of one store: a topic tree and a newest-first message list. All other parts (profiles, TLS, history, updates) support these two features. The UI uses gpui-kit components only (no webview). See [docs/ui-design.md](docs/ui-design.md).

## Crates

| Crate | Contents | Rule |
|---|---|---|
| `navigator-core` | MQTT session, topic store, decoders, profiles, settings | It must not depend on gpui. It forbids `unsafe`. |
| `navigator-update` | Update check, signed download, and the swap and rollback steps. | It has no gpui. `swap.rs` uses only `std`. |
| `navigator-app` | gpui-kit UI, updater, `main()`. The binary is `mqtt-navigator`, the only program that ships. | It is the only crate that knows the UI. |

## Threads

```
 gpui main thread                         "net" thread (tokio current_thread + ntex)
 ┌──────────────────────────┐  commands  ┌─────────────────────────────────────┐
 │ UI and TopicStore        │──bounded──▶│ Supervisor: state machine,          │
 │                          │── cancel ─▶│ reconnect, resubscribe              │
 │ drain task:              │            │ Session: ntex-mqtt (v3 or v5)       │
 │  wait, merge ≤ 4 ms,     │◀─ bounded ─│ messages (try_send; full = drop++)  │
 │  notify, wait 33 ms      │◀── watch ──│ state + bounded result log          │
 └──────────────────────────┘            └─────────────────────────────────────┘
```

- The net thread never waits for the UI. A full queue drops the message and counts the drop.
- The UI merges messages at frame rate. It does no work for each separate message.
- Each connection has a generation number. The UI ignores data from an old generation.
- A panic in the session task stops the session, not the app.

## MQTT client

The session uses `ntex-mqtt` ([ADR 0006](docs/adr/0006-ntex-mqtt.md)). Only `navigator-core/src/net.rs` and its submodules use ntex.

- ntex runs on the net thread, on tokio, inside one ntex reactor scope. The ntex types are not `Send`.
- Two adapters (MQTT 3.1.1 and MQTT 5) sit behind one private interface. The supervisor, the queues and the timers do not know the version.
- The supervisor owns reconnect. Each attempt makes one connector call and one client.
- Our own dialer does DNS and TCP under one connect deadline. TLS uses OpenSSL (`tls.rs`) through `ntex-tls`. WebSocket uses `ntex::ws`.
- Every deadline is set in our code: connect, write, frame read rate, keep alive and operation. Each one has a fake-broker test.
- `NetLimits.timers` (`Timers`) holds the time limits. The default is the shipped value: connect 10 s, operation 10 s, write 10 s, TLS handshake 5 s, WebSocket upgrade 5 s, frame check 10 s, whole packet 60 s. Only tests set shorter values.
- The keep alive comes from the profile (default 60 s). The app finds a silent broker in about 2 times the keep alive, plus timer and shutdown delays. Keep alive 0 turns this off, unless an MQTT 5 broker sets its own.
- Reconnect waits a base of 1 s that doubles for each retry up to 30 s. Jitter of 20% either way gives waits of 0.8 s to 36 s. A connection that stays up for 30 s resets the count.
- Limits come from the profile and `NetLimits`: the incoming packet limit is 1 MiB on both versions.
- ntex logs through the `log` crate. The app installs no `log` logger, so these lines do not reach the log file.

## Testing

- Unit tests and the fake-broker tests (`navigator-core/tests/net_fake.rs`) run in `cargo test --workspace`. They need no broker. The fake broker sends and withholds bytes on purpose. Each transport and each MQTT version has a case. Each time limit has one case with the shipped value. The other cases use short test-only `Timers`.
- Real-broker tests run with `--features broker-tests` and need Docker (see [tests/broker/README.md](tests/broker/README.md)). Test against a real broker first. Use the fake broker for faults that a real broker does not make.
- Three fake-broker tests run with the shipped timer values and take minutes: the 60 s packet cap and the keep-alive cases. They are `#[ignore]`, so `cargo test --workspace` skips them. Run `cargo test -p navigator-core --test net_fake -- --ignored` after a change to the timers, to the frame read rate or to the ntex version.
- The Mosquitto services are: the main one on ports 18830 to 18838, and `mosquitto-limits` on 18860 (Server Keep Alive, Receive Maximum and Maximum Packet Size). EMQX and HiveMQ CE run the same checks in `broker_compat`.
- The broker tests run in parallel. Each test has its own topics and client IDs. A `RwLock` keeps the tests that restart a broker apart from the others.

## Event-driven design

All work starts from an event:
- an MQTT packet;
- a user input;
- an OS window event;
- a timer that one of these events started.

Rules:
- There are no polling loops.
- When the app is idle (no traffic, no input), it does not wake up.
- The 33 ms coalescing timer starts after a message arrives. It stops when the queue is empty.
- The msg/s display refreshes once a second, but only while messages arrive.
- The reconnect timer runs only in the `Reconnecting` state.
- A settings change starts a write after 500 ms of quiet.
- The update check runs once at start, and when the user selects "Check for updates". It has no periodic timer.
- The first call of `main()` is `navigator_update::startup()`. It applies a staged update, or rolls back a version that never opened a window, and starts the right copy again. It does no network work. See [ADR 0005](docs/adr/0005-release-and-update.md).

## Responsive UI

- The UI thread does no blocking I/O. These jobs run on a background executor or on the net thread, and their result comes back as an event:
  - file reads and writes;
  - keyring access;
  - certificate loading;
  - DNS;
  - update downloads.
- Message merging is time-sliced (≤ 4 ms each frame), so input is never queued behind a message flood.
- Only visible rows render.
- The app decodes and formats a large payload on a background executor. A new selection cancels the old job.
- Startup loads the settings and the profiles in the background, applies the theme, then opens the window. The first frame has the saved theme and pane widths. The app reads the keyring only when a connection needs a secret, in the background.
- Targets:
  - input-to-frame latency below 50 ms (p95) at 10k msg/s;
  - frame time below 16 ms (p95);
  - no wake-ups when idle and disconnected.

## Memory

One memory limit covers all stored data: default 128 MiB, settable from 32 MiB to 2 GiB. There are also limits for each topic and for the topic count.

- Storing a message never evicts. A message is dropped only above budget + 25%.
- Once per frame, the store evicts in a batch down to 90% of the limit: the oldest history first, then the oldest message-list rows, then the oldest latest values.
- Near the topic cap, the least recently updated leaf topics are removed the same way, so new topics are always accepted.

See the TopicStore section of the plan.

## Persistence

- Settings: JSON with `schema_version` (now 2). Writes are atomic. A profile without the `protocol` field loads as MQTT 5. A new profile uses MQTT 3.1.1. Version 0.1.x does not load schema 2.
- Secrets: system keyring only.
- Logs: platform log folder, size-capped. Logs never contain payloads or secrets.
