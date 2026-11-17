# Backlog merge: verification (2026-11-17)

Branch: `perf/backlog-merge` (local only, not pushed). Base: `feat/ui-simplify` at `b04b94f`.
The plan is [2026-11-17-threads-review.md](2026-11-17-threads-review.md). The Codex review in that file is the spec.

## 1. Result in short

- With a flood that really delivers the stated rate, the candidate admits all of 50k msg/s with 100 B (no drop, queue age 0.11 to 0.17 s). The baseline admits 30k to 35k. At 75k with 100 B the candidate admits 52k to 55k, and the baseline 14k to 20k. There is no change at 10k and 25k.
- **The flood tool does not deliver 1 KB at 50k or 75k msg/s here.** With 1 KB it publishes about 30k msg/s in total. The rest are failed publishes. An earlier version of this report counted them as offered load. Section 7 gives the real offered rate of each run. The 1 KB cases at "50k" and "75k" are 30k msg/s cases.
- **Not met: "no merge allocation above 4 ms".** The candidate still logs allocations of 4.0 to 6.0 ms in the load runs. I measured the thread CPU time of only 20 of them (section 6). Each of those 20 had 1.5 to 3.1 ms of CPU time, and the rest was run queue wait. I did not measure the other allocations above 4 ms (137 in the first matrix, not counted again in the new one). So the claim that the OS causes all of them is **not shown**. The release unit test allows the worst allocation to end 5.75 ms after its 3.25 ms deadline (about 9 ms, section 4). It passes, but it does not prove 4 ms.
- **Limit of the Xvfb test.** llvmpipe draws a frame every 45 ms (22 frames/s). It is **not a measurement on a real GPU**. Section 7 has the effect of the frame-callback fix and an emulation of a fast display.
- **Cost of the change.** In overload (75k with 100 B) the net queue stays full (50k messages). The oldest message in it waits 1.1 s. The old build dropped the excess in the store instead, and its queue age was 0.2 to 0.4 s. The number of drops is lower, but the data on the screen is older during an overload.
- Idle: no new wake-up. The main thread has the same 626 voluntary context switches in 10 s of idle in both builds (section 8, measured with the first candidate `3e0abfd`, not repeated). This count is not zero in either build. It comes from the existing code.

## 2. Changes (commits)

| Commit | Change |
|---|---|
| `4322347` feat(metrics) | Separate counters. Core: `NetStats::queued_msgs`, `Net::queued_msgs()`. App: dequeued, admitted, stale. The metrics line shows net drops, store memory drops, topic drops, list drops, queue length, queued KiB, queue age (longest wait of the first message of a merge), the dequeue and admit rate per second, and the number of allocations. The status bar total is the same as before. |
| `650fb3d` fix(ui) | The ingest stops 0.75 ms before the end of the allocation also when maintenance starts during the ingest. The plan and the loop are in `crates/navigator-app/src/ingest.rs`. The snapshot round has 16 units, was 256. Tests: a large tree with two filters, and a print of single round costs. |
| `adf3b85` perf(ui) | Backlog mode and frame pacing (section 3). |
| `3e0abfd` fix(store) | A work round is 32 items (was 256). Section 5 explains why. The backlog plan runs only when a frame callback paces the allocation. |
| fix(metrics) | The queue age is measured at the first message that the allocation pulls. It was measured only for the message that the loop pulled before. |
| fix(ui) | The frame callback runs the paced merge (section 3). Before, it only woke the loop. GPUI decided to draw before the loop ran, so one tick merged and the next drew: one allocation for two frames. |

## 3. Parameters

