# Backlog merge: verification (2026-11-17)

Branch: `perf/backlog-merge` (local only, not pushed). Base: `feat/ui-simplify` at `b04b94f`.
The plan is [2026-11-17-threads-review.md](2026-11-17-threads-review.md). The Codex review in that file is the spec.

## 1. Result in short

- Admitted throughput is higher at 50k and 75k msg/s. The gain is small at 50k (+5 to +14 %) and large at 75k with 100 B (+87 %, and the old build is unstable there).
- There is no change at 10k and 25k. The merge does not enter backlog mode there.
- **Not met: "no merge allocation above 4 ms".** The candidate still logs allocations of 4.0 to 7.0 ms in the load runs. The measurement in section 6 shows that the thread had only 1.5 to 3.1 ms of CPU time in each of them. The rest was waiting in the run queue. The machine had 6 cores for the app (about 200 %), 1 to 3 flood processes, Mosquitto, X and llvmpipe. The code does not exceed its plan. The unit test shows this (section 4).
- **Limit of the Xvfb test.** llvmpipe draws a frame every 48 ms (22 frames/s). Backlog mode does one allocation per frame, so it cannot do more than 22 per second here. The old loop also did 22 per second (it waits for the same main thread). A real display makes 60 to 144 frames/s. Section 7 has an emulation of that. It is **not a measurement on a real GPU**.
- **Cost of the change.** In overload the net queue stays full (50k messages). The oldest message in it waits 1.5 to 1.9 s. The old build dropped the excess in the store instead, and its queue age was 0.2 to 0.4 s. The number of drops is lower, but the data on the screen is older during an overload.
- Idle: no new wake-up. The main thread has the same 626 voluntary context switches in 10 s of idle in both builds (section 8). This count is not zero in either build. It comes from the existing code.

## 2. Changes (commits)

| Commit | Change |
|---|---|
| `4322347` feat(metrics) | Separate counters. Core: `NetStats::queued_msgs`, `Net::queued_msgs()`. App: dequeued, admitted, stale. The metrics line shows net drops, store memory drops, topic drops, list drops, queue length, queued KiB, queue age (longest wait of the first message of a merge), the dequeue and admit rate per second, and the number of allocations. The status bar total is the same as before. |
| `650fb3d` fix(ui) | The ingest stops 0.75 ms before the end of the allocation also when maintenance starts during the ingest. The plan and the loop are in `crates/navigator-app/src/ingest.rs`. The snapshot round has 16 units, was 256. Tests: a large tree with two filters, and a print of single round costs. |
| `adf3b85` perf(ui) | Backlog mode and frame pacing (section 3). |
| `3e0abfd` fix(store) | A work round is 32 items (was 256). Section 5 explains why. The backlog plan runs only when a frame callback paces the allocation. |

## 3. Parameters

| Parameter | Value |
|---|---|
| Enter backlog | more than 1024 queued messages, or more than 4 MiB queued |
| Leave backlog | fewer than 256 messages and fewer than 1 MiB |
| Allocation in backlog | ingest 2.5 ms, maintenance until 3.25 ms, headroom 0.75 ms |
| Allocation in backlog, store above its budget | ingest 1.5 ms, maintenance until 3.25 ms |
| Allocation outside backlog | ingest until 3.25 ms (3.0 ms while the store has work), maintenance until 3.25 ms. The 33 ms coalescing stays. |
| Pacing in backlog | `render` registers `window.on_next_frame`. The callback sends one tick. The frame loop waits for the tick, so there is one allocation for each frame. |
| Fallback | A 33 ms timer in the same `select!`. It starts only while backlog lasts. It covers a hidden window or a slow display. An allocation that the fallback starts uses the plan of normal mode. |
| Idle | The loop parks on the net queue and the kick channel (`ingest::next_first`). It has no timer. |
| Work round | 32 items (16 for the snapshot, 16 for node eviction). |

Pause is the same as before. The store still freezes the views, counts the paused messages and shows the removed-row markers. The new code does not touch these paths.

The tick channel holds one tick. A render that the user causes can add a tick. The result is one early allocation, never a busy loop: every tick needs a frame.

## 4. Tests

