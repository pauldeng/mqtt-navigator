# Architecture

This page gives the high-level design. The full design and the reasons are in [the plan](docs/plans/2026-11-10-plan.md). The decisions are in [docs/adr/](docs/adr/).

## Product

The app has two major features: **Subscribe** and **Publish**. Received messages show in two views of one store: a topic tree and a newest-first message list. All other parts (profiles, TLS, history, updates) support these two features. The UI uses gpui-kit components only (no webview). See [docs/ui-design.md](docs/ui-design.md).

## Crates

| Crate | Contents | Rule |
|---|---|---|
| `navigator-core` | MQTT session, topic store, decoders, profiles, settings | It must not depend on gpui. It forbids `unsafe`. |
| `navigator-app` | gpui-kit UI, updater, `main()`. The binary is `mqtt-navigator`. | It is the only crate that knows the UI. |

## Threads

```
 gpui main thread                         "net" thread (tokio, current_thread)
 ┌──────────────────────────┐  commands  ┌─────────────────────────────────────┐
 │ UI and TopicStore        │──bounded──▶│ Supervisor: state machine,          │
 │                          │── cancel ─▶│ reconnect, resubscribe              │
 │ drain task:              │            │ Session: ms-mqtt-client             │
 │  wait, merge ≤ 4 ms,     │◀─ bounded ─│ messages (try_send; full = drop++)  │
 │  notify, wait 33 ms      │◀── watch ──│ state + bounded result log          │
 └──────────────────────────┘            └─────────────────────────────────────┘
```

- The net thread never waits for the UI. A full queue drops the message and counts the drop.
- The UI merges messages at frame rate. It does no work for each separate message.
- Each connection has a generation number. The UI ignores data from an old generation.
- A panic in the session task stops the session, not the app.

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
- The first window opens before profiles and settings load. They arrive as an event.
- Targets:
  - input-to-frame latency below 50 ms (p95) at 10k msg/s;
  - frame time below 16 ms (p95);
  - no wake-ups when idle and disconnected.

## Memory

One budget (default 128 MiB) covers all stored data. There are also limits for each topic and for the topic count. The message list (up to 10 000 entries) is inside the same budget. When the store is full, it evicts in this order: the oldest message-list entries, then the oldest history, then the oldest latest-value payloads. See the TopicStore section of the plan.

## Persistence

- Settings: JSON with `schema_version`. Writes are atomic.
- Secrets: system keyring only.
- Logs: platform log folder, size-capped. Logs never contain payloads or secrets.
