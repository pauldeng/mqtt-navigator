# ADR 0004: Release profile and size gate

Status: accepted (2026-11-11). Advice: Codex (gpt-6.1-sol), decision D7 of P5.

## Measurements (Linux x64, stripped, panic = unwind, split debug info)

Load: 10k msg/s, 10k topics, 100 B payloads, 40 s; warm-cache first frame of 3 runs.

| Profile | Size | First frame | Frame p95 (median) | Worst frame | Worst merge | Drops |
|---|---|---|---|---|---|---|
| opt-level 3, lto thin | 47 702 232 B | 203–216 ms | 7.1 ms | 13.7 ms | 2.1 ms | 0 |
| opt-level 3, lto fat | 44 173 144 B | 201–206 ms | 6.4 ms | 8.1 ms | 1.8 ms | 0 |
| opt-level s, lto thin | 38 376 792 B | 197–205 ms | 7.6 ms | 10.9 ms | 2.0 ms | 0 |
| **opt-level s, lto fat** | **33 924 696 B** | 208–226 ms | 7.0 ms | 8.5 ms | 3.1 ms | 0 |

## Decision

- Use `opt-level = "s"` and `lto = "fat"` with `codegen-units = 1`. It is the smallest build, and it passes the budgets (frame p95 < 16 ms, merge ≤ 4 ms, no drops, first frame ≈ 200 ms warm).
- The CI `size` job fails when the Linux x64 binary is larger than 37 317 165 B (measured + 10%).
- Windows and macOS get their own gates after their first CI builds.

## Consequences

- A release build takes longer (about 6 minutes on this machine).
- The worst merge slice is closer to the 4 ms budget (3.1 ms). The CI load test asserts merge p99 ≤ 4 ms and max ≤ 8 ms.