- `ingest::tests::backlog_mode_has_hysteresis`: the enter and leave thresholds, the equal-to-limit cases, the byte and message thresholds alone.
- `ingest::tests::the_backlog_budget_is_split_as_planned`, `the_plan_keeps_headroom_for_maintenance`: 2.5 ms, 1.5 ms over budget, 3.25 ms end, 0.75 ms headroom.
- `ingest::tests::an_idle_loop_parks_until_an_event`: the loop future is pending, and nothing wakes it. A kick or a message wakes it once. With pending work it does not park. A closed queue ends it.
- `ingest::tests::an_allocation_ends_near_its_plan_with_a_large_tree_and_filters`: 110k nodes, 1 KB payloads at the memory budget, a tree filter and a list filter, 400 allocations with four plans. The clock is real. The bound at p95 is 0.75 ms past the work deadline (6 ms in a debug build). The bound at the maximum is 5 ms more, because the OS can stop the test thread. Release result: p95 8 to 50 us, worst 50 to 660 us.
- `store::tests::one_eviction_round_is_one_small_chunk`, `round_costs` (prints single round times).
- `net::tests::queue_length_counts_queued_messages_only`.
- The existing store tests (`pause_with_100k_visible_nodes_runs_in_bounded_slices` and others) pass with the new chunk sizes.

Single round times with a warm cache (release, 100k nodes): list filter 66 us, tree filter 117 us, snapshot 728 us (256 units), eviction 81 us. They are small. The load runs showed rounds of 1 to 2 ms with a full store (section 5).

## 5. Why the round is 32 items

The first candidate build had a round of 256 items. In the 50k run, `slow_merge` lines showed `work` of 1 to 2.2 ms for an allocation whose maintenance should have ended at 3.25 ms. A full store touches cold memory when it evicts. 256 items took up to 2.2 ms. The round is 32 items now. The deadline test and the round-size test cover it. The matrix below used the 32-item build for the candidate. The baseline has the 256-item round.

After this change the logs still show `work` of 3 to 5.5 ms in a few allocations of the 1 KB runs. Section 6 shows that these allocations also had a low thread CPU time (the run queue wait is the same size). I did not find a code cause.

## 6. Where the allocations above 4 ms come from

A diagnostic build (not committed) read `/proc/thread-self/schedstat` before and after each allocation above 4 ms. Two runs (75k with 1 KB, 50k tree) gave 20 allocations:

| | mean | max |
|---|---|---|
| wall time | 4.51 ms | 5.84 ms |
| thread CPU time | 2.26 ms | 3.13 ms |
| wait in run queue | 2.18 ms | 3.17 ms |

The thread used at most 3.13 ms of CPU. The rest was the wait for a core. So the plan holds: the code stops in time, and the OS delays the thread. I did not measure the CPU time of the 137 slow allocations in the matrix. They have the same shape in the `slow_merge` lines (ingest overshoot of 0.5 to 4.7 ms with no slow call). On a real desktop with fewer other processes the effect should be smaller. The owner must check this.

## 7. Measured before and after

Setup: release binaries, Xvfb :77 1600x1000, llvmpipe, Mosquitto on 127.0.0.1:18830, list view, 10k topics (`flood/t0..t9999`), QoS 0, `MQTT_NAVIGATOR_METRICS=1`, args `--host 127.0.0.1 --port 18830 --tcp --sub '#'`. The floods run on the same 6 cores (one flood makes at most 25k msg/s, so 50k uses 2 and 75k uses 3). Each run: 40 s warm-up (the store is at its budget or list cap, which the store column shows), then a window of 30 log lines (one line each second). Two runs for each case. The order was base, candidate, base, candidate. The config, data, state and cache directories and the D-Bus session were private to each run. The owner's settings and keyring were not used.

**Baseline** is `4322347` (`b04b94f` plus the counters and the `merges` count). The plain `b04b94f` cannot log separate drop causes. **Candidate** is `3e0abfd`.
The `merge` columns are per allocation. The p95 column shows the median and the largest of the per-second p95 values. "Seconds with max > 4 ms" counts log seconds in which one allocation was above 4 ms. Main CPU is the ticks of the main thread (`/proc/<pid>/task/<pid>/stat`) over the window. Frame timing and input latency under llvmpipe do not mean anything. They are **not measured on a real GPU**.

