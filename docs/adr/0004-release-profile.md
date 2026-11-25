# ADR 0004: Release profile and size gate

Status: accepted (2026-11-11). The size gate is removed (2026-11-20, owner decision). The release profile uses `opt-level = 3` since 2026-11-26 (owner decision: speed, not size). Advice: Codex (gpt-6.1-sol), decision D7 of P5. The table of 2026-11-11 is for 0.1.x (`ms-mqtt-client`). The ntex build of P8 (S4, before 0.2.0) is 2.2% larger than the 0.1.1 release binary ([ADR 0006](0006-ntex-mqtt.md)).

## Measurements of 2026-11-11 (Linux x64, stripped, panic = unwind, split debug info)

Load: 10k msg/s, 10k topics, 100 B payloads, 40 s; warm-cache first frame of 3 runs.

| Profile | Size | First frame | Frame p95 (median) | Worst frame | Worst merge | Drops |
|---|---|---|---|---|---|---|
| opt-level 3, lto thin | 47 702 232 B | 203–216 ms | 7.1 ms | 13.7 ms | 2.1 ms | 0 |
| opt-level 3, lto fat | 44 173 144 B | 201–206 ms | 6.4 ms | 8.1 ms | 1.8 ms | 0 |
| opt-level s, lto thin | 38 376 792 B | 197–205 ms | 7.6 ms | 10.9 ms | 2.0 ms | 0 |
| **opt-level s, lto fat** | **33 924 696 B** | 208–226 ms | 7.0 ms | 8.5 ms | 3.1 ms | 0 |

## Measurement of 2026-11-26 (`opt-level`, 0.2.6)

Linux x64, Xvfb 1500x900 (software rendering), `lto = "fat"`, frame pointers on, no perf running. Load: 10 000 msg/s, 10 000 topics, 100 B payloads. Two runs of 30 s each. The measurement starts 14 s after the flood starts (S4: 39 s, because the script sets up the composer first). UI CPU is the CPU time of the main thread in the 30 s, divided by the frames of the same 30 s (the frame rate of the log lines, times 30 s). S3 offers about 50 000 msg/s. S4 has a second flood of 100 msg/s on the selected topic, and the details pane and the composer open (MQTT 5, 20 user properties, JSON mode).

| Scenario | UI CPU per frame, `s` | UI CPU per frame, `3` | Change | Merge p95, `s` / `3` |
|---|---|---|---|---|
| S1 Messages view, Follow on | 4.29 ms | 4.01 ms | -6.5% | 0.57 / 0.53 ms |
| S2 Tree view | 2.87 ms | 2.62 ms | -8.7% | 0.56 / 0.53 ms |
| S3 Messages view, 50 000 msg/s | 6.69 ms | 6.27 ms | -6.3% | 3.29 / 3.29 ms |
| S4 Messages view, composer open | 7.85 ms | 7.02 ms | -10.6% | 0.71 / 0.69 ms |

Frames per second are the same in both builds (about 22 in S1 to S3 and 32 in S4: the software renderer sets the pace). The merge maximum is 3.3 to 4.7 ms with `s` and 3.3 to 3.8 ms with `3`. S1, S2 and S4 have no drops. S3 drops messages at the net queue in both builds in these runs. Merge p95 is the median of the one-second values of a run. The binary (stripped) grows from 36.1 MB to 47.9 MB (+33%).

## Decision

- Use `opt-level = 3` and `lto = "fat"` with `codegen-units = 1`. It uses 6 to 11% less UI CPU for each frame than `opt-level = "s"`, and its worst merge slice stays under the 4 ms budget (3.3 to 3.8 ms, S1 to S4). We keep `lto = "fat"` because of the table of 2026-11-11: at opt-level 3, thin was larger (47.7 MB against 44.2 MB) and had a worse frame p95 (7.1 ms against 6.4 ms). The frame times of the new runs are the same in both builds, because the software renderer limits them.
- ~~The CI `size` job fails when the Linux x64 binary is larger than 37 317 165 B (measured + 10%).~~ Removed on 2026-11-20: the owner sets no size limit. CI has no size job.

## Consequences

- A release build takes longer. On this machine, `cargo build --release -p navigator-app --bins -p navigator-core --example flood` took about 7 minutes with `s` and 9 minutes with `3`. These builds used `RUSTFLAGS="-C force-frame-pointers=yes"`, `CARGO_PROFILE_RELEASE_STRIP=false`, `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only` and `CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=off`.
- The worst merge slice is close to the 4 ms budget (3.1 ms on 2026-11-11, 3.8 ms with `opt-level = 3` on 2026-11-26). The CI load test asserts merge p99 ≤ 4 ms and max ≤ 8 ms.
