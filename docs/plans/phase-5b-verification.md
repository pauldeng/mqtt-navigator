# Phase 5b verification (event-driven and complexity audit, memory limit)

Date: 2026-11-11. Owner requests: the internal logic must be event-driven and async; time and compute complexity must be reasonable; the memory limit is configurable (default 128 MiB, maximum 2 GiB); old data is cleared with the least performance impact (a soft limit is acceptable); the MQTT packet cap is 1 MiB.

## Audit findings and fixes

| Finding | Fix |
|---|---|
| Caret blink woke the app every 500 ms while a text field had focus | Caret blink with an idle timeout. |
| TLS files read and parsed on the net thread on every attempt | Built once for each Connect in `spawn_blocking`, raced against stop; reused on reconnect. |
| `FEED_RETRY` (50 ms) polled for event-log room; the report timer re-armed without room | `Net::events_drained()`: the UI signals after each drain; no timer runs while idle or blocked. |
| Subscriptions panel not virtualised (O(S) elements on every notify) | `uniform_list`, and a cached `SubList` view that redraws only when the subscriptions change. |
| `Subs` O(S²) updates and two clones of all filters on each UNSUBACK | Indexed rows; one shared copy. |
| Subscription colours rescanned all filters on each change | Filters parsed once (`topic::Filter`) and indexed by first level. |
| Tree rows rebuilt fully (O(V)) on each new visible topic | Splice by binary search on path order. |
| Per-insert eviction with a count cap dropped big payloads | Soft budget: insert never evicts; `work()` evicts in batches down to 90%; hard ceiling budget + 25%. |
| Topics were never removed (topic cap reached in long runs) | Least recently updated leaf topics are removed in batches near the cap; `NodeId` has a generation. |
| Filter work starved during a flood | `work()` has a share of every frame and stops 0.75 ms before the deadline. |
| History tab not virtualised; old store dropped on the UI thread; settings cloned on each resize | `uniform_list`; `TopicStore::take()` dropped in the background; debounced resize. |
| Small: `sent_retained` linear scan, O(S×D) at CONNACK | Maps and a `HashSet`. |

New settings: memory limit (32 MiB to 2 GiB, default 128 MiB) in the command palette and the status-bar memory menu; incoming packet cap 1 MiB.

## Checks run

| Check | Result |
|---|---|
| fmt, clippy (`--all-features`, `-D warnings`), cargo-deny | pass |
| `cargo test --workspace` | pass: 207 (67 app, 119 core unit + 1 long-run ignored, 21 Mosquitto and fake-broker) |
| Long run (release, ignored test): 2 M messages, 1 M topics | 2.9 µs per insert; memory at most 2% over the budget and back under it after each `work()`; 0 topic drops; worst `work()` 0.84 ms |
| Broker matrix; CI load test 10/60 | 15 pass; 0 missing, 0 drops, merge-slice p99 1.69 ms |
| Idle 30 s, connected, 2 000 subscriptions | 0 wake-ups on the main thread and `mqtt-net` (gpui `blocking-1`/`-2`: about 2 per second, as before) |
| Flood 10k msg/s, 10k topics, 60 s (same hour) | P5: frame p95 median 10.8 ms, worst second 16.3 ms. Now, 1 subscription: 8.9 / 14.5 ms, merge max 3.5 ms. Now, 2 001 subscriptions: 9.5 / 16.7 ms, merge max 3.6 ms. 0 drops. |
| Release binary | 34 058 840 B (limit 37 317 165 B) |

## Not checked

- No UI interaction in a real window (memory-limit menu, cached subscription list: click, hover, remove, scroll).
- With 2 001 subscriptions, the worst second of frame p95 is 16.7 ms (target < 16 ms); P5 measured 16.3 ms in the same hour.