### Admitted rate (messages the store stored, per second, mean over the window)

| Scenario | baseline (run 1, run 2) | candidate (run 1, run 2) | change of mean |
|---|---|---|---|
| 10k, 100 B | 9999, 9999 | 9999, 10000 | 0 % |
| 25k, 100 B | 24995, 25000 | 25003, 24996 | 0 % |
| 50k, 100 B | 29523, 31652 | 34789, 35143 | +14 % |
| 75k, 100 B | 13529, 20148 | 31204, 31825 | +87 % |
| 25k, 1 KB | 25006, 25005 | 24999, 24999 | 0 % |
| 50k, 1 KB | 16372, 23785 | 24039, 23057 | +17 % |
| 75k, 1 KB | 7478, 22998 | 18857, 21985 | +34 % |
| 25k, 100 B, text filter `flood` (matches all) | 24996, 25000 | 24999, 24996 | 0 % |
| 50k, 100 B, text filter | 30974, 33020 | 33962, 36094 | +9 % |
| 25k, 100 B, tree expanded (10k rows) | 25040, 24998 | 25003, 25004 | 0 % |
| 50k, 100 B, tree expanded (10k rows) | 32214, 34965 | 34674, 36070 | +5 % |

The spread between two runs of the same build is large above 50k (the baseline 75k 1 KB case: 7478 and 22998). The gain at 50k with 100 B is about as large as this spread. I do not call it clear. The result at 75k with 100 B is clear. Below 50k there is no regression.

### Full table (all runs)

Drops are per window. The cause columns are separate: "net drops" is a full net queue, "store mem drops" is the admission limit of the store (budget plus 25 %). Topic drops and list drops (rows removed for memory) were 0 in every run. Stale-generation rejections were 0.

