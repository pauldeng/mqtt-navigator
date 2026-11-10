# Phase 3 verification (store and flow control)

Date: 2026-11-11.

## Advisor decisions (Codex)

- D1: a newest-first message list (10 000 rows) with stable row ids; payload `Bytes` are shared, and each holder records its own charge. Eviction order (revised): oldest history, then oldest list rows, then oldest latest payloads.
- D2: incremental list filter; tree filter with a resumable rebuild; one shared 4 ms frame budget; 150 ms text debounce.
- D3: pause freezes the list at a sequence cutoff and the tree as a bounded, charged snapshot; evicted rows become placeholders with one marker.
- D4: the UI records a sent message after `Net::send` succeeds; the net result updates it by `op_id`.
- D5: subscription colour = first matching subscription, cached, for visible rows only; MQTT matching in core.
- D6: own `uniform_list` rows (not `DataTable`).
- D7: a headless load test in the CI broker job.
- D8: result events must not be lost.

## Done

- `store.rs`: per-topic history (100, 64 KiB payloads), message list with sent rows, send status and "Reconnected" markers, per-holder accounting, list and tree filters with `work(deadline)`, pause with `paused_new`, colour cache. `topic.rs`: MQTT 5 matching and filter validation.
- `net.rs`: the event channel cannot be full for a result (reserve check in `Net::send`); `NetStats.event_drops`.
- App: Messages view (default; 2-line or 1-line rows), Tree view, "Messages | Tree" (Ctrl/Cmd+1/2), "All | Received | Sent", Pause and Follow, filter chips and "Clear filters", debounced text filter (Ctrl/Cmd+F, Esc), Show only by clicking a subscription, colour chips, sent rows with status, marker rows, selected-message clone.
- `tests/load_ci.rs`: headless load test (Net + store, UI-like scheduler) in the CI broker job.

## Checks run

| Check | Result |
|---|---|
| fmt, clippy (`--all-features`, `-D warnings`), cargo-deny | pass |
| `cargo test --workspace` | pass: 90 tests (17 app, 61 core unit, 12 Mosquitto and fake-broker) |
| Broker matrix (Docker) | pass: 15 |
| CI load test, 10 s warm-up + 60 s, store budget 64 MiB | 700 100 sent, 700 100 received, 0 missing, 0 drops; saturated after 10 s; RSS growth after saturation 1.4 MB |
| CI load test, full defaults before the history change (300 s) | 3 600 100 received, 0 missing, 0 drops, RSS growth 2.9 MB |
| App, release, 10k msg/s, 10k topics, 60 s | 0 drops; merge p95 0.9–1.1 ms (worst 2.4 ms); frame p95 about 6 ms (worst 8.7 ms); worst frame 18.6 ms in the ramp; store 255 MiB (full budget); RSS 345 MiB |

## Not checked

- No click, key or layout path was run in a real window (no screenshots, no input automation here).
- Windows and macOS (no CI run yet).
- With Follow off, rows move down when new messages arrive (no scroll compensation yet).
- "Removed from the message list" in the details pane is not shown, because the UI cannot tell a filtered row from an evicted row.
- `set_paused(true)` with a filter that shows all 101k nodes takes 4.9 ms (the snapshot is not time-sliced).
- The app's default store budget (256 MiB) gives an RSS of about 345 MiB under a long flood.

## Codex review 1: findings and fixes

Advisor decisions in this review: Follow off must keep the reading position (P3 defect), and the default store budget drops to **128 MiB** (node budget 32 MiB).

| Finding | Fix |
|---|---|
| High: the result reserve did not cover Connect, automatic resubscribes and `Reconnected` | Every result producer checks for free room first. Automatic resubscribes and `Reconnected` wait (50 ms timer, only while blocked). Test: 3 000 filters + 300 QoS 1 publishes with a slow drain; every result once, `event_drops` 0. |
| Shared subscriptions did not match locally | `$share/<group>/<filter>` is validated and matched on the inner filter. |
| Filter work did not share the frame budget | One scheduler: one 4 ms deadline for each frame, shared by merge and `work()`. One continuation only while work remains. |
| Pause did unbounded work | The snapshot is a job that `work()` builds in slices of at most 256 units. |
| Follow off lost the reading position | An anchor row and its pixel offset are kept across merges. |
| View switch dropped the details; no eviction notice | The details selection is separate from the list highlight. `store.row_evicted()` gives "Removed from the message list (memory limit)." |
| Old-generation publish results left rows "Sending" | `Published` is applied by `op_id` before the generation check. |
| Text casing differed between the incremental and the full path | Same casing rules in both; tests with non-ASCII cases. |
| Load test: the RSS interval was not guaranteed | The interval starts after warm-up and saturation and must last `LOAD_SECS`. Merge-slice p99 ≤ 4 ms and max ≤ 8 ms are asserted. |

Measurements after the fixes:

| Check | Result |
|---|---|
| CI load test (10 s + 60 s, 64 MiB) | 700 100 / 700 100, 0 drops of any kind, RSS growth 1.6 MB, merge-slice p99 1.24 ms, max 2.85 ms |
| App, 10k msg/s, 10k topics, 180 s | 0 drops; store 127 MiB; RSS about 245 MiB; merge p95 0.9–1.4 ms (max 2.8 ms); frame p95 6.9–11.4 ms; worst frame 13–22 ms |

## Codex review 2: findings and fixes

| Finding | Fix |
|---|---|
| View switches lost or replaced the details | The details show what the user selected last (`DetailOf::Topic` or `DetailOf::Message`), in both views. A view switch does not change it. |
| A paused tree could stay on live rows after an expand | Every expand and the ancestor expand of a view switch wake the frame loop (`kick_work`). |
| Follow off: sent rows and reconnect markers shifted the reading position | The anchor is kept around `record_sent` and `mark_reconnected` too. |
| Frame time (verdict) | p95 6.9–11.4 ms passes the 16 ms target. No avoidable per-frame work was found. |

These three fixes are in the gpui view. They have no unit tests; they were not run in a real window.