| Parameter | Value |
|---|---|
| Enter backlog | more than 1024 queued messages, or more than 4 MiB queued |
| Leave backlog | fewer than 256 messages and fewer than 1 MiB |
| Allocation in backlog | ingest 2.5 ms, maintenance until 3.25 ms, headroom 0.75 ms |
| Allocation in backlog, store above its budget | ingest 1.5 ms, maintenance until 3.25 ms |
| Allocation outside backlog | ingest until 3.25 ms (3.0 ms while the store has work), maintenance until 3.25 ms. The 33 ms coalescing stays. |
| Pacing in backlog | `render` registers `window.on_next_frame`. GPUI runs these callbacks before it decides to draw (`gpui-pre` `window.rs`, the frame request handler). The callback runs the merge and marks the view dirty, so the same frame draws the result. Then it sends a tick. The frame loop waits for the tick and reads the state. |
| Fallback | A 33 ms timer in the same `select!`. It starts only while backlog lasts. It covers a hidden window or a slow display. An allocation that the fallback starts uses the plan of normal mode. |
| Idle | The loop parks on the net queue and the kick channel (`ingest::next_first`). It has no timer. |
| Work round | 32 items (16 for the snapshot, 16 for node eviction). |

Pause is the same as before. The store still freezes the views, counts the paused messages and shows the removed-row markers. The new code does not touch these paths.

The tick channel holds one tick. A render that the user causes can add a callback. The result is one early allocation, never a busy loop: every callback needs a frame. The loop and the callback share the net queue through one lock. The callback skips the merge if the loop holds the lock.

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

These 20 allocations are the only ones with a CPU time measurement. For them, the thread used at most 3.13 ms of CPU, and the rest was the wait for a core. I did not measure the other allocations above 4 ms: 137 in the first matrix, and the ones in the second matrix. They have the same shape in the `slow_merge` lines (ingest overshoot of 0.5 to 4.7 ms with no slow call), but this is not a measurement. So I can say only that the sampled allocations did not exceed their plan in CPU time. I cannot say that none of the others did. On a real desktop with fewer other processes the effect should be smaller. The owner must check this.

## 7. Measured before and after

Setup: release binaries, Xvfb :77 1600x1000, llvmpipe, Mosquitto on 127.0.0.1:18830, list view, 10k topics (`flood/t0..t9999`), QoS 0, `MQTT_NAVIGATOR_METRICS=1`, args `--host 127.0.0.1 --port 18830 --tcp --sub '#'`. The floods run on the same 6 cores (one flood makes at most 25k msg/s, so 50k uses 2 and 75k uses 3). Each run: 40 s warm-up (the store is at its budget or list cap, which the store column shows), then a window of 30 log lines (one line each second). Two runs for each case. The order was base, candidate, base, candidate. The config, data, state and cache directories and the D-Bus session were private to each run. The owner's settings and keyring were not used.

**Baseline** is `4322347` (`b04b94f` plus the counters and the `merges` count). The plain `b04b94f` cannot log separate drop causes. **Candidate** is the head of the branch (`3e0abfd` plus the two fixes above). The 10k and the two 25k cases with a filter or a tree were not repeated. Their candidate rows are from `3e0abfd`. The queue-age fix changes only the metric. The merge fix acts only in backlog mode, which these cases do not enter.
The `merge` columns are per allocation. The p95 column shows the median and the largest of the per-second p95 values. "Seconds with max > 4 ms" counts log seconds in which one allocation was above 4 ms. Main CPU is the ticks of the main thread (`/proc/<pid>/task/<pid>/stat`) over the window. Frame timing and input latency under llvmpipe do not mean anything. They are **not measured on a real GPU**.

### Admitted rate (messages the store stored, per second, mean over the window)

"Offered" is the rate of successful publishes over the same window, summed over the flood processes (`analyze.py` reads the flood logs). Failed publishes do not count. A run is valid when the offered rate is within 1 % of the stated rate. All runs in the first matrix with 1 KB at 50k and 75k were not valid, except `base-50k-1k-r1` (49980 offered, admitted 16372) and `base-75k-1k-r1` (71357 offered, admitted 7478). I repeated the baseline for these cases, and the table shows the real offered rate (30k) instead.