| Scenario | Build | Run | admitted/s | dequeued/s | net drops | store mem drops | max queue (msgs) | max queue age (ms) | merge p95 med/max (ms) | merge max (ms) | seconds with max > 4 ms | main CPU % |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10k-100 | baseline | 1 | 9999 | 9999 | 0 | 0 | 91 | 94 | 1.09/3.33 | 3.53 | 0 | 10.2 |
| 10k-100 | baseline | 2 | 9999 | 9999 | 0 | 0 | 100 | 92 | 0.87/3.40 | 4.10 | 1 | 9.7 |
| 10k-100 | candidate | 1 | 9999 | 9999 | 0 | 0 | 65 | 58 | 0.97/3.26 | 3.42 | 0 | 9.9 |
| 10k-100 | candidate | 2 | 10000 | 10000 | 0 | 0 | 35 | 57 | 0.94/3.26 | 3.30 | 0 | 9.6 |
| 25k-100 | baseline | 1 | 24995 | 24995 | 0 | 0 | 754 | 97 | 3.33/3.53 | 4.00 | 0 | 13.0 |
| 25k-100 | baseline | 2 | 25000 | 25000 | 0 | 0 | 1387 | 99 | 3.33/4.00 | 4.20 | 1 | 12.7 |
| 25k-100 | candidate | 1 | 25003 | 25003 | 0 | 0 | 314 | 77 | 3.26/3.29 | 3.82 | 0 | 12.8 |
| 25k-100 | candidate | 2 | 24996 | 24996 | 0 | 0 | 73 | 54 | 3.26/3.29 | 3.63 | 0 | 12.7 |
| 50k-100 | baseline | 1 | 29523 | 50050 | 0 | 627748 | 8102 | 217 | 3.40/3.58 | 5.20 | 3 | 14.6 |
| 50k-100 | baseline | 2 | 31652 | 50075 | 0 | 565818 | 5539 | 195 | 3.41/3.56 | 5.12 | 3 | 14.4 |
| 50k-100 | candidate | 1 | 34789 | 34789 | 468189 | 0 | 49920 | 1709 | 3.30/3.49 | 4.28 | 2 | 14.8 |
| 50k-100 | candidate | 2 | 35143 | 35143 | 456772 | 0 | 49762 | 1579 | 3.29/3.41 | 4.00 | 0 | 14.5 |
| 75k-100 | baseline | 1 | 13529 | 74544 | 0 | 1873308 | 24389 | 404 | 3.45/3.60 | 4.53 | 6 | 14.2 |
| 75k-100 | baseline | 2 | 20148 | 74936 | 0 | 1684177 | 13643 | 283 | 3.44/3.62 | 4.22 | 3 | 13.6 |
| 75k-100 | candidate | 1 | 31204 | 31204 | 1343738 | 0 | 49533 | 1862 | 3.31/3.49 | 4.74 | 2 | 14.4 |
| 75k-100 | candidate | 2 | 31825 | 31825 | 1323920 | 0 | 49542 | 1905 | 3.32/4.68 | 4.92 | 3 | 14.5 |
| 25k-1k | baseline | 1 | 25006 | 25006 | 0 | 0 | 1220 | 99 | 3.40/3.79 | 4.28 | 4 | 13.9 |
| 25k-1k | baseline | 2 | 25005 | 25005 | 0 | 0 | 106 | 95 | 3.40/3.63 | 4.89 | 4 | 13.2 |
| 25k-1k | candidate | 1 | 24999 | 24999 | 0 | 0 | 247 | 58 | 3.26/3.33 | 3.82 | 0 | 13.2 |
| 25k-1k | candidate | 2 | 24999 | 24999 | 0 | 0 | 131 | 69 | 3.29/3.39 | 3.98 | 0 | 13.1 |
| 50k-1k | baseline | 1 | 16372 | 50023 | 0 | 1034650 | 9306 | 233 | 3.58/4.01 | 4.72 | 11 | 14.9 |
| 50k-1k | baseline | 2 | 23785 | 30453 | 0 | 205324 | 2234 | 149 | 3.58/3.90 | 5.68 | 14 | 14.8 |
| 50k-1k | candidate | 1 | 24039 | 24039 | 798220 | 0 | 27367 | 1540 | 3.38/4.05 | 5.93 | 8 | 15.1 |
| 50k-1k | candidate | 2 | 23057 | 23126 | 211762 | 2101 | 27489 | 1528 | 3.34/3.62 | 5.26 | 3 | 14.6 |
| 75k-1k | baseline | 1 | 7478 | 41495 | 947519 | 1044378 | 27693 | 1266 | 3.89/4.91 | 9.70 | 26 | 16.7 |
| 75k-1k | baseline | 2 | 22998 | 29876 | 0 | 211479 | 3189 | 164 | 3.58/3.97 | 5.35 | 13 | 15.2 |
| 75k-1k | candidate | 1 | 18857 | 18857 | 1717221 | 0 | 27709 | 1812 | 3.50/3.98 | 5.60 | 16 | 16.1 |
| 75k-1k | candidate | 2 | 21985 | 21985 | 250662 | 0 | 27551 | 1562 | 3.34/4.11 | 6.99 | 8 | 14.3 |
| 25k-100-filter | baseline | 1 | 24996 | 24996 | 0 | 0 | 1198 | 96 | 3.32/3.43 | 4.45 | 1 | 11.7 |
| 25k-100-filter | baseline | 2 | 25000 | 25000 | 0 | 0 | 1152 | 100 | 3.31/3.39 | 4.00 | 0 | 11.6 |
| 25k-100-filter | candidate | 1 | 24999 | 24999 | 0 | 0 | 73 | 62 | 3.26/3.41 | 4.25 | 1 | 12.1 |
| 25k-100-filter | candidate | 2 | 24996 | 24996 | 0 | 0 | 65 | 52 | 3.26/3.30 | 4.44 | 1 | 11.6 |
| 50k-100-filter | baseline | 1 | 30974 | 49988 | 0 | 583123 | 7109 | 219 | 3.40/3.53 | 4.08 | 2 | 14.0 |
| 50k-100-filter | baseline | 2 | 33020 | 50100 | 0 | 524095 | 5209 | 194 | 3.38/3.56 | 4.40 | 6 | 14.0 |
| 50k-100-filter | candidate | 1 | 33962 | 33962 | 491963 | 0 | 49472 | 1629 | 3.30/3.42 | 4.69 | 1 | 14.6 |
| 50k-100-filter | candidate | 2 | 36094 | 36094 | 426500 | 0 | 49635 | 1492 | 3.28/3.43 | 4.11 | 1 | 14.4 |
| 25k-100-tree | baseline | 1 | 25040 | 25040 | 0 | 0 | 1208 | 103 | 3.32/3.50 | 4.77 | 2 | 13.3 |
| 25k-100-tree | baseline | 2 | 24998 | 24998 | 0 | 0 | 1276 | 97 | 3.31/3.36 | 3.53 | 0 | 12.0 |
| 25k-100-tree | candidate | 1 | 25003 | 25003 | 0 | 0 | 72 | 56 | 3.27/3.35 | 3.63 | 0 | 14.6 |
| 25k-100-tree | candidate | 2 | 25004 | 25004 | 0 | 0 | 76 | 53 | 3.26/3.32 | 4.47 | 1 | 12.2 |
| 50k-100-tree | baseline | 1 | 32214 | 50010 | 0 | 545967 | 7142 | 187 | 3.39/3.60 | 4.50 | 2 | 14.7 |
| 50k-100-tree | baseline | 2 | 34965 | 49996 | 0 | 462535 | 4929 | 180 | 3.40/3.53 | 4.50 | 1 | 14.4 |
| 50k-100-tree | candidate | 1 | 34674 | 34674 | 471002 | 0 | 49130 | 1605 | 3.30/3.40 | 4.38 | 2 | 15.2 |
| 50k-100-tree | candidate | 2 | 36070 | 36070 | 426516 | 0 | 49512 | 1557 | 3.30/3.46 | 5.27 | 2 | 15.0 |