| Scenario | offered/s (mean) | baseline (run 1, run 2) | candidate (run 1, run 2) | change of mean |
|---|---|---|---|---|
| 10k-100 | 10000 | 9999, 9999 | 9999, 10000 | +0 % |
| 25k-100 | 24998 | 24995, 25000 | 25000, 24997 | +0 % |
| 50k-100 | 49996 | 29523, 31652 | 49958, 50031 | +63 % |
| 75k-100 | 74990 | 13529, 20148 | 52054, 54662 | +217 % |
| 25k-1k | 24997 | 25006, 25005 | 25002, 24993 | -0 % |
| 50k-1k | 30369 | 23463, 23919 | 30111, 30407 | +28 % |
| 75k-1k | 30259 | 21203, 23489 | 30923, 29745 | +36 % |
| 25k-100-filter | 25002 | 24996, 25000 | 24999, 24996 | -0 % |
| 50k-100-filter | 50002 | 30974, 33020 | 49995, 50001 | +56 % |
| 25k-100-tree | 25004 | 25040, 24998 | 25003, 25004 | -0 % |
| 50k-100-tree | 50004 | 32214, 34965 | 49999, 49995 | +49 % |

The 1 KB cases at 50k and 75k have the same real offered rate (30k msg/s), so their rows test the same load. The flood cannot sustain more than 30k msg/s with 1 KB here. The candidate admits all of it. The baseline loses 20 % to 30 % of it in the store. The spread between two runs of the same build was large in the first matrix at 75k. It is small in the repeated runs.

### Full table (all runs)

Drops are per window. The cause columns are separate: "net drops" is a full net queue, "store mem drops" is the admission limit of the store (budget plus 25 %). Topic drops and list drops were 0 in every run. Stale-generation rejections were 0. Queue age is the age of the first message pulled in each allocation.

| Scenario | Build | Run | offered/s | admitted/s | net drops | store mem drops | max queue (msgs) | max queue age (ms) | merge p95 med/max (ms) | merge max (ms) | seconds with max > 4 ms | main CPU % |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10k-100 | base | 1 | 10000 | 9999 | 0 | 0 | 91 | 94 | 1.09/3.33 | 3.53 | 0 | 10.2 |
| 10k-100 | base | 2 | 10000 | 9999 | 0 | 0 | 100 | 92 | 0.87/3.40 | 4.10 | 1 | 9.7 |
| 10k-100 | cand 3e0abfd | 1 | 10000 | 9999 | 0 | 0 | 65 | 58 | 0.97/3.26 | 3.42 | 0 | 9.9 |
| 10k-100 | cand 3e0abfd | 2 | 10000 | 10000 | 0 | 0 | 35 | 57 | 0.94/3.26 | 3.30 | 0 | 9.6 |
| 25k-100 | base | 1 | 25001 | 24995 | 0 | 0 | 754 | 97 | 3.33/3.53 | 4.00 | 0 | 13.0 |
| 25k-100 | base | 2 | 24998 | 25000 | 0 | 0 | 1387 | 99 | 3.33/4.00 | 4.20 | 1 | 12.7 |
| 25k-100 | cand HEAD | 1 | 24998 | 25000 | 0 | 0 | 113 | 55 | 3.26/3.34 | 3.54 | 0 | 11.9 |
| 25k-100 | cand HEAD | 2 | 24997 | 24997 | 0 | 0 | 71 | 57 | 3.26/3.29 | 3.39 | 0 | 11.9 |
| 50k-100 | base | 1 | 50004 | 29523 | 0 | 627748 | 8102 | 217 | 3.40/3.58 | 5.20 | 3 | 14.6 |
| 50k-100 | base | 2 | 49995 | 31652 | 0 | 565818 | 5539 | 195 | 3.41/3.56 | 5.12 | 3 | 14.4 |
| 50k-100 | cand HEAD | 1 | 49993 | 49958 | 0 | 0 | 3425 | 126 | 3.29/3.41 | 4.16 | 3 | 15.7 |
| 50k-100 | cand HEAD | 2 | 49994 | 50031 | 0 | 0 | 2989 | 113 | 3.30/3.34 | 3.62 | 0 | 15.3 |
| 75k-100 | base | 1 | 74997 | 13529 | 0 | 1873308 | 24389 | 404 | 3.45/3.60 | 4.53 | 6 | 14.2 |
| 75k-100 | base | 2 | 74997 | 20148 | 0 | 1684177 | 13643 | 283 | 3.44/3.62 | 4.22 | 3 | 13.6 |
| 75k-100 | cand HEAD | 1 | 74981 | 52054 | 701044 | 0 | 49303 | 1113 | 3.32/3.44 | 4.07 | 3 | 16.9 |
| 75k-100 | cand HEAD | 2 | 74985 | 54662 | 620893 | 0 | 49153 | 1067 | 3.30/3.38 | 4.79 | 3 | 17.0 |
| 25k-1k | base | 1 | 24990 | 25006 | 0 | 0 | 1220 | 99 | 3.40/3.79 | 4.28 | 4 | 13.9 |
| 25k-1k | base | 2 | 24998 | 25005 | 0 | 0 | 106 | 95 | 3.40/3.63 | 4.89 | 4 | 13.2 |
| 25k-1k | cand HEAD | 1 | 24997 | 25002 | 0 | 0 | 374 | 69 | 3.29/3.38 | 4.20 | 1 | 12.8 |
| 25k-1k | cand HEAD | 2 | 25003 | 24993 | 0 | 0 | 90 | 74 | 3.29/3.36 | 5.76 | 1 | 12.9 |
| 50k-1k | base | 1 | 30405 | 23463 | 0 | 211900 | 3393 | 147 | 3.53/3.87 | 5.03 | 7 | 14.7 |
| 50k-1k | base | 2 | 30111 | 23919 | 0 | 190646 | 3958 | 213 | 3.52/4.18 | 5.00 | 12 | 14.5 |
| 50k-1k | cand HEAD | 1 | 30154 | 30111 | 0 | 0 | 5049 | 233 | 3.34/3.38 | 4.96 | 5 | 15.7 |
| 50k-1k | cand HEAD | 2 | 30806 | 30407 | 0 | 16618 | 1389 | 119 | 3.32/3.45 | 4.24 | 1 | 15.3 |
| 75k-1k | base | 1 | 30217 | 21203 | 0 | 278218 | 2495 | 191 | 3.58/3.85 | 5.25 | 10 | 14.5 |
| 75k-1k | base | 2 | 30101 | 23489 | 0 | 203234 | 2650 | 156 | 3.50/4.00 | 6.00 | 8 | 14.5 |
| 75k-1k | cand HEAD | 1 | 31000 | 30923 | 0 | 5487 | 3086 | 172 | 3.34/3.66 | 5.01 | 6 | 16.1 |
| 75k-1k | cand HEAD | 2 | 29719 | 29745 | 0 | 0 | 4056 | 213 | 3.33/3.95 | 4.71 | 5 | 15.4 |
| 25k-100-filter | base | 1 | 24995 | 24996 | 0 | 0 | 1198 | 96 | 3.32/3.43 | 4.45 | 1 | 11.7 |
| 25k-100-filter | base | 2 | 25011 | 25000 | 0 | 0 | 1152 | 100 | 3.31/3.39 | 4.00 | 0 | 11.6 |
| 25k-100-filter | cand 3e0abfd | 1 | 25008 | 24999 | 0 | 0 | 73 | 62 | 3.26/3.41 | 4.25 | 1 | 12.1 |
| 25k-100-filter | cand 3e0abfd | 2 | 24995 | 24996 | 0 | 0 | 65 | 52 | 3.26/3.30 | 4.44 | 1 | 11.6 |
| 50k-100-filter | base | 1 | 49994 | 30974 | 0 | 583123 | 7109 | 219 | 3.40/3.53 | 4.08 | 2 | 14.0 |
| 50k-100-filter | base | 2 | 50007 | 33020 | 0 | 524095 | 5209 | 194 | 3.38/3.56 | 4.40 | 6 | 14.0 |
| 50k-100-filter | cand HEAD | 1 | 50003 | 49995 | 0 | 0 | 1987 | 136 | 3.30/3.44 | 5.24 | 2 | 15.5 |
| 50k-100-filter | cand HEAD | 2 | 50002 | 50001 | 0 | 0 | 4512 | 156 | 3.29/3.36 | 4.34 | 1 | 15.2 |
| 25k-100-tree | base | 1 | 25024 | 25040 | 0 | 0 | 1208 | 103 | 3.32/3.50 | 4.77 | 2 | 13.3 |
| 25k-100-tree | base | 2 | 25003 | 24998 | 0 | 0 | 1276 | 97 | 3.31/3.36 | 3.53 | 0 | 12.0 |
| 25k-100-tree | cand 3e0abfd | 1 | 24993 | 25003 | 0 | 0 | 72 | 56 | 3.27/3.35 | 3.63 | 0 | 14.6 |
| 25k-100-tree | cand 3e0abfd | 2 | 24996 | 25004 | 0 | 0 | 76 | 53 | 3.26/3.32 | 4.47 | 1 | 12.2 |
| 50k-100-tree | base | 1 | 49998 | 32214 | 0 | 545967 | 7142 | 187 | 3.39/3.60 | 4.50 | 2 | 14.7 |
| 50k-100-tree | base | 2 | 49998 | 34965 | 0 | 462535 | 4929 | 180 | 3.40/3.53 | 4.50 | 1 | 14.4 |
| 50k-100-tree | cand HEAD | 1 | 50006 | 49999 | 0 | 0 | 1419 | 146 | 3.30/3.40 | 4.61 | 2 | 15.4 |
| 50k-100-tree | cand HEAD | 2 | 50012 | 49995 | 0 | 0 | 5242 | 168 | 3.29/3.39 | 3.76 | 0 | 15.5 |