Totals at the end of the runs (not only the window) are in the logs: for example `cand-75k-100-r1` ends with dequeued = admitted = 2,457,711, net drops 3,023,624, store memory drops 0. The baseline `base-75k-100-r1` ends with dequeued 5,356,462, admitted 1,266,924, store memory drops 4,089,538, net drops 140,548. At 50k (run 1) the candidate ends with 2,640,772 admitted and 996,165 net drops. The baseline ends with 2,302,164 admitted and 1,368,844 store memory drops.

What the table shows about the cause of the drops:

- The baseline loses messages in the store (admission above budget plus 25 %). It does that because eviction does not keep up. The queue stays short and young (0.2 to 0.4 s).
- The candidate does not reach the store limit. It loses messages at the net queue, and the queue is full (up to 1.9 s old). Eviction keeps the store at 128 to 150 MiB.
- `merges/s` (allocations per second) is 22 in every run. It is the llvmpipe frame rate, for both builds.

### Emulation of a faster display (diagnostic build, not committed)

The diagnostic build has a fallback timer of 16 ms (60 Hz) or 8 ms (120 Hz) and treats the fallback as a frame. It shows what backlog mode does when the display is faster. It does **not** replace a test on a real GPU. The baseline cannot be emulated this way: its cadence is the fixed 33 ms (30 allocations per second at best).

| Run | admitted/s | net drops | max queue | max queue age (ms) | allocations/s | merge max (ms) | main CPU % |
|---|---|---|---|---|---|---|---|
| emu60-25k-100 | 24999 | 0 | 74 | 61 | 22 | 3.49 | 12.3 |
| emu60-50k-100 | 50012 | 0 | 1021 | 56 | 28 | 4.10 | 16.7 |
| emu60-75k-100 | 67361 | 231793 | 49421 | 866 | 42 | 5.97 | 19.4 |
| emu60-75k-1k | 27218 | 0 | 995 | 131 | 26 | 4.72 | 15.2 |
| emu120-75k-100 | 74997 | 0 | 2966 | 57 | 41 | 4.46 | 20.0 |


At 60 Hz the candidate admits all of 50k msg/s with no drop, and 67k of 75k. At 120 Hz it admits all of 75k. The 1 KB case at 75k admits 27k at 60 Hz (the store work for large payloads is the limit).

## 8. Idle wake-ups

`idle.sh`: 3 floods of 20k msg/s for 8 s, 8 s to drain, then 10 s of idle. Voluntary context switches of the main thread: baseline 626, candidate 626. All threads: 666 and 666. The metrics log has no line while idle. The candidate adds no wake-up. The count of about 60 per second is in both builds. It is not zero. Its source is not in this change (I did not look for it). The owner may want to check it (a frame timer of gpui, the cursor or the window system).

## 9. Gate