What the table shows about the cause of the drops:

- The baseline loses messages in the store (admission above budget plus 25 %). It does that because eviction does not keep up. The queue stays short and young (0.1 to 0.4 s).
- The candidate does not reach the store limit. At 75k with 100 B it loses messages at the net queue, and the queue is full (up to 1.1 s old). Up to 50k it loses none. Eviction keeps the store at 128 to 150 MiB.
- `merges/s` is 22 in the 25k runs and the old candidate runs. With the merge in the frame callback it is 26 to 28 at 50k and 30 to 36 at 75k, with 22 frames/s. The extra allocations are from the 33 ms fallback timer, which fires between the slow frames.

### Effect of the frame-callback fix (Xvfb, 20 s warm-up, 20 s window, 100 B, list view)

"Paced" is the `backlog_merges` counter: allocations that a frame callback started.

| Case | Build | paced/s | allocations/s | frames/s | admitted/s | net drops/s |
|---|---|---|---|---|---|---|
| 50k | before (`3e0abfd`) | 13.7 | 22 | 22 | 34.5k to 38.9k | 40k |
| 50k | after | 9.4 | 27 to 30 | 22 | 46k to 53k | 3k |
| 75k | before (`3e0abfd`) | 11.4 | 22 | 22 | 33k to 36k | 41k |
| 75k | after | 21.9 | 30 to 36 | 21 to 23 | 48k to 61k | 15k |

Before the fix, the paced allocations were about half of the frames (11 to 14 of 22). After the fix, they equal the frames (22 of 22) as long as the backlog lasts. At 50k, the backlog now ends because the loop keeps up, so fewer allocations are paced. These runs are single runs and use the same warm-up as the matrix only in part. The frame rate under llvmpipe is the limit of the draw (45 ms), so a real display shows the gain of the fix more.

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
| Admitted throughput clearly higher at 50k and 75k | Yes with 100 B: +49 % to +63 % at 50k, +217 % at 75k (real offered rate). With 1 KB, offered is 30k msg/s: +28 % and +36 %. |
| No merge allocation above 4 ms | **Not met** in the load runs (max 4.0 to 6.0 ms). Only 20 allocations have a CPU time measurement (at most 3.13 ms, section 6). The others are not measured. Met in the unit test (p95 bound 0.75 ms past the deadline; the worst-case bound is 5.75 ms past it, about 9 ms in total, so it is not a 4 ms test) and at 10k and 25k except single noise seconds (the baseline has them too). |
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