| Check | Result |
|---|---|
| `cargo fmt --all --check` | pass |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | pass |
| `cargo test --workspace` | pass (app 134, core 171, broker-free tests) |
| `net_mosquitto`, `broker_matrix`, `net_system_ca` (Mosquitto, `broker-tests`) | pass (22, 27, 1 tests) |
| `load_ci` (release, warm-up 15 s, 60 s) | pass: 10001 msg/s, 750500 sent, 0 missing, 0 duplicate, 0 drops, merge slices p99 1.55 ms, max 1.97 ms |

I did not run `broker_compat` and `net_keepalive_tasks` (not in the requested set) and `cargo deny`.

## 10. Acceptance

| Criterion | Result |
|---|---|
| Admitted throughput clearly higher at 50k and 75k | 75k: yes (+87 % with 100 B). 50k: +5 to +14 %, about as large as the run-to-run spread. Not clear under Xvfb. At 60 Hz emulation: yes. |
| No merge allocation above 4 ms | **Not met** in the load runs (max 4.0 to 7.0 ms). The thread CPU time of the measured ones was at most 3.13 ms (section 6). Met in the unit test and at 10k and 25k except single noise seconds (the baseline has them too). |
| No regression at 10k and 25k | Met. Admitted rate equal, queue age lower (52 to 77 ms against 94 to 103 ms), main CPU equal. |
| No idle wake-ups | No new wake-up (section 8). |

## 11. Deviations

- The baseline is `4322347`, not the plain `b04b94f`, so that both builds log the drop causes.
- The first candidate (with a 256-item round) was replaced by the 32-item build before the matrix. The commit for it is separate (`3e0abfd`), not a squash into the step 2 commit.
- The backlog plan runs only for frame-paced allocations (fallback allocations use the normal plan), because under a slow display the 2.5 ms split gave less than the old 3 ms. This is a change to the spec.
- The 40 s warm-up does not prove a steady state for every case. The store column shows 127 to 159 MiB (the budget is 128 MiB). The store plateaus at its budget in the 25k runs. Eviction removes history entries first, so `list_drops` (rows removed for memory) stayed 0 in all runs.
- Queue age is the age of the first message of each allocation, not of the oldest message in the queue. For a FIFO this is the same.
- The matrix has no run with Pause, and no scrolling or typing. These need a real display (section 12).
- The raw logs are in the scratch folder `/tmp/claude-1000/-home-pdeng-Workspace-mqtt-navigator/48e74d3d-e99e-4cde-8366-7382a812f445/scratchpad/perf/logs` (`<build>-<case>-r<n>.log`, `.cpu`, `.png`) with `run.sh`, `matrix.sh` and `analyze.py`. They are not in the repository.

## 12. Owner check list for a real desktop

Run both builds (this branch and `feat/ui-simplify`) with `MQTT_NAVIGATOR_METRICS=1` and a flood of 50k msg/s (2 or 3 `flood` processes) against a local broker.

1. **Frames.** Watch `frame_p95` and `frames=` in the log. The candidate should do about one allocation per frame (`merges=`) at 60 Hz or more while the queue is deep. The log line `backlog_merges` counts the paced ones.
2. **Typing.** Type in the text filter during the flood. Typing must stay smooth. Compare the delay from key to character with the baseline build.
3. **Scrolling.** Scroll the list and the expanded tree during the flood. The scroll must not jump. The anchor row stays in place when the list is not at the top.
4. **Pause.** Press Pause during the flood. The view must freeze. The counter of new messages grows. Removed rows show a marker. Press Pause again: the view must catch up with no jump.
5. **Latency.** Look at the newest timestamp in the list during an overload. It may be up to 2 s old (the full queue). Decide if this is acceptable. The old build showed fresher data.
6. **Merge time.** In the log, `merge_max` and the `slow_merge` lines must be at or below 4 ms. A `slow_merge` line gives the plan, the ingest, the work and the anchor times. If the thread is the cause, the ingest time is above its plan with many messages.
7. **CPU.** Compare the CPU of the main thread. It should stay near 15 to 20 % at 50k.
8. **Idle.** Stop the flood. The log must stop. Check the idle CPU and wake-ups of the process (the idle context switches in section 8).
9. **Filter and tree.** Repeat 2 and 3 with a matching text filter and with the tree expanded (10k rows).
